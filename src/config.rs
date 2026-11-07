//! Mentos Macro configuration: `$XDG_CONFIG_HOME/mentos-macro/config.toml`.
//!
//! * every field has a default (`#[serde(default)]`), so older and partially edited files load;
//! * values are sanitized (clamped) after loading;
//! * writes are atomic (temp file + fsync + rename);
//! * an unparsable file is moved aside as `config.toml.corrupt-<unix time>` and defaults
//!   are used, so a bad edit never prevents the application from starting.

use crate::input::BackendKind;
use crate::keys::{self, KeyCombo};
use crate::sequence::{Preset, ReservedKeys, Sequence, Timing};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const APP_DIR_NAME: &str = "mentos-macro";
pub const CONFIG_FILE: &str = "config.toml";
pub const CONFIG_VERSION: u32 = 1;
pub const MAX_START_DELAY_MS: u32 = 30_000;
pub const MAX_REPEAT_COUNT: u32 = 1_000_000;
pub const MAX_TARGET_CLASS_LEN: usize = 128;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivationMode {
    /// Press the hotkey once to start, again to stop.
    #[default]
    Toggle,
    /// Runs only while the hotkey is held down.
    Hold,
}

impl ActivationMode {
    pub fn index(self) -> usize {
        match self {
            ActivationMode::Toggle => 0,
            ActivationMode::Hold => 1,
        }
    }
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            ActivationMode::Hold
        } else {
            ActivationMode::Toggle
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FocusLoss {
    /// Stop the macro when the target window loses focus.
    #[default]
    Stop,
    /// Release everything and resume when the target window is focused again.
    Pause,
}

impl FocusLoss {
    pub fn index(self) -> usize {
        match self {
            FocusLoss::Stop => 0,
            FocusLoss::Pause => 1,
        }
    }
    pub fn from_index(i: usize) -> Self {
        if i == 1 {
            FocusLoss::Pause
        } else {
            FocusLoss::Stop
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopConfig {
    pub enabled: bool,
    /// Number of cycles when looping; 0 means until stopped.
    pub count: u32,
}

impl Default for LoopConfig {
    fn default() -> Self {
        LoopConfig { enabled: true, count: 0 }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub start_stop: KeyCombo,
    pub emergency: KeyCombo,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Hotkeys { start_stop: KeyCombo::new(keys::XK_F1 + 5), emergency: KeyCombo::new(keys::XK_F1 + 8) }
    }
}

impl Hotkeys {
    pub fn reserved(&self) -> ReservedKeys {
        ReservedKeys { start_stop: Some(self.start_stop), emergency: Some(self.emergency) }
    }

    pub fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        for (name, k) in [("Start/stop", self.start_stop), ("Emergency stop", self.emergency)] {
            if keys::is_modifier(k.key) {
                out.push(format!("{name} hotkey needs a non-modifier key"));
            }
        }
        if self.start_stop.key == self.emergency.key {
            out.push("Start/stop and emergency stop must use different keys".into());
        }
        out
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct InputConfig {
    pub backend: BackendKind,
    /// Only send input to the window that was focused when the macro started.
    pub focus_protection: bool,
    pub focus_loss: FocusLoss,
    /// Optional WM_CLASS substring the target window must match (e.g. "sober").
    pub target_class: String,
}

impl Default for InputConfig {
    fn default() -> Self {
        InputConfig { backend: BackendKind::Xtest, focus_protection: true, focus_loss: FocusLoss::Stop, target_class: String::new() }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Advanced {
    /// Use `custom_sequence` instead of the built-in default sequence.
    pub custom_enabled: bool,
    /// Profile the custom sequence was last loaded from or saved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    pub custom_sequence: Sequence,
}

impl Default for Advanced {
    fn default() -> Self {
        Advanced { custom_enabled: false, active_profile: None, custom_sequence: Sequence::default_sequence() }
    }
}

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub preset: Preset,
    pub start_delay_ms: u32,
    pub activation: ActivationMode,
    pub timing: Timing,
    #[serde(rename = "loop")]
    pub looping: LoopConfig,
    pub hotkeys: Hotkeys,
    pub input: InputConfig,
    pub advanced: Advanced,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            version: CONFIG_VERSION,
            preset: Preset::Balanced,
            timing: Timing::default(),
            looping: LoopConfig::default(),
            start_delay_ms: 0,
            activation: ActivationMode::Toggle,
            hotkeys: Hotkeys::default(),
            input: InputConfig::default(),
            advanced: Advanced::default(),
        }
    }
}

impl Config {
    /// The sequence that runs: the custom one when enabled, otherwise the default.
    pub fn active_sequence(&self) -> Sequence {
        if self.advanced.custom_enabled {
            self.advanced.custom_sequence.clone()
        } else {
            Sequence::default_sequence()
        }
    }

    /// Apply a named preset's key/gap/loop timing.
    pub fn apply_preset(&mut self, preset: Preset) {
        if preset != Preset::Custom {
            let t = preset.timing();
            self.timing.key_ms = t.key_ms;
            self.timing.gap_ms = t.gap_ms;
            self.timing.loop_ms = t.loop_ms;
        }
        self.preset = preset;
    }

    /// Re-derive the preset after a manual timing change.
    pub fn refresh_preset(&mut self) {
        self.preset = Preset::detect(&self.timing);
    }

    /// Clamp values into range and repair invalid entries. Returns human-readable notes
    /// about anything that had to be changed.
    pub fn sanitize(&mut self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.timing.sanitize() {
            notes.push("Some timing values were out of range and have been adjusted".to_string());
        }
        if self.preset != Preset::Custom && Preset::detect(&self.timing) != self.preset {
            self.preset = Preset::Custom;
        }
        if self.start_delay_ms > MAX_START_DELAY_MS {
            self.start_delay_ms = MAX_START_DELAY_MS;
            notes.push(format!("Start delay limited to {MAX_START_DELAY_MS} ms"));
        }
        if self.looping.count > MAX_REPEAT_COUNT {
            self.looping.count = MAX_REPEAT_COUNT;
        }
        let trimmed: String = self.input.target_class.trim().chars().take(MAX_TARGET_CLASS_LEN).collect();
        if trimmed != self.input.target_class {
            self.input.target_class = trimmed;
        }
        let hotkey_issues = self.hotkeys.validate();
        if !hotkey_issues.is_empty() {
            notes.push(format!("Hotkeys reset to F6/F9: {}", hotkey_issues.join("; ")));
            self.hotkeys = Hotkeys::default();
        }
        if self.advanced.custom_sequence.steps.len() > crate::sequence::MAX_STEPS {
            self.advanced.custom_sequence.steps.truncate(crate::sequence::MAX_STEPS);
            notes.push("Custom sequence truncated".into());
        }
        self.advanced.custom_sequence.steps = std::mem::take(&mut self.advanced.custom_sequence.steps)
            .into_iter()
            .map(crate::sequence::Step::normalized)
            .collect();
        notes
    }
}

/// `$XDG_CONFIG_HOME/mentos-macro`, falling back to `~/.config/mentos-macro`.
pub fn default_config_dir() -> Result<PathBuf, String> {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        let p = PathBuf::from(xdg);
        if p.is_absolute() {
            return Ok(p.join(APP_DIR_NAME));
        }
    }
    let home = std::env::var_os("HOME").filter(|v| !v.is_empty()).ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config").join(APP_DIR_NAME))
}

/// Write `bytes` to `path` atomically: temp file in the same directory, fsync, rename.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(dir)?;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)?;
        // Persist the rename itself.
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn unix_time() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub struct LoadOutcome {
    pub config: Config,
    /// Things the user should know about (recovered corrupt file, adjusted values, ...).
    pub notices: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct ConfigStore {
    dir: PathBuf,
}

impl ConfigStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ConfigStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn config_path(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    pub fn profiles_dir(&self) -> PathBuf {
        self.dir.join("profiles")
    }

    pub fn load(&self) -> LoadOutcome {
        let path = self.config_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return LoadOutcome { config: Config::default(), notices: Vec::new() };
            }
            Err(e) => {
                return LoadOutcome {
                    config: Config::default(),
                    notices: vec![format!("Could not read {}: {e}. Using defaults.", path.display())],
                };
            }
        };
        match toml::from_str::<Config>(&text) {
            Ok(mut config) => {
                let mut notices = Vec::new();
                if config.version > CONFIG_VERSION {
                    notices.push(format!(
                        "config.toml was written by a newer version (v{}); unknown settings are ignored",
                        config.version
                    ));
                }
                config.version = CONFIG_VERSION;
                notices.extend(config.sanitize());
                LoadOutcome { config, notices }
            }
            Err(e) => {
                let backup = self.dir.join(format!("{CONFIG_FILE}.corrupt-{}", unix_time()));
                let moved = fs::rename(&path, &backup).is_ok();
                tracing::warn!("config parse error: {e}");
                let msg = if moved {
                    format!("config.toml was invalid and was moved to {}. Defaults restored.", backup.display())
                } else {
                    "config.toml is invalid; defaults are used for this session.".to_string()
                };
                LoadOutcome { config: Config::default(), notices: vec![msg] }
            }
        }
    }

    pub fn save(&self, config: &Config) -> io::Result<()> {
        let mut c = config.clone();
        c.version = CONFIG_VERSION;
        let body = toml::to_string_pretty(&c).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let text = format!("# Mentos Macro configuration (written automatically; safe to edit while the app is closed)\n{body}");
        atomic_write(&self.config_path(), text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_specification() {
        let c = Config::default();
        assert_eq!((c.timing.key_ms, c.timing.gap_ms, c.timing.loop_ms), (25, 15, 40));
        assert!(c.looping.enabled);
        assert_eq!(c.hotkeys.start_stop.to_string(), "F6");
        assert_eq!(c.hotkeys.emergency.to_string(), "F9");
        assert!(c.input.focus_protection);
        assert_eq!(c.input.backend, BackendKind::Xtest);
        assert!(!c.advanced.custom_enabled);
        assert_eq!(c.preset, Preset::Balanced);
    }

    #[test]
    fn round_trip_through_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path().join(APP_DIR_NAME));
        let mut c = Config::default();
        c.timing.key_ms = 33;
        c.refresh_preset();
        c.advanced.custom_enabled = true;
        c.input.target_class = "sober".into();
        store.save(&c).unwrap();
        let loaded = store.load();
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        assert_eq!(loaded.config, c);
        assert_eq!(loaded.config.preset, Preset::Custom);
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = ConfigStore::new(dir.path()).load();
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.notices.is_empty());
    }

    #[test]
    fn corrupt_file_is_moved_aside() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        fs::write(store.config_path(), "this is = = not toml").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.config, Config::default());
        assert_eq!(loaded.notices.len(), 1);
        assert!(!store.config_path().exists());
        let backups: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("config.toml.corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn partial_and_out_of_range_values_are_repaired() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        fs::write(
            store.config_path(),
            "version = 1\nstart_delay_ms = 999999\n[timing]\nkey_ms = 0\n[hotkeys]\nstart_stop = \"F6\"\nemergency = \"F6\"\n",
        )
        .unwrap();
        let loaded = store.load();
        let c = loaded.config;
        assert_eq!(c.timing.key_ms, 1);
        assert_eq!(c.timing.gap_ms, 15, "unspecified fields keep defaults");
        assert_eq!(c.start_delay_ms, MAX_START_DELAY_MS);
        assert_eq!(c.hotkeys, Hotkeys::default());
        assert_eq!(c.preset, Preset::Custom);
        assert!(loaded.notices.len() >= 3, "{:?}", loaded.notices);
    }

    #[test]
    fn newer_version_is_accepted_with_notice() {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(dir.path());
        fs::write(store.config_path(), "version = 7\nfuture_setting = true\n").unwrap();
        let loaded = store.load();
        assert_eq!(loaded.config.version, CONFIG_VERSION);
        assert_eq!(loaded.notices.len(), 1);
    }

    #[test]
    fn hotkey_validation() {
        let mut h = Hotkeys::default();
        assert!(h.validate().is_empty());
        h.emergency = h.start_stop;
        assert_eq!(h.validate().len(), 1);
        h.emergency = KeyCombo::new(keys::XK_SHIFT_L);
        assert_eq!(h.validate().len(), 1);
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.toml");
        atomic_write(&p, b"one").unwrap();
        atomic_write(&p, b"two").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "two");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
