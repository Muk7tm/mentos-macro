//! Named sequence profiles stored as individual TOML files in
//! `$XDG_CONFIG_HOME/mentos-macro/profiles/`. The same file format is used for import and
//! export, so an exported profile can be shared and imported elsewhere.

use crate::config::{atomic_write, LoopConfig};
use crate::sequence::{self, ReservedKeys, Sequence, Step, Timing};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const PROFILE_FORMAT: &str = "mentos-sequence";
pub const PROFILE_VERSION: u32 = 1;
pub const MAX_NAME_LEN: usize = 48;
const MAX_FILE_BYTES: u64 = 1024 * 1024;

#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub struct Profile {
    pub format: String,
    pub version: u32,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<Timing>,
    #[serde(default, rename = "loop", skip_serializing_if = "Option::is_none")]
    pub looping: Option<LoopConfig>,
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Profile {
    pub fn new(name: &str, sequence: &Sequence, timing: Option<Timing>, looping: Option<LoopConfig>) -> Profile {
        Profile {
            format: PROFILE_FORMAT.into(),
            version: PROFILE_VERSION,
            name: name.to_string(),
            timing,
            looping,
            steps: sequence.steps.iter().cloned().map(Step::normalized).collect(),
        }
    }

    pub fn sequence(&self) -> Sequence {
        Sequence { steps: self.steps.clone() }
    }

    /// Parse and validate profile text (used for both stored and imported files).
    pub fn parse(text: &str) -> Result<Profile, String> {
        let mut p: Profile = toml::from_str(text).map_err(|e| format!("Not a valid profile file: {}", e.message()))?;
        if p.format != PROFILE_FORMAT {
            return Err(format!("Unsupported file format \"{}\" (expected \"{PROFILE_FORMAT}\")", p.format));
        }
        if p.version > PROFILE_VERSION {
            return Err(format!("Profile version {} is newer than this application supports", p.version));
        }
        p.name = validate_name(&p.name)?;
        if let Some(t) = p.timing.as_mut() {
            t.sanitize();
        }
        p.steps = p.steps.into_iter().map(Step::normalized).collect();
        let timing = p.timing.unwrap_or_default();
        let issues = sequence::validate(&p.sequence(), &timing, false, &ReservedKeys::default());
        if let Some(first) = issues.first() {
            return Err(format!("Invalid sequence: {first}"));
        }
        Ok(p)
    }

    pub fn to_toml(&self) -> Result<String, String> {
        let body = toml::to_string_pretty(self).map_err(|e| e.to_string())?;
        Ok(format!("# Mentos Macro sequence profile\n{body}"))
    }
}

pub fn validate_name(name: &str) -> Result<String, String> {
    let n = name.trim();
    if n.is_empty() {
        return Err("Profile name cannot be empty".into());
    }
    if n.chars().count() > MAX_NAME_LEN {
        return Err(format!("Profile name is longer than {MAX_NAME_LEN} characters"));
    }
    if n.chars().any(char::is_control) {
        return Err("Profile name contains control characters".into());
    }
    Ok(n.to_string())
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s: String = out.trim_matches('-').chars().take(40).collect();
    if s.is_empty() {
        "profile".into()
    } else {
        s
    }
}

/// Expand a leading `~/` in user-entered paths.
pub fn expand_home(path: &str) -> PathBuf {
    let p = path.trim();
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(p)
}

#[derive(Clone, Debug)]
struct Entry {
    name: String,
    path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ProfileStore {
    dir: PathBuf,
}

impl ProfileStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        ProfileStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn read_file(path: &Path) -> Result<Profile, String> {
        let meta = fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.len() > MAX_FILE_BYTES {
            return Err(format!("{} is too large to be a profile", path.display()));
        }
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Profile::parse(&text)
    }

    fn entries(&self) -> Vec<Entry> {
        let Ok(rd) = fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out: Vec<Entry> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .filter_map(|path| match Self::read_file(&path) {
                Ok(p) => Some(Entry { name: p.name, path }),
                Err(e) => {
                    tracing::warn!("skipping profile: {e}");
                    None
                }
            })
            .collect();
        out.sort_by_key(|e| e.name.to_lowercase());
        out
    }

    fn find(&self, name: &str) -> Option<Entry> {
        self.entries().into_iter().find(|e| e.name.eq_ignore_ascii_case(name))
    }

    fn unused_path(&self, name: &str) -> PathBuf {
        let base = slug(name);
        let mut candidate = self.dir.join(format!("{base}.toml"));
        let mut n = 2;
        while candidate.exists() {
            candidate = self.dir.join(format!("{base}-{n}.toml"));
            n += 1;
        }
        candidate
    }

    fn unique_name(&self, wanted: &str) -> String {
        if self.find(wanted).is_none() {
            return wanted.to_string();
        }
        (2..)
            .map(|n| {
                let suffix = format!(" ({n})");
                let base: String = wanted.chars().take(MAX_NAME_LEN - suffix.len()).collect();
                format!("{base}{suffix}")
            })
            .find(|candidate| self.find(candidate).is_none())
            .unwrap_or_else(|| wanted.to_string())
    }

    fn write(path: &Path, profile: &Profile) -> Result<(), String> {
        let text = profile.to_toml()?;
        atomic_write(path, text.as_bytes()).map_err(|e| format!("Could not write {}: {e}", path.display()))
    }

    pub fn list(&self) -> Vec<String> {
        self.entries().into_iter().map(|e| e.name).collect()
    }

    pub fn load(&self, name: &str) -> Result<Profile, String> {
        let entry = self.find(name).ok_or_else(|| format!("Profile \"{name}\" not found"))?;
        Self::read_file(&entry.path)
    }

    /// Create or overwrite the profile with `profile.name`.
    pub fn save(&self, profile: &Profile) -> Result<(), String> {
        let mut p = profile.clone();
        p.name = validate_name(&p.name)?;
        let path = match self.find(&p.name) {
            Some(existing) => {
                p.name = existing.name;
                existing.path
            }
            None => self.unused_path(&p.name),
        };
        Self::write(&path, &p)
    }

    pub fn rename(&self, old: &str, new: &str) -> Result<String, String> {
        let new = validate_name(new)?;
        let entry = self.find(old).ok_or_else(|| format!("Profile \"{old}\" not found"))?;
        if let Some(other) = self.find(&new) {
            if other.path != entry.path {
                return Err(format!("A profile named \"{}\" already exists", other.name));
            }
        }
        let mut p = Self::read_file(&entry.path)?;
        p.name = new.clone();
        let path = self.unused_path(&new);
        Self::write(&path, &p)?;
        if path != entry.path {
            fs::remove_file(&entry.path).map_err(|e| format!("Could not remove old profile file: {e}"))?;
        }
        Ok(new)
    }

    pub fn duplicate(&self, name: &str) -> Result<String, String> {
        let mut p = self.load(name)?;
        let wanted: String = format!("{} copy", p.name).chars().take(MAX_NAME_LEN).collect();
        p.name = self.unique_name(&wanted);
        Self::write(&self.unused_path(&p.name), &p)?;
        Ok(p.name)
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        let entry = self.find(name).ok_or_else(|| format!("Profile \"{name}\" not found"))?;
        fs::remove_file(&entry.path).map_err(|e| format!("Could not delete profile: {e}"))
    }

    /// Export to `target`. A directory target receives `<slug>.toml`. Returns the file path.
    pub fn export(&self, name: &str, target: &Path) -> Result<PathBuf, String> {
        let p = self.load(name)?;
        let path = if target.is_dir() { target.join(format!("{}.toml", slug(&p.name))) } else { target.to_path_buf() };
        if path.as_os_str().is_empty() {
            return Err("Enter a file path to export to".into());
        }
        Self::write(&path, &p)?;
        Ok(path)
    }

    /// Import a profile file, renaming it if the name is taken. Returns the stored name.
    pub fn import(&self, source: &Path) -> Result<String, String> {
        if source.as_os_str().is_empty() {
            return Err("Enter the path of a profile file to import".into());
        }
        let mut p = Self::read_file(source)?;
        p.name = self.unique_name(&p.name);
        Self::write(&self.unused_path(&p.name), &p)?;
        Ok(p.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyCombo;

    fn store() -> (tempfile::TempDir, ProfileStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = ProfileStore::new(dir.path().join("profiles"));
        (dir, s)
    }

    fn sample(name: &str) -> Profile {
        let seq = Sequence { steps: vec![Step::press(KeyCombo::parse("Shift+E").unwrap()), Step::wait(50)] };
        Profile::new(name, &seq, Some(Timing::default()), None)
    }

    #[test]
    fn save_list_load() {
        let (_d, s) = store();
        s.save(&sample("Beta")).unwrap();
        s.save(&sample("alpha")).unwrap();
        assert_eq!(s.list(), ["alpha", "Beta"]);
        assert_eq!(s.load("beta").unwrap(), sample("Beta"));
        // overwrite keeps a single file
        s.save(&sample("BETA")).unwrap();
        assert_eq!(s.list().len(), 2);
    }

    #[test]
    fn rename_duplicate_delete() {
        let (_d, s) = store();
        s.save(&sample("One")).unwrap();
        s.save(&sample("Two")).unwrap();
        assert!(s.rename("One", "two").is_err(), "name collision");
        assert_eq!(s.rename("One", "Uno").unwrap(), "Uno");
        assert_eq!(s.list(), ["Two", "Uno"]);
        assert_eq!(s.duplicate("Uno").unwrap(), "Uno copy");
        assert_eq!(s.duplicate("Uno").unwrap(), "Uno copy (2)");
        s.delete("Uno").unwrap();
        assert_eq!(s.list(), ["Two", "Uno copy", "Uno copy (2)"]);
        assert!(s.delete("missing").is_err());
    }

    #[test]
    fn export_then_import_round_trips() {
        let (d, s) = store();
        s.save(&sample("Combo")).unwrap();
        let out = s.export("Combo", d.path()).unwrap();
        assert!(out.ends_with("combo.toml"));
        let imported = s.import(&out).unwrap();
        assert_eq!(imported, "Combo (2)");
        assert_eq!(s.load("Combo (2)").unwrap().steps, sample("Combo").steps);
    }

    #[test]
    fn import_rejects_bad_files() {
        let (d, s) = store();
        let bad = d.path().join("bad.toml");
        fs::write(&bad, "format = \"something-else\"\nversion = 1\nname = \"x\"\n").unwrap();
        assert!(s.import(&bad).unwrap_err().contains("Unsupported"));
        fs::write(&bad, "not toml ===").unwrap();
        assert!(s.import(&bad).is_err());
        fs::write(&bad, "format = \"mentos-sequence\"\nversion = 1\nname = \"x\"\n[[steps]]\naction = \"key_down\"\nkey = \"R\"\n")
            .unwrap();
        assert!(s.import(&bad).unwrap_err().contains("never released"));
        fs::write(&bad, "format = \"mentos-sequence\"\nversion = 1\nname = \"x\"\n[[steps]]\naction = \"explode\"\n").unwrap();
        assert!(s.import(&bad).is_err());
        assert!(s.import(Path::new("")).is_err());
        assert!(s.list().is_empty());
    }

    #[test]
    fn names_are_validated() {
        assert!(validate_name("   ").is_err());
        assert!(validate_name(&"x".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(validate_name("a\u{7}b").is_err());
        assert_eq!(validate_name("  ok  ").unwrap(), "ok");
        assert_eq!(slug("My Combo #1!"), "my-combo-1");
        assert_eq!(slug("???"), "profile");
    }
}
