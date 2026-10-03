use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicU64, Ordering},
        LazyLock,
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use parking_lot::{Condvar, Mutex};

use crate::native_config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Read,
    Write,
}

#[derive(Debug, Clone)]
pub struct RunningQueryInfo {
    pub id: u64,
    pub received_at: i64,
    pub graph_name: String,
    pub query: String,
    pub start: Instant,
}

#[derive(Debug, Clone)]
pub struct WaitingQueryInfo {
    pub id: u64,
    pub received_at: i64,
    pub graph_name: String,
    pub query: String,
    pub enqueued: Instant,
}

#[derive(Debug, Clone)]
struct RunningTicket {
    info: RunningQueryInfo,
    mode: QueryMode,
}

#[derive(Debug, Clone)]
struct WaitingTicket {
    id: u64,
    received_at: i64,
    graph_name: String,
    query: String,
    enqueued: Instant,
    mode: QueryMode,
}

#[derive(Default)]
struct SchedulerState {
    running: Vec<RunningTicket>,
    waiting: VecDeque<WaitingTicket>,
}

struct Scheduler {
    state: Mutex<SchedulerState>,
    available: Condvar,
}

static NEXT_QUERY_ID: AtomicU64 = AtomicU64::new(1);
static SCHEDULER: LazyLock<Scheduler> = LazyLock::new(|| Scheduler {
    state: Mutex::new(SchedulerState::default()),
    available: Condvar::new(),
});

fn worker_limit() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .max(1)
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

fn running_on_graph(state: &SchedulerState, graph_name: &str) -> usize {
    state
        .running
        .iter()
        .filter(|entry| entry.info.graph_name == graph_name)
        .count()
}

fn graph_has_running_writer(state: &SchedulerState, graph_name: &str) -> bool {
    state.running.iter().any(|entry| {
        entry.info.graph_name == graph_name && entry.mode == QueryMode::Write
    })
}

fn earlier_waiting_writer(
    state: &SchedulerState,
    index: usize,
    graph_name: &str,
) -> bool {
    state.waiting.iter().take(index).any(|entry| {
        entry.graph_name == graph_name && entry.mode == QueryMode::Write
    })
}

fn ticket_is_eligible(
    state: &SchedulerState,
    index: usize,
    ticket: &WaitingTicket,
) -> bool {
    if state.running.len() >= worker_limit() {
        return false;
    }

    match ticket.mode {
        // MVCC readers execute on detached committed snapshots, so an existing
        // reader does not block a writer from building the next COW version.
        QueryMode::Write => !graph_has_running_writer(state, &ticket.graph_name),
        QueryMode::Read => {
            !graph_has_running_writer(state, &ticket.graph_name)
                && !earlier_waiting_writer(state, index, &ticket.graph_name)
        }
    }
}

/// Select the next waiter with two goals:
///
/// 1. preserve writer progress within each graph; and
/// 2. prevent one hot graph from monopolizing every global worker slot.
///
/// Among eligible waiters, prefer the graph with the fewest running queries,
/// then preserve FIFO order as the tie-breaker.
fn next_eligible_waiter_index(state: &SchedulerState) -> Option<usize> {
    if state.running.len() >= worker_limit() {
        return None;
    }

    state
        .waiting
        .iter()
        .enumerate()
        .filter(|(index, ticket)| ticket_is_eligible(state, *index, ticket))
        .min_by_key(|(index, ticket)| {
            (
                running_on_graph(state, &ticket.graph_name),
                *index,
            )
        })
        .map(|(index, _)| index)
}

fn start_ticket(state: &mut SchedulerState, ticket: WaitingTicket) -> QueryPermit {
    let id = ticket.id;
    state.running.push(RunningTicket {
        mode: ticket.mode,
        info: RunningQueryInfo {
            id,
            received_at: ticket.received_at,
            graph_name: ticket.graph_name,
            query: ticket.query,
            start: Instant::now(),
        },
    });
    QueryPermit { id }
}

pub struct QueryPermit {
    id: u64,
}

impl QueryPermit {
    pub fn acquire(
        graph_name: &str,
        query: &str,
        mode: QueryMode,
    ) -> Result<Self, String> {
        let id = NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        let received_at = unix_now_secs();
        let enqueued = Instant::now();
        let mut state = SCHEDULER.state.lock();

        let ticket = WaitingTicket {
            id,
            received_at,
            graph_name: graph_name.to_string(),
            query: query.to_string(),
            enqueued,
            mode,
        };

        // Fast path only when no older waiter exists. Once a queue forms, all
        // promotions use the same fairness rule.
        if state.waiting.is_empty()
            && state.running.len() < worker_limit()
            && match mode {
                QueryMode::Write => !graph_has_running_writer(&state, graph_name),
                QueryMode::Read => !graph_has_running_writer(&state, graph_name),
            }
        {
            return Ok(start_ticket(&mut state, ticket));
        }

        if state.waiting.len() >= native_config::max_queued_queries() {
            return Err("Max pending queries exceeded".to_string());
        }

        state.waiting.push_back(ticket);

        loop {
            let next = next_eligible_waiter_index(&state);
            if let Some(index) = next
                && state.waiting.get(index).is_some_and(|entry| entry.id == id)
            {
                let waiting = state
                    .waiting
                    .remove(index)
                    .expect("selected waiter exists while promoting query");
                let permit = start_ticket(&mut state, waiting);
                SCHEDULER.available.notify_all();
                return Ok(permit);
            }
            SCHEDULER.available.wait(&mut state);
        }
    }
}

impl Drop for QueryPermit {
    fn drop(&mut self) {
        let mut state = SCHEDULER.state.lock();
        if let Some(pos) = state
            .running
            .iter()
            .position(|entry| entry.info.id == self.id)
        {
            state.running.swap_remove(pos);
        }
        SCHEDULER.available.notify_all();
    }
}

pub fn snapshot_running() -> Vec<RunningQueryInfo> {
    SCHEDULER
        .state
        .lock()
        .running
        .iter()
        .map(|entry| entry.info.clone())
        .collect()
}

pub fn snapshot_waiting() -> Vec<WaitingQueryInfo> {
    SCHEDULER
        .state
        .lock()
        .waiting
        .iter()
        .map(|entry| WaitingQueryInfo {
            id: entry.id,
            received_at: entry.received_at,
            graph_name: entry.graph_name.clone(),
            query: entry.query.clone(),
            enqueued: entry.enqueued,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(id: u64, graph: &str, mode: QueryMode) -> WaitingTicket {
        WaitingTicket {
            id,
            received_at: 0,
            graph_name: graph.to_string(),
            query: format!("q{id}"),
            enqueued: Instant::now(),
            mode,
        }
    }

    fn running(id: u64, graph: &str, mode: QueryMode) -> RunningTicket {
        RunningTicket {
            mode,
            info: RunningQueryInfo {
                id,
                received_at: 0,
                graph_name: graph.to_string(),
                query: format!("q{id}"),
                start: Instant::now(),
            },
        }
    }

    #[test]
    fn cold_graph_beats_hot_graph_when_both_are_waiting() {
        let mut state = SchedulerState::default();
        state.running.push(running(1, "hot", QueryMode::Read));
        state.running.push(running(2, "hot", QueryMode::Read));
        state.waiting.push_back(waiting(3, "hot", QueryMode::Read));
        state.waiting.push_back(waiting(4, "cold", QueryMode::Read));

        // With at least one free worker, the cold graph should be selected
        // because it currently has zero running queries.
        if worker_limit() > state.running.len() {
            assert_eq!(next_eligible_waiter_index(&state), Some(1));
        }
    }

    #[test]
    fn second_writer_on_same_graph_waits() {
        let mut state = SchedulerState::default();
        state.running.push(running(1, "g", QueryMode::Write));
        state.waiting.push_back(waiting(2, "g", QueryMode::Write));
        assert!(!ticket_is_eligible(&state, 0, &state.waiting[0]));
    }

    #[test]
    fn writer_can_overlap_existing_mvcc_reader() {
        let mut state = SchedulerState::default();
        state.running.push(running(1, "g", QueryMode::Read));
        state.waiting.push_back(waiting(2, "g", QueryMode::Write));
        if worker_limit() > state.running.len() {
            assert!(ticket_is_eligible(&state, 0, &state.waiting[0]));
        }
    }

    #[test]
    fn waiting_writer_blocks_later_same_graph_reader() {
        let mut state = SchedulerState::default();
        state.waiting.push_back(waiting(1, "g", QueryMode::Write));
        state.waiting.push_back(waiting(2, "g", QueryMode::Read));
        assert!(!ticket_is_eligible(&state, 1, &state.waiting[1]));
    }

    #[test]
    fn different_graph_can_run_while_writer_is_active() {
        let mut state = SchedulerState::default();
        state.running.push(running(1, "a", QueryMode::Write));
        state.waiting.push_back(waiting(2, "b", QueryMode::Read));
        if worker_limit() > state.running.len() {
            assert!(ticket_is_eligible(&state, 0, &state.waiting[0]));
        }
    }
}
