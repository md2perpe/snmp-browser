//! Standalone HTTP backend, so the frontend can be run and tested outside of
//! the Tauri shell (e.g. in a plain browser tab, like VSCode's Simple
//! Browser). It exposes the same commands as `lib.rs`'s Tauri
//! `invoke_handler` over a single JSON endpoint, `POST /api/invoke/:cmd`,
//! with CORS enabled so a Vite dev server on a different port can call it.

use serde_json::{json, Value};
use snmp_mib_client_lib::{mib, settings, snmp};
use std::path::PathBuf;
use std::sync::Mutex;
use tiny_http::{Header, Method, Response, Server};

struct AppState {
    settings_path: PathBuf,
    settings: Mutex<settings::Settings>,
    last_parse: Mutex<Option<mib::ParseResult>>,
}

impl AppState {
    fn save(&self) {
        settings::save(&self.settings_path, &self.settings.lock().unwrap());
    }
}

fn default_settings_path() -> PathBuf {
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".snmp-mib-client").join("settings.json")
}

fn cors_headers() -> Vec<Header> {
    vec![
        Header::from_bytes(&b"Access-Control-Allow-Origin"[..], &b"*"[..]).unwrap(),
        Header::from_bytes(&b"Access-Control-Allow-Methods"[..], &b"GET, POST, OPTIONS"[..]).unwrap(),
        Header::from_bytes(&b"Access-Control-Allow-Headers"[..], &b"Content-Type"[..]).unwrap(),
    ]
}

fn json_response(status: u16, body: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let mut response = Response::from_data(serde_json::to_vec(body).unwrap()).with_status_code(status);
    for h in cors_headers() {
        response = response.with_header(h);
    }
    response.with_header(Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap())
}

/// Mirrors the command dispatch in `lib.rs`'s `invoke_handler!`, minus the
/// Tauri-specific plumbing.
fn handle(state: &AppState, cmd: &str, args: &Value) -> Result<Value, (u16, String)> {
    match cmd {
        "list_mib_dirs" => Ok(json!(state.settings.lock().unwrap().mib_dirs.clone())),

        "add_mib_dir" => {
            let path = args.get("path").and_then(Value::as_str).ok_or((400, "missing 'path'".to_string()))?.to_string();
            let dirs = {
                let mut s = state.settings.lock().unwrap();
                if !s.mib_dirs.contains(&path) {
                    s.mib_dirs.push(path);
                }
                s.mib_dirs.clone()
            };
            state.save();
            Ok(json!(dirs))
        }

        "remove_mib_dir" => {
            let path = args.get("path").and_then(Value::as_str).ok_or((400, "missing 'path'".to_string()))?.to_string();
            let dirs = {
                let mut s = state.settings.lock().unwrap();
                s.mib_dirs.retain(|d| d != &path);
                s.mib_dirs.clone()
            };
            state.save();
            Ok(json!(dirs))
        }

        "list_host_profiles" => Ok(json!(state.settings.lock().unwrap().host_profiles.clone())),

        "get_mib_tree" => {
            let dirs = state.settings.lock().unwrap().mib_dirs.clone();
            let result = mib::parse_directories(&dirs);
            *state.last_parse.lock().unwrap() = Some(result.clone());
            Ok(serde_json::to_value(&result).unwrap())
        }

        "fetch" => {
            let node_id = args.get("nodeId").and_then(Value::as_str).ok_or((400, "missing 'nodeId'".to_string()))?.to_string();
            let connection: snmp::ConnectionParams = serde_json::from_value(
                args.get("connection").cloned().ok_or((400, "missing 'connection'".to_string()))?,
            )
            .map_err(|e| (400, e.to_string()))?;

            let dirs = state.settings.lock().unwrap().mib_dirs.clone();
            let mut cache = state.last_parse.lock().unwrap();
            if cache.is_none() {
                *cache = Some(mib::parse_directories(&dirs));
            }
            let parsed = cache.as_ref().unwrap();

            let symbol = parsed.symbols.get(&node_id).ok_or((404, format!("unknown OID node '{node_id}'")))?;
            let result = match symbol.kind {
                mib::NodeKind::Table => {
                    let table = parsed.tables.get(&node_id).ok_or((404, format!("no table definition found for '{node_id}'")))?;
                    snmp::fetch_table(&connection, table)
                }
                mib::NodeKind::Scalar => {
                    if !symbol.resolved {
                        return Err((400, format!("'{node_id}' could not be resolved to an absolute OID")));
                    }
                    snmp::fetch_scalar(&connection, &symbol.oid)
                }
                mib::NodeKind::Group => Err(format!("'{node_id}' is a group, not a fetchable object")),
            };
            result.map(|r| serde_json::to_value(&r).unwrap()).map_err(|e| (400, e))
        }

        other => Err((404, format!("unknown command '{other}'"))),
    }
}

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8787);
    let settings_path = default_settings_path();
    let settings = settings::load(&settings_path);
    // Seed a fresh settings.json immediately, matching the Tauri app's setup behavior.
    settings::save(&settings_path, &settings);
    let state = AppState { settings_path, settings: Mutex::new(settings), last_parse: Mutex::new(None) };

    let server = Server::http(("127.0.0.1", port)).expect("failed to bind HTTP server");
    println!("SNMP MIB Client standalone backend listening on http://127.0.0.1:{port}");
    println!("Settings file: {}", state.settings_path.display());

    for mut request in server.incoming_requests() {
        if *request.method() == Method::Options {
            let mut response = Response::empty(204);
            for h in cors_headers() {
                response = response.with_header(h);
            }
            let _ = request.respond(response);
            continue;
        }

        let Some(cmd) = request.url().strip_prefix("/api/invoke/") else {
            let _ = request.respond(json_response(404, &json!("not found")));
            continue;
        };
        let cmd = cmd.to_string();

        let mut body = String::new();
        let _ = request.as_reader().read_to_string(&mut body);
        let args: Value = if body.trim().is_empty() { json!({}) } else { serde_json::from_str(&body).unwrap_or(json!({})) };

        match handle(&state, &cmd, &args) {
            Ok(v) => {
                let _ = request.respond(json_response(200, &v));
            }
            Err((status, msg)) => {
                let _ = request.respond(json_response(status, &json!(msg)));
            }
        }
    }
}
