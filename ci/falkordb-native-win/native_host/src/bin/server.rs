use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
};

use falkordb_native_host::{
    Engine,
    server::{ServerConfig, TlsConfig, serve},
};

fn main() -> Result<(), String> {
    let _engine = Engine::init()?;

    let mut config = ServerConfig::default();
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

    let mut tls_cert: Option<PathBuf> = env::var_os("FALKORDB_TLS_CERT").map(PathBuf::from);
    let mut tls_key: Option<PathBuf> = env::var_os("FALKORDB_TLS_KEY").map(PathBuf::from);
    let mut tls_client_ca: Option<PathBuf> =
        env::var_os("FALKORDB_TLS_CLIENT_CA").map(PathBuf::from);

    let mut args = env::args().skip(1);
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
            "--data-dir" => {
                let value = args.next().ok_or_else(|| "--data-dir requires a path".to_string())?;
                config.data_dir = PathBuf::from(value);
            }
            "--username" => {
                config.username = args.next().ok_or_else(|| "--username requires a value".to_string())?;
            }
            "--password" => {
                config.password = Some(args.next().ok_or_else(|| "--password requires a value".to_string())?);
            }
            "--tls-cert" => {
                tls_cert = Some(PathBuf::from(
                    args.next().ok_or_else(|| "--tls-cert requires a PEM path".to_string())?,
                ));
            }
            "--tls-key" => {
                tls_key = Some(PathBuf::from(
                    args.next().ok_or_else(|| "--tls-key requires a PEM path".to_string())?,
                ));
            }
            "--tls-client-ca" => {
                tls_client_ca = Some(PathBuf::from(
                    args.next().ok_or_else(|| "--tls-client-ca requires a PEM path".to_string())?,
                ));
            }
            "--allow-plaintext-remote" => {
                config.allow_plaintext_remote = true;
            }
            "--allow-unauthenticated-remote" => {
                config.allow_unauthenticated_remote = true;
            }
            "--help" | "-h" => {
                println!(r#"falkordb-native-server

USAGE:
  falkordb-native-server [OPTIONS]

OPTIONS:
  --bind HOST:PORT                 Bind address (default 127.0.0.1:6379)
  --port PORT                      Override port
  --data-dir PATH                  Persistent graph data directory
  --username USER                  AUTH username (default: default)
  --password PASSWORD              AUTH password (or FALKORDB_PASSWORD)
  --tls-cert PATH                  PEM server certificate/chain (or FALKORDB_TLS_CERT)
  --tls-key PATH                   PEM private key (or FALKORDB_TLS_KEY)
  --tls-client-ca PATH             Require mTLS clients signed by this PEM CA
  --allow-plaintext-remote         Permit non-loopback RESP without TLS
  --allow-unauthenticated-remote   Permit non-loopback bind without AUTH
"#);
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    match (tls_cert, tls_key) {
        (Some(cert_path), Some(key_path)) => {
            config.tls = Some(TlsConfig {
                cert_path,
                key_path,
                client_ca_path: tls_client_ca,
            });
        }
        (None, None) if tls_client_ca.is_none() => {}
        (None, None) => {
            return Err("--tls-client-ca requires --tls-cert and --tls-key".to_string());
        }
        _ => {
            return Err("--tls-cert and --tls-key must be provided together".to_string());
        }
    }

    serve(config)
}
