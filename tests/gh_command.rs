#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
// allow: SIZE_OK: 905 lines - real-binary gh passthrough scenarios share one fixture and process harness.

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
    command
        .env("GIT_CONFIG_GLOBAL", scratch.join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KNIVES_CONFIG_HOME", scratch)
        .env("HOME", scratch)
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_GH_SHIM_DEPTH", "1")
        .env_remove("GH_TOKEN");
    command
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
        .env("JJ_CONFIG", "/dev/null")
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

/// A config home whose registry forks `<host>/routed-a/upstream`, plus its path,
/// for the placement-gate scenarios. The ledger for the entry sits beside it.
fn placement_gate_home() -> tempfile::TempDir {
    let host = concat!("github", ".com");
    let config_home = tempfile::tempdir().expect("config home");
    fs::write(
        config_home.path().join("repos.toml"),
        format!(
            "[repos.registered]\nupstream = \"git@{host}:routed-a/upstream.git\"\norigin = \"git@{host}:routed-b/origin.git\"\n"
        ),
    )
    .expect("write registry");
    config_home
}

/// Record a placement verdict for `branch` in the `registered` entry's ledger.
fn record_placement(config_home: &Path, branch: &str, verdict: &str) {
    let placement = knives::placement::Placement::parse(&format!(
        "verdict: {verdict}\nalternative: a consumer-side setting; the library exposes none for \
         this\nclass: gap-others-need\njudge: lab-red-team\n"
    ))
    .expect("parse verdict");
    knives::ledger::Ledger::at(config_home.join("ledger").join("registered"))
        .append(&knives::ledger::Entry {
            ts: "2026-09-17T10:00:00Z".to_owned(),
            owner: "lab".to_owned(),
            subject: Some(branch.to_owned()),
            kind: knives::ledger::Kind::Note,
            disposition: None,
            text: placement.note_text(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        })
        .expect("record verdict");
}

#[test]
fn an_upstream_pr_create_without_a_verdict_is_refused_before_gh_runs() {
    // Given: the registered upstream as the explicit target, and no ledger note.
    let config_home = placement_gate_home();
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    // When: a pull request is created there for a branch with no verdict.
    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--head",
            "feat/gamma",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: refused with the member text, and gh never ran.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        format!(
            "knives gh: {}",
            knives::placement::missing_member_refusal("feat/gamma")
        )
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn an_upstream_pr_create_for_a_fork_verdict_names_the_verdict_in_the_refusal() {
    // Given: the branch's recorded verdict says FORK: it rides the release cut
    // and never becomes an upstream pull request.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "FORK");
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--head",
            "feat/gamma",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("has placement verdict FORK, not UPSTREAM"),
        "{stderr}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn an_upstream_pr_create_with_an_upstream_verdict_passes_through() {
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");

    let output = knives_cmd(helper_dir.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--head",
            "feat/gamma",
        ])
        .current_dir(helper_dir.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("feat/gamma"), "{recorded}");
}

#[test]
fn a_pr_create_toward_a_repository_that_is_no_registered_upstream_passes() {
    // The verdict governs upstream pull requests; the fork's own origin (and any
    // unregistered repository) is not upstream's business.
    let config_home = placement_gate_home();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-b");

    let output = knives_cmd(helper_dir.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-b/origin",
            "--head",
            "feat/gamma",
        ])
        .current_dir(helper_dir.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    assert!(output.status.success(), "{output:?}");
    assert!(log.exists(), "gh should run for a non-upstream target");
}

#[test]
fn a_rest_pull_creation_against_the_upstream_is_gated_like_pr_create() {
    // Given: `gh api repos/<upstream>/pulls` with a body — REST creation — for a
    // branch whose verdict is FORK.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "FORK");
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "api",
            "repos/routed-a/upstream/pulls",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
            "-f",
            "base=main",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: refused, naming the verdict; listing the same endpoint still works.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("has placement verdict FORK, not UPSTREAM"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");

    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let listed = knives_cmd(helper_dir.path())
        .args(["gh", "--", "api", "repos/routed-a/upstream/pulls"])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");
    assert!(listed.status.success(), "{listed:?}");
    assert!(log.exists(), "a bodyless GET must pass");
}

#[test]
fn a_rest_pull_creation_is_gated_with_flags_before_its_path() {
    // `gh api -X POST repos/…/pulls` is how GitHub's documentation spells it; a
    // method, or a header, before the path is the same request; so is one
    // whose fields are attached to their flags (gh infers POST from them), or
    // whose path carries a `#fragment` the server never sees.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "FORK");
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();
    let spellings: [&[&str]; 5] = [
        &[
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
            "-f",
            "base=main",
        ],
        &[
            "--method",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
            "-f",
            "base=main",
        ],
        &[
            "-H",
            "Accept: application/vnd.github+json",
            "repos/routed-a/upstream/pulls",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
            "-f",
            "base=main",
        ],
        &[
            "repos/routed-a/upstream/pulls",
            "-ftitle=x",
            "-fhead=feat/gamma",
            "-fbase=main",
        ],
        &[
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls#x",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
        ],
    ];
    for spelling in spellings {
        let output = knives_cmd(scratch.path())
            .args(["gh", "--", "api"])
            .args(spelling)
            .current_dir(scratch.path())
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh");
        assert_eq!(output.status.code(), Some(2), "{spelling:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("has placement verdict FORK, not UPSTREAM"),
            "{spelling:?}: {output:?}"
        );
        assert!(!log.exists(), "{spelling:?}: gh ran despite the refusal");
    }
}

#[test]
fn a_rest_pull_creation_by_numeric_repository_id_is_refused_and_routes_no_token() {
    // `repositories/<id>/pulls` is the same creation endpoint addressed by
    // GitHub's numeric id, which names no owner: the gate cannot check it and
    // refuses; a GET of it lists, runs, and is handed no routed token — least
    // of all the checkout's upstream one.
    let config_home = placement_gate_home();
    let lab = fork_checkout_of_the_registered_upstream();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "api"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };
    let host = concat!("api.github", ".com");
    let absolute = format!("https://{host}/repositories/1318902388/pulls");
    let spellings: [&[&str]; 3] = [
        &[
            "-X",
            "POST",
            "repositories/1318902388/pulls",
            "-f",
            "title=t",
            "-f",
            "head=feat/eps",
            "-f",
            "base=main",
        ],
        &[
            "repositories/1318902388/pulls",
            "-ftitle=t",
            "-fhead=feat/eps",
            "-fbase=main",
        ],
        &[
            "-X",
            "POST",
            absolute.as_str(),
            "-f",
            "head=feat/eps",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
    ];
    for spelling in spellings {
        let output = run(spelling);
        assert_eq!(output.status.code(), Some(2), "{spelling:?}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim_end(),
            "knives gh: a pull request creation by numeric repository id (1318902388) cannot be \
             checked against the registry: state the repository as repos/<owner>/<repo>",
            "{spelling:?}"
        );
        assert!(!log.exists(), "{spelling:?}: gh ran despite the refusal");
    }

    // A GET is not a creation: it runs, and no token is minted for a path
    // that names no owner.
    let listed = run(&["repositories/1318902388/pulls"]);
    assert!(listed.status.success(), "{listed:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn a_repo_flag_before_the_pr_verb_is_still_a_pr_create() {
    // `-R` is a persistent flag on `gh pr`, so cobra accepts `pr -R o/r create`;
    // the gate and the head injection must both see the `create`.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |middle: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "pr"])
            .args(middle)
            .args(["--title", "t", "--body", "b"])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };

    // Then: with no head stated, @'s FORK bookmark is gated and refused, in
    // either spelling of the flag.
    for middle in [
        &["-R", "routed-a/upstream", "create"][..],
        &["--repo", "routed-a/upstream", "create"][..],
    ] {
        let refused = run(middle);
        assert_eq!(refused.status.code(), Some(2), "{middle:?}: {refused:?}");
        assert!(
            String::from_utf8_lossy(&refused.stderr)
                .contains("feat/eps has placement verdict FORK"),
            "{middle:?}: {refused:?}"
        );
        assert!(!log.exists(), "{middle:?}: gh ran despite the refusal");
    }

    // And: an UPSTREAM head stated after the verb reaches gh as the one head.
    let passed = run(&["-R", "routed-a/upstream", "create", "-H", "feat/gamma"]);
    assert!(passed.status.success(), "{passed:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    let argv: Vec<&str> = recorded
        .lines()
        .take_while(|line| !line.starts_with("GH_TOKEN="))
        .collect();
    assert_eq!(
        argv.iter()
            .filter(|a| **a == "--head" || **a == "-H")
            .count(),
        1,
        "{recorded}"
    );
    assert!(!argv.contains(&"feat/eps"), "{recorded}");
}

#[test]
fn two_stated_heads_are_refused_naming_both() {
    // gh keeps the last head it is given; the gate would have read the first.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--head",
            "feat/gamma",
            "-H",
            "feat/none",
            "--title",
            "t",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("states 2 heads (feat/gamma, feat/none)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("gh would open feat/none while a reader expects feat/gamma"),
        "{stderr}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn an_upstream_pr_create_with_the_short_head_flag_is_gated() {
    let config_home = placement_gate_home();
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();

    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "-H",
            "feat/gamma",
            "--title",
            "t",
            "--body",
            "b",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        format!(
            "knives gh: {}",
            knives::placement::missing_member_refusal("feat/gamma")
        )
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_stated_head_inside_a_jj_checkout_is_the_only_head_gh_receives() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout, and feat/gamma
    // ruled UPSTREAM. gh takes the last head it is given, so a `--head
    // <current>` added behind the stated one would open feat/eps upstream —
    // the branch the gate never read.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |head: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "pr", "create", "-R", "routed-a/upstream"])
            .args(head)
            .args(["--title", "t", "--body", "b"])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };

    // Then: with no head stated, the current bookmark is gated — and refused.
    let current = run(&[]);
    assert_eq!(current.status.code(), Some(2), "{current:?}");
    assert!(
        String::from_utf8_lossy(&current.stderr).contains("feat/eps has placement verdict FORK"),
        "{current:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");

    // And: a stated head, in either spelling, is gated and is the one head gh
    // receives; nothing is added behind it.
    for head in [&["-H", "feat/gamma"][..], &["-Hfeat/gamma"][..]] {
        let output = run(head);
        assert!(output.status.success(), "{head:?}: {output:?}");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        // The fake gh logs its argv first, then its environment (`GH_TOKEN=`,
        // `BRANCH=`); only the argv is what gh was told.
        let argv: Vec<&str> = recorded
            .lines()
            .take_while(|line| !line.starts_with("GH_TOKEN="))
            .collect();
        let heads: Vec<&str> = argv
            .iter()
            .enumerate()
            .filter_map(|(index, argument)| match *argument {
                "--head" | "-H" => argv.get(index + 1).copied(),
                attached => attached
                    .strip_prefix("--head=")
                    .or_else(|| attached.strip_prefix("-H")),
            })
            .collect();
        assert_eq!(heads, ["feat/gamma"], "{head:?}: {recorded}");
        assert!(
            !argv.contains(&"feat/eps"),
            "the current bookmark reached gh: {head:?}: {recorded}"
        );
        fs::remove_file(&log).expect("reset the gh log");
    }
}

#[test]
fn heads_are_read_as_gh_parses_them_values_clusters_and_the_terminator() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout, feat/gamma
    // UPSTREAM. gh's parser gives a valued flag the next argument whatever it
    // looks like, and hands a shorthand cluster's tail to its first valued
    // shorthand; the gate must read the same head gh does, or none.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "pr", "create", "-R", "routed-a/upstream"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };
    let gh_argv = |recorded: &str| -> Vec<String> {
        recorded
            .lines()
            .take_while(|line| !line.starts_with("GH_TOKEN="))
            .map(str::to_owned)
            .collect()
    };

    // Over-read: a `-H…` that is another flag's value is not a head, so the
    // current bookmark is the head — and it is FORK, refused.
    for arguments in [
        &["--title", "t", "--body", "-Hfeat/gamma"][..],
        &["--title", "t", "--body=-Hfeat/gamma"][..],
        &["--title", "t", "-b", "-Hfeat/gamma"][..],
        &["--title", "t", "--body", "b", "--", "--head", "feat/gamma"][..],
    ] {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    }

    // Under-read: a head inside a shorthand cluster is the head gh sees; it is
    // gated (FORK refused), and an UPSTREAM one reaches gh as the only head.
    let clustered_fork = run(&["-dHfeat/eps", "--title", "t", "--body", "b"]);
    assert_eq!(clustered_fork.status.code(), Some(2), "{clustered_fork:?}");
    assert!(
        String::from_utf8_lossy(&clustered_fork.stderr)
            .contains("feat/eps has placement verdict FORK"),
        "{clustered_fork:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
    for arguments in [
        &["-dHfeat/gamma", "--title", "t", "--body", "b"][..],
        &["-fHfeat/gamma", "--title", "t"][..],
        &["-dH", "feat/gamma", "--title", "t", "--body", "b"][..],
    ] {
        let output = run(arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        let argv = gh_argv(&recorded);
        assert!(
            !argv.iter().any(|a| a == "--head"),
            "a head was added behind the stated one: {arguments:?}: {recorded}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("feat/eps")),
            "the current bookmark reached gh: {arguments:?}: {recorded}"
        );
        fs::remove_file(&log).expect("reset the gh log");
    }

    // Unknown: a flag gh does not define for `pr create` makes the head
    // unreadable; refused with the remedy, nothing runs.
    let unknown = run(&["--mystery", "x", "-H", "feat/gamma", "--title", "t"]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("knives cannot tell whether --mystery takes a value"),
        "{stderr}"
    );
    assert!(
        stderr.contains("spell the head as --head=<branch> and put it first"),
        "{stderr}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_clustered_head_in_a_plain_git_clone_is_the_head_the_gate_reads() {
    // Given: a git-only clone on feat/allowed (UPSTREAM), and feat/forked
    // ruled FORK. `-dHfeat/forked` is draft plus head to gh; the gate must
    // read that head, not the checked-out branch.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/allowed", "UPSTREAM");
    record_placement(config_home.path(), "feat/forked", "FORK");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = scratch.path().join("clone");
    lab::git_repository(&clone, &[]);
    fs::write(clone.join("README.md"), "seed\n").expect("write seed");
    lab::git_commit_all(&clone, "seed");
    let status = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["checkout", "--quiet", "-b", "feat/allowed"])
        .status()
        .expect("run git");
    assert!(status.success(), "git checkout");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "pr", "create", "-R", "routed-a/upstream"])
            .args(arguments)
            .current_dir(&clone)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };

    // Then: the clustered FORK head is refused, though the checked-out branch
    // would have passed.
    for arguments in [
        &["-dHfeat/forked", "--title", "t", "--body", "b"][..],
        &["-fHfeat/forked", "--title", "t"][..],
    ] {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("feat/forked has placement verdict FORK"),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    }
    // And: the checked-out UPSTREAM branch, stated the same way, passes.
    let passed = run(&["-dHfeat/allowed", "--title", "t", "--body", "b"]);
    assert!(passed.status.success(), "{passed:?}");
    assert!(log.exists(), "gh must run for an UPSTREAM head");
}

#[test]
fn a_pr_create_from_a_plain_git_clone_gates_the_checked_out_branch() {
    // Given: a git-only clone (an agent's /tmp checkout, no jj) on an
    // unverdicted branch; gh defaults the head to git's current branch.
    let config_home = placement_gate_home();
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = scratch.path().join("clone");
    lab::git_repository(&clone, &[]);
    fs::write(clone.join("README.md"), "seed\n").expect("write seed");
    lab::git_commit_all(&clone, "seed");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(args)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?}");
    };
    git(&["checkout", "--quiet", "-b", "feat/none"]);
    let (dir, log) = fake_gh();
    let run = || {
        knives_cmd(scratch.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                "routed-a/upstream",
                "--title",
                "t",
                "--body",
                "b",
            ])
            .current_dir(&clone)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh")
    };

    // When: a pull request is opened toward the registered upstream with no
    // head stated.
    let output = run();

    // Then: git's branch is the head, and it is refused by name.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        format!(
            "knives gh: {}",
            knives::placement::missing_member_refusal("feat/none")
        )
    );
    assert!(!log.exists(), "gh ran despite the refusal");

    // And: with nothing checked out at all, nothing verifiable is let through.
    git(&["checkout", "--quiet", "--detach"]);
    let detached = run();
    assert_eq!(detached.status.code(), Some(2), "{detached:?}");
    let stderr = String::from_utf8_lossy(&detached.stderr);
    assert!(
        stderr.contains("needs a head branch to check its placement verdict"),
        "{stderr}"
    );
    assert!(stderr.contains("`--head <branch>`"), "{stderr}");
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_rest_pull_creation_by_absolute_url_or_colon_placeholders_is_gated() {
    // gh sends an absolute URL verbatim and fills `:owner/:repo` from the
    // current directory's base repository, exactly as it does `{owner}`.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    let (dir, log) = fake_gh();
    let host = concat!("api.github", ".com");
    let absolute = format!("https://{host}/repos/routed-a/upstream/pulls");
    for path in [absolute.as_str(), "repos/:owner/:repo/pulls"] {
        let output = knives_cmd(config_home.path())
            .args(["gh", "--", "api", "-X", "POST", path])
            .args(["-f", "title=x", "-f", "head=feat/gamma", "-f", "base=main"])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh");
        assert_eq!(output.status.code(), Some(2), "{path}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("has placement verdict FORK, not UPSTREAM"),
            "{path}: {output:?}"
        );
        assert!(!log.exists(), "{path}: gh ran despite the refusal");
    }
}

/// A lab checkout whose `upstream` remote is the registered fork's upstream,
/// so commands run inside it resolve their target the way `gh` does from a
/// fork checkout: to the upstream.
fn fork_checkout_of_the_registered_upstream() -> lab::Lab {
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &[
            "remote.upstream.url",
            &format!("https://{host}/routed-a/upstream.git"),
        ],
    );
    lab
}

#[test]
fn a_rest_pull_creation_with_gh_placeholders_targets_the_fork_checkout_upstream() {
    // `repos/{owner}/{repo}/pulls` is gh's own spelling for the current
    // directory's base repository — the upstream, inside a fork checkout.
    let config_home = placement_gate_home();
    let lab = fork_checkout_of_the_registered_upstream();
    let (dir, log) = fake_gh();

    let output = knives_cmd(config_home.path())
        .args([
            "gh",
            "--",
            "api",
            "-X",
            "POST",
            "repos/{owner}/{repo}/pulls",
            "-f",
            "title=x",
            "-f",
            "head=feat/gamma",
            "-f",
            "base=main",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr).trim_end(),
        format!(
            "knives gh: {}",
            knives::placement::missing_member_refusal("feat/gamma")
        )
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_review_mutation_inside_a_fork_checkout_passes_while_the_create_mutation_is_refused() {
    let config_home = placement_gate_home();
    let lab = fork_checkout_of_the_registered_upstream();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |query: &str| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--", "api", "graphql", "-f"])
            .arg(format!("query={query}"))
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };

    // Posting a review is maintenance of a pull request already open.
    let review = run(
        "mutation { createPullRequestReview(input:{pullRequestId:\"PR\",event:COMMENT,\
         body:\"hi\"}) { clientMutationId } }",
    );
    assert!(review.status.success(), "{review:?}");
    assert!(log.exists(), "the review mutation must reach gh");
    fs::remove_file(&log).expect("reset the gh log");

    // Opening one names its repository by node id, which knives cannot read —
    // however GraphQL's insignificant tokens (a comma, a `#` comment) are
    // placed after the mutation's name.
    for query in [
        "mutation { createPullRequest(input:{repositoryId:\"R\",headRefName:\"feat/gamma\",\
         baseRefName:\"main\",title:\"t\"}) { clientMutationId } }",
        "mutation { createPullRequest,(input:{repositoryId:\"R\"}) { clientMutationId } }",
        "mutation { createPullRequest#c\n(input:{repositoryId:\"R\"}) { clientMutationId } }",
    ] {
        let create = run(query);
        assert_eq!(create.status.code(), Some(2), "{query}: {create:?}");
        assert!(
            String::from_utf8_lossy(&create.stderr)
                .contains("a GraphQL createPullRequest names its repository by node id"),
            "{query}: {create:?}"
        );
        assert!(!log.exists(), "{query}: gh ran despite the refusal");
    }
}

#[test]
fn a_registry_that_does_not_load_skips_the_gate_and_says_so() {
    // The gate guards policy, so its absence is said once on stderr; the
    // command itself passes through as it always did.
    let config_home = tempfile::tempdir().expect("config home");
    fs::write(config_home.path().join("repos.toml"), "[repos.broken\n").expect("write registry");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");

    let output = knives_cmd(helper_dir.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--head",
            "feat/gamma",
        ])
        .current_dir(helper_dir.path())
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");

    assert!(output.status.success(), "{output:?}");
    assert!(
        log.exists(),
        "gh must run when there is nothing to gate against"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("knives: placement gate skipped: "),
        "{output:?}"
    );
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

#[test]
fn stacked_git_wrappers_reach_terminal_git_once() {
    // Given: a bookmarked repo, a foreign wrapper left by an older gh shim, and terminal git.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["edit", "feat/alpha"]);
    let scripts = tempfile::tempdir().expect("script directory");
    let foreign_wrapper = scripts.path().join("foreign-wrapper");
    let terminal = scripts.path().join("terminal");
    fs::create_dir(&foreign_wrapper).expect("foreign wrapper directory");
    fs::create_dir(&terminal).expect("terminal directory");
    let terminal_log = scripts.path().join("terminal-git.log");
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
    let terminal_git = terminal.join("git");
    fs::write(
        &terminal_git,
        "#!/bin/sh\nif [ \"$1\" = remote ]; then\n\
         printf 'terminal git\n' >> \"$TERMINAL_GIT_LOG\"\nfi\n",
    )
    .expect("write terminal git");
    fs::set_permissions(&terminal_git, fs::Permissions::from_mode(0o755))
        .expect("chmod terminal git");
    let path = format!(
        "{}:{}",
        terminal.display(),
        std::env::var("PATH").expect("PATH")
    );

    // When: knives prepends its wrapper before invoking a real gh that passes through to git.
    let output = std::process::Command::new("timeout")
        .args(["10", env!("CARGO_BIN_EXE_knives"), "gh", "--", "pr", "view"])
        .current_dir(&lab.work)
        .env("GIT_CONFIG_GLOBAL", scripts.path().join("gitconfig"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("KNIVES_CONFIG_HOME", scripts.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env_remove("GH_TOKEN")
        .env("KNIVES_GH_SHIM_DEPTH", "1")
        .env("KNIVES_REAL_GH", &real_gh)
        .env("PATH", path)
        .env("FOREIGN_WRAPPER_DIR", &foreign_wrapper)
        .env("TERMINAL_GIT_LOG", &terminal_log)
        .output()
        .expect("run timeout-bounded knives gh");

    // Then: each wrapper skips itself and the terminal git receives the passthrough once.
    assert!(
        output.status.success(),
        "knives gh failed: {}",
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
