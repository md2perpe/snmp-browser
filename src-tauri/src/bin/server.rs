//! Standalone HTTP backend, so the frontend can be run and tested outside of
//! the Tauri shell (e.g. in a plain browser tab, like VSCode's Simple
//! Browser). It exposes the same commands as `lib.rs`'s Tauri
//! `invoke_handler` over a single JSON endpoint, `POST /api/invoke/:cmd`,
//! with CORS enabled so a Vite dev server on a different port can call it.

use serde_json::{json, Value};
use snmp_mib_client_lib::{mib, settings, snmp, trap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

const MAX_REQUEST_BODY_BYTES: u64 = 1024 * 1024;

struct AppState {
    settings_path: PathBuf,
    settings: Mutex<settings::Settings>,
    last_parse: Mutex<Option<mib::ParseResult>>,
    trap_state: trap::TrapState,
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

fn profiles_response(settings: &settings::Settings) -> Value {
    json!({ "profiles": settings.mib_profiles, "activeProfileId": settings.active_mib_profile_id })
}

/// Mirrors the command dispatch in `lib.rs`'s `invoke_handler!`, minus the
/// Tauri-specific plumbing.
fn handle(state: &AppState, cmd: &str, args: &Value) -> Result<Value, (u16, String)> {
    match cmd {
        "list_mib_profiles" => Ok(profiles_response(&state.settings.lock().unwrap())),

        "add_mib_profile" => {
            let name = args.get("name").and_then(Value::as_str).ok_or((400, "missing 'name'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                s.add_mib_profile(name);
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "remove_mib_profile" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                s.remove_mib_profile(&id);
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "rename_mib_profile" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            let name = args.get("name").and_then(Value::as_str).ok_or((400, "missing 'name'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                s.rename_mib_profile(&id, name);
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "set_active_mib_profile" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                if s.mib_profiles.iter().any(|p| p.id == id) {
                    s.active_mib_profile_id = id;
                }
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "add_mib_dir" => {
            let path = args.get("path").and_then(Value::as_str).ok_or((400, "missing 'path'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                if let Some(p) = s.active_profile_mut() {
                    if !p.dirs.contains(&path) {
                        p.dirs.push(path);
                    }
                }
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "remove_mib_dir" => {
            let path = args.get("path").and_then(Value::as_str).ok_or((400, "missing 'path'".to_string()))?.to_string();
            let resp = {
                let mut s = state.settings.lock().unwrap();
                if let Some(p) = s.active_profile_mut() {
                    p.dirs.retain(|d| d != &path);
                }
                profiles_response(&s)
            };
            state.save();
            Ok(resp)
        }

        "list_host_profiles" => Ok(json!(state.settings.lock().unwrap().host_profiles.clone())),

        "get_mib_tree" => {
            let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
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

            let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
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

        "walk_timed" => {
            let oid = args.get("oid").and_then(Value::as_str).ok_or((400, "missing 'oid'".to_string()))?.to_string();
            let connection: snmp::ConnectionParams = serde_json::from_value(
                args.get("connection").cloned().ok_or((400, "missing 'connection'".to_string()))?,
            )
            .map_err(|e| (400, e.to_string()))?;
            snmp::walk_timed(&connection, &oid).map(|r| serde_json::to_value(&r).unwrap()).map_err(|e| (400, e))
        }

        "start_trap_listener" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            let config: trap::TrapListenerConfig = serde_json::from_value(
                args.get("config").cloned().ok_or((400, "missing 'config'".to_string()))?,
            )
            .map_err(|e| (400, e.to_string()))?;

            let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
            let mut cache = state.last_parse.lock().unwrap();
            if cache.is_none() {
                *cache = Some(mib::parse_directories(&dirs));
            }
            let oid_index = Arc::new(mib::build_oid_index(&cache.as_ref().unwrap().tree));
            state.trap_state.start(id, config, oid_index).map(|r| serde_json::to_value(&r).unwrap()).map_err(|e| (400, e))
        }

        "stop_trap_listener" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            state.trap_state.stop(&id);
            Ok(Value::Null)
        }

        "poll_traps" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            let after_seq = args.get("afterSeq").and_then(Value::as_u64).unwrap_or(0);
            Ok(serde_json::to_value(state.trap_state.poll(&id, after_seq)).unwrap())
        }

        "clear_traps" => {
            let id = args.get("id").and_then(Value::as_str).ok_or((400, "missing 'id'".to_string()))?.to_string();
            state.trap_state.clear(&id);
            Ok(Value::Null)
        }

        "local_ips" => Ok(serde_json::to_value(trap::local_ips()).unwrap()),

        other => Err((404, format!("unknown command '{other}'"))),
    }
}

fn main() {
    let port: u16 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(8787);
    let settings_path = default_settings_path();
    let settings = settings::load(&settings_path);
    // Seed a fresh settings.json immediately, matching the Tauri app's setup behavior.
    settings::save(&settings_path, &settings);
    let state = AppState { settings_path, settings: Mutex::new(settings), last_parse: Mutex::new(None), trap_state: trap::TrapState::default() };

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
        let body_too_large = {
            let mut reader = request.as_reader().take(MAX_REQUEST_BODY_BYTES + 1);
            reader.read_to_string(&mut body).is_err() || body.len() as u64 > MAX_REQUEST_BODY_BYTES
        };
        if body_too_large {
            let _ = request.respond(Response::from_string("request body too large").with_status_code(413));
            continue;
        }
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
