mod mib;
mod snmp;

use std::sync::Mutex;
use tauri::State;

#[derive(Default)]
struct AppState {
    mib_dirs: Mutex<Vec<String>>,
    last_parse: Mutex<Option<mib::ParseResult>>,
}

#[tauri::command]
fn list_mib_dirs(state: State<AppState>) -> Vec<String> {
    state.mib_dirs.lock().unwrap().clone()
}

#[tauri::command]
fn add_mib_dir(state: State<AppState>, path: String) -> Vec<String> {
    let mut dirs = state.mib_dirs.lock().unwrap();
    if !dirs.contains(&path) {
        dirs.push(path);
    }
    dirs.clone()
}

#[tauri::command]
fn remove_mib_dir(state: State<AppState>, path: String) -> Vec<String> {
    let mut dirs = state.mib_dirs.lock().unwrap();
    dirs.retain(|d| d != &path);
    dirs.clone()
}

#[tauri::command]
fn get_mib_tree(state: State<AppState>) -> mib::ParseResult {
    let dirs = state.mib_dirs.lock().unwrap().clone();
    let result = mib::parse_directories(&dirs);
    *state.last_parse.lock().unwrap() = Some(result.clone());
    result
}

#[tauri::command]
fn fetch(state: State<AppState>, node_id: String, connection: snmp::ConnectionParams) -> Result<snmp::FetchResult, String> {
    let dirs = state.mib_dirs.lock().unwrap().clone();
    let mut cache = state.last_parse.lock().unwrap();
    if cache.is_none() {
        *cache = Some(mib::parse_directories(&dirs));
    }
    let parsed = cache.as_ref().unwrap();

    let symbol = parsed.symbols.get(&node_id).ok_or_else(|| format!("unknown OID node '{node_id}'"))?;
    match symbol.kind {
        mib::NodeKind::Table => {
            let table = parsed.tables.get(&node_id).ok_or_else(|| format!("no table definition found for '{node_id}'"))?;
            snmp::fetch_table(&connection, table)
        }
        mib::NodeKind::Scalar => {
            if !symbol.resolved {
                return Err(format!("'{node_id}' could not be resolved to an absolute OID"));
            }
            snmp::fetch_scalar(&connection, &symbol.oid)
        }
        mib::NodeKind::Group => Err(format!("'{node_id}' is a group, not a fetchable object")),
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![list_mib_dirs, add_mib_dir, remove_mib_dir, get_mib_tree, fetch])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
