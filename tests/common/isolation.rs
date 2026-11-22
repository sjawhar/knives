//! The environment every test process starts git, jj and knives in.
//!
//! Included by `tests/common/lab.rs`, which every integration test that starts
//! git, jj or knives includes, by `tests/view_compat.rs`, and by the library's
//! unit tests (`src/lib.rs`).
//!
//! A test process inherits the environment of whoever runs the suite: a
//! developer's shell, CI, or an agent session. Left as it is, every git, jj and
//! knives a test starts reads that person's configuration and runs that
//! person's tools: git signs each lab commit with their key (`commit.gpgsign`
//! in `~/.gitconfig`) and runs their global hooks; a `git` or `jj` wrapper
//! first on `PATH` adds trailers, hooks and leak checks to each commit and
//! push; and session variables (`OMP_SESSION_ID`, `GH_TOKEN`, `SSH_AUTH_SOCK`,
//! `GIT_CONFIG_COUNT`, `JJ_CONFIG` overlays, `JJ_USER`, which jj-lib lets
//! outrank a repository's own identity) change what the tools do. A test then
//! passes or fails by who runs it, and is slow where it should be quick.
//!
//! So [`isolate`] gives the process an environment of its own before `main`
//! runs, hence before libtest starts a thread:
//!
//! - Every variable is removed except those cargo, libtest, nextest and
//!   coverage tools hand the process, `TMPDIR`, `TERM`, and the `KNIVES_TEST_*`
//!   variables a test hands to this test binary when it starts it again
//!   ([`kept`]). A test that needs another variable sets it on its own command.
//! - `HOME` is a new directory for this process alone, removed when the process
//!   exits ([`remove_home`]). `XDG_CONFIG_HOME` is its `.config`, set rather
//!   than derived so that the lab's jj, a knives binary a test starts with
//!   another `HOME`, and knives called in-process agree on where jj keeps a
//!   repository's config: jj creates `$XDG_CONFIG_HOME/jj/repos/<id>/` on
//!   first contact.
//! - `GIT_CONFIG_GLOBAL` and `JJ_CONFIG` name the files [`GIT_CONFIG`] and
//!   [`JJ_CONFIG`] written in that home, which sign nothing, and
//!   `GIT_CONFIG_NOSYSTEM` drops the machine's `/etc/gitconfig`. Being named,
//!   they hold for a command a test gives another `HOME`. A test that wants a
//!   jj user config of its own still sets `JJ_CONFIG` on its command.
//! - `JJ_EDITOR` and `GIT_EDITOR` are `false`, so a command that would open an
//!   editor fails the same way everywhere instead of waiting on one.
//! - `PATH` keeps its order but loses each directory whose `git` or `jj` is a
//!   wrapper rather than the program ([`wraps_git_or_jj`]), so the first `git`
//!   and `jj` on it are the programs themselves.
//!
//! Once for the whole process, rather than on each spawn, because tests also
//! call knives in-process, and what it starts inherits this environment.

#![allow(
    clippy::expect_used,
    reason = "a test process that cannot build its environment cannot run a test"
)]

use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The global git configuration of every test process: nothing is signed.
const GIT_CONFIG: &str = "\
# A knives test process's global git config (tests/common/isolation.rs).
[commit]
\tgpgsign = false
[tag]
\tgpgsign = false
";

/// The jj user configuration of every test process: nothing is signed.
const JJ_CONFIG: &str = "\
# A knives test process's jj user config (tests/common/isolation.rs).
[signing]
behavior = \"drop\"
";

/// This process's `HOME`, for [`remove_home`].
static HOME: OnceLock<PathBuf> = OnceLock::new();

#[ctor::ctor(unsafe)]
fn isolate() {
    let path = std::env::var_os("PATH").map(|path| without_wrappers(&path));
    let home = tempfile::Builder::new()
        .prefix("knives-test-home.")
        .tempdir()
        .expect("create the test process's HOME")
        .keep();
    let git_config = home.join(".gitconfig");
    std::fs::write(&git_config, GIT_CONFIG).expect("write the test git config");
    let config_home = home.join(".config");
    let jj_config = config_home.join("jj").join("config.toml");
    std::fs::create_dir_all(jj_config.parent().expect("a file has a directory"))
        .expect("create the test jj config directory");
    std::fs::write(&jj_config, JJ_CONFIG).expect("write the test jj config");
    let removed: Vec<OsString> = std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| !kept(name))
        .collect();
    // SAFETY: a constructor runs before `main`, hence before libtest starts
    // its first thread; nothing reads or writes the environment concurrently.
    unsafe {
        for name in removed {
            std::env::remove_var(name);
        }
        if let Some(path) = path {
            std::env::set_var("PATH", path);
        }
        std::env::set_var("HOME", &home);
        std::env::set_var("XDG_CONFIG_HOME", &config_home);
        std::env::set_var("GIT_CONFIG_GLOBAL", &git_config);
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("JJ_CONFIG", &jj_config);
        std::env::set_var("JJ_EDITOR", "false");
        std::env::set_var("GIT_EDITOR", "false");
    }
    HOME.set(home).expect("the constructor runs once");
}

/// Remove this process's `HOME` ([`isolate`]) and whatever the tools wrote
/// there, when the process exits.
#[dtor::dtor(unsafe)]
fn remove_home() {
    if let Some(home) = HOME.get() {
        let _ = std::fs::remove_dir_all(home);
    }
}

/// Whether the variable `name` survives into the test process: what cargo,
/// libtest, nextest and coverage tools set for it (`CARGO*`, `RUST*`,
/// `NEXTEST*`, `LLVM_PROFILE_FILE`, the dynamic linker's search paths), the
/// directory temporary files go to, the terminal libtest reports to, and what
/// a test that starts its own test binary again hands the child it runs
/// there (`KNIVES_TEST_*`; `src/lock.rs`'s killed-holder test). `PATH` is not
/// kept as it is: [`isolate`] sets it from [`without_wrappers`].
fn kept(name: &OsStr) -> bool {
    let Some(name) = name.to_str() else {
        return false;
    };
    matches!(
        name,
        "TMPDIR"
            | "TERM"
            | "LLVM_PROFILE_FILE"
            | "LD_LIBRARY_PATH"
            | "DYLD_LIBRARY_PATH"
            | "DYLD_FALLBACK_LIBRARY_PATH"
    ) || ["CARGO", "RUST", "NEXTEST", "KNIVES_TEST_"]
        .into_iter()
        .any(|prefix| name.starts_with(prefix))
}

/// `path` without the directories that wrap git or jj ([`wraps_git_or_jj`]),
/// in its own order.
fn without_wrappers(path: &OsStr) -> OsString {
    std::env::join_paths(
        std::env::split_paths(path).filter(|directory| !wraps_git_or_jj(directory)),
    )
    .expect("directories split from PATH join again")
}

/// Whether `directory` holds a `git` or `jj` that is a wrapper rather than the
/// program: a script (it starts with `#!`, as the dotfiles' shims and asdf's
/// do), or a link to a program of another name (mise links every shim to
/// itself). A wrapper runs the program found later on `PATH`, after doing what
/// this module keeps out: reading the session, adding trailers and hooks.
fn wraps_git_or_jj(directory: &Path) -> bool {
    ["git", "jj"].into_iter().any(|program| {
        let candidate = directory.join(program);
        candidate.is_file() && !is_program(&candidate, program)
    })
}

/// Whether `candidate`, found on `PATH` as `program`, is that program: a file
/// of that name, links followed, that is not a script.
fn is_program(candidate: &Path, program: &str) -> bool {
    let Ok(target) = candidate.canonicalize() else {
        return false;
    };
    let mut start = [0_u8; 2];
    target.file_name() == Some(OsStr::new(program))
        && std::fs::File::open(&target)
            .and_then(|mut file| file.read_exact(&mut start))
            .is_ok()
        && &start != b"#!"
}
