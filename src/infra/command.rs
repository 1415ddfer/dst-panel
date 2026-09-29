//! Fakeable command execution adapter for external SteamCMD/DST operations.

use std::{
    collections::VecDeque,
    fmt,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    task::{JoinError, JoinHandle},
};

/// Default maximum captured bytes per output stream.
///
/// SteamCMD and DST can be noisy. Keeping a bounded in-memory head buffer
/// prevents a child process from exhausting panel memory while tests can still
/// inspect enough output for compatibility assertions.
const DEFAULT_OUTPUT_LIMIT_BYTES: usize = 1024 * 1024;
const POST_TIMEOUT_OUTPUT_DRAIN: Duration = Duration::from_secs(1);

/// Command process specification.
///
/// Arguments are stored as an argv array. This type deliberately has no API for
/// shell command strings, so service code cannot accidentally concatenate user
/// input into `sh -c` style commands.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandSpec {
    program: String,
    args: Vec<String>,
    current_dir: Option<PathBuf>,
    timeout: Option<Duration>,
    output_limit: usize,
}

impl CommandSpec {
    /// Creates a command spec for an executable path or program name.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            current_dir: None,
            timeout: None,
            output_limit: DEFAULT_OUTPUT_LIMIT_BYTES,
        }
    }

    /// Appends one argv argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends multiple argv arguments.
    pub fn extend_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Sets a timeout for the command.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Sets the child process working directory.
    ///
    /// This preserves Go command strings that used `cd <dir>; ./program`
    /// without introducing a shell.
    pub fn with_current_dir(mut self, current_dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(current_dir.into());
        self
    }

    /// Sets the maximum captured bytes for each output stream.
    pub fn with_output_limit(mut self, output_limit: usize) -> Self {
        self.output_limit = output_limit;
        self
    }

    /// Returns the executable path or program name.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Returns argv arguments.
    pub fn args(&self) -> &[String] {
        &self.args
    }

    /// Returns the configured working directory, if any.
    pub fn current_dir(&self) -> Option<&Path> {
        self.current_dir.as_deref()
    }

    /// Returns the configured timeout.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Returns the maximum captured bytes for stdout and stderr separately.
    pub fn output_limit(&self) -> usize {
        self.output_limit
    }
}

impl fmt::Debug for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandSpec")
            .field("program", &self.program)
            .field("args_len", &self.args.len())
            .field("has_current_dir", &self.current_dir.is_some())
            .field("timeout", &self.timeout)
            .field("output_limit", &self.output_limit)
            .finish()
    }
}

/// Captured process output.
#[derive(Clone, PartialEq, Eq)]
pub struct CommandOutput {
    pub status_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

impl fmt::Debug for CommandOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandOutput")
            .field("status_code", &self.status_code)
            .field("stdout_len", &self.stdout.len())
            .field("stderr_len", &self.stderr.len())
            .field("timed_out", &self.timed_out)
            .field("stdout_truncated", &self.stdout_truncated)
            .field("stderr_truncated", &self.stderr_truncated)
            .finish()
    }
}

impl CommandOutput {
    /// Creates a successful fake output.
    pub fn success(stdout: Vec<u8>, stderr: Vec<u8>) -> Self {
        Self {
            status_code: Some(0),
            stdout,
            stderr,
            timed_out: false,
            stdout_truncated: false,
            stderr_truncated: false,
        }
    }
}

/// Command runner errors. Messages are safe to return or log because they do
/// not include argv values or command output.
#[derive(Debug, Error)]
pub enum CommandError {
    #[error("command failed to start")]
    Spawn(#[source] std::io::Error),
    #[error("command output failed")]
    Output(#[source] std::io::Error),
    #[error("command output task failed")]
    OutputTask(#[source] JoinError),
    #[error("command timed out")]
    Timeout,
    #[error("fake command runner has no output configured")]
    FakeExhausted,
    #[error("external commands are unsupported on this platform")]
    UnsupportedPlatform,
}

/// Future returned by command runners.
pub type CommandFuture<'a> =
    Pin<Box<dyn Future<Output = Result<CommandOutput, CommandError>> + Send + 'a>>;

/// Fakeable command adapter used by services that need external processes.
pub trait CommandRunner: Send + Sync {
    fn run<'a>(&'a self, spec: CommandSpec) -> CommandFuture<'a>;
}

/// Tokio process-backed command runner for production use.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioCommandRunner;

impl TokioCommandRunner {
    pub fn new() -> Self {
        Self
    }

    /// Returns whether timeout cleanup can terminate descendants of a command.
    ///
    /// Write-heavy operations such as SteamCMD updates and DST starts must not
    /// keep running after the HTTP request reports a timeout. Windows commands
    /// are placed in a Job Object that also terminates descendants on close.
    pub fn process_tree_cleanup_supported() -> bool {
        process_tree_cleanup_supported()
    }

    async fn run_inner(&self, spec: CommandSpec) -> Result<CommandOutput, CommandError> {
        let timeout_ms = spec.timeout.map(|timeout| timeout.as_millis());
        if !Self::process_tree_cleanup_supported() {
            tracing::warn!(
                program = %spec.program,
                timeout_ms = ?timeout_ms,
                "refusing external command because process-tree cleanup is unsupported"
            );
            return Err(CommandError::UnsupportedPlatform);
        }

        tracing::info!(
            program = %spec.program,
            args_len = spec.args.len(),
            has_current_dir = spec.current_dir.is_some(),
            timeout_ms = ?timeout_ms,
            "running external command"
        );

        let mut command = tokio::process::Command::new(&spec.program);
        if let Some(current_dir) = &spec.current_dir {
            command.current_dir(current_dir);
        }
        command
            .args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_isolation(&mut command);

        #[cfg(windows)]
        let job = WindowsJob::new().map_err(CommandError::Spawn)?;

        let mut child = command.spawn().map_err(|err| {
            tracing::warn!(program = %spec.program, error = %err, "external command failed to start");
            CommandError::Spawn(err)
        })?;
        let child_id = child.id();
        #[cfg(windows)]
        if let Err(error) = job.assign(child_id) {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(CommandError::Spawn(error));
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(read_optional_bounded(stdout, spec.output_limit, "stdout"));
        let stderr_task = tokio::spawn(read_optional_bounded(stderr, spec.output_limit, "stderr"));

        let status = if let Some(timeout) = spec.timeout {
            match tokio::time::timeout(timeout, child.wait()).await {
                Ok(status) => status.map_err(CommandError::Output)?,
                Err(_) => {
                    tracing::warn!(program = %spec.program, "external command timed out");
                    #[cfg(windows)]
                    job.terminate();
                    terminate_child(&mut child, child_id, &spec.program).await;
                    drain_output_after_timeout(stdout_task, stderr_task, &spec.program).await;
                    return Err(CommandError::Timeout);
                }
            }
        } else {
            child.wait().await.map_err(CommandError::Output)?
        };

        #[cfg(windows)]
        drop(job);

        let stdout = stdout_task
            .await
            .map_err(CommandError::OutputTask)?
            .map_err(CommandError::Output)?;
        let stderr = stderr_task
            .await
            .map_err(CommandError::OutputTask)?
            .map_err(CommandError::Output)?;
        let status_code = status.code();
        tracing::info!(
            program = %spec.program,
            status_code = ?status_code,
            success = status.success(),
            stdout_len = stdout.bytes.len(),
            stderr_len = stderr.bytes.len(),
            stdout_truncated = stdout.truncated,
            stderr_truncated = stderr.truncated,
            "external command exited"
        );

        Ok(CommandOutput {
            status_code,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            timed_out: false,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        })
    }
}

struct BoundedBytes {
    bytes: Vec<u8>,
    truncated: bool,
}

type OutputTask = JoinHandle<Result<BoundedBytes, std::io::Error>>;

async fn read_optional_bounded<R>(
    reader: Option<R>,
    limit: usize,
    stream_name: &'static str,
) -> Result<BoundedBytes, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    match reader {
        Some(reader) => read_bounded(reader, limit, stream_name).await,
        None => Ok(BoundedBytes {
            bytes: Vec::new(),
            truncated: false,
        }),
    }
}

async fn read_bounded<R>(
    mut reader: R,
    limit: usize,
    stream_name: &'static str,
) -> Result<BoundedBytes, std::io::Error>
where
    R: AsyncRead + Unpin,
{
    let mut bytes = Vec::with_capacity(limit.min(8192));
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];

    loop {
        let read = reader.read(&mut buffer).await?;
        if read == 0 {
            break;
        }

        let remaining = limit.saturating_sub(bytes.len());
        if remaining >= read {
            bytes.extend_from_slice(&buffer[..read]);
        } else {
            if remaining > 0 {
                bytes.extend_from_slice(&buffer[..remaining]);
            }
            if !truncated {
                tracing::warn!(
                    stream = stream_name,
                    limit,
                    "truncated external command output"
                );
            }
            truncated = true;
        }
    }

    Ok(BoundedBytes { bytes, truncated })
}

async fn drain_output_after_timeout(
    mut stdout_task: OutputTask,
    mut stderr_task: OutputTask,
    program: &str,
) {
    let deadline = tokio::time::sleep(POST_TIMEOUT_OUTPUT_DRAIN);
    tokio::pin!(deadline);
    let mut stdout_done = false;
    let mut stderr_done = false;

    loop {
        if stdout_done && stderr_done {
            return;
        }

        tokio::select! {
            _ = &mut deadline => {
                if !stdout_done {
                    stdout_task.abort();
                }
                if !stderr_done {
                    stderr_task.abort();
                }
                tracing::warn!(
                    program = %program,
                    drain_timeout_ms = POST_TIMEOUT_OUTPUT_DRAIN.as_millis(),
                    "aborted external command output drain after timeout"
                );
                return;
            }
            result = &mut stdout_task, if !stdout_done => {
                log_output_task_result("stdout", result, program);
                stdout_done = true;
            }
            result = &mut stderr_task, if !stderr_done => {
                log_output_task_result("stderr", result, program);
                stderr_done = true;
            }
        }
    }
}

fn log_output_task_result(
    stream: &'static str,
    result: Result<Result<BoundedBytes, std::io::Error>, JoinError>,
    program: &str,
) {
    match result {
        Ok(Ok(_)) => {}
        Ok(Err(error)) => {
            tracing::warn!(
                program = %program,
                stream,
                error = %error,
                "failed to drain external command output after timeout"
            );
        }
        Err(error) if error.is_cancelled() => {}
        Err(error) => {
            tracing::warn!(
                program = %program,
                stream,
                error = %error,
                "external command output drain task failed after timeout"
            );
        }
    }
}

#[cfg(unix)]
fn configure_process_isolation(command: &mut tokio::process::Command) {
    // Put the child in its own process group so timeout cleanup can terminate
    // SteamCMD/DST helper processes that inherit the group.
    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_process_isolation(_command: &mut tokio::process::Command) {
    // Windows process isolation is established by WindowsJob after spawn.
}

#[cfg(unix)]
fn process_tree_cleanup_supported() -> bool {
    true
}

#[cfg(windows)]
fn process_tree_cleanup_supported() -> bool {
    true
}

#[cfg(all(not(unix), not(windows)))]
fn process_tree_cleanup_supported() -> bool {
    false
}

async fn terminate_child(child: &mut tokio::process::Child, child_id: Option<u32>, program: &str) {
    terminate_process_group(child_id, program);
    if let Err(error) = child.start_kill() {
        tracing::warn!(program = %program, error = %error, "failed to signal external command");
    }
    if let Err(error) = child.wait().await {
        tracing::warn!(program = %program, error = %error, "failed to reap external command after timeout");
    }
}

#[cfg(unix)]
fn terminate_process_group(child_id: Option<u32>, program: &str) {
    let Some(child_id) = child_id else {
        tracing::warn!(program = %program, "cannot kill process group without child pid");
        return;
    };

    // SAFETY: `child_id` comes from the just-spawned child. The command was
    // started with `process_group(0)`, so its process group id equals its pid.
    let result = unsafe { libc::killpg(child_id as libc::pid_t, libc::SIGKILL) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        tracing::warn!(program = %program, error = %error, "failed to kill external command process group");
    }
}

#[cfg(windows)]
fn terminate_process_group(_child_id: Option<u32>, _program: &str) {}

#[cfg(all(not(unix), not(windows)))]
fn terminate_process_group(_child_id: Option<u32>, program: &str) {
    tracing::warn!(
        program = %program,
        "process-group timeout cleanup is unavailable on this platform"
    );
}

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
// HANDLE belongs solely to this job; WindowsJob closes it in Drop.
unsafe impl Send for WindowsJob {}

#[cfg(windows)]
impl WindowsJob {
    fn new() -> std::io::Result<Self> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };

        // SAFETY: null security attributes and name request a private job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let job = Self(handle);
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits is a valid structure of the size supplied here.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const std::ffi::c_void,
                std::mem::size_of_val(&limits) as u32,
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(job)
    }

    fn assign(&self, child_id: Option<u32>) -> std::io::Result<()> {
        use windows_sys::Win32::System::{
            JobObjects::AssignProcessToJobObject,
            Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
        };

        let pid = child_id.ok_or_else(|| std::io::Error::other("child process ID unavailable"))?;
        // SAFETY: pid identifies the child just spawned by this command.
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: both handles are valid for the duration of this call.
        let ok = unsafe { AssignProcessToJobObject(self.0, process) };
        let error = if ok == 0 {
            Some(std::io::Error::last_os_error())
        } else {
            None
        };
        // SAFETY: process was opened above and is no longer needed.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(process) };
        match error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn terminate(&self) {
        // SAFETY: self.0 is a live Job Object handle.
        if unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) } == 0 {
            tracing::warn!(error = %std::io::Error::last_os_error(), "failed to terminate command job");
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        // SAFETY: this is the unique owner of the handle.
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.0) };
    }
}

impl CommandRunner for TokioCommandRunner {
    fn run<'a>(&'a self, spec: CommandSpec) -> CommandFuture<'a> {
        Box::pin(async move { self.run_inner(spec).await })
    }
}

/// Test command runner that records specs and returns preconfigured outputs.
#[derive(Debug, Clone, Default)]
pub struct FakeCommandRunner {
    outputs: Arc<Mutex<VecDeque<CommandOutput>>>,
    calls: Arc<Mutex<Vec<CommandSpec>>>,
}

impl FakeCommandRunner {
    pub fn new(outputs: Vec<CommandOutput>) -> Self {
        Self {
            outputs: Arc::new(Mutex::new(outputs.into())),
            calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn calls(&self) -> Vec<CommandSpec> {
        self.calls
            .lock()
            .expect("fake command calls poisoned")
            .clone()
    }
}

impl CommandRunner for FakeCommandRunner {
    fn run<'a>(&'a self, spec: CommandSpec) -> CommandFuture<'a> {
        let result = {
            self.calls
                .lock()
                .expect("fake command calls poisoned")
                .push(spec);
            self.outputs
                .lock()
                .expect("fake command outputs poisoned")
                .pop_front()
                .ok_or(CommandError::FakeExhausted)
        };
        Box::pin(async move { result })
    }
}
