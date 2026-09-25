use std::sync::OnceLock;

use parking_lot::RwLock;

use graph::graph::graph::NODE_CREATION_BUFFER;

pub const CONFIG_NAMES: &[&str] = &[
    "TIMEOUT",
    "TIMEOUT_DEFAULT",
    "TIMEOUT_MAX",
    "CACHE_SIZE",
    "ASYNC_DELETE",
    "OMP_THREAD_COUNT",
    "THREAD_COUNT",
    "INDEX_WORKER_THREADS",
    "RESULTSET_SIZE",
    "VKEY_MAX_ENTITY_COUNT",
    "MAX_QUEUED_QUERIES",
    "QUERY_MEM_CAPACITY",
    "DELTA_MAX_PENDING_CHANGES",
    "NODE_CREATION_BUFFER",
    "CMD_INFO",
    "MAX_INFO_QUERIES",
    "EFFECTS_COMPRESSION",
    "EFFECTS_THRESHOLD",
    "BOLT_PORT",
    "DELAY_INDEXING",
    "IMPORT_FOLDER",
    "TEMP_FOLDER",
    "JS_HEAP_SIZE",
    "JS_STACK_SIZE",
];

#[derive(Debug, Clone)]
pub enum ConfigValue {
    Int(i64),
    Text(String),
}

#[derive(Debug, Clone)]
struct State {
    timeout: i64,
    timeout_default: i64,
    timeout_max: i64,
    async_delete: i64,
    result_set_size: i64,
    vkey_max_entity_count: i64,
    max_queued_queries: u64,
    query_mem_capacity: i64,
    delta_max_pending_changes: i64,
    cmd_info: bool,
    max_info_queries: i64,
    effects_threshold: i64,
    delay_indexing: bool,
    import_folder: String,
    temp_folder: String,
    js_heap_size: i64,
    js_stack_size: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            timeout: 0,
            timeout_default: 0,
            timeout_max: 0,
            async_delete: 0,
            result_set_size: -1,
            vkey_max_entity_count: 100_000,
            max_queued_queries: u32::MAX as u64,
            query_mem_capacity: 0,
            delta_max_pending_changes: 10_000,
            cmd_info: true,
            max_info_queries: 1000,
            effects_threshold: 300,
            delay_indexing: false,
            import_folder: "/var/lib/FalkorDB/import/".to_string(),
            temp_folder: "/tmp".to_string(),
            js_heap_size: 256 * 1024 * 1024,
            js_stack_size: 1024 * 1024,
        }
    }
}

fn state() -> &'static RwLock<State> {
    static STATE: OnceLock<RwLock<State>> = OnceLock::new();
    STATE.get_or_init(|| RwLock::new(State::default()))
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub import_folder: String,
    pub result_set_size: i64,
    pub query_mem_capacity: i64,
}

pub fn runtime_config() -> RuntimeConfig {
    let state = state().read();
    RuntimeConfig {
        import_folder: state.import_folder.clone(),
        result_set_size: state.result_set_size,
        query_mem_capacity: state.query_mem_capacity,
    }
}

pub fn effective_timeout(
    per_query_timeout: Option<i64>,
    is_write: bool,
) -> Result<Option<u64>, String> {
    let state = state().read();

    if let Some(timeout) = per_query_timeout {
        if timeout < 0 {
            return Err("Query timeout must be a non-negative integer".to_string());
        }
        if state.timeout_max > 0 && timeout > state.timeout_max {
            return Err(
                "The query TIMEOUT parameter value cannot exceed the TIMEOUT_MAX configuration parameter value"
                    .to_string(),
            );
        }
        if !is_write && timeout > 0 {
            return Ok(Some(timeout as u64));
        }
    }

    if state.timeout_default > 0 {
        return Ok(Some(state.timeout_default as u64));
    }
    if state.timeout_max > 0 {
        return Ok(Some(state.timeout_max as u64));
    }
    if state.timeout > 0 && !is_write {
        return Ok(Some(state.timeout as u64));
    }
    Ok(None)
}

pub fn get(name: &str) -> Result<ConfigValue, String> {
    let upper = name.to_ascii_uppercase();
    let state = state().read();
    let threads = std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get) as i64;

    let value = match upper.as_str() {
        "TIMEOUT" => ConfigValue::Int(state.timeout),
        "TIMEOUT_DEFAULT" => ConfigValue::Int(state.timeout_default),
        "TIMEOUT_MAX" => ConfigValue::Int(state.timeout_max),
        "CACHE_SIZE" => ConfigValue::Int(25),
        "ASYNC_DELETE" => ConfigValue::Int(state.async_delete),
        "OMP_THREAD_COUNT" | "THREAD_COUNT" => ConfigValue::Int(threads),
        "INDEX_WORKER_THREADS" => ConfigValue::Int(0),
        "RESULTSET_SIZE" => ConfigValue::Int(state.result_set_size),
        "VKEY_MAX_ENTITY_COUNT" => ConfigValue::Int(state.vkey_max_entity_count),
        "MAX_QUEUED_QUERIES" => ConfigValue::Int(state.max_queued_queries as i64),
        "QUERY_MEM_CAPACITY" => ConfigValue::Int(state.query_mem_capacity),
        "DELTA_MAX_PENDING_CHANGES" => ConfigValue::Int(state.delta_max_pending_changes),
        "NODE_CREATION_BUFFER" => ConfigValue::Int(
            NODE_CREATION_BUFFER.load(std::sync::atomic::Ordering::Relaxed) as i64,
        ),
        "CMD_INFO" => ConfigValue::Int(if state.cmd_info { 1 } else { 0 }),
        "MAX_INFO_QUERIES" => ConfigValue::Int(state.max_info_queries),
        "EFFECTS_COMPRESSION" => ConfigValue::Int(
            graph::effects::EFFECTS_COMPRESSION
                .load(std::sync::atomic::Ordering::Relaxed),
        ),
        "EFFECTS_THRESHOLD" => ConfigValue::Int(state.effects_threshold),
        "BOLT_PORT" => ConfigValue::Int(65535),
        "DELAY_INDEXING" => ConfigValue::Int(if state.delay_indexing { 1 } else { 0 }),
        "IMPORT_FOLDER" => ConfigValue::Text(state.import_folder.clone()),
        "TEMP_FOLDER" => ConfigValue::Text(state.temp_folder.clone()),
        "JS_HEAP_SIZE" => ConfigValue::Int(state.js_heap_size),
        "JS_STACK_SIZE" => ConfigValue::Int(state.js_stack_size),
        _ => return Err(format!("Unknown configuration field '{name}'")),
    };
    Ok(value)
}

#[derive(Debug, Clone)]
enum Validated {
    Int(i64),
    Uint(u64),
}

pub fn set_many(pairs: &[(String, String)]) -> Result<(), String> {
    if pairs.is_empty() {
        return Err("Missing configuration parameter name".to_string());
    }

    let mut validated = Vec::with_capacity(pairs.len());
    for (name, value) in pairs {
        let upper = name.to_ascii_uppercase();
        let parsed = validate(&upper, value)?;
        validated.push((upper, parsed));
    }

    {
        let current = state().read();
        let mut timeout_default = current.timeout_default;
        let mut timeout_max = current.timeout_max;
        let mut setting_timeout = false;

        for (name, value) in &validated {
            match name.as_str() {
                "TIMEOUT" => setting_timeout = true,
                "TIMEOUT_DEFAULT" => timeout_default = as_i64(value),
                "TIMEOUT_MAX" => timeout_max = as_i64(value),
                _ => {}
            }
        }

        if setting_timeout && (timeout_default > 0 || timeout_max > 0) {
            return Err(
                "The TIMEOUT configuration parameter is deprecated. Please set TIMEOUT_MAX and TIMEOUT_DEFAULT instead"
                    .to_string(),
            );
        }
        if timeout_default > 0 && timeout_max > 0 && timeout_default > timeout_max {
            if validated.iter().any(|(name, _)| name == "TIMEOUT_DEFAULT") {
                return Err(
                    "TIMEOUT_DEFAULT configuration parameter cannot be set to a value higher than TIMEOUT_MAX"
                        .to_string(),
                );
            }
            return Err(
                "TIMEOUT_MAX configuration parameter cannot be set to a value lower than TIMEOUT_DEFAULT"
                    .to_string(),
            );
        }
    }

    let mut state = state().write();
    let mut udf_config_changed = false;
    for (name, value) in validated {
        match name.as_str() {
            "TIMEOUT" => state.timeout = as_i64(&value),
            "TIMEOUT_DEFAULT" => state.timeout_default = as_i64(&value),
            "TIMEOUT_MAX" => state.timeout_max = as_i64(&value),
            "ASYNC_DELETE" => state.async_delete = as_i64(&value),
            "RESULTSET_SIZE" => state.result_set_size = as_i64(&value),
            "VKEY_MAX_ENTITY_COUNT" => state.vkey_max_entity_count = as_i64(&value),
            "MAX_QUEUED_QUERIES" => state.max_queued_queries = as_u64(&value),
            "QUERY_MEM_CAPACITY" => state.query_mem_capacity = as_i64(&value),
            "DELTA_MAX_PENDING_CHANGES" => state.delta_max_pending_changes = as_i64(&value),
            "CMD_INFO" => state.cmd_info = as_i64(&value) != 0,
            "MAX_INFO_QUERIES" => state.max_info_queries = as_i64(&value),
            "EFFECTS_COMPRESSION" => {
                graph::effects::EFFECTS_COMPRESSION.store(
                    as_i64(&value),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            "EFFECTS_THRESHOLD" => state.effects_threshold = as_i64(&value),
            "DELAY_INDEXING" => state.delay_indexing = as_i64(&value) != 0,
            "JS_HEAP_SIZE" => {
                state.js_heap_size = as_i64(&value);
                graph::udf::js_context::JS_HEAP_SIZE.store(
                    state.js_heap_size,
                    std::sync::atomic::Ordering::Relaxed,
                );
                udf_config_changed = true;
            }
            "JS_STACK_SIZE" => {
                state.js_stack_size = as_i64(&value);
                graph::udf::js_context::JS_STACK_SIZE.store(
                    state.js_stack_size,
                    std::sync::atomic::Ordering::Relaxed,
                );
                udf_config_changed = true;
            }
            _ => {}
        }
    }
    drop(state);

    if udf_config_changed {
        graph::udf::get_udf_repo().bump_version();
    }

    Ok(())
}

fn validate(name: &str, value: &str) -> Result<Validated, String> {
    match name {
        "TIMEOUT"
        | "TIMEOUT_DEFAULT"
        | "TIMEOUT_MAX"
        | "QUERY_MEM_CAPACITY"
        | "DELTA_MAX_PENDING_CHANGES"
        | "EFFECTS_COMPRESSION"
        | "EFFECTS_THRESHOLD" => {
            let value = parse_i64(name, value)?;
            if value < 0 {
                return Err(format!("Failed to set config value {name} to {value}"));
            }
            Ok(Validated::Int(value))
        }
        "ASYNC_DELETE" | "CMD_INFO" | "DELAY_INDEXING" => {
            let value = match value.to_ascii_lowercase().as_str() {
                "yes" | "1" | "true" => 1,
                "no" | "0" | "false" => 0,
                _ => return Err(format!("Failed to set config value {name} to {value}")),
            };
            Ok(Validated::Int(value))
        }
        "RESULTSET_SIZE" => {
            let value = parse_i64(name, value)?;
            Ok(Validated::Int(if value < 0 { -1 } else { value }))
        }
        "MAX_QUEUED_QUERIES" => {
            let value = parse_i64(name, value)?;
            if value <= 0 {
                return Err(format!("Failed to set config value {name} to {value}"));
            }
            Ok(Validated::Uint(value as u64))
        }
        "VKEY_MAX_ENTITY_COUNT" => Ok(Validated::Int(parse_i64(name, value)?)),
        "MAX_INFO_QUERIES" => {
            let value = parse_i64(name, value)?;
            if value < 0 {
                return Err(format!("Failed to set config value {name} to {value}"));
            }
            Ok(Validated::Int(value.min(1000)))
        }
        "JS_HEAP_SIZE" | "JS_STACK_SIZE" => {
            let value = parse_i64(name, value)?;
            if value < 0 {
                return Err(format!(
                    "Failed to set config value {name} to {value} - value must be non-negative"
                ));
            }
            Ok(Validated::Int(value))
        }
        "THREAD_COUNT"
        | "INDEX_WORKER_THREADS"
        | "OMP_THREAD_COUNT"
        | "CACHE_SIZE"
        | "NODE_CREATION_BUFFER"
        | "BOLT_PORT"
        | "IMPORT_FOLDER"
        | "TEMP_FOLDER" => {
            Err("This configuration parameter cannot be set at run-time".to_string())
        }
        _ => Err(format!("Unknown configuration field '{name}'")),
    }
}

fn parse_i64(name: &str, value: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .map_err(|_| format!("Failed to set config value {name} to {value}"))
}

const fn as_i64(value: &Validated) -> i64 {
    match value {
        Validated::Int(value) => *value,
        Validated::Uint(value) => *value as i64,
    }
}

const fn as_u64(value: &Validated) -> u64 {
    match value {
        Validated::Int(value) => *value as u64,
        Validated::Uint(value) => *value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_max_rejects_larger_per_query_timeout() {
        set_many(&[("TIMEOUT_MAX".into(), "10".into())]).unwrap();
        assert!(effective_timeout(Some(11), false).is_err());
        set_many(&[("TIMEOUT_MAX".into(), "0".into())]).unwrap();
    }

    #[test]
    fn result_set_size_round_trips() {
        set_many(&[("RESULTSET_SIZE".into(), "7".into())]).unwrap();
        match get("RESULTSET_SIZE").unwrap() {
            ConfigValue::Int(value) => assert_eq!(value, 7),
            ConfigValue::Text(_) => panic!("unexpected text config"),
        }
        set_many(&[("RESULTSET_SIZE".into(), "-1".into())]).unwrap();
    }
}
