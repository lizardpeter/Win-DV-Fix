use std::{
    collections::HashMap,
    env,
    fs,
    path::{Component, Path, PathBuf},
};

use ring::digest::{digest, SHA256};

const RDB_OPCODE_SLOT_INFO: u8 = 244;
const RDB_OPCODE_FUNCTION2: u8 = 245;
const RDB_OPCODE_FUNCTION_PRE_GA: u8 = 246;
const RDB_OPCODE_MODULE_AUX: u8 = 247;
const RDB_OPCODE_IDLE: u8 = 248;
const RDB_OPCODE_FREQ: u8 = 249;
const RDB_OPCODE_AUX: u8 = 250;
const RDB_OPCODE_RESIZEDB: u8 = 251;
const RDB_OPCODE_EXPIRETIME_MS: u8 = 252;
const RDB_OPCODE_EXPIRETIME: u8 = 253;
const RDB_OPCODE_SELECTDB: u8 = 254;
const RDB_OPCODE_EOF: u8 = 255;

const RDB_TYPE_MODULE_PRE_GA: u8 = 6;
const RDB_TYPE_MODULE_2: u8 = 7;
// Redis stream encodings used by FalkorDB's per-graph telemetry key.
const RDB_TYPE_STREAM_LISTPACKS_4: u8 = 26;
const RDB_TYPE_STREAM_LISTPACKS_5: u8 = 27;

const RDB_MODULE_OPCODE_EOF: u64 = 0;
const RDB_MODULE_OPCODE_SINT: u64 = 1;
const RDB_MODULE_OPCODE_UINT: u64 = 2;
const RDB_MODULE_OPCODE_FLOAT: u64 = 3;
const RDB_MODULE_OPCODE_DOUBLE: u64 = 4;
const RDB_MODULE_OPCODE_STRING: u64 = 5;

const RDB_ENC_INT8: u64 = 0;
const RDB_ENC_INT16: u64 = 1;
const RDB_ENC_INT32: u64 = 2;
const RDB_ENC_LZF: u64 = 3;

const MODULE_CHARSET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

const TYPE_BYTES: u8 = 0;
const TYPE_FLOAT: u8 = 1;
const TYPE_DOUBLE: u8 = 2;
const TYPE_SIGNED: u8 = 3;
const TYPE_UNSIGNED: u8 = 4;
const TYPE_LONG_DOUBLE: u8 = 5;
const TYPE_BLOB: u8 = 6;

#[derive(Debug, Clone)]
pub struct RdbGraph {
    pub name: String,
    pub node_count: u64,
    pub edge_count: u64,
    pub key_count: u64,
    pub fragments: Vec<Vec<u8>>,
}

#[derive(Debug)]
pub struct ParsedRdb {
    pub version: u32,
    pub size_bytes: u64,
    pub sha256_hex: String,
    pub graphs: Vec<RdbGraph>,
    pub udfs: HashMap<String, String>,
    /// FalkorDB-owned auxiliary Redis keys intentionally not imported into
    /// the native graph catalog. At present this is restricted to the
    /// telemetry{graph} stream emitted beside a graph.
    pub ignored_aux_keys: Vec<String>,
}

#[derive(Debug, Clone)]
struct GraphHeader {
    name: String,
    node_count: u64,
    edge_count: u64,
    key_count: u64,
}

#[derive(Debug)]
enum ModuleRecord {
    Sint(u64),
    Uint(u64),
    Float(Vec<u8>),
    Double(Vec<u8>),
    Bytes(Vec<u8>),
}

struct RdbReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RdbReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "RDB offset overflow".to_string())?;
        if end > self.data.len() {
            return Err(format!(
                "truncated RDB at offset {}: need {} bytes, have {}",
                self.pos,
                n,
                self.remaining()
            ));
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.read(1)?[0])
    }

    fn read_len(&mut self) -> Result<(u64, bool), String> {
        let first = self.u8()?;
        match first >> 6 {
            0 => Ok(((first & 0x3f) as u64, false)),
            1 => Ok(((((first & 0x3f) as u64) << 8) | self.u8()? as u64, false)),
            2 => match first {
                0x80 => {
                    let bytes: [u8; 4] = self
                        .read(4)?
                        .try_into()
                        .map_err(|_| "invalid 32-bit RDB length".to_string())?;
                    Ok((u32::from_be_bytes(bytes) as u64, false))
                }
                0x81 => {
                    let bytes: [u8; 8] = self
                        .read(8)?
                        .try_into()
                        .map_err(|_| "invalid 64-bit RDB length".to_string())?;
                    Ok((u64::from_be_bytes(bytes), false))
                }
                _ => Err(format!("unknown RDB length encoding byte 0x{first:02x}")),
            },
            _ => Ok(((first & 0x3f) as u64, true)),
        }
    }

    fn string(&mut self) -> Result<Vec<u8>, String> {
        let (value, encoded) = self.read_len()?;
        if !encoded {
            let len = usize::try_from(value)
                .map_err(|_| format!("RDB string length {value} does not fit usize"))?;
            return Ok(self.read(len)?.to_vec());
        }

        match value {
            RDB_ENC_INT8 => {
                let n = i8::from_le_bytes([self.u8()?]);
                Ok(n.to_string().into_bytes())
            }
            RDB_ENC_INT16 => {
                let bytes: [u8; 2] = self
                    .read(2)?
                    .try_into()
                    .map_err(|_| "invalid encoded int16".to_string())?;
                Ok(i16::from_le_bytes(bytes).to_string().into_bytes())
            }
            RDB_ENC_INT32 => {
                let bytes: [u8; 4] = self
                    .read(4)?
                    .try_into()
                    .map_err(|_| "invalid encoded int32".to_string())?;
                Ok(i32::from_le_bytes(bytes).to_string().into_bytes())
            }
            RDB_ENC_LZF => {
                let (compressed_len, compressed_encoded) = self.read_len()?;
                let (original_len, original_encoded) = self.read_len()?;
                if compressed_encoded || original_encoded {
                    return Err("encoded length inside RDB LZF string".to_string());
                }
                let compressed_len = usize::try_from(compressed_len)
                    .map_err(|_| "RDB LZF compressed length too large".to_string())?;
                let original_len = usize::try_from(original_len)
                    .map_err(|_| "RDB LZF original length too large".to_string())?;
                lzf_decompress(self.read(compressed_len)?, original_len)
            }
            _ => Err(format!("unsupported RDB string encoding {value}")),
        }
    }
}

fn lzf_decompress(data: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected_len);
    let mut ip = 0usize;

    while ip < data.len() {
        let ctrl = data[ip];
        ip += 1;
        if ctrl < 32 {
            let length = ctrl as usize + 1;
            let end = ip
                .checked_add(length)
                .ok_or_else(|| "RDB LZF literal overflow".to_string())?;
            if end > data.len() {
                return Err("truncated RDB LZF literal".to_string());
            }
            out.extend_from_slice(&data[ip..end]);
            ip = end;
            continue;
        }

        let mut length = (ctrl >> 5) as usize;
        let high = ((ctrl & 0x1f) as usize) << 8;
        if length == 7 {
            if ip >= data.len() {
                return Err("truncated RDB LZF extended length".to_string());
            }
            length += data[ip] as usize;
            ip += 1;
        }
        if ip >= data.len() {
            return Err("truncated RDB LZF offset".to_string());
        }
        let low = data[ip] as usize;
        ip += 1;
        length += 2;

        let offset = high | low;
        let reference = out
            .len()
            .checked_sub(offset + 1)
            .ok_or_else(|| "invalid RDB LZF back-reference".to_string())?;

        for i in 0..length {
            let source = reference + i;
            if source >= out.len() {
                return Err("invalid overlapping RDB LZF back-reference".to_string());
            }
            let byte = out[source];
            out.push(byte);
            if out.len() > expected_len {
                return Err("RDB LZF output exceeds advertised length".to_string());
            }
        }
    }

    if out.len() != expected_len {
        return Err(format!(
            "RDB LZF output length {} != expected {}",
            out.len(),
            expected_len
        ));
    }
    Ok(out)
}

fn redis_crc64(data: &[u8]) -> u64 {
    const POLY: u64 = 0xAD93_D235_94C9_35A9;
    let mut crc = 0u64;
    for &byte in data {
        for mask in [1u8, 2, 4, 8, 16, 32, 64, 128] {
            let high = (crc & 0x8000_0000_0000_0000) != 0;
            let bit = (byte & mask) != 0;
            crc <<= 1;
            if high ^ bit {
                crc ^= POLY;
            }
        }
    }
    crc.reverse_bits()
}

fn module_name(module_id: u64) -> String {
    let mut value = module_id >> 10;
    let mut chars = [0u8; 9];
    for index in (0..9).rev() {
        chars[index] = MODULE_CHARSET[(value & 63) as usize];
        value >>= 6;
    }
    String::from_utf8_lossy(&chars).into_owned()
}

fn read_module_records(reader: &mut RdbReader<'_>) -> Result<Vec<ModuleRecord>, String> {
    let mut records = Vec::new();
    loop {
        let (opcode, encoded) = reader.read_len()?;
        if encoded {
            return Err("encoded module opcode is invalid".to_string());
        }
        match opcode {
            RDB_MODULE_OPCODE_EOF => return Ok(records),
            RDB_MODULE_OPCODE_SINT => {
                let (value, value_encoded) = reader.read_len()?;
                if value_encoded {
                    return Err("encoded module integer is invalid".to_string());
                }
                records.push(ModuleRecord::Sint(value));
            }
            RDB_MODULE_OPCODE_UINT => {
                let (value, value_encoded) = reader.read_len()?;
                if value_encoded {
                    return Err("encoded module integer is invalid".to_string());
                }
                records.push(ModuleRecord::Uint(value));
            }
            RDB_MODULE_OPCODE_STRING => records.push(ModuleRecord::Bytes(reader.string()?)),
            RDB_MODULE_OPCODE_FLOAT => records.push(ModuleRecord::Float(reader.read(4)?.to_vec())),
            RDB_MODULE_OPCODE_DOUBLE => {
                records.push(ModuleRecord::Double(reader.read(8)?.to_vec()))
            }
            _ => return Err(format!("unknown Redis module opcode {opcode}")),
        }
    }
}

fn buffered_chunks_to_v19(chunks: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if chunks.is_empty() {
        return Err("FalkorDB module value contains no serializer chunks".to_string());
    }

    let mut out = Vec::new();
    let mut chunk_index = 0usize;
    while chunk_index < chunks.len() {
        let chunk = &chunks[chunk_index];
        let mut pos = 0usize;
        while pos < chunk.len() {
            let tag = chunk[pos];
            pos += 1;
            match tag {
                TYPE_BYTES => {
                    if pos + 8 > chunk.len() {
                        return Err("truncated FalkorDB byte-buffer length".to_string());
                    }
                    let length = u64::from_le_bytes(
                        chunk[pos..pos + 8]
                            .try_into()
                            .map_err(|_| "invalid FalkorDB byte-buffer length".to_string())?,
                    );
                    pos += 8;
                    let length = usize::try_from(length)
                        .map_err(|_| "FalkorDB byte buffer too large".to_string())?;
                    let end = pos
                        .checked_add(length)
                        .ok_or_else(|| "FalkorDB byte buffer overflow".to_string())?;
                    if end > chunk.len() {
                        return Err("truncated FalkorDB byte buffer".to_string());
                    }
                    out.push(TYPE_BYTES);
                    out.extend_from_slice(&(length as u64).to_le_bytes());
                    out.extend_from_slice(&chunk[pos..end]);
                    pos = end;
                }
                TYPE_FLOAT => {
                    let end = pos + 4;
                    if end > chunk.len() {
                        return Err("truncated FalkorDB float".to_string());
                    }
                    out.push(tag);
                    out.extend_from_slice(&chunk[pos..end]);
                    pos = end;
                }
                TYPE_DOUBLE | TYPE_SIGNED | TYPE_UNSIGNED => {
                    let end = pos + 8;
                    if end > chunk.len() {
                        return Err("truncated FalkorDB fixed-width value".to_string());
                    }
                    out.push(tag);
                    out.extend_from_slice(&chunk[pos..end]);
                    pos = end;
                }
                TYPE_LONG_DOUBLE => {
                    return Err(
                        "unexpected long-double tag in current FalkorDB graphdata v19".to_string(),
                    );
                }
                TYPE_BLOB => {
                    if pos != chunk.len() {
                        return Err(
                            "FalkorDB blob sentinel was not at end of serializer chunk".to_string(),
                        );
                    }
                    chunk_index += 1;
                    if chunk_index >= chunks.len() {
                        return Err("FalkorDB blob sentinel has no following chunk".to_string());
                    }
                    let blob = &chunks[chunk_index];
                    out.push(TYPE_BYTES);
                    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
                    out.extend_from_slice(blob);
                    break;
                }
                _ => return Err(format!("unknown FalkorDB serializer type tag {tag}")),
            }
        }
        chunk_index += 1;
    }
    Ok(out)
}

struct V19Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> V19Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| "FalkorDB v19 header offset overflow".to_string())?;
        if end > self.data.len() {
            return Err("truncated FalkorDB v19 fragment header".to_string());
        }
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn buffer(&mut self) -> Result<Vec<u8>, String> {
        if self.take(1)?[0] != TYPE_BYTES {
            return Err("FalkorDB v19 header expected byte-buffer tag".to_string());
        }
        let length = u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| "invalid FalkorDB v19 buffer length".to_string())?,
        );
        let length = usize::try_from(length)
            .map_err(|_| "FalkorDB v19 buffer length too large".to_string())?;
        Ok(self.take(length)?.to_vec())
    }

    fn unsigned(&mut self) -> Result<u64, String> {
        if self.take(1)?[0] != TYPE_UNSIGNED {
            return Err("FalkorDB v19 header expected unsigned tag".to_string());
        }
        Ok(u64::from_le_bytes(
            self.take(8)?
                .try_into()
                .map_err(|_| "invalid FalkorDB v19 unsigned value".to_string())?,
        ))
    }
}

fn parse_fragment_header(payload: &[u8]) -> Result<GraphHeader, String> {
    let mut reader = V19Reader {
        data: payload,
        pos: 0,
    };
    let mut name_bytes = reader.buffer()?;
    if name_bytes.last() == Some(&0) {
        name_bytes.pop();
    }
    let name = String::from_utf8(name_bytes)
        .map_err(|_| "FalkorDB v19 graph name is not UTF-8".to_string())?;
    let node_count = reader.unsigned()?;
    let edge_count = reader.unsigned()?;
    let _deleted_nodes = reader.unsigned()?;
    let _deleted_edges = reader.unsigned()?;
    let _label_count = reader.unsigned()?;
    let relationship_count = reader.unsigned()?;
    for _ in 0..relationship_count {
        reader.unsigned()?;
    }
    let key_count = reader.unsigned()?;
    Ok(GraphHeader {
        name,
        node_count,
        edge_count,
        key_count,
    })
}

fn parse_falkordb_aux(
    module: &str,
    encver: u64,
    records: &[ModuleRecord],
    udfs: &mut HashMap<String, String>,
) -> Result<(), String> {
    if module != "graphdata" {
        return Err(format!(
            "RDB contains unsupported module AUX data for {module:?}"
        ));
    }
    if encver != 19 {
        return Err(format!(
            "RDB contains FalkorDB graphdata AUX version {encver}; current raw importer requires v19"
        ));
    }

    if records.len() < 2 {
        return Ok(());
    }
    if !matches!(records[0], ModuleRecord::Uint(_)) {
        return Err("FalkorDB module AUX has invalid when opcode".to_string());
    }

    let body = &records[1..];
    if body.is_empty() {
        return Ok(());
    }
    let count = match body[0] {
        ModuleRecord::Uint(value) => value,
        _ => return Err("FalkorDB module AUX expected UDF count".to_string()),
    };
    if count == 0 {
        return Ok(());
    }

    let count = usize::try_from(count).map_err(|_| "FalkorDB UDF count too large".to_string())?;
    let expected = 1usize
        .checked_add(
            count
                .checked_mul(2)
                .ok_or_else(|| "FalkorDB UDF count overflow".to_string())?,
        )
        .ok_or_else(|| "FalkorDB UDF record count overflow".to_string())?;
    if body.len() != expected {
        return Err(format!(
            "FalkorDB UDF AUX declared {count} libraries but has {} value records",
            body.len().saturating_sub(1)
        ));
    }

    for i in 0..count {
        let name = match &body[1 + i * 2] {
            ModuleRecord::Bytes(bytes) => trim_nul_utf8(bytes, "UDF name")?,
            _ => return Err("FalkorDB UDF AUX expected name/script strings".to_string()),
        };
        let script = match &body[2 + i * 2] {
            ModuleRecord::Bytes(bytes) => trim_nul_utf8(bytes, "UDF script")?,
            _ => return Err("FalkorDB UDF AUX expected name/script strings".to_string()),
        };
        if let Some(existing) = udfs.get(&name) {
            if existing != &script {
                return Err(format!("conflicting UDF definitions for {name:?}"));
            }
        } else {
            udfs.insert(name, script);
        }
    }
    Ok(())
}

fn trim_nul_utf8(bytes: &[u8], label: &str) -> Result<String, String> {
    let end = bytes
        .iter()
        .rposition(|b| *b != 0)
        .map_or(0, |index| index + 1);
    String::from_utf8(bytes[..end].to_vec())
        .map_err(|_| format!("{label} is not valid UTF-8"))
}


fn read_plain_len(reader: &mut RdbReader<'_>, label: &str) -> Result<u64, String> {
    let (value, encoded) = reader.read_len()?;
    if encoded {
        return Err(format!("encoded RDB length where {label} was expected"));
    }
    Ok(value)
}

/// Consume a Redis stream value without materializing it. FalkorDB writes
/// one telemetry{graph} stream alongside graph module keys. We preserve
/// fail-closed behavior by accepting stream encodings 26/27 only for that
/// exact auxiliary key shape; arbitrary Redis streams remain rejected.
fn skip_stream_value(reader: &mut RdbReader<'_>, record_type: u8) -> Result<(), String> {
    let listpack_count = read_plain_len(reader, "stream listpack count")?;
    for _ in 0..listpack_count {
        reader.string()?; // radix-tree node key
        reader.string()?; // serialized listpack
    }

    // Common stream metadata for current stream RDB encodings.
    read_plain_len(reader, "stream length")?;
    read_plain_len(reader, "stream last-id ms")?;
    read_plain_len(reader, "stream last-id seq")?;
    read_plain_len(reader, "stream first-id ms")?;
    read_plain_len(reader, "stream first-id seq")?;
    read_plain_len(reader, "stream max-deleted-id ms")?;
    read_plain_len(reader, "stream max-deleted-id seq")?;
    read_plain_len(reader, "stream entries-added")?;

    let group_count = read_plain_len(reader, "stream consumer-group count")?;
    for _ in 0..group_count {
        reader.string()?; // group name
        read_plain_len(reader, "consumer-group last-id ms")?;
        read_plain_len(reader, "consumer-group last-id seq")?;
        read_plain_len(reader, "consumer-group entries-read")?;

        let pel_count = read_plain_len(reader, "consumer-group PEL count")?;
        for _ in 0..pel_count {
            reader.read(16)?; // stream ID
            reader.read(8)?;  // delivery time (mstime_t)
            read_plain_len(reader, "consumer-group delivery count")?;
        }

        let consumer_count = read_plain_len(reader, "stream consumer count")?;
        for _ in 0..consumer_count {
            reader.string()?; // consumer name
            reader.read(8)?;  // seen time
            reader.read(8)?;  // active time
            let local_pel_count = read_plain_len(reader, "consumer PEL count")?;
            for _ in 0..local_pel_count {
                reader.read(16)?;
            }
        }

        if record_type >= RDB_TYPE_STREAM_LISTPACKS_5 {
            let nack_zone_count = read_plain_len(reader, "stream NACK zone count")?;
            let bytes = usize::try_from(nack_zone_count)
                .ok()
                .and_then(|value| value.checked_mul(16))
                .ok_or_else(|| "stream NACK zone byte count overflow".to_string())?;
            reader.read(bytes)?;
        }
    }

    // STREAM_LISTPACKS_4 added IDMP state.
    read_plain_len(reader, "stream IDMP duration")?;
    read_plain_len(reader, "stream IDMP max entries")?;
    let producer_count = read_plain_len(reader, "stream IDMP producer count")?;
    for _ in 0..producer_count {
        reader.string()?; // producer id
        let entry_count = read_plain_len(reader, "stream IDMP producer entry count")?;
        for _ in 0..entry_count {
            reader.string()?; // IID
            read_plain_len(reader, "stream IDMP entry ms")?;
            read_plain_len(reader, "stream IDMP entry seq")?;
        }
    }
    read_plain_len(reader, "stream IDMP IIDs added")?;
    read_plain_len(reader, "stream IDMP IIDs duplicates")?;
    Ok(())
}

fn module_records_as_chunks(records: Vec<ModuleRecord>, key: &str) -> Result<Vec<Vec<u8>>, String> {
    let mut chunks = Vec::with_capacity(records.len());
    for record in records {
        match record {
            ModuleRecord::Bytes(bytes) => chunks.push(bytes),
            ModuleRecord::Sint(value) => {
                return Err(format!(
                    "FalkorDB graph key {key:?} contains unexpected signed module record {value}"
                ));
            }
            ModuleRecord::Uint(value) => {
                return Err(format!(
                    "FalkorDB graph key {key:?} contains unexpected unsigned module record {value}"
                ));
            }
            ModuleRecord::Float(bytes) => {
                return Err(format!(
                    "FalkorDB graph key {key:?} contains unexpected {}-byte float module record",
                    bytes.len()
                ));
            }
            ModuleRecord::Double(bytes) => {
                return Err(format!(
                    "FalkorDB graph key {key:?} contains unexpected {}-byte double module record",
                    bytes.len()
                ));
            }
        }
    }
    Ok(chunks)
}

pub fn resolve_import_file(data_dir: &Path, file: &str) -> Result<(PathBuf, PathBuf), String> {
    if file.trim().is_empty() {
        return Err("file must be a non-empty relative path".to_string());
    }
    let relative = Path::new(file);
    if relative.is_absolute()
        || relative.components().any(|component| {
            !matches!(component, Component::Normal(_))
        })
    {
        return Err("file must be a relative path below the configured FalkorDB import directory".to_string());
    }

    let root = env::var_os("FALKORDB_IMPORT_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("imports"));
    fs::create_dir_all(&root)
        .map_err(|e| format!("create RDB import directory {}: {e}", root.display()))?;
    let root = root
        .canonicalize()
        .map_err(|e| format!("canonicalize RDB import directory {}: {e}", root.display()))?;

    let candidate = root.join(relative);
    let candidate = candidate.canonicalize().map_err(|e| {
        format!(
            "RDB import file {file:?} was not found below {}: {e}",
            root.display()
        )
    })?;
    if !candidate.starts_with(&root) {
        return Err("RDB import file escaped the configured import directory".to_string());
    }
    if !candidate.is_file() {
        return Err(format!("RDB import path is not a file: {}", candidate.display()));
    }
    Ok((root, candidate))
}

pub fn parse_rdb_file(path: &Path) -> Result<ParsedRdb, String> {
    let data = fs::read(path).map_err(|e| format!("read RDB {}: {e}", path.display()))?;
    if data.len() < 17 || !data.starts_with(b"REDIS") {
        return Err("not a Redis RDB file".to_string());
    }

    let version_text = std::str::from_utf8(&data[5..9])
        .map_err(|_| "invalid Redis RDB version header".to_string())?;
    let version: u32 = version_text
        .parse()
        .map_err(|_| "invalid Redis RDB version header".to_string())?;
    if !(5..=15).contains(&version) {
        return Err(format!("unsupported Redis RDB version {version}"));
    }

    let hash = digest(&SHA256, &data);
    let sha256_hex = hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let crc_offset = data.len() - 8;
    let stored_crc = u64::from_le_bytes(
        data[crc_offset..]
            .try_into()
            .map_err(|_| "invalid RDB checksum trailer".to_string())?,
    );
    if stored_crc != 0 {
        let actual_crc = redis_crc64(&data[..crc_offset]);
        if actual_crc != stored_crc {
            return Err(format!(
                "RDB CRC64 mismatch: expected {stored_crc:016x}, computed {actual_crc:016x}"
            ));
        }
    }

    let mut reader = RdbReader::new(&data[..crc_offset]);
    let magic = reader.read(9)?;
    if magic != &data[..9] {
        return Err("RDB header read mismatch".to_string());
    }

    let mut graph_order: Vec<RdbGraph> = Vec::new();
    let mut graph_indices: HashMap<String, usize> = HashMap::new();
    let mut udfs = HashMap::new();
    let mut ignored_aux_keys = Vec::<String>::new();
    let mut current_db = 0u64;
    let mut pending_expire = false;

    loop {
        let record_type = reader.u8()?;
        match record_type {
            RDB_OPCODE_EOF => {
                if reader.remaining() != 0 {
                    return Err(format!(
                        "unexpected {} bytes after RDB EOF",
                        reader.remaining()
                    ));
                }
                break;
            }
            RDB_OPCODE_AUX => {
                reader.string()?;
                reader.string()?;
                continue;
            }
            RDB_OPCODE_RESIZEDB => {
                reader.read_len()?;
                reader.read_len()?;
                continue;
            }
            RDB_OPCODE_SELECTDB => {
                let (value, encoded) = reader.read_len()?;
                if encoded {
                    return Err("encoded SELECTDB value".to_string());
                }
                current_db = value;
                continue;
            }
            RDB_OPCODE_EXPIRETIME_MS => {
                reader.read(8)?;
                pending_expire = true;
                continue;
            }
            RDB_OPCODE_EXPIRETIME => {
                reader.read(4)?;
                pending_expire = true;
                continue;
            }
            RDB_OPCODE_IDLE => {
                reader.read_len()?;
                continue;
            }
            RDB_OPCODE_FREQ => {
                reader.read(1)?;
                continue;
            }
            RDB_OPCODE_SLOT_INFO => {
                reader.read_len()?;
                reader.read_len()?;
                reader.read_len()?;
                continue;
            }
            RDB_OPCODE_FUNCTION_PRE_GA => {
                return Err("pre-release Redis function RDB format is unsupported".to_string());
            }
            RDB_OPCODE_FUNCTION2 => {
                let code = reader.string()?;
                return Err(format!(
                    "RDB contains a Redis FUNCTION library ({} bytes); native graph host cannot preserve it",
                    code.len()
                ));
            }
            RDB_OPCODE_MODULE_AUX => {
                let (module_id, encoded) = reader.read_len()?;
                if encoded {
                    return Err("encoded module AUX id".to_string());
                }
                let records = read_module_records(&mut reader)?;
                parse_falkordb_aux(
                    &module_name(module_id),
                    module_id & 1023,
                    &records,
                    &mut udfs,
                )?;
                continue;
            }
            _ => {}
        }

        let key_bytes = reader.string()?;
        let key_text = String::from_utf8_lossy(&key_bytes).into_owned();

        if pending_expire {
            return Err(format!(
                "RDB graph/key {key_text:?} has an expiration; the native graph catalog does not preserve Redis key TTL semantics"
            ));
        }
        if current_db != 0 {
            return Err(format!(
                "RDB contains key {key_text:?} in Redis DB {current_db}; native FalkorDB host raw import requires DB 0"
            ));
        }
        if matches!(
            record_type,
            RDB_TYPE_STREAM_LISTPACKS_4 | RDB_TYPE_STREAM_LISTPACKS_5
        ) && key_text.starts_with("telemetry{")
            && key_text.ends_with('}')
        {
            skip_stream_value(&mut reader, record_type)?;
            ignored_aux_keys.push(key_text);
            pending_expire = false;
            continue;
        }

        if record_type == RDB_TYPE_MODULE_PRE_GA {
            return Err(format!(
                "key {key_text:?} uses unsupported pre-GA Redis module format"
            ));
        }
        if record_type != RDB_TYPE_MODULE_2 {
            return Err(format!(
                "RDB contains ordinary Redis key {key_text:?} of type {record_type}; raw FalkorDB import refuses to silently drop non-graph Redis data"
            ));
        }

        let (module_id, encoded) = reader.read_len()?;
        if encoded {
            return Err(format!("encoded module id for key {key_text:?}"));
        }
        let module = module_name(module_id);
        let encver = module_id & 1023;
        let records = read_module_records(&mut reader)?;

        if module != "graphdata" && module != "graphmeta" {
            return Err(format!(
                "RDB key {key_text:?} uses unsupported module {module:?}"
            ));
        }
        if encver != 19 {
            return Err(format!(
                "RDB key {key_text:?} uses FalkorDB {module} encoding v{encver}; current raw importer requires v19"
            ));
        }

        let chunks = module_records_as_chunks(records, &key_text)?;
        let payload = buffered_chunks_to_v19(&chunks)?;
        let header = parse_fragment_header(&payload)?;

        if let Some(&index) = graph_indices.get(&header.name) {
            let graph = &mut graph_order[index];
            if graph.node_count != header.node_count
                || graph.edge_count != header.edge_count
                || graph.key_count != header.key_count
            {
                return Err(format!(
                    "inconsistent RDB fragment header for graph {:?}",
                    header.name
                ));
            }
            graph.fragments.push(payload);
        } else {
            let index = graph_order.len();
            graph_indices.insert(header.name.clone(), index);
            graph_order.push(RdbGraph {
                name: header.name,
                node_count: header.node_count,
                edge_count: header.edge_count,
                key_count: header.key_count,
                fragments: vec![payload],
            });
        }
        pending_expire = false;
    }

    if graph_order.is_empty() {
        return Err("RDB contains no current FalkorDB graphdata/graphmeta keys".to_string());
    }

    for key in &ignored_aux_keys {
        let graph_name = key
            .strip_prefix("telemetry{")
            .and_then(|value| value.strip_suffix('}'))
            .ok_or_else(|| format!("invalid FalkorDB telemetry key {key:?}"))?;
        if !graph_order.iter().any(|graph| graph.name == graph_name) {
            return Err(format!(
                "telemetry key {key:?} does not correspond to a graph in this RDB"
            ));
        }
    }
    for graph in &graph_order {
        if graph.fragments.len() as u64 != graph.key_count {
            return Err(format!(
                "graph {:?} is incomplete in RDB: {}/{} fragments",
                graph.name,
                graph.fragments.len(),
                graph.key_count
            ));
        }
    }

    Ok(ParsedRdb {
        version,
        size_bytes: data.len() as u64,
        sha256_hex,
        graphs: graph_order,
        udfs,
        ignored_aux_keys,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc64_matches_known_empty_and_stable_value() {
        assert_eq!(redis_crc64(b""), 0);
        assert_eq!(redis_crc64(b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }

    #[test]
    fn rejects_import_path_traversal_before_io() {
        let data_dir = Path::new(".");
        assert!(resolve_import_file(data_dir, "../secret.rdb").is_err());
        assert!(resolve_import_file(data_dir, "/absolute.rdb").is_err());
    }
}
