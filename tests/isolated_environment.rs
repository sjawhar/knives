//! The suite runs the same whoever starts it. Each test here starts this test
//! binary again, running only [`scenario`] (lab helpers, a release home and
//! `knives start`, then a spawned `env`), in an environment that plants one
//! kind of thing a developer's or an agent session's environment carries, and
//! checks the scenario passes without reaching it. `tests/common/isolation.rs`
//! is what keeps each plant out.

#![allow(
    clippy::expect_used,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The test the other tests start in a planted environment.
const SCENARIO: &str = "scenario";
const ENVIRONMENT_START: &str = "--- a spawned process's environment ---";
const ENVIRONMENT_END: &str = "--- end of environment ---";

/// What the suite's helpers do with git, jj and knives: build a lab (git
/// commits and pushes, jj clones and fetches), commit and push a branch with
/// jj, commit a consumer with git, and start a branch with knives. Then print
/// the environment a process the test starts sees.
#[test]
#[ignore = "started by the other tests in this file, each in a planted environment"]
fn scenario() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let (home, _consumer) = lab::release_test_home(&lab);
    let started = lab::knives_start(&lab, &home, "feat/beta");
    assert!(started.status.success(), "{started:?}");
    let environment = Command::new("env").output().expect("run env");
    assert!(environment.status.success(), "{environment:?}");
    println!(
        "{ENVIRONMENT_START}\n{}{ENVIRONMENT_END}",
        String::from_utf8_lossy(&environment.stdout)
    );
}

/// A directory for one planted environment: its `HOME` and whatever it plants.
struct Plant {
    root: tempfile::TempDir,
}

impl Plant {
    fn new() -> Self {
        let plant = Self {
            root: tempfile::tempdir().expect("plant directory"),
        };
        std::fs::create_dir(plant.home()).expect("planted HOME");
        plant
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.root.path().join(name)
    }

    /// Write an executable `#!/bin/sh` script at `name` that records each run
    /// in `log` and fails.
    fn failing_script(&self, name: &str, log: &str) -> PathBuf {
        let script = self.path(name);
        std::fs::create_dir_all(script.parent().expect("a script has a directory"))
            .expect("script directory");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$0 $*\" >> '{}'\nexit 1\n",
                self.path(log).display()
            ),
        )
        .expect("write script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
            .expect("make script executable");
        script
    }

    /// A hooks directory whose every hook records its run in `hooks.log` and
    /// fails.
    fn failing_hooks(&self) -> PathBuf {
        for hook in [
            "pre-commit",
            "prepare-commit-msg",
            "commit-msg",
            "post-commit",
            "pre-push",
            "pre-receive",
            "update",
            "post-receive",
            "reference-transaction",
            "post-checkout",
            "post-merge",
            "post-rewrite",
        ] {
            self.failing_script(&format!("hooks/{hook}"), "hooks.log");
        }
        self.path("hooks")
    }
}

/// What a run of [`scenario`] in a planted environment printed.
struct Run {
    output: std::process::Output,
    stdout: String,
}

impl Run {
    /// Run [`scenario`] with exactly this environment: `HOME`, `PATH` (the
    /// test process's own, after `path_first`), the test process's `TMPDIR`,
    /// and `variables`.
    fn scenario(home: &Path, path_first: Option<&Path>, variables: &[(&str, String)]) -> Self {
        let inherited = std::env::var_os("PATH").expect("PATH is set");
        let path = std::env::join_paths(
            path_first
                .map(Path::to_path_buf)
                .into_iter()
                .chain(std::env::split_paths(&inherited)),
        )
        .expect("join PATH");
        let mut command = Command::new(std::env::current_exe().expect("this test binary"));
        command
            .args([
                "--exact",
                SCENARIO,
                "--include-ignored",
                "--nocapture",
                "--test-threads",
                "1",
            ])
            .env_clear()
            .env("HOME", home)
            .env("PATH", path);
        if let Some(temporary) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", temporary);
        }
        for (name, value) in variables {
            command.env(name, value);
        }
        let output = command.output().expect("run the scenario");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        Self { output, stdout }
    }

    /// Fail unless the scenario ran, alone, and passed: a filter that matched
    /// no test would exit 0 too.
    fn assert_passed(&self) {
        assert!(
            self.output.status.success()
                && self.stdout.contains("running 1 test")
                && self.stdout.contains("test result: ok. 1 passed"),
            "the scenario did not pass:\nstdout:\n{}\nstderr:\n{}",
            self.stdout,
            String::from_utf8_lossy(&self.output.stderr)
        );
    }

    /// The `NAME=value` lines the scenario's spawned `env` printed.
    fn spawned_environment(&self) -> Vec<&str> {
        let (_, rest) = self
            .stdout
            .split_once(ENVIRONMENT_START)
            .unwrap_or_else(|| panic!("no environment printed:\n{}", self.stdout));
        let (environment, _) = rest
            .split_once(ENVIRONMENT_END)
            .unwrap_or_else(|| panic!("the environment never ended:\n{}", self.stdout));
        environment
            .lines()
            .filter(|line| !line.is_empty())
            .collect()
    }

    /// The value the scenario's spawned `env` printed for `name`, if any.
    fn spawned(&self, name: &str) -> Option<String> {
        self.spawned_environment()
            .into_iter()
            .find_map(|line| line.strip_prefix(&format!("{name}=")).map(str::to_owned))
    }
}

#[test]
fn the_scenario_passes_in_a_clean_environment() {
    // Given: an empty HOME, the test process's PATH and nothing else.
    let plant = Plant::new();

    // When / Then: the scenario passes, so a failure of a test below is its
    // plant's doing.
    Run::scenario(&plant.home(), None, &[]).assert_passed();
}

#[test]
fn a_developers_home_reaches_no_git_or_jj_a_test_starts() {
    // Given: a HOME whose git config signs every commit with a key that does
    // not exist and runs hooks that fail, and whose jj config signs every
    // commit with a key that does not exist.
    let plant = Plant::new();
    let hooks = plant.failing_hooks();
    let missing_key = plant.path("no-such-key");
    std::fs::write(
        plant.home().join(".gitconfig"),
        format!(
            "[user]\n\tsigningkey = {}.pub\n[gpg]\n\tformat = ssh\n[commit]\n\tgpgsign = true\n\
             [tag]\n\tgpgsign = true\n[core]\n\thooksPath = {}\n",
            missing_key.display(),
            hooks.display()
        ),
    )
    .expect("plant a git config");
    let jj_config = format!(
        "[signing]\nbehavior = \"force\"\nbackend = \"ssh\"\nkey = \"{}\"\n\n[git]\nsign-on-push = true\n",
        missing_key.display()
    );
    let jj_directory = plant.home().join(".config/jj");
    std::fs::create_dir_all(&jj_directory).expect("plant a jj config directory");
    std::fs::write(jj_directory.join("config.toml"), &jj_config).expect("plant a jj config");
    std::fs::write(plant.home().join(".jjconfig.toml"), &jj_config).expect("plant a jj config");

    // When: the scenario runs with that HOME.
    let run = Run::scenario(&plant.home(), None, &[]);

    // Then: it passes, no hook ran, and what it starts has a HOME of its own.
    run.assert_passed();
    assert!(
        !plant.path("hooks.log").exists(),
        "a planted hook ran: {:?}",
        std::fs::read_to_string(plant.path("hooks.log"))
    );
    let home = run.spawned("HOME").expect("a spawned process has a HOME");
    assert_ne!(PathBuf::from(home), plant.home());
}

#[test]
fn wrappers_first_on_path_are_never_run() {
    // Given: a directory first on PATH whose git, jj and ssh-keygen are
    // wrappers that record each run and fail, as a developer's shims would
    // add trailers and hooks.
    let plant = Plant::new();
    for program in ["git", "jj", "ssh-keygen"] {
        plant.failing_script(&format!("bin/{program}"), "wrappers.log");
    }

    // When: the scenario runs with that PATH.
    let run = Run::scenario(&plant.home(), Some(&plant.path("bin")), &[]);

    // Then: it passes, no wrapper ran, and the directory is not on the PATH of
    // what it starts.
    run.assert_passed();
    assert!(
        !plant.path("wrappers.log").exists(),
        "a planted wrapper ran: {:?}",
        std::fs::read_to_string(plant.path("wrappers.log"))
    );
    let path = run.spawned("PATH").expect("a spawned process has a PATH");
    assert!(
        std::env::split_paths(&path).all(|directory| directory != plant.path("bin")),
        "{path}"
    );
}

#[test]
fn a_path_with_only_wrappers_of_git_and_jj_stops_the_test_process_naming_them() {
    // Given: a PATH of one directory, whose git is a script and whose jj is a
    // link to a program of another name, as on a machine whose only git and
    // jj are wrappers.
    let plant = Plant::new();
    let git = plant.failing_script("bin/git", "wrappers.log");
    let jj = plant.path("bin/jj");
    let target = std::env::current_exe().expect("this test binary");
    std::os::unix::fs::symlink(&target, &jj).expect("link jj to another program");

    // When: this test binary starts with that PATH.
    let output = Command::new(&target)
        .arg("--list")
        .env_clear()
        .env("HOME", plant.home())
        .env("PATH", plant.path("bin"))
        .envs(std::env::var_os("TMPDIR").map(|temporary| ("TMPDIR", temporary)))
        .output()
        .expect("run this test binary");

    // Then: it stops before listing a test, naming both wrappers and the PATH
    // it searched, and runs neither.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stdout}\n{stderr}");
    assert!(!stdout.contains(": test"), "{stdout}");
    for line in [
        format!(
            "knives tests: PATH has no real git: skipped {}, a script",
            git.display()
        ),
        format!(
            "knives tests: PATH has no real jj: skipped {}, a link to {}",
            jj.display(),
            target
                .canonicalize()
                .expect("the test binary's real path")
                .display()
        ),
        format!(
            "knives tests: searched PATH={}",
            plant.path("bin").display()
        ),
    ] {
        assert!(
            stderr.lines().any(|seen| seen == line),
            "no line {line:?} in:\n{stderr}"
        );
    }
    assert!(!plant.path("wrappers.log").exists());
}

#[test]
fn a_sessions_variables_reach_nothing_a_test_starts() {
    // Given: the variables an agent session exports: its id under each
    // harness's name, a forge token and repository, an ssh agent, git
    // configuration handed through the environment (hooks that fail), a jj
    // config overlay and a jj identity.
    let plant = Plant::new();
    let hooks = plant.failing_hooks();
    let overlay = plant.path("omp-attribution-planted.toml");
    std::fs::write(
        &overlay,
        format!(
            "[signing]\nbehavior = \"force\"\nbackend = \"ssh\"\nkey = \"{}\"\n",
            plant.path("no-such-key").display()
        ),
    )
    .expect("plant a jj overlay");
    let variables = [
        ("OMP_SESSION_ID", "01planted-session".to_owned()),
        ("PI_SESSION_ID", "01planted-session".to_owned()),
        (
            "PI_SESSION_FILE",
            plant.path("session.jsonl").display().to_string(),
        ),
        ("CLAUDE_CODE_SESSION_ID", "planted-session".to_owned()),
        ("CLAUDECODE", "1".to_owned()),
        ("KNIVES_OWNER", "planted-owner".to_owned()),
        ("GH_TOKEN", "planted-token".to_owned()),
        ("GH_REPO", "planted/elsewhere".to_owned()),
        (
            "SSH_AUTH_SOCK",
            plant.path("agent.sock").display().to_string(),
        ),
        ("GIT_CONFIG_COUNT", "1".to_owned()),
        ("GIT_CONFIG_KEY_0", "core.hooksPath".to_owned()),
        ("GIT_CONFIG_VALUE_0", hooks.display().to_string()),
        ("JJ_CONFIG", overlay.display().to_string()),
        ("JJ_USER", "Planted Session".to_owned()),
        ("JJ_EMAIL", "planted@example.invalid".to_owned()),
    ];

    // When: the scenario runs with them.
    let run = Run::scenario(&plant.home(), None, &variables);

    // Then: it passes, no hook ran, and none of them reaches what it starts.
    run.assert_passed();
    assert!(
        !plant.path("hooks.log").exists(),
        "a planted hook ran: {:?}",
        std::fs::read_to_string(plant.path("hooks.log"))
    );
    // The test process names a JJ_CONFIG of its own; only the planted one is a
    // leak.
    let overlay = overlay.display().to_string();
    let reached: Vec<&str> = variables
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| {
            run.spawned(name)
                .is_some_and(|value| *name != "JJ_CONFIG" || value == overlay)
        })
        .collect();
    assert!(
        reached.is_empty(),
        "planted variables reached a spawned process: {reached:?}"
    );
}
