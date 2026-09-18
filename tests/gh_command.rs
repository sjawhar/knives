#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
// allow: SIZE_OK: 6023 lines - real-binary gh passthrough scenarios share one fixture and process harness.

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
    // Given: a remote that resolves to a different owner/repository spec;
    // the value's host is the marked remote's, as gh reads it.
    let lab = lab::Lab::new();
    let host = concat!("github", ".com");
    git_config(
        &lab.work,
        &[
            "remote.token-spec.url",
            &format!("https://{host}/decoy/other.git"),
        ],
    );
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

    // Then: knives emits the exact diagnostic before spawning gh (after the
    // gate's one line that no registry is at this scratch config home).
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("knives: no registry at "), "{stderr}");
    assert!(
        stderr.ends_with(
            "Error: No jj bookmark at current change (@)\n\nCreate one with:\n  jj bookmark \
             create <name>\n\nOr push and create in one step:\n  jj git push --named=<name>=@\n"
        ),
        "{stderr}"
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
            "routed-b:feat/gamma",
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
            "routed-b:feat/gamma",
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
            "routed-b:feat/gamma",
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
            "routed-b:feat/gamma",
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
            "head=routed-b:feat/gamma",
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
            "head=routed-b:feat/gamma",
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
            "head=routed-b:feat/gamma",
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
            "head=routed-b:feat/gamma",
            "-f",
            "base=main",
        ],
        &[
            "repos/routed-a/upstream/pulls",
            "-ftitle=x",
            "-fhead=routed-b:feat/gamma",
            "-fbase=main",
        ],
        &[
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls#x",
            "-f",
            "title=x",
            "-f",
            "head=routed-b:feat/gamma",
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
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
        ],
        &[
            "repositories/1318902388/pulls",
            "-ftitle=t",
            "-fhead=routed-b:feat/eps",
            "-fbase=main",
        ],
        &[
            "-X",
            "POST",
            absolute.as_str(),
            "-f",
            "head=routed-b:feat/eps",
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
    let passed = run(&[
        "-R",
        "routed-a/upstream",
        "create",
        "-H",
        "routed-b:feat/gamma",
    ]);
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
            "routed-b:feat/gamma",
            "-H",
            "routed-b:feat/none",
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
        stderr.contains("states 2 heads (routed-b:feat/gamma, routed-b:feat/none)"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "gh would open routed-b:feat/none while a reader expects routed-b:feat/gamma"
        ),
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
            "routed-b:feat/gamma",
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
    for head in [
        &["-H", "routed-b:feat/gamma"][..],
        &["-Hrouted-b:feat/gamma"][..],
    ] {
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
        assert_eq!(heads, ["routed-b:feat/gamma"], "{head:?}: {recorded}");
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
        &["--title", "t", "--body", "-Hrouted-b:feat/gamma"][..],
        &["--title", "t", "--body=-Hfeat/gamma"][..],
        &["--title", "t", "-b", "-Hrouted-b:feat/gamma"][..],
        &[
            "--title",
            "t",
            "--body",
            "b",
            "--",
            "--head",
            "routed-b:feat/gamma",
        ][..],
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
    let clustered_fork = run(&["-dHrouted-b:feat/eps", "--title", "t", "--body", "b"]);
    assert_eq!(clustered_fork.status.code(), Some(2), "{clustered_fork:?}");
    assert!(
        String::from_utf8_lossy(&clustered_fork.stderr)
            .contains("feat/eps has placement verdict FORK"),
        "{clustered_fork:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
    for arguments in [
        &["-dHrouted-b:feat/gamma", "--title", "t", "--body", "b"][..],
        &["-fHrouted-b:feat/gamma", "--title", "t"][..],
        &["-dH", "routed-b:feat/gamma", "--title", "t", "--body", "b"][..],
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
    let unknown = run(&[
        "--mystery",
        "x",
        "-H",
        "routed-b:feat/gamma",
        "--title",
        "t",
    ]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains(
            "knives does not know gh's flag --mystery for pr create: if gh accepts it, add it \
             to PR_CREATE_FLAGS in src/commands/gh_args.rs; otherwise remove it"
        ),
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
        &["-dHrouted-b:feat/forked", "--title", "t", "--body", "b"][..],
        &["-fHrouted-b:feat/forked", "--title", "t"][..],
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
    let passed = run(&["-dHrouted-b:feat/allowed", "--title", "t", "--body", "b"]);
    assert!(passed.status.success(), "{passed:?}");
    assert!(log.exists(), "gh must run for an UPSTREAM head");
}

#[test]
fn the_gate_reads_pr_create_as_gh_parses_it_whatever_the_flag_order_alias_or_spelling() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout, feat/gamma
    // UPSTREAM, feat/none unverdicted, and a routed token helper so a minted
    // token is observable. Every shape here is one gh 2.98.0 accepts as a
    // `pr create` toward the upstream.
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
            .args(["gh", "--"])
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
    let refused = |arguments: &[&str], text: &str| {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let up = "routed-a/upstream";
    let none = knives::placement::missing_member_refusal("feat/none");
    let eps = "feat/eps has placement verdict FORK";

    // gh's own alias of the verb.
    refused(&["pr", "new", "-R", up, "-H", "routed-b:feat/none"], &none);
    refused(&["pr", "new", "-R", up, "--title", "t", "--body", "b"], eps);
    // A head or any valued flag before the verb: cobra hands the child every flag.
    refused(
        &["pr", "-H", "routed-b:feat/none", "create", "-R", up],
        &none,
    );
    refused(
        &["pr", "--head=routed-b:feat/none", "create", "-R", up],
        &none,
    );
    refused(
        &[
            "pr",
            "--title",
            "t",
            "create",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &none,
    );
    refused(
        &["pr", "--title", "t", "create", "-R", up, "--body", "b"],
        eps,
    );
    // A second -R: refused naming both, not read last-wins; a decoy first
    // one mints nothing for the decoy. A `--body` whose value is `-R` is a
    // body, and the one `--repo` is the upstream.
    refused(
        &[
            "pr",
            "create",
            "-R",
            "zz/yy",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        "states 2 repositories (-R \"zz/yy\", \"routed-a/upstream\")",
    );
    refused(
        &[
            "pr",
            "create",
            "--body",
            "-R",
            "--repo",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &none,
    );
}

#[test]
fn the_gate_refuses_url_repositories_empty_heads_and_unknown_flags_whatever_the_order() {
    // Given: the same fork checkout, `@` on FORK feat/eps.
    // Given: `@` on feat/eps (FORK) inside a fork checkout, feat/gamma
    // UPSTREAM, feat/none unverdicted, and a routed token helper so a minted
    // token is observable. Every shape here is one gh 2.98.0 accepts as a
    // `pr create` toward the upstream.
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
            .args(["gh", "--"])
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
    let refused = |arguments: &[&str], text: &str| {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let up = "routed-a/upstream";

    // URL forms of the upstream are outside the grammar knives compares:
    // refused with the canonical spelling, whatever they would name to gh.
    let host = concat!("github", ".com");
    let slash = format!("https://{host}/{up}/");
    let dotgit = format!("https://{host}/{up}.git");
    let canonical = "knives compares repositories only as OWNER/REPO or HOST/OWNER/REPO";
    refused(
        &["pr", "create", "-R", &slash, "-H", "routed-b:feat/none"],
        canonical,
    );
    refused(
        &["pr", "create", "-R", &dotgit, "-H", "routed-b:feat/none"],
        canonical,
    );
    // An empty head: gh would open the current branch, which that spelling
    // gives knives no way to certify.
    refused(
        &["pr", "create", "-R", up, "--head=", "--title", "t"],
        "states an empty head (`--head=`)",
    );
    // A flag gh does not define for `pr create`: refused with a remedy an
    // operator can follow.
    refused(
        &[
            "pr",
            "create",
            "-R",
            up,
            "--head=routed-b:feat/gamma",
            "--mystery",
            "x",
        ],
        "knives does not know gh's flag --mystery for pr create: if gh accepts it, add it to \
         PR_CREATE_FLAGS in src/commands/gh_args.rs; otherwise remove it",
    );
}

#[test]
fn an_upstream_head_in_any_pflag_spelling_passes_with_one_head_and_the_upstream_token() {
    // Given: the same fork checkout, FORK bookmark on `@`, feat/gamma UPSTREAM.
    // Given: `@` on feat/eps (FORK) inside a fork checkout, feat/gamma
    // UPSTREAM, feat/none unverdicted, and a routed token helper so a minted
    // token is observable. Every shape here is one gh 2.98.0 accepts as a
    // `pr create` toward the upstream.
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
            .args(["gh", "--"])
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
    let up = "routed-a/upstream";

    // Then: an UPSTREAM head in a pflag-legal spelling passes with exactly
    // one head in gh's argv and the routed token for the upstream.
    for arguments in [
        &[
            "pr",
            "create",
            "-R",
            up,
            "-d=true",
            "-H",
            "routed-b:feat/gamma",
            "--title",
            "t",
            "--body",
            "b",
        ][..],
        &[
            "pr",
            "-H",
            "routed-b:feat/gamma",
            "new",
            "-R",
            up,
            "--title",
            "t",
            "--body",
            "b",
        ][..],
    ] {
        let output = run(arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
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
            "{arguments:?}: {recorded}"
        );
        assert!(!argv.contains(&"feat/eps"), "{arguments:?}: {recorded}");
        assert!(
            recorded.contains("GH_TOKEN=tok-routed-a"),
            "{arguments:?}: {recorded}"
        );
        fs::remove_file(&log).expect("reset the gh log");
    }
}

#[test]
fn the_gate_reads_gh_api_as_gh_parses_it_and_a_flag_value_is_never_the_endpoint() {
    // Given: a fork checkout on FORK feat/eps, feat/none unverdicted, and the
    // routed token helper on PATH.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
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
    let none = knives::placement::missing_member_refusal("feat/none");
    let pulls = "repos/routed-a/upstream/pulls";
    let decoy = "repos/zz/yy/pulls";
    // Two methods are refused rather than read last-wins.
    let two = run(&[
        "-X",
        "GET",
        "-X",
        "POST",
        pulls,
        "-f",
        "head=routed-b:feat/none",
    ]);
    assert_eq!(two.status.code(), Some(2), "{two:?}");
    assert!(
        String::from_utf8_lossy(&two.stderr)
            .contains("states 2 methods (-X GET, -X POST); gh would use the last"),
        "{two:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
    for arguments in [
        // A flag's value shaped like an endpoint is not the endpoint.
        &[
            "-X",
            "POST",
            "-t",
            decoy,
            pulls,
            "-f",
            "head=routed-b:feat/none",
        ][..],
        &[
            "--jq",
            decoy,
            "-X",
            "POST",
            pulls,
            "-f",
            "head=routed-b:feat/none",
        ][..],
        &[
            "-H",
            "X-Decoy: repos/zz/yy/pulls",
            "-X",
            "POST",
            pulls,
            "-f",
            "head=routed-b:feat/none",
        ][..],
    ] {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&none),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    }
}

#[test]
fn a_gh_api_creation_with_input_or_an_unknown_flag_is_refused_and_a_get_lists() {
    let gate = GateLab::on_fork_bookmark();
    let run = |arguments: &[&str]| gate.run(&[&["api"][..], arguments].concat(), &[]);
    let log = &gate.log;
    let pulls = "repos/routed-a/upstream/pulls";
    let decoy = "repos/zz/yy/pulls";
    // `--input` moves the fields to the query string and takes the body from
    // a file knives does not read: refused whatever `-f head=` says, and the
    // flag's value is still never the endpoint.
    let input = run(&["--input", decoy, pulls, "-f", "head=routed-b:feat/none"]);
    assert_eq!(input.status.code(), Some(2), "{input:?}");
    assert!(
        String::from_utf8_lossy(&input.stderr)
            .contains("a POST to \"repos/routed-a/upstream/pulls\" takes its body from --input"),
        "{input:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
    // An attached method is read: `-XGET` on the pulls endpoint lists, and
    // the fields do not make it a creation; the upstream's token is minted.
    let listed = run(&["-XGET", pulls, "-f", "state=open"]);
    assert!(listed.status.success(), "{listed:?}");
    let recorded = fs::read_to_string(log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    fs::remove_file(log).expect("reset the gh log");
    // A flag gh does not define for `api` is refused with the table remedy.
    let unknown = run(&[
        "--nope",
        "x",
        "-X",
        "POST",
        pulls,
        "-f",
        "head=routed-b:feat/none",
    ]);
    assert_eq!(unknown.status.code(), Some(2), "{unknown:?}");
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains(
            "knives does not know gh's flag --nope for api: if gh accepts it, add it to API_FLAGS \
             in src/commands/gh_args.rs; otherwise remove it"
        ),
        "{unknown:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_missing_registry_says_so_once_and_an_unreadable_one_refuses() {
    // No registry file: nothing is registered, so nothing is gated — said once
    // on stderr. A registry file that cannot be read is an error: nothing
    // passes on a ledger the tool cannot read.
    let scratch = tempfile::tempdir().expect("scratch");
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |config_home: &Path| {
        knives_cmd(helper_dir.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                "routed-a/upstream",
                "--head",
                "routed-b:feat/none",
            ])
            .current_dir(scratch.path())
            .env("KNIVES_CONFIG_HOME", config_home)
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };

    let missing = tempfile::tempdir().expect("config home");
    let output = run(missing.path());
    assert!(output.status.success(), "{output:?}");
    assert!(log.exists(), "gh must run when nothing is registered");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(&format!(
            "knives: no registry at {}; nothing to gate",
            missing.path().join("repos.toml").display()
        )),
        "{stderr}"
    );
    fs::remove_file(&log).expect("reset the gh log");

    let broken = tempfile::tempdir().expect("config home");
    fs::write(broken.path().join("repos.toml"), "[repos.broken\n").expect("write registry");
    let output = run(broken.path());
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("is not a valid registry"), "{output:?}");
    // The TOML parse cause is part of the message, not a chained anyhow
    // source, so it names the problem once rather than twice.
    assert_eq!(stderr.matches("TOML parse error").count(), 1, "{stderr}");
    assert!(!log.exists(), "gh ran on a registry knives could not read");
}

#[test]
fn two_resolved_base_markers_rank_upstream_over_origin_like_gh_does() {
    // Given: a plain git clone whose "origin" remote — physically first in
    // git config, since it is the first one added — is a decoy, and whose
    // "upstream" remote is the registered upstream; both carry a
    // `gh-resolved = base` marker, the shape a hand-edited `gh repo
    // set-default` history can leave. gh's own `Remotes.Sort` ranks named
    // remotes upstream > github > origin > everything else and takes the
    // first with any resolved value at all, so it always picks upstream
    // here regardless of which config line comes first.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = scratch.path().join("clone");
    lab::git_repository(
        &clone,
        &[
            ("origin", &format!("https://{host}/decoy/other.git")),
            ("upstream", &format!("https://{host}/routed-a/upstream.git")),
        ],
    );
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
    git(&["checkout", "--quiet", "-b", "feat/eps"]);
    git_config(&clone, &["remote.origin.gh-resolved", "base"]);
    git_config(&clone, &["remote.upstream.gh-resolved", "base"]);
    let (dir, log) = fake_gh();

    // When: a pull request is opened toward whichever remote the marker
    // resolves to, with no explicit `-R` or `--head`.
    let output = knives_cmd(scratch.path())
        .args(["gh", "--", "pr", "create", "--title", "t", "--body", "b"])
        .current_dir(&clone)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");

    // Then: the upstream remote's marker wins by rank, not by which config
    // line comes first, and feat/eps's FORK verdict refuses it — a decoy
    // pick would resolve to an unregistered repository and pass through.
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
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
    assert!(stderr.contains("`--head routed-b:<branch>`"), "{stderr}");
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
            .args([
                "-f",
                "title=x",
                "-f",
                "head=routed-b:feat/gamma",
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
            "head=routed-b:feat/gamma",
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

#[test]
fn a_flag_before_the_command_word_still_refuses_a_fork_creation() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout. cobra's root
    // `Find` walks the whole line for the command word, so a flag before
    // "pr"/"api" is read by the eventual leaf grammar exactly like one
    // after it — the position pass 6's grammar did not reach.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let run = |arguments: &[&str]| {
        knives_cmd(config_home.path())
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh")
    };
    let up = "routed-a/upstream";
    let none = knives::placement::missing_member_refusal("feat/none");

    for arguments in [
        vec!["-R", up, "pr", "create", "-H", "routed-b:feat/none"],
        vec!["-H", "routed-b:feat/none", "pr", "create", "-R", up],
        vec![
            "-X",
            "POST",
            "api",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=routed-b:feat/none",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
        vec![
            "-f",
            "head=routed-b:feat/none",
            "api",
            "repos/routed-a/upstream/pulls",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
    ] {
        let output = run(&arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&none),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    }
    // `--repo` before the verb, with no stated head, relies on the fork
    // bookmark (feat/eps, FORK).
    let output = run(&["--repo", up, "pr", "create", "--title", "t", "--body", "b"]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_flag_before_the_command_word_still_passes_an_upstream_creation() {
    // Given: the same fork checkout, feat/gamma UPSTREAM, and a routed
    // token helper. The UPSTREAM variant runs, with the upstream's routed
    // token, whichever position the flags sit at; `--version`/`-h` before
    // the command name no command at all, so gh receives them unchanged.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/gamma", "UPSTREAM");
    let lab = fork_checkout_of_the_registered_upstream();
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
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
    let up = "routed-a/upstream";

    for arguments in [
        vec![
            "-R",
            up,
            "pr",
            "create",
            "-H",
            "routed-b:feat/gamma",
            "--title",
            "t",
            "--body",
            "b",
        ],
        vec![
            "-X",
            "POST",
            "api",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=routed-b:feat/gamma",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
    ] {
        let output = run(&arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains("GH_TOKEN=tok-routed-a"),
            "{arguments:?}: {recorded}"
        );
        fs::remove_file(&log).expect("reset the gh log");
    }

    // `gh --version pr create` and `gh -h pr create` behave as real gh:
    // `--version` is a genuine root switch, so it is `pr`'s own flag that
    // eats `create` as an unrecognised value, and gh errors "unknown flag:
    // --version" against `pr`'s usage; `-h` is never registered anywhere,
    // so it eats `pr` itself, leaving `create` as an unmatched top-level
    // command, and gh errors "unknown command "create" for "gh"". Neither
    // is a pull-request creation, so knives passes both through unchanged.
    for arguments in [
        vec!["--version", "pr", "create"],
        vec!["-h", "pr", "create"],
    ] {
        let output = run(&arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(log.exists(), "{arguments:?}: gh must have run");
        let recorded = fs::read_to_string(&log).expect("fake gh ran");
        assert!(
            recorded.contains(&arguments.join("\n")),
            "{arguments:?}: {recorded}"
        );
        fs::remove_file(&log).expect("reset the gh log");
    }
}

#[test]
fn a_repository_spelled_outside_the_grammar_is_refused_with_the_canonical_spelling() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout. Every `-R`
    // spelling below names the registered upstream to gh in some
    // normalisation knives no longer reproduces — scp form, `www.`, a query
    // string, a fragment, a percent-escape, a `.git` suffix, a trailing
    // slash — so each is refused with the canonical spelling rather than
    // compared (round-7 code F3 / deep H1, and every earlier round's
    // spelling).
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let host = concat!("github", ".com");
    let run = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let mut command = knives_cmd(config_home.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    };
    let refused = |arguments: &[&str], extra_env: &[(&str, &str)], text: &str| {
        let output = run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let canonical = "knives compares repositories only as OWNER/REPO or HOST/OWNER/REPO";

    for spec in [
        format!("git@{host}:routed-a/upstream"),
        format!("git@{host}:routed-a/upstream.git"),
        format!("https://www.{host}/routed-a/upstream"),
        format!("https://{host}/routed-a/upstream?tab=readme"),
        format!("https://{host}/routed-a/upstream#readme"),
        format!("https://{host}/routed-a/upstream.git?x=1"),
        format!("https://{host}/routed-a/%75pstream"),
        format!("https://{host}/%72outed-a/upstream"),
        format!("git@{host}:routed-a/%75pstream.git"),
        format!("ssh://git@{host}/%72outed-a/upstream"),
        format!("HTTPS://{host}/routed-a/upstream"),
        "routed-a/upstream/".to_owned(),
        "routed-a/upstream.git".to_owned(),
        "routed-a/upstream/extra/more".to_owned(),
        String::new(),
    ] {
        refused(
            &["pr", "create", "-R", &spec, "--title", "t", "--body", "b"],
            &[],
            &format!(
                "{canonical} (each segment of [A-Za-z0-9._-]); -R {spec:?} is not one: state it that way"
            ),
        );
    }
    refused(
        &["pr", "create", "--repo=", "--title", "t", "--body", "b"],
        &[],
        "-R \"\" is not one",
    );
}

#[test]
fn a_canonical_repository_is_compared_and_a_second_or_environment_one_is_read_the_same_way() {
    // Given: the same fork checkout, `@` on feat/eps (FORK).
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let host = concat!("github", ".com");
    let run = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let mut command = knives_cmd(config_home.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    };
    let refused = |arguments: &[&str], extra_env: &[(&str, &str)], text: &str| {
        let output = run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let canonical = "knives compares repositories only as OWNER/REPO or HOST/OWNER/REPO";

    // `GH_REPO` is read the same way, and a `-R` spelled canonically is the
    // upstream: the FORK verdict refuses it.
    refused(
        &["pr", "create", "--title", "t", "--body", "b"],
        &[("GH_REPO", &format!("https://{host}/routed-a/%75pstream"))],
        &format!("{canonical} (each segment of [A-Za-z0-9._-]); GH_REPO"),
    );
    refused(
        &[
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "--title",
            "t",
            "--body",
            "b",
        ],
        &[],
        "feat/eps has placement verdict FORK",
    );
    refused(
        &[
            "pr",
            "create",
            "-R",
            &format!("{host}/routed-a/upstream"),
            "--title",
            "t",
        ],
        &[],
        "feat/eps has placement verdict FORK",
    );
    // Two repositories are refused naming both, not read last-wins.
    refused(
        &[
            "pr",
            "create",
            "-R",
            "zz/yy",
            "-R",
            "routed-a/upstream",
            "--title",
            "t",
        ],
        &[],
        "states 2 repositories (-R \"zz/yy\", \"routed-a/upstream\")",
    );
}

/// A fork checkout with `@` on FORK `feat/eps`, a routed token helper, and a
/// runner for `knives gh -- <arguments>` in it (extra environment per call).
struct GateLab {
    config_home: tempfile::TempDir,
    lab: lab::Lab,
    gh: tempfile::TempDir,
    log: PathBuf,
    helper_dir: tempfile::TempDir,
    gitconfig: PathBuf,
}

impl GateLab {
    fn on_fork_bookmark() -> Self {
        let config_home = placement_gate_home();
        record_placement(config_home.path(), "feat/eps", "FORK");
        record_placement(config_home.path(), "feat/up", "UPSTREAM");
        let lab = fork_checkout_of_the_registered_upstream();
        lab.branch("feat/eps", "eps.txt", "eps\n");
        lab.jj_work(["edit", "feat/eps"]);
        let (gh, log) = fake_gh();
        let helper_dir = fake_app_token();
        let gitconfig = token_config(helper_dir.path(), "routed-a");
        Self {
            config_home,
            lab,
            gh,
            log,
            helper_dir,
            gitconfig,
        }
    }

    fn run(&self, arguments: &[&str], extra_env: &[(&str, &str)]) -> std::process::Output {
        let mut command = knives_cmd(self.helper_dir.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&self.lab.work)
            .env("KNIVES_CONFIG_HOME", self.config_home.path())
            .env("HOME", self.lab.temp_path())
            .env("KNIVES_REAL_GH", self.gh.path().join("gh"))
            .env("FAKE_GH_LOG", &self.log)
            .env("PATH", helper_path(self.helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    }

    /// Refused before gh runs, with `text` on stderr.
    fn refused(&self, arguments: &[&str], extra_env: &[(&str, &str)], text: &str) {
        let output = self.run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(
            !self.log.exists(),
            "{arguments:?}: gh ran despite the refusal"
        );
    }

    /// Passed to gh; what the fake gh recorded (argv, then `GH_TOKEN=`).
    fn passed(&self, arguments: &[&str], extra_env: &[(&str, &str)]) -> String {
        let output = self.run(arguments, extra_env);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        let recorded = fs::read_to_string(&self.log).expect("fake gh ran");
        fs::remove_file(&self.log).expect("reset the gh log");
        recorded
    }
}

/// `gh api -X POST <path>` with a feat/none head and a base: a REST creation
/// at gh's placeholders.
fn placeholder_creation(path: &str) -> Vec<&str> {
    let mut arguments = vec!["api", "-X", "POST", path];
    arguments.extend([
        "-f",
        "head=routed-b:feat/none",
        "-f",
        "base=main",
        "-f",
        "title=t",
    ]);
    arguments
}

/// `gh api -X POST <extra…> <path>` with a feat/none head and a base.
fn rest_creation<'a>(path: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut arguments = vec!["api", "-X", "POST"];
    arguments.extend(extra);
    arguments.push(path);
    arguments.extend(["-f", "head=routed-b:feat/none", "-f", "base=main"]);
    arguments
}

#[test]
fn help_disables_the_gate_only_when_gh_would_print_help() {
    // `--help=false` is a bool switch given false: gh runs the command, so
    // the gate reads on (round-7 code F1). `-h` is help whatever follows it;
    // a last `--help` that is bare or true is help; a value that is not a
    // bool is gh's own error, refused rather than read either way.
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    let none = knives::placement::missing_member_refusal("feat/none");
    let eps = "feat/eps has placement verdict FORK";
    let pulls = "repos/routed-a/upstream/pulls";
    gate.refused(
        &[
            "pr",
            "create",
            "--help=false",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
            "-t",
            "t",
            "-b",
            "b",
        ],
        &[],
        &none,
    );
    gate.refused(
        &[
            "pr",
            "create",
            "--help=0",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &[],
        &none,
    );
    gate.refused(
        &["pr", "create", "--help=false", "-t", "t", "-b", "b"],
        &[],
        eps,
    );
    gate.refused(
        &[
            "--help=false",
            "pr",
            "create",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &[],
        &none,
    );
    gate.refused(
        &[
            "pr",
            "create",
            "--help",
            "--help=false",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &[],
        &none,
    );
    gate.refused(
        &[
            "api",
            "--help=false",
            "-X",
            "POST",
            pulls,
            "-f",
            "head=routed-b:feat/none",
            "-f",
            "base=main",
        ],
        &[],
        &none,
    );
    gate.refused(
        &[
            "pr",
            "create",
            "--help=maybe",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ],
        &[],
        "--help=maybe gives a switch a value that is not a bool",
    );
    gate.refused(
        &["pr", "create", "-d=yes", "-R", up, "-H", "routed-b:feat/up"],
        &[],
        "-d=yes gives a switch a value that is not a bool",
    );
}

#[test]
fn help_in_effect_passes_the_creation_through_to_gh() {
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    // Help in effect: gh prints help and opens nothing, so the FORK bookmark
    // and the unverdicted head pass through to it.
    for arguments in [
        &[
            "pr",
            "create",
            "--help=false",
            "--help",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ][..],
        &[
            "pr",
            "create",
            "-h=false",
            "-R",
            up,
            "-H",
            "routed-b:feat/none",
        ][..],
        &["pr", "create", "-h", "--help=false", "-R", up, "-t", "t"][..],
        &["pr", "create", "--help=true", "-R", up, "-t", "t"][..],
    ] {
        gate.passed(arguments, &[]);
    }
}

#[test]
fn an_empty_or_second_repository_is_refused_not_read_as_gh_would_fall_back() {
    // `-R ""` and `--repo=` are "no -R" to gh, which falls back to `GH_REPO`
    // and then the checkout's base repository — the upstream here (round-7
    // code F2). knives reads an empty repository as one outside the grammar
    // and refuses it, wherever it runs; a second `-R` is refused naming both.
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    let empty = "-R \"\" is not one: state it that way";
    gate.refused(
        &[
            "pr",
            "create",
            "-R",
            "",
            "-H",
            "routed-b:feat/none",
            "-t",
            "t",
            "-b",
            "b",
        ],
        &[],
        empty,
    );
    gate.refused(
        &["pr", "create", "--repo=", "-t", "t", "-b", "b"],
        &[],
        empty,
    );
    gate.refused(
        &["pr", "create", "-R", "", "-H", "routed-b:feat/none"],
        &[("GH_REPO", up)],
        empty,
    );
    gate.refused(
        &["pr", "create", "--repo", "-t", "t"],
        &[],
        "-R \"-t\" is not one",
    );
    gate.refused(
        &[
            "pr",
            "create",
            "-R",
            "routed-b/origin",
            "-R",
            "",
            "-H",
            "routed-b:feat/none",
        ],
        &[],
        "states 2 repositories (-R \"routed-b/origin\", \"\")",
    );
    // Outside any checkout the same spelling is refused the same way: the
    // grammar does not depend on where the command runs.
    let scratch = tempfile::tempdir().expect("scratch");
    let output = knives_cmd(gate.helper_dir.path())
        .args([
            "gh",
            "--",
            "pr",
            "create",
            "-R",
            "",
            "-H",
            "routed-b:feat/none",
            "-t",
            "t",
        ])
        .current_dir(scratch.path())
        .env("KNIVES_CONFIG_HOME", gate.config_home.path())
        .env("KNIVES_REAL_GH", gate.gh.path().join("gh"))
        .env("FAKE_GH_LOG", &gate.log)
        .output()
        .expect("run knives gh");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(empty),
        "{output:?}"
    );
}

#[test]
fn a_percent_escaped_environment_repository_or_a_marker_with_a_host_is_refused() {
    // `GH_REPO` fills `repos/{owner}/{repo}/pulls` verbatim and GitHub
    // decodes the escape (round-7 code F4); a `gh-resolved` marker's host is
    // the remote's to gh whatever the value says (code F5). Both are read
    // only in canonical form.
    let gate = GateLab::on_fork_bookmark();
    let canonical = "knives compares repositories only as OWNER/REPO or HOST/OWNER/REPO";
    gate.refused(
        &placeholder_creation("repos/{owner}/{repo}/pulls"),
        &[("GH_REPO", "routed-a/upstre%61m")],
        &format!("{canonical} (each segment of [A-Za-z0-9._-]); GH_REPO \"routed-a/upstre%61m\""),
    );
    gate.refused(
        &placeholder_creation("repos/:owner/:repo/pulls"),
        &[("GH_REPO", "routed-%61/upstream")],
        "GH_REPO \"routed-%61/upstream\" is not one",
    );
    // A canonical `GH_REPO` fills the placeholders with the upstream, and
    // the unverdicted head is refused by name.
    gate.refused(
        &placeholder_creation("repos/{owner}/{repo}/pulls"),
        &[("GH_REPO", "routed-a/upstream")],
        &knives::placement::missing_member_refusal("feat/none"),
    );

    // Markers: `base` and `OWNER/REPO` are gh's own two spellings; a value
    // carrying a host, or anything else, is refused with the remedy.
    let marker = |value: &str| {
        git_config(&gate.lab.work, &["remote.upstream.gh-resolved", value]);
    };
    let create = ["pr", "create", "-t", "t", "-b", "b"];
    marker("other.example/routed-a/upstream");
    gate.refused(
        &create,
        &[],
        "remote.upstream.gh-resolved is read only as `base` or `OWNER/REPO` (the host is the \
         remote's, as gh reads it); \"other.example/routed-a/upstream\" is neither",
    );
    marker("https://other.example/routed-a/upstream");
    gate.refused(
        &create,
        &[],
        "\"https://other.example/routed-a/upstream\" is neither",
    );
    marker("routed-a/upstream");
    gate.refused(&create, &[], "feat/eps has placement verdict FORK");
    marker("base");
    gate.refused(&create, &[], "feat/eps has placement verdict FORK");
    // A marker naming another repository on the remote's host is that
    // repository: unregistered, so the creation passes.
    marker("decoy/other");
    let recorded = gate.passed(&create, &[]);
    assert!(recorded.contains("GH_TOKEN=tok-decoy"), "{recorded}");
}

#[test]
fn a_head_names_the_forks_branch_as_owner_colon_branch_and_nothing_else_is_read() {
    // Measured against gh 2.98.0 (MEASUREMENT.md, pass 9): a bare `--head
    // BRANCH` is `headRefName: "BRANCH"` on the base repository — the
    // upstream's own branch, not this fork's; only `OWNER:BRANCH` names the
    // fork's. So toward a registered upstream a bare head is refused with
    // the fork's spelling, `OWNER:BRANCH` is read only for the fork's own
    // origin owner (round-8 code F4 / deep M1), any other owner is refused
    // naming the fork's (round-7 deep M1), and a REST `-f head=` follows the
    // same rule.
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    let pulls = "repos/routed-a/upstream/pulls";
    let bare = "states the head --head \"feat/up\" without an owner, which gh opens from the \
                upstream's own branch, not this fork's: state it as routed-b:feat/up";
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "--head", "feat/up"],
        &[],
        bare,
    );
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "--head=feat/up"],
        &[],
        bare,
    );
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "-Hfeat/up"],
        &[],
        bare,
    );
    gate.refused(
        &["api", pulls, "-f", "head=feat/up", "-f", "base=main"],
        &[],
        "states the head -f head \"feat/up\" without an owner, which gh opens from the upstream's \
         own branch, not this fork's: state it as routed-b:feat/up",
    );
    let decoy = "states the head --head \"decoy:feat/up\", a branch of decoy's repository, which \
                 this fork's ledger never ruled on: state it as routed-b:feat/up, the fork's own \
                 branch (a cross-repository head cannot be checked here)";
    gate.refused(
        &[
            "pr",
            "create",
            "-R",
            up,
            "-t",
            "t",
            "--head",
            "decoy:feat/up",
        ],
        &[],
        decoy,
    );
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "--head=decoy:feat/up"],
        &[],
        decoy,
    );
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "-H", "decoy:feat/up"],
        &[],
        decoy,
    );
    gate.refused(
        &["api", pulls, "-f", "head=decoy:feat/up", "-f", "base=main"],
        &[],
        "a branch of decoy's repository, which this fork's ledger never ruled on: state it as \
         routed-b:feat/up",
    );
}

#[test]
fn the_forks_own_owner_in_a_head_is_judged_by_the_branchs_verdict() {
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    let pulls = "repos/routed-a/upstream/pulls";
    // The fork's own owner, judged by the branch's verdict: FORK refused,
    // UPSTREAM passes with exactly the stated head and the upstream's token.
    gate.refused(
        &[
            "pr",
            "create",
            "-R",
            up,
            "-t",
            "t",
            "--head",
            "routed-b:feat/eps",
        ],
        &[],
        "feat/eps has placement verdict FORK",
    );
    gate.refused(
        &[
            "api",
            pulls,
            "-f",
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
        ],
        &[],
        "feat/eps has placement verdict FORK",
    );
    gate.refused(
        &[
            "pr",
            "create",
            "-R",
            up,
            "-t",
            "t",
            "--head",
            "routed-b:feat/none",
        ],
        &[],
        &knives::placement::missing_member_refusal("feat/none"),
    );
    for head in ["routed-b:feat/up", "Routed-B:feat/up"] {
        let recorded = gate.passed(
            &[
                "pr", "create", "-R", up, "-t", "t", "-b", "b", "--head", head,
            ],
            &[],
        );
        let argv: Vec<&str> = recorded
            .lines()
            .take_while(|line| !line.starts_with("GH_TOKEN="))
            .collect();
        assert_eq!(
            argv.iter().filter(|a| **a == "--head").count(),
            1,
            "{head}: {recorded}"
        );
        assert!(argv.contains(&head), "{head}: {recorded}");
        assert!(
            recorded.contains("GH_TOKEN=tok-routed-a"),
            "{head}: {recorded}"
        );
    }
    let recorded = gate.passed(
        &[
            "api",
            pulls,
            "-f",
            "head=routed-b:feat/up",
            "-f",
            "base=main",
        ],
        &[],
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    // A branch outside the grammar, with or without an owner; two heads.
    gate.refused(
        &["pr", "create", "-R", up, "--head", "routed-b:feat up"],
        &[],
        "knives compares heads only as OWNER:BRANCH",
    );
    gate.refused(
        &["api", pulls, "-f", "head=@{-1}", "-f", "base=main"],
        &[],
        "-f head \"@{-1}\" for registered is not one: state the branch",
    );
    gate.refused(
        &[
            "api",
            pulls,
            "-f",
            "head=routed-b:feat/up",
            "-f",
            "head=routed-b:feat/none",
        ],
        &[],
        "states 2 heads (routed-b:feat/up, routed-b:feat/none)",
    );
}

#[test]
fn with_no_head_stated_knives_states_the_forks_branch_in_a_jj_checkout() {
    // Measured (MEASUREMENT.md ctx a): the bare bookmark knives used to
    // append was read by gh as the upstream's own branch. Now the head is
    // stated as `<origin-owner>:<bookmark on @>`, the one spelling that
    // names the fork's branch, and the verdict lookup is by the bookmark.
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    gate.refused(
        &["pr", "create", "-R", up, "-t", "t", "-b", "b"],
        &[],
        "feat/eps has placement verdict FORK",
    );
    gate.lab.branch("feat/up", "up.txt", "up\n");
    gate.lab.jj_work(["edit", "feat/up"]);
    let recorded = gate.passed(&["pr", "create", "-R", up, "-t", "t", "-b", "b"], &[]);
    let argv: Vec<&str> = recorded
        .lines()
        .take_while(|line| !line.starts_with("GH_TOKEN="))
        .collect();
    assert_eq!(
        argv,
        [
            "pr",
            "create",
            "-R",
            up,
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/up"
        ],
        "{recorded}"
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    // Toward the fork's own origin — not gated — the bare bookmark still
    // stands in for git's detached HEAD, as gh reads a bare head as the
    // base's own branch, which here it is.
    let recorded = gate.passed(
        &[
            "pr",
            "create",
            "-R",
            "routed-b/origin",
            "-t",
            "t",
            "-b",
            "b",
        ],
        &[],
    );
    let argv: Vec<&str> = recorded
        .lines()
        .take_while(|line| !line.starts_with("GH_TOKEN="))
        .collect();
    assert_eq!(argv.last().copied(), Some("feat/up"), "{recorded}");
    assert!(argv.contains(&"--head"), "{recorded}");
}

#[test]
fn with_no_head_stated_in_a_plain_clone_knives_states_heads_branch_not_the_push_target() {
    // Measured (MEASUREMENT.md ctx d): with no `--head`, gh 2.98.0 resolves
    // the branch's push target — `branch.<b>.merge` under
    // `push.default=upstream`, here `feat/eps` (FORK) while `feat/up`
    // (UPSTREAM) is checked out — and sends `routed-b:feat/eps`. knives
    // states HEAD's branch explicitly, so gh opens the branch knives
    // certified and never resolves one it did not read (round-8 deep H1 /
    // code F3).
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    record_placement(config_home.path(), "feat/up", "UPSTREAM");
    let host = concat!("github", ".com");
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
    git_config(
        &clone,
        &[
            "remote.origin.url",
            &format!("https://{host}/routed-b/origin.git"),
        ],
    );
    git_config(
        &clone,
        &[
            "remote.upstream.url",
            &format!("https://{host}/routed-a/upstream.git"),
        ],
    );
    git(&["checkout", "--quiet", "-b", "feat/up"]);
    git(&["update-ref", "refs/remotes/origin/feat/eps", "HEAD"]);
    git_config(&clone, &["branch.feat/up.remote", "origin"]);
    git_config(&clone, &["branch.feat/up.merge", "refs/heads/feat/eps"]);
    git_config(&clone, &["push.default", "upstream"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                "routed-a/upstream",
                "-t",
                "t",
                "-b",
                "b",
            ])
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
    let output = run(&[]);
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    let argv: Vec<&str> = recorded
        .lines()
        .take_while(|line| !line.starts_with("GH_TOKEN="))
        .collect();
    assert_eq!(
        argv,
        [
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/up"
        ],
        "{recorded}"
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    fs::remove_file(&log).expect("reset the gh log");
    // And HEAD on the FORK branch is refused by that name, push target or not.
    git(&["checkout", "--quiet", "-b", "feat/eps"]);
    let output = run(&[]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_subdomain_spelling_of_the_upstreams_host_is_the_upstream() {
    // gh folds every subdomain of the default host to it when choosing the
    // token and the endpoint (round-8 code F1 / L1, deep L1): `foo.<host>/o/r`
    // is `o/r` on the default host to gh. The one comparison rule folds any
    // subdomain of the registered host, so each spelling is the upstream —
    // and the same fold decides that a token is minted only for the default
    // host.
    let gate = GateLab::on_fork_bookmark();
    let host = concat!("github", ".com");
    for spec in [
        format!("foo.{host}/routed-a/upstream"),
        format!("api.{host}/routed-a/upstream"),
        format!("www.www.{host}/routed-a/upstream"),
        format!("{host}./routed-a/upstream"),
    ] {
        gate.refused(
            &["pr", "create", "-R", &spec, "-t", "t", "-b", "b"],
            &[],
            "feat/eps has placement verdict FORK",
        );
        gate.refused(
            &placeholder_creation("repos/{owner}/{repo}/pulls"),
            &[("GH_REPO", &spec)],
            &knives::placement::missing_member_refusal("feat/none"),
        );
    }
    // Another host is another repository, and mints no token: gh would send
    // there without GH_TOKEN.
    let recorded = gate.passed(&["pr", "list", "-R", "ghe.example/routed-a/upstream"], &[]);
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    let recorded = gate.passed(
        &[
            "api",
            "--hostname",
            "ghe.example",
            "-XGET",
            "repos/routed-a/upstream/pulls",
        ],
        &[],
    );
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    // A subdomain spelling of the default host mints as the default host.
    let recorded = gate.passed(
        &["pr", "list", "-R", &format!("foo.{host}/routed-a/upstream")],
        &[],
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
}

/// A registry whose one entry's upstream lives on a GitHub Enterprise host.
fn enterprise_gate_home() -> tempfile::TempDir {
    let config_home = tempfile::tempdir().expect("config home");
    fs::write(
        config_home.path().join("repos.toml"),
        "[repos.registered]\nupstream = \"git@ghe.example:routed-a/upstream.git\"\norigin = \"git@ghe.example:routed-b/origin.git\"\n",
    )
    .expect("write registry");
    config_home
}

#[test]
fn gh_host_is_the_default_host_for_a_two_part_repository_and_a_relative_endpoint() {
    // gh takes the default host from GH_HOST for a 2-part -R/GH_REPO and for a
    // REST path with no --hostname (round-8 code F2): a GHE-registered upstream
    // is reached that way. GH_HOST is read verbatim; one outside the grammar
    // is refused rather than read.
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let mut command = knives_cmd(helper_dir.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    };
    let refused = |arguments: &[&str], extra_env: &[(&str, &str)], text: &str| {
        let output = run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let fork = "feat/eps has placement verdict FORK";
    let ghe = [("GH_HOST", "ghe.example")];
    refused(
        &[
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "-t",
            "t",
            "-b",
            "b",
        ],
        &ghe,
        fork,
    );
    refused(
        &["pr", "create", "-t", "t", "-b", "b"],
        &[("GH_HOST", "ghe.example"), ("GH_REPO", "routed-a/upstream")],
        fork,
    );
    refused(
        &[
            "api",
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
        ],
        &ghe,
        fork,
    );
    // A subdomain of the GHE host is that host too.
    refused(
        &["pr", "create", "-R", "routed-a/upstream", "-t", "t"],
        &[("GH_HOST", "api.ghe.example")],
        fork,
    );
}

#[test]
fn gh_host_yields_to_hostname_and_to_an_absent_override_and_is_refused_outside_the_grammar() {
    // gh takes the default host from GH_HOST for a 2-part -R/GH_REPO and for a
    // REST path with no --hostname (round-8 code F2): a GHE-registered upstream
    // is reached that way. GH_HOST is read verbatim; one outside the grammar
    // is refused rather than read.
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let mut command = knives_cmd(helper_dir.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    };
    let refused = |arguments: &[&str], extra_env: &[(&str, &str)], text: &str| {
        let output = run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let ghe = [("GH_HOST", "ghe.example")];
    // Without GH_HOST the same spellings are on the default host: not this
    // upstream, and minted for as the default host.
    let output = run(
        &[
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "-t",
            "t",
            "-b",
            "b",
        ],
        &[],
    );
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    fs::remove_file(&log).expect("reset the gh log");
    // A GHE-hosted call mints nothing: gh would send there without GH_TOKEN.
    let output = run(&["pr", "list", "-R", "routed-a/upstream"], &ghe);
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    fs::remove_file(&log).expect("reset the gh log");
    // --hostname beats GH_HOST for a relative path, as in gh.
    let output = run(
        &[
            "api",
            "--hostname",
            "other.example",
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=x",
        ],
        &ghe,
    );
    assert!(output.status.success(), "{output:?}");
    fs::remove_file(&log).expect("reset the gh log");
    // A GH_HOST outside the grammar is refused, not read.
    refused(
        &["pr", "create", "-R", "routed-a/upstream", "-t", "t"],
        &[("GH_HOST", "https://ghe.example")],
        "GH_HOST, \"https://ghe.example\", is not a host knives compares",
    );
}

#[test]
fn an_absolute_url_states_its_own_host_and_a_disagreeing_hostname_is_refused() {
    // gh sends an absolute URL verbatim and authenticates by the URL's host,
    // whatever --hostname says (round-8 deep H2): the URL's host is the host
    // compared, and a --hostname that does not fold to it is refused rather
    // than read either way — in both orders.
    let gate = GateLab::on_fork_bookmark();
    let host = concat!("github", ".com");
    let absolute = format!("https://api.{host}/repos/routed-a/upstream/pulls");
    let none = knives::placement::missing_member_refusal("feat/none");
    gate.refused(&rest_creation(&absolute, &[]), &[], &none);
    gate.refused(&rest_creation(&absolute, &["--hostname", host]), &[], &none);
    gate.refused(
        &rest_creation(&absolute, &["--hostname", &format!("foo.{host}")]),
        &[],
        &none,
    );
    let two = format!(
        "states two hosts (--hostname ghe.example, URL host {host}); gh sends to the URL's host \
         whatever --hostname says: state one"
    );
    gate.refused(
        &rest_creation(&absolute, &["--hostname", "ghe.example"]),
        &[],
        &two,
    );
    let mut hostname_last = vec![
        "api",
        "-X",
        "POST",
        absolute.as_str(),
        "--hostname",
        "ghe.example",
    ];
    hostname_last.extend(["-f", "head=feat/none", "-f", "base=main"]);
    gate.refused(&hostname_last, &[], &two);
    // GH_HOST does not move an absolute URL either.
    gate.refused(
        &rest_creation(&absolute, &[]),
        &[("GH_HOST", "ghe.example")],
        &none,
    );
}

#[test]
fn a_body_file_with_a_stated_head_field_is_refused_not_certified() {
    // Measured (pass 10, MEASUREMENT.md): with `--input FILE`, gh 2.98.0 puts
    // every -f/-F field on the query string and POSTs the file as the body;
    // GitHub reads `head` from the body. A `-f head=<fork-owner>:<UPSTREAM>`
    // beside `--input` certified nothing GitHub would use (round-9 H1, both
    // lanes): refused, in every spelling of --input, the placeholder form
    // included.
    let gate = GateLab::on_fork_bookmark();
    let body = gate.lab.temp_path().join("body.json");
    fs::write(
        &body,
        "{\"head\":\"routed-b:feat/eps\",\"base\":\"main\",\"title\":\"t\"}\n",
    )
    .expect("write body");
    let body = body.to_str().expect("utf-8 path");
    let text = "takes its body from --input, which knives does not read (gh puts -f/-F fields on \
                the query string, not in the body): state the head as -f head=<fork-owner>:<branch> \
                and the other fields as -f, without --input";
    let pulls = "repos/routed-a/upstream/pulls";
    gate.refused(
        &[
            "api",
            "-X",
            "POST",
            pulls,
            "-f",
            "head=routed-b:feat/up",
            "--input",
            body,
        ],
        &[],
        text,
    );
    gate.refused(
        &["api", pulls, "-f", "head=routed-b:feat/up", "--input", body],
        &[],
        text,
    );
    gate.refused(
        &[
            "api",
            pulls,
            "-f",
            "head=routed-b:feat/up",
            &format!("--input={body}"),
        ],
        &[],
        text,
    );
    gate.refused(&["api", "-X", "POST", pulls, "--input", "-"], &[], text);
    gate.refused(
        &[
            "api",
            "-X",
            "POST",
            "repos/{owner}/{repo}/pulls",
            "-f",
            "head=routed-b:feat/up",
            "--input",
            body,
        ],
        &[],
        text,
    );
    // Without --input the same fields are the body, and the UPSTREAM head passes.
    let recorded = gate.passed(
        &[
            "api",
            "-X",
            "POST",
            pulls,
            "-f",
            "head=routed-b:feat/up",
            "-f",
            "base=main",
        ],
        &[],
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    // A body file on an endpoint that opens nothing is not the gate's business.
    let recorded = gate.passed(
        &[
            "api",
            "-X",
            "POST",
            "repos/routed-a/upstream/issues",
            "--input",
            body,
        ],
        &[],
    );
    assert!(recorded.contains("--input"), "{recorded}");
}

#[test]
fn the_one_host_in_hosts_yml_is_gh_s_default_host_when_gh_host_is_unset() {
    // Measured (pass 10, MEASUREMENT.md): with GH_HOST unset gh takes the
    // default host from hosts.yml when it holds exactly one host (go-gh
    // `defaultHost`), under GH_CONFIG_DIR, else $XDG_CONFIG_HOME/gh, else
    // ~/.config/gh; two or more hosts → github.com; an invalid file is gh's
    // own error. knives reads the same file the same way, and refuses one it
    // cannot read rather than guess (round-9 code M1).
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let gh_config = tempfile::tempdir().expect("gh config dir");
    let hosts =
        |text: &str| fs::write(gh_config.path().join("hosts.yml"), text).expect("write hosts.yml");
    let run = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let mut command = knives_cmd(helper_dir.path());
        command
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", gh_config.path());
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().expect("run knives gh")
    };
    let refused = |arguments: &[&str], extra_env: &[(&str, &str)], text: &str| {
        let output = run(arguments, extra_env);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let passed = |arguments: &[&str], extra_env: &[(&str, &str)]| {
        let output = run(arguments, extra_env);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        fs::remove_file(&log).expect("reset the gh log");
    };
    let fork = "feat/eps has placement verdict FORK";
    let create = [
        "pr",
        "create",
        "-R",
        "routed-a/upstream",
        "-t",
        "t",
        "-b",
        "b",
    ];
    let post = [
        "api",
        "-X",
        "POST",
        "repos/routed-a/upstream/pulls",
        "-f",
        "head=routed-b:feat/eps",
        "-f",
        "base=main",
    ];

    // One GHE host: a two-part -R and a relative POST are on it — the
    // GHE-registered upstream, FORK refused.
    hosts("ghe.example:\n    user: m\n    oauth_token: x\n    git_protocol: https\n");
    refused(&create, &[], fork);
    refused(&post, &[], fork);
    // GH_HOST beats it; --hostname beats both.
    passed(&create, &[("GH_HOST", concat!("github", ".com"))]);
    passed(
        &[
            "api",
            "--hostname",
            "other.example",
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=x",
        ],
        &[],
    );
    // github.com only, or two hosts (with or without github.com), or no file:
    // github.com — not this upstream.
    hosts(concat!("github", ".com", ":\n    user: m\n"));
    passed(&create, &[]);
    hosts(concat!(
        "ghe.example:\n    user: m\n",
        "github",
        ".com",
        ":\n    user: m\n"
    ));
    passed(&create, &[]);
    hosts("ghe.example:\n    user: m\nother.example:\n    user: m\n");
    passed(&create, &[]);
    fs::remove_file(gh_config.path().join("hosts.yml")).expect("remove hosts.yml");
    passed(&create, &[]);
}

#[test]
fn hosts_yml_is_found_by_go_ghs_config_dir_precedence() {
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let gh_config = tempfile::tempdir().expect("gh config dir");
    let hosts =
        |text: &str| fs::write(gh_config.path().join("hosts.yml"), text).expect("write hosts.yml");
    let fork = "feat/eps has placement verdict FORK";
    let create = [
        "pr",
        "create",
        "-R",
        "routed-a/upstream",
        "-t",
        "t",
        "-b",
        "b",
    ];
    let refused = |text: &str, expected: &str| {
        hosts(text);
        let output = knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(create)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", gh_config.path())
            .output()
            .expect("run knives gh");
        assert_eq!(output.status.code(), Some(2), "{text:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{text:?}: {output:?}"
        );
        assert!(!log.exists(), "{text:?}: gh ran despite the refusal");
    };
    // A file knives cannot read as a hosts map — anything but `key:` lines
    // with the indented map below, as gh writes it — or a host outside the
    // grammar, is refused rather than guessed.
    refused("this: [is: not\n", "is not YAML knives reads");
    refused("- ghe.example\n", "is not YAML knives reads");
    refused("ghe.example: scalar\n", "is not YAML knives reads");
    refused(
        "ghe example:\n    user: m\n",
        "is not YAML knives reads (line 1: \"ghe example:\")",
    );
    fs::remove_file(gh_config.path().join("hosts.yml")).expect("remove hosts.yml");
    // The directory precedence is go-gh's: GH_CONFIG_DIR, then $XDG_CONFIG_HOME/gh.
    let xdg = tempfile::tempdir().expect("xdg");
    fs::create_dir_all(xdg.path().join("gh")).expect("xdg gh dir");
    fs::write(
        xdg.path().join("gh").join("hosts.yml"),
        "ghe.example:\n    user: m\n",
    )
    .expect("write");
    let mut command = knives_cmd(helper_dir.path());
    command
        .args(["gh", "--"])
        .args(create)
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .env_remove("GH_HOST")
        .env_remove("GH_CONFIG_DIR")
        .env("XDG_CONFIG_HOME", xdg.path());
    let output = command.output().expect("run knives gh");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(fork),
        "{output:?}"
    );
    // …and GH_CONFIG_DIR (github.com only) beats that XDG file.
    hosts(concat!("github", ".com", ":\n    user: m\n"));
    let output = command
        .env("GH_CONFIG_DIR", gh_config.path())
        .output()
        .expect("run knives gh");
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn a_registry_upstream_spelled_on_a_www_host_still_gates_the_canonical_spelling() {
    // gh's own `normalizeHostname` strips `www.`, so a registry that spells its
    // upstream `www.github.com/o/r` names what `-R o/r` names; the fold
    // applies to the registered side too (round-9 deep M1, code L2).
    let host = concat!("github", ".com");
    let config_home = tempfile::tempdir().expect("config home");
    fs::write(
        config_home.path().join("repos.toml"),
        format!(
            "[repos.registered]\nupstream = \"https://www.{host}/routed-a/upstream.git\"\norigin = \"git@WWW.{host}:routed-b/origin.git\"\n"
        ),
    )
    .expect("write registry");
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    for spec in ["routed-a/upstream", &format!("{host}/routed-a/upstream")] {
        let output = knives_cmd(config_home.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                spec,
                "-t",
                "t",
                "-b",
                "b",
                "--head",
                "routed-b:feat/eps",
            ])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .output()
            .expect("run knives gh");
        assert_eq!(output.status.code(), Some(2), "{spec}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{spec}: {output:?}"
        );
        assert!(!log.exists(), "{spec}: gh ran despite the refusal");
    }
    // A REST creation at the canonical spelling likewise, and the fork's own
    // owner read off the `WWW.` origin.
    let output = knives_cmd(config_home.path())
        .args([
            "gh",
            "--",
            "api",
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls",
            "-f",
            "head=routed-b:feat/eps",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("HOME", lab.temp_path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
}

#[test]
fn a_registry_origin_without_a_readable_owner_is_refused_and_a_userless_scp_one_is_read() {
    // Measured (pass 9 rig): gh sends `--head :feat/x` as `headRefName:
    // "feat/x"` — the upstream's own branch. A registry origin knives reads
    // no owner from must not become an empty owner a head can match or be
    // stated with (round-9 code M2 / deep L1); the user-less scp form
    // `host:owner/repo`, which the registry already accepts as a URL, reads
    // its owner like every other spelling.
    let host = concat!("github", ".com");
    let run_with = |origin: &str, arguments: &[&str]| {
        let config_home = tempfile::tempdir().expect("config home");
        fs::write(
            config_home.path().join("repos.toml"),
            format!("[repos.registered]\nupstream = \"git@{host}:routed-a/upstream.git\"\norigin = \"{origin}\"\n"),
        )
        .expect("write registry");
        record_placement(config_home.path(), "feat/up", "UPSTREAM");
        let lab = lab::Lab::new();
        lab.branch("feat/up", "up.txt", "up\n");
        lab.jj_work(["edit", "feat/up"]);
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
                "-t",
                "t",
                "-b",
                "b",
            ])
            .args(arguments)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh");
        let recorded = fs::read_to_string(&log).unwrap_or_default();
        (output, recorded)
    };
    // A forge URL naming no owner is outside the remote grammar: the
    // registry itself is refused at load, before any gate (round-13).
    for arguments in [&[][..], &["--head", "routed-b:feat/up"][..]] {
        let origin = format!("https://{host}/");
        let (output, recorded) = run_with(&origin, arguments);
        assert_eq!(output.status.code(), Some(3), "{arguments:?}: {output:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!(
                "is not a valid registry: [repos.registered] origin = \"{origin}\" is outside the remote grammar knives compares"
            )),
            "{arguments:?}: {stderr}"
        );
        assert!(recorded.is_empty(), "{arguments:?}: gh ran: {recorded}");
    }
    // A local origin names no owner: refused with the registry remedy,
    // whether a head is stated (bare, `:branch`, the real owner) or not.
    for origin in ["/srv/git/origin.git", "file:///srv/git/origin.git"] {
        for arguments in [
            &[][..],
            &["--head", ":feat/up"][..],
            &["--head", "feat/up"][..],
            &["--head", "routed-b:feat/up"][..],
        ] {
            let (output, recorded) = run_with(origin, arguments);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{origin} {arguments:?}: {output:?}"
            );
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                stderr.contains(&format!(
                    "the registry's origin for registered ({origin}) names no owner knives can read: state it as https://<host>/OWNER/REPO in"
                )),
                "{origin} {arguments:?}: {stderr}"
            );
            assert!(
                recorded.is_empty(),
                "{origin} {arguments:?}: gh ran: {recorded}"
            );
        }
    }
    // The user-less scp form reads its owner: `routed-b:feat/up` passes and
    // the stated head is the fork's own.
    let (output, recorded) = run_with(
        &format!("{host}:routed-b/origin.git"),
        &["--head", "routed-b:feat/up"],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(recorded.contains("routed-b:feat/up"), "{recorded}");
    let (output, recorded) = run_with(&format!("{host}:routed-b/origin.git"), &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(recorded.contains("--head\nrouted-b:feat/up"), "{recorded}");
    let (output, _) = run_with(
        &format!("{host}:routed-b/origin.git"),
        &["--head", ":feat/up"],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("a branch of 's repository"),
        "{output:?}"
    );
}

/// A fake `ssh` whose `-G <host>` answers `hostname <target>` for one alias
/// and echoes any other host, the way go-gh's own tests inject the command.
fn fake_ssh(alias: &str, target: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("fake ssh dir");
    let script = dir.path().join("ssh");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\n[ \"$1\" = -G ] || exit 255\ncase \"$2\" in\n  {alias}) printf 'user git\\nhostname {target}\\n' ;;\n  *) printf 'hostname %s\\n' \"$2\" ;;\nesac\n"
        ),
    )
    .expect("write fake ssh");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).expect("chmod fake ssh");
    dir
}

/// A plain git clone of the fork on `branch`, its `upstream` remote spelled
/// `upstream_url` and its `origin` the fork; `configure` runs `git config`
/// lines in it.
fn fork_clone_with_upstream(
    scratch: &Path,
    branch: &str,
    upstream_url: &str,
    configure: &[(&str, &str)],
) -> PathBuf {
    let host = concat!("github", ".com");
    let clone = scratch.join("clone");
    lab::git_repository(&clone, &[]);
    fs::write(clone.join("README.md"), "seed\n").expect("write seed");
    lab::git_commit_all(&clone, "seed");
    git_config(
        &clone,
        &[
            "remote.origin.url",
            &format!("https://{host}/routed-b/origin.git"),
        ],
    );
    git_config(&clone, &["remote.upstream.url", upstream_url]);
    for (key, value) in configure {
        git_config(&clone, &[*key, *value]);
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["checkout", "--quiet", "-b", branch])
        .status()
        .expect("git checkout");
    assert!(status.success());
    clone
}

#[test]
fn an_insteadof_rewritten_upstream_remote_is_the_upstream_to_gh_and_to_knives() {
    // gh lists remotes with `git remote -v`, which applies `url.<base>.insteadOf`;
    // reading the raw `remote.<n>.url` called an aliased upstream another
    // repository, and a FORK branch reached the upstream with no -R
    // (round-10 M2, both lanes). knives now reads the effective URL.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    record_placement(config_home.path(), "feat/up", "UPSTREAM");
    let host = concat!("github", ".com");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = fork_clone_with_upstream(
        scratch.path(),
        "feat/eps",
        "gh:routed-a/upstream.git",
        &[(&format!("url.https://{host}/.insteadOf"), "gh:")],
    );
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&clone)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", scratch.path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };
    let fork = "feat/eps has placement verdict FORK";
    for arguments in [
        &[
            "pr",
            "create",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/eps",
        ][..],
        &["pr", "create", "-t", "t", "-b", "b"][..],
        &[
            "api",
            "-X",
            "POST",
            "repos/{owner}/{repo}/pulls",
            "-f",
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
        ][..],
    ] {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(fork),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    }
    // The GraphQL creation is refused inside the recognised fork too.
    let mutation = "mutation { createPullRequest(input:{repositoryId:\"x\",headRefName:\"feat/eps\"}) { pullRequest { id } } }";
    let output = run(&["api", "graphql", "-f", &format!("query={mutation}")]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("targets registered's upstream"),
        "{output:?}"
    );
    // The UPSTREAM branch passes with the head stated and the upstream's token.
    let status = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["checkout", "--quiet", "-b", "feat/up"])
        .status()
        .expect("git");
    assert!(status.success());
    let output = run(&["pr", "create", "-t", "t", "-b", "b"]);
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("--head\nrouted-b:feat/up"), "{recorded}");
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
}

#[test]
fn an_ssh_alias_in_the_upstream_remote_is_resolved_the_way_gh_resolves_it() {
    // gh translates an ssh remote's host through `ssh -G <host>` (go-gh's
    // ssh translator); an ssh-config alias for github.com names the upstream.
    // ssh reads the passwd home's config, not $HOME, so the alias is answered
    // by a fake `ssh` on PATH, as go-gh's own tests inject the command.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = fork_clone_with_upstream(
        scratch.path(),
        "feat/eps",
        "git@ghalias:routed-a/upstream.git",
        &[],
    );
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let ssh = fake_ssh("ghalias", host);
    let run = |arguments: &[&str], with_ssh: bool| {
        let path = if with_ssh {
            format!(
                "{}:{}",
                ssh.path().display(),
                helper_path(helper_dir.path())
            )
        } else {
            helper_path(helper_dir.path())
        };
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(arguments)
            .current_dir(&clone)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", scratch.path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", path)
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .output()
            .expect("run knives gh")
    };
    let create = [
        "pr",
        "create",
        "-t",
        "t",
        "-b",
        "b",
        "--head",
        "routed-b:feat/eps",
    ];
    let output = run(&create, true);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
    // A scp URL spelled with an ssh:// scheme and a port resolves the same.
    git_config(
        &clone,
        &[
            "remote.upstream.url",
            "ssh://git@ghalias:22/routed-a/upstream.git",
        ],
    );
    let output = run(&create, true);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    // Without ssh on PATH the host stays as written — gh's own fallback —
    // and the alias is another repository; the real ssh on this box answers
    // the alias with itself (no such Host in the passwd home's config).
    let output = run(&create, false);
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn a_hosts_block_inside_config_yml_is_gh_s_default_host_too() {
    // go-gh's `load` keeps config.yml's own `hosts` entry ahead of hosts.yml's
    // (pass-11 MEASUREMENT.md rows A–E, I, J): the pre-multi-account layout
    // gh still reads. Read first; hosts.yml only when config.yml has no
    // `hosts:` block (round-10 M1, both lanes).
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let gh_config = tempfile::tempdir().expect("gh config dir");
    let write = |name: &str, text: &str| {
        fs::write(gh_config.path().join(name), text).expect("write gh file");
    };
    let remove = |name: &str| {
        let _ = fs::remove_file(gh_config.path().join(name));
    };
    let create = [
        "pr",
        "create",
        "-R",
        "routed-a/upstream",
        "-t",
        "t",
        "-b",
        "b",
    ];
    let run = || {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(create)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", gh_config.path())
            .output()
            .expect("run knives gh")
    };
    let refused = |text: &str| {
        let output = run();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{output:?}"
        );
        assert!(!log.exists(), "gh ran despite the refusal");
    };
    let passed = || {
        let output = run();
        assert!(output.status.success(), "{output:?}");
        fs::remove_file(&log).expect("reset the gh log");
    };
    let fork = "feat/eps has placement verdict FORK";
    let ghe_block =
        "version: \"1\"\nhosts:\n    ghe.example:\n        user: m\n        oauth_token: x\n";
    // Rows A–E: config.yml's block wins over an absent, github.com-only,
    // empty, `{}`, or other-host hosts.yml.
    write("config.yml", ghe_block);
    remove("hosts.yml");
    refused(fork);
    write("hosts.yml", concat!("github", ".com", ":\n    user: m\n"));
    refused(fork);
    write("hosts.yml", "");
    refused(fork);
    write("hosts.yml", "{}\n");
    refused(fork);
    write("hosts.yml", "other.example:\n    user: m\n");
    refused(fork);
    // Row F (control): config.yml's block names github.com; hosts.yml's GHE
    // host is not read.
    write(
        "config.yml",
        concat!(
            "version: \"1\"\nhosts:\n    ",
            "github",
            ".com",
            ":\n        user: m\n"
        ),
    );
    write("hosts.yml", "ghe.example:\n    user: m\n");
    passed();
    // Row J: two hosts in the block → github.com. A versionless block reads
    // the same way (gh migrates it in place).
    write(
        "config.yml",
        "version: \"1\"\nhosts:\n    ghe.example:\n        user: m\n    other.example:\n        user: m\n",
    );
    remove("hosts.yml");
    passed();
    write("config.yml", "hosts:\n    ghe.example:\n        user: m\n");
    refused(fork);
}

#[test]
fn without_a_hosts_block_config_yml_yields_to_hosts_yml_and_an_unreadable_block_is_refused() {
    // go-gh's `load` keeps config.yml's own `hosts` entry ahead of hosts.yml's
    // (pass-11 MEASUREMENT.md rows A–E, I, J): the pre-multi-account layout
    // gh still reads. Read first; hosts.yml only when config.yml has no
    // `hosts:` block (round-10 M1, both lanes).
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let gh_config = tempfile::tempdir().expect("gh config dir");
    let write = |name: &str, text: &str| {
        fs::write(gh_config.path().join(name), text).expect("write gh file");
    };
    let remove = |name: &str| {
        let _ = fs::remove_file(gh_config.path().join(name));
    };
    let create = [
        "pr",
        "create",
        "-R",
        "routed-a/upstream",
        "-t",
        "t",
        "-b",
        "b",
    ];
    let run = || {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
            .args(create)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env("PATH", helper_path(helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &gitconfig)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", gh_config.path())
            .output()
            .expect("run knives gh")
    };
    let refused = |text: &str| {
        let output = run();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{output:?}"
        );
        assert!(!log.exists(), "gh ran despite the refusal");
    };
    let passed = || {
        let output = run();
        assert!(output.status.success(), "{output:?}");
        fs::remove_file(&log).expect("reset the gh log");
    };
    let fork = "feat/eps has placement verdict FORK";
    // No `hosts:` block: hosts.yml decides, as before; other config.yml keys
    // (`git_protocol`, `editor`, `aliases:` with its own indented map) are not hosts.
    write(
        "config.yml",
        "version: \"1\"\ngit_protocol: https\naliases:\n    co: pr checkout\n",
    );
    write("hosts.yml", "ghe.example:\n    user: m\n");
    refused(fork);
    remove("hosts.yml");
    passed();
    // Row I: a flow-mapped block gh reads is refused rather than parsed; so
    // is a scalar under `hosts:`.
    write(
        "config.yml",
        "version: \"1\"\nhosts: {ghe.example: {user: m}}\n",
    );
    refused("is not YAML knives reads");
    write("config.yml", "version: \"1\"\nhosts:\n    ghe.example: m\n");
    refused("is not YAML knives reads");
}

#[test]
fn a_bom_and_a_port_in_hosts_yml_are_read_as_gh_reads_them() {
    // Two shapes gh reads that pass 10 over-refused (round-10 L1): a UTF-8
    // byte-order mark before the first key, and a `HOST:PORT:` key, whose
    // host is the host.
    let config_home = enterprise_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = lab::Lab::new();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let gh_config = tempfile::tempdir().expect("gh config dir");
    for text in [
        "\u{feff}ghe.example:\n    user: m\n",
        "ghe.example:8443:\n    user: m\n",
    ] {
        fs::write(gh_config.path().join("hosts.yml"), text).expect("write hosts.yml");
        let output = knives_cmd(config_home.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                "routed-a/upstream",
                "-t",
                "t",
                "-b",
                "b",
            ])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", config_home.path())
            .env("HOME", lab.temp_path())
            .env("KNIVES_REAL_GH", dir.path().join("gh"))
            .env("FAKE_GH_LOG", &log)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", gh_config.path())
            .output()
            .expect("run knives gh");
        assert_eq!(output.status.code(), Some(2), "{text:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{text:?}: {output:?}"
        );
    }
}

#[test]
fn a_query_string_on_the_creation_endpoint_is_refused() {
    // GitHub may read a query parameter over the body knives certifies the
    // head from (unverified either way without a real token): fail closed
    // (round-10 deep L2). A `#fragment` is never sent and is still stripped.
    let gate = GateLab::on_fork_bookmark();
    let host = concat!("github", ".com");
    let text = "carries a query string, which GitHub may read over the body: state the endpoint as \
                repos/OWNER/REPO/pulls and the fields as -f";
    gate.refused(
        &rest_creation("repos/routed-a/upstream/pulls?head=routed-b:feat/eps", &[]),
        &[],
        text,
    );
    gate.refused(
        &rest_creation(
            &format!("https://api.{host}/repos/routed-a/upstream/pulls?x=1"),
            &[],
        ),
        &[],
        text,
    );
    gate.refused(
        &[
            "api",
            "repos/routed-a/upstream/pulls?x=1#f",
            "-f",
            "head=routed-b:feat/up",
        ],
        &[],
        text,
    );
    // The fragment alone is stripped; the UPSTREAM head passes.
    let recorded = gate.passed(
        &[
            "api",
            "-X",
            "POST",
            "repos/routed-a/upstream/pulls#f",
            "-f",
            "head=routed-b:feat/up",
            "-f",
            "base=main",
        ],
        &[],
    );
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    // A query on a GET is nothing to the gate.
    let recorded = gate.passed(&["api", "repos/routed-a/upstream/pulls?state=open"], &[]);
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
}

/// A GHE-registered fixture with `feat/eps` FORK on `@`, a scratch
/// `GH_CONFIG_DIR`, and a runner for `pr create -R routed-a/upstream` in it.
struct HostsLab {
    config_home: tempfile::TempDir,
    lab: lab::Lab,
    gh: tempfile::TempDir,
    log: PathBuf,
    helper_dir: tempfile::TempDir,
    gitconfig: PathBuf,
    gh_config: tempfile::TempDir,
}

impl HostsLab {
    fn new() -> Self {
        let config_home = enterprise_gate_home();
        record_placement(config_home.path(), "feat/eps", "FORK");
        let lab = lab::Lab::new();
        lab.branch("feat/eps", "eps.txt", "eps\n");
        lab.jj_work(["edit", "feat/eps"]);
        let (gh, log) = fake_gh();
        let helper_dir = fake_app_token();
        let gitconfig = token_config(helper_dir.path(), "routed-a");
        let gh_config = tempfile::tempdir().expect("gh config dir");
        Self {
            config_home,
            lab,
            gh,
            log,
            helper_dir,
            gitconfig,
            gh_config,
        }
    }

    fn write(&self, name: &str, text: &str) {
        fs::write(self.gh_config.path().join(name), text).expect("write gh file");
    }

    fn remove(&self, name: &str) {
        let _ = fs::remove_file(self.gh_config.path().join(name));
    }

    fn run(&self) -> std::process::Output {
        knives_cmd(self.helper_dir.path())
            .args([
                "gh",
                "--",
                "pr",
                "create",
                "-R",
                "routed-a/upstream",
                "-t",
                "t",
                "-b",
                "b",
            ])
            .current_dir(&self.lab.work)
            .env("KNIVES_CONFIG_HOME", self.config_home.path())
            .env("HOME", self.lab.temp_path())
            .env("KNIVES_REAL_GH", self.gh.path().join("gh"))
            .env("FAKE_GH_LOG", &self.log)
            .env("PATH", helper_path(self.helper_dir.path()))
            .env("GIT_CONFIG_GLOBAL", &self.gitconfig)
            .env_remove("GH_HOST")
            .env("GH_CONFIG_DIR", self.gh_config.path())
            .output()
            .expect("run knives gh")
    }

    /// The GHE upstream was the default host: the FORK verdict refused it.
    fn gated(&self, what: &str) {
        let output = self.run();
        assert_eq!(output.status.code(), Some(2), "{what}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{what}: {output:?}"
        );
        assert!(!self.log.exists(), "{what}: gh ran despite the refusal");
    }

    fn refused(&self, what: &str, text: &str) {
        let output = self.run();
        assert_eq!(output.status.code(), Some(2), "{what}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{what}: {output:?}"
        );
        assert!(!self.log.exists(), "{what}: gh ran despite the refusal");
    }

    /// github.com was the default host: another repository, passed.
    fn passed(&self, what: &str) {
        let output = self.run();
        assert!(output.status.success(), "{what}: {output:?}");
        fs::remove_file(&self.log).expect("reset the gh log");
    }
}

#[test]
fn a_quoted_or_padded_hosts_heading_and_host_key_are_read_as_gh_reads_them() {
    // Measured (pass 12, MEASUREMENT.md): gh reads `"hosts":`, `'hosts':`,
    // `hosts :`, a `hosts:  # comment` heading, and quoted or commented host
    // keys; the literal-only matcher fell through to hosts.yml (round-11
    // M2/F2, both lanes) or refused a key gh reads (L1).
    let hosts = HostsLab::new();
    hosts.write("hosts.yml", concat!("github", ".com", ":\n    user: m\n"));
    for heading in [
        "\"hosts\":",
        "'hosts':",
        "hosts :",
        "hosts:  # comment",
        "hosts: # c",
    ] {
        hosts.write(
            "config.yml",
            &format!("version: \"1\"\n{heading}\n    ghe.example:\n        user: m\n"),
        );
        hosts.gated(heading);
    }
    for key in [
        "\"ghe.example\":",
        "'ghe.example':",
        "ghe.example :",
        "ghe.example:  # main",
    ] {
        hosts.write(
            "config.yml",
            &format!("version: \"1\"\nhosts:\n    {key}\n        user: m\n"),
        );
        hosts.gated(key);
        hosts.remove("config.yml");
        hosts.write("hosts.yml", &format!("{key}\n    user: m\n"));
        hosts.gated(key);
        hosts.write("hosts.yml", concat!("github", ".com", ":\n    user: m\n"));
    }
    // A `hosts:` heading with a value on the line is still refused, not
    // read as `hosts.yml`'s.
    for heading in ["\"hosts\": {ghe.example: {user: m}}", "hosts : ~"] {
        hosts.write("config.yml", &format!("version: \"1\"\n{heading}\n"));
        hosts.refused(heading, "is not YAML knives reads");
    }
}

#[test]
fn an_indented_hosts_map_is_read_at_its_own_indent_and_never_as_zero_hosts() {
    // Measured (pass 12): gh reads a hosts.yml whose whole map is indented,
    // and a config.yml block at any indent; knives skipped the deeper lines
    // and read zero hosts — github.com — certifying a GHE-upstream creation
    // for the wrong host (round-11 M3/F3, both lanes). A block's indent is
    // its first content line's; a mixed indent is refused (gh errors too),
    // and content with no readable key is never zero hosts.
    let hosts = HostsLab::new();
    hosts.remove("config.yml");
    hosts.write("hosts.yml", " ghe.example:\n     user: m\n");
    hosts.gated("1-space hosts.yml");
    hosts.write("hosts.yml", "  ghe.example:\n      user: m\n");
    hosts.gated("2-space hosts.yml");
    hosts.write("hosts.yml", "\n# logged in\n\n    ghe.example:\n        user: m\n        users:\n            m:\n                oauth_token: x\n");
    hosts.gated("4-space hosts.yml after noise, with a deeper sub-map");
    hosts.write(
        "hosts.yml",
        "ghe.example:\n    user: m\n  other.example:\n    user: m\n",
    );
    hosts.refused(
        "mixed indent 0 then 2",
        "is not YAML knives reads (line 3: \"  other.example:\")",
    );
    hosts.write(
        "hosts.yml",
        "  ghe.example:\n    user: m\nother.example:\n    user: m\n",
    );
    hosts.refused(
        "mixed indent 2 then 0",
        "is not YAML knives reads (line 3: \"other.example:\")",
    );
    hosts.write("hosts.yml", "    user: m\n");
    hosts.refused(
        "content with no key",
        "is not YAML knives reads (line 1: \"    user: m\")",
    );
    hosts.write("hosts.yml", "ghe.example:\n  - x\n");
    hosts.refused(
        "a list under a read key is outside the grammar",
        "is not YAML knives reads (line 2: \"  - x\")",
    );
    // config.yml blocks at 5 and 2 spaces read; a block whose first line is
    // shallower than the heading's children cannot be (it ends the block).
    hosts.write("hosts.yml", concat!("github", ".com", ":\n    user: m\n"));
    hosts.write(
        "config.yml",
        "version: \"1\"\nhosts:\n     ghe.example:\n         user: m\n",
    );
    hosts.gated("5-space config block");
    hosts.write(
        "config.yml",
        "version: \"1\"\nhosts:\n  ghe.example:\n    user: m\n",
    );
    hosts.gated("2-space config block");
    hosts.write("config.yml", "version: \"1\"\nhosts:\n    ghe.example:\n        user: m\n  other.example:\n        user: m\n");
    hosts.refused(
        "mixed config block",
        "is not YAML knives reads (line 5: \"  other.example:\")",
    );
    // Genuinely empty files and blocks are zero hosts: github.com.
    hosts.write("config.yml", "version: \"1\"\nhosts:\n");
    hosts.write("hosts.yml", "ghe.example:\n    user: m\n");
    hosts.passed("an empty hosts: block yields to hosts.yml? no: the block exists and is empty");
}

/// A plain git clone of the fork on `feat/eps` with `origin` the fork and
/// the `remote.upstream.*` config lines given (`git config --add`), run
/// through `knives gh -- <arguments>` with the routed token helper.
fn run_in_clone_with_upstream_config(
    config_home: &Path,
    config: &[(&str, &str)],
    arguments: &[&str],
) -> (std::process::Output, Option<String>) {
    let host = concat!("github", ".com");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = scratch.path().join("clone");
    lab::git_repository(&clone, &[]);
    fs::write(clone.join("README.md"), "seed\n").expect("write seed");
    lab::git_commit_all(&clone, "seed");
    git_config(
        &clone,
        &[
            "remote.origin.url",
            &format!("https://{host}/routed-b/origin.git"),
        ],
    );
    for (key, value) in config {
        let status = Command::new("git")
            .arg("-C")
            .arg(&clone)
            .args(["config", "--add", key, value])
            .status()
            .expect("git config");
        assert!(status.success(), "git config {key}");
    }
    let status = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["checkout", "--quiet", "-b", "feat/eps"])
        .status()
        .expect("git");
    assert!(status.success());
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let output = knives_cmd(helper_dir.path())
        .args(["gh", "--"])
        .args(arguments)
        .current_dir(&clone)
        .env("KNIVES_CONFIG_HOME", config_home)
        .env("HOME", scratch.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .env("PATH", helper_path(helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gitconfig)
        .output()
        .expect("run knives gh");
    (output, fs::read_to_string(&log).ok())
}

#[test]
fn a_push_only_or_degenerate_fetch_upstream_remote_is_the_upstream_gh_targets() {
    // Measured (pass 12, MEASUREMENT.md): gh takes a remote's repository
    // from its fetch URL when that names one, else from its (last) push URL
    // — a pushurl-only remote, a one-segment fetch URL, a path fetch URL all
    // resolve routed-a/upstream and send createPullRequest. Reading only the
    // `(fetch)` lines dropped or misread them (round-11 M1/F1, both lanes).
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let upstream = format!("https://{host}/routed-a/upstream.git");
    let first = format!("https://{host}/zz/first.git");
    let one_segment = format!("https://{host}/routed-a");
    let shapes: [(&str, Vec<(&str, &str)>); 5] = [
        (
            "pushurl only",
            vec![("remote.upstream.pushurl", upstream.as_str())],
        ),
        (
            "two pushurls, the registered one last",
            vec![
                ("remote.upstream.pushurl", first.as_str()),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
        ),
        (
            "one-segment fetch + push",
            vec![
                ("remote.upstream.url", one_segment.as_str()),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
        ),
        (
            "path fetch + push",
            vec![
                ("remote.upstream.url", "/srv/git/upstream.git"),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
        ),
        (
            "one-letter fetch + push",
            vec![
                ("remote.upstream.url", "x"),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
        ),
    ];
    for (what, config) in &shapes {
        for arguments in [
            &[
                "pr",
                "create",
                "-t",
                "t",
                "-b",
                "b",
                "--head",
                "routed-b:feat/eps",
            ][..],
            &[
                "api",
                "-X",
                "POST",
                "repos/{owner}/{repo}/pulls",
                "-f",
                "head=routed-b:feat/eps",
                "-f",
                "base=main",
            ][..],
        ] {
            let (output, recorded) =
                run_in_clone_with_upstream_config(config_home.path(), config, arguments);
            assert_eq!(
                output.status.code(),
                Some(2),
                "{what} {arguments:?}: {output:?}"
            );
            assert!(
                String::from_utf8_lossy(&output.stderr)
                    .contains("feat/eps has placement verdict FORK"),
                "{what} {arguments:?}: {output:?}"
            );
            assert!(
                recorded.is_none(),
                "{what} {arguments:?}: gh ran: {recorded:?}"
            );
        }
    }
}

#[test]
fn a_fetch_url_naming_a_repository_wins_over_the_push_url_as_in_gh() {
    // Measured: fetch other/valid + pushurl routed-a/upstream → gh targets
    // other/valid; not the upstream, so not gated, and the token is other's.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let valid = format!("https://{host}/other/valid.git");
    let upstream = format!("https://{host}/routed-a/upstream.git");
    let (output, recorded) = run_in_clone_with_upstream_config(
        config_home.path(),
        &[
            ("remote.upstream.url", valid.as_str()),
            ("remote.upstream.pushurl", upstream.as_str()),
        ],
        &[
            "pr",
            "create",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/eps",
        ],
    );
    assert!(output.status.success(), "{output:?}");
    let recorded = recorded.expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-other"), "{recorded}");
}

#[test]
fn any_spelling_of_a_hosts_heading_is_the_hosts_region_and_never_falls_through() {
    // Measured (round-12 reviews, both lanes): gh reads `hosts  : {…}`,
    // `"hosts" : {…}`, `'hosts' : {…}`, `hosts<TAB>: {…}` and `hosts:<TAB># c`
    // as the hosts key. A heading knives did not enumerate fell through to
    // hosts.yml and certified the wrong default host. Now any column-0 line
    // whose key token is `hosts` IS the hosts region: read when its block is
    // readable, refused otherwise — never hosts.yml.
    let hosts = HostsLab::new();
    // hosts.yml names the GHE host, so a fall-through would look gated;
    // discriminate by naming github.com in config.yml's region instead: a
    // fall-through gates (wrongly), the correct read passes.
    hosts.write("hosts.yml", "ghe.example:\n    user: m\n");
    for heading in [
        "hosts  :",
        "\"hosts\" :",
        "'hosts' :",
        "hosts\t:",
        "hosts:\t# c",
        "\"hosts\":\t# c",
    ] {
        hosts.write(
            "config.yml",
            &format!(
                "version: \"1\"\n{heading}\n    {}:\n        user: m\n",
                concat!("github", ".com")
            ),
        );
        hosts.passed(heading);
    }
    // The same spellings with a value on the line are the region too, and
    // refused — not hosts.yml's GHE host.
    for heading in [
        "hosts  : {ghe.example: {user: m}}",
        "\"hosts\" : {ghe.example: {user: m}}",
        "'hosts' : {ghe.example: {user: m}}",
        "hosts\t: {ghe.example: {user: m}}",
        "hosts: ~",
    ] {
        hosts.write("config.yml", &format!("version: \"1\"\n{heading}\n"));
        hosts.refused(heading, "is not YAML knives reads");
    }
    // A BOM before a first-line heading is the heading.
    hosts.write(
        "config.yml",
        &format!(
            "\u{feff}hosts:\n    {}:\n        user: m\n",
            concat!("github", ".com")
        ),
    );
    hosts.passed("BOM heading");
    // A column-0 line whose key is not `hosts` is not the region: `hostsx:`,
    // `hosts.old:`, a key ending in `hosts`.
    for other in ["hostsx:", "hosts.old:", "old_hosts:"] {
        hosts.write(
            "config.yml",
            &format!(
                "version: \"1\"\n{other}\n    {}:\n        user: m\n",
                concat!("github", ".com")
            ),
        );
        hosts.gated(other);
    }
    // A key with a character outside the grammar is the whole file refused,
    // whichever key it is (round-13: no reading around a line).
    hosts.write(
        "config.yml",
        &format!(
            "version: \"1\"\n\"hosts x\":\n    {}:\n        user: m\n",
            concat!("github", ".com")
        ),
    );
    hosts.refused(
        "\"hosts x\":",
        "is not YAML knives reads (line 2: \"\\\"hosts x\\\":\")",
    );
}

#[test]
fn only_ascii_space_and_tab_are_yaml_whitespace_to_the_config_reader() {
    // Measured (pass 13): a hosts.yml whose key is led by a no-break space
    // is content to YAML — gh reads zero hosts and acts on github.com with
    // no warning — while Rust's trim read it as a host (round-12 deep F2).
    // Such a key, or any key with a character outside the host charset, is
    // refused; knives never reads a host gh does not.
    let hosts = HostsLab::new();
    hosts.remove("config.yml");
    for text in [
        "\u{a0}ghe.example:\n\u{a0}\u{a0}\u{a0}\u{a0}user: m\n",
        "ghe.example\u{a0}:\n    user: m\n",
        "\u{2003}ghe.example:\n    user: m\n",
        "ghe.ex\u{200b}ample:\n    user: m\n",
        "ghe/example:\n    user: m\n",
    ] {
        hosts.write("hosts.yml", text);
        hosts.refused(text, "is not YAML knives reads");
    }
    // In config.yml a no-break-space-led heading is a key outside the
    // grammar — gh reads it as some other key and hosts.yml's github.com
    // would pass — and the whole file is refused (round-13): knives reads
    // nothing around a line it does not read.
    hosts.write("hosts.yml", concat!("github", ".com", ":\n    user: m\n"));
    hosts.write(
        "config.yml",
        "version: \"1\"\n\u{a0}hosts:\n    ghe.example:\n        user: m\n",
    );
    hosts.refused(
        "NBSP-led heading",
        "is not YAML knives reads (line 2: \"\\u{a0}hosts:\")",
    );
    // A tab before the colon and a tab-then-comment are YAML whitespace.
    hosts.write(
        "config.yml",
        "version: \"1\"\nhosts\t:\n    ghe.example\t:\t# main\n        user: m\n",
    );
    hosts.gated("tabs around the colons");
}

#[test]
fn a_fetch_url_gh_s_parser_rejects_yields_to_the_push_url() {
    // Measured (pass 13, MEASUREMENT.md): an invalid `%` escape in the path
    // or userinfo, whitespace, an encoded slash making three segments, or a
    // backslash in an scp value is no URL to gh, which falls to the push
    // URL; knives' textual reader called each a repository (round-12 F1/F3,
    // both lanes). knives reads each by the canonical remote grammar
    // (round-13): outside it, a URL is unreadable and yields to a readable
    // push URL, whatever gh's parser would make of it.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let upstream = format!("https://{host}/routed-a/upstream.git");
    let fetches = [
        format!("https://{host}/routed%zz/decoy.git"),
        format!("https://x%zz@{host}/routed-a/decoy.git"),
        format!("https://{host}/other%2Fdecoy/x.git"),
        format!("https://{host}/rou ted/decoy.git"),
        format!("{host}:a\\b/decoy"),
        format!("git@{host}:routed%zz/decoy.git"),
        format!("ssh://git@{host}/routed%zz/decoy.git"),
    ];
    for fetch in &fetches {
        let (output, recorded) = run_in_clone_with_upstream_config(
            config_home.path(),
            &[
                ("remote.upstream.url", fetch.as_str()),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
            &[
                "pr",
                "create",
                "-t",
                "t",
                "-b",
                "b",
                "--head",
                "routed-b:feat/eps",
            ],
        );
        assert_eq!(output.status.code(), Some(2), "{fetch}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{fetch}: {output:?}"
        );
        assert!(recorded.is_none(), "{fetch}: gh ran: {recorded:?}");
    }
    // A valid escape gh decodes to the upstream itself is still a `%`: with
    // no readable push URL the remote is unreadable and the target cannot be
    // certified — refused with the `-R` remedy, never read around (the
    // accepted over-refusal, round-13).
    let (output, recorded) = run_in_clone_with_upstream_config(
        config_home.path(),
        &[(
            "remote.upstream.url",
            &format!("https://{host}/routed-a/upstre%61m.git"),
        )],
        &[
            "pr",
            "create",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/eps",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!(
            "remote upstream's URL (https://{host}/routed-a/upstre%61m.git) is outside the grammar knives compares, so the creation's target cannot be certified: state the repository (-R OWNER/REPO)"
        )),
        "{output:?}"
    );
    assert!(recorded.is_none(), "gh ran: {recorded:?}");
    // A query on the fetch URL puts it outside the grammar too, though gh
    // reads the decoy repository from it: the readable push URL is the
    // remote, and the upstream gate applies.
    let (output, recorded) = run_in_clone_with_upstream_config(
        config_home.path(),
        &[
            (
                "remote.upstream.url",
                &format!("https://{host}/other/decoy.git?x=%zz"),
            ),
            ("remote.upstream.pushurl", upstream.as_str()),
        ],
        &[
            "pr",
            "create",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/eps",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(recorded.is_none(), "gh ran: {recorded:?}");
}

#[test]
fn a_remote_outside_the_canonical_grammar_is_never_read_around() {
    // Round-13 (code F1-F3, deep F1-F2): an empty port, an empty host, a
    // `%` escape in the authority, a query or fragment however spelled, an
    // escape past a decoded slash — each a spelling gh's parser reads one
    // way or another and knives' reader read differently. knives now reads a
    // remote by one grammar: outside it the URL is unreadable; a readable
    // push URL is then the remote (gh's own fallback), and a remote with no
    // readable URL leaves the target uncertifiable — refused with the `-R`
    // remedy, never guessed.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let host = concat!("github", ".com");
    let upstream = format!("https://{host}/routed-a/upstream.git");
    let outside = [
        format!("https://{host}:/routed-a/upstream.git"),
        "https://:8080/routed-a/upstream.git".to_owned(),
        "https://github%2Ecom/routed-a/upstream.git".to_owned(),
        format!("https://{host}/other/decoy.git#%zz"),
        format!("https://{host}/other/decoy.git?a=%20b"),
        format!("https://{host}/other/decoy.git#a-b"),
        format!("https://{host}/other/decoy.git?a=b"),
        format!("https://{host}/a%2Fb%FF/decoy.git"),
        format!("https://{host}/other/decoy.git/extra"),
        format!("git@{host}:other/decoy.git#f"),
    ];
    let create = [
        "pr",
        "create",
        "-t",
        "t",
        "-b",
        "b",
        "--head",
        "routed-b:feat/eps",
    ];
    for fetch in &outside {
        // Alone: unreadable, refused with the remedy.
        let (output, recorded) = run_in_clone_with_upstream_config(
            config_home.path(),
            &[("remote.upstream.url", fetch.as_str())],
            &create,
        );
        assert_eq!(output.status.code(), Some(2), "{fetch}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(&format!(
                "remote upstream's URL ({fetch}) is outside the grammar knives compares, so the creation's target cannot be certified: state the repository (-R OWNER/REPO)"
            )),
            "{fetch}: {output:?}"
        );
        assert!(recorded.is_none(), "{fetch}: gh ran: {recorded:?}");
        // With a readable push URL: that is the remote, and it is the
        // upstream — the FORK verdict refuses, whatever repository gh would
        // have read from the fetch URL.
        let (output, recorded) = run_in_clone_with_upstream_config(
            config_home.path(),
            &[
                ("remote.upstream.url", fetch.as_str()),
                ("remote.upstream.pushurl", upstream.as_str()),
            ],
            &create,
        );
        assert_eq!(output.status.code(), Some(2), "{fetch} + push: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
            "{fetch} + push: {output:?}"
        );
        assert!(recorded.is_none(), "{fetch} + push: gh ran: {recorded:?}");
    }
    // A stated repository does not consult the remotes: the unreadable one
    // is irrelevant and the gate is on the stated upstream.
    let (output, recorded) = run_in_clone_with_upstream_config(
        config_home.path(),
        &[("remote.upstream.url", &outside[0])],
        &[
            "pr",
            "create",
            "-R",
            "routed-a/upstream",
            "-t",
            "t",
            "-b",
            "b",
            "--head",
            "routed-b:feat/eps",
        ],
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("feat/eps has placement verdict FORK"),
        "{output:?}"
    );
    assert!(recorded.is_none(), "gh ran: {recorded:?}");
    // Control: a readable fetch URL naming another repository is the remote
    // whatever the push URL says, and that repository passes.
    let (output, recorded) = run_in_clone_with_upstream_config(
        config_home.path(),
        &[
            (
                "remote.upstream.url",
                &format!("https://{host}/other/decoy.git"),
            ),
            ("remote.upstream.pushurl", upstream.as_str()),
        ],
        &create,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        recorded
            .expect("fake gh ran")
            .contains("GH_TOKEN=tok-other")
    );
}

#[test]
fn a_config_file_outside_the_grammar_gh_writes_is_refused_whole_and_never_falls_through() {
    // Round-13 (code F4, deep F3-F4 and the Lows): a `hosts` heading YAML
    // decodes from `"\x68osts":`, a complex key, an anchor, a flow document,
    // a second document, a bare scalar line, a tab indent, an empty flow map
    // after a block — each read by knives' line reader as something gh reads
    // differently (or, for the last two, as a file gh rejects outright).
    // Either file with a line outside the grammar gh writes makes the
    // default host unknown: refused naming the file and the line, and never
    // read from the other file.
    let hosts = HostsLab::new();
    // hosts.yml names the GHE host: a fall-through would gate (exit 2 with
    // the FORK text), the correct read refuses with the file text.
    hosts.write("hosts.yml", "ghe.example:\n    user: m\n");
    let github = concat!("github", ".com");
    for (text, line) in [
        (
            format!("version: \"1\"\n\"\\x68osts\":\n    {github}:\n        user: m\n"),
            "line 2: \"\\\"\\\\x68osts\\\":\"",
        ),
        (
            format!("version: \"1\"\n? hosts\n: {{{github}: {{user: m}}}}\n"),
            "line 2: \"? hosts\"",
        ),
        (
            format!("{{version: \"1\", hosts: {{{github}: {{user: m}}}}}}\n"),
            "line 1: \"{version:",
        ),
        (
            format!("version: \"1\"\n&a hosts:\n    {github}:\n        user: m\n"),
            "line 2: \"&a hosts:\"",
        ),
        (
            format!("version: \"1\"\n---\nhosts:\n    {github}:\n        user: m\n"),
            "line 2: \"---\"",
        ),
        ("version: \"1\"\nhosts\n".to_owned(), "line 2: \"hosts\""),
        (
            format!("version: \"1\"\nhosts: !!map\n    {github}:\n        user: m\n"),
            "line 2: \"hosts: !!map\"",
        ),
        (
            format!("version: \"1\"\nhosts: *h\n    {github}:\n        user: m\n"),
            "line 2: \"hosts: *h\"",
        ),
        (
            format!(
                "version: \"1\"\nhosts:\n    {github}:\n        user: m\n    {github}:\n        user: n\n"
            ),
            "line 5:",
        ),
        (
            "version: \"1\"\nversion: \"2\"\n".to_owned(),
            "line 2: \"version: \\\"2\\\"\"",
        ),
        (
            "version: \"1\"\n- hosts\n".to_owned(),
            "line 2: \"- hosts\"",
        ),
        (
            "version: \"1\"\nhosts: >\n    text\n".to_owned(),
            "line 2: \"hosts: >\"",
        ),
    ] {
        hosts.write("config.yml", &text);
        hosts.refused(
            &text,
            &format!(
                "gh's {} is not YAML knives reads ({line}",
                hosts.gh_config.path().join("config.yml").display()
            ),
        );
    }
}

#[test]
fn a_hosts_file_outside_the_grammar_is_refused_and_the_files_gh_writes_are_read() {
    // The hosts.yml half of the round-13 sweep, and the control: the
    // shapes gh itself writes — `m: {}` for a token-less user, an unquoted
    // version, gh's comments — read as gh reads them.
    let hosts = HostsLab::new();
    hosts.remove("config.yml");
    // hosts.yml outside the grammar: refused, not zero hosts and not
    // github.com — including the shapes gh itself rejects.
    for (text, line) in [
        ("ghe.example:\n\tuser: m\n", "line 2: \"\\tuser: m\""),
        ("ghe.example:\n    user: m\n{}\n", "line 3: \"{}\""),
        (
            "ghe.example:\n    user: m\n---\nother.example:\n",
            "line 3: \"---\"",
        ),
        (
            "ghe.example: {user: m}\n",
            "line 1: \"ghe.example: {user: m}\"",
        ),
        (
            "ghe.example:\n    user: m\n    user: n\n",
            "line 3: \"    user: n\"",
        ),
        ("*ghe :\n    user: m\n", "line 1: \"*ghe :\""),
        ("\"ghe.example\\n\":\n    user: m\n", "line 1:"),
        ("ghe.example:\n    user: \"m\n", "line 2:"),
    ] {
        hosts.write("hosts.yml", text);
        hosts.refused(
            text,
            &format!(
                "gh's {} is not YAML knives reads ({line}",
                hosts.gh_config.path().join("hosts.yml").display()
            ),
        );
    }
    // The shapes gh writes read: a host with a token-less user (`m: {}`),
    // an unquoted version, gh's own comments, `config.yml` with no hosts key.
    hosts.write(
        "hosts.yml",
        "ghe.example:\n    users:\n        m: {}\n    git_protocol: https\n    user: m\n",
    );
    hosts.write(
        "config.yml",
        "# The current version of the config schema\nversion: 1\n# What protocol to use when performing git operations. Supported values: ssh, https\ngit_protocol: https\neditor:\naliases:\n    co: pr checkout\nhttp_unix_socket:\n",
    );
    hosts.gated("gh-written files");
    // `hosts: {}` in config.yml is the hosts region, empty: zero hosts,
    // github.com — hosts.yml's GHE host is not read.
    hosts.write("config.yml", "version: \"1\"\nhosts: {}\n");
    hosts.passed("empty flow map as the hosts region");
}

#[test]
fn a_bare_dash_or_empty_argument_is_not_the_command_word_or_the_verb() {
    // cobra skips both when finding the command and the verb; gh then
    // refuses the stray positional (round-7 deep L1). knives reads `pr
    // create` past them the same way, so the gate applies.
    let gate = GateLab::on_fork_bookmark();
    let up = "routed-a/upstream";
    let none = knives::placement::missing_member_refusal("feat/none");
    for arguments in [
        &["-", "pr", "create", "-R", up, "-H", "routed-b:feat/none"][..],
        &["", "pr", "create", "-R", up, "-H", "routed-b:feat/none"][..],
        &["pr", "-", "create", "-R", up, "-H", "routed-b:feat/none"][..],
        &["pr", "", "create", "-R", up, "-H", "routed-b:feat/none"][..],
    ] {
        gate.refused(arguments, &[], &none);
    }
}

#[test]
fn a_token_is_routed_only_for_a_canonical_owner() {
    // An owner read from a remote URL through a `//` or `git@host:/` path is
    // empty; one read from an endpoint through a percent-escape is not a
    // segment. Neither mints a token — for a decoy owner or for the
    // checkout's own (round-7 deep L2).
    let gate = GateLab::on_fork_bookmark();
    let host = concat!("github", ".com");
    let recorded = gate.passed(&["api", "-XGET", "repos/%72outed-a/upstream/pulls"], &[]);
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    let recorded = gate.passed(&["api", "-XGET", "repos//upstream/pulls"], &[]);
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    // A URL-form `-R` on a command the gate does not judge routes nothing
    // and says so once; gh runs on its own auth.
    let output = gate.run(
        &[
            "pr",
            "list",
            "-R",
            &format!("git@{host}:/routed-a/upstream"),
        ],
        &[],
    );
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("knives gh: no token routed: knives compares repositories only as"),
        "{output:?}"
    );
    let recorded = fs::read_to_string(&gate.log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
    fs::remove_file(&gate.log).expect("reset the gh log");
    // A checkout remote spelled `git@host:/owner/repo` has an empty owner
    // segment once normalised: nothing is minted for it.
    let plain = lab::Lab::new();
    git_config(
        &plain.work,
        &["remote.origin.url", &format!("git@{host}:/decoy/other.git")],
    );
    let output = knives_cmd(gate.helper_dir.path())
        .args(["gh", "--", "api", "rate_limit"])
        .current_dir(&plain.work)
        .env("KNIVES_REAL_GH", gate.gh.path().join("gh"))
        .env("FAKE_GH_LOG", &gate.log)
        .env("PATH", helper_path(gate.helper_dir.path()))
        .env("GIT_CONFIG_GLOBAL", &gate.gitconfig)
        .output()
        .expect("run knives gh");
    assert!(output.status.success(), "{output:?}");
    let recorded = fs::read_to_string(&gate.log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn a_graphql_creation_inside_a_plain_git_clone_of_the_fork_is_refused_too() {
    // The fork's checkout may be a plain git clone (an agent's /tmp
    // checkout, no jj); the mutation names its repository by node id there
    // as anywhere, and is refused inside the registered fork (round-7 code L1).
    let config_home = placement_gate_home();
    let host = concat!("github", ".com");
    let scratch = tempfile::tempdir().expect("scratch");
    let clone = scratch.path().join("clone");
    lab::git_repository(&clone, &[]);
    fs::write(clone.join("README.md"), "seed\n").expect("write seed");
    lab::git_commit_all(&clone, "seed");
    git_config(
        &clone,
        &[
            "remote.upstream.url",
            &format!("https://{host}/routed-a/upstream.git"),
        ],
    );
    let (dir, log) = fake_gh();
    let mutation = "mutation { createPullRequest(input:{repositoryId:\"x\",headRefName:\"feat/none\"}) \
                     { pullRequest { id } } }";
    let output = knives_cmd(scratch.path())
        .args([
            "gh",
            "--",
            "api",
            "graphql",
            "-f",
            &format!("query={mutation}"),
        ])
        .current_dir(&clone)
        .env("KNIVES_CONFIG_HOME", config_home.path())
        .env("KNIVES_REAL_GH", dir.path().join("gh"))
        .env("FAKE_GH_LOG", &log)
        .output()
        .expect("run knives gh");
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("a GraphQL createPullRequest names its repository by node id, so knives cannot tell whether it targets registered's upstream"),
        "{output:?}"
    );
    assert!(!log.exists(), "gh ran despite the refusal");
}

#[test]
fn a_rest_creation_states_its_host_with_hostname_and_nothing_else_is_compared() {
    // `--hostname` is the host a REST creation addresses; a foreign absolute
    // URL, an empty segment, and a numeric repository id are refused rather
    // than compared.
    let gate = GateLab::on_fork_bookmark();
    let host = concat!("github", ".com");
    let none = knives::placement::missing_member_refusal("feat/none");
    gate.refused(
        &rest_creation("repos/routed-a/upstream/pulls", &["--hostname", host]),
        &[],
        &none,
    );
    gate.refused(
        &rest_creation(
            &format!("https://api.{host}/repos/routed-a/upstream/pulls"),
            &[],
        ),
        &[],
        &none,
    );
    // Another host is another repository: not the registered upstream.
    let recorded = gate.passed(
        &rest_creation(
            "repos/routed-a/upstream/pulls",
            &["--hostname", "ghe.example"],
        ),
        &[],
    );
    assert!(recorded.contains("--hostname"), "{recorded}");
    for (path, text) in [
        (
            format!("https://API.{host}/repos/routed-a/upstream/pulls"),
            "addresses a host knives does not compare",
        ),
        (
            format!("http://api.{host}/repos/routed-a/upstream/pulls"),
            "addresses a host knives does not compare",
        ),
        (
            "repos//upstream/pulls".to_owned(),
            "has an empty path segment",
        ),
        (
            "repos/routed-a//pulls".to_owned(),
            "has an empty path segment",
        ),
        (
            "repos/routed-a/upstream.git/pulls".to_owned(),
            "names its repository outside the grammar knives compares",
        ),
        (
            "repositories/1318902388/pulls".to_owned(),
            "cannot be checked against the registry",
        ),
    ] {
        gate.refused(&rest_creation(&path, &[]), &[], text);
    }
}

#[test]
fn a_percent_encoded_rest_or_graphql_creation_is_refused_not_guessed() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout, and a routed
    // token helper. GitHub decodes the owner and repository dynamic
    // segments before routing (and its `graphql` handler answers to a
    // percent-escape too); pass 6 compared the raw, still-encoded argument.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
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
    let refused = |arguments: &[&str], text: &str| {
        let output = run(arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(text),
            "{arguments:?}: {output:?}"
        );
        assert!(!log.exists(), "{arguments:?}: gh ran despite the refusal");
    };
    let encoded = "is percent-encoded, which GitHub decodes and knives does not: state the \
                   endpoint unencoded, as repos/OWNER/REPO/pulls";

    refused(
        &[
            "-X",
            "POST",
            "repos/%72outed-a/upstream/pulls",
            "-f",
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
        encoded,
    );
    refused(
        &[
            "-X",
            "POST",
            "repos/routed-a/%75pstream/pulls",
            "-f",
            "head=routed-b:feat/eps",
            "-f",
            "base=main",
            "-f",
            "title=t",
        ],
        encoded,
    );
    let mutation = "mutation { createPullRequest(input:{repositoryId:\"x\",headRefName:\"feat/eps\"}) \
                     { pullRequest { id } } }";
    refused(&["gra%70hql", "-f", &format!("query={mutation}")], encoded);

    // A GET on the same percent-encoded path is not a creation; its owner is
    // outside the grammar, so no token is routed for it either.
    let listed = run(&["-XGET", "repos/%72outed-a/upstream/pulls"]);
    assert!(listed.status.success(), "{listed:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=unset"), "{recorded}");
}

#[test]
fn pr_create_help_or_h_never_reaches_the_gate() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout — a `pr create`
    // toward the registered upstream on that bookmark would otherwise be
    // refused. gh prints help and never makes the request when `--help`/
    // `-h` is anywhere in the line; nothing about the invocation can open a
    // pull request past it.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let up = "routed-a/upstream";
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
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
    // Control: without --help, this exact call is refused.
    let refused = run(&["pr", "create", "-R", up, "--title", "t", "--body", "b"]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(!log.exists(), "gh ran despite the refusal");

    for arguments in [
        vec![
            "pr", "create", "-R", up, "--title", "t", "--body", "b", "--help",
        ],
        vec![
            "pr", "create", "-R", up, "--title", "t", "--body", "b", "-h",
        ],
        vec!["pr", "create", "--help", "-R", up],
    ] {
        let output = run(&arguments);
        assert!(output.status.success(), "{arguments:?}: {output:?}");
        assert!(log.exists(), "{arguments:?}: gh must have run");
        fs::remove_file(&log).expect("reset the gh log");
    }
}

#[test]
fn pr_h_create_is_read_as_gh_reads_it_not_as_a_pr_create() {
    // Given: `@` on feat/eps (FORK) inside a fork checkout — `pr create`
    // toward the upstream on that bookmark is refused, the control below.
    // gh gives `--help` no `-h` shorthand anywhere, so cobra's own scan for
    // `pr`'s verb reads `-h` between `pr` and `create` as an unrecognised
    // flag that consumes `create`, leaving no verb: gh resolves to `pr`
    // alone and shows its help, never `create`'s, and never reaches the API.
    let config_home = placement_gate_home();
    record_placement(config_home.path(), "feat/eps", "FORK");
    let lab = fork_checkout_of_the_registered_upstream();
    lab.branch("feat/eps", "eps.txt", "eps\n");
    lab.jj_work(["edit", "feat/eps"]);
    let (dir, log) = fake_gh();
    let helper_dir = fake_app_token();
    let gitconfig = token_config(helper_dir.path(), "routed-a");
    let up = "routed-a/upstream";
    let run = |arguments: &[&str]| {
        knives_cmd(helper_dir.path())
            .args(["gh", "--"])
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
    // Control: the ordinary spelling is refused.
    let refused = run(&["pr", "create", "-R", up, "--title", "t", "--body", "b"]);
    assert_eq!(refused.status.code(), Some(2), "{refused:?}");
    assert!(!log.exists(), "gh ran despite the refusal");

    let output = run(&[
        "pr", "-h", "create", "-R", up, "--title", "t", "--body", "b",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(log.exists(), "gh must have run");
}
