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
struct WaitingTicket {
    id: u64,
    received_at: i64,
    graph_name: String,
    query: String,
    enqueued: Instant,
}

#[derive(Default)]
struct SchedulerState {
    running: Vec<RunningQueryInfo>,
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

pub struct QueryPermit {
    id: u64,
}

impl QueryPermit {
    pub fn acquire(
        graph_name: &str,
        query: &str,
    ) -> Result<Self, String> {
        let id = NEXT_QUERY_ID.fetch_add(1, Ordering::Relaxed);
        let received_at = unix_now_secs();
        let enqueued = Instant::now();
        let mut state = SCHEDULER.state.lock();

        // If a worker is immediately available and nobody is queued ahead,
        // enter directly. Otherwise queue fairly behind existing waiters.
        if state.running.len() < worker_limit() && state.waiting.is_empty() {
            state.running.push(RunningQueryInfo {
                id,
                received_at,
                graph_name: graph_name.to_string(),
                query: query.to_string(),
                start: Instant::now(),
            });
            return Ok(Self { id });
        }

        if state.waiting.len() >= native_config::max_queued_queries() {
            return Err("Max pending queries exceeded".to_string());
        }

        state.waiting.push_back(WaitingTicket {
            id,
            received_at,
            graph_name: graph_name.to_string(),
            query: query.to_string(),
            enqueued,
        });

        loop {
            let is_front = state.waiting.front().is_some_and(|entry| entry.id == id);
            if is_front && state.running.len() < worker_limit() {
                let waiting = state
                    .waiting
                    .pop_front()
                    .expect("front waiter exists while promoting query");
                state.running.push(RunningQueryInfo {
                    id: waiting.id,
                    received_at: waiting.received_at,
                    graph_name: waiting.graph_name,
                    query: waiting.query,
                    start: Instant::now(),
                });
                SCHEDULER.available.notify_all();
                return Ok(Self { id });
            }
            SCHEDULER.available.wait(&mut state);
        }
    }
}

impl Drop for QueryPermit {
    fn drop(&mut self) {
        let mut state = SCHEDULER.state.lock();
        if let Some(pos) = state.running.iter().position(|entry| entry.id == self.id) {
            state.running.swap_remove(pos);
        }
        SCHEDULER.available.notify_all();
    }
}

pub fn snapshot_running() -> Vec<RunningQueryInfo> {
    SCHEDULER.state.lock().running.clone()
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

    #[test]
    fn snapshots_start_empty() {
        assert!(snapshot_running().is_empty());
        assert!(snapshot_waiting().is_empty());
    }
}
