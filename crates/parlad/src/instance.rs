//! Single-instance guard: an exclusive `flock` on
//! `$XDG_STATE_HOME/parla/parlad.lock`. Two daemons would both register the
//! same hotkeys and both type every utterance.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::io::AsRawFd as _;
use std::path::PathBuf;

/// Held for the daemon's lifetime; dropping it releases the lock.
pub struct InstanceLock {
    _file: File,
    path: PathBuf,
}

impl InstanceLock {
    pub fn acquire() -> anyhow::Result<Self> {
        let path = lock_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        // SAFETY: flock on a valid, open descriptor we own; no memory involved.
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
                let holder = std::fs::read_to_string(&path).unwrap_or_default();
                let holder = holder.trim();
                anyhow::bail!(
                    "another parlad is already running{} (lock {})",
                    if holder.is_empty() {
                        String::new()
                    } else {
                        format!(" (pid {holder})")
                    },
                    path.display()
                );
            }
            return Err(anyhow::Error::from(err).context(format!("cannot lock {}", path.display())));
        }
        // Best effort: record who holds it, for the error message above.
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

fn lock_path() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
                .join(".local/state")
        });
    base.join("parla/parlad.lock")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_is_refused() {
        let dir = std::env::temp_dir().join(format!("parla-instance-{}", std::process::id()));
        std::env::set_var("XDG_STATE_HOME", &dir);
        let first = InstanceLock::acquire().unwrap();
        let Err(err) = InstanceLock::acquire() else {
            panic!("second acquire succeeded while the first lock is held");
        };
        assert!(err.to_string().contains("already running"), "{err}");
        drop(first);
        let _again = InstanceLock::acquire().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
