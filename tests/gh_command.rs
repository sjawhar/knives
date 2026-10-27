#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
// allow: SIZE_OK: 2154 lines - real-binary gh passthrough scenarios share one fixture and process harness.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;

fn fake_gh() -> (tempfile::TempDir, PathBuf) {
    fake_gh_exiting(0)
}

fn fake_gh_exiting(code: i32) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("fake gh dir");
    let log = dir.path().join("gh.log");
    let gh = dir.path().join("gh");
    fs::write(
        &gh,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$FAKE_GH_LOG\"\n\
             printf 'GH_TOKEN=%s\\n' \"${{GH_TOKEN:-unset}}\" >> \"$FAKE_GH_LOG\"\n\
             printf 'BRANCH=%s\\n' \"$(git symbolic-ref --short HEAD 2>&1)\" >> \"$FAKE_GH_LOG\"\n\
             exit {code}\n"
        ),
    )
    .expect("write fake gh");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod fake gh");
    (dir, log)
}

fn fake_app_token() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("fake helper dir");
    let helper = dir.path().join("gh-app-token");
    fs::write(
        &helper,
        "#!/bin/sh\nowner=$(sed -n 's|^path=\\([^/]*\\)/.*|\\1|p')\n[ -n \"$owner\" ] || owner=noowner\nprintf 'username=x-access-token\\npassword=tok-%s\\n' \"$owner\"\n",
    )
    .expect("write fake gh-app-token");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("chmod helper");
    dir
}

fn token_config(helper_dir: &Path, owner: &str) -> PathBuf {
    let host = concat!("github", ".com");
    let gitconfig = helper_dir.join("gitconfig");
    fs::write(
        &gitconfig,
        format!("[credential \"https://{host}/\"]\n\thelper = !gh-app-token {owner}\n"),
    )
    .expect("write gitconfig");
    gitconfig
}

fn helper_path(helper_dir: &Path) -> String {
    format!(
        "{}:{}",
        helper_dir.display(),
        std::env::var("PATH").expect("PATH")
    )
}

/// knives as the shim's inner pass: PATH is inherited and may well begin with the
/// real shim, so the depth marker the shim sets on every re-entry keeps these
/// scenarios on the pass that mints and runs gh. A direct-invocation scenario
/// removes it.
fn knives_cmd(scratch: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    knives_env(&mut command, scratch);
    command
}

/// The `knives_cmd` environment on a command that reaches knives some other way.
fn knives_env(command: &mut Command, scratch: &Path) {
    command
        .env("GIT_CONFIG_GLOBAL", scratch.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KNIVES_CONFIG_HOME", scratch)
        .env("HOME", scratch)
        .env("KNIVES_GH_SHIM_DEPTH", "1")
        .env_remove("GH_TOKEN");
}

fn git_config(work: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(["config"])
        .args(args)
        .current_dir(work)
        .status()
        .expect("run git config");
    assert!(status.success(), "git config {args:?}");
}

#[test]
fn pr_view_injects_current_bookmark_and_wrapper_branch() {
    // Given: a jj repo whose working copy is the bookmarked feature change.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let (dir, log) = fake_gh();

    // When: gh pr view has no positional target.
    let output = knives_cmd(dir.path())
        .args(["gh", "--", "pr", "view", "--json", "title"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: knives injects the bookmark and compensates for jj's detached HEAD.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        &lines[..5],
        &["pr", "view", "feat/alpha", "--json", "title"]
    );
    assert!(recorded.contains("BRANCH=feat/alpha"), "{recorded}");
}

#[test]
fn pr_view_injects_a_bookmark_that_is_ahead_of_its_remote() {
    // Given: the working copy is a bookmarked change amended since its push, which jj's
    // own rendering marks `feat/alpha*`.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["edit", "feat/alpha"]);
    fs::write(lab.work.join("alpha.txt"), "alpha, amended\n").expect("amend the branch");
    assert_eq!(
        lab.jj_work_output(["log", "-r", "@", "--no-graph", "-T", "bookmarks"]),
        "feat/alpha*"
    );
    let (dir, log) = fake_gh();

    // When: gh pr view has no positional target.
    let output = knives_cmd(dir.path())
        .args(["gh", "--", "pr", "view", "--json", "title"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: the bookmark's name is injected, without the mark.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(
        &lines[..5],
        &["pr", "view", "feat/alpha", "--json", "title"]
    );
}

#[test]
fn spawn_failure_cleans_up_the_git_wrapper_tempdir() {
    // Given: a bookmarked jj repo and a TMPDIR-local nonexistent real gh.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let scratch = tempfile::tempdir().expect("scratch");
    let missing = scratch.path().join("missing-gh");

    // When: a PR subcommand creates the wrapper but cannot spawn real gh.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "pr", "view"])
        .current_dir(&lab.work)
        .env("TMPDIR", scratch.path())
        .env("KNIVES_REAL_GH", missing)
        .output()
        .expect("run knives gh");

    // Then: the shell-compatible failure is returned without leaving the wrapper behind.
    assert_eq!(output.status.code(), Some(127));
    assert!(
        !fs::read_dir(scratch.path())
            .expect("read TMPDIR")
            .any(|entry| {
                entry
                    .expect("TMPDIR entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".tmp")
            }),
        "git wrapper tempfile remained in {}",
        scratch.path().display()
    );
}

#[test]
fn outside_jj_repo_arguments_pass_through_untouched() {
    // Given: a directory outside any jj repository and a fake gh.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: knives invokes gh.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "pr", "list", "--json", "state"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: every gh argument is preserved.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(
        recorded.starts_with("pr\nlist\n--json\nstate\n"),
        "{recorded}"
    );
}

#[test]
fn no_target_repository_says_so_before_gh_runs() {
    // Given: a directory with no remote, no -R flag, and a fake gh that fails.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh_exiting(3);

    // When: knives invokes gh on a call that names no repository.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "issue", "list", "--limit", "1"])
        .current_dir(scratch.path())
        .env_remove("GH_TOKEN")
        .env_remove("GH_REPO")
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: one stderr line names the cause and remedy, and gh's status is kept.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("knives gh: no target repository"),
        "{stderr}"
    );
    assert!(stderr.contains("pass -R <owner>/<repo>"), "{stderr}");
    assert_eq!(output.status.code(), Some(3));
}

#[test]
fn no_target_advice_names_r_only_for_commands_that_take_it() {
    // Given: a directory with no remote, and commands with and without -R.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();
    let takes = "pass -R <owner>/<repo>";
    let account = "an App token cannot run it, and agent sessions have no ambient login";
    let named = "name the repository as <owner>/<repo>";
    let cases: [(&[&str], &str, bool); 25] = [
        (&["api", "user"], "gh api repos/<owner>/<repo>", false),
        (&["repo", "view"], named, false),
        (&["repo", "view", "--json", "name"], named, false),
        (&["repo", "clone", "x"], named, false),
        (&["repo", "fork"], named, false),
        (&["repo", "edit"], named, false),
        (&["repo", "delete"], named, false),
        (&["repo", "archive"], named, false),
        (&["repo", "unarchive"], named, false),
        (&["repo", "sync"], named, false),
        (&["repo", "set-default"], named, false),
        (
            &["repo", "list"],
            "GH_REPO=<owner>/<repo> gh repo list <owner>",
            false,
        ),
        (
            &["repo", "list"],
            "orgs/<org>/repos?per_page=100' --paginate",
            false,
        ),
        (&["extension", "list"], "set GH_REPO=<owner>/<repo>", false),
        (&["gist", "list"], account, false),
        (&["ssh-key", "list"], account, false),
        (&["discussion", "list"], takes, true),
        (&["search", "issues", "x"], takes, true),
        (&["codespace", "list"], takes, true),
        (&["repo", "read-file", "x"], takes, true),
        (&["repo", "rename", "y"], takes, true),
        (&["repo", "rename", "acme/y"], takes, true),
        (&["repo", "autolink", "list"], takes, true),
        (&["repo", "deploy-key", "list"], takes, true),
        (&["workflow", "list"], takes, true),
    ];
    for (arguments, advice, takes_r) in cases {
        // When: knives invokes gh on a call that names no repository.
        let output = knives_cmd(scratch.path())
            .arg("gh")
            .arg("--")
            .args(arguments)
            .current_dir(scratch.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh");

        // Then: the advice names a spelling that command accepts.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(advice), "{arguments:?}: {stderr}");
        assert_eq!(
            stderr.contains("pass -R"),
            takes_r,
            "{arguments:?}: {stderr}"
        );
        if advice == account || advice == named {
            assert!(!stderr.contains("GH_REPO"), "{arguments:?}: {stderr}");
        }
    }
}

#[test]
fn local_commands_print_no_target_line() {
    // Given: a directory with no remote, and commands that never call the API.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();
    let cases: [&[&str]; 4] = [
        &["alias", "list"],
        &["--version"],
        &["skill", "list"],
        &["attestation", "trusted-root"],
    ];
    for arguments in cases {
        // When: knives invokes gh on one.
        let output = knives_cmd(scratch.path())
            .arg("gh")
            .arg("--")
            .args(arguments)
            .current_dir(scratch.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh");

        // Then: no no-target line is printed.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("no target repository"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn a_resolved_owner_prints_no_target_hint() {
    // Given: a call naming its repository with -R.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: knives invokes gh.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "pr", "view", "1", "-R", "acme/widgets"])
        .current_dir(scratch.path())
        .env_remove("GH_TOKEN")
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: no no-target line is printed.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("no target repository"), "{stderr}");
}

#[test]
fn a_git_failure_inside_a_checkout_is_reported_not_fallen_through() {
    // Given: a worktree whose admin registration was pruned away — the cwd is a
    // checkout, and every git read in it fails fatally.
    let scratch = tempfile::tempdir().expect("scratch");
    let main = scratch.path().join("main");
    fs::create_dir(&main).expect("mkdir main");
    let git = |dir: &Path, args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&main, &["init", "-q"]);
    git(&main, &["commit", "-q", "--allow-empty", "-m", "init"]);
    git(&main, &["worktree", "add", "-q", "../wt", "-b", "b1"]);
    fs::remove_dir_all(main.join(".git/worktrees/wt")).expect("prune the registration");
    let worktree = scratch.path().join("wt");
    let (dir, log) = fake_gh();

    // When: knives gh runs a routed command from that worktree.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "pr", "list"])
        .current_dir(&worktree)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: git's own error reaches stderr, knives exits non-zero, and gh never
    // runs on its own auth (a fall-through here read as a gh login problem).
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.starts_with("knives gh: reading remotes of"),
        "{stderr}"
    );
    // git words this failure two ways: git 2.43 says "not a git repository",
    // and the GitHub runners' git (2026-10-27) says "gitfile does not point to
    // a valid repository".
    assert!(
        stderr.contains("not a git repository")
            || stderr.contains("gitfile does not point to a valid repository"),
        "{stderr}"
    );
    assert!(
        !log.exists(),
        "gh must not run on its own auth when git fails in a checkout"
    );
}

#[test]
fn bare_gh_separator_passes_zero_args_through() {
    // Given: a fake gh outside any jj repository.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: knives receives the required separator with no following arguments.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run bare knives gh");

    // Then: gh receives an empty argument list.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert_eq!(recorded.lines().next(), Some(""), "{recorded}");
}

#[test]
fn gh_help_after_separator_passes_through_to_gh() {
    // Given: a fake gh outside any jj repository.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: --help follows the gh argument separator.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "--help"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh help");

    // Then: clap does not consume gh's own help flag.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.starts_with("--help\n"), "{recorded}");
}

#[test]
fn valueless_resolved_marker_stops_remote_token_routing() {
    // Given: a hand-edited empty marker and a fallback remote with a routed token.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &[
            "remote.upstream.url",
            &format!("https://{host}/fallback/repository.git"),
        ],
    );
    let config = lab.work.join(".git").join("config");
    let existing = fs::read_to_string(&config).expect("read git config");
    fs::write(
        &config,
        format!("{existing}\n[remote \"marker\"]\n\tgh-resolved\n"),
    )
    .expect("write valueless marker");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");

    // When: gh resolves its target without an explicit API or repository signal.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: the marker is terminal and an empty target does not mint a fallback token.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn gh_search_owner_and_repo_flags_route_the_token_and_print_no_hint() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for (flags, token) in [
        (["--owner", "acme"], "GH_TOKEN=tok-acme"),
        (["--owner=acme", "--limit=1"], "GH_TOKEN=tok-acme"),
        (["--repo", "acme/widgets"], "GH_TOKEN=tok-acme"),
    ] {
        let (dir, log) = fake_gh();

        // When: knives invokes gh search naming its target by flag.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--", "search", "issues"])
            .args(flags)
            .arg("grader")
            .current_dir(scratch.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the owner's token is used and no no-target line is printed.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(recorded.contains(token), "{flags:?}: {recorded}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("no target repository"), "{stderr}");
    }
}

/// `gh repo` calls whose repository positional is acme/widgets, in each spelling
/// gh 2.98 reads as that repository.
fn acme_widgets_positional_calls() -> Vec<Vec<String>> {
    let host = concat!("github", ".com");
    let url = format!("https://{host}/acme/widgets");
    let scp = format!("git@{host}:acme/widgets.git");
    let host_spec = format!("{host}/acme/widgets");
    let bare_scp = format!("{host}:acme/widgets");
    let user_scp = format!("alice@{host}:acme/widgets");
    let calls: [&[&str]; 23] = [
        &["repo", "view", "acme/widgets", "--json", "name"],
        &["repo", "view", "--json", "name", "acme/widgets"],
        &["repo", "view", "-b", "main", &url],
        &["repo", "view", "-wb", "main", "acme/widgets"],
        &["repo", "view", "-bmain", "acme/widgets"],
        &["repo", "view", "--", "acme/widgets"],
        &["repo", "clone", "acme/widgets"],
        &["repo", "clone", &scp],
        &["repo", "clone", &bare_scp],
        &["repo", "clone", &user_scp],
        &[
            "repo",
            "clone",
            "-u",
            "up",
            &host_spec,
            "dir",
            "--",
            "--depth=1",
        ],
        &["repo", "clone", "--", "acme/widgets", "dir"],
        &["repo", "fork", "--fork-name", "gadgets", "acme/widgets"],
        &["repo", "edit", "-h", "https://x.example", "acme/widgets"],
        // gh rejects -R here, so these are -d's value, not a target.
        &["repo", "edit", "acme/widgets", "-d", "--repo=rival/y"],
        &[
            "repo",
            "edit",
            "--description",
            "-R=rival/y",
            "acme/widgets",
        ],
        &["repo", "delete", "--yes", "acme/widgets"],
        &["repo", "archive", "-y", "acme/widgets"],
        &["repo", "unarchive", "-y", "acme/widgets"],
        &["repo", "sync", "-s", "other/source", "acme/widgets"],
        &["repo", "set-default", "acme/widgets"],
        &["repo", "garden", "acme/widgets"],
        &["repo", "credits", "-s", "acme/widgets"],
    ];
    calls
        .iter()
        .map(|call| call.iter().map(|word| (*word).to_owned()).collect())
        .collect()
}

#[test]
fn a_repository_positional_routes_gh_repo_outside_a_checkout() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for arguments in acme_widgets_positional_calls() {
        let (dir, log) = fake_gh();

        // When: knives invokes a gh repo subcommand that names its repository.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(&arguments)
            .current_dir(scratch.path())
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the named owner's token is used, gh sees the arguments unchanged,
        // and no no-target line is printed.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=tok-acme"),
            "{arguments:?}: {recorded}"
        );
        assert!(
            recorded.starts_with(&format!("{}\n", arguments.join("\n"))),
            "{arguments:?}: {recorded}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("no target repository"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn a_repository_positional_beats_gh_repo_and_the_checkout_remote() {
    // Given: a checkout whose origin is another owner's, GH_REPO naming a third, and
    // a helper that mints per owner.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &["remote.origin.url", &format!("https://{host}/other/thing")],
    );
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for arguments in acme_widgets_positional_calls() {
        let (dir, log) = fake_gh();

        // When: the call names acme/widgets, the repository gh itself acts on.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(&arguments)
            .current_dir(&lab.work)
            .env("GH_REPO", "elsewhere/thing")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the token is acme's.
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=tok-acme"),
            "{arguments:?}: {recorded}"
        );
    }
}

/// `gh pr`, `gh issue` and `gh agent-task view` calls whose selector is a URL
/// of acme/widgets, which gh 2.98 acts on whatever `-R` or the checkout say.
fn acme_widgets_selector_url_calls() -> Vec<Vec<String>> {
    let host = concat!("github", ".com");
    let pull = format!("https://{host}/acme/widgets/pull/2112");
    let files = format!("https://www.{host}/acme/widgets/pull/2112/files");
    let issue = format!("https://{host}/acme/widgets/issues/9");
    let calls: [&[&str]; 12] = [
        &["pr", "view", &pull, "--json", "state"],
        &["-R", "rival/y", "pr", "view", &pull],
        &["pr", "view", "--json", "state", &pull],
        &["pr", "checks", &pull, "--watch"],
        &["pr", "diff", &files],
        &["pr", "comment", &pull, "--body", "hi"],
        &["pr", "merge", "--squash", &pull],
        &["pr", "view", "--", &pull],
        &["issue", "view", &issue],
        &["issue", "comment", &pull, "-b", "hi"],
        &["issue", "close", &issue],
        &["agent-task", "view", &pull],
    ];
    calls
        .iter()
        .map(|call| call.iter().map(|word| (*word).to_owned()).collect())
        .collect()
}

#[test]
fn a_selector_url_routes_gh_pr_and_issue_outside_a_checkout() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for arguments in acme_widgets_selector_url_calls() {
        let (dir, log) = fake_gh();

        // When: knives invokes a gh call whose selector is a URL.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(&arguments)
            .current_dir(scratch.path())
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the URL owner's token is used, gh sees the arguments unchanged,
        // and no no-target line is printed.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=tok-acme"),
            "{arguments:?}: {recorded}"
        );
        assert!(
            recorded.starts_with(&format!("{}\n", arguments.join("\n"))),
            "{arguments:?}: {recorded}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains("no target repository"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn a_selector_url_beats_the_repo_flag_gh_repo_and_the_checkout_remote() {
    // Given: a checkout whose origin is another owner's, GH_REPO naming a third,
    // and a helper that mints per owner.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &["remote.origin.url", &format!("https://{host}/other/thing")],
    );
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for mut arguments in acme_widgets_selector_url_calls() {
        arguments.extend(["-R".to_owned(), "rival/y".to_owned()]);
        let (dir, log) = fake_gh();

        // When: the call also passes -R for a rival, as gh ignores for a URL.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(&arguments)
            .current_dir(&lab.work)
            .env("GH_REPO", "elsewhere/thing")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the token is acme's, the repository gh acts on.
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=tok-acme"),
            "{arguments:?}: {recorded}"
        );
    }
}

#[test]
fn a_url_that_is_not_a_selector_does_not_route() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    let host = concat!("github", ".com");
    let pull = format!("https://{host}/acme/widgets/pull/2112");
    let run = format!("https://{host}/acme/widgets/actions/runs/9");
    let other_host = "https://forge.example/acme/widgets/pull/2112".to_owned();
    for arguments in [
        &["pr", "comment", "5", "--body", &pull][..],
        &["pr", "view", &other_host],
        &["issue", "list", "--search", &pull],
        &["run", "view", &run],
        &["search", "prs", &pull],
    ] {
        let (dir, log) = fake_gh();

        // When: the URL is a flag value, another host's, or where gh reads no URL.
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(scratch.path())
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: no token is minted for the URL's owner.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            !recorded.contains("GH_TOKEN=tok-acme"),
            "{arguments:?}: {recorded}"
        );
    }
}

#[test]
fn a_watching_call_asks_the_helper_for_a_fresh_tokens_hour() {
    // Given: a helper that answers with the lifetime knives asked it for.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = tempfile::tempdir().expect("fake helper dir");
    let helper = helper_dir.path().join("gh-app-token");
    fs::write(
        &helper,
        "#!/bin/sh\ncat > /dev/null\nlifetime=\"$GH_APP_TOKEN_MIN_LIFETIME\"\n[ -n \"$lifetime\" ] || lifetime=unset\nprintf 'username=x-access-token\\npassword=life-%s\\n' \"$lifetime\"\n",
    )
    .expect("write lifetime-echoing gh-app-token");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("chmod helper");
    let gitconfig = token_config(helper_dir.path(), "agent");
    for (arguments, lifetime) in [
        (
            &["run", "watch", "39834746929", "--exit-status"][..],
            "3600",
        ),
        (&["pr", "checks", "7", "--watch"], "3600"),
        (&["run", "view", "39834746929"], "1800"),
        (&["pr", "checks", "7"], "1800"),
    ] {
        let (dir, log) = fake_gh();

        // When: knives runs the call for a named repository.
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(arguments)
            .args(["-R", "acme/widgets"])
            .current_dir(scratch.path())
            .env_remove("GH_APP_TOKEN_MIN_LIFETIME")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: a watch asks for the hour a fresh token lives; any other call for
        // half an hour, more than the five minutes a cached token may have left.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains(&format!("GH_TOKEN=life-{lifetime}\n")),
            "{arguments:?}: {recorded}"
        );
    }
}

#[test]
fn gh_repo_fork_with_an_org_routes_by_that_org() {
    // Given: a scratch directory with no remote, a checkout whose origin is another
    // owner's, and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &["remote.origin.url", &format!("https://{host}/other/thing")],
    );
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    let calls: [&[&str]; 4] = [
        &["repo", "fork", "acme/widgets", "--org", "partner"],
        &["repo", "fork", "--org=partner", "acme/widgets", "--clone"],
        &[
            "repo",
            "fork",
            "--org",
            "first",
            "--org",
            "partner",
            "acme/widgets",
        ],
        &["repo", "fork", "--org", "partner"],
    ];
    for (cwd, gh_repo) in [
        (scratch.path(), None),
        (lab.work.as_path(), Some("elsewhere/x")),
    ] {
        for arguments in calls {
            let (dir, log) = fake_gh();

            // When: the fork is created in the partner organization.
            let mut command = knives_cmd(helper_dir.path());
            command
                .args(["gh", "--"])
                .args(arguments)
                .current_dir(cwd)
                .env_remove("GH_REPO")
                .env("KNIVES_REAL_GH", dir.path().join("gh"))
                .env("FAKE_GH_LOG", &log)
                .env("PATH", helper_path(helper_dir.path()))
                .env("GIT_CONFIG_GLOBAL", &gitconfig);
            if let Some(spec) = gh_repo {
                command.env("GH_REPO", spec);
            }
            let output = command.output().expect("run knives gh");

            // Then: the token is the organization's, which owns what gh creates.
            assert!(output.status.success(), "{arguments:?}: {output:?}");
            let recorded = fs::read_to_string(&log).expect("fake gh ran");
            assert!(
                recorded.contains("GH_TOKEN=tok-partner"),
                "{arguments:?} in {cwd:?}: {recorded}"
            );
        }
    }
}

#[test]
fn a_bare_or_renamed_repository_positional_does_not_route() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    // A bare name is a repository of the caller's own account, which knives cannot
    // name; `rename`'s positional is the new name; `--json` and the `b` ending a
    // short-flag cluster take the next word as their value; `list` names an owner,
    // not a repository.
    for arguments in [
        &["repo", "clone", "widgets"][..],
        &["repo", "view", "--", "widgets"],
        &["repo", "rename", "acme/widgets"],
        &["repo", "view", "--json", "acme/widgets"],
        &["repo", "view", "-wb", "acme/widgets"],
        &["repo", "list", "acme/widgets"],
    ] {
        let (dir, log) = fake_gh();

        // When: knives invokes it.
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(scratch.path())
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: nothing routes, so no token is minted and the no-target line prints.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=unset"),
            "{arguments:?}: {recorded}"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("no target repository"),
            "{arguments:?}: {stderr}"
        );
    }
}

#[test]
fn owner_flags_after_the_double_dash_terminator_are_search_terms_not_routing() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    let spellings: [&[&str]; 6] = [
        &["--owner", "acme"],
        &["--owner=acme"],
        &["-R", "acme/widgets"],
        &["-R=acme/widgets"],
        &["--repo", "acme/widgets"],
        &["--repo=acme/widgets"],
    ];
    let run = |args: Vec<&str>| {
        let (dir, log) = fake_gh();
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(args)
            .current_dir(scratch.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        (
            recorded,
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    for spelling in spellings {
        // When: the flag spelling follows `--`, where gh reads it as a search term.
        let mut after = vec!["search", "issues", "--"];
        after.extend(spelling);
        let (recorded, stderr) = run(after);

        // Then: nothing routes, and the no-target line says so.
        assert!(
            recorded.contains("GH_TOKEN=unset"),
            "{spelling:?}: {recorded}"
        );
        assert!(
            stderr.contains("no target repository"),
            "{spelling:?}: {stderr}"
        );

        // When: the same spelling precedes `--`.
        let mut before = vec!["search", "issues"];
        before.extend(spelling);
        before.extend(["--", "grader"]);
        let (recorded, stderr) = run(before);

        // Then: it still routes to acme.
        assert!(
            recorded.contains("GH_TOKEN=tok-acme"),
            "{spelling:?}: {recorded}"
        );
        assert!(!stderr.contains("no target repository"), "{stderr}");
    }
}

#[test]
fn a_double_dash_taken_as_a_flag_value_does_not_end_the_owner_scan() {
    // Given: a scratch directory with no remote and a helper that mints per owner.
    let scratch = tempfile::tempdir().expect("scratch");
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "fallback");
    for flag in [
        "--body",
        "-b",
        "--title",
        "-T",
        "--label",
        "--reason",
        "--head",
        "--base",
        "--assignee",
        "--milestone",
        "--notes",
        "--limit",
        "-L",
        "--search",
        "--owner",
    ] {
        let (dir, log) = fake_gh();

        // When: `--` directly follows a value-taking flag (gh reads it as the value)
        // and a -R follows.
        let output = knives_cmd(helper_dir.path())
            .args([
                "gh",
                "--",
                "pr",
                "comment",
                "5",
                flag,
                "--",
                "-R",
                "acme/widgets",
            ])
            .current_dir(scratch.path())
            .env_remove("GH_TOKEN")
            .env_remove("GH_REPO")
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");

        // Then: the -R after it still routes to acme.
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(recorded.contains("GH_TOKEN=tok-acme"), "{flag}: {recorded}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("no target repository"), "{flag}: {stderr}");
    }
}

#[test]
fn gh_exit_code_is_propagated() {
    // Given: a gh executable that exits with a nonzero code.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh_exiting(4);

    // When: knives invokes it.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: knives preserves gh's status.
    assert_eq!(output.status.code(), Some(4));
}

#[test]
fn gh_signal_exit_status_is_relayed_with_shell_convention() {
    // Given: a gh executable that terminates itself with SIGTERM.
    let scratch = tempfile::tempdir().expect("scratch");
    let gh = scratch.path().join("gh");
    fs::write(&gh, "#!/bin/sh\nkill -TERM $$\n").expect("write fake gh");
    fs::set_permissions(&gh, fs::Permissions::from_mode(0o755)).expect("chmod fake gh");

    // When: knives invokes gh outside a jj repository.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", &gh)
        .output()
        .expect("run knives gh");

    // Then: the conventional signal-derived status is relayed to the shell.
    assert_eq!(output.status.code(), Some(143));
}

#[test]
fn empty_real_gh_override_falls_back_to_path_discovery() {
    // Given: PATH contains an executable fake gh and the override is empty.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: knives discovers gh.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", "")
        .env("PATH", helper_path(dir.path()))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: it ignores the empty override and invokes PATH's gh.
    assert!(output.status.success());
    assert!(log.exists(), "PATH fake gh should run");
}

#[test]
fn routed_invocation_mints_token_and_explicit_token_is_preserved() {
    // Given: a credential helper that routes the acme owner.
    let lab = lab::Lab::new();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "acme");
    let path = helper_path(helper_dir.path());

    // When: a routed request has no explicit token.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "repos/acme/work/pulls"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run routed knives gh");

    // Then: the child receives the minted token.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-acme"), "{recorded}");

    // When: the caller supplies a token.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "repos/acme/work/pulls"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env("GH_TOKEN", "user-token")
        .output()
        .expect("run knives gh with explicit token");

    // Then: the explicit token wins.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran again");
    assert!(recorded.contains("GH_TOKEN=user-token"), "{recorded}");
}

#[test]
fn auth_commands_pass_through_without_a_routed_token() {
    // Given: a credential helper that routes the acme owner, and a cwd it routes.
    let lab = lab::Lab::new();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "acme");
    let path = helper_path(helper_dir.path());

    // When: git's fallback credential helper asks gh for the user's own credential.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "auth", "git-credential", "get"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh auth git-credential");

    // Then: gh runs with no App token, so an owner the App is not installed on gets no
    // credential instead of the cwd repo's.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    assert!(
        recorded.starts_with("auth\ngit-credential\nget\n"),
        "{recorded}"
    );

    // When: an auth invocation whose values spell a PR subcommand.
    let output = knives_cmd(helper_dir.path())
        .args([
            "gh",
            "--",
            "auth",
            "token",
            "--hostname",
            "pr",
            "--user",
            "view",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", &path)
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh auth token");

    // Then: it reaches gh verbatim rather than the `gh pr view` bookmark path.
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let recorded = fs::read_to_string(&log).expect("fake gh ran again");
    assert!(
        recorded.starts_with("auth\ntoken\n--hostname\npr\n--user\nview\n"),
        "{recorded}"
    );
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn repo_flag_routes_token_even_when_cwd_remotes_differ() {
    // Given: an acme credential route and a jj repo with unrelated remotes.
    let lab = lab::Lab::new();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "acme");

    // When: -R names acme/work without a REST-path owner signal.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "-R", "acme/work", "rate_limit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: -R selected the acme credential route.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-acme"), "{recorded}");
}

#[test]
fn resolved_base_remote_routes_token_from_remote_url() {
    // Given: a remote tagged as the base with an acme URL owner, unlike the base spec.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &[
            "remote.token-base.url",
            &format!("https://{host}/acme/work.git"),
        ],
    );
    git_config(&lab.work, &["remote.token-base.gh-resolved", "base"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "acme");

    // When: gh has no explicit target.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: its base URL selects acme's credential route.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-acme"), "{recorded}");
}

#[test]
fn resolved_remote_spec_routes_token_from_spec_url() {
    // Given: a remote that resolves to a different owner/repository spec.
    let lab = lab::Lab::new();
    git_config(&lab.work, &["remote.token-spec.gh-resolved", "other/repo"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "other");

    // When: gh resolves the current repository.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: the spec owner selects the other credential route.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-other"), "{recorded}");
}

#[test]
fn registered_fork_roles_beat_nonstandard_remote_names() {
    // Given: a registered repo whose upstream role names routed-a over routed-b and a wrong remote.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &[
            "remote.upstream.url",
            &format!("https://{host}/wrong-owner/x.git"),
        ],
    );
    git_config(
        &lab.work,
        &[
            "remote.legacy-primary.url",
            &format!("https://{host}/wrong/one.git"),
        ],
    );
    git_config(
        &lab.work,
        &[
            "remote.scratch-clone.url",
            &format!("https://{host}/wrong/two.git"),
        ],
    );
    // The checkout is the registered fork because its `upstream` names the same
    // repository as the entry, in the https spelling of the registry's git@ URL.
    git_config(
        &lab.work,
        &[
            "remote.upstream.url",
            &format!("https://{host}/routed-a/upstream.git"),
        ],
    );
    let config_home = tempfile::tempdir().expect("config home");
    fs::write(
        config_home.path().join("repos.toml"),
        format!(
            "[repos.registered]\nupstream = \"git@{host}:routed-a/upstream.git\"\norigin = \"git@{host}:routed-b/origin.git\"\n"
        ),
    )
    .expect("write registry");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");

    // When: gh resolves the registered repository.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("HOME", lab.temp_path())
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: the registry's upstream role routes the token to routed-a.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
}

#[test]
fn pr_create_without_bookmark_fails_without_invoking_gh() {
    // Given: a jj working copy with no bookmark and a fake gh.
    let lab = lab::Lab::new();
    lab.jj_work(["new"]);
    let (dir, log) = fake_gh();

    // When: gh pr create needs a head bookmark.
    let output = knives_cmd(dir.path())
        .args(["gh", "--", "pr", "create"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: knives emits the exact diagnostic before spawning gh.
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        output.stderr,
        b"Error: No jj bookmark at current change (@)\n\nCreate one with:\n  jj bookmark create <name>\n\nOr push and create in one step:\n  jj git push --named=<name>=@\n"
    );
    assert!(!log.exists(), "fake gh should not run");
}

#[test]
fn pr_create_appends_bookmark_as_head() {
    // Given: a jj working copy on a bookmarked feature.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let (dir, log) = fake_gh();

    // When: gh pr create has no --head.
    let output = knives_cmd(dir.path())
        .args(["gh", "--", "pr", "create", "--title", "Alpha"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: knives appends the current bookmark as --head.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    let lines: Vec<&str> = recorded.lines().collect();
    assert_eq!(&lines[..5], &["pr", "create", "--title", "Alpha", "--head"]);
    assert_eq!(lines[5], "feat/alpha");
}

/// Runs `knives gh -- <args>` from a jj working copy on bookmark `feat/alpha` and
/// returns the arguments the fake gh received (its log minus the trailing probes).
fn gh_args_seen_on_a_bookmark(args: &[&str]) -> Vec<String> {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let (dir, log) = fake_gh();
    let output = knives_cmd(dir.path())
        .args(["gh", "--"])
        .args(args)
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");
    assert!(output.status.success(), "{:?}", output.stderr);
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    recorded
        .lines()
        .take_while(|line| !line.starts_with("GH_TOKEN="))
        .map(str::to_owned)
        .collect()
}

#[test]
fn a_pr_command_after_the_terminator_is_a_search_term_not_a_subcommand() {
    // Given/When: gh search whose terms after `--` read `pr view`.
    let seen = gh_args_seen_on_a_bookmark(&["search", "issues", "--", "pr", "view"]);

    // Then: no bookmark is injected.
    assert_eq!(seen, ["search", "issues", "--", "pr", "view"]);
}

#[test]
fn a_positional_after_the_terminator_is_the_pr_target() {
    // Given/When: `pr view` whose target follows `--`.
    let seen = gh_args_seen_on_a_bookmark(&["pr", "view", "--", "123"]);

    // Then: no bookmark is injected next to the explicit target.
    assert_eq!(seen, ["pr", "view", "--", "123"]);
}

#[test]
fn a_head_word_after_the_terminator_is_not_the_head_flag() {
    // Given/When: `pr create` with `--head` only after `--`.
    let seen = gh_args_seen_on_a_bookmark(&["pr", "create", "--title", "A", "--", "--head", "x"]);

    // Then: the bookmark's --head lands before the terminator.
    assert_eq!(
        seen,
        [
            "pr",
            "create",
            "--title",
            "A",
            "--head",
            "feat/alpha",
            "--",
            "--head",
            "x"
        ]
    );
}

#[test]
fn pr_create_does_not_duplicate_explicit_head() {
    // Given: a jj working copy and an explicit --head.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let (dir, log) = fake_gh();

    // When: gh pr create already has --head.
    let output = knives_cmd(dir.path())
        .args(["gh", "--", "pr", "create", "--head", "explicit"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: gh receives precisely one --head flag.
    assert!(output.status.success());
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert_eq!(recorded.lines().filter(|line| *line == "--head").count(), 1);
}

/// A marked shim `gh` that records its arguments and exits 99 without running anything.
fn marker_shim() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().expect("marker shim dir");
    let log = dir.path().join("shim.log");
    let shim = dir.path().join("gh");
    fs::write(
        &shim,
        "#!/bin/sh\n# knives-gh-shim\nprintf '%s\\n' \"$@\" > \"$SHIM_LOG\"\nexit 99\n",
    )
    .expect("write marker shim");
    fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).expect("chmod marker shim");
    (dir, log)
}

#[test]
fn direct_invocation_re_enters_the_marker_shim_with_the_original_arguments() {
    // Given: PATH begins with the marked shim and then a real fake gh, and no depth
    // marker: knives was invoked directly, not by the shim.
    let scratch = tempfile::tempdir().expect("scratch");
    let (shim_dir, shim_log) = marker_shim();
    let (real_dir, gh_log) = fake_gh();
    let path = format!(
        "{}:{}:{}",
        shim_dir.path().display(),
        real_dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives gh runs.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit", "--jq", ".rate"])
        .current_dir(scratch.path())
        .env("PATH", path)
        .env("SHIM_LOG", &shim_log)
        .env("FAKE_GH_LOG", &gh_log)
        .env_remove("KNIVES_GH_SHIM_DEPTH")
        .env_remove("KNIVES_REAL_GH")
        .output()
        .expect("run knives gh");

    // Then: the shim got the call verbatim and its status is knives'; the real gh never ran.
    assert_eq!(output.status.code(), Some(99));
    assert_eq!(
        fs::read_to_string(&shim_log).expect("shim ran"),
        "api\nrate_limit\n--jq\n.rate\n"
    );
    assert!(
        !gh_log.exists(),
        "the real gh must not run on the direct pass"
    );
}

#[test]
fn a_hand_set_override_without_the_depth_marker_still_re_enters_the_shim() {
    // Given: the same shim-first PATH and KNIVES_REAL_GH set by hand, without the depth
    // marker only the shim sets.
    let scratch = tempfile::tempdir().expect("scratch");
    let (shim_dir, shim_log) = marker_shim();
    let (real_dir, gh_log) = fake_gh();
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives gh runs.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "user"])
        .current_dir(scratch.path())
        .env("PATH", path)
        .env("SHIM_LOG", &shim_log)
        .env("FAKE_GH_LOG", &gh_log)
        .env_remove("KNIVES_GH_SHIM_DEPTH")
        .env("KNIVES_REAL_GH", real_dir.path().join("gh"))
        .output()
        .expect("run knives gh");

    // Then: the override is no way around the shim; it gets the call and gh never runs here.
    assert_eq!(output.status.code(), Some(99));
    assert_eq!(
        fs::read_to_string(&shim_log).expect("shim ran"),
        "api\nuser\n"
    );
    assert!(
        !gh_log.exists(),
        "the override must not run the real gh on the direct pass"
    );
}

#[test]
fn a_shim_re_entry_with_the_depth_marker_runs_the_real_gh() {
    // Given: the same shim-first PATH, but the depth marker and KNIVES_REAL_GH set as
    // the shim sets them.
    let scratch = tempfile::tempdir().expect("scratch");
    let (shim_dir, shim_log) = marker_shim();
    let (real_dir, gh_log) = fake_gh();
    let path = format!(
        "{}:{}",
        shim_dir.path().display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives gh runs as the shim's re-entry.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(scratch.path())
        .env("PATH", path)
        .env("SHIM_LOG", &shim_log)
        .env("FAKE_GH_LOG", &gh_log)
        .env("KNIVES_GH_SHIM_DEPTH", "1")
        .env("KNIVES_REAL_GH", real_dir.path().join("gh"))
        .output()
        .expect("run knives gh");

    // Then: the override is used and the shim is not re-entered.
    assert!(output.status.success());
    assert!(gh_log.exists(), "the real gh should run");
    assert!(
        !shim_log.exists(),
        "the shim must not be re-entered a second time"
    );
}

#[test]
fn a_helper_refusal_is_relayed_and_stops_gh() {
    // Given: a routed helper that refuses the way gh-app-token does for an owner it
    // cannot serve: reason on stderr, quit=1 on stdout, exit 1.
    let lab = lab::Lab::new();
    let (dir, log) = fake_gh();
    let helper_dir = tempfile::tempdir().expect("fake helper dir");
    let helper = helper_dir.path().join("gh-app-token");
    fs::write(
        &helper,
        "#!/bin/sh\ncat > /dev/null\necho \"gh-app-token: no App installation for owner acme and no fallback-secret on profile $1\" >&2\nprintf 'quit=1\\n'\nexit 1\n",
    )
    .expect("write refusing gh-app-token");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("chmod helper");
    let gitconfig = token_config(helper_dir.path(), "agent");

    // When: a routed request is refused.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "repos/acme/work/pulls"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run refused knives gh");

    // Then: the helper's reason reaches the caller, its status is knives', and gh never runs.
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "gh-app-token: no App installation for owner acme and no fallback-secret on profile agent\n"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(
        !log.exists(),
        "gh must not run on its own auth after a refusal"
    );
}

#[test]
fn a_non_utf8_helper_answer_is_refused_and_stops_gh() {
    // Given: a routed helper that exits 0 with an answer knives cannot read as UTF-8.
    let lab = lab::Lab::new();
    let (dir, log) = fake_gh();
    let helper_dir = tempfile::tempdir().expect("fake helper dir");
    let helper = helper_dir.path().join("gh-app-token");
    fs::write(
        &helper,
        "#!/bin/sh\ncat > /dev/null\nprintf 'password=\\377\\376\\n'\n",
    )
    .expect("write non-UTF-8 gh-app-token");
    fs::set_permissions(&helper, fs::Permissions::from_mode(0o755)).expect("chmod helper");
    let gitconfig = token_config(helper_dir.path(), "agent");

    // When: a routed request gets that answer.
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "repos/acme/work/pulls"])
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    // Then: knives says what it could not read, exits 1, and gh never runs on its own auth.
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "knives gh: gh-app-token agent answered non-UTF-8\n"
    );
    assert!(output.stdout.is_empty(), "{:?}", output.stdout);
    assert!(
        !log.exists(),
        "gh must not run on its own auth after an unreadable answer"
    );
}

#[test]
fn nonexistent_real_gh_path_reports_not_found() {
    // Given: KNIVES_REAL_GH points at no executable.
    let scratch = tempfile::tempdir().expect("scratch");
    let missing = scratch.path().join("missing-gh");

    // When: knives invokes gh.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(scratch.path())
        .env("KNIVES_REAL_GH", missing)
        .output()
        .expect("run knives gh");

    // Then: its discovery diagnostic and shell-compatible exit status are preserved.
    assert_eq!(output.status.code(), Some(127));
    assert!(String::from_utf8_lossy(&output.stderr).contains("knives gh: real gh not found"));
}

/// A terminal `git` under `scripts/terminal` that records each `git remote` call as one
/// line of `scripts/terminal-git.log` and answers everything else with silent success.
/// Returns its directory and that log.
fn terminal_git(scripts: &Path) -> (PathBuf, PathBuf) {
    let terminal = scripts.join("terminal");
    fs::create_dir(&terminal).expect("terminal directory");
    let log = scripts.join("terminal-git.log");
    let git = terminal.join("git");
    fs::write(
        &git,
        format!(
            "#!/bin/sh\nif [ \"$1\" = remote ]; then\n    printf 'terminal git\\n' >> '{}'\nfi\n",
            log.display()
        ),
    )
    .expect("write terminal git");
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).expect("chmod terminal git");
    (terminal, log)
}

/// `knives gh -- pr view` in `lab` with `real_gh` on `path`, under a 10 s `timeout`: the
/// regression these scenarios guard is two git wrappers exec'ing each other forever.
fn bounded_pr_view(lab: &lab::Lab, scripts: &Path, real_gh: &Path, path: &str) -> Command {
    let mut command = Command::new("timeout");
    command.args(["10", env!("CARGO_BIN_EXE_knives"), "gh", "--", "pr", "view"]);
    knives_env(&mut command, scripts);
    command
        .current_dir(&lab.work)
        .env("KNIVES_REAL_GH", real_gh)
        .env("PATH", path);
    command
}

#[test]
fn stacked_git_wrappers_reach_terminal_git_once() {
    // Given: a bookmarked repo, a foreign wrapper left by an older gh shim, and terminal git.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let scripts = tempfile::tempdir().expect("script directory");
    let (terminal, terminal_log) = terminal_git(scripts.path());
    let foreign_wrapper = scripts.path().join("foreign-wrapper");
    fs::create_dir(&foreign_wrapper).expect("foreign wrapper directory");
    let real_gh = scripts.path().join("gh");
    fs::write(
        &real_gh,
        "#!/bin/sh\nexport _JJ_WRAPPER_DIR=\"$FOREIGN_WRAPPER_DIR\"\n\
         export PATH=\"$FOREIGN_WRAPPER_DIR:$PATH\"\ngit remote -v\n",
    )
    .expect("write real gh");
    fs::set_permissions(&real_gh, fs::Permissions::from_mode(0o755)).expect("chmod real gh");
    let foreign_git = foreign_wrapper.join("git");
    fs::write(
        &foreign_git,
        r#"#!/bin/bash
IFS=':' read -ra _path_dirs <<< "$PATH"
for _d in "${_path_dirs[@]}"; do
    [[ "$_d" == "$_JJ_WRAPPER_DIR" ]] && continue
    [[ -x "$_d/git" ]] && exec "$_d/git" "$@"
done
echo "error: git not found" >&2
exit 127
"#,
    )
    .expect("write foreign git wrapper");
    fs::set_permissions(&foreign_git, fs::Permissions::from_mode(0o755))
        .expect("chmod foreign git wrapper");
    let path = format!(
        "{}:{}",
        terminal.display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives prepends its wrapper before invoking a real gh that passes through to git.
    let output = bounded_pr_view(&lab, scripts.path(), &real_gh, &path)
        .env("FOREIGN_WRAPPER_DIR", &foreign_wrapper)
        .output()
        .expect("run timeout-bounded knives gh");

    // Then: each wrapper skips itself and the terminal git receives the passthrough once.
    assert!(
        output.status.success(),
        "knives gh failed ({:?}, 124 is the timeout): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&terminal_log)
            .expect("terminal git ran")
            .lines()
            .count(),
        1
    );
}

#[test]
fn a_content_scanning_git_shim_below_the_wrapper_reaches_terminal_git_once() {
    // Given: a bookmarked repo, a real gh that runs `git remote -v`, and below knives'
    // wrapper on PATH a shim that finds "the real git" by scanning PATH from the top for
    // the first git whose content lacks its own marker. Handed a PATH it is still on, it
    // picks the wrapper above it, which hands straight back; a re-entered shim names the
    // git it chose and exits 1, so that ring is a one-line failure instead of a hang.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let scripts = tempfile::tempdir().expect("script directory");
    let (terminal, terminal_log) = terminal_git(scripts.path());
    let content_shim = scripts.path().join("content-shim");
    fs::create_dir(&content_shim).expect("content shim directory");
    let real_gh = scripts.path().join("gh");
    fs::write(&real_gh, "#!/bin/sh\ngit remote -v\n").expect("write real gh");
    fs::set_permissions(&real_gh, fs::Permissions::from_mode(0o755)).expect("chmod real gh");
    let shim_git = content_shim.join("git");
    fs::write(
        &shim_git,
        r#"#!/bin/bash
# content-scanning-shim
if [[ -n "$CONTENT_SHIM_CHOSE" ]]; then
    echo "content-scanning shim re-entered after choosing $CONTENT_SHIM_CHOSE" >&2
    exit 1
fi
IFS=':' read -ra _path_dirs <<< "$PATH"
for _d in "${_path_dirs[@]}"; do
    [[ -x "$_d/git" ]] || continue
    _header=""
    IFS= read -r -n 512 -d '' _header < "$_d/git" || true
    [[ "$_header" == *content-scanning-shim* ]] && continue
    CONTENT_SHIM_CHOSE="$_d/git" exec "$_d/git" "$@"
done
echo "error: git not found" >&2
exit 127
"#,
    )
    .expect("write content-scanning git shim");
    fs::set_permissions(&shim_git, fs::Permissions::from_mode(0o755))
        .expect("chmod content-scanning git shim");
    let path = format!(
        "{}:{}:{}",
        content_shim.display(),
        terminal.display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives prepends its wrapper before invoking a real gh that passes through to git.
    let output = bounded_pr_view(&lab, scripts.path(), &real_gh, &path)
        .output()
        .expect("run timeout-bounded knives gh");

    // Then: the wrapper hands the shim a PATH it is no longer on, so the shim's scan
    // lands on the terminal git, which receives the passthrough once.
    assert!(
        output.status.success(),
        "knives gh failed ({:?}, 124 is the timeout): {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&terminal_log)
            .expect("terminal git ran")
            .lines()
            .count(),
        1
    );
}

/// A fake gh that records each run's token and arguments in `$FAKE_DIR/calls`
/// and the branch git reports to it in `$FAKE_DIR/branches`; it watches (sleeps
/// three seconds, then exits 0) on its first run and on any later run exits at
/// once with `$FAKE_EXIT`, gh's own status for the watch's end. A helper that
/// answers `tok-1`, `tok-2`, … one per mint, and refuses as gh-app-token does
/// once it has minted `$FAKE_MINT_LIMIT` tokens.
fn watching_fakes() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("fake dir");
    let gh = dir.path().join("gh");
    fs::write(
        &gh,
        "#!/bin/sh\nn=$(($(cat \"$FAKE_DIR/gh-runs\" 2>/dev/null || echo 0) + 1))\necho \"$n\" > \"$FAKE_DIR/gh-runs\"\nprintf '%s %s\\n' \"$GH_TOKEN\" \"$*\" >> \"$FAKE_DIR/calls\"\ngit symbolic-ref --short HEAD >> \"$FAKE_DIR/branches\" 2>/dev/null\nif [ \"$n\" -gt 1 ]; then exit \"$FAKE_EXIT\"; fi\nexec sleep 3\n",
    )
    .expect("write watching fake gh");
    let helper = dir.path().join("gh-app-token");
    fs::write(
        &helper,
        "#!/bin/sh\ncat > /dev/null\nn=$(($(cat \"$FAKE_DIR/mints\" 2>/dev/null || echo 0) + 1))\necho \"$n\" > \"$FAKE_DIR/mints\"\nif [ -n \"$FAKE_MINT_LIMIT\" ] && [ \"$n\" -gt \"$FAKE_MINT_LIMIT\" ]; then echo 'gh-app-token: GitHub API 502' >&2; echo quit=1; exit 1; fi\nprintf 'username=x-access-token\\npassword=tok-%s\\n' \"$n\"\n",
    )
    .expect("write counting gh-app-token");
    for script in [&gh, &helper] {
        fs::set_permissions(script, fs::Permissions::from_mode(0o755)).expect("chmod fake");
    }
    token_config(dir.path(), "agent");
    dir
}

/// What a call through the [`watching_fakes`] left: knives' output, the runs
/// gh recorded, and the branches git reported to them.
struct Watched {
    output: std::process::Output,
    calls: String,
    branches: String,
}

/// Runs `arguments` through knives in `cwd` (a scratch directory when None)
/// with the [`watching_fakes`] and `env`, restarting a watch after one second.
fn run_watching(arguments: &[&str], cwd: Option<&Path>, env: &[(&str, &str)]) -> Watched {
    let fakes = watching_fakes();
    let scratch = tempfile::tempdir().expect("scratch");
    let output = knives_cmd(fakes.path())
        .args(["gh", "--"])
        .args(arguments)
        .current_dir(cwd.unwrap_or_else(|| scratch.path()))
        .env("KNIVES_REAL_GH", fakes.path().join("gh"))
        .env("KNIVES_GH_WATCH_RESTART_SECS", "1")
        .env("FAKE_DIR", fakes.path())
        .env("FAKE_EXIT", "1")
        .env("PATH", helper_path(fakes.path()))
        .envs(env.iter().copied())
        .output()
        .expect("run knives gh");
    let read = |name: &str| fs::read_to_string(fakes.path().join(name)).unwrap_or_default();
    Watched {
        output,
        calls: read("calls"),
        branches: read("branches"),
    }
}

#[test]
fn a_watch_that_outlives_its_run_window_runs_again_on_a_fresh_token() {
    // Given/When: a watch still running when its window (here one second) ends,
    // whose rerun ends as `gh run watch --exit-status` does for a failed run.
    let watched = run_watching(
        &["run", "watch", "9", "-R", "acme/widgets", "--exit-status"],
        None,
        &[],
    );

    // Then: knives ended it, ran the same command on a newly minted token, said so
    // once, and exits with the last run's own status.
    assert_eq!(
        watched.calls,
        "tok-1 run watch 9 -R acme/widgets --exit-status\ntok-2 run watch 9 -R acme/widgets --exit-status\n"
    );
    let stderr = String::from_utf8_lossy(&watched.output.stderr);
    assert_eq!(
        stderr.matches("running it again on a fresh one").count(),
        1,
        "{stderr}"
    );
    assert_eq!(watched.output.status.code(), Some(1), "{stderr}");
}

#[test]
fn a_restarted_pr_checks_watch_keeps_the_bookmark_and_gh_s_pending_status() {
    // Given: a jj checkout on a bookmarked change, where knives names the PR by
    // its bookmark and answers gh's HEAD reads through its git wrapper.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);

    // When: `gh pr checks --watch` outlives its window and its rerun ends with
    // gh's exit 8, checks still pending.
    let watched = run_watching(
        &["pr", "checks", "--watch", "-R", "acme/widgets"],
        Some(&lab.work),
        &[("FAKE_EXIT", "8")],
    );

    // Then: both runs got the bookmark and the wrapper's branch, and the call
    // exits 8.
    assert_eq!(
        watched.calls,
        "tok-1 pr checks feat/alpha --watch -R acme/widgets\ntok-2 pr checks feat/alpha --watch -R acme/widgets\n"
    );
    assert_eq!(watched.branches, "feat/alpha\nfeat/alpha\n");
    assert_eq!(
        watched.output.status.code(),
        Some(8),
        "{:?}",
        watched.output
    );
}

#[test]
fn a_refused_renewal_ends_the_watch_with_the_helpers_reason() {
    // Given/When: a watch outlives its window, and the helper then refuses.
    let watched = run_watching(
        &["run", "watch", "9", "-R", "acme/widgets"],
        None,
        &[("FAKE_MINT_LIMIT", "1")],
    );

    // Then: gh is not run again, the helper's reason and exit stand, and no line
    // claims a restart.
    assert_eq!(watched.calls, "tok-1 run watch 9 -R acme/widgets\n");
    let stderr = String::from_utf8_lossy(&watched.output.stderr);
    assert!(stderr.contains("gh-app-token: GitHub API 502"), "{stderr}");
    assert!(!stderr.contains("running it again"), "{stderr}");
    assert_eq!(watched.output.status.code(), Some(1), "{stderr}");
}

#[test]
fn only_a_rerunnable_watch_on_a_token_knives_minted_is_restarted() {
    for (arguments, env) in [
        // Not a watch: it runs to its end however long it takes.
        (&["run", "view", "9", "-R", "acme/widgets"][..], &[][..]),
        (&["pr", "checks", "7", "-R", "acme/widgets"], &[]),
        // A rerun would submit a second task, or print the whole log again.
        (
            &[
                "agent-task",
                "create",
                "do it",
                "--follow",
                "-R",
                "acme/widgets",
            ],
            &[],
        ),
        (
            &[
                "agent-task",
                "view",
                "1",
                "--log",
                "--follow",
                "-R",
                "acme/widgets",
            ],
            &[],
        ),
        // A watch on an inherited GH_TOKEN: knives cannot renew a token it did not mint.
        (
            &["run", "watch", "9", "-R", "acme/widgets"],
            &[("GH_TOKEN", "ghs_inherited")],
        ),
    ] {
        // When: the call runs past the one-second window.
        let watched = run_watching(arguments, None, env);

        // Then: gh ran once, to its own end.
        assert_eq!(
            watched.calls.lines().count(),
            1,
            "{arguments:?}: {}",
            watched.calls
        );
        assert_eq!(
            watched.output.status.code(),
            Some(0),
            "{arguments:?}: {:?}",
            watched.output
        );
    }
}
