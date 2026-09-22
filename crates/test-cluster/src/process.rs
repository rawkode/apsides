//! Bounded invocations of `kind` and `docker`, the two tools with no API the
//! runner can call directly.
//!
//! Each child gets a cleared environment plus an allowlist, a deadline after
//! which it is killed, and a cap on how much output is kept. `KUBECONFIG` is
//! deliberately not on the allowlist: every `kind` call that touches a
//! kubeconfig names the runner's private one.

use crate::{Error, Result};
use std::{
    ffi::{OsStr, OsString},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt as _};

/// Variables a child may inherit: enough to find the tools and reach the
/// Docker daemon the caller already selected, nothing that selects a cluster.
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
    "KIND_EXPERIMENTAL_PROVIDER",
];

/// Output beyond this is drained and dropped rather than kept in memory.
const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

pub struct Invocation {
    program: &'static str,
    arguments: Vec<OsString>,
    stdin: Option<PathBuf>,
    timeout: Duration,
}

pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

impl Invocation {
    pub const fn new(program: &'static str) -> Self {
        Self {
            program,
            arguments: Vec::new(),
            stdin: None,
            timeout: Duration::from_secs(120),
        }
    }

    pub fn arg(mut self, argument: impl AsRef<OsStr>) -> Self {
        self.arguments.push(argument.as_ref().to_owned());
        self
    }

    pub fn args<I, S>(mut self, arguments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.arguments
            .extend(arguments.into_iter().map(|item| item.as_ref().to_owned()));
        self
    }

    pub fn stdin(mut self, path: PathBuf) -> Self {
        self.stdin = Some(path);
        self
    }

    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Run the command and return its output whether or not it succeeded.
    pub async fn output(self) -> Result<Output> {
        let mut command = tokio::process::Command::new(self.program);
        command
            .args(&self.arguments)
            .env_clear()
            .envs(inherited(std::env::vars_os()))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match &self.stdin {
            Some(path) => command.stdin(std::fs::File::open(path)?),
            None => command.stdin(Stdio::null()),
        };
        let mut child = command.spawn().map_err(|error| Error::Process {
            program: self.program.into(),
            detail: format!("could not start: {error}"),
        })?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let finished = tokio::time::timeout(self.timeout, async {
            tokio::try_join!(bounded(stdout), bounded(stderr), child.wait())
        })
        .await;
        let Ok(finished) = finished else {
            // Dropping the child kills it; kill_on_drop is set above.
            return Err(Error::Process {
                program: self.program.into(),
                detail: format!("did not finish within {}s", self.timeout.as_secs()),
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
    pub async fn run(self) -> Result<String> {
        let program = self.program;
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
