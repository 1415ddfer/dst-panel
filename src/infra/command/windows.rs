//! Windows replacement for the two `screen` command shapes used by DST.

use std::{
    collections::HashMap,
    io::{self, Write},
    path::Path,
    process::{ChildStdin, Command, Stdio},
    sync::{Mutex, OnceLock},
};

use super::{CommandError, CommandOutput, CommandSpec};

static SESSIONS: OnceLock<Mutex<HashMap<String, ChildStdin>>> = OnceLock::new();

fn sessions() -> &'static Mutex<HashMap<String, ChildStdin>> {
    SESSIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) fn run_screen(spec: &CommandSpec) -> Result<CommandOutput, CommandError> {
    let args = spec.args();
    match args {
        [start, multi, session_flag, session, program, rest @ ..]
            if start == "-d" && multi == "-m" && session_flag == "-S" =>
        {
            launch(spec, session, program, rest)
        }
        [
            session_flag,
            session,
            page_flag,
            page,
            execute_flag,
            stuff,
            command,
        ] if session_flag == "-S"
            && page_flag == "-p"
            && page == "0"
            && execute_flag == "-X"
            && stuff == "stuff" =>
        {
            send(session, command)
        }
        _ => Err(CommandError::Output(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported DST console command",
        ))),
    }
}

fn launch(
    spec: &CommandSpec,
    session: &str,
    program: &str,
    args: &[String],
) -> Result<CommandOutput, CommandError> {
    let current_dir = spec.current_dir().ok_or_else(|| {
        CommandError::Spawn(io::Error::new(
            io::ErrorKind::InvalidInput,
            "DST executable directory is missing",
        ))
    })?;
    let program = Path::new(program);
    let program = if program.is_absolute() {
        program.to_path_buf()
    } else {
        current_dir.join(program)
    };
    let mut child = Command::new(&program)
        .args(args)
        .current_dir(current_dir)
        .env("SteamAppId", "322330")
        .env("SteamGameId", "322330")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(CommandError::Spawn)?;
    if let Some(status) = child.try_wait().map_err(CommandError::Spawn)? {
        return Err(CommandError::Spawn(io::Error::other(format!(
            "DST process exited during launch with status {status}"
        ))));
    }
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| CommandError::Spawn(io::Error::other("DST process has no console input")))?;
    // Dropping std::process::Child does not terminate the server. The pipe
    // remains available for later console commands while this panel runs.
    let pid = child.id();
    sessions()
        .lock()
        .map_err(|_| CommandError::Output(io::Error::other("DST session registry poisoned")))?
        .insert(session.to_owned(), stdin);
    tracing::info!(session, pid, program = %program.display(), "started Windows DST shard");
    Ok(CommandOutput::success(Vec::new(), Vec::new()))
}

fn send(session: &str, command: &str) -> Result<CommandOutput, CommandError> {
    let mut sessions = sessions()
        .lock()
        .map_err(|_| CommandError::Output(io::Error::other("DST session registry poisoned")))?;
    let stdin = sessions.get_mut(session).ok_or_else(|| {
        CommandError::Output(io::Error::new(
            io::ErrorKind::NotFound,
            "DST console session is unavailable",
        ))
    })?;
    if let Err(error) = stdin
        .write_all(command.as_bytes())
        .and_then(|_| stdin.flush())
    {
        sessions.remove(session);
        return Err(CommandError::Output(error));
    }
    Ok(CommandOutput::success(Vec::new(), Vec::new()))
}
