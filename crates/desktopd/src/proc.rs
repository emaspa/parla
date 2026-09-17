//! One way to run a child process: with a timeout, with stderr captured, and
//! with every error naming the program. A missing binary reads
//! `kdotool: not found in PATH`, not `No such file or directory (os error 2)`.

use std::io::Write as _;
use std::process::{ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// Default per-invocation budget.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum ProcError {
    #[error("{program}: not found in PATH")]
    NotFound { program: String },
    #[error("{program}: could not start: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("{program} {verb}: timed out after {timeout:?}")]
    Timeout {
        program: String,
        verb: String,
        timeout: Duration,
    },
    #[error("{program} {verb}: failed ({status}): {stderr}")]
    Failed {
        program: String,
        verb: String,
        status: String,
        stderr: String,
    },
    #[error("{program}: i/o error: {source}")]
    Io {
        program: String,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Output {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn success(&self) -> bool {
        self.status.success()
    }
}

/// A command under construction. `output()` returns whatever the child did;
/// `run()` additionally turns a non-zero exit into `ProcError::Failed`.
#[derive(Debug, Clone)]
pub struct Cmd {
    program: String,
    args: Vec<String>,
    env: Vec<(String, String)>,
    stdin: Option<Vec<u8>>,
    timeout: Duration,
}

impl Cmd {
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            stdin: None,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn arg(mut self, a: impl Into<String>) -> Self {
        self.args.push(a.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.push((k.into(), v.into()));
        self
    }

    /// Bytes written to the child's stdin (closed afterwards). Without this
    /// stdin is /dev/null.
    pub fn stdin(mut self, data: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(data.into());
        self
    }

    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = t;
        self
    }

    pub fn program(&self) -> &str {
        &self.program
    }

    /// The first argument, used to label errors ("kdotool search: ...").
    fn verb(&self) -> String {
        self.args.first().cloned().unwrap_or_default()
    }

    fn spawn_error(&self, e: std::io::Error) -> ProcError {
        if e.kind() == std::io::ErrorKind::NotFound {
            ProcError::NotFound {
                program: self.program.clone(),
            }
        } else {
            ProcError::Spawn {
                program: self.program.clone(),
                source: e,
            }
        }
    }

    fn io_error(&self, e: std::io::Error) -> ProcError {
        ProcError::Io {
            program: self.program.clone(),
            source: e,
        }
    }

    fn check(&self, out: Output) -> Result<Output, ProcError> {
        if out.status.success() {
            Ok(out)
        } else {
            Err(ProcError::Failed {
                program: self.program.clone(),
                verb: self.verb(),
                status: describe_status(out.status),
                stderr: out.stderr.trim().to_string(),
            })
        }
    }

    /// Run to completion on the tokio runtime.
    pub async fn output(&self) -> Result<Output, ProcError> {
        let mut c = tokio::process::Command::new(&self.program);
        c.args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = c.spawn().map_err(|e| self.spawn_error(e))?;
        if let (Some(data), Some(mut sin)) = (&self.stdin, child.stdin.take()) {
            use tokio::io::AsyncWriteExt as _;
            // a child that exits before reading everything is reported by
            // its status, not by the broken pipe
            let _ = sin.write_all(data).await;
            drop(sin);
        }
        let waited = tokio::time::timeout(self.timeout, child.wait_with_output()).await;
        match waited {
            Ok(Ok(o)) => self.check(Output {
                status: o.status,
                stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
            }),
            Ok(Err(e)) => Err(self.io_error(e)),
            // kill_on_drop reaps the child
            Err(_) => Err(ProcError::Timeout {
                program: self.program.clone(),
                verb: self.verb(),
                timeout: self.timeout,
            }),
        }
    }

    /// Like `output`, but a non-zero exit is an error and only stdout comes back.
    pub async fn run(&self) -> Result<String, ProcError> {
        self.output().await.map(|o| o.stdout)
    }

    /// Blocking twin of `output`, for callers off the async runtime (the
    /// injectors implement a synchronous trait).
    pub fn output_blocking(&self) -> Result<Output, ProcError> {
        let mut c = std::process::Command::new(&self.program);
        c.args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(if self.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().map_err(|e| self.spawn_error(e))?;
        if let (Some(data), Some(mut sin)) = (&self.stdin, child.stdin.take()) {
            let _ = sin.write_all(data);
            drop(sin);
        }
        // drain both pipes on helper threads so a chatty child cannot block
        let stdout = child.stdout.take().map(drain);
        let stderr = child.stderr.take().map(drain);
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Ok(None) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ProcError::Timeout {
                        program: self.program.clone(),
                        verb: self.verb(),
                        timeout: self.timeout,
                    });
                }
                Err(e) => return Err(self.io_error(e)),
            }
        };
        let join = |h: Option<std::thread::JoinHandle<Vec<u8>>>| {
            h.and_then(|h| h.join().ok()).unwrap_or_default()
        };
        self.check(Output {
            status,
            stdout: String::from_utf8_lossy(&join(stdout)).into_owned(),
            stderr: String::from_utf8_lossy(&join(stderr)).into_owned(),
        })
    }

    pub fn run_blocking(&self) -> Result<String, ProcError> {
        self.output_blocking().map(|o| o.stdout)
    }

    /// Start the process detached (no wait, all fds to /dev/null). For
    /// terminals and GUI apps that outlive the daemon's interest in them.
    pub fn spawn_detached(&self) -> Result<u32, ProcError> {
        let child = std::process::Command::new(&self.program)
            .args(&self.args)
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| self.spawn_error(e))?;
        Ok(child.id())
    }
}

fn drain<R: std::io::Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

fn describe_status(status: ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt as _;
    match (status.code(), status.signal()) {
        (Some(c), _) => format!("exit {c}"),
        (None, Some(s)) => format!("signal {s}"),
        (None, None) => "unknown status".into(),
    }
}

/// Is `prog` on PATH?
pub fn which(prog: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).find_map(|dir| {
            let full = dir.join(prog);
            full.is_file().then_some(full)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_binary_names_the_program() {
        let err = Cmd::new("parla-definitely-not-a-binary")
            .arg("x")
            .output()
            .await
            .unwrap_err();
        assert!(matches!(err, ProcError::NotFound { .. }));
        assert!(err.to_string().starts_with("parla-definitely-not-a-binary:"));
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let err = Cmd::new("sleep")
            .arg("5")
            .timeout(Duration::from_millis(100))
            .output()
            .await
            .unwrap_err();
        assert!(matches!(err, ProcError::Timeout { .. }), "{err}");
    }

    #[tokio::test]
    async fn failure_carries_stderr_and_verb() {
        let err = Cmd::new("sh")
            .args(["-c", "echo boom >&2; exit 3"])
            .run()
            .await
            .unwrap_err();
        let s = err.to_string();
        assert!(s.contains("sh -c: failed (exit 3): boom"), "{s}");
    }

    #[test]
    fn blocking_stdin_and_timeout() {
        let out = Cmd::new("cat").stdin("hello").run_blocking().unwrap();
        assert_eq!(out, "hello");
        let err = Cmd::new("sleep")
            .arg("5")
            .timeout(Duration::from_millis(100))
            .output_blocking()
            .unwrap_err();
        assert!(matches!(err, ProcError::Timeout { .. }));
        let err = Cmd::new("parla-definitely-not-a-binary")
            .output_blocking()
            .unwrap_err();
        assert!(matches!(err, ProcError::NotFound { .. }));
    }
}
