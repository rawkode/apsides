//! A synchronous subprocess runner with a deadline, bounded output capture
//! and redacted live streaming.

use crate::redact::{TAIL_LINES, bounded, diagnostic, redact};
use std::{
    collections::VecDeque,
    ffi::OsString,
    io::{self, Read, Write},
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError},
    thread,
    time::{Duration, Instant},
};

/// How many bytes of each output stream a run retains.
const MAX_RETAINED_BYTES: usize = 16 * 1024 * 1024;
/// How often a running child is polled.
const POLL: Duration = Duration::from_millis(100);
/// How long the `on_timeout` clean-up command may take.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(60);
/// How long output still arriving after the child exited is waited for. A
/// grandchild holding the pipes open must not hang the build.
const DRAIN_GRACE: Duration = Duration::from_secs(5);

/// The environment a child runs with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Env {
    /// This process's environment plus these variables.
    Inherit(Vec<(String, String)>),
    /// Exactly these variables.
    Clear(Vec<(String, String)>),
}

/// One command to run: stdin is null, stdout and stderr are read.
#[derive(Clone, Debug)]
pub struct Run {
    program: PathBuf,
    args: Vec<OsString>,
    cwd: Option<PathBuf>,
    env: Env,
    timeout: Duration,
    on_timeout: Option<Vec<OsString>>,
}

/// What a successful [`Run::capture`] read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

/// Why a run failed, with the raw tail of what it printed.
#[derive(Debug)]
pub struct Failure {
    pub program: String,
    pub cause: Cause,
    /// The last [`TAIL_LINES`] lines of output, unredacted.
    pub tail: String,
}

#[derive(Debug)]
pub enum Cause {
    Spawn(io::Error),
    Exit(ExitStatus),
    Timeout(Duration),
}

impl Failure {
    /// "{what} failed: {program} {cause}", then the redacted last
    /// `tail_lines` lines of output when there are any.
    #[must_use]
    pub fn describe(&self, what: &str, tail_lines: usize) -> String {
        let cause = match &self.cause {
            Cause::Spawn(error) => format!("could not start: {error}"),
            Cause::Exit(status) => match (status.code(), status.signal()) {
                (Some(code), _) => format!("exited with status {code}"),
                (None, Some(signal)) => format!("was killed by signal {signal}"),
                (None, None) => format!("ended with {status}"),
            },
            Cause::Timeout(timeout) => {
                format!("did not finish within {} s", timeout.as_secs())
            }
        };
        let mut message = format!("{what} failed: {} {cause}", self.program);
        if !self.tail.trim().is_empty() {
            message.push('\n');
            message.push_str(&bounded(&diagnostic(&self.tail), tail_lines));
        }
        message
    }

    /// The exit code, when the child exited normally.
    #[must_use]
    pub fn code(&self) -> Option<i32> {
        match &self.cause {
            Cause::Exit(status) => status.code(),
            Cause::Spawn(_) | Cause::Timeout(_) => None,
        }
    }

    /// Whether the run hit its deadline.
    #[must_use]
    pub const fn timed_out(&self) -> bool {
        matches!(self.cause, Cause::Timeout(_))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

enum Event {
    Data(Stream, Vec<u8>),
    Closed(Stream),
}

/// What a run collects from its output while it runs.
trait Collector {
    fn data(&mut self, stream: Stream, bytes: &[u8]);
    fn closed(&mut self, stream: Stream);
}

/// Keeps the first [`MAX_RETAINED_BYTES`] of each stream.
#[derive(Default)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl Collector for Capture {
    fn data(&mut self, stream: Stream, bytes: &[u8]) {
        let buffer = match stream {
            Stream::Stdout => &mut self.stdout,
            Stream::Stderr => &mut self.stderr,
        };
        let room = MAX_RETAINED_BYTES.saturating_sub(buffer.len());
        buffer.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    fn closed(&mut self, _: Stream) {}
}

impl Capture {
    fn tail(&self) -> String {
        let stderr = String::from_utf8_lossy(&self.stderr);
        let text = if stderr.trim().is_empty() {
            String::from_utf8_lossy(&self.stdout)
        } else {
            stderr
        };
        bounded(&text, TAIL_LINES)
    }
}

/// Merges both streams into lines, writes each redacted to `sink` and keeps
/// the last [`TAIL_LINES`] raw lines.
struct Lines<'a> {
    sink: &'a mut dyn Write,
    partial: [Vec<u8>; 2],
    tail: VecDeque<String>,
}

impl Lines<'_> {
    fn emit(&mut self, line: &[u8]) {
        let line = String::from_utf8_lossy(line).into_owned();
        // A closed or failing terminal must not fail the build it reports on.
        let _ = writeln!(self.sink, "{}", redact(&line));
        if self.tail.len() == TAIL_LINES {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    const fn index(stream: Stream) -> usize {
        match stream {
            Stream::Stdout => 0,
            Stream::Stderr => 1,
        }
    }
}

impl Collector for Lines<'_> {
    fn data(&mut self, stream: Stream, bytes: &[u8]) {
        let index = Self::index(stream);
        let mut pending = std::mem::take(&mut self.partial[index]);
        pending.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(end) = pending[start..].iter().position(|&byte| byte == b'\n') {
            self.emit(&pending[start..start + end]);
            start += end + 1;
        }
        let mut rest = pending.split_off(start);
        rest.truncate(MAX_RETAINED_BYTES);
        self.partial[index] = rest;
    }

    fn closed(&mut self, stream: Stream) {
        let rest = std::mem::take(&mut self.partial[Self::index(stream)]);
        if !rest.is_empty() {
            self.emit(&rest);
        }
    }
}

impl Run {
    /// `program` with `args`, inheriting the environment, with no working
    /// directory change and a 60 s deadline.
    pub fn new<I, S>(program: impl Into<PathBuf>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
            cwd: None,
            env: Env::Inherit(Vec::new()),
            timeout: Duration::from_secs(60),
            on_timeout: None,
        }
    }

    #[must_use]
    pub fn cwd(mut self, cwd: &Path) -> Self {
        self.cwd = Some(cwd.to_path_buf());
        self
    }

    #[must_use]
    pub fn env(mut self, env: Env) -> Self {
        self.env = env;
        self
    }

    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Arguments to run with the same program when the deadline passes,
    /// before the child is killed: `rm --force <name>` for an engine.
    #[must_use]
    pub fn on_timeout(mut self, args: Vec<OsString>) -> Self {
        self.on_timeout = Some(args);
        self
    }

    /// Runs to completion and returns what it printed.
    ///
    /// # Errors
    /// Fails when the program cannot start, exits unsuccessfully or misses
    /// its deadline; the failure carries the tail of stderr (or of stdout,
    /// when stderr is blank).
    pub fn capture(&self) -> Result<Captured, Failure> {
        let mut capture = Capture::default();
        let status = self.execute(&mut capture);
        let tail = capture.tail();
        match status {
            Ok(status) if status.success() => Ok(Captured {
                stdout: capture.stdout,
                stderr: String::from_utf8_lossy(&capture.stderr).into_owned(),
            }),
            Ok(status) => Err(self.failure(Cause::Exit(status), tail)),
            Err(cause) => Err(self.failure(cause, tail)),
        }
    }

    /// Runs to completion, writing every output line, redacted, to this
    /// process's stderr as it arrives.
    ///
    /// # Errors
    /// As [`Run::capture`]; the failure carries the last lines of the merged
    /// output.
    pub fn stream(&self) -> Result<(), Failure> {
        self.stream_to(&mut io::stderr())
    }

    /// [`Run::stream`] into `sink`.
    pub(crate) fn stream_to(&self, sink: &mut dyn Write) -> Result<(), Failure> {
        let mut lines = Lines {
            sink,
            partial: [Vec::new(), Vec::new()],
            tail: VecDeque::new(),
        };
        let status = self.execute(&mut lines);
        let tail = Vec::from(lines.tail).join("\n");
        match status {
            Ok(status) if status.success() => Ok(()),
            Ok(status) => Err(self.failure(Cause::Exit(status), tail)),
            Err(cause) => Err(self.failure(cause, tail)),
        }
    }

    fn failure(&self, cause: Cause, tail: String) -> Failure {
        Failure {
            program: self.program.display().to_string(),
            cause,
            tail,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = &self.cwd {
            command.current_dir(cwd);
        }
        match &self.env {
            Env::Inherit(extra) => {
                command.envs(extra.iter().map(|(key, value)| (key, value)));
            }
            Env::Clear(only) => {
                command
                    .env_clear()
                    .envs(only.iter().map(|(key, value)| (key, value)));
            }
        }
        command
    }

    /// Starts the child, retrying briefly while its executable is still open
    /// for writing somewhere (`ETXTBSY`), which a just-written program can be
    /// while another thread forks.
    fn spawn(&self) -> io::Result<Child> {
        let mut attempts = 0;
        loop {
            match self.command().spawn() {
                Err(error)
                    if error.kind() == io::ErrorKind::ExecutableFileBusy && attempts < 10 =>
                {
                    attempts += 1;
                    thread::sleep(Duration::from_millis(50));
                }
                spawned => return spawned,
            }
        }
    }

    fn execute(&self, collector: &mut dyn Collector) -> Result<ExitStatus, Cause> {
        let mut child = self.spawn().map_err(Cause::Spawn)?;
        let (sender, events) = mpsc::channel();
        let mut open = 0;
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, Stream::Stdout, sender.clone());
            open += 1;
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Stream::Stderr, sender);
            open += 1;
        }
        let deadline = Instant::now() + self.timeout;
        let mut exited: Option<(ExitStatus, Instant)> = None;
        loop {
            if let (0, Some((status, _))) = (open, exited) {
                return Ok(status);
            }
            if open == 0 {
                thread::sleep(POLL);
            } else {
                match events.recv_timeout(POLL) {
                    Ok(Event::Data(stream, bytes)) => collector.data(stream, &bytes),
                    Ok(Event::Closed(stream)) => {
                        collector.closed(stream);
                        open -= 1;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => open = 0,
                }
            }
            match exited {
                Some((status, at)) => {
                    if at.elapsed() >= DRAIN_GRACE {
                        return Ok(status);
                    }
                }
                None => match child.try_wait() {
                    Ok(Some(status)) => exited = Some((status, Instant::now())),
                    Ok(None) if Instant::now() >= deadline => {
                        self.stop(&mut child, &events, collector);
                        return Err(Cause::Timeout(self.timeout));
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(Cause::Spawn(error));
                    }
                },
            }
        }
    }

    /// Runs the clean-up command, kills the child and collects what output
    /// is already buffered.
    fn stop(&self, child: &mut Child, events: &Receiver<Event>, collector: &mut dyn Collector) {
        if let Some(args) = &self.on_timeout {
            // Best effort: the deadline already failed the run.
            let _ = Self::new(&self.program, args.clone())
                .timeout(CLEANUP_TIMEOUT)
                .capture();
        }
        let _ = child.kill();
        let _ = child.wait();
        while let Ok(event) = events.try_recv() {
            match event {
                Event::Data(stream, bytes) => collector.data(stream, &bytes),
                Event::Closed(stream) => collector.closed(stream),
            }
        }
    }
}

fn spawn_reader(mut pipe: impl Read + Send + 'static, stream: Stream, sender: mpsc::Sender<Event>) {
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 64 * 1024];
        loop {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if sender
                        .send(Event::Data(stream, buffer[..count].to_vec()))
                        .is_err()
                    {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = sender.send(Event::Closed(stream));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn sh(script: &str) -> Run {
        Run::new("/bin/sh", ["-c", script])
    }

    #[test]
    fn capture_returns_stdout_bytes_and_stderr_text() -> TestResult {
        let captured = sh("printf 'a\\0b'; echo warn >&2")
            .capture()
            .map_err(|f| f.describe("probe", 5))?;
        assert_eq!(captured.stdout, b"a\0b");
        assert_eq!(captured.stderr, "warn\n");
        Ok(())
    }

    #[test]
    fn the_environment_is_cleared_or_extended() -> TestResult {
        let cleared = sh("echo \"$ONLY:$HOME\"")
            .env(Env::Clear(vec![("ONLY".into(), "1".into())]))
            .capture()
            .map_err(|f| f.describe("env", 5))?;
        assert_eq!(cleared.stdout, b"1:\n");
        let inherited = sh("echo \"$EXTRA\"")
            .env(Env::Inherit(vec![("EXTRA".into(), "x y".into())]))
            .capture()
            .map_err(|f| f.describe("env", 5))?;
        assert_eq!(inherited.stdout, b"x y\n");
        Ok(())
    }

    #[test]
    fn streamed_lines_are_redacted_but_the_tail_stays_raw() -> TestResult {
        let script = "i=0; while [ $i -lt 250 ]; do echo line $i; i=$((i+1)); done; \
                      echo 'Authorization: Bearer sekret-value-1' >&2; printf 'no newline'; exit 3";
        let mut sink = Vec::new();
        let failure = sh(script)
            .stream_to(&mut sink)
            .err()
            .ok_or("the run succeeded")?;
        let streamed = String::from_utf8(sink)?;
        assert!(streamed.contains("Authorization: Bearer [REDACTED]\n"));
        assert!(!streamed.contains("sekret-value-1"));
        assert!(streamed.contains("line 0\n") && streamed.ends_with("no newline\n"));
        assert!(failure.tail.contains("sekret-value-1"));
        assert_eq!(failure.tail.lines().count(), TAIL_LINES);
        assert_eq!(failure.code(), Some(3));
        let message = failure.describe("the controller compile", 2);
        assert!(
            message.starts_with("the controller compile failed: /bin/sh exited with status 3\n")
        );
        assert!(!message.contains("sekret-value-1"));
        assert_eq!(message.lines().count(), 3);
        Ok(())
    }

    #[test]
    fn a_timeout_kills_the_child_and_runs_the_clean_up() -> TestResult {
        let directory = tempfile::tempdir()?;
        let marker = directory.path().join("cleaned");
        let script = format!(
            "if [ \"$1\" = clean ]; then touch '{}'; exit 0; fi; echo started; sleep 30",
            marker.display()
        );
        let started = Instant::now();
        let failure = Run::new("/bin/sh", ["-c", &script, "sh", "run"])
            .timeout(Duration::from_millis(300))
            .on_timeout(vec![
                "-c".into(),
                script.clone().into(),
                "sh".into(),
                "clean".into(),
            ])
            .capture()
            .err()
            .ok_or("the run finished")?;
        assert!(failure.timed_out());
        assert!(started.elapsed() < Duration::from_secs(20));
        assert!(marker.exists(), "the clean-up command did not run");
        assert!(
            failure
                .describe("probe", 5)
                .contains("did not finish within 0 s")
        );
        Ok(())
    }

    #[test]
    fn a_spawn_failure_names_the_program() -> TestResult {
        let failure = Run::new("/nonexistent/tool", ["--version"])
            .capture()
            .err()
            .ok_or("a missing program started")?;
        let message = failure.describe("host tool check", 5);
        assert!(message.starts_with("host tool check failed: /nonexistent/tool could not start:"));
        Ok(())
    }
}
