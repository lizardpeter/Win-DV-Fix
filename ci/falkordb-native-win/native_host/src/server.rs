use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Write},
    net::{SocketAddr, TcpListener},
    path::{Path, PathBuf},
    sync::Arc,
    thread,
};

use rustls::{
    RootCertStore, ServerConfig as RustlsServerConfig, ServerConnection, StreamOwned,
    server::WebPkiClientVerifier,
};

use parking_lot::RwLock;

use crate::{NativeGraph, OutputStats, QueryOutput, wire::WireValue};

#[derive(Debug, Clone)]
pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
    /// When present, require a client certificate chaining to this CA.
    pub client_ca_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub bind: SocketAddr,
    pub data_dir: PathBuf,
    pub username: String,
    pub password: Option<String>,
    pub allow_unauthenticated_remote: bool,
    pub allow_plaintext_remote: bool,
    pub tls: Option<TlsConfig>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:6379".parse().expect("valid default socket"),
            data_dir: PathBuf::from("falkordb-native-data"),
            username: "default".to_string(),
            password: None,
            allow_unauthenticated_remote: false,
            allow_plaintext_remote: false,
            tls: None,
        }
    }
}

impl ServerConfig {
    pub fn validate(&self) -> Result<(), String> {
        let remote = !self.bind.ip().is_loopback();

        if remote && self.tls.is_none() && !self.allow_plaintext_remote {
            return Err(
                "refusing plaintext non-loopback bind; configure TLS or explicitly pass                  --allow-plaintext-remote"
                    .to_string(),
            );
        }

        if self.password.is_none() && !self.allow_unauthenticated_remote && remote {
            return Err(
                "refusing unauthenticated non-loopback bind; configure --password or                  --allow-unauthenticated-remote"
                    .to_string(),
            );
        }

        if let Some(tls) = &self.tls {
            if !tls.cert_path.is_file() {
                return Err(format!("TLS certificate not found: {}", tls.cert_path.display()));
            }
            if !tls.key_path.is_file() {
                return Err(format!("TLS private key not found: {}", tls.key_path.display()));
            }
            if let Some(ca) = &tls.client_ca_path
                && !ca.is_file()
            {
                return Err(format!("TLS client CA not found: {}", ca.display()));
            }
        }

        Ok(())
    }
}

pub struct GraphCatalog {
    graph_dir: PathBuf,
    graphs: RwLock<HashMap<String, Arc<NativeGraph>>>,
}

impl GraphCatalog {
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, String> {
        let graph_dir = data_dir.as_ref().join("graphs");
        fs::create_dir_all(&graph_dir)
            .map_err(|e| format!("create graph data directory {}: {e}", graph_dir.display()))?;

        let mut graphs = HashMap::new();
        for entry in fs::read_dir(&graph_dir)
            .map_err(|e| format!("read graph data directory {}: {e}", graph_dir.display()))?
        {
            let entry = entry.map_err(|e| format!("read graph directory entry: {e}"))?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("wal") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let name = decode_graph_name(stem)?;
            let graph = NativeGraph::open_persistent(&name, &path)?;
            graphs.insert(name, Arc::new(graph));
        }

        Ok(Self {
            graph_dir,
            graphs: RwLock::new(graphs),
        })
    }

    fn wal_path(&self, name: &str) -> PathBuf {
        self.graph_dir.join(format!("{}.wal", encode_graph_name(name)))
    }

    pub fn get(&self, name: &str) -> Option<Arc<NativeGraph>> {
        self.graphs.read().get(name).cloned()
    }

    pub fn get_or_create(&self, name: &str) -> Result<Arc<NativeGraph>, String> {
        if let Some(graph) = self.get(name) {
            return Ok(graph);
        }

        let mut graphs = self.graphs.write();
        if let Some(graph) = graphs.get(name) {
            return Ok(Arc::clone(graph));
        }

        let graph = Arc::new(NativeGraph::open_persistent(name, self.wal_path(name))?);
        graphs.insert(name.to_string(), Arc::clone(&graph));
        Ok(graph)
    }

    pub fn list(&self) -> Vec<String> {
        let mut names: Vec<String> = self.graphs.read().keys().cloned().collect();
        names.sort();
        names
    }

    pub fn contains(&self, name: &str) -> bool {
        self.graphs.read().contains_key(name)
    }

    pub fn delete(&self, name: &str) -> Result<bool, String> {
        let graph = self.graphs.write().remove(name);
        let Some(graph) = graph else {
            return Ok(false);
        };

        // Windows will not unlink a WAL while another handle is alive.
        if Arc::strong_count(&graph) != 1 {
            self.graphs.write().insert(name.to_string(), graph);
            return Err("graph is busy; retry GRAPH.DELETE after active queries complete".to_string());
        }
        drop(graph);

        let wal = self.wal_path(name);
        if wal.exists() {
            fs::remove_file(&wal)
                .map_err(|e| format!("delete graph WAL {}: {e}", wal.display()))?;
        }
        Ok(true)
    }

    pub fn flush(&self) -> Result<usize, String> {
        let names = self.list();
        let mut removed = 0;
        for name in names {
            if self.delete(&name)? {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

fn encode_graph_name(name: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(name.len() * 2);
    for b in name.as_bytes() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn decode_graph_name(stem: &str) -> Result<String, String> {
    if stem.len() % 2 != 0 {
        return Err(format!("invalid graph WAL filename: {stem}"));
    }
    let mut bytes = Vec::with_capacity(stem.len() / 2);
    let raw = stem.as_bytes();
    for i in (0..raw.len()).step_by(2) {
        let hi = hex(raw[i]).ok_or_else(|| format!("invalid graph WAL filename: {stem}"))?;
        let lo = hex(raw[i + 1]).ok_or_else(|| format!("invalid graph WAL filename: {stem}"))?;
        bytes.push((hi << 4) | lo);
    }
    String::from_utf8(bytes).map_err(|e| format!("graph WAL name is not UTF-8: {e}"))
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn serve(config: ServerConfig) -> Result<(), String> {
    config.validate()?;
    let tls = config
        .tls
        .as_ref()
        .map(load_tls_config)
        .transpose()?
        .map(Arc::new);
    let catalog = Arc::new(GraphCatalog::open(&config.data_dir)?);
    let listener = TcpListener::bind(config.bind)
        .map_err(|e| format!("bind {}: {e}", config.bind))?;

    eprintln!(
        "FalkorDB native RESP{} server listening on {} (data: {})",
        if tls.is_some() { "/TLS" } else { "" },
        config.bind,
        config.data_dir.display()
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
                    let result = if let Some(tls) = tls {
                        stream
                            .set_nodelay(true)
                            .map_err(|e| format!("set TCP_NODELAY: {e}"))
                            .and_then(|_| {
                                let conn = ServerConnection::new(tls)
                                    .map_err(|e| format!("create TLS server connection: {e}"))?;
                                handle_connection_io(
                                    StreamOwned::new(conn, stream),
                                    &catalog,
                                    &config,
                                    peer,
                                )
                            })
                    } else {
                        stream
                            .set_nodelay(true)
                            .map_err(|e| format!("set TCP_NODELAY: {e}"))
                            .and_then(|_| handle_connection_io(stream, &catalog, &config, peer))
                    };

                    if let Err(err) = result {
                        eprintln!("client connection ended with error: {err}");
                    }
                });
            }
            Err(err) => eprintln!("accept failed: {err}"),
        }
    }
    Ok(())
}

fn load_tls_config(tls: &TlsConfig) -> Result<RustlsServerConfig, String> {
    // Multiple transitive crates can enable more than one rustls provider.
    // Select ring explicitly so TLS startup is deterministic instead of
    // relying on rustls feature auto-detection.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut cert_reader = BufReader::new(
        File::open(&tls.cert_path)
            .map_err(|e| format!("open TLS certificate {}: {e}", tls.cert_path.display()))?,
    );
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("parse TLS certificate {}: {e}", tls.cert_path.display()))?;
    if certs.is_empty() {
        return Err(format!("TLS certificate file contains no certificates: {}", tls.cert_path.display()));
    }

    let mut key_reader = BufReader::new(
        File::open(&tls.key_path)
            .map_err(|e| format!("open TLS private key {}: {e}", tls.key_path.display()))?,
    );
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| format!("parse TLS private key {}: {e}", tls.key_path.display()))?
        .ok_or_else(|| format!("TLS private key file contains no key: {}", tls.key_path.display()))?;

    if let Some(client_ca_path) = &tls.client_ca_path {
        let mut ca_reader = BufReader::new(
            File::open(client_ca_path)
                .map_err(|e| format!("open TLS client CA {}: {e}", client_ca_path.display()))?,
        );
        let mut roots = RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut ca_reader) {
            roots
                .add(cert.map_err(|e| format!("parse TLS client CA {}: {e}", client_ca_path.display()))?)
                .map_err(|e| format!("add TLS client CA {}: {e}", client_ca_path.display()))?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| format!("build TLS client certificate verifier: {e}"))?;
        RustlsServerConfig::builder()
            .with_client_cert_verifier(verifier)
            .with_single_cert(certs, key)
            .map_err(|e| format!("configure TLS certificate/key: {e}"))
    } else {
        RustlsServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .map_err(|e| format!("configure TLS certificate/key: {e}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RespProtocol {
    Resp2,
    Resp3,
}

struct ConnectionState {
    authenticated: bool,
    protocol: RespProtocol,
    client_name: Option<String>,
}

fn handle_connection_io<S: Read + Write>(
    io: S,
    catalog: &GraphCatalog,
    config: &ServerConfig,
    peer: Option<SocketAddr>,
) -> Result<(), String> {
    let mut reader = BufReader::new(io);

    let mut state = ConnectionState {
        authenticated: config.password.is_none(),
        protocol: RespProtocol::Resp2,
        client_name: None,
    };

    loop {
        let command = match read_command(&mut reader)? {
            Some(command) => command,
            None => return Ok(()),
        };

        let response = dispatch(command, catalog, config, &mut state);
        write_resp(reader.get_mut(), &response, state.protocol)
            .map_err(|e| format!("write response to {peer:?}: {e}"))?;
        reader
            .get_mut()
            .flush()
            .map_err(|e| format!("flush response: {e}"))?;
    }
}

fn dispatch(
    args: Vec<Vec<u8>>,
    catalog: &GraphCatalog,
    config: &ServerConfig,
    state: &mut ConnectionState,
) -> Resp {
    if args.is_empty() {
        return Resp::Error("ERR empty command".to_string());
    }

    let command = ascii_upper(&args[0]);

    if command == "AUTH" {
        return handle_auth(&args, config, state);
    }
    if command == "HELLO" {
        return handle_hello(&args, config, state);
    }

    if !state.authenticated {
        return Resp::Error("NOAUTH Authentication required.".to_string());
    }

    match command.as_str() {
        "PING" => {
            if args.len() > 1 {
                Resp::Bulk(args[1].clone())
            } else {
                Resp::Simple("PONG".to_string())
            }
        }
        "ECHO" => require_arity(&args, 2).map_or_else(Resp::Error, |_| Resp::Bulk(args[1].clone())),
        "QUIT" => Resp::Simple("OK".to_string()),
        "SELECT" => {
            if args.get(1).map(|v| v.as_slice()) == Some(b"0") {
                Resp::Simple("OK".to_string())
            } else {
                Resp::Error("ERR only database 0 is supported".to_string())
            }
        }
        "CLIENT" => handle_client(&args, state),
        "INFO" => {
            let body = "# Server\r\nredis_mode:standalone\r\nredis_version:7.2.0\r\nfalkordb_native:1\r\n";
            Resp::Bulk(body.as_bytes().to_vec())
        }
        "COMMAND" => Resp::Array(Vec::new()),
        "GRAPH.LIST" => Resp::Array(catalog.list().into_iter().map(bulk).collect()),
        "GRAPH.QUERY" => handle_graph_query(&args, catalog, false),
        "GRAPH.RO_QUERY" => handle_graph_query(&args, catalog, true),
        "GRAPH.DELETE" => {
            if let Err(err) = require_arity(&args, 2) {
                return Resp::Error(err);
            }
            let name = match utf8(&args[1], "graph name") {
                Ok(v) => v,
                Err(err) => return Resp::Error(err),
            };
            match catalog.delete(name) {
                Ok(true) => Resp::Simple("Graph removed, internal execution time: 0.000000 milliseconds".to_string()),
                Ok(false) => Resp::Error("ERR Invalid graph operation on empty key".to_string()),
                Err(err) => Resp::Error(format!("ERR {err}")),
            }
        }
        "EXISTS" => {
            let count = args
                .iter()
                .skip(1)
                .filter_map(|v| std::str::from_utf8(v).ok())
                .filter(|name| catalog.contains(name))
                .count() as i64;
            Resp::Int(count)
        }
        "DEL" => {
            let mut count = 0i64;
            for name in args.iter().skip(1).filter_map(|v| std::str::from_utf8(v).ok()) {
                match catalog.delete(name) {
                    Ok(true) => count += 1,
                    Ok(false) => {}
                    Err(err) => return Resp::Error(format!("ERR {err}")),
                }
            }
            Resp::Int(count)
        }
        "FLUSHDB" | "FLUSHALL" => match catalog.flush() {
            Ok(_) => Resp::Simple("OK".to_string()),
            Err(err) => Resp::Error(format!("ERR {err}")),
        },
        "TYPE" => {
            if args.len() != 2 {
                Resp::Error("ERR wrong number of arguments for 'type' command".to_string())
            } else if std::str::from_utf8(&args[1]).ok().is_some_and(|n| catalog.contains(n)) {
                Resp::Simple("graphdata".to_string())
            } else {
                Resp::Simple("none".to_string())
            }
        }
        "GRAPH.SLOWLOG" => Resp::Array(Vec::new()),
        "GRAPH.CONFIG" => handle_graph_config(&args),
        _ => Resp::Error(format!("ERR unknown command '{}'", String::from_utf8_lossy(&args[0]))),
    }
}

fn handle_auth(args: &[Vec<u8>], config: &ServerConfig, state: &mut ConnectionState) -> Resp {
    let (username, password) = match args.len() {
        2 => (config.username.as_str(), String::from_utf8_lossy(&args[1]).into_owned()),
        3 => (
            match std::str::from_utf8(&args[1]) {
                Ok(v) => v,
                Err(_) => return Resp::Error("WRONGPASS invalid username-password pair".to_string()),
            },
            String::from_utf8_lossy(&args[2]).into_owned(),
        ),
        _ => return Resp::Error("ERR wrong number of arguments for 'auth' command".to_string()),
    };

    let valid = config.password.as_ref().is_none_or(|expected| {
        username == config.username && password.as_bytes() == expected.as_bytes()
    });

    if valid {
        state.authenticated = true;
        Resp::Simple("OK".to_string())
    } else {
        Resp::Error("WRONGPASS invalid username-password pair or user is disabled.".to_string())
    }
}

fn handle_hello(
    args: &[Vec<u8>],
    config: &ServerConfig,
    state: &mut ConnectionState,
) -> Resp {
    let mut requested = 2u8;
    if let Some(proto) = args.get(1).and_then(|v| std::str::from_utf8(v).ok()) {
        match proto {
            "2" => requested = 2,
            "3" => requested = 3,
            _ => return Resp::Error("NOPROTO unsupported protocol version".to_string()),
        }
    }

    let mut i = 2;
    while i < args.len() {
        let option = ascii_upper(&args[i]);
        match option.as_str() {
            "AUTH" if i + 2 < args.len() => {
                let auth_args = vec![b"AUTH".to_vec(), args[i + 1].clone(), args[i + 2].clone()];
                if matches!(handle_auth(&auth_args, config, state), Resp::Error(_)) {
                    return Resp::Error("WRONGPASS invalid username-password pair or user is disabled.".to_string());
                }
                i += 3;
            }
            "SETNAME" if i + 1 < args.len() => {
                state.client_name = Some(String::from_utf8_lossy(&args[i + 1]).into_owned());
                i += 2;
            }
            _ => return Resp::Error("ERR syntax error in HELLO".to_string()),
        }
    }

    if config.password.is_some() && !state.authenticated {
        return Resp::Error("NOAUTH HELLO must be called with the client already authenticated".to_string());
    }

    state.protocol = if requested == 3 {
        RespProtocol::Resp3
    } else {
        RespProtocol::Resp2
    };

    let entries = vec![
        (bulk("server"), bulk("redis")),
        (bulk("version"), bulk("7.2.0")),
        (bulk("proto"), Resp::Int(i64::from(requested))),
        (bulk("id"), Resp::Int(1)),
        (bulk("mode"), bulk("standalone")),
        (bulk("role"), bulk("master")),
        (bulk("modules"), Resp::Array(Vec::new())),
    ];
    if state.protocol == RespProtocol::Resp3 {
        Resp::Map(entries)
    } else {
        Resp::Array(entries.into_iter().flat_map(|(k, v)| [k, v]).collect())
    }
}

fn handle_client(args: &[Vec<u8>], state: &mut ConnectionState) -> Resp {
    let Some(sub) = args.get(1) else {
        return Resp::Error("ERR wrong number of arguments for 'client' command".to_string());
    };
    match ascii_upper(sub).as_str() {
        "SETINFO" => Resp::Simple("OK".to_string()),
        "SETNAME" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'client setname'".to_string());
            }
            state.client_name = Some(String::from_utf8_lossy(&args[2]).into_owned());
            Resp::Simple("OK".to_string())
        }
        "GETNAME" => state
            .client_name
            .as_ref()
            .map_or(Resp::Null, |name| bulk(name)),
        "ID" => Resp::Int(1),
        _ => Resp::Simple("OK".to_string()),
    }
}

fn handle_graph_config(args: &[Vec<u8>]) -> Resp {
    if args.len() < 2 {
        return Resp::Error("ERR wrong number of arguments for 'graph.config' command".to_string());
    }
    match ascii_upper(&args[1]).as_str() {
        "GET" => {
            if args.len() != 3 {
                return Resp::Error("ERR wrong number of arguments for 'graph.config get'".to_string());
            }
            Resp::Array(vec![Resp::Bulk(args[2].clone()), Resp::Int(0)])
        }
        "SET" => Resp::Simple("OK".to_string()),
        _ => Resp::Error("ERR GRAPH.CONFIG expects GET or SET".to_string()),
    }
}

fn handle_graph_query(args: &[Vec<u8>], catalog: &GraphCatalog, read_only: bool) -> Resp {
    if args.len() < 3 {
        return Resp::Error("ERR wrong number of arguments for graph query".to_string());
    }
    let name = match utf8(&args[1], "graph name") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };
    let query = match utf8(&args[2], "query") {
        Ok(v) => v,
        Err(err) => return Resp::Error(err),
    };

    let graph = if read_only {
        match catalog.get(name) {
            Some(graph) => graph,
            None => return Resp::Error("ERR Invalid graph operation on empty key".to_string()),
        }
    } else {
        match catalog.get_or_create(name) {
            Ok(graph) => graph,
            Err(err) => return Resp::Error(format!("ERR {err}")),
        }
    };

    let result = if read_only {
        graph.query_read_only(query)
    } else {
        graph.query(query)
    };

    match result {
        Ok(output) => compact_query_response(&output),
        Err(err) => Resp::Error(format!("ERR {err}")),
    }
}

fn compact_query_response(output: &QueryOutput) -> Resp {
    let stats = stats_response(&output.stats, output.graph_version);
    if output.columns.is_empty() {
        return Resp::Array(vec![stats]);
    }

    let header = Resp::Array(
        output
            .columns
            .iter()
            .map(|column| Resp::Array(vec![Resp::Int(1), bulk(column)]))
            .collect(),
    );
    let rows = Resp::Array(
        output
            .wire_rows
            .iter()
            .map(|row| Resp::Array(row.iter().map(compact_cell).collect()))
            .collect(),
    );
    Resp::Array(vec![header, rows, stats])
}

fn compact_cell(value: &WireValue) -> Resp {
    let (kind, inner) = match value {
        WireValue::Null => (1, Resp::Null),
        WireValue::String(v) => (2, bulk(v)),
        WireValue::Int(v) => (3, Resp::Int(*v)),
        WireValue::Bool(v) => (4, bulk(if *v { "true" } else { "false" })),
        WireValue::Float(v) => (5, bulk(format_float(*v))),
        WireValue::List(values) => (
            6,
            Resp::Array(values.iter().map(compact_cell).collect()),
        ),
        WireValue::Relationship {
            id,
            type_id,
            src,
            dst,
            properties,
        } => (
            7,
            Resp::Array(vec![
                Resp::Int(*id as i64),
                Resp::Int(*type_id as i64),
                Resp::Int(*src as i64),
                Resp::Int(*dst as i64),
                Resp::Array(
                    properties
                        .iter()
                        .map(|(attr, value)| {
                            let cell = compact_cell(value);
                            let Resp::Array(mut pair) = cell else { unreachable!() };
                            let mut prop = vec![Resp::Int(*attr as i64)];
                            prop.append(&mut pair);
                            Resp::Array(prop)
                        })
                        .collect(),
                ),
            ]),
        ),
        WireValue::Node {
            id,
            labels,
            properties,
        } => (
            8,
            Resp::Array(vec![
                Resp::Int(*id as i64),
                Resp::Array(labels.iter().map(|v| Resp::Int(*v as i64)).collect()),
                Resp::Array(
                    properties
                        .iter()
                        .map(|(attr, value)| {
                            let cell = compact_cell(value);
                            let Resp::Array(mut pair) = cell else { unreachable!() };
                            let mut prop = vec![Resp::Int(*attr as i64)];
                            prop.append(&mut pair);
                            Resp::Array(prop)
                        })
                        .collect(),
                ),
            ]),
        ),
        WireValue::Path {
            nodes,
            relationships,
        } => (
            9,
            Resp::Array(vec![
                Resp::Array(vec![
                    Resp::Int(6),
                    Resp::Array(nodes.iter().map(compact_cell).collect()),
                ]),
                Resp::Array(vec![
                    Resp::Int(6),
                    Resp::Array(relationships.iter().map(compact_cell).collect()),
                ]),
            ]),
        ),
        WireValue::Map(values) => {
            let mut entries = Vec::with_capacity(values.len() * 2);
            for (key, value) in values {
                entries.push(bulk(key));
                entries.push(compact_cell(value));
            }
            (10, Resp::Array(entries))
        }
        WireValue::Point { latitude, longitude } => (
            11,
            Resp::Array(vec![bulk(format_float(*latitude)), bulk(format_float(*longitude))]),
        ),
        WireValue::VecF32(values) => (
            12,
            Resp::Array(
                values
                    .iter()
                    .map(|v| bulk(format_float(f64::from(*v))))
                    .collect(),
            ),
        ),
        WireValue::Datetime(v) => (13, Resp::Int(*v)),
        WireValue::Date(v) => (14, Resp::Int(*v)),
        WireValue::Time(v) => (15, Resp::Int(*v)),
        WireValue::Duration(v) => (16, Resp::Int(*v)),
    };
    Resp::Array(vec![Resp::Int(kind), inner])
}

fn stats_response(stats: &OutputStats, version: u64) -> Resp {
    let mut out = Vec::new();
    if stats.labels_added > 0 {
        out.push(bulk(format!("Labels added: {}", stats.labels_added)));
    }
    if stats.labels_removed > 0 {
        out.push(bulk(format!("Labels removed: {}", stats.labels_removed)));
    }
    if stats.nodes_created > 0 {
        out.push(bulk(format!("Nodes created: {}", stats.nodes_created)));
    }
    if stats.properties_set > 0 {
        out.push(bulk(format!("Properties set: {}", stats.properties_set)));
    }
    if stats.properties_removed > 0 {
        out.push(bulk(format!("Properties removed: {}", stats.properties_removed)));
    }
    if stats.relationships_created > 0 {
        out.push(bulk(format!("Relationships created: {}", stats.relationships_created)));
    }
    if stats.nodes_deleted > 0 {
        out.push(bulk(format!("Nodes deleted: {}", stats.nodes_deleted)));
    }
    if stats.relationships_deleted > 0 {
        out.push(bulk(format!("Relationships deleted: {}", stats.relationships_deleted)));
    }
    if stats.indexes_created > 0 {
        out.push(bulk(format!("Indices created: {}", stats.indexes_created)));
    }
    if stats.indexes_dropped > 0 {
        out.push(bulk(format!("Indices deleted: {}", stats.indexes_dropped)));
    }
    out.push(bulk(format!("Cached execution: {}", i32::from(stats.cached))));
    out.push(bulk(format!(
        "Query internal execution time: {:.6} milliseconds",
        stats.execution_time_ms
    )));
    out.push(bulk(format!("Graph version: {version}")));
    Resp::Array(out)
}

fn format_float(value: f64) -> String {
    // FalkorDB uses %.15g on the wire. Rust's shortest round-trippable decimal
    // is accepted by the official clients and preserves the same numeric value.
    format!("{value}")
}

fn require_arity(args: &[Vec<u8>], expected: usize) -> Result<(), String> {
    if args.len() == expected {
        Ok(())
    } else {
        Err("ERR wrong number of arguments".to_string())
    }
}

fn utf8<'a>(value: &'a [u8], what: &str) -> Result<&'a str, String> {
    std::str::from_utf8(value).map_err(|_| format!("ERR {what} must be UTF-8"))
}

fn ascii_upper(value: &[u8]) -> String {
    String::from_utf8_lossy(value).to_ascii_uppercase()
}

fn bulk(value: impl AsRef<[u8]>) -> Resp {
    Resp::Bulk(value.as_ref().to_vec())
}

#[derive(Debug)]
enum Resp {
    Simple(String),
    Error(String),
    Int(i64),
    Bulk(Vec<u8>),
    Null,
    Array(Vec<Resp>),
    Map(Vec<(Resp, Resp)>),
}

fn write_resp(writer: &mut impl Write, value: &Resp, protocol: RespProtocol) -> std::io::Result<()> {
    match value {
        Resp::Simple(v) => write!(writer, "+{}\r\n", sanitize_line(v)),
        Resp::Error(v) => write!(writer, "-{}\r\n", sanitize_line(v)),
        Resp::Int(v) => write!(writer, ":{v}\r\n"),
        Resp::Bulk(v) => {
            write!(writer, "${}\r\n", v.len())?;
            writer.write_all(v)?;
            writer.write_all(b"\r\n")
        }
        Resp::Null => {
            if protocol == RespProtocol::Resp3 {
                writer.write_all(b"_\r\n")
            } else {
                writer.write_all(b"$-1\r\n")
            }
        }
        Resp::Array(values) => {
            write!(writer, "*{}\r\n", values.len())?;
            for item in values {
                write_resp(writer, item, protocol)?;
            }
            Ok(())
        }
        Resp::Map(entries) => {
            if protocol == RespProtocol::Resp3 {
                write!(writer, "%{}\r\n", entries.len())?;
                for (key, value) in entries {
                    write_resp(writer, key, protocol)?;
                    write_resp(writer, value, protocol)?;
                }
                Ok(())
            } else {
                write!(writer, "*{}\r\n", entries.len() * 2)?;
                for (key, value) in entries {
                    write_resp(writer, key, protocol)?;
                    write_resp(writer, value, protocol)?;
                }
                Ok(())
            }
        }
    }
}

fn sanitize_line(value: &str) -> String {
    value.replace(['\r', '\n'], " ")
}

fn read_command<R: BufRead>(reader: &mut R) -> Result<Option<Vec<Vec<u8>>>, String> {
    let mut first = Vec::new();
    let read = reader
        .read_until(b'\n', &mut first)
        .map_err(|e| format!("read command: {e}"))?;
    if read == 0 {
        return Ok(None);
    }
    trim_crlf(&mut first);

    if first.first() == Some(&b'*') {
        let count = parse_len(&first[1..], "array length")?;
        let mut args = Vec::with_capacity(count);
        for _ in 0..count {
            args.push(read_bulkish(reader)?);
        }
        return Ok(Some(args));
    }

    // Redis inline command compatibility, useful for manual diagnostics.
    let line = std::str::from_utf8(&first)
        .map_err(|_| "inline command is not UTF-8".to_string())?;
    Ok(Some(
        line.split_ascii_whitespace()
            .map(|s| s.as_bytes().to_vec())
            .collect(),
    ))
}

fn read_bulkish<R: BufRead>(reader: &mut R) -> Result<Vec<u8>, String> {
    let mut header = Vec::new();
    reader
        .read_until(b'\n', &mut header)
        .map_err(|e| format!("read RESP item: {e}"))?;
    if header.is_empty() {
        return Err("unexpected EOF inside command".to_string());
    }
    trim_crlf(&mut header);

    match header.first().copied() {
        Some(b'$') => {
            let len = parse_len(&header[1..], "bulk length")?;
            let mut data = vec![0u8; len];
            reader
                .read_exact(&mut data)
                .map_err(|e| format!("read bulk payload: {e}"))?;
            let mut crlf = [0u8; 2];
            reader
                .read_exact(&mut crlf)
                .map_err(|e| format!("read bulk terminator: {e}"))?;
            if crlf != *b"\r\n" {
                return Err("invalid bulk string terminator".to_string());
            }
            Ok(data)
        }
        Some(b'+') => Ok(header[1..].to_vec()),
        Some(b':') => Ok(header[1..].to_vec()),
        _ => Err(format!("unsupported RESP request item: {}", String::from_utf8_lossy(&header))),
    }
}

fn parse_len(bytes: &[u8], what: &str) -> Result<usize, String> {
    let s = std::str::from_utf8(bytes).map_err(|_| format!("invalid {what}"))?;
    s.parse::<usize>().map_err(|_| format!("invalid {what}: {s}"))
}

fn trim_crlf(buf: &mut Vec<u8>) {
    if buf.ends_with(b"\n") {
        buf.pop();
    }
    if buf.ends_with(b"\r") {
        buf.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graph_name_filename_roundtrip() {
        for name in ["main", "T6 / Nuketown", "Δ-graph"] {
            assert_eq!(decode_graph_name(&encode_graph_name(name)).unwrap(), name);
        }
    }

    #[test]
    fn remote_unauthenticated_bind_is_rejected() {
        let config = ServerConfig {
            bind: "0.0.0.0:6379".parse().unwrap(),
            allow_plaintext_remote: true,
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn remote_plaintext_bind_is_rejected() {
        let config = ServerConfig {
            bind: "0.0.0.0:6379".parse().unwrap(),
            password: Some("secret".to_string()),
            ..ServerConfig::default()
        };
        assert!(config.validate().is_err());
    }
}
