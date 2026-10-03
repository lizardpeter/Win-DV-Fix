use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use parking_lot::Mutex;
use ring::{
    digest,
    hmac,
    rand::{SecureRandom, SystemRandom},
};
use serde_json::{Map as JsonMap, Value as JsonValue, json};

use crate::{
    api::{ApiConfig, HttpRequest, HttpResponse, constant_time_eq, json_response, query_output_json, raw_response},
    server::GraphCatalog,
};

const PROTOCOL_MODERN: &str = "2026-07-28";
pub const OAUTH_BUILD_ID: &str = "oauth-pairing-v3-20261001";
const SERVER_VERSION: &str = "0.5.0";
pub const TOOLSET_VERSION: &str = "2026-10-01.4";
const PROTOCOL_LEGACY: &str = "2025-11-25";
const ACCESS_TOKEN_TTL_SECS: u64 = 60 * 60;
const REFRESH_TOKEN_TTL_SECS: u64 = 365 * 24 * 60 * 60;
const AUTH_CODE_TTL_SECS: u64 = 10 * 60;
const MAX_MCP_BATCH_QUERIES: usize = 100;

const SCOPE_READ: &str = "graph:read";
const SCOPE_WRITE: &str = "graph:write";
const SCOPE_ADMIN: &str = "graph:admin";
const ALL_SCOPES: &str = "graph:read graph:write graph:admin";
const CLAUDE_CODE_CLIENT_ID: &str = "https://claude.ai/oauth/claude-code-client-metadata";

#[derive(Debug, Clone)]
struct OAuthCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    resource: String,
    issuer: String,
    scope: String,
    expires_at: u64,
}

static OAUTH_CODES: OnceLock<Mutex<HashMap<String, OAuthCode>>> = OnceLock::new();

fn oauth_codes() -> &'static Mutex<HashMap<String, OAuthCode>> {
    OAUTH_CODES.get_or_init(|| Mutex::new(HashMap::new()))
}

#[derive(Clone, Copy, Default)]
struct GrantedScopes {
    read: bool,
    write: bool,
    admin: bool,
}

impl GrantedScopes {
    fn full() -> Self {
        Self {
            read: true,
            write: true,
            admin: true,
        }
    }

    fn read_only() -> Self {
        Self {
            read: true,
            write: false,
            admin: false,
        }
    }

    fn allows(self, required: RequiredScope) -> bool {
        match required {
            RequiredScope::Read => self.read || self.write || self.admin,
            RequiredScope::Write => self.write || self.admin,
            RequiredScope::Admin => self.admin,
        }
    }
}

#[derive(Clone, Copy)]
enum RequiredScope {
    Read,
    Write,
    Admin,
}

impl RequiredScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Read => SCOPE_READ,
            Self::Write => SCOPE_WRITE,
            Self::Admin => SCOPE_ADMIN,
        }
    }
}

pub(crate) fn route_http(
    request: &HttpRequest,
    catalog: &GraphCatalog,
    config: &ApiConfig,
) -> Option<HttpResponse> {
    let path = request
        .target
        .split_once('?')
        .map_or(request.target.as_str(), |(path, _)| path);

    if config.tunnel_mode && path != "/mcp" {
        return None;
    }

    match path {
        "/.well-known/oauth-protected-resource"
        | "/.well-known/oauth-protected-resource/mcp" => {
            return Some(protected_resource_metadata(request, config));
        }
        "/.well-known/oauth-authorization-server"
        | "/.well-known/openid-configuration" => {
            return Some(authorization_server_metadata(request, config));
        }
        "/oauth/authorize" => {
            return Some(match request.method.as_str() {
                "GET" => oauth_authorize_get(request, config),
                "POST" => oauth_authorize_post(request, config),
                _ => method_not_allowed("GET, POST"),
            });
        }
        "/oauth/token" => {
            return Some(match request.method.as_str() {
                "POST" => oauth_token(request, config),
                _ => method_not_allowed("POST"),
            });
        }
        "/mcp" => {}
        _ => return None,
    }

    let response = match request.method.as_str() {
        "POST" => mcp_post(request, catalog, config),
        "OPTIONS" => mcp_options_response(),
        // Stateless JSON-response Streamable HTTP uses POST only for protocol
        // messages. Legacy GET/DELETE probes receive an explicit 405.
        "GET" | "DELETE" => method_not_allowed("POST, OPTIONS"),
        _ => method_not_allowed("POST, OPTIONS"),
    };
    Some(with_mcp_cors(response))
}

fn mcp_options_response() -> HttpResponse {
    let mut response = raw_response(204, "text/plain; charset=utf-8", Vec::new());
    response
        .headers
        .push(("Allow".to_string(), "POST, OPTIONS".to_string()));
    response
}

fn with_mcp_cors(mut response: HttpResponse) -> HttpResponse {
    // Authorization is carried in Bearer tokens, not cookies. Browser-based
    // connector setup can therefore preflight safely while privileged calls
    // remain protected by the same OAuth scope checks.
    response.headers.push((
        "Access-Control-Allow-Origin".to_string(),
        "*".to_string(),
    ));
    response.headers.push((
        "Access-Control-Allow-Methods".to_string(),
        "POST, OPTIONS".to_string(),
    ));
    response.headers.push((
        "Access-Control-Allow-Headers".to_string(),
        "accept, authorization, content-type, mcp-protocol-version, mcp-method, mcp-name".to_string(),
    ));
    response.headers.push((
        "Access-Control-Expose-Headers".to_string(),
        "WWW-Authenticate".to_string(),
    ));
    response.headers.push((
        "Vary".to_string(),
        "Origin, Access-Control-Request-Method, Access-Control-Request-Headers".to_string(),
    ));
    response
}

fn protected_resource_metadata(request: &HttpRequest, config: &ApiConfig) -> HttpResponse {
    let Some(base) = request_base(request, config) else {
        return oauth_error(400, "invalid_request", "missing or invalid Host header");
    };
    json_response(
        200,
        json!({
            "resource": format!("{base}/mcp"),
            "authorization_servers": [base],
            "scopes_supported": [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN],
            "resource_documentation": format!("{}/openapi.json", request_base(request, config).unwrap_or_default())
        }),
    )
}

fn authorization_server_metadata(request: &HttpRequest, config: &ApiConfig) -> HttpResponse {
    let Some(base) = request_base(request, config) else {
        return oauth_error(400, "invalid_request", "missing or invalid Host header");
    };
    json_response(
        200,
        json!({
            "issuer": base,
            "authorization_response_iss_parameter_supported": true,
            "authorization_endpoint": format!("{base}/oauth/authorize"),
            "token_endpoint": format!("{base}/oauth/token"),
            "client_id_metadata_document_supported": true,
            "token_endpoint_auth_methods_supported": ["none"],
            "response_types_supported": ["code"],
            "grant_types_supported": ["authorization_code", "refresh_token"],
            "code_challenge_methods_supported": ["S256"],
            "scopes_supported": [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN]
        }),
    )
}

fn oauth_authorize_get(request: &HttpRequest, config: &ApiConfig) -> HttpResponse {
    if config.oauth_owner_secret.is_none()
        && config.read_write_token.is_none()
        && config.oauth_owner_secret_fallbacks.is_empty()
        && config.oauth_pairing_code.is_none()
    {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "OAuth linking requires a configured owner secret",
        );
    }

    let Some(base) = request_base(request, config) else {
        return oauth_error(400, "invalid_request", "missing or invalid Host header");
    };
    let params = target_query_params(&request.target);

    let validated = match validate_authorize_params(&params, &base) {
        Ok(v) => v,
        Err(message) => return oauth_error(400, "invalid_request", &message),
    };

    let scope_display = html_escape(&validated.scope);
    let client_display = html_escape(mcp_client_display_name(&validated.client_id));
    let html = format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Authorize FalkorDB Graph Access</title>
<style>
body{{font-family:system-ui,-apple-system,Segoe UI,sans-serif;background:#111;color:#eee;max-width:620px;margin:64px auto;padding:0 24px}}
.card{{background:#1d1d1d;border:1px solid #444;border-radius:14px;padding:28px}}
input[type=password]{{box-sizing:border-box;width:100%;padding:12px;margin:10px 0 18px;background:#090909;color:#fff;border:1px solid #666;border-radius:8px}}
button{{padding:11px 18px;border:0;border-radius:8px;font-weight:700;cursor:pointer}}
code{{word-break:break-word}}
.small{{color:#aaa;font-size:.9rem}}
</style>
</head>
<body>
<div class="card">
<h1>Authorize {client_display}</h1>
<p>Grant {client_display} direct access to this FalkorDB server with these scopes:</p>
<p><code>{scope_display}</code></p>
<p><strong>Recommended:</strong> enter the restart-scoped <strong>OAuth pairing code</strong> printed in the server console. It is independent of password, environment-variable, and secrets-file parsing.</p>
<p>You may also use the RESP server password or <strong>FALKORDB_API_TOKEN</strong>. Raw values, complete <code>NAME=value</code> lines, and the complete <code>falkordb-secrets.txt</code> contents are accepted.</p>
<p class="small">OAuth build: <code>{build_id}</code></p>
<form method="post" action="/oauth/authorize">
{hidden}
<label for="owner_secret">OAuth pairing code or FalkorDB owner secret</label>
<input id="owner_secret" name="owner_secret" type="password" autocomplete="current-password" required autofocus>
<button type="submit">Authorize {client_display}</button>
</form>
<p class="small">The owner secret is sent only to this server over HTTPS and is never returned to the MCP client.</p>
</div>
</body>
</html>"#,
        hidden = validated.hidden_fields(),
        build_id = OAUTH_BUILD_ID,
        client_display = client_display
    );

    let mut response = raw_response(200, "text/html; charset=utf-8", html.into_bytes());
    response.headers.push((
        "Content-Security-Policy".to_string(),
        "default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; base-uri 'none'; frame-ancestors 'none'".to_string(),
    ));
    response
}

fn oauth_authorize_post(request: &HttpRequest, config: &ApiConfig) -> HttpResponse {
    if config.oauth_owner_secret.is_none()
        && config.read_write_token.is_none()
        && config.oauth_owner_secret_fallbacks.is_empty()
        && config.oauth_pairing_code.is_none()
    {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "OAuth linking requires a configured owner secret",
        );
    }
    let Some(base) = request_base(request, config) else {
        return oauth_error(400, "invalid_request", "missing or invalid Host header");
    };

    let body = match std::str::from_utf8(&request.body) {
        Ok(body) => body,
        Err(_) => return oauth_error(400, "invalid_request", "form body is not UTF-8"),
    };
    let params = parse_urlencoded(body);
    let validated = match validate_authorize_params(&params, &base) {
        Ok(v) => v,
        Err(message) => return oauth_error(400, "invalid_request", &message),
    };

    // Accept the new owner_secret field and the old api_token field so a
    // browser page opened by a previous build can still complete after restart.
    let supplied = params
        .get("owner_secret")
        .or_else(|| params.get("api_token"))
        .map_or("", String::as_str);
    if !owner_secret_matches(config, supplied) {
        let mut response = raw_response(
            403,
            "text/html; charset=utf-8",
            format!(
                "<!doctype html><title>Authorization failed</title><h1>Authorization failed</h1><p>The FalkorDB owner secret was not accepted.</p><p>This server currently has <strong>{}</strong> distinct owner credential source(s) available for OAuth. You may paste the raw password/token, a complete FALKORDB_PASSWORD=... or FALKORDB_API_TOKEN=... line, or the complete falkordb-secrets.txt contents.</p>",
                usize::from(config.oauth_pairing_code.is_some())
                    + usize::from(config.oauth_owner_secret.is_some())
                    + usize::from(config.read_write_token.is_some())
                    + config.oauth_owner_secret_fallbacks.len()
            ).into_bytes(),
        );
        response.headers.push((
            "Content-Security-Policy".to_string(),
            "default-src 'none'; frame-ancestors 'none'".to_string(),
        ));
        return response;
    }

    cleanup_expired_codes();
    let code = match random_token("ac_", 32) {
        Ok(v) => v,
        Err(err) => return oauth_error(500, "server_error", &err),
    };
    oauth_codes().lock().insert(
        code.clone(),
        OAuthCode {
            client_id: validated.client_id.clone(),
            redirect_uri: validated.redirect_uri.clone(),
            code_challenge: validated.code_challenge.clone(),
            resource: validated.resource.clone(),
            issuer: base.clone(),
            scope: validated.scope.clone(),
            expires_at: now_secs().saturating_add(AUTH_CODE_TTL_SECS),
        },
    );

    let separator = if validated.redirect_uri.contains('?') { '&' } else { '?' };
    let mut location = format!(
        "{}{}code={}&iss={}",
        validated.redirect_uri,
        separator,
        percent_encode(&code),
        percent_encode(&base)
    );
    if !validated.state.is_empty() {
        location.push_str("&state=");
        location.push_str(&percent_encode(&validated.state));
    }

    let mut response = raw_response(302, "text/plain; charset=utf-8", Vec::new());
    response.headers.push(("Location".to_string(), location));
    response
}

fn owner_secret_matches(config: &ApiConfig, supplied: &str) -> bool {
    let candidates = normalize_owner_secret_candidates(supplied);
    if candidates.is_empty() {
        return false;
    }

    let matches_expected = |expected: &str| {
        candidates
            .iter()
            .any(|candidate| constant_time_eq(expected, candidate))
    };

    config
        .oauth_pairing_code
        .as_deref()
        .is_some_and(matches_expected)
        || config
            .oauth_owner_secret
            .as_deref()
            .is_some_and(matches_expected)
        || config
            .read_write_token
            .as_deref()
            .is_some_and(matches_expected)
        || config
            .oauth_owner_secret_fallbacks
            .iter()
            .any(|expected| matches_expected(expected))
}

fn normalize_owner_secret_candidates(input: &str) -> Vec<String> {
    let mut candidates = Vec::new();

    let mut push_candidate = |value: &str| {
        let value = unquote_owner_secret(value.trim());
        if !value.is_empty() && !candidates.iter().any(|existing| existing == &value) {
            candidates.push(value);
        }
    };

    // Accept a raw secret, a full NAME=value assignment, or even the complete
    // two-line falkordb-secrets.txt contents pasted into the form.
    let trimmed = input.trim();
    if !trimmed.contains('\n') && !trimmed.contains('\r') {
        push_candidate(trimmed);
    }

    for line in input.lines() {
        let line = line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }

        if let Some((name, value)) = line.split_once('=') {
            let mut key = name.trim();
            if let Some(rest) = key.strip_prefix("set ") {
                key = rest.trim();
            }
            if let Some(rest) = key.strip_prefix("$env:") {
                key = rest.trim();
            }
            if key.eq_ignore_ascii_case("FALKORDB_PASSWORD")
                || key.eq_ignore_ascii_case("FALKORDB_API_TOKEN")
            {
                push_candidate(value);
                continue;
            }
        }

        push_candidate(line);
    }

    candidates
}

fn unquote_owner_secret(value: &str) -> String {
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

fn oauth_token(request: &HttpRequest, config: &ApiConfig) -> HttpResponse {
    let Some(secret) = config.read_write_token.as_deref() else {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "OAuth token issuance requires a configured read/write API token",
        );
    };
    let body = match std::str::from_utf8(&request.body) {
        Ok(body) => body,
        Err(_) => return oauth_error(400, "invalid_request", "form body is not UTF-8"),
    };
    let params = parse_urlencoded(body);
    match params.get("grant_type").map(String::as_str) {
        Some("authorization_code") => exchange_authorization_code(&params, secret),
        Some("refresh_token") => exchange_refresh_token(&params, secret),
        _ => oauth_error(400, "unsupported_grant_type", "unsupported grant_type"),
    }
}

fn exchange_authorization_code(params: &HashMap<String, String>, secret: &str) -> HttpResponse {
    cleanup_expired_codes();
    let Some(code_value) = params.get("code") else {
        return oauth_error(400, "invalid_request", "code is required");
    };
    let Some(record) = oauth_codes().lock().get(code_value).cloned() else {
        return oauth_error(400, "invalid_grant", "authorization code is invalid or expired");
    };

    if record.expires_at < now_secs() {
        oauth_codes().lock().remove(code_value);
        return oauth_error(400, "invalid_grant", "authorization code expired");
    }
    if params.get("client_id") != Some(&record.client_id) {
        return oauth_error(400, "invalid_grant", "client_id does not match authorization code");
    }
    if params.get("redirect_uri") != Some(&record.redirect_uri) {
        return oauth_error(400, "invalid_grant", "redirect_uri does not match authorization code");
    }
    if params.get("resource") != Some(&record.resource) {
        return oauth_error(400, "invalid_target", "resource does not match authorization code");
    }

    let Some(verifier) = params.get("code_verifier") else {
        return oauth_error(400, "invalid_grant", "code_verifier is required");
    };
    if pkce_s256(verifier) != record.code_challenge {
        return oauth_error(400, "invalid_grant", "PKCE verification failed");
    }

    oauth_codes().lock().remove(code_value);
    issue_token_pair(
        secret,
        &record.issuer,
        &record.resource,
        &record.scope,
    )
}

fn exchange_refresh_token(params: &HashMap<String, String>, secret: &str) -> HttpResponse {
    let Some(refresh) = params.get("refresh_token") else {
        return oauth_error(400, "invalid_request", "refresh_token is required");
    };
    let Some(resource) = params.get("resource") else {
        return oauth_error(400, "invalid_target", "resource is required");
    };
    let issuer = resource.strip_suffix("/mcp").unwrap_or(resource);

    let token = match verify_signed_token(secret, refresh, issuer, resource, "refresh") {
        Ok(token) => token,
        Err(message) => return oauth_error(400, "invalid_grant", &message),
    };

    let scope = params
        .get("scope")
        .map(String::as_str)
        .unwrap_or(&token.scope);
    if let Err(message) = validate_scope_subset(scope, &token.scope) {
        return oauth_error(400, "invalid_scope", &message);
    }

    issue_token_pair(secret, issuer, resource, scope)
}

fn issue_token_pair(secret: &str, issuer: &str, resource: &str, scope: &str) -> HttpResponse {
    let access = match sign_token(
        secret,
        "access",
        issuer,
        resource,
        scope,
        ACCESS_TOKEN_TTL_SECS,
    ) {
        Ok(v) => v,
        Err(err) => return oauth_error(500, "server_error", &err),
    };
    let refresh = match sign_token(
        secret,
        "refresh",
        issuer,
        resource,
        scope,
        REFRESH_TOKEN_TTL_SECS,
    ) {
        Ok(v) => v,
        Err(err) => return oauth_error(500, "server_error", &err),
    };

    json_response(
        200,
        json!({
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": ACCESS_TOKEN_TTL_SECS,
            "refresh_token": refresh,
            "scope": scope
        }),
    )
}

#[derive(Debug)]
struct ValidatedAuthorize {
    client_id: String,
    redirect_uri: String,
    state: String,
    code_challenge: String,
    resource: String,
    scope: String,
}

impl ValidatedAuthorize {
    fn hidden_fields(&self) -> String {
        [
            ("response_type", "code"),
            ("client_id", self.client_id.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("state", self.state.as_str()),
            ("code_challenge", self.code_challenge.as_str()),
            ("code_challenge_method", "S256"),
            ("resource", self.resource.as_str()),
            ("scope", self.scope.as_str()),
        ]
        .iter()
        .map(|(name, value)| {
            format!(
                r#"<input type="hidden" name="{}" value="{}">"#,
                html_escape(name),
                html_escape(value)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
    }
}

fn validate_authorize_params(
    params: &HashMap<String, String>,
    base: &str,
) -> Result<ValidatedAuthorize, String> {
    if params.get("response_type").map(String::as_str) != Some("code") {
        return Err("response_type must be code".to_string());
    }

    let client_id = required_param(params, "client_id")?;
    if !allowed_mcp_client_id(client_id) {
        return Err("client_id is not an accepted MCP OAuth client".to_string());
    }

    let redirect_uri = required_param(params, "redirect_uri")?;
    if !allowed_mcp_redirect(client_id, redirect_uri) {
        return Err("redirect_uri is not accepted for this MCP OAuth client".to_string());
    }

    if params.get("code_challenge_method").map(String::as_str) != Some("S256") {
        return Err("code_challenge_method must be S256".to_string());
    }
    let code_challenge = required_param(params, "code_challenge")?;
    if code_challenge.len() < 32 || code_challenge.len() > 128 {
        return Err("invalid PKCE code_challenge".to_string());
    }

    let expected_resource = format!("{base}/mcp");
    let resource = required_param(params, "resource")?;
    if resource != expected_resource {
        return Err(format!("resource must be {expected_resource}"));
    }

    let scope = params
        .get("scope")
        .map(String::as_str)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or(ALL_SCOPES);
    validate_requested_scopes(scope)?;

    Ok(ValidatedAuthorize {
        client_id: client_id.to_string(),
        redirect_uri: redirect_uri.to_string(),
        state: params.get("state").cloned().unwrap_or_default(),
        code_challenge: code_challenge.to_string(),
        resource: resource.to_string(),
        scope: canonical_scope(scope),
    })
}

fn allowed_mcp_client_id(client_id: &str) -> bool {
    allowed_chatgpt_client_id(client_id) || client_id == CLAUDE_CODE_CLIENT_ID
}

fn allowed_mcp_redirect(client_id: &str, uri: &str) -> bool {
    if allowed_chatgpt_client_id(client_id) {
        return allowed_chatgpt_redirect(uri);
    }
    if client_id == CLAUDE_CODE_CLIENT_ID {
        return allowed_claude_code_redirect(uri);
    }
    false
}

fn allowed_claude_code_redirect(uri: &str) -> bool {
    ["http://localhost", "http://127.0.0.1", "http://[::1]"]
        .into_iter()
        .any(|prefix| {
            let Some(rest) = uri.strip_prefix(prefix) else {
                return false;
            };
            if rest == "/callback" {
                return true;
            }
            let Some(without_colon) = rest.strip_prefix(':') else {
                return false;
            };
            let Some((port, path)) = without_colon.split_once('/') else {
                return false;
            };
            !port.is_empty()
                && port.bytes().all(|byte| byte.is_ascii_digit())
                && path == "callback"
        })
}

fn mcp_client_display_name(client_id: &str) -> &'static str {
    if allowed_chatgpt_client_id(client_id) {
        "ChatGPT"
    } else if client_id == CLAUDE_CODE_CLIENT_ID {
        "Claude Code"
    } else {
        "MCP client"
    }
}

fn allowed_chatgpt_client_id(client_id: &str) -> bool {
    client_id == "https://chatgpt.com/oauth/client.json"
        || (client_id.starts_with("https://chatgpt.com/oauth/")
            && client_id.ends_with("/client.json"))
}

fn allowed_chatgpt_redirect(uri: &str) -> bool {
    uri == "https://chatgpt.com/connector_platform_oauth_redirect"
        || uri.starts_with("https://chatgpt.com/connector/oauth/")
}

fn validate_requested_scopes(scope: &str) -> Result<(), String> {
    for item in scope.split_whitespace() {
        if !matches!(item, SCOPE_READ | SCOPE_WRITE | SCOPE_ADMIN) {
            return Err(format!("unsupported scope {item:?}"));
        }
    }
    Ok(())
}

fn validate_scope_subset(requested: &str, granted: &str) -> Result<(), String> {
    validate_requested_scopes(requested)?;
    let granted: Vec<&str> = granted.split_whitespace().collect();
    for item in requested.split_whitespace() {
        if !granted.contains(&item) {
            return Err(format!("scope {item:?} was not granted"));
        }
    }
    Ok(())
}

fn canonical_scope(scope: &str) -> String {
    [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN]
        .into_iter()
        .filter(|candidate| scope.split_whitespace().any(|item| item == *candidate))
        .collect::<Vec<_>>()
        .join(" ")
}

fn mcp_post(request: &HttpRequest, catalog: &GraphCatalog, config: &ApiConfig) -> HttpResponse {
    let rpc: JsonValue = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(err) => {
            return json_response(
                400,
                rpc_error(JsonValue::Null, -32700, &format!("parse error: {err}")),
            );
        }
    };

    let Some(object) = rpc.as_object() else {
        return json_response(400, rpc_error(JsonValue::Null, -32600, "invalid request"));
    };
    if object.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
        return json_response(400, rpc_error(JsonValue::Null, -32600, "jsonrpc must be 2.0"));
    }
    let id = object.get("id").cloned().unwrap_or(JsonValue::Null);
    let Some(method) = object.get("method").and_then(JsonValue::as_str) else {
        return json_response(400, rpc_error(id, -32600, "method is required"));
    };
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
    let modern = is_modern_request(request, &params) || method == "server/discover";

    // JSON-RPC notifications do not receive a JSON-RPC response.
    if object.get("id").is_none() {
        return raw_response(202, "application/json; charset=utf-8", Vec::new());
    }

    let result = match method {
        "server/discover" => server_discover_result(),
        "initialize" => initialize_result(&params),
        "ping" => json!({}),
        "tools/list" => tools_list_result(modern, config.tunnel_mode),
        "tools/call" => {
            let Some(base) = request_base(request, config) else {
                return json_response(
                    400,
                    rpc_error(id, -32600, "missing or invalid Host header"),
                );
            };
            let resource = format!("{base}/mcp");
            match call_tool(&params, request, catalog, config, &base, &resource, modern) {
                Ok(result) => result,
                Err(message) => {
                    return json_response(200, rpc_error(id, -32602, &message));
                }
            }
        }
        "resources/list" => modernize_list(json!({"resources": []}), modern),
        "resources/templates/list" => modernize_list(json!({"resourceTemplates": []}), modern),
        "prompts/list" => modernize_list(json!({"prompts": []}), modern),
        _ => {
            return json_response(200, rpc_error(id, -32601, "method not found"));
        }
    };

    json_response(200, rpc_result(id, modernize_result(result, modern)))
}

fn server_discover_result() -> JsonValue {
    json!({
        "supportedVersions": [
            PROTOCOL_MODERN,
            PROTOCOL_LEGACY,
            "2025-06-18",
            "2025-03-26"
        ],
        "capabilities": {
            "tools": {"listChanged": true}
        },
        "instructions": "Direct access to the user's native FalkorDB graph server. Use read tools for inspection. Use write/admin tools when the user asks to create, update, import, checkpoint, copy, restore, or delete graph data.",
        "ttlMs": 60000,
        "cacheScope": "private",
        "toolsetVersion": TOOLSET_VERSION,
        "resultType": "complete",
        "_meta": server_meta()
    })
}

fn initialize_result(params: &JsonValue) -> JsonValue {
    let requested = params
        .get("protocolVersion")
        .and_then(JsonValue::as_str)
        .unwrap_or(PROTOCOL_LEGACY);
    let negotiated = match requested {
        "2025-03-26" | "2025-06-18" | "2025-11-25" => requested,
        _ => PROTOCOL_LEGACY,
    };
    json!({
        "protocolVersion": negotiated,
        "capabilities": {
            "tools": {"listChanged": true}
        },
        "serverInfo": {
            "name": "falkordb-native-windows",
            "version": SERVER_VERSION
        },
        "toolsetVersion": TOOLSET_VERSION,
        "instructions": "Direct read/write/admin access to the native FalkorDB graph server."
    })
}

fn tools_list_result(modern: bool, tunnel_mode: bool) -> JsonValue {
    let mut tools = vec![
        tool_definition(
            "list_graphs",
            "List graphs",
            "List all persistent named graphs on this FalkorDB server.",
            empty_schema(),
            RequiredScope::Read,
            true,
            false,
            true,
        ),
        tool_definition(
            "database_stats",
            "Database statistics",
            "Show exact persistent storage usage for the database and each graph: WAL bytes, checkpoint bytes/count, total graph storage, node/relationship counts, graph version, and optional sampled in-memory size. Supply graph to restrict the report to one named graph.",
            object_schema(
                json!({
                    "graph": {"type": "string", "minLength": 1},
                    "include_memory": {"type": "boolean", "default": true},
                    "memory_samples": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 100}
                }),
                &[],
            ),
            RequiredScope::Read,
            true,
            false,
            true,
        ),
        tool_definition(
            "create_graph",
            "Create graph",
            "Create an empty named graph if it does not already exist.",
            object_schema(
                json!({"graph": {"type": "string", "minLength": 1}}),
                &["graph"],
            ),
            RequiredScope::Write,
            false,
            false,
            true,
        ),
        tool_definition(
            "query_graph_read",
            "Query graph (read only)",
            "Execute arbitrary Cypher in enforced read-only mode against an existing graph. This cannot create a missing graph or commit writes.",
            object_schema(
                json!({
                    "graph": {"type": "string", "minLength": 1},
                    "cypher": {"type": "string", "minLength": 1}
                }),
                &["graph", "cypher"],
            ),
            RequiredScope::Read,
            true,
            false,
            false,
        ),
        tool_definition(
            "query_graph_write",
            "Query graph (read/write)",
            "Execute arbitrary Cypher with full write capability. A missing graph is created automatically. This can create, modify, delete, index, constrain, or otherwise mutate graph data.",
            object_schema(
                json!({
                    "graph": {"type": "string", "minLength": 1},
                    "cypher": {"type": "string", "minLength": 1}
                }),
                &["graph", "cypher"],
            ),
            RequiredScope::Write,
            false,
            true,
            false,
        ),
        tool_definition(
            "batch_graph_queries",
            "Batch graph queries",
            "Execute up to 100 Cypher operations sequentially. Each item can be read-only or read/write.",
            object_schema(
                json!({
                    "queries": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_MCP_BATCH_QUERIES,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["graph", "cypher"],
                            "properties": {
                                "graph": {"type": "string", "minLength": 1},
                                "cypher": {"type": "string", "minLength": 1},
                                "read_only": {"type": "boolean", "default": false}
                            }
                        }
                    }
                }),
                &["queries"],
            ),
            RequiredScope::Write,
            false,
            true,
            false,
        ),
        tool_definition(
            "copy_graph",
            "Copy graph",
            "Create a persistent copy of one graph under a new graph name.",
            object_schema(
                json!({
                    "source": {"type": "string", "minLength": 1},
                    "destination": {"type": "string", "minLength": 1}
                }),
                &["source", "destination"],
            ),
            RequiredScope::Admin,
            false,
            false,
            false,
        ),
        tool_definition(
            "delete_graph",
            "Delete graph",
            "Permanently delete a named graph, its WAL, and checkpoints.",
            object_schema(
                json!({"graph": {"type": "string", "minLength": 1}}),
                &["graph"],
            ),
            RequiredScope::Admin,
            false,
            true,
            true,
        ),
        tool_definition(
            "checkpoint_graph",
            "Checkpoint graph",
            "Create a durable checkpoint for one graph and compact its covered WAL history.",
            object_schema(
                json!({"graph": {"type": "string", "minLength": 1}}),
                &["graph"],
            ),
            RequiredScope::Admin,
            false,
            false,
            true,
        ),
        tool_definition(
            "checkpoint_all_graphs",
            "Checkpoint all graphs",
            "Create durable checkpoints for every loaded graph and compact covered WAL history.",
            empty_schema(),
            RequiredScope::Admin,
            false,
            false,
            true,
        ),
        tool_definition(
            "flush_all_graphs",
            "Delete all graphs",
            "Permanently delete every graph in this server. This is equivalent to FLUSHALL for graph data.",
            empty_schema(),
            RequiredScope::Admin,
            false,
            true,
            true,
        ),
        tool_definition(
            "export_graph_dump",
            "Export graph dump",
            "Export one graph as a base64-encoded Redis/FalkorDB DUMP payload suitable for lossless restore. Large graphs can produce large tool results.",
            object_schema(
                json!({"graph": {"type": "string", "minLength": 1}}),
                &["graph"],
            ),
            RequiredScope::Read,
            true,
            false,
            true,
        ),
        tool_definition(
            "import_falkordb_rdb_file",
            "Import FalkorDB RDB file",
            "Parse, verify, and restore a complete native FalkorDB Redis dump.rdb from the server import directory without sending the database through MCP. The import is non-destructive and refuses to overwrite an existing graph. Supply sha256 and expected_graph to bind the import to an exact authoritative database. Set dry_run=true to validate without restoring.",
            object_schema(
                json!({
                    "file": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Relative path below FALKORDB_IMPORT_DIR, or <data-dir>/imports when unset."
                    },
                    "expected_graph": {"type": "string", "minLength": 1},
                    "sha256": {
                        "type": "string",
                        "minLength": 64,
                        "maxLength": 64,
                        "pattern": "^[0-9A-Fa-f]{64}$"
                    },
                    "dry_run": {"type": "boolean", "default": false}
                }),
                &["file"],
            ),
            RequiredScope::Admin,
            false,
            false,
            false,
        ),
        tool_definition(
            "bulk_import_file",
            "Bulk import server file",
            "High-throughput schema-agnostic import from a file already present in the server import directory. Version 1 supports format=jsonl: every non-blank line must be a JSON object. The supplied Cypher template must contain exactly one {{ROWS}} placeholder, which is replaced locally with batches of JSON records converted to Cypher maps. This supports arbitrary MATCH/MERGE/CREATE node and relationship schemas without sending the corpus through MCP. Use dry_run=true for full parse/hash validation without graph writes; start_record supports resumable imports.",
            object_schema(
                json!({
                    "graph": {"type": "string", "minLength": 1},
                    "file": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Relative path below FALKORDB_IMPORT_DIR, or <data-dir>/imports when unset."
                    },
                    "format": {"type": "string", "enum": ["jsonl"], "default": "jsonl"},
                    "cypher": {
                        "type": "string",
                        "minLength": 1,
                        "description": "Batch Cypher template containing exactly one {{ROWS}} placeholder."
                    },
                    "sha256": {
                        "type": "string",
                        "minLength": 64,
                        "maxLength": 64,
                        "pattern": "^[0-9A-Fa-f]{64}$"
                    },
                    "batch_size": {"type": "integer", "minimum": 1, "maximum": 10000, "default": 1000},
                    "max_batch_bytes": {"type": "integer", "minimum": 1, "maximum": 67108864, "default": 8388608},
                    "start_record": {"type": "integer", "minimum": 1, "default": 1},
                    "max_records": {"type": "integer", "minimum": 1},
                    "dry_run": {"type": "boolean", "default": false},
                    "create_graph": {"type": "boolean", "default": false},
                    "checkpoint": {"type": "boolean", "default": true}
                }),
                &["graph", "file", "format", "cypher"],
            ),
            RequiredScope::Admin,
            false,
            false,
            false,
        ),
        tool_definition(
            "restore_graph_dump",
            "Restore graph dump",
            "Restore a base64-encoded Redis/FalkorDB DUMP payload into a named graph. Set replace=true to replace an existing graph.",
            object_schema(
                json!({
                    "graph": {"type": "string", "minLength": 1},
                    "dump_base64": {"type": "string", "minLength": 1},
                    "replace": {"type": "boolean", "default": false}
                }),
                &["graph", "dump_base64"],
            ),
            RequiredScope::Admin,
            false,
            true,
            false,
        ),
    ];

    if tunnel_mode {
        for tool in &mut tools {
            let noauth = json!([{"type": "noauth"}]);
            if let Some(object) = tool.as_object_mut() {
                object.insert("securitySchemes".to_string(), noauth.clone());
                if let Some(meta) = object.get_mut("_meta").and_then(JsonValue::as_object_mut) {
                    meta.insert("securitySchemes".to_string(), noauth);
                }
            }
        }
    }

    modernize_list(json!({"tools": tools}), modern)
}

fn tool_definition(
    name: &str,
    title: &str,
    description: &str,
    input_schema: JsonValue,
    scope: RequiredScope,
    read_only: bool,
    destructive: bool,
    idempotent: bool,
) -> JsonValue {
    json!({
        "name": name,
        "title": title,
        "description": description,
        "inputSchema": input_schema,
        "annotations": {
            "title": title,
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": false
        },
        "securitySchemes": [
            {
                "type": "oauth2",
                "scopes": [SCOPE_READ, SCOPE_WRITE, SCOPE_ADMIN]
            }
        ],
        "_meta": {
            "requiredScope": scope.as_str()
        }
    })
}

fn empty_schema() -> JsonValue {
    json!({
        "type": "object",
        "properties": {},
        "additionalProperties": false
    })
}

fn object_schema(properties: JsonValue, required: &[&str]) -> JsonValue {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn call_tool(
    params: &JsonValue,
    request: &HttpRequest,
    catalog: &GraphCatalog,
    config: &ApiConfig,
    issuer: &str,
    resource: &str,
    modern: bool,
) -> Result<JsonValue, String> {
    let name = params
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "tools/call requires params.name".to_string())?;
    let args = params
        .get("arguments")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    let required_scope = match name {
        "list_graphs" | "database_stats" | "query_graph_read" | "export_graph_dump" => RequiredScope::Read,
        "create_graph" | "query_graph_write" | "batch_graph_queries" => RequiredScope::Write,
        "copy_graph"
        | "delete_graph"
        | "checkpoint_graph"
        | "checkpoint_all_graphs"
        | "flush_all_graphs"
        | "restore_graph_dump"
        | "import_falkordb_rdb_file"
        | "bulk_import_file" => RequiredScope::Admin,
        _ => return Err(format!("unknown tool {name:?}")),
    };

    let granted = mcp_granted_scopes(request, config, issuer, resource);
    if !granted.is_some_and(|scope| scope.allows(required_scope)) {
        if config.tunnel_mode {
            return Ok(tool_failure(
                "Secure MCP Tunnel backend authentication header is missing or invalid",
                modern,
            ));
        }
        return Ok(auth_required_result(
            issuer,
            resource,
            required_scope,
            modern,
        ));
    }

    let result = match name {
        "list_graphs" => Ok(json!({"graphs": catalog.list()})),
        "database_stats" => {
            let graph_filter = args
                .get("graph")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty());
            let include_memory = args
                .get("include_memory")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);
            let memory_samples = match args.get("memory_samples") {
                None | Some(JsonValue::Null) => 100usize,
                Some(value) => {
                    let value = value
                        .as_u64()
                        .ok_or_else(|| "memory_samples must be a positive integer".to_string())?;
                    let value = usize::try_from(value)
                        .map_err(|_| "memory_samples is too large".to_string())?;
                    if value == 0 || value > 10_000 {
                        return Err("memory_samples must be between 1 and 10000".to_string());
                    }
                    value
                }
            };

            catalog
                .database_stats(graph_filter, include_memory, memory_samples)
                .map(|stats| {
                    let graphs = stats
                        .graphs
                        .into_iter()
                        .map(|graph| {
                            json!({
                                "graph": graph.graph,
                                "nodes": graph.nodes,
                                "relationships": graph.relationships,
                                "graph_version": graph.graph_version,
                                "wal_bytes": graph.wal_bytes,
                                "wal_size": human_bytes(graph.wal_bytes),
                                "checkpoint_bytes": graph.checkpoint_bytes,
                                "checkpoint_size": human_bytes(graph.checkpoint_bytes),
                                "checkpoint_count": graph.checkpoint_count,
                                "latest_checkpoint_sequence": graph.latest_checkpoint_sequence,
                                "latest_checkpoint_bytes": graph.latest_checkpoint_bytes,
                                "latest_checkpoint_size": graph.latest_checkpoint_bytes.map(human_bytes),
                                "persistent_bytes": graph.persistent_bytes,
                                "persistent_size": human_bytes(graph.persistent_bytes),
                                "estimated_memory_bytes": graph.estimated_memory_bytes,
                                "estimated_memory_size": graph.estimated_memory_bytes.map(human_bytes)
                            })
                        })
                        .collect::<Vec<_>>();

                    json!({
                        "total_storage_bytes": stats.total_storage_bytes,
                        "total_storage_size": human_bytes(stats.total_storage_bytes),
                        "graphs_storage_bytes": stats.graphs_storage_bytes,
                        "graphs_storage_size": human_bytes(stats.graphs_storage_bytes),
                        "attributed_graph_bytes": stats.attributed_graph_bytes,
                        "attributed_graph_size": human_bytes(stats.attributed_graph_bytes),
                        "unattributed_graph_storage_bytes": stats.unattributed_graph_storage_bytes,
                        "unattributed_graph_storage_size": human_bytes(stats.unattributed_graph_storage_bytes),
                        "imports_storage_bytes": stats.imports_storage_bytes,
                        "imports_storage_size": human_bytes(stats.imports_storage_bytes),
                        "imports_inside_data_dir": stats.imports_inside_data_dir,
                        "other_storage_bytes": stats.other_storage_bytes,
                        "other_storage_size": human_bytes(stats.other_storage_bytes),
                        "graph_count": graphs.len(),
                        "memory_samples": include_memory.then_some(memory_samples),
                        "graphs": graphs
                    })
                })
        }
        "create_graph" => {
            let graph_name = required_string(&args, "graph")?;
            let existed = catalog.contains(graph_name);
            catalog
                .get_or_create(graph_name)
                .map(|_| json!({"graph": graph_name, "created": !existed}))
        }
        "query_graph_read" => {
            let graph_name = required_string(&args, "graph")?;
            let cypher = required_string(&args, "cypher")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| "graph does not exist".to_string())?;
            graph
                .query_read_only(cypher)
                .map(|output| query_output_json(graph_name, output))
        }
        "query_graph_write" => {
            let graph_name = required_string(&args, "graph")?;
            let cypher = required_string(&args, "cypher")?;
            let graph = catalog.get_or_create(graph_name)?;
            graph
                .query(cypher)
                .map(|output| query_output_json(graph_name, output))
        }
        "batch_graph_queries" => run_batch(&args, catalog),
        "copy_graph" => {
            let source = required_string(&args, "source")?;
            let destination = required_string(&args, "destination")?;
            catalog
                .copy(source, destination)
                .map(|_| json!({"source": source, "destination": destination, "copied": true}))
        }
        "delete_graph" => {
            let graph_name = required_string(&args, "graph")?;
            catalog
                .delete(graph_name)
                .map(|deleted| json!({"graph": graph_name, "deleted": deleted}))
        }
        "checkpoint_graph" => {
            let graph_name = required_string(&args, "graph")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| "graph does not exist".to_string())?;
            graph.checkpoint().map(|path| {
                json!({
                    "graph": graph_name,
                    "checkpointed": true,
                    "checkpoint_file": path.file_name().and_then(|v| v.to_str()).unwrap_or("")
                })
            })
        }
        "checkpoint_all_graphs" => {
            let mut results = Vec::new();
            for graph_name in catalog.list() {
                let entry = match catalog.get(&graph_name) {
                    Some(graph) => match graph.checkpoint() {
                        Ok(path) => json!({
                            "graph": graph_name,
                            "ok": true,
                            "checkpoint_file": path.file_name().and_then(|v| v.to_str()).unwrap_or("")
                        }),
                        Err(err) => json!({"graph": graph_name, "ok": false, "error": err}),
                    },
                    None => json!({"graph": graph_name, "ok": false, "error": "graph disappeared"}),
                };
                results.push(entry);
            }
            Ok(json!({"results": results}))
        }
        "flush_all_graphs" => catalog
            .flush()
            .map(|deleted| json!({"deleted_graphs": deleted})),
        "export_graph_dump" => {
            let graph_name = required_string(&args, "graph")?;
            catalog.redis_dump(graph_name).map(|dump| {
                json!({
                    "graph": graph_name,
                    "format": "redis-dump-falkordb-v19",
                    "size_bytes": dump.len(),
                    "dump_base64": URL_SAFE_NO_PAD.encode(dump)
                })
            })
        }
        "bulk_import_file" => {
            let graph_name = required_string(&args, "graph")?;
            let file = required_string(&args, "file")?;
            let format = required_string(&args, "format")?;
            if !format.eq_ignore_ascii_case("jsonl") {
                return Err(format!(
                    "bulk_import_file format {format:?} is not supported by this executable; supported formats: jsonl"
                ));
            }
            let cypher = required_string(&args, "cypher")?;
            let sha256 = args
                .get("sha256")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty());

            let to_usize = |name: &str| -> Result<Option<usize>, String> {
                match args.get(name) {
                    None | Some(JsonValue::Null) => Ok(None),
                    Some(value) => {
                        let raw = value
                            .as_u64()
                            .ok_or_else(|| format!("{name} must be a positive integer"))?;
                        usize::try_from(raw)
                            .map(Some)
                            .map_err(|_| format!("{name} is too large for this server"))
                    }
                }
            };

            let batch_size = to_usize("batch_size")?;
            let max_batch_bytes = to_usize("max_batch_bytes")?;
            let start_record = to_usize("start_record")?;
            let max_records = to_usize("max_records")?;
            let dry_run = args
                .get("dry_run")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let create_graph = args
                .get("create_graph")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let checkpoint = args
                .get("checkpoint")
                .and_then(JsonValue::as_bool)
                .unwrap_or(true);

            catalog
                .bulk_import_jsonl_file(
                    graph_name,
                    file,
                    cypher,
                    sha256,
                    batch_size,
                    max_batch_bytes,
                    start_record,
                    max_records,
                    dry_run,
                    create_graph,
                    checkpoint,
                )
                .map(|(import_root, report)| {
                    json!({
                        "graph": graph_name,
                        "format": "jsonl",
                        "import_root": import_root.display().to_string(),
                        "file": file,
                        "size_bytes": report.size_bytes,
                        "sha256": report.sha256,
                        "records_total": report.records_total,
                        "records_selected": report.records_selected,
                        "records_imported": report.records_imported,
                        "physical_lines": report.physical_lines,
                        "start_record": report.start_record,
                        "last_record": report.last_record,
                        "batches": report.batches,
                        "dry_run": report.dry_run,
                        "checkpoint_file": report.checkpoint_file,
                        "stats": {
                            "labels_added": report.totals.labels_added,
                            "labels_removed": report.totals.labels_removed,
                            "nodes_created": report.totals.nodes_created,
                            "relationships_created": report.totals.relationships_created,
                            "nodes_deleted": report.totals.nodes_deleted,
                            "relationships_deleted": report.totals.relationships_deleted,
                            "properties_set": report.totals.properties_set,
                            "properties_removed": report.totals.properties_removed,
                            "indexes_created": report.totals.indexes_created,
                            "indexes_dropped": report.totals.indexes_dropped
                        }
                    })
                })
        }
        "import_falkordb_rdb_file" => {
            let file = required_string(&args, "file")?;
            let expected_graph = args
                .get("expected_graph")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty());
            let sha256 = args
                .get("sha256")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty());
            let dry_run = args
                .get("dry_run")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);

            catalog
                .import_rdb_file(file, expected_graph, sha256, dry_run)
                .map(|report| {
                    json!({
                        "import_root": report.import_root,
                        "file": report.file,
                        "redis_rdb_version": report.redis_rdb_version,
                        "size_bytes": report.size_bytes,
                        "sha256": report.sha256,
                        "dry_run": report.dry_run,
                        "udf_count": report.udf_count,
                        "ignored_aux_keys": report.ignored_aux_keys,
                        "graphs": report.graphs.into_iter().map(|graph| {
                            json!({
                                "graph": graph.graph,
                                "fragments": graph.fragments,
                                "nodes": graph.nodes,
                                "relationships": graph.relationships,
                                "checkpoint_file": graph.checkpoint_file
                            })
                        }).collect::<Vec<_>>()
                    })
                })
        }
        "restore_graph_dump" => {
            let graph_name = required_string(&args, "graph")?;
            let dump_base64 = required_string(&args, "dump_base64")?;
            let replace = args
                .get("replace")
                .and_then(JsonValue::as_bool)
                .unwrap_or(false);
            let dump = URL_SAFE_NO_PAD
                .decode(dump_base64.as_bytes())
                .map_err(|e| format!("invalid dump_base64: {e}"))?;
            catalog
                .redis_restore(graph_name, &dump, replace)
                .map(|_| json!({"graph": graph_name, "restored": true, "replace": replace}))
        }
        _ => unreachable!(),
    };

    Ok(match result {
        Ok(value) => tool_success(value, modern),
        Err(err) => tool_failure(&err, modern),
    })
}

fn run_batch(args: &JsonMap<String, JsonValue>, catalog: &GraphCatalog) -> Result<JsonValue, String> {
    let queries = args
        .get("queries")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "queries must be an array".to_string())?;
    if queries.is_empty() {
        return Err("queries must contain at least one item".to_string());
    }
    if queries.len() > MAX_MCP_BATCH_QUERIES {
        return Err(format!(
            "queries exceeds maximum of {MAX_MCP_BATCH_QUERIES}"
        ));
    }

    let mut results = Vec::with_capacity(queries.len());
    for item in queries {
        let object = item
            .as_object()
            .ok_or_else(|| "each query must be an object".to_string())?;
        let graph_name = required_string(object, "graph")?;
        let cypher = required_string(object, "cypher")?;
        let read_only = object
            .get("read_only")
            .and_then(JsonValue::as_bool)
            .unwrap_or(false);

        let query_result = if read_only {
            match catalog.get(graph_name) {
                Some(graph) => graph.query_read_only(cypher),
                None => Err("graph does not exist".to_string()),
            }
        } else {
            match catalog.get_or_create(graph_name) {
                Ok(graph) => graph.query(cypher),
                Err(err) => Err(err),
            }
        };

        match query_result {
            Ok(output) => results.push(json!({
                "ok": true,
                "result": query_output_json(graph_name, output)
            })),
            Err(err) => results.push(json!({
                "ok": false,
                "graph": graph_name,
                "error": err
            })),
        }
    }
    Ok(json!({"results": results}))
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

fn required_string<'a>(
    args: &'a JsonMap<String, JsonValue>,
    key: &str,
) -> Result<&'a str, String> {
    args.get(key)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{key} must be a non-empty string"))
}

fn mcp_granted_scopes(
    request: &HttpRequest,
    config: &ApiConfig,
    issuer: &str,
    resource: &str,
) -> Option<GrantedScopes> {
    if config.read_write_token.is_none() && config.read_only_token.is_none() {
        return Some(GrantedScopes::full());
    }

    let header = request.headers.get("authorization")?;
    let token = header.strip_prefix("Bearer ")?;

    if config
        .read_write_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(expected, token))
    {
        return Some(GrantedScopes::full());
    }
    if config
        .read_only_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(expected, token))
    {
        return Some(GrantedScopes::read_only());
    }

    let secret = config.read_write_token.as_deref()?;
    let signed = verify_signed_token(secret, token, issuer, resource, "access").ok()?;
    Some(scopes_from_string(&signed.scope))
}

fn auth_required_result(
    issuer: &str,
    resource: &str,
    required: RequiredScope,
    modern: bool,
) -> JsonValue {
    let metadata = format!("{issuer}/.well-known/oauth-protected-resource");
    let challenge = format!(
        "Bearer resource_metadata=\"{metadata}\", scope=\"{}\", error=\"insufficient_scope\", error_description=\"Authorize this MCP client to access this FalkorDB server\"",
        required.as_str()
    );
    let mut result = json!({
        "content": [{
            "type": "text",
            "text": "Authorization required. Authenticate this MCP client with the FalkorDB server to continue."
        }],
        "isError": true,
        "_meta": {
            "mcp/www_authenticate": [challenge],
            "io.modelcontextprotocol/serverInfo": {
                "name": "falkordb-native-windows",
                "version": SERVER_VERSION
            }
        }
    });
    if modern {
        if let Some(obj) = result.as_object_mut() {
            obj.insert("resultType".to_string(), json!("complete"));
        }
    }
    let _ = resource;
    result
}

fn tool_success(value: JsonValue, modern: bool) -> JsonValue {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    let mut result = json!({
        "content": [{"type": "text", "text": text}],
        "structuredContent": value,
        "isError": false
    });
    modernize_result_in_place(&mut result, modern);
    result
}

fn tool_failure(message: &str, modern: bool) -> JsonValue {
    let mut result = json!({
        "content": [{"type": "text", "text": message}],
        "isError": true
    });
    modernize_result_in_place(&mut result, modern);
    result
}

fn modernize_result(mut value: JsonValue, modern: bool) -> JsonValue {
    modernize_result_in_place(&mut value, modern);
    value
}

fn modernize_result_in_place(value: &mut JsonValue, modern: bool) {
    if !modern {
        return;
    }
    let Some(object) = value.as_object_mut() else {
        return;
    };
    object
        .entry("resultType".to_string())
        .or_insert_with(|| json!("complete"));
    let meta = object
        .entry("_meta".to_string())
        .or_insert_with(|| json!({}));
    if let Some(meta) = meta.as_object_mut() {
        meta.entry("io.modelcontextprotocol/serverInfo".to_string())
            .or_insert_with(|| server_meta()["io.modelcontextprotocol/serverInfo"].clone());
    }
}

fn modernize_list(mut value: JsonValue, modern: bool) -> JsonValue {
    if modern {
        if let Some(object) = value.as_object_mut() {
            object.insert("ttlMs".to_string(), json!(60000));
            object.insert("cacheScope".to_string(), json!("private"));
        }
        modernize_result_in_place(&mut value, true);
    }
    value
}

fn server_meta() -> JsonValue {
    json!({
        "io.modelcontextprotocol/serverInfo": {
            "name": "falkordb-native-windows",
            "version": SERVER_VERSION,
            "toolsetVersion": TOOLSET_VERSION
        }
    })
}

fn rpc_result(id: JsonValue, result: JsonValue) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
}

fn rpc_error(id: JsonValue, code: i64, message: &str) -> JsonValue {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
}

fn is_modern_request(request: &HttpRequest, params: &JsonValue) -> bool {
    if request
        .headers
        .get("mcp-protocol-version")
        .is_some_and(|value| value == PROTOCOL_MODERN)
    {
        return true;
    }
    params
        .get("_meta")
        .and_then(|meta| meta.get("io.modelcontextprotocol/protocolVersion"))
        .and_then(JsonValue::as_str)
        == Some(PROTOCOL_MODERN)
}

#[derive(Debug)]
struct SignedToken {
    scope: String,
}

fn sign_token(
    secret: &str,
    token_type: &str,
    issuer: &str,
    audience: &str,
    scope: &str,
    ttl: u64,
) -> Result<String, String> {
    let nonce = random_token("", 16)?;
    let now = now_secs();
    let payload = json!({
        "typ": token_type,
        "iss": issuer,
        "aud": audience,
        "scope": scope,
        "iat": now,
        "exp": now.saturating_add(ttl),
        "nonce": nonce
    });
    let payload_bytes = serde_json::to_vec(&payload)
        .map_err(|e| format!("serialize OAuth token: {e}"))?;
    let encoded = URL_SAFE_NO_PAD.encode(payload_bytes);
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let signature = hmac::sign(&key, encoded.as_bytes());
    Ok(format!(
        "mcp1.{encoded}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_ref())
    ))
}

fn verify_signed_token(
    secret: &str,
    token: &str,
    issuer: &str,
    audience: &str,
    expected_type: &str,
) -> Result<SignedToken, String> {
    let mut parts = token.split('.');
    if parts.next() != Some("mcp1") {
        return Err("invalid token format".to_string());
    }
    let encoded = parts
        .next()
        .ok_or_else(|| "invalid token format".to_string())?;
    let signature = parts
        .next()
        .ok_or_else(|| "invalid token format".to_string())?;
    if parts.next().is_some() {
        return Err("invalid token format".to_string());
    }

    let signature = URL_SAFE_NO_PAD
        .decode(signature.as_bytes())
        .map_err(|_| "invalid token signature encoding".to_string())?;
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, encoded.as_bytes(), &signature)
        .map_err(|_| "invalid token signature".to_string())?;

    let payload = URL_SAFE_NO_PAD
        .decode(encoded.as_bytes())
        .map_err(|_| "invalid token payload encoding".to_string())?;
    let value: JsonValue = serde_json::from_slice(&payload)
        .map_err(|_| "invalid token payload".to_string())?;

    if value.get("typ").and_then(JsonValue::as_str) != Some(expected_type) {
        return Err("wrong token type".to_string());
    }
    if value.get("iss").and_then(JsonValue::as_str) != Some(issuer) {
        return Err("token issuer mismatch".to_string());
    }
    if value.get("aud").and_then(JsonValue::as_str) != Some(audience) {
        return Err("token audience mismatch".to_string());
    }
    let exp = value
        .get("exp")
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| "token missing expiration".to_string())?;
    if exp < now_secs() {
        return Err("token expired".to_string());
    }
    let scope = value
        .get("scope")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "token missing scope".to_string())?
        .to_string();
    validate_requested_scopes(&scope)?;
    Ok(SignedToken { scope })
}

fn scopes_from_string(scope: &str) -> GrantedScopes {
    GrantedScopes {
        read: scope.split_whitespace().any(|item| item == SCOPE_READ),
        write: scope.split_whitespace().any(|item| item == SCOPE_WRITE),
        admin: scope.split_whitespace().any(|item| item == SCOPE_ADMIN),
    }
}

fn pkce_s256(verifier: &str) -> String {
    let digest = digest::digest(&digest::SHA256, verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest.as_ref())
}

fn random_token(prefix: &str, bytes: usize) -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut buffer = vec![0u8; bytes];
    rng.fill(&mut buffer)
        .map_err(|_| "secure random number generation failed".to_string())?;
    Ok(format!("{prefix}{}", URL_SAFE_NO_PAD.encode(buffer)))
}

fn cleanup_expired_codes() {
    let now = now_secs();
    oauth_codes().lock().retain(|_, code| code.expires_at >= now);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn request_base(request: &HttpRequest, config: &ApiConfig) -> Option<String> {
    let host = request.headers.get("host")?.trim();
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
    {
        return None;
    }
    let scheme = if config.tls.is_some() { "https" } else { "http" };
    Some(format!("{scheme}://{host}"))
}

fn target_query_params(target: &str) -> HashMap<String, String> {
    target
        .split_once('?')
        .map_or_else(HashMap::new, |(_, query)| parse_urlencoded(query))
}

fn parse_urlencoded(input: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in input.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if let (Some(key), Some(value)) = (percent_decode(key), percent_decode(value)) {
            out.insert(key, value);
        }
    }
    out
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = hex_value(bytes[i + 1])?;
                let lo = hex_value(bytes[i + 2])?;
                out.push((hi << 4) | lo);
                i += 3;
            }
            b'%' => return None,
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(input: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push(HEX[(byte >> 4) as usize] as char);
            out.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    out
}

fn html_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn required_param<'a>(
    params: &'a HashMap<String, String>,
    name: &str,
) -> Result<&'a str, String> {
    params
        .get(name)
        .map(String::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("{name} is required"))
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn method_not_allowed(allow: &str) -> HttpResponse {
    let mut response = json_response(
        405,
        json!({"error": {"code": "method_not_allowed", "message": "method not allowed"}}),
    );
    response
        .headers
        .push(("Allow".to_string(), allow.to_string()));
    response
}

fn oauth_error(status: u16, code: &str, description: &str) -> HttpResponse {
    json_response(
        status,
        json!({
            "error": code,
            "error_description": description
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_example() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            pkce_s256(verifier),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn signed_oauth_tokens_are_scoped_and_tamper_evident() {
        let token = sign_token(
            "super-secret",
            "access",
            "https://db.example.test",
            "https://db.example.test/mcp",
            ALL_SCOPES,
            60,
        )
        .unwrap();

        let decoded = verify_signed_token(
            "super-secret",
            &token,
            "https://db.example.test",
            "https://db.example.test/mcp",
            "access",
        )
        .unwrap();
        assert_eq!(decoded.scope, ALL_SCOPES);

        let mut tampered = token.into_bytes();
        let last = tampered.len() - 1;
        tampered[last] = if tampered[last] == b'A' { b'B' } else { b'A' };
        let tampered = String::from_utf8(tampered).unwrap();
        assert!(
            verify_signed_token(
                "super-secret",
                &tampered,
                "https://db.example.test",
                "https://db.example.test/mcp",
                "access"
            )
            .is_err()
        );
    }

    #[test]
    fn mcp_oauth_clients_are_narrowly_allowlisted() {
        let chatgpt = "https://chatgpt.com/oauth/client.json";
        assert!(allowed_mcp_client_id(chatgpt));
        assert!(allowed_mcp_client_id(
            "https://chatgpt.com/oauth/callback123/client.json"
        ));
        assert!(allowed_mcp_client_id(CLAUDE_CODE_CLIENT_ID));
        assert!(!allowed_mcp_client_id("https://evil.example/client.json"));

        assert!(allowed_mcp_redirect(
            chatgpt,
            "https://chatgpt.com/connector_platform_oauth_redirect"
        ));
        assert!(allowed_mcp_redirect(
            chatgpt,
            "https://chatgpt.com/connector/oauth/callback123"
        ));
        assert!(!allowed_mcp_redirect(chatgpt, "https://evil.example/callback"));

        assert!(allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "http://localhost/callback"
        ));
        assert!(allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "http://localhost:54321/callback"
        ));
        assert!(allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "http://127.0.0.1:3118/callback"
        ));
        assert!(allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "http://[::1]:49152/callback"
        ));
        assert!(!allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "http://localhost.evil.example/callback"
        ));
        assert!(!allowed_mcp_redirect(
            CLAUDE_CODE_CLIENT_ID,
            "https://evil.example/callback"
        ));
    }

    #[test]
    fn owner_secret_accepts_password_token_and_secret_file_lines() {
        let config = ApiConfig {
            oauth_owner_secret: Some("server-password".to_string()),
            read_write_token: Some("api-token-value".to_string()),
            ..ApiConfig::default()
        };

        assert!(owner_secret_matches(&config, "server-password"));
        assert!(owner_secret_matches(&config, "api-token-value"));
        assert!(owner_secret_matches(
            &config,
            "FALKORDB_API_TOKEN=api-token-value"
        ));
        assert!(owner_secret_matches(
            &config,
            "FALKORDB_PASSWORD=\"server-password\""
        ));
        assert!(owner_secret_matches(
            &config,
            "$env:FALKORDB_API_TOKEN='api-token-value'"
        ));
        assert!(!owner_secret_matches(&config, "wrong-secret"));
    }

    #[test]
    fn owner_secret_accepts_secrets_file_values_even_when_runtime_values_differ() {
        let config = ApiConfig {
            oauth_owner_secret: Some("runtime-password".to_string()),
            read_write_token: Some("runtime-api-token".to_string()),
            oauth_owner_secret_fallbacks: vec![
                "file-password".to_string(),
                "file-api-token".to_string(),
            ],
            ..ApiConfig::default()
        };

        assert!(owner_secret_matches(&config, "file-password"));
        assert!(owner_secret_matches(
            &config,
            "FALKORDB_API_TOKEN=file-api-token"
        ));
        assert!(owner_secret_matches(
            &config,
            "FALKORDB_PASSWORD=file-password\nFALKORDB_API_TOKEN=file-api-token\n"
        ));
        assert!(!owner_secret_matches(&config, "not-in-any-source"));
    }

    #[test]
    fn oauth_pairing_code_is_independent_owner_approval() {
        let config = ApiConfig {
            oauth_owner_secret: Some("wrong-runtime-password".to_string()),
            read_write_token: Some("wrong-runtime-api-token".to_string()),
            oauth_owner_secret_fallbacks: vec!["wrong-file-value".to_string()],
            oauth_pairing_code: Some("restart-scoped-pairing-code".to_string()),
            ..ApiConfig::default()
        };

        assert!(owner_secret_matches(
            &config,
            "restart-scoped-pairing-code"
        ));
        assert!(!owner_secret_matches(&config, "different-code"));
    }

    #[test]
    fn mcp_options_preflight_exposes_cors_headers() {
        let response = with_mcp_cors(mcp_options_response());
        assert_eq!(response.status, 204);
        assert!(response.headers.iter().any(|(k, v)| {
            k == "Allow" && v == "POST, OPTIONS"
        }));
        assert!(response.headers.iter().any(|(k, v)| {
            k == "Access-Control-Allow-Methods" && v.contains("OPTIONS")
        }));
        assert!(response.headers.iter().any(|(k, v)| {
            k == "Access-Control-Allow-Headers" && v.contains("authorization")
        }));
    }

    #[test]
    fn tool_list_exposes_full_read_write_admin_surface() {
        let value = tools_list_result(false, false);
        let names: Vec<&str> = value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool["name"].as_str())
            .collect();
        assert!(names.contains(&"list_graphs"));
        assert!(names.contains(&"database_stats"));
        assert!(names.contains(&"query_graph_read"));
        assert!(names.contains(&"query_graph_write"));
        assert!(names.contains(&"batch_graph_queries"));
        assert!(names.contains(&"delete_graph"));
        assert!(names.contains(&"flush_all_graphs"));
        assert!(names.contains(&"export_graph_dump"));
        assert!(names.contains(&"restore_graph_dump"));
        assert!(names.contains(&"import_falkordb_rdb_file"));
        assert!(names.contains(&"bulk_import_file"));
    }
}
