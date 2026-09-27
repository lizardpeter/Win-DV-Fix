use std::{
    env,
    fs,
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    sync::Arc,
    thread,
};

use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};

use falkordb_native_host::{
    Engine,
    api::{ApiConfig, serve_api},
    mcp::OAUTH_BUILD_ID,
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

fn generate_oauth_pairing_code() -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 24];
    rng.fill(&mut bytes)
        .map_err(|_| "generate OAuth pairing code: secure random generator failed".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
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

    let candidate = root.join(raw);

    // Reject pre-existing symlink/junction/reparse-point chains that redirect
    // an apparently local path outside the package. Check every existing
    // prefix so this also protects a not-yet-created final data directory.
    let relative = candidate
        .strip_prefix(root)
        .map_err(|_| format!("{label} escaped the executable folder"))?;
    let mut prefix = root.to_path_buf();
    for component in relative.components() {
        prefix.push(component.as_os_str());
        if prefix.exists() {
            let resolved = prefix.canonicalize().map_err(|e| {
                format!("canonicalize portable path {}: {e}", prefix.display())
            })?;
            if !resolved.starts_with(root) {
                return Err(format!(
                    "{label} resolves outside the executable folder in --portable mode: {} -> {}",
                    prefix.display(),
                    resolved.display()
                ));
            }
        }
    }

    Ok(candidate)
}


#[derive(Default)]
struct LocalSecrets {
    password: Option<String>,
    api_token: Option<String>,
    api_read_token: Option<String>,
}

fn unquote_secret_value(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2 {
        let bytes = value.as_bytes();
        if (bytes[0] == b'"' && bytes[value.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[value.len() - 1] == b'\'')
        {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

fn parse_secret_assignment(line: &str) -> Option<(String, String)> {
    let line = line.trim_start_matches('\u{feff}').trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
        return None;
    }

    let (raw_name, raw_value) = line.split_once('=')?;
    let mut name = raw_name.trim();
    if let Some(rest) = name.strip_prefix("set ") {
        name = rest.trim();
    }
    if let Some(rest) = name.strip_prefix("$env:") {
        name = rest.trim();
    }

    match name {
        "FALKORDB_PASSWORD" | "FALKORDB_API_TOKEN" | "FALKORDB_API_READ_TOKEN" => {
            Some((name.to_string(), unquote_secret_value(raw_value)))
        }
        _ => None,
    }
}

fn load_local_secrets(root: &Path) -> Result<LocalSecrets, String> {
    let path = root.join("falkordb-secrets.txt");
    if !path.exists() {
        return Ok(LocalSecrets::default());
    }

    let text = fs::read_to_string(&path)
        .map_err(|e| format!("read local secrets file {}: {e}", path.display()))?;
    let mut secrets = LocalSecrets::default();
    for line in text.lines() {
        let Some((name, value)) = parse_secret_assignment(line) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        match name.as_str() {
            "FALKORDB_PASSWORD" => secrets.password = Some(value),
            "FALKORDB_API_TOKEN" => secrets.api_token = Some(value),
            "FALKORDB_API_READ_TOKEN" => secrets.api_read_token = Some(value),
            _ => {}
        }
    }
    Ok(secrets)
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

fn configure_portable_process(root: &Path) -> Result<(), String> {
    let local_dirs = [
        "data",
        "logs",
        "tls",
        "tmp",
        "pycache",
        "imports",
        "exports",
        "migration",
        "cache",
        "config",
        "profile",
        "profile/AppData/Roaming",
        "profile/AppData/Local",
    ];
    for relative in local_dirs {
        let path = root.join(relative);
        fs::create_dir_all(&path)
            .map_err(|e| format!("create portable directory {}: {e}", path.display()))?;
    }

    env::set_current_dir(root).map_err(|e| {
        format!(
            "set portable working directory to {}: {e}",
            root.display()
        )
    })?;

    let tmp = root.join("tmp");
    let profile = root.join("profile");
    let roaming = profile.join("AppData/Roaming");
    let local = profile.join("AppData/Local");
    let cache = root.join("cache");
    let config = root.join("config");
    let pycache = root.join("pycache");

    // SAFETY: portable-process environment is configured before Engine::init
    // starts the FalkorDB threadpool or any other worker threads.
    unsafe {
        env::set_var("FALKORDB_PORTABLE_ROOT", root);
        if env::var_os("FALKORDB_DATA_DIR").is_none() {
            env::set_var("FALKORDB_DATA_DIR", "data");
        }
        env::set_var("TEMP", &tmp);
        env::set_var("TMP", &tmp);
        env::set_var("HOME", &profile);
        env::set_var("USERPROFILE", &profile);
        env::set_var("APPDATA", &roaming);
        env::set_var("LOCALAPPDATA", &local);
        env::set_var("XDG_CACHE_HOME", &cache);
        env::set_var("XDG_CONFIG_HOME", &config);
        env::set_var("PIP_CACHE_DIR", cache.join("pip"));
        env::set_var("PYTHONPYCACHEPREFIX", &pycache);
        env::set_var("PYTHONDONTWRITEBYTECODE", "1");
        env::set_var("PYTHONNOUSERSITE", "1");
        env::set_var("PYTHONPATH", "");
    }

    Ok(())
}

fn main() -> Result<(), String> {
    let cli_args: Vec<String> = env::args().skip(1).collect();
    let portable = portable_enabled_from_env()
        || cli_args.iter().any(|arg| arg == "--portable");
    let portable_root = portable.then(executable_root).transpose()?;
    if let Some(root) = &portable_root {
        configure_portable_process(root)?;
    }

    let executable_dir = executable_root()?;
    let local_secrets = load_local_secrets(&executable_dir)?;

    let _engine = Engine::init()?;

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
    if config.password.is_none() {
        config.password = local_secrets.password.clone();
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
    let mut tunnel_mcp_bind: Option<SocketAddr> = env::var("FALKORDB_TUNNEL_MCP_BIND")
        .ok()
        .map(|v| {
            v.parse::<SocketAddr>()
                .map_err(|e| format!("invalid FALKORDB_TUNNEL_MCP_BIND {v:?}: {e}"))
        })
        .transpose()?;
    let mut api_token = env::var("FALKORDB_API_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| local_secrets.api_token.clone());
    let mut api_read_token = env::var("FALKORDB_API_READ_TOKEN")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| local_secrets.api_read_token.clone());
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

    // A configured API credential implies local management surfaces even when
    // no bind was specified. The Secure MCP Tunnel backend is loopback-only and
    // remains protected by the read/write API token, so enabling it by default
    // does not create a new network ingress path.
    if api_bind.is_none() && (api_token.is_some() || api_read_token.is_some()) {
        api_bind = Some("127.0.0.1:8443".parse().expect("valid default API bind"));
    }
    if tunnel_mcp_bind.is_none() && api_token.is_some() {
        tunnel_mcp_bind = Some(
            "127.0.0.1:18444"
                .parse()
                .expect("valid default Secure MCP Tunnel bind"),
        );
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
            "--tunnel-mcp-bind" => {
                let value = args.next().ok_or_else(|| "--tunnel-mcp-bind requires HOST:PORT".to_string())?;
                let bind = value
                    .parse::<SocketAddr>()
                    .map_err(|e| format!("invalid --tunnel-mcp-bind {value:?}: {e}"))?;
                if !bind.ip().is_loopback() {
                    return Err("--tunnel-mcp-bind must use a loopback address".to_string());
                }
                tunnel_mcp_bind = Some(bind);
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
                                   Falls back to falkordb-secrets.txt beside server.exe
  --tls-cert PATH                  PEM server certificate/chain (or FALKORDB_TLS_CERT)
  --tls-key PATH                   PEM private key (or FALKORDB_TLS_KEY)
  --tls-client-ca PATH             Require RESP mTLS clients signed by this PEM CA
  --allow-plaintext-remote         Permit non-loopback RESP without TLS
  --allow-unauthenticated-remote   Permit non-loopback RESP without AUTH

CHATGPT HTTPS API OPTIONS:
  --api-bind HOST:PORT             Enable API listener, e.g. 0.0.0.0:8443
  --api-token TOKEN                Read/write Bearer token (or FALKORDB_API_TOKEN)
                                   Falls back to falkordb-secrets.txt beside server.exe
  --api-read-token TOKEN           Optional read-only Bearer token
                                   OAuth owner approval accepts the RESP password
                                   and, for compatibility, the read/write API token.
  --tunnel-mcp-bind HOST:PORT      Override the loopback-only, MCP-only backend
                                   for OpenAI Secure MCP Tunnel.
                                   Default with FALKORDB_API_TOKEN: 127.0.0.1:18444
                                   Env: FALKORDB_TUNNEL_MCP_BIND
                                   Requires FALKORDB_API_TOKEN; tunnel-client
                                   injects it only on the local backend hop.
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

    if let Some(bind) = tunnel_mcp_bind {
        if !bind.ip().is_loopback() {
            return Err("Secure MCP Tunnel backend must bind to loopback only".to_string());
        }
        let Some(local_token) = api_token.clone() else {
            return Err(
                "Secure MCP Tunnel backend requires FALKORDB_API_TOKEN/--api-token for the local tunnel-client hop"
                    .to_string(),
            );
        };

        let tunnel_config = ApiConfig {
            bind,
            read_write_token: Some(local_token),
            read_only_token: None,
            oauth_owner_secret: None,
            oauth_owner_secret_fallbacks: Vec::new(),
            oauth_pairing_code: None,
            allow_unauthenticated_remote: false,
            allow_plaintext_remote: false,
            tunnel_mode: true,
            tls: None,
        };
        tunnel_config.validate()?;

        eprintln!(
            "FalkorDB Secure MCP Tunnel backend listening on http://{bind}/mcp (loopback-only, local Bearer protected)"
        );
        let tunnel_catalog = Arc::clone(&catalog);
        thread::spawn(move || {
            if let Err(err) = serve_api(tunnel_config, tunnel_catalog) {
                eprintln!("Secure MCP Tunnel backend stopped: {err}");
            }
        });
    }

    if let Some(bind) = api_bind {
        let mut oauth_owner_secret_fallbacks = Vec::new();
        for candidate in [
            local_secrets.password.as_ref(),
            local_secrets.api_token.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            if !candidate.is_empty()
                && config.password.as_deref() != Some(candidate.as_str())
                && api_token.as_deref() != Some(candidate.as_str())
                && !oauth_owner_secret_fallbacks.iter().any(|existing| existing == candidate)
            {
                oauth_owner_secret_fallbacks.push(candidate.clone());
            }
        }

        let oauth_pairing_code = generate_oauth_pairing_code()?;
        eprintln!("FalkorDB OAuth build: {OAUTH_BUILD_ID}");
        eprintln!("OAuth pairing code (valid until this server restarts): {oauth_pairing_code}");
        eprintln!(
            "OAuth credential diagnostics: secrets_file={} found={}, file_password={}, file_api_token={}, runtime_password={}, runtime_api_token={}",
            executable_dir.join("falkordb-secrets.txt").display(),
            executable_dir.join("falkordb-secrets.txt").exists(),
            local_secrets.password.is_some(),
            local_secrets.api_token.is_some(),
            config.password.is_some(),
            api_token.is_some(),
        );

        let api_config = ApiConfig {
            bind,
            read_write_token: api_token.clone(),
            read_only_token: api_read_token.clone(),
            oauth_owner_secret: config.password.clone(),
            oauth_owner_secret_fallbacks,
            oauth_pairing_code: Some(oauth_pairing_code),
            allow_unauthenticated_remote: api_allow_unauthenticated_remote,
            allow_plaintext_remote: api_allow_plaintext_remote,
            tunnel_mode: false,
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
