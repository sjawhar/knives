#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;
#[path = "common/pulls.rs"]
mod pulls;

use knives::commands::audit;
use knives::forge::fake::FakeForge;
use knives::forge::{CheckRun, ChecksSummary, PullRequest};
use knives::ids::{BookmarkRef, BranchName, BranchTarget, RemoteName, RepoName};
use knives::jj::Repo;
use knives::store::Store;
use lab::{Lab, commit_at, lab_entry};
use std::collections::BTreeMap;
use std::process::Command;

/// The lab entry with the bare origin as origin, so live origin refs are the pushed ones.
fn origin_entry(lab: &Lab) -> knives::config::RepoEntry {
    knives::config::RepoEntry::new(
        lab.upstream.display().to_string(),
        lab.temp_origin().display().to_string(),
    )
}
/// Registry home for commands that reconcile the lab's live bare remotes, with
/// `extra` TOML lines appended to the `demo` entry.
fn mutation_test_home(lab: &Lab, extra: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"{}\"\n{extra}",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");
    home
}

/// Run the reconciliation command against the lab's registry entry.
fn knives_pushed(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--json", "pushed"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("run knives pushed")
}

#[test]
fn pushed_confirms_a_pushed_branch_and_flags_an_unpushed_one() {
    // Given: alpha reached the live origin and beta exists only in the checkout.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.push_branch("feat/alpha");
    let home = mutation_test_home(&lab, "");

    // When: every local bookmark is reconciled against its owning remote.
    let output = knives_pushed(&lab, &home, &["--repo", "demo"]);

    // Then: the missing beta ref is a finding while alpha is confirmed live.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let rows = report["rows"].as_array().expect("rows");
    let alpha = rows
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .expect("alpha row");
    let beta = rows
        .iter()
        .find(|row| row["branch"] == "feat/beta")
        .expect("beta row");
    assert_eq!(alpha["verdicts"][0]["verdict"], "in-sync");
    assert_eq!(beta["verdicts"][0]["verdict"], "not-on-remote");
    assert_eq!(beta["verdicts"][0]["remote"], "origin");
}

#[test]
fn pushed_catches_the_no_op_delete() {
    // Given: alpha was pushed before its local bookmark was removed.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "delete", "feat/alpha"]);
    let home = mutation_test_home(&lab, "");

    // When: the named, now-local-absent branch is reconciled.
    let output = knives_pushed(&lab, &home, &["feat/alpha", "--repo", "demo"]);

    // Then: the live ref is reported rather than silently accepting the delete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let row = report["rows"][0].as_object().expect("one row");
    assert!(row.get("local").is_none(), "was: {row:?}");
    assert_eq!(row["verdicts"][0]["verdict"], "remote-only");
    assert_eq!(row["verdicts"][0]["remote"], "origin");
}

#[test]
fn pushed_compares_a_tracked_pull_head() {
    // Given: alpha's tracked pull ref still names the older trunk commit.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let trunk = Repo::open(&lab.work)
        .expect("open lab")
        .resolve_commit("main@origin")
        .expect("resolve trunk");
    let status = Command::new("git")
        .args(["update-ref", "refs/pull/7/head", trunk.as_str()])
        .current_dir(lab.temp_origin())
        .status()
        .expect("write pull fixture");
    assert!(status.success(), "write pull fixture");
    let home = mutation_test_home(&lab, "");
    let tracked = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "track",
            "feat/alpha",
            "--pr",
            "7",
            "--repo",
            "demo",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("track pull");
    assert!(
        tracked.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&tracked.stderr)
    );

    // When: pushed compares the stated pull head from origin.
    let output = knives_pushed(&lab, &home, &["feat/alpha", "--repo", "demo"]);

    // Then: the independent pull-head mismatch is surfaced.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("pushed emits JSON");
    let verdicts = report["rows"][0]["verdicts"].as_array().expect("verdicts");
    assert!(
        verdicts
            .iter()
            .any(|verdict| verdict["verdict"] == "pull-head-differs" && verdict["number"] == 7),
        "was: {verdicts:?}"
    );
}

#[test]
fn pushed_partitions_release_names_to_the_release_remote() {
    // Given: the release and origin roles point at separate bare remotes.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("release/2026-08-04", "release.txt", "release\n");
    lab.push_branch("feat/alpha");
    let home = tempfile::tempdir().expect("create release remote home");
    let release = home.path().join("release.git");
    let status = Command::new("git")
        .args(["init", "--bare", release.to_str().expect("utf-8 path")])
        .status()
        .expect("create release remote");
    assert!(status.success(), "create release remote");
    lab.jj_work([
        "git",
        "remote",
        "add",
        "release",
        release.to_str().expect("utf-8 path"),
    ]);
    lab.jj_work([
        "git",
        "push",
        "--remote",
        "release",
        "--bookmark",
        "release/2026-08-04",
    ]);
    let config = mutation_test_home(&lab, &format!("release = \"{}\"\n", release.display()));

    // When: both roles contain only the ref class they own.
    let synced = knives_pushed(&lab, &config, &["--repo", "demo"]);

    // Then: cross-remote absence is topology, so both refs are in sync.
    assert!(
        synced.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&synced.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&synced.stdout).expect("pushed emits JSON");
    for branch in ["feat/alpha", "release/2026-08-04"] {
        let row = report["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["branch"] == branch)
            .expect("row");
        assert_eq!(row["verdicts"][0]["verdict"], "in-sync", "row: {row}");
    }

    // When: the release remote moves its release ref to a different live commit.
    let trunk = Repo::open(&lab.work)
        .expect("open lab")
        .resolve_commit("main@origin")
        .expect("resolve trunk");
    let status = Command::new("git")
        .args([
            "update-ref",
            "refs/heads/release/2026-08-04",
            trunk.as_str(),
        ])
        .current_dir(&release)
        .status()
        .expect("move release ref");
    assert!(status.success(), "move release ref");
    let drifted = knives_pushed(&lab, &config, &["release/2026-08-04", "--repo", "demo"]);

    // Then: only its owner role names the mismatch.
    assert_eq!(
        drifted.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "stderr: {}",
        String::from_utf8_lossy(&drifted.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&drifted.stdout).expect("pushed emits JSON");
    assert_eq!(report["rows"][0]["verdicts"][0]["verdict"], "differs");
    assert_eq!(report["rows"][0]["verdicts"][0]["remote"], "release");
}

/// Run estate reconciliation against the lab's registry entry.
fn knives_audit(lab: &Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--json", "audit"]);
    command.args(args);
    command
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("run knives audit")
}

fn gather_audit(lab: &Lab) -> audit::Report {
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let entry = lab_entry(lab);
    let fork = lab::lab_fork(lab, "demo", &entry);

    audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: None,
        cache_root: None,
        workers: 1,
    })
}

fn assert_only_unconfigured_remote(report: &audit::Report, branch: &str) {
    let mut findings = report
        .findings
        .iter()
        .filter(|finding| finding.kind.to_string() == "unconfigured-remote");
    let finding = findings.next().expect("one unconfigured remote finding");
    assert_eq!(finding.subject.to_string(), format!("{branch}@extra"));
    assert!(findings.next().is_none(), "unexpected findings: {report:?}");
    assert!(
        !report.findings.iter().any(|finding| {
            finding.kind.to_string() == "unconfigured-remote"
                && finding.subject.to_string().ends_with("@git")
        }),
        "jj's internal git remote must not be reported: {report:?}"
    );
}

/// Mutate the colocated Git config directly so jj's remote bookmark remains in its view.
fn git_remote_in_colocated_config(lab: &Lab, args: &[&str]) {
    let store = &lab.work;
    let output = Command::new("git")
        .arg("-C")
        .arg(store)
        .arg("remote")
        .args(args)
        .output()
        .expect("run git remote in jj store");
    assert!(
        output.status.success(),
        "git remote {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn current_operation(lab: &Lab) -> String {
    let output = Command::new("jj")
        .args(["op", "log", "--no-graph", "-T", "id ++ \"\\n\""])
        .current_dir(&lab.work)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("read current operation");
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("utf-8 operation id")
        .lines()
        .next()
        .expect("current operation")
        .to_owned()
}

fn fetch_remote_without_integrating(lab: &Lab, operation: &str, remote: &str) -> String {
    let output = Command::new("jj")
        .args([
            "--at-op",
            operation,
            "--no-integrate-operation",
            "git",
            "fetch",
            "--remote",
            remote,
        ])
        .current_dir(&lab.work)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("fetch remote at operation");
    assert!(
        output.status.success(),
        "fetch {remote} at {operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf-8 fetch stdout");
    let stderr = String::from_utf8(output.stderr).expect("utf-8 fetch stderr");
    let operation = if stdout.trim().is_empty() {
        stderr.as_str()
    } else {
        stdout.as_str()
    };
    operation
        .split_whitespace()
        .last()
        .unwrap_or_else(|| {
            panic!("unintegrated operation id: stdout={stdout:?}, stderr={stderr:?}")
        })
        .to_owned()
}

fn integrate_operation(lab: &Lab, operation: &str) {
    let output = Command::new("jj")
        .args(["op", "integrate", operation])
        .current_dir(&lab.work)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Knives Lab")
        .env("JJ_EMAIL", "knives-lab@example.test")
        .output()
        .expect("integrate operation");
    assert!(
        output.status.success(),
        "integrate {operation} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_remote_tracking_ref_whose_remote_is_gone_is_reported() {
    // Given: extra contributes a normal tracking ref while still configured.
    let lab = Lab::new();
    let extra = lab.temp_origin();
    lab.jj_work([
        "git",
        "remote",
        "add",
        "extra",
        extra.to_str().expect("utf-8 remote path"),
    ]);
    lab.jj_work(["git", "fetch", "--remote", "extra"]);
    let tips = Repo::open(&lab.work)
        .expect("open lab")
        .bookmark_tips()
        .expect("read bookmark tips");
    assert!(
        tips.contains_key(&BookmarkRef::Remote {
            branch: BranchName::new("main"),
            remote: RemoteName::new("git"),
        }),
        "fixture must include jj's internal git remote: {tips:?}"
    );
    let configured = gather_audit(&lab);
    assert!(
        configured
            .findings
            .iter()
            .all(|finding| finding.kind.to_string() != "unconfigured-remote"),
        "configured remotes must not be reported: {configured:?}"
    );

    // When: configuration is removed without deleting extra's remote-tracking ref.
    git_remote_in_colocated_config(&lab, &["remove", "extra"]);
    let home = mutation_test_home(&lab, "");
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);
    let cli_report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        cli_report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| finding["kind"] == "unconfigured-remote"),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report = gather_audit(&lab);

    // Then: the orphan remains visible as its remote bookmark, never jj's internal remote.
    assert_only_unconfigured_remote(&report, "main");
}

#[test]
fn a_remote_tracking_ref_is_reported_after_its_remote_is_removed() {
    // Given: extra contributes a tracking ref alongside the lab's standard remotes.
    // Only extra is removed: a checkout with no `upstream` is not a managed fork,
    // and the checkout must still bind to `demo` for the audit to look at it.
    let lab = Lab::new();
    let extra = lab.temp_origin();
    lab.jj_work([
        "git",
        "remote",
        "add",
        "extra",
        extra.to_str().expect("utf-8 remote path"),
    ]);
    lab.jj_work(["git", "fetch", "--remote", "extra"]);

    // When: config surgery removes the remote while retaining its tracking ref.
    git_remote_in_colocated_config(&lab, &["remove", "extra"]);

    // Then: the audit CLI reports the ref whose remote is gone.
    let home = mutation_test_home(&lab, "");
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| {
                finding["kind"] == "unconfigured-remote"
                    && finding["subject"]["bookmark"]["Remote"]["branch"] == "main"
                    && finding["subject"]["bookmark"]["Remote"]["remote"] == "extra"
            }),
        "missing extra tracking-ref finding: stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn a_conflicted_ref_on_an_unconfigured_remote_is_still_reported() {
    // Given: two concurrent fetches see non-ancestral extra/main moves.
    let lab = Lab::new();
    lab.advance_origin_branch("main", "origin main advance\n");
    lab.advance_upstream("upstream main advance\n");
    let extra = lab.temp_origin();
    lab.jj_work([
        "git",
        "remote",
        "add",
        "extra",
        extra.to_str().expect("utf-8 remote path"),
    ]);
    let before_fetch = current_operation(&lab);
    let origin_fetch = fetch_remote_without_integrating(&lab, &before_fetch, "extra");
    git_remote_in_colocated_config(
        &lab,
        &[
            "set-url",
            "extra",
            lab.upstream.to_str().expect("utf-8 remote path"),
        ],
    );
    let upstream_fetch = fetch_remote_without_integrating(&lab, &before_fetch, "extra");
    integrate_operation(&lab, &origin_fetch);
    integrate_operation(&lab, &upstream_fetch);
    let conflicted = Repo::open(&lab.work)
        .expect("open lab")
        .conflicted_bookmarks()
        .expect("read conflicted bookmarks");
    assert!(
        conflicted.iter().any(|(reference, _)| {
            *reference
                == BookmarkRef::Remote {
                    branch: BranchName::new("main"),
                    remote: RemoteName::new("extra"),
                }
        }),
        "fixture must make main@extra conflicted: {conflicted:?}"
    );

    // When: the remote config disappears but its conflicted tracking ref stays pinned.
    git_remote_in_colocated_config(&lab, &["remove", "extra"]);
    let report = gather_audit(&lab);

    // Then: a target absent from bookmark_tips still produces the remote finding.
    assert_only_unconfigured_remote(&report, "main");
}

#[test]
fn audit_reports_zombie_drift_and_anonymous_heads() {
    // Given: one locally rewritten pushed branch, one remote-only branch, and
    // a described anonymous head no workspace currently holds.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/zombie", "zombie.txt", "zombie\n");
    lab.push_branch("feat/alpha");
    lab.push_branch("feat/zombie");
    lab.rewrite_local_branch("feat/alpha", "locally moved\n");
    lab.jj_work(["bookmark", "delete", "feat/zombie"]);
    lab.jj_work(["new", "main@origin", "-m", "stranded"]);
    lab.jj_work(["new", "main@origin"]);
    let home = mutation_test_home(&lab, "");

    // When: audit runs without a forge session.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: each independently recoverable estate fact remains a separate
    // finding, while the skipped pull-head reconciliation makes the result incomplete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let kinds = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .map(|finding| finding["kind"].as_str().expect("finding kind"))
        .collect::<Vec<_>>();
    for expected in ["remote-drift", "zombie-branch", "orphan-commit"] {
        assert!(kinds.contains(&expected), "missing {expected}: {report}");
    }
}

#[test]
fn audit_does_not_treat_a_shared_release_url_as_a_separate_zombie_remote() {
    // Given: release is configured to the same remote as origin, where alpha
    // remains after its local bookmark is deleted.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.jj_work(["bookmark", "delete", "feat/alpha"]);
    let release = lab.temp_origin();
    let home = mutation_test_home(&lab, &format!("release = \"{}\"\n", release.display()));

    // When: audit classifies the one shared remote.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: origin's missing bookmark is a zombie once, never a second release
    // zombie; skipped pull-head reconciliation makes the result incomplete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let zombies = report["findings"]
        .as_array()
        .expect("findings")
        .iter()
        .filter(|finding| finding["kind"] == "zombie-branch")
        .collect::<Vec<_>>();
    assert_eq!(zombies.len(), 1, "was: {report}");
    assert!(
        zombies[0]["detail"]
            .as_str()
            .is_some_and(|detail| detail.starts_with("origin has feat/alpha")),
        "was: {zombies:?}"
    );
}

#[test]
fn audit_reports_release_drift_from_the_recorded_cut() {
    // Given: a cut records its created commit, then its bookmark moves sideways.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = mutation_test_home(&lab, "");
    let cut = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "release",
            "--repo",
            "demo",
            "cut",
            "release/2026-08-04",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_OWNER", "test-owner")
        .output()
        .expect("cut release");
    assert!(
        cut.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&cut.stdout),
        String::from_utf8_lossy(&cut.stderr)
    );
    knives::jj::set_bookmark_anywhere(&lab.work, "release/2026-08-04", "feat/alpha")
        .expect("move local release sideways");
    lab.jj_work(["workspace", "update-stale"]);

    // When: audit compares the release's current tip to its newest record.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: the recorded cut disagreement remains a content finding, but the
    // skipped pull-head reconciliation prevents a completed result.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| finding["kind"] == "release-drift"),
        "was: {report}"
    );
}

#[test]
fn audit_with_no_github_still_reconciles() {
    // Given: a local-only branch and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = mutation_test_home(&lab, "");

    // When: the optional forge check is disabled.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: local reconciliation still reports its remote fact, while the
    // skipped open-pull reconciliation leaves the audit incomplete.
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    assert!(
        report["findings"]
            .as_array()
            .expect("findings")
            .iter()
            .any(|finding| finding["kind"] == "remote-drift"),
        "was: {report}"
    );
}

/// The audit gathered over `entry` with `forge` and a store that marks
/// `fork_only` as fork-only and nothing else.
fn gather_with(
    lab: &Lab,
    entry: &knives::config::RepoEntry,
    forge: Option<&dyn knives::forge::Forge>,
    fork_only: &[&str],
) -> audit::Report {
    let fork = lab::lab_fork(lab, "demo", entry);
    let state = tempfile::tempdir().expect("state");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    for branch in fork_only {
        store.mark_fork_only(
            &BranchTarget::new(RepoName::new("demo"), BranchName::new(*branch)),
            "test",
        );
    }
    audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge,
        cache_root: None,
        workers: 4,
    })
}

/// The row for `branch`, or a panic naming every row there is.
fn row<'a>(report: &'a audit::Report, branch: &str) -> &'a audit::BranchFacts {
    report
        .branches
        .iter()
        .find(|row| row.branch.as_str() == branch)
        .unwrap_or_else(|| panic!("no row for {branch}: {:?}", report.branches))
}

#[test]
fn audit_json_without_github_has_no_pull_key_and_names_the_skip_as_a_problem() {
    // Given: one branch, a `forbidden` list, and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# plain\n");
    let home = mutation_test_home(&lab, "forbidden = [\"acme-corp\"]\n");

    // When: the binary audits with --no-github as JSON.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: the row carries its local facts, `forbidden: []` (scanned, nothing
    // found) and `member_of: []` (lone), no `pull` key at all; the template
    // was not read; and the skipped reconciliation is a problem line, so the
    // exit is 3.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let rows = report["branches"].as_array().expect("branches is an array");
    let row = rows
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .unwrap_or_else(|| panic!("feat/alpha row: {report}"));
    assert_eq!(row["tip"], commit_at(&lab, "feat/alpha").as_str());
    assert_eq!(row["origin_tip"], serde_json::Value::Null);
    assert_eq!(row["tip_matches_origin"], serde_json::Value::Null);
    assert_eq!(row["fork_only"], false);
    assert_eq!(row["forbidden"], serde_json::json!([]));
    assert_eq!(row["member_of"], serde_json::json!([]));
    assert!(row.get("pull").is_none(), "no forge, no pull facts: {row}");
    assert_eq!(report["template"], serde_json::Value::Null);
    assert!(
        report["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| problem == "open pull-head reconciliation was skipped (--no-github)"),
        "was: {report}"
    );
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "{report}"
    );
}

#[test]
fn audit_reports_branch_and_pull_facts() {
    // Given: a branch whose diff names a forbidden term, a PR answered by the forge
    // with a body missing one template heading, and an upstream trunk carrying the template.
    let lab = Lab::new();
    lab.upstream_trunk_file(
        ".github/pull_request_template.md",
        "## Overview\n\n## Approach\n\n## Testing & validation\n",
    );
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.push_branch("feat/alpha");
    let mut entry = origin_entry(&lab);
    entry.forbidden = vec!["acme-corp".to_owned()];
    let tip = commit_at(&lab, "feat/alpha");
    let fake = FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            PullRequest {
                mergeable: Some("MERGEABLE".into()),
                merge_state_status: Some("CLEAN".into()),
                review_decision: "REVIEW_REQUIRED".into(),
                ..pulls::pull_request_with_head(7, "OPEN", "feat/alpha", tip.as_str())
            },
        )]),
        checks: BTreeMap::from([(
            7,
            ChecksSummary {
                runs: vec![
                    CheckRun {
                        name: "lint".into(),
                        conclusion: Some("SUCCESS".into()),
                    },
                    CheckRun {
                        name: "test".into(),
                        conclusion: None,
                    },
                    CheckRun {
                        name: "e2e".into(),
                        conclusion: Some("ACTION_REQUIRED".into()),
                    },
                ],
            },
        )]),
        bodies: BTreeMap::from([(
            7,
            "## Overview\nfix\n\n## Testing & validation\nran it\n".to_owned(),
        )]),
        unresolved_threads: BTreeMap::from([(7, 2)]),
        ..FakeForge::default()
    };

    // When: the audit gathers with the fake forge.
    let report = gather_with(&lab, &entry, Some(&fake), &[]);

    // Then: the report carries the template once; the row carries the local
    // facts, every pull fact, the headings its body lacks and the one
    // forbidden hit; and nothing was unanswered.
    let template = report.template.as_ref().expect("template");
    assert_eq!(template.file, ".github/pull_request_template.md");
    assert_eq!(
        template.headings,
        ["Overview", "Approach", "Testing & validation"]
    );
    let row = row(&report, "feat/alpha");
    assert_eq!(row.tip, tip);
    assert_eq!(row.tip_matches_origin(), Some(true));
    assert!(!row.fork_only);
    assert_eq!(row.member_of, Vec::<BranchName>::new());
    let pull = row.pull.as_ref().expect("pull facts");
    assert_eq!(
        (
            pull.number,
            pull.mergeable.as_deref(),
            pull.merge_state_status.as_deref()
        ),
        (7, Some("MERGEABLE"), Some("CLEAN"))
    );
    assert_eq!(pull.review_decision.as_deref(), Some("REVIEW_REQUIRED"));
    assert!(pull.head_matches_tip);
    let checks = pull.checks.as_ref().expect("checks");
    assert_eq!((checks.total, checks.pending), (3, 1));
    assert_eq!(checks.conclusions.get("SUCCESS"), Some(&1));
    assert_eq!(checks.conclusions.get("ACTION_REQUIRED"), Some(&1));
    assert_eq!(pull.unresolved_review_threads, Some(2));
    assert_eq!(
        pull.template_missing.as_deref(),
        Some(&["Approach".to_owned()][..])
    );
    let hits = row.forbidden.as_ref().expect("scan ran");
    assert_eq!(hits.len(), 1, "was: {hits:?}");
    assert_eq!(
        (hits[0].file.as_str(), hits[0].line, hits[0].term.as_str()),
        ("alpha.py", 1, "acme-corp")
    );
    assert_eq!(report.problems, Vec::<String>::new());
}

#[test]
fn a_pull_with_no_review_decision_is_null_in_the_row() {
    // Given: an open pull the forge reports with an empty review decision.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let entry = origin_entry(&lab);
    let tip = commit_at(&lab, "feat/alpha");
    let fake = FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pulls::pull_request_with_head(7, "OPEN", "feat/alpha", tip.as_str()),
        )]),
        ..FakeForge::default()
    };

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, Some(&fake), &[]);

    // Then: the row's `review_decision` is `None`, and serialises as `null`.
    let pull = row(&report, "feat/alpha")
        .pull
        .as_ref()
        .expect("pull facts");
    assert_eq!(pull.review_decision, None);
    let json = serde_json::to_value(pull).expect("serialise");
    assert_eq!(json["review_decision"], serde_json::Value::Null);
}

#[test]
fn an_uppercase_pull_request_template_is_read_too() {
    // Given: upstream names its template in upper case, `.github/PULL_REQUEST_TEMPLATE.md`,
    // and a pull whose body lacks one heading.
    let lab = Lab::new();
    lab.upstream_trunk_file(
        ".github/PULL_REQUEST_TEMPLATE.md",
        "# Summary\n\n# Checklist\n",
    );
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let entry = origin_entry(&lab);
    let tip = commit_at(&lab, "feat/alpha");
    let fake = FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pulls::pull_request_with_head(9, "OPEN", "feat/alpha", tip.as_str()),
        )]),
        bodies: BTreeMap::from([(9, "# summary\nwhat changed\n".to_owned())]),
        ..FakeForge::default()
    };

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, Some(&fake), &[]);

    // Then: the upper-case file is the template, and its headings are held
    // against the body case-insensitively.
    let template = report
        .template
        .as_ref()
        .expect("the uppercase template was read");
    assert_eq!(template.file, ".github/PULL_REQUEST_TEMPLATE.md");
    assert_eq!(template.headings, ["Summary", "Checklist"]);
    assert_eq!(
        row(&report, "feat/alpha")
            .pull
            .as_ref()
            .and_then(|pull| pull.template_missing.as_deref()),
        Some(&["Checklist".to_owned()][..]),
        "heading comparison is case-insensitive"
    );
    assert_eq!(report.problems, Vec::<String>::new());
}

#[test]
fn origin_parity_reports_differs_and_absent() {
    // Given: one branch origin holds at a different tip, one origin never saw.
    let lab = Lab::new();
    lab.branch("feat/moved", "moved.txt", "one\n");
    lab.push_branch("feat/moved");
    lab.advance_origin_branch("feat/moved", "two\n"); // origin moves; local does not
    lab.branch("feat/unpushed", "u.txt", "u\n");
    let entry = origin_entry(&lab);

    // When: the audit gathers without a forge.
    let report = gather_with(&lab, &entry, None, &[]);

    // Then: the moved branch differs from its origin tip; the unpushed one has none.
    let moved = row(&report, "feat/moved");
    assert_eq!(moved.tip_matches_origin(), Some(false));
    assert!(moved.origin_tip.is_some());
    assert_ne!(moved.origin_tip.as_ref(), Some(&moved.tip));
    let unpushed = row(&report, "feat/unpushed");
    assert_eq!(unpushed.tip_matches_origin(), None);
    assert_eq!(unpushed.origin_tip, None);
}

#[test]
fn a_fork_only_branch_is_exempt_from_the_forbidden_scan() {
    // Given: a term is configured, one branch names it but is stated fork-only,
    // another branch is clean.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.branch("feat/beta", "beta.py", "# plain\n");
    let mut entry = origin_entry(&lab);
    entry.forbidden = vec!["acme-corp".to_owned()];

    // When: the audit gathers with alpha stated fork-only.
    let report = gather_with(&lab, &entry, None, &["feat/alpha"]);

    // Then: the fork-only row is not scanned at all; the other is scanned and clean.
    let alpha = row(&report, "feat/alpha");
    assert!(alpha.fork_only);
    assert!(alpha.forbidden.is_none(), "exempt: {alpha:?}");
    let beta = row(&report, "feat/beta");
    assert!(!beta.fork_only);
    assert_eq!(
        beta.forbidden.as_deref(),
        Some(&[][..]),
        "scanned, nothing found: {beta:?}"
    );
}

#[test]
fn no_forbidden_list_means_no_scan_on_any_row() {
    // Given: two branches and a registry entry with no `forbidden` list.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.branch("feat/beta", "beta.py", "# plain\n");
    let entry = origin_entry(&lab);

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, None, &[]);

    // Then: no row is scanned and none is fork-only.
    assert!(!report.branches.is_empty());
    for row in &report.branches {
        assert!(row.forbidden.is_none(), "not configured: {row:?}");
        assert!(!row.fork_only, "{row:?}");
    }
}

#[test]
fn the_forbidden_scan_measures_from_the_fork_point_not_the_trunk_tip() {
    // Given: the trunk both forks share names the term in NOTES.md; a branch forks
    // from it and adds a clean file; upstream then rewrites NOTES.md. Diffed from
    // the trunk tip, the branch's untouched NOTES.md would read as re-adding the
    // term; diffed from the fork point, the branch adds only its own file.
    let lab = Lab::new();
    lab.upstream_trunk_file("NOTES.md", "hosted at acme-corp\n");
    lab.mirror_upstream_trunk_to_origin();
    lab.branch("feat/alpha", "alpha.py", "# plain\n");
    lab.upstream_trunk_file("NOTES.md", "hosted elsewhere\n");
    let mut entry = origin_entry(&lab);
    entry.forbidden = vec!["acme-corp".to_owned()];

    // When: the audit scans the branch.
    let report = gather_with(&lab, &entry, None, &[]);

    // Then: the trunk's own line is never the branch's addition: zero hits.
    let alpha = row(&report, "feat/alpha");
    assert_eq!(
        alpha.forbidden.as_deref(),
        Some(&[][..]),
        "the trunk's line was charged to the branch: {alpha:?}"
    );
    assert!(
        !report
            .problems
            .iter()
            .any(|problem| problem.contains("forbidden")),
        "was: {:?}",
        report.problems
    );
}

#[test]
fn an_unresolvable_upstream_trunk_skips_every_scan_with_one_problem() {
    // Given: three branches, a `forbidden` list, and an entry whose trunk name
    // upstream never had, so `<trunk>@upstream` resolves to nothing.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.branch("feat/beta", "beta.py", "# plain\n");
    lab.branch("feat/gamma", "gamma.py", "# acme-corp\n");
    let mut entry = origin_entry(&lab);
    entry.base = Some("trunk-that-does-not-exist".to_owned());
    entry.forbidden = vec!["acme-corp".to_owned()];

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, None, &[]);

    // Then: one problem names the trunk, and no row was scanned.
    let scan_problems: Vec<&String> = report
        .problems
        .iter()
        .filter(|problem| problem.contains("forbidden"))
        .collect();
    assert_eq!(scan_problems.len(), 1, "was: {:?}", report.problems);
    assert!(
        scan_problems[0].starts_with(
            "upstream trunk trunk-that-does-not-exist@upstream cannot be resolved; forbidden scans skipped"
        ),
        "was: {}",
        scan_problems[0]
    );
    // `main` is a maintained branch too now that the trunk is named otherwise.
    for branch in ["feat/alpha", "feat/beta", "feat/gamma", "main"] {
        let row = row(&report, branch);
        assert!(row.forbidden.is_none(), "scanned anyway: {row:?}");
    }
}

#[test]
fn many_branches_are_scanned_in_parallel_with_one_row_each() {
    // Given: four branches — two naming the term, one clean, one fork-only that
    // names it — scanned across four threads.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.branch("feat/beta", "beta.py", "# plain\n");
    lab.branch(
        "feat/gamma",
        "gamma.py",
        "print(\"Acme-Corp\")\nx = 1\ny = \"acme-corp\"\n",
    );
    lab.branch("feat/delta", "delta.py", "# acme-corp internal\n");
    let mut entry = origin_entry(&lab);
    entry.forbidden = vec!["acme-corp".to_owned()];

    // When: the audit gathers with delta stated fork-only.
    let report = gather_with(&lab, &entry, None, &["feat/delta"]);

    // Then: one row per branch in bookmark order, each with its own answer.
    let names: Vec<&str> = report
        .branches
        .iter()
        .map(|row| row.branch.as_str())
        .collect();
    assert_eq!(
        names,
        ["feat/alpha", "feat/beta", "feat/delta", "feat/gamma"]
    );
    let lines = |branch: &str| -> Option<Vec<usize>> {
        row(&report, branch)
            .forbidden
            .as_ref()
            .map(|hits| hits.iter().map(|hit| hit.line).collect())
    };
    assert_eq!(lines("feat/alpha"), Some(vec![1]));
    assert_eq!(lines("feat/beta"), Some(Vec::new()));
    assert_eq!(lines("feat/gamma"), Some(vec![1, 3]));
    assert_eq!(lines("feat/delta"), None, "fork-only is exempt");
    assert!(
        !report
            .problems
            .iter()
            .any(|problem| problem.contains("forbidden")),
        "was: {:?}",
        report.problems
    );
}

#[test]
fn audit_without_github_still_reports_local_branch_facts() {
    // Given: one pushed and one unpushed branch, and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let entry = origin_entry(&lab);

    // When: the audit gathers without a forge.
    let report = gather_with(&lab, &entry, None, &[]);

    // Then: the local facts are on every row and no row has pull facts.
    assert!(!report.branches.is_empty());
    assert_eq!(
        row(&report, "feat/alpha").tip,
        commit_at(&lab, "feat/alpha")
    );
    assert_eq!(row(&report, "feat/alpha").tip_matches_origin(), Some(true));
    assert_eq!(row(&report, "feat/beta").tip_matches_origin(), None);
    for row in &report.branches {
        assert!(row.pull.is_none(), "no forge, no pull facts: {row:?}");
    }
}

#[test]
fn a_divergent_bookmark_is_a_problem_line_and_no_row() {
    // Given: two branches, one rewritten in two clones so its bookmark has two
    // targets after the fetch, and a forge that answers nothing.
    let lab = Lab::new();
    lab.branch("feat/ok", "ok.txt", "ok\n");
    lab.branch("feat/div", "feature.txt", "original\n");
    lab.rewrite_in_both_clones("feat/div");
    let entry = origin_entry(&lab);

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, Some(&FakeForge::default()), &[]);

    // Then: the divergent bookmark has no row, its omission is the one problem
    // line, and the audit is therefore incomplete.
    assert_eq!(
        report.problems,
        vec!["bookmark feat/div is divergent (2 targets); no row"]
    );
    assert!(
        !report
            .branches
            .iter()
            .any(|row| row.branch.as_str() == "feat/div"),
        "was: {:?}",
        report.branches
    );
    assert_eq!(row(&report, "feat/ok").member_of, Vec::<BranchName>::new());
    assert_eq!(audit::exit_for(&report), knives::cli::Exit::Incomplete);
}

#[test]
fn a_row_names_every_release_its_tip_is_a_parent_of() {
    // Given: three branches, and two releases — one over alpha and gamma, one
    // over gamma alone — plus a release commit on a rewritten alpha, so alpha's
    // current tip is a parent of the first release only.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus("release/2026-01-01", "feat/alpha", "feat/gamma");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/gamma",
        "-m",
        "second cut",
    ]);
    lab.jj_work(["bookmark", "create", "release/2026-01-02", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = origin_entry(&lab);

    // When: the audit gathers.
    let report = gather_with(&lab, &entry, Some(&FakeForge::default()), &[]);

    // Then: membership is by direct parenthood of the release commit, in
    // bookmark order; beta is lone; releases themselves get no row.
    assert_eq!(
        row(&report, "feat/alpha").member_of,
        [BranchName::new("release/2026-01-01")]
    );
    assert_eq!(
        row(&report, "feat/beta").member_of,
        Vec::<BranchName>::new()
    );
    assert_eq!(
        row(&report, "feat/gamma").member_of,
        [
            BranchName::new("release/2026-01-01"),
            BranchName::new("release/2026-01-02")
        ]
    );
    assert!(
        !report
            .branches
            .iter()
            .any(|row| row.branch.as_str().starts_with("release/")),
        "was: {:?}",
        report.branches
    );
    assert_eq!(report.problems, Vec::<String>::new());
}
