//! Where the files live. XDG, with the same fallbacks parlad's config uses.

use std::path::PathBuf;

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// `$XDG_CONFIG_HOME/parla`: the config plus the three editable files.
pub fn config_dir() -> PathBuf {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"))
        .join("parla")
}

/// `$XDG_DATA_HOME/parla`: models, history and learned spellings.
pub fn data_dir() -> PathBuf {
    std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/share"))
        .join("parla")
}

pub fn dictionary() -> PathBuf {
    config_dir().join("dictionary.toml")
}

pub fn snippets() -> PathBuf {
    config_dir().join("snippets.toml")
}

pub fn apps() -> PathBuf {
    config_dir().join("apps.toml")
}

pub fn history() -> PathBuf {
    data_dir().join("history.jsonl")
}

/// Corrections noticed after dictations, waiting to be accepted.
pub fn learned() -> PathBuf {
    data_dir().join("learned.toml")
}

/// Write `contents` to `path` through a temporary file in the same
/// directory, so a reader never sees a half-written file.
pub fn write_atomic(path: &std::path::Path, contents: &str) -> anyhow::Result<()> {
    use anyhow::Context as _;
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp",
        path.file_name()
            .map(|f| f.to_string_lossy())
            .unwrap_or_default()
    ));
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
