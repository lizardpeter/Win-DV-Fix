use std::{
    env,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    thread,
};

use falkordb_native_host::{
    Engine,
    api::{ApiConfig, serve_api},
    oauth::{OAuthConfig, OAuthVerifier},
    server::{GraphCatalog, ServerConfig, TlsConfig, serve_with_catalog},
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
    let mut api_openai_mtls_ca: Option<PathBuf> =
        env::var_os("FALKORDB_API_OPENAI_MTLS_CA").map(PathBuf::from);

    let mut oauth_resource = env::var("FALKORDB_OAUTH_RESOURCE").ok().filter(|v| !v.is_empty());
    let mut oauth_issuer = env::var("FALKORDB_OAUTH_ISSUER").ok().filter(|v| !v.is_empty());
    let mut oauth_audience = env::var("FALKORDB_OAUTH_AUDIENCE").ok().filter(|v| !v.is_empty());
    let mut oauth_jwks_url = env::var("FALKORDB_OAUTH_JWKS_URL").ok().filter(|v| !v.is_empty());
    let mut oauth_read_scope = env::var("FALKORDB_OAUTH_READ_SCOPE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "graph:read".to_string());
    let mut oauth_write_scope = env::var("FALKORDB_OAUTH_WRITE_SCOPE")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "graph:write".to_string());

    // A configured API credential implies a localhost API even when no bind
    // was specified, making the ChatGPT surface easy to enable safely.
    if api_bind.is_none()
        && (api_token.is_some()
            || api_read_token.is_some()
            || oauth_resource.is_some()
            || oauth_issuer.is_some()
            || oauth_audience.is_some()
            || oauth_jwks_url.is_some())
    {
        api_bind = Some("127.0.0.1:8443".parse().expect("valid default API bind"));
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
            "--api-openai-mtls-ca" => {
                api_openai_mtls_ca = Some(PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--api-openai-mtls-ca requires a PEM CA path".to_string())?,
                ));
            }
            "--oauth-resource" => {
                oauth_resource = Some(
                    args.next()
                        .ok_or_else(|| "--oauth-resource requires a URL".to_string())?,
                );
            }
            "--oauth-issuer" => {
                oauth_issuer = Some(
                    args.next()
                        .ok_or_else(|| "--oauth-issuer requires a URL".to_string())?,
                );
            }
            "--oauth-audience" => {
                oauth_audience = Some(
                    args.next()
                        .ok_or_else(|| "--oauth-audience requires a value".to_string())?,
                );
            }
            "--oauth-jwks-url" => {
                oauth_jwks_url = Some(
                    args.next()
                        .ok_or_else(|| "--oauth-jwks-url requires a URL".to_string())?,
                );
            }
            "--oauth-read-scope" => {
                oauth_read_scope = args.next()
                    .ok_or_else(|| "--oauth-read-scope requires a value".to_string())?;
            }
            "--oauth-write-scope" => {
                oauth_write_scope = args.next()
                    .ok_or_else(|| "--oauth-write-scope requires a value".to_string())?;
            }
            "--help" | "-h" => {
                println!(r#"falkordb-native-server

USAGE:
  falkordb-native-server [OPTIONS]

RESP/FalkorDB OPTIONS:
  --bind HOST:PORT                 RESP bind address (default 127.0.0.1:6379)
  --port PORT                      Override RESP port
  --data-dir PATH                  Persistent graph data directory
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
  --api-openai-mtls-ca PATH        Require OpenAI connector mTLS using this CA;
                                   leaf SAN must be mtls.prod.connectors.openai.com
  --oauth-resource URL             Public HTTPS MCP resource identifier
  --oauth-issuer URL               OAuth/OIDC token issuer
  --oauth-audience AUDIENCE        Required access-token audience
  --oauth-jwks-url URL             Issuer JWKS URL for JWT validation
  --oauth-read-scope SCOPE         Read scope (default graph:read)
  --oauth-write-scope SCOPE        Write scope (default graph:write)

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
            client_dns_name: None,
        });
    }

    let oauth = {
        let supplied = [
            oauth_resource.is_some(),
            oauth_issuer.is_some(),
            oauth_audience.is_some(),
            oauth_jwks_url.is_some(),
        ];
        if supplied.iter().any(|v| *v) && !supplied.iter().all(|v| *v) {
            return Err(
                "OAuth requires --oauth-resource, --oauth-issuer, --oauth-audience, and --oauth-jwks-url together"
                    .to_string(),
            );
        }

        match (oauth_resource, oauth_issuer, oauth_audience, oauth_jwks_url) {
            (Some(resource), Some(issuer), Some(audience), Some(jwks_url)) => {
                Some(Arc::new(OAuthVerifier::new(OAuthConfig {
                    resource,
                    issuer,
                    audience,
                    jwks_url,
                    read_scope: oauth_read_scope,
                    write_scope: oauth_write_scope,
                })?))
            }
            (None, None, None, None) => None,
            _ => unreachable!("OAuth completeness checked above"),
        }
    };

    let catalog = Arc::new(GraphCatalog::open(&config.data_dir)?);

    if let Some(bind) = api_bind {
        let api_config = ApiConfig {
            bind,
            read_write_token: api_token,
            read_only_token: api_read_token,
            oauth,
            allow_unauthenticated_remote: api_allow_unauthenticated_remote,
            allow_plaintext_remote: api_allow_plaintext_remote,
            tls: shared_tls.as_ref().map(|(cert_path, key_path)| TlsConfig {
                cert_path: cert_path.clone(),
                key_path: key_path.clone(),
                client_ca_path: api_openai_mtls_ca,
                client_dns_name: api_openai_mtls_ca
                    .as_ref()
                    .map(|_| "mtls.prod.connectors.openai.com".to_string()),
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
