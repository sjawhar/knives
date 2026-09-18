#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
// allow: SIZE_OK: 4160 lines - real-binary gh passthrough scenarios share one fixture and process harness.

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
        &["--input", decoy, pulls, "-f", "head=routed-b:feat/none"][..],
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
    // An attached method is read: `-XGET` on the pulls endpoint lists, and
    // the fields do not make it a creation; the upstream's token is minted.
    let listed = run(&["-XGET", pulls, "-f", "state=open"]);
    assert!(listed.status.success(), "{listed:?}");
    let recorded = fs::read_to_string(&log).expect("fake gh ran");
    assert!(recorded.contains("GH_TOKEN=tok-routed-a"), "{recorded}");
    fs::remove_file(&log).expect("reset the gh log");
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
        "GH_HOST \"https://ghe.example\" is not a host knives compares",
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
