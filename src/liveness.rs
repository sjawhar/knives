//! Whether something is still working in a workspace: the processes using it
//! now, and where its claim was taken.
//!
//! `finish` removes a workspace. A process still inside it (an agent's shell,
//! an editor, a duplicate harness of the same session) would lose its directory
//! and any edit it has not saved. [`live_users`] reads `/proc`, so it sees what
//! this process can see: processes of this user in this PID namespace. An agent
//! box runs in its own PID namespace under a remapped user, so its processes
//! are invisible from the host and from every other box; [`this_place`] names
//! the namespace a claim was taken in, so a `finish` from another one can be
//! told apart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A process with its working directory or an open file inside a directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveUser {
    pub pid: u32,
    pub command: String,
    /// The working directory or open file that lies inside the directory.
    pub path: PathBuf,
}

impl std::fmt::Display for LiveUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "pid {} ({}) at {}",
            self.pid,
            self.command,
            self.path.display()
        )
    }
}

/// The variable a harness names its session by, and its value, as this
/// process sees them.
pub type Session<'a> = (&'a str, &'a str);

/// Programs that run a command on someone's behalf without being a harness:
/// shells and wrappers between a harness and the `knives` it runs.
const PASS_THROUGH: [&str; 19] = [
    "direnv",
    "tmux",
    "sh",
    "bash",
    "dash",
    "zsh",
    "fish",
    "ksh",
    "brush",
    "env",
    "timeout",
    "nice",
    "nohup",
    "stdbuf",
    "xargs",
    "setsid",
    "sudo",
    "script",
    "systemd-run",
];

/// The processes using `directory`, except the caller's own.
///
/// The caller's own are this process and its ancestors, and, when a harness
/// names the session (`session`), the processes its harness started that carry
/// the same session: its language servers and background jobs. The harness is
/// the nearest ancestor that is not a shell or a wrapper (`PASS_THROUGH`);
/// harnesses differ in whether their own environment carries the session, so
/// the environment cannot mark the boundary. A duplicate harness of the same
/// session is outside that harness's process tree, so it and what it started
/// are counted, even where both inherited the session from one launcher.
///
/// Empty where `/proc` cannot be read (not Linux).
pub fn live_users(directory: &Path, session: Option<Session<'_>>) -> Vec<LiveUser> {
    let Ok(directory) = directory.canonicalize() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let pids: Vec<u32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    let users: Vec<LiveUser> = pids
        .iter()
        .filter_map(|&pid| using(pid, &directory))
        .collect();
    if users.is_empty() {
        return users;
    }
    let parents: HashMap<u32, u32> = pids
        .iter()
        .filter_map(|&pid| Some((pid, parent_of(pid)?)))
        .collect();
    let own = ancestry(std::process::id(), &parents);
    let harness = session.and_then(|session| {
        own.iter()
            .skip(1)
            .find(|&&pid| !PASS_THROUGH.contains(&comm_of(pid).as_str()))
            .map(|&harness| (harness, session))
    });
    users
        .into_iter()
        .filter(|user| {
            !own.contains(&user.pid)
                && !harness.is_some_and(|(root, session)| {
                    ancestry(user.pid, &parents).contains(&root) && carries(user.pid, session)
                })
        })
        .collect()
}

/// The program name of `pid`, as `/proc/<pid>/comm` gives it.
fn comm_of(pid: u32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|comm| comm.trim().to_owned())
        .unwrap_or_default()
}

/// This process's place: the host name and the PID namespace it runs in. Two
/// processes in one place can see each other in `/proc`; an agent box is a
/// place of its own.
pub fn this_place() -> String {
    let host = rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned();
    let namespace = std::fs::read_link("/proc/self/ns/pid").map_or_else(
        |_| "pid:[unknown]".to_owned(),
        |link| link.display().to_string(),
    );
    format!("{host} {namespace}")
}

/// What of `pid` lies inside `directory`: its working directory, else the
/// first open file there. `None` for a process this user cannot inspect.
fn using(pid: u32, directory: &Path) -> Option<LiveUser> {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    let inside = std::fs::read_link(proc.join("cwd"))
        .ok()
        .filter(|cwd| cwd.starts_with(directory))
        .or_else(|| {
            std::fs::read_dir(proc.join("fd"))
                .ok()?
                .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
                .map(|target| {
                    let shown = target.to_string_lossy();
                    shown
                        .strip_suffix(" (deleted)")
                        .map_or_else(|| target.clone(), PathBuf::from)
                })
                .find(|target| target.starts_with(directory))
        })?;
    Some(LiveUser {
        pid,
        command: command_of(pid),
        path: inside,
    })
}

/// The command line of `pid`, shortened for a refusal line.
fn command_of(pid: u32) -> String {
    let line = std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|bytes| {
            bytes
                .split(|&byte| byte == 0)
                .filter(|part| !part.is_empty())
                .map(String::from_utf8_lossy)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let line = if line.is_empty() { comm_of(pid) } else { line };
    let line = if line.is_empty() {
        "?".to_owned()
    } else {
        line
    };
    match line.char_indices().nth(120) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// The parent of `pid`, from `/proc/<pid>/stat`: the field after the state,
/// which follows the command name's closing parenthesis.
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// `pid` and its ancestors, nearest first.
fn ancestry(pid: u32, parents: &HashMap<u32, u32>) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    while let Some(parent) = parents
        .get(&current)
        .copied()
        .or_else(|| parent_of(current))
    {
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Whether `pid`'s environment names `session`.
fn carries(pid: u32, (name, value): Session<'_>) -> bool {
    let wanted = format!("{name}={value}");
    std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
        environ
            .split(|&byte| byte == 0)
            .any(|entry| entry == wanted.as_bytes())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_inside_the_directory_is_a_live_user() {
        let directory = tempfile::tempdir().expect("tempdir");
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .current_dir(directory.path())
            .spawn()
            .expect("spawn sleep");
        let users = live_users(directory.path(), None);
        let _ = child.kill();
        let _ = child.wait();
        assert!(users.iter().any(|user| user.pid == child.id()), "{users:?}");
    }

    #[test]
    fn this_process_inside_the_directory_is_not() {
        let directory = tempfile::tempdir().expect("tempdir");
        let file = std::fs::File::create(directory.path().join("held")).expect("create");
        let users = live_users(directory.path(), None);
        drop(file);
        assert!(users.is_empty(), "{users:?}");
    }

    #[test]
    fn a_place_names_the_host_and_the_pid_namespace() {
        assert!(this_place().contains(" pid:["), "{}", this_place());
    }
}
