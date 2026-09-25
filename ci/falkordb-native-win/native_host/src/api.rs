use std::{
    collections::HashMap,
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    sync::Arc,
    thread,
};

use rustls::{ServerConnection, StreamOwned};
use serde::Deserialize;
use serde_json::{Value as JsonValue, json};
use subtle::ConstantTimeEq;

use crate::{
    OutputStats, QueryOutput,
    server::{GraphCatalog, TlsConfig, load_tls_config},
    wire::WireValue,
};

const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_BATCH_QUERIES: usize = 100;

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub bind: SocketAddr,
    pub read_write_token: Option<String>,
    pub read_only_token: Option<String>,
    pub allow_unauthenticated_remote: bool,
    pub allow_plaintext_remote: bool,
    pub tls: Option<TlsConfig>,
}

impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8443".parse().expect("valid default API socket"),
            read_write_token: None,
            read_only_token: None,
            allow_unauthenticated_remote: false,
            allow_plaintext_remote: false,
            tls: None,
        }
    }
}

impl ApiConfig {
    pub fn validate(&self) -> Result<(), String> {
        let remote = !self.bind.ip().is_loopback();

        if remote && self.tls.is_none() && !self.allow_plaintext_remote {
            return Err(
                "refusing plaintext non-loopback HTTPS API bind; configure TLS or explicitly pass --api-allow-plaintext-remote"
                    .to_string(),
            );
        }

        if remote
            && self.read_write_token.is_none()
            && self.read_only_token.is_none()
            && !self.allow_unauthenticated_remote
        {
            return Err(
                "refusing unauthenticated non-loopback HTTPS API bind; configure --api-token or --api-read-token"
                    .to_string(),
            );
        }

        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AuthScope {
    None,
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Deserialize)]
struct QueryRequest {
    graph: String,
    cypher: String,
    #[serde(default)]
    read_only: bool,
}

#[derive(Debug, Deserialize)]
struct BatchRequest {
    queries: Vec<QueryRequest>,
}

#[derive(Debug, Deserialize)]
struct DeleteGraphRequest {
    graph: String,
}

pub fn serve_api(config: ApiConfig, catalog: Arc<GraphCatalog>) -> Result<(), String> {
    config.validate()?;
    let tls = config
        .tls
        .as_ref()
        .map(load_tls_config)
        .transpose()?
        .map(Arc::new);

    let listener = TcpListener::bind(config.bind)
        .map_err(|e| format!("bind ChatGPT HTTPS API {}: {e}", config.bind))?;

    eprintln!(
        "FalkorDB native ChatGPT API listening on {}{}",
        if tls.is_some() { "https://" } else { "http://" },
        config.bind
    );

    let config = Arc::new(config);
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let catalog = Arc::clone(&catalog);
                let config = Arc::clone(&config);
                let tls = tls.clone();
                thread::spawn(move || {
                    let peer = stream.peer_addr().ok();
                    let result = stream
                        .set_nodelay(true)
                        .map_err(|e| format!("set API TCP_NODELAY: {e}"))
                        .and_then(|_| {
                            if let Some(tls) = tls {
                                let conn = ServerConnection::new(tls)
                                    .map_err(|e| format!("create API TLS connection: {e}"))?;
                                handle_http_connection(
                                    StreamOwned::new(conn, stream),
                                    &catalog,
                                    &config,
                                )
                            } else {
                                handle_http_connection(stream, &catalog, &config)
                            }
                        });

                    if let Err(err) = result {
                        eprintln!("ChatGPT API connection {peer:?} ended with error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("ChatGPT API accept failed: {err}"),
        }
    }

    Ok(())
}

fn handle_http_connection<S: Read + Write>(
    io: S,
    catalog: &GraphCatalog,
    config: &ApiConfig,
) -> Result<(), String> {
    let mut reader = BufReader::new(io);
    let request = read_http_request(&mut reader)?;
    let response = route_http(request, catalog, config);
    write_http_response(reader.get_mut(), response)
        .map_err(|e| format!("write ChatGPT API response: {e}"))?;
    reader
        .get_mut()
        .flush()
        .map_err(|e| format!("flush ChatGPT API response: {e}"))
}

struct HttpRequest {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    body: Vec<u8>,
    content_type: &'static str,
}

fn read_http_request<R: BufRead>(reader: &mut R) -> Result<HttpRequest, String> {
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|e| format!("read HTTP request line: {e}"))?;
    if request_line.is_empty() {
        return Err("HTTP client closed before request line".to_string());
    }

    let mut parts = request_line.trim_end_matches(['\r', '\n']).split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "malformed HTTP request line".to_string())?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| "malformed HTTP request target".to_string())?
        .to_string();
    let version = parts
        .next()
        .ok_or_else(|| "malformed HTTP version".to_string())?;
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") {
        return Err(format!("unsupported HTTP version: {version}"));
    }

    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|e| format!("read HTTP header: {e}"))?;
        if line == "\r\n" || line == "\n" {
            break;
        }
        if line.is_empty() {
            return Err("unexpected EOF inside HTTP headers".to_string());
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("malformed HTTP header".to_string());
        };
        headers.insert(
            name.trim().to_ascii_lowercase(),
            value.trim().to_string(),
        );
    }

    if headers
        .get("transfer-encoding")
        .is_some_and(|v| !v.eq_ignore_ascii_case("identity"))
    {
        return Err("chunked/encoded request bodies are not supported".to_string());
    }

    let content_length = headers
        .get("content-length")
        .map(|v| {
            v.parse::<usize>()
                .map_err(|_| "invalid Content-Length".to_string())
        })
        .transpose()?
        .unwrap_or(0);

    if content_length > MAX_BODY_BYTES {
        return Err(format!("request body exceeds {MAX_BODY_BYTES} bytes"));
    }

    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|e| format!("read HTTP request body: {e}"))?;

    Ok(HttpRequest {
        method,
        target,
        headers,
        body,
    })
}

fn route_http(
    request: HttpRequest,
    catalog: &GraphCatalog,
    config: &ApiConfig,
) -> HttpResponse {
    if request.method == "GET" && request.target == "/healthz" {
        return json_response(200, json!({
            "ok": true,
            "service": "falkordb-native-chatgpt-api",
            "version": 1
        }));
    }

    if request.method == "GET"
        && matches!(
            request.target.as_str(),
            "/openapi.json" | "/.well-known/openapi.json"
        )
    {
        return json_response(200, openapi_document());
    }

    let scope = auth_scope(&request.headers, config);
    if scope == AuthScope::None
        && (config.read_write_token.is_some() || config.read_only_token.is_some())
    {
        return error_response(401, "unauthorized", "valid Bearer token required");
    }

    if request.target == "/mcp" {
        if request.method != "POST" {
            return error_response(405, "method_not_allowed", "MCP uses POST on /mcp");
        }
        return handle_mcp(&request, catalog, scope);
    }

    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/v1/capabilities") => json_response(200, json!({
            "api_version": 1,
            "database": "FalkorDB native standalone",
            "operations": [
                "list_graphs",
                "query",
                "batch_query",
                "delete_graph"
            ],
            "max_batch_queries": MAX_BATCH_QUERIES,
            "max_body_bytes": MAX_BODY_BYTES
        })),
        ("GET", "/v1/graphs") => {
            json_response(200, json!({"graphs": catalog.list()}))
        }
        ("POST", "/v1/query") => {
            let payload: QueryRequest = match parse_json(&request.body) {
                Ok(v) => v,
                Err(response) => return response,
            };
            run_query(payload, catalog, scope)
        }
        ("POST", "/v1/batch") => {
            let payload: BatchRequest = match parse_json(&request.body) {
                Ok(v) => v,
                Err(response) => return response,
            };
            if payload.queries.len() > MAX_BATCH_QUERIES {
                return error_response(
                    400,
                    "batch_too_large",
                    &format!("at most {MAX_BATCH_QUERIES} queries are allowed per batch"),
                );
            }

            let mut results = Vec::with_capacity(payload.queries.len());
            for query in payload.queries {
                let response = run_query(query, catalog, scope);
                let body: JsonValue = serde_json::from_slice(&response.body)
                    .unwrap_or_else(|_| json!({"error": "invalid_internal_json"}));
                results.push(json!({
                    "status": response.status,
                    "body": body
                }));
            }
            json_response(200, json!({"results": results}))
        }
        ("POST", "/v1/graphs/delete") => {
            if scope != AuthScope::ReadWrite {
                return error_response(403, "read_only_token", "write scope required");
            }
            let payload: DeleteGraphRequest = match parse_json(&request.body) {
                Ok(v) => v,
                Err(response) => return response,
            };
            match catalog.delete(&payload.graph) {
                Ok(removed) => json_response(200, json!({
                    "graph": payload.graph,
                    "deleted": removed
                })),
                Err(err) => error_response(409, "graph_delete_failed", &err),
            }
        }
        _ => error_response(404, "not_found", "unknown API route"),
    }
}


fn handle_mcp(
    request: &HttpRequest,
    catalog: &GraphCatalog,
    scope: AuthScope,
) -> HttpResponse {
    let message: JsonValue = match serde_json::from_slice(&request.body) {
        Ok(value) => value,
        Err(err) => {
            return mcp_jsonrpc_error(
                JsonValue::Null,
                -32700,
                "Parse error",
                Some(json!({"detail": err.to_string()})),
                false,
            );
        }
    };

    let id = message.get("id").cloned().unwrap_or(JsonValue::Null);
    let Some(method) = message.get("method").and_then(JsonValue::as_str) else {
        return mcp_jsonrpc_error(
            id,
            -32600,
            "Invalid Request",
            None,
            mcp_is_modern(request, &message),
        );
    };
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    let modern = mcp_is_modern(request, &message);

    // Notifications have no JSON-RPC id and must not receive a JSON-RPC reply.
    if message.get("id").is_none() {
        return match method {
            "notifications/initialized"
            | "notifications/cancelled"
            | "notifications/progress" => empty_response(202),
            _ => empty_response(202),
        };
    }

    match method {
        "server/discover" => {
            let result = json!({
                "supportedVersions": ["2026-07-28", "2025-11-25"],
                "capabilities": {
                    "tools": {"listChanged": false}
                },
                "instructions": "Use list_graphs/read_graph for inspection. Use write_graph only for deliberate graph mutations. Use batch_graph to reduce round trips. delete_graph is destructive.",
                "ttlMs": 0,
                "cacheScope": "private",
                "resultType": "complete",
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": "reversalgraph-native",
                        "title": "ReversalGraph Native",
                        "version": "1.0.0"
                    }
                }
            });
            mcp_jsonrpc_result(id, result)
        }
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(JsonValue::as_str)
                .unwrap_or("2025-11-25");
            let negotiated = match requested {
                "2025-11-25" | "2025-06-18" | "2025-03-26" => requested,
                _ => "2025-11-25",
            };
            mcp_jsonrpc_result(
                id,
                json!({
                    "protocolVersion": negotiated,
                    "capabilities": {
                        "tools": {"listChanged": false}
                    },
                    "serverInfo": {
                        "name": "reversalgraph-native",
                        "title": "ReversalGraph Native",
                        "version": "1.0.0"
                    },
                    "instructions": "Use list_graphs/read_graph for inspection. Use write_graph only for deliberate graph mutations. Use batch_graph to reduce round trips. delete_graph is destructive."
                }),
            )
        }
        "ping" if !modern => mcp_jsonrpc_result(id, json!({})),
        "tools/list" => {
            let mut result = json!({"tools": mcp_tools(scope)});
            if modern {
                if let Some(object) = result.as_object_mut() {
                    object.insert("ttlMs".to_string(), json!(0));
                    object.insert("cacheScope".to_string(), json!("private"));
                    object.insert("resultType".to_string(), json!("complete"));
                }
            }
            mcp_jsonrpc_result(id, result)
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(JsonValue::as_str) else {
                return mcp_jsonrpc_error(
                    id,
                    -32602,
                    "Invalid params: missing tool name",
                    None,
                    modern,
                );
            };
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));

            match mcp_call_tool(name, arguments, catalog, scope) {
                Ok(structured) => {
                    let mut result = json!({
                        "content": [{
                            "type": "text",
                            "text": serde_json::to_string(&structured)
                                .unwrap_or_else(|_| "{\"error\":\"serialization\"}".to_string())
                        }],
                        "structuredContent": structured,
                        "isError": false
                    });
                    if modern {
                        result
                            .as_object_mut()
                            .expect("tool result is object")
                            .insert("resultType".to_string(), json!("complete"));
                    }
                    mcp_jsonrpc_result(id, result)
                }
                Err((code, message, details)) => {
                    let mut result = json!({
                        "content": [{
                            "type": "text",
                            "text": message
                        }],
                        "structuredContent": {
                            "error": {
                                "code": code,
                                "message": message,
                                "details": details
                            }
                        },
                        "isError": true
                    });
                    if modern {
                        result
                            .as_object_mut()
                            .expect("tool error result is object")
                            .insert("resultType".to_string(), json!("complete"));
                    }
                    mcp_jsonrpc_result(id, result)
                }
            }
        }
        _ => mcp_jsonrpc_error(id, -32601, "Method not found", None, modern),
    }
}

fn mcp_is_modern(request: &HttpRequest, message: &JsonValue) -> bool {
    if request
        .headers
        .get("mcp-protocol-version")
        .is_some_and(|value| value == "2026-07-28")
    {
        return true;
    }

    message
        .pointer("/params/_meta/io.modelcontextprotocol~1protocolVersion")
        .and_then(JsonValue::as_str)
        .is_some_and(|value| value == "2026-07-28")
        || message
            .get("method")
            .and_then(JsonValue::as_str)
            .is_some_and(|method| method == "server/discover")
}

fn mcp_tools(scope: AuthScope) -> Vec<JsonValue> {
    let mut tools = vec![
        json!({
            "name": "list_graphs",
            "title": "List graphs",
            "description": "List persistent graph names available in ReversalGraph Native.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            },
            "annotations": {
                "title": "List graphs",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "read_graph",
            "title": "Read graph",
            "description": "Execute read-only Cypher against an existing graph. Use explicit RETURN projections for compact model-friendly results.",
            "inputSchema": {
                "type": "object",
                "required": ["graph", "cypher"],
                "properties": {
                    "graph": {"type": "string", "minLength": 1},
                    "cypher": {"type": "string", "minLength": 1}
                },
                "additionalProperties": false
            },
            "annotations": {
                "title": "Read graph",
                "readOnlyHint": true,
                "destructiveHint": false,
                "idempotentHint": true,
                "openWorldHint": false
            }
        }),
        json!({
            "name": "batch_graph",
            "title": "Batch graph operations",
            "description": if scope == AuthScope::ReadWrite {
                "Execute up to 100 Cypher operations sequentially. Mark individual operations read_only=true when they are reads."
            } else {
                "Execute up to 100 read-only Cypher operations sequentially."
            },
            "inputSchema": {
                "type": "object",
                "required": ["queries"],
                "properties": {
                    "queries": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_BATCH_QUERIES,
                        "items": {
                            "type": "object",
                            "required": ["graph", "cypher"],
                            "properties": {
                                "graph": {"type": "string", "minLength": 1},
                                "cypher": {"type": "string", "minLength": 1},
                                "read_only": {"type": "boolean", "default": false}
                            },
                            "additionalProperties": false
                        }
                    }
                },
                "additionalProperties": false
            },
            "annotations": {
                "title": "Batch graph operations",
                "readOnlyHint": scope != AuthScope::ReadWrite,
                "destructiveHint": scope == AuthScope::ReadWrite,
                "idempotentHint": scope != AuthScope::ReadWrite,
                "openWorldHint": false
            }
        }),
    ];

    if scope == AuthScope::ReadWrite {
        tools.push(json!({
            "name": "write_graph",
            "title": "Write graph",
            "description": "Execute mutating Cypher against a graph. The graph is created automatically if it does not exist.",
            "inputSchema": {
                "type": "object",
                "required": ["graph", "cypher"],
                "properties": {
                    "graph": {"type": "string", "minLength": 1},
                    "cypher": {"type": "string", "minLength": 1}
                },
                "additionalProperties": false
            },
            "annotations": {
                "title": "Write graph",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }));
        tools.push(json!({
            "name": "delete_graph",
            "title": "Delete graph",
            "description": "Delete a graph and its persistent WAL. This is destructive.",
            "inputSchema": {
                "type": "object",
                "required": ["graph"],
                "properties": {
                    "graph": {"type": "string", "minLength": 1}
                },
                "additionalProperties": false
            },
            "annotations": {
                "title": "Delete graph",
                "readOnlyHint": false,
                "destructiveHint": true,
                "idempotentHint": false,
                "openWorldHint": false
            }
        }));
    }

    tools
}

fn mcp_call_tool(
    name: &str,
    arguments: JsonValue,
    catalog: &GraphCatalog,
    scope: AuthScope,
) -> Result<JsonValue, (String, String, JsonValue)> {
    match name {
        "list_graphs" => Ok(json!({"graphs": catalog.list()})),
        "read_graph" => {
            let payload: QueryRequest = serde_json::from_value(json!({
                "graph": arguments.get("graph").cloned().unwrap_or(JsonValue::Null),
                "cypher": arguments.get("cypher").cloned().unwrap_or(JsonValue::Null),
                "read_only": true
            }))
            .map_err(|err| mcp_tool_error("invalid_arguments", err.to_string(), json!({})))?;
            mcp_query_value(payload, catalog, AuthScope::ReadOnly)
        }
        "write_graph" => {
            if scope != AuthScope::ReadWrite {
                return Err(mcp_tool_error(
                    "read_only_token",
                    "write scope required".to_string(),
                    json!({}),
                ));
            }
            let payload: QueryRequest = serde_json::from_value(json!({
                "graph": arguments.get("graph").cloned().unwrap_or(JsonValue::Null),
                "cypher": arguments.get("cypher").cloned().unwrap_or(JsonValue::Null),
                "read_only": false
            }))
            .map_err(|err| mcp_tool_error("invalid_arguments", err.to_string(), json!({})))?;
            mcp_query_value(payload, catalog, scope)
        }
        "batch_graph" => {
            let Some(items) = arguments.get("queries").and_then(JsonValue::as_array) else {
                return Err(mcp_tool_error(
                    "invalid_arguments",
                    "queries must be an array".to_string(),
                    json!({}),
                ));
            };
            if items.is_empty() || items.len() > MAX_BATCH_QUERIES {
                return Err(mcp_tool_error(
                    "invalid_arguments",
                    format!("queries must contain 1..={MAX_BATCH_QUERIES} entries"),
                    json!({}),
                ));
            }

            let mut results = Vec::with_capacity(items.len());
            for item in items {
                let mut payload: QueryRequest = serde_json::from_value(item.clone())
                    .map_err(|err| {
                        mcp_tool_error("invalid_arguments", err.to_string(), json!({}))
                    })?;
                if scope != AuthScope::ReadWrite {
                    payload.read_only = true;
                }
                match mcp_query_value(payload, catalog, scope) {
                    Ok(value) => results.push(json!({"ok": true, "result": value})),
                    Err((code, message, details)) => results.push(json!({
                        "ok": false,
                        "error": {
                            "code": code,
                            "message": message,
                            "details": details
                        }
                    })),
                }
            }
            Ok(json!({"results": results}))
        }
        "delete_graph" => {
            if scope != AuthScope::ReadWrite {
                return Err(mcp_tool_error(
                    "read_only_token",
                    "write scope required".to_string(),
                    json!({}),
                ));
            }
            let graph = arguments
                .get("graph")
                .and_then(JsonValue::as_str)
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| {
                    mcp_tool_error(
                        "invalid_arguments",
                        "graph must be a non-empty string".to_string(),
                        json!({}),
                    )
                })?;
            catalog
                .delete(graph)
                .map(|deleted| json!({"graph": graph, "deleted": deleted}))
                .map_err(|err| {
                    mcp_tool_error("graph_delete_failed", err, json!({"graph": graph}))
                })
        }
        _ => Err(mcp_tool_error(
            "unknown_tool",
            format!("unknown tool: {name}"),
            json!({}),
        )),
    }
}

fn mcp_query_value(
    request: QueryRequest,
    catalog: &GraphCatalog,
    scope: AuthScope,
) -> Result<JsonValue, (String, String, JsonValue)> {
    let response = run_query(request, catalog, scope);
    let value: JsonValue = serde_json::from_slice(&response.body).map_err(|err| {
        mcp_tool_error(
            "internal_json_error",
            err.to_string(),
            json!({"http_status": response.status}),
        )
    })?;

    if response.status == 200 {
        Ok(value)
    } else {
        let code = value
            .pointer("/error/code")
            .and_then(JsonValue::as_str)
            .unwrap_or("query_failed")
            .to_string();
        let message = value
            .pointer("/error/message")
            .and_then(JsonValue::as_str)
            .unwrap_or("query failed")
            .to_string();
        Err(mcp_tool_error(
            &code,
            message,
            json!({"http_status": response.status}),
        ))
    }
}

fn mcp_tool_error(
    code: &str,
    message: String,
    details: JsonValue,
) -> (String, String, JsonValue) {
    (code.to_string(), message, details)
}

fn mcp_jsonrpc_result(id: JsonValue, result: JsonValue) -> HttpResponse {
    json_response(
        200,
        json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result
        }),
    )
}

fn mcp_jsonrpc_error(
    id: JsonValue,
    code: i64,
    message: &str,
    data: Option<JsonValue>,
    modern: bool,
) -> HttpResponse {
    let mut error = json!({
        "code": code,
        "message": message
    });
    if let Some(data) = data {
        error
            .as_object_mut()
            .expect("JSON-RPC error is object")
            .insert("data".to_string(), data);
    }
    let mut body = json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error
    });
    if modern {
        body.as_object_mut()
            .expect("JSON-RPC envelope is object")
            .insert(
                "_meta".to_string(),
                json!({"io.modelcontextprotocol/protocolVersion": "2026-07-28"}),
            );
    }
    json_response(200, body)
}

fn empty_response(status: u16) -> HttpResponse {
    HttpResponse {
        status,
        body: Vec::new(),
        content_type: "application/json; charset=utf-8",
    }
}

fn auth_scope(headers: &HashMap<String, String>, config: &ApiConfig) -> AuthScope {
    if config.read_write_token.is_none() && config.read_only_token.is_none() {
        return AuthScope::ReadWrite;
    }

    let Some(header) = headers.get("authorization") else {
        return AuthScope::None;
    };
    let Some(token) = header.strip_prefix("Bearer ") else {
        return AuthScope::None;
    };

    if config
        .read_write_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(expected, token))
    {
        return AuthScope::ReadWrite;
    }
    if config
        .read_only_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(expected, token))
    {
        return AuthScope::ReadOnly;
    }

    AuthScope::None
}

fn constant_time_eq(expected: &str, candidate: &str) -> bool {
    if expected.len() != candidate.len() {
        return false;
    }
    bool::from(expected.as_bytes().ct_eq(candidate.as_bytes()))
}

fn run_query(
    request: QueryRequest,
    catalog: &GraphCatalog,
    scope: AuthScope,
) -> HttpResponse {
    if request.graph.trim().is_empty() {
        return error_response(400, "invalid_graph", "graph name must not be empty");
    }
    if request.cypher.trim().is_empty() {
        return error_response(400, "invalid_query", "Cypher query must not be empty");
    }

    // Authorize the caller's requested operation before graph lookup. A
    // read-only credential must never be able to probe graph existence by
    // submitting a write and observing 404 vs 403 behavior.
    if !request.read_only && scope != AuthScope::ReadWrite {
        return error_response(403, "read_only_token", "write scope required");
    }
    let read_only = request.read_only || scope == AuthScope::ReadOnly;

    let graph = if read_only {
        match catalog.get(&request.graph) {
            Some(graph) => graph,
            None => {
                return error_response(
                    404,
                    "graph_not_found",
                    "read-only query cannot create a missing graph",
                );
            }
        }
    } else {
        match catalog.get_or_create(&request.graph) {
            Ok(graph) => graph,
            Err(err) => return error_response(500, "graph_open_failed", &err),
        }
    };

    let result = if read_only {
        graph.query_read_only(&request.cypher)
    } else {
        graph.query(&request.cypher)
    };

    match result {
        Ok(output) => json_response(200, query_output_json(&request.graph, output)),
        Err(err) => error_response(400, "cypher_error", &err),
    }
}

fn query_output_json(graph: &str, output: QueryOutput) -> JsonValue {
    let rows: Vec<Vec<JsonValue>> = output
        .wire_rows
        .iter()
        .map(|row| row.iter().map(wire_value_json).collect())
        .collect();

    json!({
        "graph": graph,
        "columns": output.columns,
        "rows": rows,
        "stats": stats_json(&output.stats),
        "graph_version": output.graph_version
    })
}

fn stats_json(stats: &OutputStats) -> JsonValue {
    json!({
        "labels_added": stats.labels_added,
        "labels_removed": stats.labels_removed,
        "nodes_created": stats.nodes_created,
        "relationships_created": stats.relationships_created,
        "nodes_deleted": stats.nodes_deleted,
        "relationships_deleted": stats.relationships_deleted,
        "properties_set": stats.properties_set,
        "properties_removed": stats.properties_removed,
        "indexes_created": stats.indexes_created,
        "indexes_dropped": stats.indexes_dropped,
        "execution_time_ms": stats.execution_time_ms,
        "cached": stats.cached
    })
}

fn wire_value_json(value: &WireValue) -> JsonValue {
    match value {
        WireValue::Null => JsonValue::Null,
        WireValue::Bool(v) => json!(v),
        WireValue::Int(v) => json!(v),
        WireValue::Float(v) => json!(v),
        WireValue::String(v) => json!(v),
        WireValue::List(values) => {
            JsonValue::Array(values.iter().map(wire_value_json).collect())
        }
        WireValue::Map(values) => {
            let mut out = serde_json::Map::new();
            for (key, value) in values {
                out.insert(key.clone(), wire_value_json(value));
            }
            JsonValue::Object(out)
        }
        WireValue::Node {
            id,
            labels,
            properties,
        } => json!({
            "type": "node",
            "id": id,
            "label_ids": labels,
            "properties": properties.iter().map(|(attribute_id, value)| {
                json!({
                    "attribute_id": attribute_id,
                    "value": wire_value_json(value)
                })
            }).collect::<Vec<_>>()
        }),
        WireValue::Relationship {
            id,
            type_id,
            src,
            dst,
            properties,
        } => json!({
            "type": "relationship",
            "id": id,
            "relationship_type_id": type_id,
            "source_id": src,
            "destination_id": dst,
            "properties": properties.iter().map(|(attribute_id, value)| {
                json!({
                    "attribute_id": attribute_id,
                    "value": wire_value_json(value)
                })
            }).collect::<Vec<_>>()
        }),
        WireValue::Path {
            nodes,
            relationships,
        } => json!({
            "type": "path",
            "nodes": nodes.iter().map(wire_value_json).collect::<Vec<_>>(),
            "relationships": relationships.iter().map(wire_value_json).collect::<Vec<_>>()
        }),
        WireValue::Point {
            latitude,
            longitude,
        } => json!({
            "type": "point",
            "latitude": latitude,
            "longitude": longitude
        }),
        WireValue::VecF32(values) => json!({
            "type": "vector_f32",
            "values": values
        }),
        WireValue::Datetime(v) => json!({"type": "datetime", "value": v}),
        WireValue::Date(v) => json!({"type": "date", "value": v}),
        WireValue::Time(v) => json!({"type": "time", "value": v}),
        WireValue::Duration(v) => json!({"type": "duration", "value": v}),
    }
}

fn parse_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, HttpResponse> {
    serde_json::from_slice(body)
        .map_err(|err| error_response(400, "invalid_json", &err.to_string()))
}

fn json_response(status: u16, value: JsonValue) -> HttpResponse {
    HttpResponse {
        status,
        body: serde_json::to_vec(&value).expect("JSON value must serialize"),
        content_type: "application/json; charset=utf-8",
    }
}

fn error_response(status: u16, code: &str, message: &str) -> HttpResponse {
    json_response(status, json!({
        "error": {
            "code": code,
            "message": message
        }
    }))
}

fn write_http_response(writer: &mut impl Write, response: HttpResponse) -> std::io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };

    write!(
        writer,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        response.status,
        reason,
        response.content_type,
        response.body.len()
    )?;
    writer.write_all(&response.body)
}

fn openapi_document() -> JsonValue {
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "ReversalGraph Native API",
            "version": "1.0.0",
            "description": "Authenticated HTTPS access to the native FalkorDB-derived reversal graph."
        },
        "components": {
            "securitySchemes": {
                "bearerAuth": {
                    "type": "http",
                    "scheme": "bearer"
                }
            },
            "schemas": {
                "QueryRequest": {
                    "type": "object",
                    "required": ["graph", "cypher"],
                    "properties": {
                        "graph": {"type": "string"},
                        "cypher": {"type": "string"},
                        "read_only": {"type": "boolean", "default": false}
                    }
                }
            }
        },
        "security": [{"bearerAuth": []}],
        "paths": {
            "/healthz": {
                "get": {
                    "operationId": "health",
                    "security": [],
                    "responses": {"200": {"description": "Service health"}}
                }
            },
            "/v1/capabilities": {
                "get": {
                    "operationId": "getCapabilities",
                    "responses": {"200": {"description": "API capabilities"}}
                }
            },
            "/v1/graphs": {
                "get": {
                    "operationId": "listGraphs",
                    "responses": {"200": {"description": "List persistent graphs"}}
                }
            },
            "/v1/query": {
                "post": {
                    "operationId": "queryGraph",
                    "description": "Execute Cypher. Set read_only=true for read-only execution.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {"$ref": "#/components/schemas/QueryRequest"}
                            }
                        }
                    },
                    "responses": {
                        "200": {"description": "Cypher result"},
                        "400": {"description": "Query or JSON error"},
                        "403": {"description": "Write attempted with read-only credential"}
                    }
                }
            },
            "/v1/batch": {
                "post": {
                    "operationId": "batchQueryGraph",
                    "description": "Execute up to 100 Cypher requests sequentially.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["queries"],
                                    "properties": {
                                        "queries": {
                                            "type": "array",
                                            "maxItems": MAX_BATCH_QUERIES,
                                            "items": {"$ref": "#/components/schemas/QueryRequest"}
                                        }
                                    }
                                }
                            }
                        }
                    },
                    "responses": {"200": {"description": "Per-query results"}}
                }
            },
            "/v1/graphs/delete": {
                "post": {
                    "operationId": "deleteGraph",
                    "description": "Delete a graph and its WAL. Requires write scope.",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": {
                                    "type": "object",
                                    "required": ["graph"],
                                    "properties": {"graph": {"type": "string"}}
                                }
                            }
                        }
                    },
                    "responses": {"200": {"description": "Deletion result"}}
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_api_requires_tls_by_default() {
        let config = ApiConfig {
            bind: "0.0.0.0:8443".parse().unwrap(),
            read_write_token: Some("secret".to_string()),
            ..ApiConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn bearer_token_scope_is_constant_time_checked() {
        let config = ApiConfig {
            read_write_token: Some("write-secret".to_string()),
            read_only_token: Some("read-secret".to_string()),
            ..ApiConfig::default()
        };

        let mut headers = HashMap::new();
        headers.insert("authorization".to_string(), "Bearer read-secret".to_string());
        assert!(auth_scope(&headers, &config) == AuthScope::ReadOnly);

        headers.insert("authorization".to_string(), "Bearer write-secret".to_string());
        assert!(auth_scope(&headers, &config) == AuthScope::ReadWrite);
    }
}
