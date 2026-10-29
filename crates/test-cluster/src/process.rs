//! Bounded invocations of `kind`, the container engine and other tools with no API the
//! runner can call directly.
//!
//! Each child gets a cleared environment plus an allowlist, a deadline after
//! which it is killed, and a cap on how much output is kept. `KUBECONFIG` is
//! deliberately not on the allowlist: every `kind` call that touches a
//! kubeconfig names the runner's private one, and a caller that needs a child
//! to see a kubeconfig names it with [`Invocation::env`].

use crate::{Error, Result};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// Variables a child may inherit: enough to find the tools and reach the
/// podman or Docker service the caller already selected, nothing that selects
/// a cluster. kind's provider is set per call by [`crate::engine::EngineCommands::kind`].
const INHERITED: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_RUNTIME_DIR",
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_CERT_PATH",
    "DOCKER_TLS_VERIFY",
    "CONTAINER_HOST",
    "CONTAINER_CONNECTION",
    "CONTAINERS_CONF",
    "CONTAINERS_STORAGE_CONF",
];

/// Output beyond this is drained and dropped rather than kept in memory.
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

/// One command to run: a program, its arguments, optional stdin from a file,
/// any variables beyond the inherited allowlist and a deadline.
pub struct Invocation {
    program: OsString,
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    stdin: Option<PathBuf>,
    timeout: Duration,
}

/// What a finished child wrote and whether it exited successfully.
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl Invocation {
    /// A command running `program`, found on the inherited `PATH` unless it
    /// is a path, with a two-minute deadline.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            program: program.as_ref().to_owned(),
            arguments: Vec::new(),
            environment: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(120),
        }
    }

    #[must_use]
    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_owned());
        self
    }

    #[must_use]
    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.arguments
            .extend(arguments.into_iter().map(|item| item.as_ref().to_owned()));
        self
    }

    /// Set a variable for the child in addition to the inherited allowlist.
    #[must_use]
    pub fn env(mut self, name: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        self.environment
            .push((name.as_ref().to_owned(), value.as_ref().to_owned()));
        self
    }

    #[must_use]
    pub fn stdin(mut self, path: PathBuf) -> Self {
        self.stdin = Some(path);
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Start the command with the same environment, stdin and output bounds
    /// as [`Invocation::output`], and return a handle to it. The deadline is
    /// not applied: the caller decides when the child is waited for or killed.
    ///
    /// # Errors
    /// Fails when the program cannot start or its stdin file cannot be opened.
    pub fn spawn(self) -> Result<Running> {
        let (program, mut child) = self.start()?;
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));
        let readers = [
            tokio::spawn(collect(child.stdout.take(), stdout.clone())),
            tokio::spawn(collect(child.stderr.take(), stderr.clone())),
        ];
        Ok(Running {
            program,
            child,
            stdout,
            stderr,
            readers,
        })
    }

    fn start(self) -> Result<(String, tokio::process::Child)> {
        let program = self.program.to_string_lossy().into_owned();
        let mut command = tokio::process::Command::new(&self.program);
        command
            .args(&self.arguments)
            .env_clear()
            .envs(inherited(std::env::vars_os()))
            .envs(self.environment)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match &self.stdin {
            Some(path) => command.stdin(std::fs::File::open(path)?),
            None => command.stdin(Stdio::null()),
        };
        let child = command.spawn().map_err(|error| Error::Process {
            program: program.clone(),
            detail: format!("could not start: {error}"),
        })?;
        Ok((program, child))
    }

    /// Run the command and return its output whether or not it succeeded.
    ///
    /// # Errors
    /// Fails when the program cannot start, its stdin file cannot be opened,
    /// or it does not finish before the deadline, in which case it is killed.
    pub async fn output(self) -> Result<Output> {
        let timeout = self.timeout;
        let (program, mut child) = self.start()?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let finished = tokio::time::timeout(timeout, async {
            tokio::try_join!(bounded(stdout), bounded(stderr), child.wait())
        })
        .await;
        let Ok(finished) = finished else {
            // Dropping the child kills it; kill_on_drop is set above.
            return Err(Error::Process {
                program,
                detail: format!("did not finish within {}s", timeout.as_secs()),
            });
        };
        let (stdout, stderr, status) = finished?;
        Ok(Output {
            stdout,
            stderr,
            success: status.success(),
        })
    }

    /// Run the command and fail unless it exits successfully.
    ///
    /// # Errors
    /// As [`Invocation::output`], and when the command exits unsuccessfully,
    /// with the tail of its stderr.
    pub async fn run(self) -> Result<String> {
        let program = self.program.to_string_lossy().into_owned();
        let first = self
            .arguments
            .first()
            .map(|argument| argument.to_string_lossy().into_owned())
            .unwrap_or_default();
        let output = self.output().await?;
        if output.success {
            return Ok(output.stdout);
        }
        Err(Error::Process {
            program: format!("{program} {first}"),
            detail: tail(&output.stderr, 2000),
        })
    }
}

/// A child started by [`Invocation::spawn`].
///
/// Its output is collected in the background, up to the same bound as a
/// finished invocation's, and can be read while it runs. Dropping the handle
/// kills the child.
pub struct Running {
    program: String,
    child: tokio::process::Child,
    stdout: Arc<Mutex<Vec<u8>>>,
    stderr: Arc<Mutex<Vec<u8>>>,
    readers: [tokio::task::JoinHandle<()>; 2],
}

/// How a spawned child ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    /// It exited by itself, successfully or not, before it was killed.
    Exited { success: bool },
    /// It was still running and was terminated by SIGKILL.
    Killed,
}

impl Running {
    /// The operating system's process ID, while the child has one.
    #[must_use]
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    /// What the child has written to stderr so far.
    #[must_use]
    pub fn stderr(&self) -> String {
        read(&self.stderr)
    }

    /// Whether the child has exited, and if so whether successfully.
    ///
    /// # Errors
    /// Fails when the child's status cannot be read.
    pub fn exited(&mut self) -> Result<Option<bool>> {
        Ok(self.child.try_wait()?.map(|status| status.success()))
    }

    /// Kill the child with SIGKILL unless it has already exited, reap it,
    /// and return how it ended with everything it wrote.
    ///
    /// # Errors
    /// Fails when the child cannot be signalled or reaped, or when it ended
    /// some other way than exiting or being killed by SIGKILL.
    pub async fn kill(mut self) -> Result<(Ending, Output)> {
        let ending = if let Some(status) = self.child.try_wait()? {
            Ending::Exited {
                success: status.success(),
            }
        } else {
            self.child.start_kill()?;
            let status = self.child.wait().await?;
            if !killed_by_sigkill(status) {
                return Err(Error::Process {
                    program: self.program,
                    detail: format!("did not end by SIGKILL: {status}"),
                });
            }
            Ending::Killed
        };
        let output = self
            .drain(matches!(ending, Ending::Exited { success: true }))
            .await;
        Ok((ending, output))
    }

    /// Wait up to `timeout` for the child to exit by itself.
    ///
    /// # Errors
    /// Fails when the child cannot be reaped or does not exit in time, in
    /// which case it is killed.
    pub async fn wait(mut self, timeout: Duration) -> Result<Output> {
        let Ok(status) = tokio::time::timeout(timeout, self.child.wait()).await else {
            return Err(Error::Process {
                program: self.program,
                detail: format!("did not finish within {}s", timeout.as_secs()),
            });
        };
        let success = status?.success();
        Ok(self.drain(success).await)
    }

    async fn drain(self, success: bool) -> Output {
        for reader in self.readers {
            let _ = reader.await;
        }
        Output {
            stdout: read(&self.stdout),
            stderr: read(&self.stderr),
            success,
        }
    }
}

#[cfg(unix)]
fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal() == Some(9)
}

#[cfg(not(unix))]
fn killed_by_sigkill(status: std::process::ExitStatus) -> bool {
    !status.success()
}

fn read(buffer: &Mutex<Vec<u8>>) -> String {
    let bytes = buffer.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Append `stream` to `buffer` until it closes, keeping at most the bound.
async fn collect(stream: Option<impl AsyncRead + Unpin>, buffer: Arc<Mutex<Vec<u8>>>) {
    let Some(mut stream) = stream else {
        return;
    };
    let mut chunk = [0; 8192];
    while let Ok(read) = stream.read(&mut chunk).await {
        if read == 0 {
            return;
        }
        let mut kept = buffer.lock().unwrap_or_else(PoisonError::into_inner);
        let room = usize::try_from(MAX_OUTPUT)
            .unwrap_or(usize::MAX)
            .saturating_sub(kept.len());
        kept.extend_from_slice(chunk.get(..read.min(room)).unwrap_or_default());
    }
}

async fn bounded(stream: Option<impl AsyncRead + Unpin>) -> std::io::Result<String> {
    let Some(mut stream) = stream else {
        return Ok(String::new());
    };
    let mut kept = Vec::new();
    (&mut stream)
        .take(MAX_OUTPUT)
        .read_to_end(&mut kept)
        .await?;
    tokio::io::copy(&mut stream, &mut tokio::io::sink()).await?;
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

/// The subset of `variables` a child process may inherit.
fn inherited(variables: impl Iterator<Item = (OsString, OsString)>) -> Vec<(OsString, OsString)> {
    variables
        .filter(|(name, _)| INHERITED.iter().any(|allowed| name == allowed))
        .collect()
}

/// The last `limit` bytes of `text`, cut on a character boundary.
#[must_use]
pub fn tail(text: &str, limit: usize) -> String {
    let mut start = text.len().saturating_sub(limit);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].trim().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn children_inherit_only_the_allowlist() {
        let variables = [
            ("PATH", "/bin"),
            ("KUBECONFIG", "/home/me/.kube/config"),
            ("DOCKER_HOST", "unix:///run/docker.sock"),
            ("AWS_SECRET_ACCESS_KEY", "never"),
        ]
        .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        let kept = inherited(variables.into_iter());
        let names = kept
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(names, ["PATH", "DOCKER_HOST"]);
    }

    #[test]
    fn tails_respect_character_boundaries() {
        assert_eq!(tail("abcdef", 3), "def");
        assert_eq!(tail("ab", 10), "ab");
        assert_eq!(tail("aé", 1), "");
    }

    #[tokio::test]
    async fn a_slow_child_is_killed_at_its_deadline() -> Result<()> {
        let started = std::time::Instant::now();
        let outcome = Invocation::new("sleep")
            .arg("30")
            .timeout(Duration::from_millis(200))
            .output()
            .await;
        assert!(matches!(outcome, Err(Error::Process { .. })));
        assert!(started.elapsed() < Duration::from_secs(10));
        Ok(())
    }

    #[tokio::test]
    async fn a_spawned_child_is_killed_by_sigkill_with_its_output_so_far() -> Result<()> {
        let mut running = Invocation::new("sh")
            .args(["-c", "echo started >&2; exec sleep 30"])
            .spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !running.stderr().contains("started") && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(running.exited()?, None);
        let (ending, output) = running.kill().await?;
        assert_eq!(ending, Ending::Killed);
        assert_eq!(output.stderr.trim(), "started");
        assert!(!output.success);
        Ok(())
    }

    #[tokio::test]
    async fn a_spawned_child_that_already_exited_is_not_killed() -> Result<()> {
        let running = Invocation::new("sh")
            .args(["-c", "echo done; exit 0"])
            .spawn()?;
        let output = running.wait(Duration::from_secs(10)).await?;
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "done");
        let mut failed = Invocation::new("sh").args(["-c", "exit 4"]).spawn()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while failed.exited()?.is_none() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (ending, _) = failed.kill().await?;
        assert_eq!(ending, Ending::Exited { success: false });
        Ok(())
    }

    #[tokio::test]
    async fn a_failing_child_reports_its_stderr() -> Result<()> {
        let outcome = Invocation::new("sh")
            .args(["-c", "echo boom >&2; exit 3"])
            .run()
            .await;
        let Err(Error::Process { detail, .. }) = outcome else {
            return Err(Error::Case("expected a process failure".into()));
        };
        assert_eq!(detail, "boom");
        Ok(())
    }
}
