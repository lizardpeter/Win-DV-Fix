use std::{
    collections::HashMap,
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::{
    digest::{SHA256, digest},
    rand::{SecureRandom, SystemRandom},
};
use serde::Deserialize;
use serde_json::{Map as JsonMap, Value as JsonValue, json};
use subtle::ConstantTimeEq;
use url::{Url, form_urlencoded};

use crate::{
    api::{HttpResponse, query_output_json},
    server::GraphCatalog,
};

const FULL_SCOPE: &str = "graph.read graph.write graph.admin";
const MAX_OAUTH_CLIENTS: usize = 256;
const MAX_AUTH_CODES: usize = 256;
const AUTH_CODE_TTL_SECS: u64 = 600;
const MAX_BATCH_QUERIES: usize = 100;

#[derive(Debug, Clone)]
struct OAuthClient {
    redirect_uris: Vec<String>,
}

#[derive(Debug, Clone)]
struct AuthorizationCode {
    client_id: String,
    redirect_uri: String,
    code_challenge: String,
    scope: String,
    issued_at: u64,
}

#[derive(Default)]
struct OAuthState {
    clients: Mutex<HashMap<String, OAuthClient>>,
    codes: Mutex<HashMap<String, AuthorizationCode>>,
}

static OAUTH_STATE: OnceLock<OAuthState> = OnceLock::new();

fn oauth_state() -> &'static OAuthState {
    OAUTH_STATE.get_or_init(OAuthState::default)
}

#[derive(Debug, Deserialize)]
struct ClientRegistration {
    redirect_uris: Vec<String>,
    #[serde(default)]
    token_endpoint_auth_method: Option<String>,
}

pub(crate) fn public_auth_route(
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
    tls_enabled: bool,
    admin_secret: Option<&str>,
    read_write_token: Option<&str>,
) -> Option<HttpResponse> {
    let path = target.split_once('?').map(|(p, _)| p).unwrap_or(target);
    let base = public_base_url(headers, tls_enabled);

    match (method, path) {
        ("GET", "/.well-known/oauth-protected-resource")
        | ("GET", "/.well-known/oauth-protected-resource/mcp") => {
            let resource = format!("{base}/mcp");
            Some(json_http(
                200,
                json!({
                    "resource": resource,
                    "authorization_servers": [base],
                    "scopes_supported": ["graph.read", "graph.write", "graph.admin"],
                    "resource_documentation": format!("{base}/mcp")
                }),
            ))
        }
        ("GET", "/.well-known/oauth-authorization-server")
        | ("GET", "/.well-known/openid-configuration") => Some(json_http(
            200,
            json!({
                "issuer": base,
                "authorization_endpoint": format!("{base}/oauth/authorize"),
                "token_endpoint": format!("{base}/oauth/token"),
                "registration_endpoint": format!("{base}/oauth/register"),
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code"],
                "code_challenge_methods_supported": ["S256"],
                "scopes_supported": ["graph.read", "graph.write", "graph.admin"],
                "token_endpoint_auth_methods_supported": ["none"]
            }),
        )),
        ("POST", "/oauth/register") => Some(register_client(body)),
        ("GET", "/oauth/authorize") => Some(authorize_form(
            target,
            admin_secret,
        )),
        ("POST", "/oauth/authorize") => Some(authorize_submit(
            body,
            admin_secret,
        )),
        ("POST", "/oauth/token") => Some(exchange_token(
            body,
            read_write_token,
        )),
        _ => None,
    }
}

pub(crate) fn oauth_unauthorized(
    headers: &HashMap<String, String>,
    tls_enabled: bool,
) -> HttpResponse {
    let base = public_base_url(headers, tls_enabled);
    let mut response = json_http(
        401,
        json!({
            "error": {
                "code": "unauthorized",
                "message": "OAuth Bearer authorization required"
            }
        }),
    );
    response.headers.push((
        "WWW-Authenticate".to_string(),
        format!(
            "Bearer resource_metadata=\"{base}/.well-known/oauth-protected-resource\", scope=\"{FULL_SCOPE}\""
        ),
    ));
    response
}

pub(crate) fn handle_mcp(
    method: &str,
    body: &[u8],
    catalog: &GraphCatalog,
    can_write: bool,
) -> HttpResponse {
    if method == "GET" {
        return plain_http(
            405,
            "MCP Streamable HTTP accepts JSON-RPC requests with POST.",
            "text/plain; charset=utf-8",
        );
    }
    if method != "POST" {
        return plain_http(405, "Method Not Allowed", "text/plain; charset=utf-8");
    }

    let message: JsonValue = match serde_json::from_slice(body) {
        Ok(value) => value,
        Err(err) => {
            return rpc_error_http(
                JsonValue::Null,
                -32700,
                "Parse error",
                Some(json!({"detail": err.to_string()})),
            );
        }
    };

    let Some(object) = message.as_object() else {
        return rpc_error_http(JsonValue::Null, -32600, "Invalid Request", None);
    };

    if object.get("jsonrpc").and_then(JsonValue::as_str) != Some("2.0") {
        return rpc_error_http(
            object.get("id").cloned().unwrap_or(JsonValue::Null),
            -32600,
            "Invalid Request",
            None,
        );
    }

    let id = object.get("id").cloned();
    let method_name = object
        .get("method")
        .and_then(JsonValue::as_str)
        .unwrap_or_default();
    let params = object.get("params").cloned().unwrap_or_else(|| json!({}));

    if id.is_none() {
        return notification_response(method_name);
    }
    let id = id.unwrap_or(JsonValue::Null);

    match method_name {
        "server/discover" => rpc_result_http(
            id,
            json!({
                "resultType": "complete",
                "supportedVersions": [
                    "2025-11-25",
                    "2025-06-18",
                    "2025-03-26"
                ],
                "capabilities": {
                    "tools": {}
                },
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": "houseofkublai-falkordb",
                        "version": "1.0.0"
                    }
                },
                "instructions": server_instructions(),
                "ttlMs": 3600000,
                "cacheScope": "private"
            }),
        ),
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(JsonValue::as_str)
                .unwrap_or("2025-11-25");
            let negotiated = match requested {
                "2025-11-25" | "2025-06-18" | "2025-03-26" | "2024-11-05" => requested,
                _ => "2025-11-25",
            };

            rpc_result_http(
                id,
                json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {
                            "listChanged": false
                        }
                    },
                    "serverInfo": {
                        "name": "houseofkublai-falkordb",
                        "version": "1.0.0"
                    },
                    "instructions": server_instructions()
                }),
            )
        }
        "ping" => rpc_result_http(id, json!({})),
        "tools/list" => rpc_result_http(id, json!({"tools": tool_definitions()})),
        "tools/call" => match call_tool(&params, catalog, can_write) {
            Ok(result) => rpc_result_http(id, tool_success(result)),
            Err(err) => rpc_result_http(id, tool_failure(&err)),
        },
        _ => rpc_error_http(id, -32601, "Method not found", None),
    }
}

fn server_instructions() -> &'static str {
    "Private native FalkorDB administration for the House of Kublai graph server. \
Use read tools for inspection. Write/admin tools have full authority to create, mutate, \
copy, checkpoint, delete, and flush graphs. Reverse-engineering projects belong in the \
ReverseEngineering graph unless the user specifies another graph. Do not use destructive \
tools unless the user's request requires the destructive change."
}

fn tool_definitions() -> Vec<JsonValue> {
    vec![
        tool(
            "list_graphs",
            "List graphs",
            "List every persistent named graph on the server.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            true,
            false,
            true,
        ),
        tool(
            "graph_info",
            "Graph information",
            "Return persistence and schema metadata for one named graph.",
            object_schema(
                &["graph"],
                json!({"graph":{"type":"string","minLength":1}}),
            ),
            true,
            false,
            true,
        ),
        tool(
            "create_graph",
            "Create graph",
            "Create an empty persistent graph if it does not already exist.",
            object_schema(
                &["graph"],
                json!({"graph":{"type":"string","minLength":1}}),
            ),
            false,
            false,
            true,
        ),
        tool(
            "query_graph_read",
            "Read graph with Cypher",
            "Execute arbitrary read-only Cypher against an existing graph. Writes are rejected by the database engine.",
            object_schema(
                &["graph", "cypher"],
                json!({
                    "graph":{"type":"string","minLength":1},
                    "cypher":{"type":"string","minLength":1}
                }),
            ),
            true,
            false,
            true,
        ),
        tool(
            "query_graph_write",
            "Write graph with Cypher",
            "Execute arbitrary Cypher with full write authority. May create, update, delete data, indexes, constraints, or other graph state.",
            object_schema(
                &["graph", "cypher"],
                json!({
                    "graph":{"type":"string","minLength":1},
                    "cypher":{"type":"string","minLength":1}
                }),
            ),
            false,
            true,
            false,
        ),
        tool(
            "batch_queries",
            "Run graph query batch",
            "Execute up to 100 Cypher operations sequentially. Each item selects read-only or read/write execution.",
            object_schema(
                &["queries"],
                json!({
                    "queries":{
                        "type":"array",
                        "minItems":1,
                        "maxItems":MAX_BATCH_QUERIES,
                        "items":{
                            "type":"object",
                            "required":["graph","cypher","read_only"],
                            "additionalProperties":false,
                            "properties":{
                                "graph":{"type":"string","minLength":1},
                                "cypher":{"type":"string","minLength":1},
                                "read_only":{"type":"boolean"}
                            }
                        }
                    }
                }),
            ),
            false,
            true,
            false,
        ),
        tool(
            "explain_query",
            "Explain Cypher query",
            "Return the database execution plan for a Cypher query without running the query.",
            object_schema(
                &["graph", "cypher"],
                json!({
                    "graph":{"type":"string","minLength":1},
                    "cypher":{"type":"string","minLength":1}
                }),
            ),
            true,
            false,
            true,
        ),
        tool(
            "copy_graph",
            "Copy graph",
            "Create a persistent full copy of one graph under a new graph name.",
            object_schema(
                &["source", "destination"],
                json!({
                    "source":{"type":"string","minLength":1},
                    "destination":{"type":"string","minLength":1}
                }),
            ),
            false,
            false,
            false,
        ),
        tool(
            "checkpoint_graph",
            "Checkpoint graph",
            "Create a durable full checkpoint for one graph and compact its WAL.",
            object_schema(
                &["graph"],
                json!({"graph":{"type":"string","minLength":1}}),
            ),
            false,
            false,
            true,
        ),
        tool(
            "checkpoint_all",
            "Checkpoint all graphs",
            "Create durable full checkpoints for every graph and compact their WALs.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            false,
            false,
            true,
        ),
        tool(
            "delete_graph",
            "Delete graph",
            "Permanently delete one named graph and its persistent WAL/checkpoints.",
            object_schema(
                &["graph"],
                json!({"graph":{"type":"string","minLength":1}}),
            ),
            false,
            true,
            false,
        ),
        tool(
            "flush_database",
            "Delete all graphs",
            "Permanently delete every graph from this FalkorDB server.",
            json!({"type":"object","properties":{},"additionalProperties":false}),
            false,
            true,
            false,
        ),
    ]
}

fn tool(
    name: &str,
    title: &str,
    description: &str,
    input_schema: JsonValue,
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
        }
    })
}

fn object_schema(required: &[&str], properties: JsonValue) -> JsonValue {
    json!({
        "type":"object",
        "required":required,
        "properties":properties,
        "additionalProperties":false
    })
}

fn call_tool(
    params: &JsonValue,
    catalog: &GraphCatalog,
    can_write: bool,
) -> Result<JsonValue, String> {
    let name = params
        .get("name")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "tools/call requires string params.name".to_string())?;
    let args = params
        .get("arguments")
        .and_then(JsonValue::as_object)
        .cloned()
        .unwrap_or_default();

    match name {
        "list_graphs" => Ok(json!({"graphs": catalog.list()})),
        "graph_info" => {
            let graph_name = required_string(&args, "graph")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| format!("graph {graph_name:?} does not exist"))?;
            Ok(json!({
                "graph": graph_name,
                "schema_version": graph.schema_version(),
                "wal_bytes": graph.wal_len()?
            }))
        }
        "create_graph" => {
            require_write(can_write)?;
            let graph_name = required_string(&args, "graph")?;
            let existed = catalog.contains(graph_name);
            let graph = catalog.get_or_create(graph_name)?;
            Ok(json!({
                "graph": graph_name,
                "created": !existed,
                "schema_version": graph.schema_version()
            }))
        }
        "query_graph_read" => {
            let graph_name = required_string(&args, "graph")?;
            let cypher = required_string(&args, "cypher")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| format!("graph {graph_name:?} does not exist"))?;
            let output = graph.query_read_only(cypher)?;
            Ok(query_output_json(graph_name, output))
        }
        "query_graph_write" => {
            require_write(can_write)?;
            let graph_name = required_string(&args, "graph")?;
            let cypher = required_string(&args, "cypher")?;
            let graph = catalog.get_or_create(graph_name)?;
            let output = graph.query(cypher)?;
            Ok(query_output_json(graph_name, output))
        }
        "batch_queries" => {
            let queries = args
                .get("queries")
                .and_then(JsonValue::as_array)
                .ok_or_else(|| "queries must be an array".to_string())?;
            if queries.is_empty() {
                return Err("queries must contain at least one item".to_string());
            }
            if queries.len() > MAX_BATCH_QUERIES {
                return Err(format!(
                    "batch exceeds maximum of {MAX_BATCH_QUERIES} queries"
                ));
            }

            let mut results = Vec::with_capacity(queries.len());
            for (index, item) in queries.iter().enumerate() {
                let object = item
                    .as_object()
                    .ok_or_else(|| format!("queries[{index}] must be an object"))?;
                let graph_name = required_string(object, "graph")?;
                let cypher = required_string(object, "cypher")?;
                let read_only = object
                    .get("read_only")
                    .and_then(JsonValue::as_bool)
                    .ok_or_else(|| format!("queries[{index}].read_only must be boolean"))?;

                let result = if read_only {
                    let graph = catalog
                        .get(graph_name)
                        .ok_or_else(|| format!("graph {graph_name:?} does not exist"))?;
                    graph.query_read_only(cypher)
                        .map(|output| query_output_json(graph_name, output))
                } else {
                    require_write(can_write)?;
                    let graph = catalog.get_or_create(graph_name)?;
                    graph.query(cypher)
                        .map(|output| query_output_json(graph_name, output))
                };

                match result {
                    Ok(value) => results.push(json!({
                        "index": index,
                        "ok": true,
                        "result": value
                    })),
                    Err(error) => results.push(json!({
                        "index": index,
                        "ok": false,
                        "error": error
                    })),
                }
            }
            Ok(json!({"results": results}))
        }
        "explain_query" => {
            let graph_name = required_string(&args, "graph")?;
            let cypher = required_string(&args, "cypher")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| format!("graph {graph_name:?} does not exist"))?;
            Ok(json!({
                "graph": graph_name,
                "plan": graph.explain(cypher)?
            }))
        }
        "copy_graph" => {
            require_write(can_write)?;
            let source = required_string(&args, "source")?;
            let destination = required_string(&args, "destination")?;
            catalog.copy(source, destination)?;
            Ok(json!({
                "source": source,
                "destination": destination,
                "copied": true
            }))
        }
        "checkpoint_graph" => {
            require_write(can_write)?;
            let graph_name = required_string(&args, "graph")?;
            let graph = catalog
                .get(graph_name)
                .ok_or_else(|| format!("graph {graph_name:?} does not exist"))?;
            graph.checkpoint()?;
            Ok(json!({"graph": graph_name, "checkpointed": true}))
        }
        "checkpoint_all" => {
            require_write(can_write)?;
            let names = catalog.list();
            let mut results = Vec::with_capacity(names.len());
            for graph_name in names {
                let result = match catalog.get(&graph_name) {
                    Some(graph) => match graph.checkpoint() {
                        Ok(_) => json!({"graph": graph_name, "ok": true}),
                        Err(error) => json!({"graph": graph_name, "ok": false, "error": error}),
                    },
                    None => json!({"graph": graph_name, "ok": false, "error": "graph disappeared"}),
                };
                results.push(result);
            }
            Ok(json!({"results": results}))
        }
        "delete_graph" => {
            require_write(can_write)?;
            let graph_name = required_string(&args, "graph")?;
            let deleted = catalog.delete(graph_name)?;
            Ok(json!({"graph": graph_name, "deleted": deleted}))
        }
        "flush_database" => {
            require_write(can_write)?;
            let deleted = catalog.flush()?;
            Ok(json!({"deleted_graphs": deleted}))
        }
        _ => Err(format!("unknown MCP tool: {name}")),
    }
}

fn required_string<'a>(
    args: &'a JsonMap<String, JsonValue>,
    key: &str,
) -> Result<&'a str, String> {
    let value = args
        .get(key)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| format!("{key} must be a string"))?;
    if value.trim().is_empty() {
        return Err(format!("{key} must not be empty"));
    }
    Ok(value)
}

fn require_write(can_write: bool) -> Result<(), String> {
    if can_write {
        Ok(())
    } else {
        Err("write/admin authorization required".to_string())
    }
}

fn tool_success(value: JsonValue) -> JsonValue {
    let text = serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string());
    json!({
        "content": [{"type":"text","text":text}],
        "structuredContent": value,
        "isError": false
    })
}

fn tool_failure(message: &str) -> JsonValue {
    json!({
        "content": [{"type":"text","text":message}],
        "isError": true
    })
}

fn notification_response(method: &str) -> HttpResponse {
    match method {
        "notifications/initialized" | "notifications/cancelled" => empty_http(202),
        _ => empty_http(202),
    }
}

fn register_client(body: &[u8]) -> HttpResponse {
    let request: ClientRegistration = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(err) => {
            return oauth_error(
                400,
                "invalid_client_metadata",
                &format!("invalid registration JSON: {err}"),
            );
        }
    };

    if request.redirect_uris.is_empty() || request.redirect_uris.len() > 16 {
        return oauth_error(
            400,
            "invalid_redirect_uri",
            "at least one and no more than 16 redirect URIs are required",
        );
    }
    if request
        .token_endpoint_auth_method
        .as_deref()
        .is_some_and(|method| method != "none")
    {
        return oauth_error(
            400,
            "invalid_client_metadata",
            "only token_endpoint_auth_method=none is supported",
        );
    }

    for redirect in &request.redirect_uris {
        if !valid_redirect_uri(redirect) {
            return oauth_error(
                400,
                "invalid_redirect_uri",
                "redirect URIs must use HTTPS, except loopback HTTP is allowed",
            );
        }
    }

    let client_id = match random_token() {
        Ok(token) => token,
        Err(err) => return oauth_error(500, "server_error", &err),
    };

    let state = oauth_state();
    let mut clients = match state.clients.lock() {
        Ok(clients) => clients,
        Err(_) => return oauth_error(500, "server_error", "OAuth client state poisoned"),
    };
    if clients.len() >= MAX_OAUTH_CLIENTS {
        return oauth_error(429, "temporarily_unavailable", "too many registered clients");
    }
    clients.insert(
        client_id.clone(),
        OAuthClient {
            redirect_uris: request.redirect_uris.clone(),
        },
    );

    json_http_status(
        201,
        json!({
            "client_id": client_id,
            "client_id_issued_at": unix_time(),
            "redirect_uris": request.redirect_uris,
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code"],
            "response_types": ["code"]
        }),
    )
}

fn authorize_form(target: &str, admin_secret: Option<&str>) -> HttpResponse {
    if admin_secret.is_none() {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "OAuth approval secret is not configured",
        );
    }

    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params = parse_form(query.as_bytes());
    if let Err(err) = validate_authorization_request(&params) {
        return oauth_error(400, "invalid_request", &err);
    }

    let hidden = [
        "response_type",
        "client_id",
        "redirect_uri",
        "code_challenge",
        "code_challenge_method",
        "state",
        "scope",
        "resource",
    ]
    .iter()
    .filter_map(|key| {
        params.get(*key).map(|value| {
            format!(
                "<input type=\"hidden\" name=\"{}\" value=\"{}\">",
                html_escape(key),
                html_escape(value)
            )
        })
    })
    .collect::<Vec<_>>()
    .join("\n");

    let html = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Authorize FalkorDB</title>\
<style>body{{font-family:system-ui;max-width:620px;margin:4rem auto;padding:0 1rem}}\
input[type=password]{{width:100%;padding:.7rem;margin:.5rem 0 1rem}}\
button{{padding:.7rem 1.2rem}}</style></head><body>\
<h1>Authorize ChatGPT FalkorDB access</h1>\
<p>This grants full read/write/admin access to the FalkorDB graphs on this server. \
It does not grant Windows shell or arbitrary filesystem access.</p>\
<form method=\"post\" action=\"/oauth/authorize\">{hidden}\
<label>FalkorDB server password</label>\
<input type=\"password\" name=\"admin_secret\" autocomplete=\"current-password\" required autofocus>\
<button type=\"submit\">Authorize</button></form></body></html>"
    );
    plain_http(200, &html, "text/html; charset=utf-8")
}

fn authorize_submit(body: &[u8], admin_secret: Option<&str>) -> HttpResponse {
    let Some(expected_secret) = admin_secret else {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "OAuth approval secret is not configured",
        );
    };

    let params = parse_form(body);
    if let Err(err) = validate_authorization_request(&params) {
        return oauth_error(400, "invalid_request", &err);
    }

    let supplied = params.get("admin_secret").map(String::as_str).unwrap_or("");
    if !constant_time_eq(expected_secret, supplied) {
        return plain_http(
            403,
            "<!doctype html><html><body><h1>Authorization denied</h1><p>The FalkorDB password was incorrect.</p></body></html>",
            "text/html; charset=utf-8",
        );
    }

    let code = match random_token() {
        Ok(token) => token,
        Err(err) => return oauth_error(500, "server_error", &err),
    };
    let client_id = params.get("client_id").cloned().unwrap_or_default();
    let redirect_uri = params.get("redirect_uri").cloned().unwrap_or_default();
    let code_challenge = params.get("code_challenge").cloned().unwrap_or_default();

    let state = oauth_state();
    let mut codes = match state.codes.lock() {
        Ok(codes) => codes,
        Err(_) => return oauth_error(500, "server_error", "OAuth code state poisoned"),
    };
    prune_codes(&mut codes);
    if codes.len() >= MAX_AUTH_CODES {
        return oauth_error(429, "temporarily_unavailable", "too many pending authorization codes");
    }
    codes.insert(
        code.clone(),
        AuthorizationCode {
            client_id,
            redirect_uri: redirect_uri.clone(),
            code_challenge,
            scope: FULL_SCOPE.to_string(),
            issued_at: unix_time(),
        },
    );
    drop(codes);

    let mut serializer = form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("code", &code);
    if let Some(state_value) = params.get("state") {
        serializer.append_pair("state", state_value);
    }
    let query = serializer.finish();
    let separator = if redirect_uri.contains('?') { '&' } else { '?' };
    redirect_http(&format!("{redirect_uri}{separator}{query}"))
}

fn exchange_token(body: &[u8], read_write_token: Option<&str>) -> HttpResponse {
    let Some(access_token) = read_write_token else {
        return oauth_error(
            503,
            "temporarily_unavailable",
            "read/write API token is not configured",
        );
    };

    let params = parse_form(body);
    if params.get("grant_type").map(String::as_str) != Some("authorization_code") {
        return oauth_error(
            400,
            "unsupported_grant_type",
            "grant_type must be authorization_code",
        );
    }

    let code = match params.get("code") {
        Some(code) if !code.is_empty() => code,
        _ => return oauth_error(400, "invalid_grant", "authorization code is required"),
    };
    let client_id = params.get("client_id").map(String::as_str).unwrap_or("");
    let redirect_uri = params.get("redirect_uri").map(String::as_str).unwrap_or("");
    let verifier = params
        .get("code_verifier")
        .map(String::as_str)
        .unwrap_or("");

    let state = oauth_state();
    let authorization = {
        let mut codes = match state.codes.lock() {
            Ok(codes) => codes,
            Err(_) => return oauth_error(500, "server_error", "OAuth code state poisoned"),
        };
        prune_codes(&mut codes);
        codes.remove(code)
    };

    let Some(authorization) = authorization else {
        return oauth_error(400, "invalid_grant", "authorization code is invalid or expired");
    };

    if authorization.client_id != client_id
        || authorization.redirect_uri != redirect_uri
        || unix_time().saturating_sub(authorization.issued_at) > AUTH_CODE_TTL_SECS
    {
        return oauth_error(400, "invalid_grant", "authorization code binding mismatch");
    }

    if verifier.len() < 43 || verifier.len() > 128 {
        return oauth_error(400, "invalid_grant", "invalid PKCE code_verifier");
    }
    let actual_challenge = pkce_s256(verifier);
    if !constant_time_eq(&authorization.code_challenge, &actual_challenge) {
        return oauth_error(400, "invalid_grant", "PKCE verification failed");
    }

    json_http(
        200,
        json!({
            "access_token": access_token,
            "token_type": "Bearer",
            "scope": authorization.scope
        }),
    )
}

fn validate_authorization_request(params: &HashMap<String, String>) -> Result<(), String> {
    if params.get("response_type").map(String::as_str) != Some("code") {
        return Err("response_type must be code".to_string());
    }
    if params
        .get("code_challenge_method")
        .map(String::as_str)
        != Some("S256")
    {
        return Err("code_challenge_method must be S256".to_string());
    }
    let challenge = params
        .get("code_challenge")
        .ok_or_else(|| "code_challenge is required".to_string())?;
    if challenge.len() < 43 || challenge.len() > 128 {
        return Err("invalid code_challenge".to_string());
    }

    let client_id = params
        .get("client_id")
        .ok_or_else(|| "client_id is required".to_string())?;
    let redirect_uri = params
        .get("redirect_uri")
        .ok_or_else(|| "redirect_uri is required".to_string())?;

    let state = oauth_state();
    let clients = state
        .clients
        .lock()
        .map_err(|_| "OAuth client state poisoned".to_string())?;
    let client = clients
        .get(client_id)
        .ok_or_else(|| "unknown client_id".to_string())?;
    if !client.redirect_uris.iter().any(|uri| uri == redirect_uri) {
        return Err("redirect_uri was not registered for this client".to_string());
    }

    if let Some(scope) = params.get("scope") {
        for item in scope.split_whitespace() {
            if !matches!(item, "graph.read" | "graph.write" | "graph.admin") {
                return Err(format!("unsupported scope {item:?}"));
            }
        }
    }

    Ok(())
}

fn valid_redirect_uri(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    if url.scheme() == "https" {
        return true;
    }
    if url.scheme() != "http" {
        return false;
    }
    matches!(url.host_str(), Some("127.0.0.1" | "::1" | "localhost"))
}

fn prune_codes(codes: &mut HashMap<String, AuthorizationCode>) {
    let now = unix_time();
    codes.retain(|_, code| now.saturating_sub(code.issued_at) <= AUTH_CODE_TTL_SECS);
}

fn pkce_s256(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(digest(&SHA256, verifier.as_bytes()).as_ref())
}

fn random_token() -> Result<String, String> {
    let rng = SystemRandom::new();
    let mut bytes = [0u8; 32];
    rng.fill(&mut bytes)
        .map_err(|_| "secure random number generation failed".to_string())?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_form(bytes: &[u8]) -> HashMap<String, String> {
    form_urlencoded::parse(bytes)
        .into_owned()
        .collect::<HashMap<_, _>>()
}

fn public_base_url(headers: &HashMap<String, String>, tls_enabled: bool) -> String {
    let host = headers
        .get("host")
        .map(String::as_str)
        .filter(|host| !host.is_empty())
        .unwrap_or("db.houseofkublai.com:18443");
    format!("{}://{}", if tls_enabled { "https" } else { "http" }, host)
}

fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn constant_time_eq(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    bool::from(expected.as_bytes().ct_eq(candidate.as_bytes()))
}

fn rpc_result_http(id: JsonValue, result: JsonValue) -> HttpResponse {
    json_http(
        200,
        json!({
            "jsonrpc":"2.0",
            "id":id,
            "result":result
        }),
    )
}

fn rpc_error_http(
    id: JsonValue,
    code: i64,
    message: &str,
    data: Option<JsonValue>,
) -> HttpResponse {
    let mut error = JsonMap::new();
    error.insert("code".to_string(), json!(code));
    error.insert("message".to_string(), json!(message));
    if let Some(data) = data {
        error.insert("data".to_string(), data);
    }
    json_http(
        200,
        json!({
            "jsonrpc":"2.0",
            "id":id,
            "error":JsonValue::Object(error)
        }),
    )
}

fn oauth_error(status: u16, error: &str, description: &str) -> HttpResponse {
    json_http_status(
        status,
        json!({
            "error": error,
            "error_description": description
        }),
    )
}

fn json_http(status: u16, value: JsonValue) -> HttpResponse {
    json_http_status(status, value)
}

fn json_http_status(status: u16, value: JsonValue) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec()),
        content_type: "application/json; charset=utf-8",
        headers: Vec::new(),
    }
}

fn plain_http(status: u16, text: &str, content_type: &'static str) -> HttpResponse {
    HttpResponse {
        status,
        body: text.as_bytes().to_vec(),
        content_type,
        headers: Vec::new(),
    }
}

fn empty_http(status: u16) -> HttpResponse {
    HttpResponse {
        status,
        body: Vec::new(),
        content_type: "application/json; charset=utf-8",
        headers: Vec::new(),
    }
}

fn redirect_http(location: &str) -> HttpResponse {
    HttpResponse {
        status: 302,
        body: Vec::new(),
        content_type: "text/plain; charset=utf-8",
        headers: vec![("Location".to_string(), location.to_string())],
    }
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
    fn redirect_uri_policy_accepts_https_and_loopback_only() {
        assert!(valid_redirect_uri("https://chatgpt.com/oauth/callback"));
        assert!(valid_redirect_uri("http://127.0.0.1:1234/callback"));
        assert!(valid_redirect_uri("http://localhost:1234/callback"));
        assert!(!valid_redirect_uri("http://example.com/callback"));
        assert!(!valid_redirect_uri("file:///tmp/callback"));
    }
}
