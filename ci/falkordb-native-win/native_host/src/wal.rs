use std::{
    fs::{self, File, OpenOptions},
    io::{Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use crc32fast::Hasher;
use graph::effects::{EffectsBuffer, ReplicationSink};
use parking_lot::Mutex;

const MAGIC: &[u8; 4] = b"FGWL";
const WAL_VERSION: u16 = 1;
const HEADER_LEN: usize = 4 + 2 + 8 + 4 + 8 + 4;

#[derive(Debug, Clone)]
pub struct WalRecord {
    pub sequence: u64,
    pub key: Vec<u8>,
    pub payload: Vec<u8>,
}

struct WalState {
    file: File,
    next_sequence: u64,
    last_error: Option<String>,
}

/// Native standalone WAL.
///
/// The payload is not Cypher text. It is FalkorDB's own sealed EffectsBuffer,
/// the same deterministic mutation format the Redis host sends as GRAPH.EFFECT.
pub struct Wal {
    path: PathBuf,
    state: Mutex<WalState>,
}

impl Wal {
    /// Open/validate the WAL. A torn final frame is truncated to the last fully
    /// fsynced frame; corruption in a complete frame is an error.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<WalRecord>), String> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("create WAL directory {}: {e}", parent.display()))?;
        }

        if !path.exists() {
            File::create(&path)
                .map_err(|e| format!("create WAL {}: {e}", path.display()))?;
        }

        let records = read_records_and_repair_tail(&path)?;
        let next_sequence = records
            .last()
            .map_or(1, |record| record.sequence.saturating_add(1));

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .append(true)
            .open(&path)
            .map_err(|e| format!("open WAL {}: {e}", path.display()))?;

        Ok((
            Self {
                path,
                state: Mutex::new(WalState {
                    file,
                    next_sequence,
                    last_error: None,
                }),
            },
            records,
        ))
    }

    pub fn append_effects(
        &self,
        effects: EffectsBuffer,
        key: &[u8],
    ) -> Result<(), String> {
        if effects.is_empty() {
            return Ok(());
        }

        self.state.lock().last_error = None;
        effects.replicate(self, key);

        self.state.lock().last_error.take().map_or(Ok(()), Err)
    }


    pub fn records(&self) -> Result<Vec<WalRecord>, String> {
        let _guard = self.state.lock();
        read_records_and_repair_tail(&self.path)
    }

    pub fn append_payload(
        &self,
        key: &[u8],
        payload: &[u8],
    ) -> Result<(), String> {
        if payload.is_empty() {
            return Ok(());
        }

        let mut state = self.state.lock();
        let sequence = state.next_sequence;
        let start = state
            .file
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("WAL seek failed: {e}"))?;

        let crc = frame_crc(sequence, key, payload);
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&WAL_VERSION.to_le_bytes());
        header.extend_from_slice(&sequence.to_le_bytes());
        header.extend_from_slice(&(key.len() as u32).to_le_bytes());
        header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());

        let result = (|| -> std::io::Result<()> {
            state.file.write_all(&header)?;
            state.file.write_all(key)?;
            state.file.write_all(payload)?;
            state.file.flush()?;
            state.file.sync_data()?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = state.file.set_len(start);
            let _ = state.file.seek(SeekFrom::End(0));
            return Err(format!("WAL append failed at sequence {sequence}: {e}"));
        }

        state.next_sequence = sequence.saturating_add(1);
        Ok(())
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl ReplicationSink for Wal {
    fn replicate(
        &self,
        key: &[u8],
        payload: &[u8],
    ) {
        let mut state = self.state.lock();
        if state.last_error.is_some() {
            return;
        }

        let sequence = state.next_sequence;
        let start = match state.file.seek(SeekFrom::End(0)) {
            Ok(v) => v,
            Err(e) => {
                state.last_error = Some(format!("WAL seek failed: {e}"));
                return;
            }
        };

        let crc = frame_crc(sequence, key, payload);
        let mut header = Vec::with_capacity(HEADER_LEN);
        header.extend_from_slice(MAGIC);
        header.extend_from_slice(&WAL_VERSION.to_le_bytes());
        header.extend_from_slice(&sequence.to_le_bytes());
        header.extend_from_slice(&(key.len() as u32).to_le_bytes());
        header.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        header.extend_from_slice(&crc.to_le_bytes());

        let result = (|| -> std::io::Result<()> {
            state.file.write_all(&header)?;
            state.file.write_all(key)?;
            state.file.write_all(payload)?;
            state.file.flush()?;
            // This is the durability boundary. NativeGraph does not publish the
            // new MVCC graph Arc until this succeeds.
            state.file.sync_data()?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = state.file.set_len(start);
            let _ = state.file.seek(SeekFrom::End(0));
            state.last_error =
                Some(format!("WAL append failed at sequence {sequence}: {e}"));
            return;
        }

        state.next_sequence = sequence.saturating_add(1);
    }
}

fn frame_crc(
    sequence: u64,
    key: &[u8],
    payload: &[u8],
) -> u32 {
    let mut h = Hasher::new();
    h.update(&sequence.to_le_bytes());
    h.update(key);
    h.update(payload);
    h.finalize()
}

fn read_records_and_repair_tail(path: &Path) -> Result<Vec<WalRecord>, String> {
    let bytes = fs::read(path).map_err(|e| format!("read WAL {}: {e}", path.display()))?;
    let mut records = Vec::new();
    let mut pos = 0usize;
    let mut last_good = 0usize;
    let mut expected_sequence = 1u64;

    while pos < bytes.len() {
        if bytes.len() - pos < HEADER_LEN {
            truncate_tail(path, last_good)?;
            break;
        }

        let start = pos;
        if &bytes[pos..pos + 4] != MAGIC {
            return Err(format!("WAL corruption at byte {pos}: bad magic"));
        }
        pos += 4;

        let version = u16::from_le_bytes(bytes[pos..pos + 2].try_into().unwrap());
        pos += 2;
        if version != WAL_VERSION {
            return Err(format!(
                "WAL version {version} at byte {start} is unsupported (expected {WAL_VERSION})"
            ));
        }

        let sequence = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        if sequence != expected_sequence {
            return Err(format!(
                "WAL sequence discontinuity at byte {start}: got {sequence}, expected {expected_sequence}"
            ));
        }

        let key_len = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap()) as usize;
        pos += 4;
        let payload_len_u64 = u64::from_le_bytes(bytes[pos..pos + 8].try_into().unwrap());
        pos += 8;
        let payload_len = usize::try_from(payload_len_u64)
            .map_err(|_| format!("WAL payload too large at byte {start}: {payload_len_u64}"))?;
        let expected_crc = u32::from_le_bytes(bytes[pos..pos + 4].try_into().unwrap());
        pos += 4;

        let Some(frame_end) = pos
            .checked_add(key_len)
            .and_then(|v| v.checked_add(payload_len))
        else {
            return Err(format!("WAL length overflow at byte {start}"));
        };

        if frame_end > bytes.len() {
            truncate_tail(path, last_good)?;
            break;
        }

        let key = bytes[pos..pos + key_len].to_vec();
        pos += key_len;
        let payload = bytes[pos..pos + payload_len].to_vec();
        pos += payload_len;

        let actual_crc = frame_crc(sequence, &key, &payload);
        if actual_crc != expected_crc {
            return Err(format!(
                "WAL CRC mismatch at sequence {sequence}: got {actual_crc:#010x}, expected {expected_crc:#010x}"
            ));
        }

        records.push(WalRecord {
            sequence,
            key,
            payload,
        });
        expected_sequence = expected_sequence.saturating_add(1);
        last_good = pos;
    }

    Ok(records)
}

fn truncate_tail(
    path: &Path,
    len: usize,
) -> Result<(), String> {
    let f = OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|e| format!("open WAL for repair {}: {e}", path.display()))?;
    f.set_len(len as u64)
        .map_err(|e| format!("truncate torn WAL tail {}: {e}", path.display()))?;
    f.sync_data()
        .map_err(|e| format!("sync repaired WAL {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_path(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "falkordb-native-{label}-{}-{nonce}.wal",
            std::process::id()
        ))
    }

    fn encoded_frame(
        sequence: u64,
        key: &[u8],
        payload: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&WAL_VERSION.to_le_bytes());
        out.extend_from_slice(&sequence.to_le_bytes());
        out.extend_from_slice(&(key.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(&frame_crc(sequence, key, payload).to_le_bytes());
        out.extend_from_slice(key);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn torn_final_frame_is_removed_but_crc_corruption_is_not_hidden() {
        let path = test_path("wal");
        let first = encoded_frame(1, b"g", b"payload-one");
        let second = encoded_frame(2, b"g", b"payload-two");
        let mut bytes = first.clone();
        bytes.extend_from_slice(&second[..second.len() - 3]);
        fs::write(&path, bytes).unwrap();

        let records = read_records_and_repair_tail(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(fs::metadata(&path).unwrap().len(), first.len() as u64);

        let mut corrupt = first;
        let last = corrupt.len() - 1;
        corrupt[last] ^= 0x80;
        fs::write(&path, corrupt).unwrap();
        assert!(read_records_and_repair_tail(&path)
            .unwrap_err()
            .contains("CRC mismatch"));

        let _ = fs::remove_file(path);
    }
}
