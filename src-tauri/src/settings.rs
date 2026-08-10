//! Persisted app settings: configured MIB directories and host profiles.
//! Stored as JSON in the OS's per-app config directory so they survive restarts.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct HostProfile {
    pub id: String,
    pub label: String,
    pub addr: String,
    pub port: String,
    pub community: String,
    pub v3_user: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub mib_dirs: Vec<String>,
    pub host_profiles: Vec<HostProfile>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            mib_dirs: Vec::new(),
            host_profiles: vec![
                HostProfile { id: "h1".into(), label: "core-switch-01".into(), addr: "10.0.1.5".into(), port: "161".into(), community: "public".into(), v3_user: "admin".into() },
                HostProfile { id: "h2".into(), label: "edge-router-02".into(), addr: "10.0.2.14".into(), port: "161".into(), community: "public".into(), v3_user: "netops".into() },
                HostProfile { id: "h3".into(), label: "dist-switch-03".into(), addr: "10.0.3.8".into(), port: "1161".into(), community: "private".into(), v3_user: "admin".into() },
            ],
        }
    }
}

pub fn load(path: &PathBuf) -> Settings {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn save(path: &PathBuf, settings: &Settings) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}
