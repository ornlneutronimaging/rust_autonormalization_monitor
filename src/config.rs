//! Read/write access to the shared auto-normalization configuration file
//! (`autoreduction.cfg`). The file is simple `key: value` YAML, e.g.:
//!
//! ```text
//! user_autoreduction_config_file: /SNS/VENUS/IPTS-xxx/.../config.h5
//! activate: false
//! ipts: IPTS-36967
//! last_modified: '2026-07-18 08:43:47'
//! last_modified_by: j35
//! ```
//!
//! Only the `activate` line is rewritten when the state is toggled (plus the
//! `last_modified`/`last_modified_by` bookkeeping lines the file already
//! carries); every other line is preserved byte-for-byte. The file is
//! rewritten in place (truncate + write, not temp + rename) so the inode and
//! its group/ACL permissions on the shared filesystem are kept.

use std::fs;
use std::path::Path;

/// Snapshot of the configuration file, keeping the raw key order for display.
#[derive(Clone, Debug, Default)]
pub struct AutoNormConfig {
    /// Parsed value of the `activate` flag.
    pub activate: bool,
    /// All `key: value` pairs in file order (values with quotes stripped),
    /// for read-only display in the UI.
    pub entries: Vec<(String, String)>,
}

/// Split a `key: value` line; returns `None` for blanks/comments.
fn split_key_value(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once(':')?;
    Some((key.trim(), value.trim()))
}

/// `True`, `true`, `1`, `yes`, `on` → true (the file historically mixes
/// Python-style `True` and YAML `true`).
fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim_matches(|c| c == '\'' || c == '"').to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// Read and parse the configuration file.
pub fn read(path: &Path) -> Result<AutoNormConfig, String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut cfg = AutoNormConfig::default();
    let mut saw_activate = false;
    for line in content.lines() {
        if let Some((key, value)) = split_key_value(line) {
            if key == "activate" {
                cfg.activate = parse_bool(value);
                saw_activate = true;
            }
            cfg.entries.push((
                key.to_owned(),
                value.trim_matches(|c| c == '\'' || c == '"').to_owned(),
            ));
        }
    }
    if !saw_activate {
        return Err(format!("no 'activate' flag found in {}", path.display()));
    }
    Ok(cfg)
}

impl AutoNormConfig {
    /// Value of `key` as displayed (quotes stripped), if present.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// Set `key` to `value` in the file, updating the `last_modified` /
/// `last_modified_by` bookkeeping lines if present. All other lines are
/// preserved unchanged. Errors if `key` is not already in the file (this
/// tool only edits existing fields, it never adds new ones).
pub fn set_value(path: &Path, key: &str, value: &str) -> Result<(), String> {
    let content = fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");

    let mut saw_key = false;
    let mut lines: Vec<String> = Vec::new();
    for line in content.lines() {
        match split_key_value(line) {
            Some((k, _)) if k == key => {
                lines.push(format!("{key}: {value}"));
                saw_key = true;
            }
            Some(("last_modified", _)) => lines.push(format!("last_modified: '{now}'")),
            Some(("last_modified_by", _)) => lines.push(format!("last_modified_by: {user}")),
            _ => lines.push(line.to_owned()),
        }
    }
    if !saw_key {
        return Err(format!("no '{key}' field found in {}", path.display()));
    }

    let mut new_content = lines.join("\n");
    new_content.push('\n');
    fs::write(path, new_content).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Set the `activate` flag (written as lowercase `true`/`false`).
pub fn set_activate(path: &Path, activate: bool) -> Result<(), String> {
    set_value(path, "activate", if activate { "true" } else { "false" })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
user_autoreduction_config_file: /SNS/VENUS/IPTS-1/shared/autoreduce/configs/config.h5
activate: false
ipts: IPTS-36967
last_modified: '2026-07-18 08:43:47'
last_modified_by: j35
";

    fn write_sample(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("autoreduction.cfg");
        fs::write(&path, SAMPLE).unwrap();
        path
    }

    #[test]
    fn read_parses_flag_and_entries() {
        let dir = std::env::temp_dir().join("anm_test_read");
        fs::create_dir_all(&dir).unwrap();
        let path = write_sample(&dir);
        let cfg = read(&path).unwrap();
        assert!(!cfg.activate);
        assert_eq!(cfg.entries.len(), 5);
        assert_eq!(cfg.entries[2], ("ipts".to_owned(), "IPTS-36967".to_owned()));
        // Quotes are stripped for display.
        assert_eq!(cfg.entries[3].1, "2026-07-18 08:43:47");
    }

    #[test]
    fn python_style_true_is_accepted() {
        let dir = std::env::temp_dir().join("anm_test_pytrue");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autoreduction.cfg");
        fs::write(&path, "activate: True\n").unwrap();
        assert!(read(&path).unwrap().activate);
    }

    #[test]
    fn set_activate_flips_flag_and_preserves_other_lines() {
        let dir = std::env::temp_dir().join("anm_test_write");
        fs::create_dir_all(&dir).unwrap();
        let path = write_sample(&dir);
        set_activate(&path, true).unwrap();
        let cfg = read(&path).unwrap();
        assert!(cfg.activate);
        let content = fs::read_to_string(&path).unwrap();
        // Untouched lines are preserved byte-for-byte.
        assert!(content.contains(
            "user_autoreduction_config_file: /SNS/VENUS/IPTS-1/shared/autoreduce/configs/config.h5"
        ));
        assert!(content.contains("ipts: IPTS-36967"));
        // Bookkeeping lines were rewritten (timestamp changed, still quoted).
        assert!(content.contains("last_modified: '2026-"));
        assert!(!content.contains("last_modified: '2026-07-18 08:43:47'"));
    }

    #[test]
    fn set_value_updates_ipts() {
        let dir = std::env::temp_dir().join("anm_test_ipts");
        fs::create_dir_all(&dir).unwrap();
        let path = write_sample(&dir);
        set_value(&path, "ipts", "IPTS-99999").unwrap();
        let cfg = read(&path).unwrap();
        assert_eq!(cfg.get("ipts"), Some("IPTS-99999"));
        // Other fields untouched, flag unchanged.
        assert!(!cfg.activate);
        // Unknown keys are rejected, nothing is appended.
        assert!(set_value(&path, "not_a_key", "x").is_err());
        assert!(read(&path).unwrap().get("not_a_key").is_none());
    }

    #[test]
    fn missing_activate_is_an_error() {
        let dir = std::env::temp_dir().join("anm_test_missing");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("autoreduction.cfg");
        fs::write(&path, "ipts: IPTS-1\n").unwrap();
        assert!(read(&path).is_err());
        assert!(set_activate(&path, true).is_err());
    }
}
