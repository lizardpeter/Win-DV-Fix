use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
};

use falkordb_native_host::{
    Engine,
    server::{ServerConfig, serve},
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
    if let Ok(cert) = env::var("FALKORDB_TLS_CERT") {
        if !cert.is_empty() {
            config.tls_cert = Some(PathBuf::from(cert));
        }
    }
    if let Ok(key) = env::var("FALKORDB_TLS_KEY") {
        if !key.is_empty() {
            config.tls_key = Some(PathBuf::from(key));
        }
    }

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
            "--allow-unauthenticated-remote" => {
                config.allow_unauthenticated_remote = true;
            }
            "--allow-insecure-remote" => {
                config.allow_insecure_remote = true;
            }
            "--tls-cert" => {
                let value = args.next().ok_or_else(|| "--tls-cert requires a PEM path".to_string())?;
                config.tls_cert = Some(PathBuf::from(value));
            }
            "--tls-key" => {
                let value = args.next().ok_or_else(|| "--tls-key requires a PEM path".to_string())?;
                config.tls_key = Some(PathBuf::from(value));
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
  --tls-cert PATH                  PEM certificate (or FALKORDB_TLS_CERT)
  --tls-key PATH                   PEM private key (or FALKORDB_TLS_KEY)
  --allow-unauthenticated-remote   Allow non-loopback bind without AUTH
  --allow-insecure-remote          Allow non-loopback plaintext RESP
"#);
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    serve(config)
}
