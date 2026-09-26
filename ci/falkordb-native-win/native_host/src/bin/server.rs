use std::{
    env,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    thread,
};

use std::time::Duration;

use falkordb_native_host::{
    Engine,
    api::{ApiConfig, serve_api},
    server::{GraphCatalog, ServerConfig, TlsConfig, serve_with_catalog},
};

fn executable_root() -> Result<PathBuf, String> {
    let exe = env::current_exe()
        .map_err(|e| format!("resolve executable path: {e}"))?;
    let parent = exe
        .parent()
        .ok_or_else(|| format!("executable has no parent directory: {}", exe.display()))?;
    parent
        .canonicalize()
        .map_err(|e| format!("canonicalize executable directory {}: {e}", parent.display()))
}

fn portable_path(root: &Path, raw: impl AsRef<Path>, label: &str) -> Result<PathBuf, String> {
    let raw = raw.as_ref();
    if raw.is_absolute() {
        return Err(format!(
            "{label} must be relative in --portable mode; all runtime files must remain below {}",
            root.display()
        ));
    }
    if raw.components().any(|component| matches!(
        component,
        Component::ParentDir | Component::RootDir | Component::Prefix(_)
    )) {
        return Err(format!(
            "{label} may not escape the executable folder in --portable mode: {}",
            raw.display()
        ));
    }
    Ok(root.join(raw))
}

fn portable_enabled_from_env() -> bool {
    env::var("FALKORDB_PORTABLE")
        .ok()
        .is_some_and(|value| {
            let value = value.trim();
            !value.is_empty()
                && value != "0"
                && !value.eq_ignore_ascii_case("false")
                && !value.eq_ignore_ascii_case("no")
        })
}

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;

    let cli_args: Vec<String> = env::args().skip(1).collect();
    let portable = portable_enabled_from_env()
        || cli_args.iter().any(|arg| arg == "--portable");
    let portable_root = portable.then(executable_root).transpose()?;

    let mut config = ServerConfig::default();
    if let Some(root) = &portable_root {
        config.data_dir = root.join("data");
    }
    if let Ok(data_dir) = env::var("FALKORDB_DATA_DIR") {
        if !data_dir.is_empty() {
            config.data_dir = if let Some(root) = &portable_root {
                portable_path(root, data_dir, "FALKORDB_DATA_DIR")?
            } else {
                PathBuf::from(data_dir)
            };
        }
    }
    if let Ok(password) = env::var("FALKORDB_PASSWORD") {
        if !password.is_empty() {
            config.password = Some(password);
        }
    }
    if let Ok(username) = env::var("FALKORDB_USERNAME") {
        if !username.is_empty() {
            config.username = username;
        }
    }

    let resolve_env_path = |name: &str| -> Result<Option<PathBuf>, String> {
        let Some(value) = env::var_os(name) else {
            return Ok(None);
        };
        if value.is_empty() {
            return Ok(None);
        }
        Ok(Some(if let Some(root) = &portable_root {
            portable_path(root, PathBuf::from(value), name)?
        } else {
            PathBuf::from(value)
        }))
    };

    let mut tls_cert = resolve_env_path("FALKORDB_TLS_CERT")?;
    let mut tls_key = resolve_env_path("FALKORDB_TLS_KEY")?;
    let mut tls_client_ca = resolve_env_path("FALKORDB_TLS_CLIENT_CA")?;

    let mut api_bind: Option<SocketAddr> = env::var("FALKORDB_API_BIND")
        .ok()
        .map(|v| {
            v.parse::<SocketAddr>()
                .map_err(|e| format!("invalid FALKORDB_API_BIND {v:?}: {e}"))
        })
        .transpose()?;
    let mut api_token = env::var("FALKORDB_API_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());
    let mut api_read_token = env::var("FALKORDB_API_READ_TOKEN")
        .ok()
        .filter(|v| !v.is_empty());
    let mut api_allow_plaintext_remote = false;
    let mut api_allow_unauthenticated_remote = false;

    let mut checkpoint_wal_mb: u64 = env::var("FALKORDB_CHECKPOINT_WAL_MB")
        .ok()
        .map(|v| {
            v.parse::<u64>()
                .map_err(|e| format!("invalid FALKORDB_CHECKPOINT_WAL_MB {v:?}: {e}"))
        })
        .transpose()?
        .unwrap_or(256);

    // A configured API credential implies a localhost API even when no bind
    // was specified, making the ChatGPT surface easy to enable safely.
    if api_bind.is_none() && (api_token.is_some() || api_read_token.is_some()) {
        api_bind = Some("127.0.0.1:8443".parse().expect("valid default API bind"));
    }

    let mut args = cli_args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bind" => {
                let value = args.next().ok_or_else(|| "--bind requires HOST:PORT".to_string())?;
                config.bind = value
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("invalid --bind {value:?}: {e}"))?;
            }
            "--port" => {
                let value = args.next().ok_or_else(|| "--port requires a number".to_string())?;
                let port = value.parse::<u16>().map_err(|e| format!("invalid --port: {e}"))?;
                config.bind.set_port(port);
            }
            "--portable" => {}
            "--data-dir" => {
                let value = args.next().ok_or_else(|| "--data-dir requires a path".to_string())?;
                config.data_dir = if let Some(root) = &portable_root {
                    portable_path(root, value, "--data-dir")?
                } else {
                    PathBuf::from(value)
                };
            }
            "--username" => {
                config.username = args.next().ok_or_else(|| "--username requires a value".to_string())?;
            }
            "--password" => {
                config.password = Some(args.next().ok_or_else(|| "--password requires a value".to_string())?);
            }
            "--tls-cert" => {
                let value = args.next().ok_or_else(|| "--tls-cert requires a PEM path".to_string())?;
                tls_cert = Some(if let Some(root) = &portable_root {
                    portable_path(root, value, "--tls-cert")?
                } else {
                    PathBuf::from(value)
                });
            }
            "--tls-key" => {
                let value = args.next().ok_or_else(|| "--tls-key requires a PEM path".to_string())?;
                tls_key = Some(if let Some(root) = &portable_root {
                    portable_path(root, value, "--tls-key")?
                } else {
                    PathBuf::from(value)
                });
            }
            "--tls-client-ca" => {
                let value = args.next().ok_or_else(|| "--tls-client-ca requires a PEM path".to_string())?;
                tls_client_ca = Some(if let Some(root) = &portable_root {
                    portable_path(root, value, "--tls-client-ca")?
                } else {
                    PathBuf::from(value)
                });
            }
            "--allow-plaintext-remote" => {
                config.allow_plaintext_remote = true;
            }
            "--allow-unauthenticated-remote" => {
                config.allow_unauthenticated_remote = true;
            }
            "--api-bind" => {
                let value = args.next().ok_or_else(|| "--api-bind requires HOST:PORT".to_string())?;
                api_bind = Some(
                    value
                        .parse::<SocketAddr>()
                        .map_err(|e| format!("invalid --api-bind {value:?}: {e}"))?,
                );
            }
            "--api-token" => {
                api_token = Some(
                    args.next()
                        .ok_or_else(|| "--api-token requires a value".to_string())?,
                );
            }
            "--api-read-token" => {
                api_read_token = Some(
                    args.next()
                        .ok_or_else(|| "--api-read-token requires a value".to_string())?,
                );
            }
            "--api-allow-plaintext-remote" => {
                api_allow_plaintext_remote = true;
            }
            "--api-allow-unauthenticated-remote" => {
                api_allow_unauthenticated_remote = true;
            }
            "--checkpoint-wal-mb" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--checkpoint-wal-mb requires a number".to_string())?;
                checkpoint_wal_mb = value
                    .parse::<u64>()
                    .map_err(|e| format!("invalid --checkpoint-wal-mb: {e}"))?;
            }
            "--help" | "-h" => {
                println!(r#"falkordb-native-server

USAGE:
  falkordb-native-server [OPTIONS]

PORTABLE OPTIONS:
  --portable                       Keep runtime state under the server.exe folder.
                                   Relative data/TLS paths are rooted there and
                                   external/parent paths are rejected.
                                   Env: FALKORDB_PORTABLE=1

RESP/FalkorDB OPTIONS:
  --bind HOST:PORT                 RESP bind address (default 127.0.0.1:6379)
  --port PORT                      Override RESP port
  --data-dir PATH                  Persistent graph data directory
                                   (portable default: .\data)
                                   Env: FALKORDB_DATA_DIR
  --username USER                  RESP AUTH username (default: default)
  --password PASSWORD              RESP AUTH password (or FALKORDB_PASSWORD)
  --tls-cert PATH                  PEM server certificate/chain (or FALKORDB_TLS_CERT)
  --tls-key PATH                   PEM private key (or FALKORDB_TLS_KEY)
  --tls-client-ca PATH             Require RESP mTLS clients signed by this PEM CA
  --allow-plaintext-remote         Permit non-loopback RESP without TLS
  --allow-unauthenticated-remote   Permit non-loopback RESP without AUTH

CHATGPT HTTPS API OPTIONS:
  --api-bind HOST:PORT             Enable API listener, e.g. 0.0.0.0:8443
  --api-token TOKEN                Read/write Bearer token (or FALKORDB_API_TOKEN)
  --api-read-token TOKEN           Optional read-only Bearer token
  --api-allow-plaintext-remote     Permit non-loopback API without TLS
  --api-allow-unauthenticated-remote
                                   Permit non-loopback API without Bearer auth

DURABILITY OPTIONS:
  --checkpoint-wal-mb MB           Auto-checkpoint each graph when its WAL reaches
                                   this size (default 256; env FALKORDB_CHECKPOINT_WAL_MB)

The HTTPS API reuses --tls-cert/--tls-key for server TLS but does not require
the RESP client certificate. This permits standard HTTPS Bearer-token clients,
including a ChatGPT custom integration.
"#);
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    let shared_tls = match (tls_cert, tls_key) {
        (Some(cert_path), Some(key_path)) => Some((cert_path, key_path)),
        (None, None) if tls_client_ca.is_none() => None,
        (None, None) => {
            return Err("--tls-client-ca requires --tls-cert and --tls-key".to_string());
        }
        _ => {
            return Err("--tls-cert and --tls-key must be provided together".to_string());
        }
    };

    if let Some((cert_path, key_path)) = &shared_tls {
        config.tls = Some(TlsConfig {
            cert_path: cert_path.clone(),
            key_path: key_path.clone(),
            client_ca_path: tls_client_ca,
        });
    }

    let catalog = Arc::new(GraphCatalog::open(&config.data_dir)?);

    if checkpoint_wal_mb > 0 {
        let checkpoint_catalog = Arc::clone(&catalog);
        let threshold_bytes = checkpoint_wal_mb.saturating_mul(1024 * 1024);
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(60));
            for (name, result) in checkpoint_catalog.checkpoint_large_wals(threshold_bytes) {
                match result {
                    Ok(path) => eprintln!(
                        "checkpointed graph {name:?} at {}",
                        path.display()
                    ),
                    Err(err) => eprintln!(
                        "automatic checkpoint failed for graph {name:?}: {err}"
                    ),
                }
            }
        });
    }

    if let Some(bind) = api_bind {
        let api_config = ApiConfig {
            bind,
            read_write_token: api_token,
            read_only_token: api_read_token,
            allow_unauthenticated_remote: api_allow_unauthenticated_remote,
            allow_plaintext_remote: api_allow_plaintext_remote,
            tls: shared_tls.as_ref().map(|(cert_path, key_path)| TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                // ChatGPT/custom HTTPS integrations use ordinary server TLS +
                // Bearer auth, not the RESP listener's optional client cert.
                client_ca_path: None,
            }),
        };
        api_config.validate()?;

        let api_catalog = Arc::clone(&catalog);
        thread::spawn(move || {
            if let Err(err) = serve_api(api_config, api_catalog) {
                eprintln!("ChatGPT HTTPS API stopped: {err}");
            }
        });
    }

    serve_with_catalog(config, catalog)
}
