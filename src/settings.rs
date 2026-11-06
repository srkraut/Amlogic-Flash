//! Persistent user settings (~/Library/Application Support/Amlogic Flasher/settings.json).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum ThemeChoice {
    System,
    Light,
    Dark,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeChoice,
    pub precheck: bool,
    pub reboot: bool,
    pub notify: bool,
    pub sound: bool,
    pub wait_minutes: u32,
    pub recent: Vec<String>,
    pub show_all_usb: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::System,
            precheck: true,
            reboot: true,
            notify: true,
            sound: true,
            wait_minutes: 10,
            recent: vec![],
            show_all_usb: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join("Library/Application Support/Amlogic Flasher/settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = path() {
            if let Some(dir) = p.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(json) = serde_json::to_vec_pretty(self) {
                let _ = std::fs::write(p, json);
            }
        }
    }

    pub fn push_recent(&mut self, p: &str) {
        self.recent.retain(|r| r != p);
        self.recent.insert(0, p.to_string());
        self.recent.truncate(8);
    }
}
