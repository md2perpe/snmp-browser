pub mod mib;
pub mod settings;
pub mod snmp;
pub mod trap;

use serde::Serialize;
use settings::{HostProfile, MibProfile, Settings};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{Manager, State};

struct AppState {
    settings_path: PathBuf,
    settings: Mutex<Settings>,
    last_parse: Mutex<Option<mib::ParseResult>>,
    trap_state: trap::TrapState,
}

impl AppState {
    fn save(&self) {
        settings::save(&self.settings_path, &self.settings.lock().unwrap());
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MibProfilesResponse {
    profiles: Vec<MibProfile>,
    active_profile_id: String,
}

fn profiles_response(settings: &Settings) -> MibProfilesResponse {
    MibProfilesResponse { profiles: settings.mib_profiles.clone(), active_profile_id: settings.active_mib_profile_id.clone() }
}

#[tauri::command]
fn list_mib_profiles(state: State<AppState>) -> MibProfilesResponse {
    profiles_response(&state.settings.lock().unwrap())
}

#[tauri::command]
fn add_mib_profile(state: State<AppState>, name: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        settings.add_mib_profile(name);
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn remove_mib_profile(state: State<AppState>, id: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        settings.remove_mib_profile(&id);
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn rename_mib_profile(state: State<AppState>, id: String, name: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        settings.rename_mib_profile(&id, name);
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn set_active_mib_profile(state: State<AppState>, id: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        if settings.mib_profiles.iter().any(|p| p.id == id) {
            settings.active_mib_profile_id = id;
        }
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn add_mib_dir(state: State<AppState>, path: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        if let Some(p) = settings.active_profile_mut() {
            if !p.dirs.contains(&path) {
                p.dirs.push(path);
            }
        }
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn remove_mib_dir(state: State<AppState>, path: String) -> MibProfilesResponse {
    let resp = {
        let mut settings = state.settings.lock().unwrap();
        if let Some(p) = settings.active_profile_mut() {
            p.dirs.retain(|d| d != &path);
        }
        profiles_response(&settings)
    };
    state.save();
    resp
}

#[tauri::command]
fn list_host_profiles(state: State<AppState>) -> Vec<HostProfile> {
    state.settings.lock().unwrap().host_profiles.clone()
}

#[tauri::command]
fn get_mib_tree(state: State<AppState>) -> mib::ParseResult {
    let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
    let result = mib::parse_directories(&dirs);
    *state.last_parse.lock().unwrap() = Some(result.clone());
    result
}

#[tauri::command]
fn fetch(state: State<AppState>, node_id: String, connection: snmp::ConnectionParams) -> Result<snmp::FetchResult, String> {
    let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
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

#[tauri::command]
fn start_trap_listener(state: State<AppState>, id: String, config: trap::TrapListenerConfig) -> Result<trap::TrapListenerStatus, String> {
    let dirs = state.settings.lock().unwrap().active_profile().map(|p| p.dirs.clone()).unwrap_or_default();
    let mut cache = state.last_parse.lock().unwrap();
    if cache.is_none() {
        *cache = Some(mib::parse_directories(&dirs));
    }
    let oid_index = Arc::new(mib::build_oid_index(&cache.as_ref().unwrap().tree));
    state.trap_state.start(id, config, oid_index)
}

#[tauri::command]
fn stop_trap_listener(state: State<AppState>, id: String) {
    state.trap_state.stop(&id);
}

#[tauri::command]
fn poll_traps(state: State<AppState>, id: String, after_seq: u64) -> Vec<trap::TrapEvent> {
    state.trap_state.poll(&id, after_seq)
}

#[tauri::command]
fn clear_traps(state: State<AppState>, id: String) {
    state.trap_state.clear(&id);
}

#[tauri::command]
fn local_ips() -> Vec<String> {
    trap::local_ips()
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
            app.manage(AppState {
                settings_path,
                settings: Mutex::new(settings),
                last_parse: Mutex::new(None),
                trap_state: trap::TrapState::default(),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_mib_profiles,
            add_mib_profile,
            remove_mib_profile,
            rename_mib_profile,
            set_active_mib_profile,
            add_mib_dir,
            remove_mib_dir,
            list_host_profiles,
            get_mib_tree,
            fetch,
            start_trap_listener,
            stop_trap_listener,
            poll_traps,
            clear_traps,
            local_ips
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
