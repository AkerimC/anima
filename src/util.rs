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
