//! Redis DUMP/RESTORE compatibility for FalkorDB module graph values.
//!
//! A Redis DUMP payload for a module value is:
//!   RDB_TYPE_MODULE_2, module-id, module opcodes..., EOF,
//!   2-byte RDB version, 8-byte Redis CRC64.
//!
//! FalkorDB's graphdata rdb_save emits its v19 graph stream as one or more
//! RedisModule_SaveStringBuffer records. This module unwraps those records
//! into the equivalent single-key GRAPH.RESTORE byte stream used by the
//! native host. It also performs the inverse operation for DUMP.

const RDB_TYPE_MODULE_2: u8 = 7;
const RDB_VERSION: u16 = 12;

const RDB_MODULE_OPCODE_EOF: u64 = 0;
const RDB_MODULE_OPCODE_STRING: u64 = 5;

const RDB_ENC_INT8: u64 = 0;
const RDB_ENC_INT16: u64 = 1;
const RDB_ENC_INT32: u64 = 2;
const RDB_ENC_LZF: u64 = 3;

const GRAPH_MODULE_NAME: &str = "graphdata";
const GRAPH_ENCODING_VERSION: u64 = 19;

const TYPE_BYTES: u8 = 0;
const TYPE_FLOAT: u8 = 1;
const TYPE_DOUBLE: u8 = 2;
const TYPE_SIGNED: u8 = 3;
const TYPE_UNSIGNED: u8 = 4;
const TYPE_BLOB: u8 = 6;

const MODULE_CHARSET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

pub fn extract_falkordb_v19_payload(dump: &[u8]) -> Result<Vec<u8>, String> {
    verify_dump_footer(dump)?;
    let body_end = dump.len() - 10;
    let mut r = RdbCursor::new(&dump[..body_end]);

    let object_type = r.read_u8()?;
    if object_type != RDB_TYPE_MODULE_2 {
        return Err(format!(
            "RESTORE payload is Redis object type {object_type}, expected FalkorDB module type {RDB_TYPE_MODULE_2}"
        ));
    }

    let (module_id, encoded) = r.read_len()?;
    if encoded {
        return Err("encoded FalkorDB module id is invalid".to_string());
    }
    let module_name = module_type_name(module_id);
    if module_name != GRAPH_MODULE_NAME {
        return Err(format!(
            "RESTORE module type is {module_name:?}, expected {GRAPH_MODULE_NAME:?}"
        ));
    }
    let encoding_version = module_id & 1023;
    if encoding_version != GRAPH_ENCODING_VERSION {
        return Err(format!(
            "unsupported FalkorDB graph encoding version {encoding_version}; expected {GRAPH_ENCODING_VERSION}"
        ));
    }

    let mut chunks = Vec::new();
    loop {
        let (opcode, encoded) = r.read_len()?;
        if encoded {
            return Err("encoded Redis module opcode is invalid".to_string());
        }
        match opcode {
            RDB_MODULE_OPCODE_EOF => break,
            RDB_MODULE_OPCODE_STRING => chunks.push(r.read_raw_string()?),
            other => {
                return Err(format!(
                    "unsupported FalkorDB module RDB opcode {other}; graphdata v19 should contain string chunks"
                ));
            }
        }
    }

    if !r.is_eof() {
        return Err(format!(
            "unexpected {} bytes after FalkorDB module EOF",
            r.remaining()
        ));
    }
    if chunks.is_empty() {
        return Err("FalkorDB RESTORE payload contains no graph chunks".to_string());
    }

    buffered_chunks_to_vec_payload(&chunks)
}

pub fn create_falkordb_dump(v19_payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v19_payload.len() + 64);
    out.push(RDB_TYPE_MODULE_2);
    write_len(&mut out, module_type_id(GRAPH_MODULE_NAME, GRAPH_ENCODING_VERSION));
    write_len(&mut out, RDB_MODULE_OPCODE_STRING);
    write_raw_string_uncompressed(&mut out, v19_payload);
    write_len(&mut out, RDB_MODULE_OPCODE_EOF);

    out.extend_from_slice(&RDB_VERSION.to_le_bytes());
    let crc = redis_crc64(&out);
    out.extend_from_slice(&crc.to_le_bytes());
    out
}

fn verify_dump_footer(dump: &[u8]) -> Result<(), String> {
    if dump.len() < 11 {
        return Err("DUMP payload is too short".to_string());
    }
    let footer = dump.len() - 10;
    let version = u16::from_le_bytes([dump[footer], dump[footer + 1]]);
    if version == 0 || version > RDB_VERSION {
        return Err(format!(
            "DUMP payload RDB version {version} is unsupported (max {RDB_VERSION})"
        ));
    }

    let expected = u64::from_le_bytes(
        dump[footer + 2..footer + 10]
            .try_into()
            .map_err(|_| "invalid DUMP CRC footer".to_string())?,
    );
    let actual = redis_crc64(&dump[..dump.len() - 8]);
    if actual != expected {
        return Err(format!(
            "DUMP payload CRC64 mismatch: expected {expected:016x}, computed {actual:016x}"
        ));
    }
    Ok(())
}

fn module_type_id(name: &str, encver: u64) -> u64 {
    debug_assert_eq!(name.len(), 9);
    let mut id = 0u64;
    for b in name.bytes() {
        let pos = MODULE_CHARSET
            .iter()
            .position(|&candidate| candidate == b)
            .expect("valid Redis module type character") as u64;
        id = (id << 6) | pos;
    }
    (id << 10) | encver
}

fn module_type_name(mut id: u64) -> String {
    id >>= 10;
    let mut name = [0u8; 9];
    for slot in name.iter_mut().rev() {
        *slot = MODULE_CHARSET[(id & 63) as usize];
        id >>= 6;
    }
    String::from_utf8(name.to_vec()).expect("Redis module charset is UTF-8")
}

fn write_len(out: &mut Vec<u8>, len: u64) {
    if len < (1 << 6) {
        out.push(len as u8);
    } else if len < (1 << 14) {
        out.push(((len >> 8) as u8) | 0x40);
        out.push(len as u8);
    } else if u32::try_from(len).is_ok() {
        out.push(0x80);
        out.extend_from_slice(&(len as u32).to_be_bytes());
    } else {
        out.push(0x81);
        out.extend_from_slice(&len.to_be_bytes());
    }
}

fn write_raw_string_uncompressed(out: &mut Vec<u8>, data: &[u8]) {
    write_len(out, data.len() as u64);
    out.extend_from_slice(data);
}

struct RdbCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RdbCursor<'a> {
    const fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn is_eof(&self) -> bool {
        self.pos == self.data.len()
    }

    fn read_u8(&mut self) -> Result<u8, String> {
        let value = *self
            .data
            .get(self.pos)
            .ok_or_else(|| "unexpected end of Redis DUMP payload".to_string())?;
        self.pos += 1;
        Ok(value)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8], String> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or_else(|| "Redis DUMP length overflow".to_string())?;
        let value = self
            .data
            .get(self.pos..end)
            .ok_or_else(|| format!("short Redis DUMP payload: need {len} bytes"))?;
        self.pos = end;
        Ok(value)
    }

    fn read_len(&mut self) -> Result<(u64, bool), String> {
        let first = self.read_u8()?;
        let kind = first >> 6;
        match kind {
            0 => Ok(((first & 0x3f) as u64, false)),
            1 => {
                let second = self.read_u8()?;
                Ok(((((first & 0x3f) as u64) << 8) | u64::from(second), false))
            }
            2 if first == 0x80 => {
                let bytes: [u8; 4] = self
                    .read_exact(4)?
                    .try_into()
                    .map_err(|_| "invalid 32-bit Redis RDB length".to_string())?;
                Ok((u64::from(u32::from_be_bytes(bytes)), false))
            }
            2 if first == 0x81 => {
                let bytes: [u8; 8] = self
                    .read_exact(8)?
                    .try_into()
                    .map_err(|_| "invalid 64-bit Redis RDB length".to_string())?;
                Ok((u64::from_be_bytes(bytes), false))
            }
            2 => Err(format!("unknown Redis RDB length encoding byte 0x{first:02x}")),
            3 => Ok(((first & 0x3f) as u64, true)),
            _ => unreachable!(),
        }
    }

    fn read_raw_string(&mut self) -> Result<Vec<u8>, String> {
        let (len_or_encoding, encoded) = self.read_len()?;
        if !encoded {
            let len = usize::try_from(len_or_encoding)
                .map_err(|_| "Redis string length does not fit usize".to_string())?;
            return Ok(self.read_exact(len)?.to_vec());
        }

        match len_or_encoding {
            RDB_ENC_INT8 => {
                let value = self.read_u8()? as i8;
                Ok(value.to_string().into_bytes())
            }
            RDB_ENC_INT16 => {
                let raw: [u8; 2] = self
                    .read_exact(2)?
                    .try_into()
                    .map_err(|_| "invalid Redis int16 string".to_string())?;
                Ok(i16::from_le_bytes(raw).to_string().into_bytes())
            }
            RDB_ENC_INT32 => {
                let raw: [u8; 4] = self
                    .read_exact(4)?
                    .try_into()
                    .map_err(|_| "invalid Redis int32 string".to_string())?;
                Ok(i32::from_le_bytes(raw).to_string().into_bytes())
            }
            RDB_ENC_LZF => {
                let (compressed_len, compressed_encoded) = self.read_len()?;
                let (original_len, original_encoded) = self.read_len()?;
                if compressed_encoded || original_encoded {
                    return Err("invalid encoded length inside Redis LZF string".to_string());
                }
                let compressed_len = usize::try_from(compressed_len)
                    .map_err(|_| "Redis LZF compressed length is too large".to_string())?;
                let original_len = usize::try_from(original_len)
                    .map_err(|_| "Redis LZF original length is too large".to_string())?;
                let compressed = self.read_exact(compressed_len)?;
                lzf_decompress(compressed, original_len)
            }
            other => Err(format!("unsupported Redis RDB string encoding {other}")),
        }
    }
}

fn buffered_chunks_to_vec_payload(chunks: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let total: usize = chunks.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    let mut chunk_index = 0usize;

    while chunk_index < chunks.len() {
        let chunk = &chunks[chunk_index];
        let mut pos = 0usize;
        while pos < chunk.len() {
            let tag = chunk[pos];
            pos += 1;
            match tag {
                TYPE_BYTES => {
                    let len_end = pos
                        .checked_add(8)
                        .ok_or_else(|| "FalkorDB chunk length overflow".to_string())?;
                    let len_raw: [u8; 8] = chunk
                        .get(pos..len_end)
                        .ok_or_else(|| "truncated FalkorDB byte-buffer length".to_string())?
                        .try_into()
                        .map_err(|_| "invalid FalkorDB byte-buffer length".to_string())?;
                    let len = usize::try_from(u64::from_le_bytes(len_raw))
                        .map_err(|_| "FalkorDB byte buffer is too large".to_string())?;
                    let end = len_end
                        .checked_add(len)
                        .ok_or_else(|| "FalkorDB byte-buffer overflow".to_string())?;
                    let data = chunk
                        .get(len_end..end)
                        .ok_or_else(|| "truncated FalkorDB inline byte buffer".to_string())?;
                    out.push(TYPE_BYTES);
                    out.extend_from_slice(&(len as u64).to_le_bytes());
                    out.extend_from_slice(data);
                    pos = end;
                }
                TYPE_FLOAT => copy_fixed(chunk, &mut pos, &mut out, tag, 4)?,
                TYPE_DOUBLE | TYPE_SIGNED | TYPE_UNSIGNED => {
                    copy_fixed(chunk, &mut pos, &mut out, tag, 8)?;
                }
                TYPE_BLOB => {
                    if pos != chunk.len() {
                        return Err(
                            "FalkorDB TYPE_BLOB sentinel was not at the end of its RDB chunk"
                                .to_string(),
                        );
                    }
                    let blob = chunks
                        .get(chunk_index + 1)
                        .ok_or_else(|| "FalkorDB TYPE_BLOB is missing its following chunk".to_string())?;
                    out.push(TYPE_BYTES);
                    out.extend_from_slice(&(blob.len() as u64).to_le_bytes());
                    out.extend_from_slice(blob);
                    chunk_index += 1;
                    break;
                }
                other => {
                    return Err(format!(
                        "unknown FalkorDB buffered graph type tag {other} in RDB payload"
                    ));
                }
            }
        }
        chunk_index += 1;
    }

    Ok(out)
}

fn copy_fixed(
    chunk: &[u8],
    pos: &mut usize,
    out: &mut Vec<u8>,
    tag: u8,
    len: usize,
) -> Result<(), String> {
    let end = pos
        .checked_add(len)
        .ok_or_else(|| "FalkorDB fixed-width value overflow".to_string())?;
    let value = chunk
        .get(*pos..end)
        .ok_or_else(|| "truncated FalkorDB fixed-width value".to_string())?;
    out.push(tag);
    out.extend_from_slice(value);
    *pos = end;
    Ok(())
}

fn lzf_decompress(input: &[u8], expected_len: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(expected_len);
    let mut ip = 0usize;

    while ip < input.len() {
        let ctrl = input[ip] as usize;
        ip += 1;

        if ctrl < 32 {
            let len = ctrl + 1;
            let end = ip
                .checked_add(len)
                .ok_or_else(|| "Redis LZF literal length overflow".to_string())?;
            let literals = input
                .get(ip..end)
                .ok_or_else(|| "truncated Redis LZF literal run".to_string())?;
            out.extend_from_slice(literals);
            ip = end;
            continue;
        }

        let mut len = ctrl >> 5;
        let high = (ctrl & 0x1f) << 8;
        let low = *input
            .get(ip)
            .ok_or_else(|| "truncated Redis LZF back-reference".to_string())? as usize;
        ip += 1;

        if len == 7 {
            len += *input
                .get(ip)
                .ok_or_else(|| "truncated Redis LZF extended length".to_string())?
                as usize;
            ip += 1;
        }
        len += 2;

        let offset = high | low;
        let reference = out
            .len()
            .checked_sub(offset + 1)
            .ok_or_else(|| "invalid Redis LZF back-reference offset".to_string())?;
        for i in 0..len {
            let byte = *out
                .get(reference + i)
                .ok_or_else(|| "invalid Redis LZF overlapping back-reference".to_string())?;
            out.push(byte);
            if out.len() > expected_len {
                return Err("Redis LZF output exceeds advertised length".to_string());
            }
        }
    }

    if out.len() != expected_len {
        return Err(format!(
            "Redis LZF length mismatch: decoded {}, expected {expected_len}",
            out.len()
        ));
    }
    Ok(out)
}

/// Redis' CRC64/Jones variant (poly 0xad93d23594c935a9, reflected output).
fn redis_crc64(data: &[u8]) -> u64 {
    const POLY: u64 = 0xad93_d235_94c9_35a9;
    let mut crc = 0u64;
    for &byte in data {
        for mask in [1u8, 2, 4, 8, 16, 32, 64, 128] {
            let high = (crc & 0x8000_0000_0000_0000) != 0;
            let input = (byte & mask) != 0;
            crc <<= 1;
            if high ^ input {
                crc ^= POLY;
            }
        }
    }
    crc.reverse_bits()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_id_roundtrip() {
        let id = module_type_id(GRAPH_MODULE_NAME, GRAPH_ENCODING_VERSION);
        assert_eq!(module_type_name(id), GRAPH_MODULE_NAME);
        assert_eq!(id & 1023, GRAPH_ENCODING_VERSION);
    }

    #[test]
    fn dump_roundtrip_preserves_v19_payload() {
        let payload = [
            TYPE_UNSIGNED, 1, 0, 0, 0, 0, 0, 0, 0,
            TYPE_BYTES, 3, 0, 0, 0, 0, 0, 0, 0, b'a', b'b', b'c',
        ];
        let dump = create_falkordb_dump(&payload);
        assert_eq!(extract_falkordb_v19_payload(&dump).unwrap(), payload);
    }

    #[test]
    fn redis_crc_known_vector() {
        // Redis' own crc64 implementation uses this Jones variant.
        assert_eq!(redis_crc64(b"123456789"), 0xe9c6_d914_c4b8_d9ca);
    }
}
