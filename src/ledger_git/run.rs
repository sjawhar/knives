//! Running `git` for the ledger: a local command run to completion, and a
//! fetch or push bounded in time so a remote that stops answering cannot hold
//! a report, or the ledger's lock, with it.

use std::ffi::OsStr;
use std::io::Write as _;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::Duration;

use super::GitError;

/// How long one fetch or push may run, start to finish, before it is ended.
///
/// git waits with no limit of its own on a remote that takes the connection
/// and sends nothing, a connect that never completes, or an ssh session gone
/// quiet, and whatever waits on it waits too: a report, or a sweep holding
/// the ledger's lock. Each machine's ref is a small tree of small files, so
/// a minute is many times what a healthy fetch or push takes.
const TRANSPORT_DEADLINE: Duration = Duration::from_secs(60);
/// git abandons an HTTP transfer that runs below [`LOW_SPEED_BYTES`] bytes a
/// second for [`LOW_SPEED_SECONDS`] seconds, so a remote that stops answering
/// mid-transfer fails well before [`TRANSPORT_DEADLINE`].
const LOW_SPEED_BYTES: &str = "1000";
const LOW_SPEED_SECONDS: &str = "15";

/// Run `command` to completion, returning its stdout when it exits zero.
pub(super) fn run(command: &mut Command) -> Result<Vec<u8>, GitError> {
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|source| GitError::Run {
            invocation: invocation(command),
            source,
        })?;
    succeeded(command, output)
}

/// [`run`] for a command that talks to a remote: one whose HTTP transfer
/// stalls fails, and one still running at [`TRANSPORT_DEADLINE`] is ended.
pub(super) fn transport(command: &mut Command) -> Result<Vec<u8>, GitError> {
    limit_speed(command);
    run_until(command, TRANSPORT_DEADLINE)?.answer(command)
}

/// [`transport`], keeping everything git said however it ended: its exit
/// status, its stdout and its stderr.
///
/// For a command whose failure must be read, not only reported: a push
/// whose porcelain lines name each ref's fate even when git exits non-zero.
/// git runs with `LC_ALL=C`, so the messages that reading looks for are
/// git's own English ones whatever language the session speaks.
pub(super) fn transport_transcript(command: &mut Command) -> Result<Transcript, GitError> {
    limit_speed(command);
    command.env("LC_ALL", "C");
    run_until(command, TRANSPORT_DEADLINE)
}

fn limit_speed(command: &mut Command) {
    command
        .env("GIT_HTTP_LOW_SPEED_LIMIT", LOW_SPEED_BYTES)
        .env("GIT_HTTP_LOW_SPEED_TIME", LOW_SPEED_SECONDS);
}

/// Everything a bounded command said, and how it ended.
#[derive(Debug)]
pub(super) struct Transcript {
    /// How it exited; `None` when its deadline ended it first.
    pub(super) status: Option<ExitStatus>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
    deadline: Duration,
}

impl Transcript {
    /// Its stdout when it exited zero; otherwise the error [`transport`]
    /// gives for the same run.
    pub(super) fn answer(self, command: &Command) -> Result<Vec<u8>, GitError> {
        match self.status {
            None => Err(self.ended(command)),
            Some(status) => succeeded(
                command,
                Output {
                    status,
                    stdout: self.stdout,
                    stderr: self.stderr,
                },
            ),
        }
    }

    /// What went wrong, as one message: how the command ended, then its
    /// stdout and its stderr, each trimmed.
    pub(super) fn account(&self, command: &Command) -> String {
        let said = [&self.stdout, &self.stderr]
            .into_iter()
            .map(|bytes| String::from_utf8_lossy(bytes).trim().to_owned())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        match self.status {
            None if said.is_empty() => self.ended(command).to_string(),
            None => format!("{}; it had printed: {said}", self.ended(command)),
            Some(_) => GitError::Failed {
                invocation: invocation(command),
                stderr: said,
            }
            .to_string(),
        }
    }

    fn ended(&self, command: &Command) -> GitError {
        GitError::Deadline {
            invocation: invocation(command),
            deadline: self.deadline,
        }
    }
}

/// Run `command` until it exits or has run for `deadline`, ending it and
/// every process it started at the deadline; what it said either way.
///
/// The command runs in a process group of its own, so ending it reaches the
/// helpers git starts for a transport (`git-remote-https`, `ssh`): they hold
/// its output pipes, and ending git alone would leave the read of its output
/// waiting on them. The same group keeps a Ctrl-C meant for knives from
/// reaching git; a fetch or push left running that way still ends at its own
/// low-speed limit or writes only the remote's copy of a ref.
fn run_until(command: &mut Command, deadline: Duration) -> Result<Transcript, GitError> {
    use std::os::unix::process::CommandExt as _;
    let child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|source| GitError::Run {
            invocation: invocation(command),
            source,
        })?;
    let group = rustix::process::Pid::from_child(&child);
    let (finished, waiting) = std::sync::mpsc::channel::<()>();
    let (output, ended) = std::thread::scope(|scope| {
        let watchdog = scope.spawn(move || {
            let expired = matches!(
                waiting.recv_timeout(deadline),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            );
            if expired {
                // Already gone is as good as ended.
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            expired
        });
        let output = child.wait_with_output();
        drop(finished);
        let ended = watchdog
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (output, ended)
    });
    match output {
        Ok(output) => Ok(Transcript {
            status: (!ended).then_some(output.status),
            stdout: output.stdout,
            stderr: output.stderr,
            deadline,
        }),
        Err(_) if ended => Err(GitError::Deadline {
            invocation: invocation(command),
            deadline,
        }),
        Err(source) => Err(GitError::Run {
            invocation: invocation(command),
            source,
        }),
    }
}

/// [`run`], with `input` written to the command's stdin.
///
/// The input is written from its own thread: git answers as it reads, and a
/// command whose stdout pipe fills while this side is still writing would wait
/// on a reader that is itself waiting on it.
pub(super) fn run_with_input(command: &mut Command, input: &[u8]) -> Result<Vec<u8>, GitError> {
    let failed_to_run = |command: &Command| {
        let invocation = invocation(command);
        move |source| GitError::Run { invocation, source }
    };
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(failed_to_run(command))?;
    let stdin = child.stdin.take();
    let (written, output) = std::thread::scope(|scope| {
        let writer = scope.spawn(move || stdin.map_or(Ok(()), |mut stdin| stdin.write_all(input)));
        let output = child.wait_with_output();
        let written = writer
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        (written, output)
    });
    // A command that failed has usually closed its stdin too: its own error,
    // not the broken pipe, is the one worth reporting.
    let answer = succeeded(command, output.map_err(failed_to_run(command))?)?;
    written.map_err(failed_to_run(command))?;
    Ok(answer)
}

pub(super) fn succeeded(command: &Command, output: Output) -> Result<Vec<u8>, GitError> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitError::Failed {
            invocation: invocation(command),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        })
    }
}

pub(super) fn output_error(command: &Command, expected: &'static str) -> GitError {
    GitError::Output {
        invocation: invocation(command),
        expected,
    }
}

/// `command` as one line, for an error message.
pub(super) fn invocation(command: &Command) -> String {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(OsStr::to_string_lossy)
        .collect::<Vec<_>>()
        .join(" ")
}
