//! Persisted app settings: named MIB directory profiles (so e.g. different
//! releases' MIBs can be kept separate and switched between) and host
//! profiles. Stored as JSON in the OS's per-app config directory so they
//! survive restarts.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

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
pub struct MibProfile {
    pub id: String,
    pub name: String,
    pub dirs: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub mib_profiles: Vec<MibProfile>,
    pub active_mib_profile_id: String,
    pub host_profiles: Vec<HostProfile>,
}

/// Pre-profiles settings shape (a single flat `mibDirs` list), kept only to
/// migrate existing settings.json files - see `load()`.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SettingsV1 {
    mib_dirs: Vec<String>,
    host_profiles: Vec<HostProfile>,
}

fn new_profile_id() -> String {
    let millis = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    format!("mp{millis}")
}

impl Default for Settings {
    fn default() -> Self {
        let id = "default".to_string();
        Settings {
            mib_profiles: vec![MibProfile { id: id.clone(), name: "Default".into(), dirs: Vec::new() }],
            active_mib_profile_id: id,
            host_profiles: Vec::new(),
        }
    }
}

impl Settings {
    pub fn active_profile(&self) -> Option<&MibProfile> {
        self.mib_profiles.iter().find(|p| p.id == self.active_mib_profile_id)
    }

    pub fn active_profile_mut(&mut self) -> Option<&mut MibProfile> {
        self.mib_profiles.iter_mut().find(|p| p.id == self.active_mib_profile_id)
    }

    pub fn add_mib_profile(&mut self, name: String) -> &MibProfile {
        let id = new_profile_id();
        self.mib_profiles.push(MibProfile { id: id.clone(), name, dirs: Vec::new() });
        self.active_mib_profile_id = id;
        self.mib_profiles.last().unwrap()
    }

    /// No-ops if `id` is the only remaining profile - there must always be at least one.
    pub fn remove_mib_profile(&mut self, id: &str) {
        if self.mib_profiles.len() <= 1 {
            return;
        }
        self.mib_profiles.retain(|p| p.id != id);
        if self.active_mib_profile_id == id {
            self.active_mib_profile_id = self.mib_profiles[0].id.clone();
        }
    }

    pub fn rename_mib_profile(&mut self, id: &str, name: String) {
        if let Some(p) = self.mib_profiles.iter_mut().find(|p| p.id == id) {
            p.name = name;
        }
    }
}

pub fn load(path: &PathBuf) -> Settings {
    let Ok(text) = std::fs::read_to_string(path) else { return Settings::default() };
    if let Ok(settings) = serde_json::from_str::<Settings>(&text) {
        return settings;
    }
    // Fall back to the pre-profiles shape and migrate its flat directory
    // list into a single "Default" profile, so upgrading doesn't silently
    // drop directories someone already configured.
    if let Ok(old) = serde_json::from_str::<SettingsV1>(&text) {
        let id = "default".to_string();
        return Settings {
            mib_profiles: vec![MibProfile { id: id.clone(), name: "Default".into(), dirs: old.mib_dirs }],
            active_mib_profile_id: id,
            host_profiles: old.host_profiles,
        };
    }
    Settings::default()
}

pub fn save(path: &PathBuf, settings: &Settings) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(json) = serde_json::to_string_pretty(settings) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loading_a_pre_profiles_settings_file_migrates_its_directories_into_a_default_profile() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(
            &path,
            r#"{"mibDirs": ["/mibs/one", "/mibs/two"], "hostProfiles": [{"id": "h1", "label": "core", "addr": "10.0.0.1", "port": "161", "community": "public", "v3User": ""}]}"#,
        )
        .unwrap();

        let settings = load(&path);

        assert_eq!(settings.mib_profiles.len(), 1);
        assert_eq!(settings.mib_profiles[0].dirs, vec!["/mibs/one", "/mibs/two"]);
        assert_eq!(settings.active_mib_profile_id, settings.mib_profiles[0].id);
        assert_eq!(settings.host_profiles.len(), 1);
    }

    #[test]
    fn removing_the_active_profile_falls_back_to_a_remaining_one() {
        let mut settings = Settings::default();
        let first_id = settings.mib_profiles[0].id.clone();
        settings.add_mib_profile("v4.0".into());
        let second_id = settings.active_mib_profile_id.clone();
        assert_ne!(first_id, second_id);

        settings.remove_mib_profile(&second_id);

        assert_eq!(settings.mib_profiles.len(), 1);
        assert_eq!(settings.active_mib_profile_id, first_id);
    }

    #[test]
    fn removing_the_last_profile_is_a_no_op() {
        let mut settings = Settings::default();
        let only_id = settings.mib_profiles[0].id.clone();

        settings.remove_mib_profile(&only_id);

        assert_eq!(settings.mib_profiles.len(), 1);
        assert_eq!(settings.mib_profiles[0].id, only_id);
    }
}
