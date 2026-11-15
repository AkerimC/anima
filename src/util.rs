use std::path::{Path, PathBuf};

/// Expand a leading `~` and make relative paths absolute against `base`.
pub fn expand_path(input: &str, base: Option<&Path>) -> PathBuf {
    let trimmed = input.trim();
    let expanded = if trimmed == "~" {
        dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"))
    } else if let Some(rest) = trimmed.strip_prefix("~/") {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(rest)
    } else {
        PathBuf::from(trimmed)
    };

    if expanded.is_absolute() {
        expanded
    } else if let Some(base) = base {
        base.join(expanded)
    } else {
        expanded
    }
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("anima")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("anima")
}

/// Directory where the daemon publishes its control socket and state.
pub fn runtime_dir() -> PathBuf {
    dirs::runtime_dir().unwrap_or_else(std::env::temp_dir)
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join("anima.sock")
}

pub fn now_secs() -> f64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Parse a JSON file.  A corrupt file is renamed to `<name>.corrupt-<secs>`
/// and defaults are returned, so the next save cannot silently destroy it.
pub fn read_json_or_backup<T>(path: &Path) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    let Ok(text) = std::fs::read_to_string(path) else {
        return T::default();
    };
    match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(err) => {
            let stamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let bad = path.with_extension(format!("corrupt-{stamp}"));
            log::warn!("{}: {err}; kept as {}", path.display(), bad.display());
            let _ = std::fs::rename(path, bad);
            T::default()
        }
    }
}

/// Write a file atomically (write to a sibling temp file, then rename).
pub fn atomic_write(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod json_tests {
    #[test]
    fn corrupt_json_is_kept_aside_not_lost() {
        let dir = std::env::temp_dir().join(format!("anima-json-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("inventory.json");
        std::fs::write(&path, "{ not json").unwrap();
        let v: std::collections::HashMap<String, Vec<String>> = super::read_json_or_backup(&path);
        assert!(v.is_empty());
        assert!(!path.exists(), "the bad file is moved aside");
        let kept: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("inventory.corrupt-"))
            .collect();
        assert_eq!(kept.len(), 1, "one backup: {kept:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_json_uses_defaults() {
        let path = std::env::temp_dir().join(format!("anima-missing-{}.json", std::process::id()));
        let v: std::collections::HashMap<String, Vec<String>> = super::read_json_or_backup(&path);
        assert!(v.is_empty());
    }
}
