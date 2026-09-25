use std::{
    collections::HashSet,
    fmt,
    time::{Duration, Instant},
};

use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{Jwk, JwkSet},
};
use parking_lot::RwLock;
use reqwest::{Url, blocking::Client};
use serde::Deserialize;

const DEFAULT_JWKS_TTL: Duration = Duration::from_secs(10 * 60);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct OAuthConfig {
    /// Canonical HTTPS identifier of this MCP resource server.
    pub resource: String,
    /// OAuth/OIDC issuer exactly as expected in the access-token `iss` claim.
    pub issuer: String,
    /// Audience/resource value expected in the access-token `aud` claim.
    pub audience: String,
    /// JWKS endpoint for verifying access-token signatures.
    pub jwks_url: String,
    pub read_scope: String,
    pub write_scope: String,
}

impl OAuthConfig {
    pub fn protected_resource_metadata_url(&self) -> Result<String, String> {
        let resource = Url::parse(&self.resource)
            .map_err(|e| format!("invalid OAuth resource URL {:?}: {e}", self.resource))?;
        let mut metadata = resource.clone();

        let path = resource.path().trim_matches('/');
        let metadata_path = if path.is_empty() {
            "/.well-known/oauth-protected-resource".to_string()
        } else {
            format!("/.well-known/oauth-protected-resource/{path}")
        };
        metadata.set_path(&metadata_path);
        metadata.set_query(None);
        metadata.set_fragment(None);
        Ok(metadata.to_string().trim_end_matches('/').to_string())
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_https_or_loopback_url(&self.resource, "OAuth resource")?;
        validate_https_or_loopback_url(&self.issuer, "OAuth issuer")?;
        validate_https_or_loopback_url(&self.jwks_url, "OAuth JWKS URL")?;

        if self.audience.trim().is_empty() {
            return Err("OAuth audience must not be empty".to_string());
        }
        if self.read_scope.trim().is_empty() || self.write_scope.trim().is_empty() {
            return Err("OAuth read/write scopes must not be empty".to_string());
        }
        if self.read_scope == self.write_scope {
            return Err("OAuth read and write scopes must be distinct".to_string());
        }
        Ok(())
    }
}

fn validate_https_or_loopback_url(value: &str, label: &str) -> Result<(), String> {
    let url = Url::parse(value).map_err(|e| format!("invalid {label} URL {value:?}: {e}"))?;
    if url.scheme() == "https" {
        return Ok(());
    }

    if url.scheme() == "http" {
        let loopback = matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1")
        );
        if loopback {
            return Ok(());
        }
    }

    Err(format!(
        "{label} must use HTTPS (HTTP is accepted only for loopback testing)"
    ))
}

#[derive(Debug, Clone)]
pub struct OAuthPrincipal {
    pub subject: Option<String>,
    pub scopes: HashSet<String>,
}

impl OAuthPrincipal {
    #[must_use]
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

#[derive(Default)]
struct CachedJwks {
    set: Option<JwkSet>,
    fetched_at: Option<Instant>,
}

pub struct OAuthVerifier {
    config: OAuthConfig,
    client: Client,
    cache: RwLock<CachedJwks>,
    jwks_ttl: Duration,
}

impl fmt::Debug for OAuthVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OAuthVerifier")
            .field("resource", &self.config.resource)
            .field("issuer", &self.config.issuer)
            .field("audience", &self.config.audience)
            .field("jwks_url", &self.config.jwks_url)
            .field("read_scope", &self.config.read_scope)
            .field("write_scope", &self.config.write_scope)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Deserialize)]
struct AccessClaims {
    #[serde(default)]
    sub: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    scp: Option<String>,
    #[serde(default)]
    permissions: Vec<String>,
}

impl OAuthVerifier {
    pub fn new(config: OAuthConfig) -> Result<Self, String> {
        config.validate()?;
        let client = Client::builder()
            .timeout(HTTP_TIMEOUT)
            .user_agent("reversalgraph-native/1.0")
            .build()
            .map_err(|e| format!("build OAuth JWKS HTTP client: {e}"))?;

        Ok(Self {
            config,
            client,
            cache: RwLock::new(CachedJwks::default()),
            jwks_ttl: DEFAULT_JWKS_TTL,
        })
    }

    #[must_use]
    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    pub fn verify(&self, token: &str) -> Result<OAuthPrincipal, String> {
        if token.len() > 32 * 1024 {
            return Err("OAuth access token is unreasonably large".to_string());
        }

        let header = decode_header(token)
            .map_err(|e| format!("invalid OAuth JWT header: {e}"))?;
        let kid = header
            .kid
            .as_deref()
            .ok_or_else(|| "OAuth JWT is missing kid".to_string())?;

        if !matches!(
            header.alg,
            Algorithm::RS256
                | Algorithm::RS384
                | Algorithm::RS512
                | Algorithm::PS256
                | Algorithm::PS384
                | Algorithm::PS512
        ) {
            return Err(format!(
                "OAuth JWT algorithm {:?} is not an allowed RSA algorithm",
                header.alg
            ));
        }

        let jwk = self.key_for(kid)?;
        let key = DecodingKey::from_jwk(&jwk)
            .map_err(|e| format!("build OAuth decoding key for kid {kid:?}: {e}"))?;

        let mut validation = Validation::new(header.alg);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.set_issuer(&[self.config.issuer.as_str()]);
        validation.set_audience(&[self.config.audience.as_str()]);
        validation.validate_nbf = true;
        validation.leeway = 60;

        let data = decode::<AccessClaims>(token, &key, &validation)
            .map_err(|e| format!("OAuth access token validation failed: {e}"))?;

        let mut scopes = HashSet::new();
        if let Some(scope) = data.claims.scope {
            scopes.extend(scope.split_ascii_whitespace().map(ToOwned::to_owned));
        }
        if let Some(scope) = data.claims.scp {
            scopes.extend(scope.split_ascii_whitespace().map(ToOwned::to_owned));
        }
        scopes.extend(data.claims.permissions);

        Ok(OAuthPrincipal {
            subject: data.claims.sub,
            scopes,
        })
    }

    fn key_for(&self, kid: &str) -> Result<Jwk, String> {
        {
            let cache = self.cache.read();
            let fresh = cache
                .fetched_at
                .is_some_and(|at| at.elapsed() <= self.jwks_ttl);
            if fresh {
                if let Some(jwk) = cache.set.as_ref().and_then(|set| set.find(kid)) {
                    return Ok(jwk.clone());
                }
            }
        }

        // Refresh on expiry or unknown kid so normal issuer key rotation works
        // without restarting the graph service.
        self.refresh_jwks()?;

        self.cache
            .read()
            .set
            .as_ref()
            .and_then(|set| set.find(kid))
            .cloned()
            .ok_or_else(|| format!("OAuth JWKS does not contain kid {kid:?}"))
    }

    fn refresh_jwks(&self) -> Result<(), String> {
        let response = self
            .client
            .get(&self.config.jwks_url)
            .send()
            .map_err(|e| format!("fetch OAuth JWKS: {e}"))?
            .error_for_status()
            .map_err(|e| format!("OAuth JWKS endpoint returned an error: {e}"))?;

        let set: JwkSet = response
            .json()
            .map_err(|e| format!("decode OAuth JWKS: {e}"))?;

        if set.keys.is_empty() {
            return Err("OAuth JWKS contained no keys".to_string());
        }

        let mut cache = self.cache.write();
        cache.set = Some(set);
        cache.fetched_at = Some(Instant::now());
        Ok(())
    }
}
