pub mod mib;
pub mod settings;
pub mod snmp;

use settings::{HostProfile, Settings};
use std::path::PathBuf;
use std::sync::Mutex;
use tauri::{Manager, State};

struct AppState {
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    last_parse: Mutex<Option<mib::ParseResult>>,
}

impl AppState {
    fn save(&self) {
        settings::save(&self.settings_path, &self.settings.lock().unwrap());
    }
}

#[tauri::command]
fn list_mib_dirs(state: State<AppState>) -> Vec<String> {
    state.settings.lock().unwrap().mib_dirs.clone()
}

#[tauri::command]
fn add_mib_dir(state: State<AppState>, path: String) -> Vec<String> {
    let dirs = {
        let mut settings = state.settings.lock().unwrap();
        if !settings.mib_dirs.contains(&path) {
            settings.mib_dirs.push(path);
        }
        settings.mib_dirs.clone()
    };
    state.save();
    dirs
}

#[tauri::command]
fn remove_mib_dir(state: State<AppState>, path: String) -> Vec<String> {
    let dirs = {
        let mut settings = state.settings.lock().unwrap();
        settings.mib_dirs.retain(|d| d != &path);
        settings.mib_dirs.clone()
    };
    state.save();
    dirs
}

#[tauri::command]
fn list_host_profiles(state: State<AppState>) -> Vec<HostProfile> {
    state.settings.lock().unwrap().host_profiles.clone()
}

#[tauri::command]
fn get_mib_tree(state: State<AppState>) -> mib::ParseResult {
    let dirs = state.settings.lock().unwrap().mib_dirs.clone();
    let result = mib::parse_directories(&dirs);
    *state.last_parse.lock().unwrap() = Some(result.clone());
    result
}

#[tauri::command]
fn fetch(state: State<AppState>, node_id: String, connection: snmp::ConnectionParams) -> Result<snmp::FetchResult, String> {
    let dirs = state.settings.lock().unwrap().mib_dirs.clone();
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
        .setup(|app| {
            let settings_path = app.path().app_config_dir()?.join("settings.json");
            let settings = settings::load(&settings_path);
            // Write it back immediately so a fresh install gets a settings.json
            // on disk right away (seeded defaults), not just on first edit.
            settings::save(&settings_path, &settings);
            app.manage(AppState { settings_path, settings: Mutex::new(settings), last_parse: Mutex::new(None) });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![list_mib_dirs, add_mib_dir, remove_mib_dir, list_host_profiles, get_mib_tree, fetch])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
