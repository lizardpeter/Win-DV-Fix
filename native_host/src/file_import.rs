use std::{
    fs::File,
    io::{BufRead, BufReader},
    path::Path,
};

use ring::digest::{Context, SHA256};
use serde_json::Value as JsonValue;

use crate::{NativeGraph, OutputStats};

const DEFAULT_BATCH_SIZE: usize = 1000;
const DEFAULT_MAX_BATCH_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_SIZE: usize = 10_000;
const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct JsonlFileInfo {
    pub size_bytes: u64,
    pub sha256: String,
    pub records: usize,
    pub physical_lines: usize,
}

#[derive(Debug, Clone, Default)]
pub struct ImportTotals {
    pub labels_added: usize,
    pub labels_removed: usize,
    pub nodes_created: u64,
    pub relationships_created: usize,
    pub nodes_deleted: u64,
    pub relationships_deleted: usize,
    pub properties_set: usize,
    pub properties_removed: usize,
    pub indexes_created: usize,
    pub indexes_dropped: usize,
}

impl ImportTotals {
    fn add(&mut self, stats: &OutputStats) {
        self.labels_added += stats.labels_added;
        self.labels_removed += stats.labels_removed;
        self.nodes_created += stats.nodes_created;
        self.relationships_created += stats.relationships_created;
        self.nodes_deleted += stats.nodes_deleted;
        self.relationships_deleted += stats.relationships_deleted;
        self.properties_set += stats.properties_set;
        self.properties_removed += stats.properties_removed;
        self.indexes_created += stats.indexes_created;
        self.indexes_dropped += stats.indexes_dropped;
    }
}

#[derive(Debug, Clone)]
pub struct JsonlImportReport {
    pub size_bytes: u64,
    pub sha256: String,
    pub records_total: usize,
    pub records_selected: usize,
    pub records_imported: usize,
    pub physical_lines: usize,
    pub start_record: usize,
    pub last_record: Option<usize>,
    pub batches: usize,
    pub dry_run: bool,
    pub checkpoint_file: Option<String>,
    pub totals: ImportTotals,
}

pub fn inspect_jsonl(path: &Path, expected_sha256: Option<&str>) -> Result<JsonlFileInfo, String> {
    let file = File::open(path)
        .map_err(|e| format!("open JSONL import file {}: {e}", path.display()))?;
    let size_bytes = file
        .metadata()
        .map_err(|e| format!("stat JSONL import file {}: {e}", path.display()))?
        .len();

    let mut reader = BufReader::new(file);
    let mut hash = Context::new(&SHA256);
    let mut buf = Vec::new();
    let mut records = 0usize;
    let mut physical_lines = 0usize;

    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("read JSONL import file {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hash.update(&buf);
        physical_lines += 1;
        let line = trim_ascii_whitespace(&buf);
        if line.is_empty() {
            continue;
        }
        let value: JsonValue = serde_json::from_slice(line).map_err(|e| {
            format!(
                "invalid JSON on physical line {physical_lines} of {}: {e}",
                path.display()
            )
        })?;
        if !value.is_object() {
            return Err(format!(
                "JSONL record on physical line {physical_lines} of {} is not an object",
                path.display()
            ));
        }
        records = records
            .checked_add(1)
            .ok_or_else(|| "JSONL record count overflow".to_string())?;
    }

    let sha256 = hash
        .finish()
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    if let Some(expected) = expected_sha256 {
        let expected = expected.trim().to_ascii_lowercase();
        if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("sha256 must be exactly 64 hexadecimal characters".to_string());
        }
        if expected != sha256 {
            return Err(format!(
                "JSONL SHA-256 mismatch: expected {expected}, computed {sha256}"
            ));
        }
    }

    Ok(JsonlFileInfo {
        size_bytes,
        sha256,
        records,
        physical_lines,
    })
}

#[allow(clippy::too_many_arguments)]
pub fn import_jsonl(
    graph: &NativeGraph,
    path: &Path,
    cypher_template: &str,
    expected_sha256: Option<&str>,
    batch_size: Option<usize>,
    max_batch_bytes: Option<usize>,
    start_record: Option<usize>,
    max_records: Option<usize>,
    dry_run: bool,
    checkpoint: bool,
) -> Result<JsonlImportReport, String> {
    validate_template(cypher_template)?;

    let batch_size = batch_size.unwrap_or(DEFAULT_BATCH_SIZE);
    if batch_size == 0 || batch_size > MAX_BATCH_SIZE {
        return Err(format!("batch_size must be between 1 and {MAX_BATCH_SIZE}"));
    }
    let max_batch_bytes = max_batch_bytes.unwrap_or(DEFAULT_MAX_BATCH_BYTES);
    if max_batch_bytes == 0 || max_batch_bytes > MAX_BATCH_BYTES {
        return Err(format!(
            "max_batch_bytes must be between 1 and {MAX_BATCH_BYTES}"
        ));
    }
    let start_record = start_record.unwrap_or(1);
    if start_record == 0 {
        return Err("start_record is 1-based and must be at least 1".to_string());
    }

    let info = inspect_jsonl(path, expected_sha256)?;
    let available = info.records.saturating_sub(start_record.saturating_sub(1));
    let records_selected = max_records.map_or(available, |limit| available.min(limit));

    if dry_run || records_selected == 0 {
        return Ok(JsonlImportReport {
            size_bytes: info.size_bytes,
            sha256: info.sha256,
            records_total: info.records,
            records_selected,
            records_imported: 0,
            physical_lines: info.physical_lines,
            start_record,
            last_record: None,
            batches: 0,
            dry_run,
            checkpoint_file: None,
            totals: ImportTotals::default(),
        });
    }

    let file = File::open(path)
        .map_err(|e| format!("open JSONL import file {}: {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut buf = Vec::new();
    let mut record_no = 0usize;
    let mut physical_line = 0usize;
    let mut selected = 0usize;
    let mut imported = 0usize;
    let mut batches = 0usize;
    let mut last_record = None;
    let mut totals = ImportTotals::default();

    let mut batch = Vec::<String>::with_capacity(batch_size.min(records_selected));
    let mut batch_bytes = 0usize;
    let mut batch_first_record = 0usize;
    let mut batch_last_record = 0usize;

    loop {
        buf.clear();
        let n = reader
            .read_until(b'\n', &mut buf)
            .map_err(|e| format!("read JSONL import file {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        physical_line += 1;
        let line = trim_ascii_whitespace(&buf);
        if line.is_empty() {
            continue;
        }

        record_no += 1;
        if record_no < start_record {
            continue;
        }
        if selected >= records_selected {
            break;
        }

        let value: JsonValue = serde_json::from_slice(line).map_err(|e| {
            format!(
                "JSONL changed after validation; invalid JSON on physical line {physical_line}: {e}"
            )
        })?;
        let literal = cypher_literal(&value)?;

        if !batch.is_empty()
            && (batch.len() >= batch_size
                || batch_bytes.saturating_add(literal.len() + 1) > max_batch_bytes)
        {
            let output = execute_batch(
                graph,
                cypher_template,
                &batch,
                batch_first_record,
                batch_last_record,
            )?;
            totals.add(&output.stats);
            imported += batch.len();
            batches += 1;
            last_record = Some(batch_last_record);
            batch.clear();
            batch_bytes = 0;
        }

        if batch.is_empty() {
            batch_first_record = record_no;
        }
        batch_last_record = record_no;
        batch_bytes = batch_bytes.saturating_add(literal.len() + 1);
        batch.push(literal);
        selected += 1;
    }

    if !batch.is_empty() {
        let output = execute_batch(
            graph,
            cypher_template,
            &batch,
            batch_first_record,
            batch_last_record,
        )?;
        totals.add(&output.stats);
        imported += batch.len();
        batches += 1;
        last_record = Some(batch_last_record);
    }

    if imported != records_selected {
        return Err(format!(
            "JSONL import selected {records_selected} records but committed {imported}; last committed record was {}",
            last_record.map_or_else(|| "none".to_string(), |v| v.to_string())
        ));
    }

    let checkpoint_file = if checkpoint && imported > 0 {
        let path = graph.checkpoint()?;
        Some(
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("")
                .to_string(),
        )
    } else {
        None
    };

    Ok(JsonlImportReport {
        size_bytes: info.size_bytes,
        sha256: info.sha256,
        records_total: info.records,
        records_selected,
        records_imported: imported,
        physical_lines: info.physical_lines,
        start_record,
        last_record,
        batches,
        dry_run: false,
        checkpoint_file,
        totals,
    })
}

fn execute_batch(
    graph: &NativeGraph,
    cypher_template: &str,
    batch: &[String],
    first_record: usize,
    last_record: usize,
) -> Result<crate::QueryOutput, String> {
    let rows = format!("[{}]", batch.join(","));
    let query = cypher_template.replace("{{ROWS}}", &rows);
    graph.query(&query).map_err(|e| {
        format!(
            "JSONL import batch failed for records {first_record}..={last_record}: {e}"
        )
    })
}

fn validate_template(cypher_template: &str) -> Result<(), String> {
    if cypher_template.trim().is_empty() {
        return Err("cypher template must not be empty".to_string());
    }
    let count = cypher_template.matches("{{ROWS}}").count();
    if count != 1 {
        return Err(format!(
            "cypher template must contain exactly one {{ROWS}} placeholder; found {count}"
        ));
    }
    Ok(())
}

fn trim_ascii_whitespace(mut bytes: &[u8]) -> &[u8] {
    while bytes.first().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[1..];
    }
    while bytes.last().is_some_and(u8::is_ascii_whitespace) {
        bytes = &bytes[..bytes.len() - 1];
    }
    bytes
}

fn cypher_literal(value: &JsonValue) -> Result<String, String> {
    match value {
        JsonValue::Null => Ok("null".to_string()),
        JsonValue::Bool(value) => Ok(if *value { "true" } else { "false" }.to_string()),
        JsonValue::Number(value) => Ok(value.to_string()),
        JsonValue::String(value) => Ok(cypher_string(value)),
        JsonValue::Array(values) => {
            let mut out = String::from("[");
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push_str(&cypher_literal(value)?);
            }
            out.push(']');
            Ok(out)
        }
        JsonValue::Object(values) => {
            let mut out = String::from("{");
            for (index, (key, value)) in values.iter().enumerate() {
                if index != 0 {
                    out.push(',');
                }
                out.push('`');
                out.push_str(&key.replace('`', "``"));
                out.push_str("`:");
                out.push_str(&cypher_literal(value)?);
            }
            out.push('}');
            Ok(out)
        }
    }
}

fn cypher_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\0' => out.push_str("\\0"),
            _ => out.push(ch),
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_requires_exactly_one_rows_placeholder() {
        assert!(validate_template("UNWIND {{ROWS}} AS row RETURN row").is_ok());
        assert!(validate_template("RETURN 1").is_err());
        assert!(validate_template("{{ROWS}} {{ROWS}}").is_err());
    }
}
