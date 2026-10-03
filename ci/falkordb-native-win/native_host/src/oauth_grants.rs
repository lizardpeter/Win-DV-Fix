use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest,
    rand::{SecureRandom, SystemRandom},
};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

const STORE_VERSION: u32 = 1;
const STORE_FILE: &str = "oauth-grants.json";
const MAX_GRANTS: usize = 1024;

static STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn store_lock() -> &'static Mutex<()> {
    STORE_LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PersistentGrant {
    pub client_id: String,
    pub issuer: String,
    pub resource: String,
    pub scope: String,
    pub created_at: u64,
    pub last_used_at: u64,
    token_hash: String,
    key_fingerprint: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct GrantStore {
    version: u32,
    grants: Vec<PersistentGrant>,
}

pub(crate) fn issue(
    data_dir: &Path,
    signing_secret: &str,
    client_id: &str,
    issuer: &str,
    resource: &str,
    scope: &str,
) -> Result<String, String> {
    let _guard = store_lock()
        .lock()
        .map_err(|_| "OAuth grant store lock is poisoned".to_string())?;

    let token = random_refresh_token()?;
    let now = now_secs();
    let mut store = load_store(data_dir)?;
    store.version = STORE_VERSION;

    // This server is deliberately single-owner. Re-authorizing the same MCP
    // client replaces its previous grant instead of leaking an unbounded list
    // of evergreen refresh grants.
    store
        .grants
        .retain(|grant| !(grant.client_id == client_id && grant.resource == resource));

    store.grants.push(PersistentGrant {
        client_id: client_id.to_string(),
        issuer: issuer.to_string(),
        resource: resource.to_string(),
        scope: scope.to_string(),
        created_at: now,
        last_used_at: now,
        token_hash: token_hash(&token),
        key_fingerprint: key_fingerprint(signing_secret),
    });

    if store.grants.len() > MAX_GRANTS {
        store.grants.sort_by_key(|grant| grant.last_used_at);
        let drop_count = store.grants.len() - MAX_GRANTS;
        store.grants.drain(0..drop_count);
    }

    save_store(data_dir, &store)?;
    Ok(token)
}

pub(crate) fn validate_and_touch(
    data_dir: &Path,
    signing_secret: &str,
    refresh_token: &str,
    client_id: Option<&str>,
    issuer: &str,
    resource: &str,
) -> Result<PersistentGrant, String> {
    let _guard = store_lock()
        .lock()
        .map_err(|_| "OAuth grant store lock is poisoned".to_string())?;

    let mut store = load_store(data_dir)?;
    let wanted_hash = token_hash(refresh_token);
    let wanted_key = key_fingerprint(signing_secret);
    let now = now_secs();

    let Some(index) = store.grants.iter().position(|grant| {
        constant_time_eq(&grant.token_hash, &wanted_hash)
            && constant_time_eq(&grant.key_fingerprint, &wanted_key)
    }) else {
        return Err("refresh token is invalid or has been revoked".to_string());
    };

    let grant = &mut store.grants[index];
    if grant.issuer != issuer || grant.resource != resource {
        return Err("refresh token target mismatch".to_string());
    }
    if let Some(client_id) = client_id {
        if grant.client_id != client_id {
            return Err("refresh token client mismatch".to_string());
        }
    }

    grant.last_used_at = now;
    let result = grant.clone();
    save_store(data_dir, &store)?;
    Ok(result)
}

pub(crate) fn revoke(data_dir: &Path, refresh_token: &str) -> Result<(), String> {
    let _guard = store_lock()
        .lock()
        .map_err(|_| "OAuth grant store lock is poisoned".to_string())?;

    let mut store = load_store(data_dir)?;
    let wanted_hash = token_hash(refresh_token);
    store
        .grants
        .retain(|grant| !constant_time_eq(&grant.token_hash, &wanted_hash));
    save_store(data_dir, &store)
}

#[cfg(test)]
pub(crate) fn grant_count(data_dir: &Path) -> Result<usize, String> {
    let _guard = store_lock()
        .lock()
        .map_err(|_| "OAuth grant store lock is poisoned".to_string())?;
    Ok(load_store(data_dir)?.grants.len())
}

fn store_path(data_dir: &Path) -> PathBuf {
    data_dir.join(STORE_FILE)
}

fn load_store(data_dir: &Path) -> Result<GrantStore, String> {
    let path = store_path(data_dir);
    if !path.exists() {
        return Ok(GrantStore {
            version: STORE_VERSION,
            grants: Vec::new(),
        });
    }

    let bytes = fs::read(&path)
        .map_err(|e| format!("read OAuth grant store {}: {e}", path.display()))?;
    if bytes.is_empty() {
        return Ok(GrantStore {
            version: STORE_VERSION,
            grants: Vec::new(),
        });
    }

    let store: GrantStore = serde_json::from_slice(&bytes)
        .map_err(|e| format!("parse OAuth grant store {}: {e}", path.display()))?;
    if store.version != STORE_VERSION {
        return Err(format!(
            "unsupported OAuth grant store version {} in {}",
            store.version,
            path.display()
        ));
    }
    Ok(store)
}

fn save_store(data_dir: &Path, store: &GrantStore) -> Result<(), String> {
    fs::create_dir_all(data_dir)
        .map_err(|e| format!("create OAuth data directory {}: {e}", data_dir.display()))?;
    let path = store_path(data_dir);
    let temp = data_dir.join(format!("{STORE_FILE}.tmp"));

    let bytes = serde_json::to_vec_pretty(store)
        .map_err(|e| format!("serialize OAuth grant store: {e}"))?;

    {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&temp)
            .map_err(|e| format!("open OAuth grant temp file {}: {e}", temp.display()))?;
        file.write_all(&bytes)
            .map_err(|e| format!("write OAuth grant temp file {}: {e}", temp.display()))?;
        file.sync_all()
            .map_err(|e| format!("sync OAuth grant temp file {}: {e}", temp.display()))?;
    }

    // Windows does not allow rename-over-existing in the same way Unix does.
    // Keep the durable temp file until the new contents are fully flushed, then
    // replace the small metadata file.
    if path.exists() {
        fs::remove_file(&path)
            .map_err(|e| format!("replace OAuth grant store {}: {e}", path.display()))?;
    }
    fs::rename(&temp, &path)
        .map_err(|e| format!("install OAuth grant store {}: {e}", path.display()))?;
    Ok(())
}

fn random_refresh_token() -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 48];
    rng.fill(&mut bytes)
        .map_err(|_| "secure random number generation failed".to_string())?;
    Ok(format!("rt_{}", URL_SAFE_NO_PAD.encode(bytes)))
}

fn token_hash(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, token.as_bytes()).as_ref())
}

fn key_fingerprint(secret: &str) -> String {
    token_hash(secret)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "falkordb-oauth-grants-{name}-{}-{}",
            std::process::id(),
            now_secs()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn persistent_grant_survives_reload_and_has_no_wall_clock_expiry() {
        let dir = temp_dir("reload");
        let token = issue(
            &dir,
            "signing-key",
            "https://claude.ai/oauth/mcp-oauth-client-metadata",
            "https://db.example.test",
            "https://db.example.test/mcp",
            "graph:read graph:write",
        )
        .unwrap();

        // Validation reloads the JSON file rather than relying on process-local
        // state, which is the restart-persistence property we need.
        let grant = validate_and_touch(
            &dir,
            "signing-key",
            &token,
            Some("https://claude.ai/oauth/mcp-oauth-client-metadata"),
            "https://db.example.test",
            "https://db.example.test/mcp",
        )
        .unwrap();
        assert_eq!(grant.scope, "graph:read graph:write");
        assert_eq!(grant_count(&dir).unwrap(), 1);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn key_rotation_and_revocation_invalidate_grants() {
        let dir = temp_dir("revoke");
        let token = issue(
            &dir,
            "old-key",
            "client",
            "https://db.example.test",
            "https://db.example.test/mcp",
            "graph:read",
        )
        .unwrap();

        assert!(validate_and_touch(
            &dir,
            "new-key",
            &token,
            Some("client"),
            "https://db.example.test",
            "https://db.example.test/mcp",
        )
        .is_err());

        revoke(&dir, &token).unwrap();
        assert!(validate_and_touch(
            &dir,
            "old-key",
            &token,
            Some("client"),
            "https://db.example.test",
            "https://db.example.test/mcp",
        )
        .is_err());
        let _ = fs::remove_dir_all(dir);
    }
}
