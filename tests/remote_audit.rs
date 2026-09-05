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
/// Registry home for commands that reconcile the lab's live bare remotes.
fn mutation_test_home(lab: &Lab, release: Option<&std::path::Path>) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    let release = release.map_or_else(String::new, |path| {
        format!("release = \"{}\"\n", path.display())
    });
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"{}\"\n{release}",
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
    let home = mutation_test_home(&lab, None);

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
    let home = mutation_test_home(&lab, None);

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
    let home = mutation_test_home(&lab, None);
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
    let config = mutation_test_home(&lab, Some(&release));

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
    let home = mutation_test_home(&lab, None);
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
    let home = mutation_test_home(&lab, None);
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
    let home = mutation_test_home(&lab, None);

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
    let home = mutation_test_home(&lab, Some(&release));

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
    let home = mutation_test_home(&lab, None);
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
    let home = mutation_test_home(&lab, None);

    // When: the optional forge check is disabled.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: local reconciliation still reports its remote fact, while the
    // skipped open-pull reconciliation is an unanswered question.
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
    assert!(
        report["problems"]
            .as_array()
            .expect("problems")
            .iter()
            .any(|problem| problem
                .as_str()
                .is_some_and(|problem| problem.contains("pull-head reconciliation was skipped"))),
        "was: {report}"
    );
}

#[test]
fn audit_json_carries_a_branches_array_with_local_facts() {
    // Given: one pushed branch and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let home = mutation_test_home(&lab, None);

    // When: the binary audits with --no-github.
    let output = knives_audit(&lab, &home, &["demo", "--no-github"]);

    // Then: every row carries its local facts even though no pull was consulted.
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("audit emits JSON");
    let rows = report["branches"].as_array().expect("branches is an array");
    let row = rows
        .iter()
        .find(|row| row["branch"] == "feat/alpha")
        .unwrap_or_else(|| panic!("feat/alpha row: {report}"));
    assert_eq!(row["tip"], commit_at(&lab, "feat/alpha").as_str());
    assert_eq!(row["tip_matches_origin"], true);
    assert!(row.get("pull").is_none(), "no forge, no pull facts: {row}");
}

#[test]
fn file_text_and_diff_git_read_the_upstream_trunk() {
    // Given: a template on upstream's trunk and a branch adding one line.
    let lab = Lab::new();
    lab.upstream_trunk_file(".github/pull_request_template.md", "## Overview\n");
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");

    // When/Then: the file reads at `main@upstream`, is absent at `main@origin`,
    // and the diff names the added line.
    let text = knives::jj::file_text(
        lab.work_path(),
        "main@upstream",
        ".github/pull_request_template.md",
    )
    .expect("read a present file");
    assert_eq!(text.as_deref(), Some("## Overview\n"));
    let absent = knives::jj::file_text(
        lab.work_path(),
        "main@origin",
        ".github/pull_request_template.md",
    )
    .expect("a missing path is not an error");
    assert_eq!(absent, None);
    let unresolvable = knives::jj::file_text(lab.work_path(), "no-such-rev", "alpha.py");
    assert!(
        unresolvable.is_err(),
        "a bad revision must not read as absent"
    );
    let diff = knives::jj::diff_git(lab.work_path(), "main@upstream", "feat/alpha").expect("diff");
    assert!(diff.contains("+++ b/alpha.py"), "was: {diff}");
    assert!(diff.contains("+# wired for acme-corp's IaC"), "was: {diff}");
    assert!(
        diff.contains("--- a/.github/pull_request_template.md") && diff.contains("+++ /dev/null"),
        "the branch lacks upstream's newer template, so the diff removes it: {diff}"
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
    let fork = lab::lab_fork(&lab, "demo", &entry);
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
    let state = tempfile::tempdir().expect("state");
    let store = Store::open(state.path().join("state.json")).expect("store");

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: Some(&fake),
        cache_root: None,
    });

    let row = report
        .branches
        .iter()
        .find(|row| row.branch == "feat/alpha")
        .expect("row");
    assert_eq!(row.tip, tip.as_str());
    assert_eq!(row.tip_matches_origin, Some(true));
    assert!(!row.fork_only);
    let pull = row.pull.as_ref().expect("pull facts");
    assert_eq!(
        (
            pull.number,
            pull.mergeable.as_deref(),
            pull.merge_state_status.as_deref()
        ),
        (7, Some("MERGEABLE"), Some("CLEAN"))
    );
    assert_eq!(pull.review_decision, "REVIEW_REQUIRED");
    assert!(pull.head_matches_tip);
    let checks = pull.checks.as_ref().expect("checks");
    assert_eq!((checks.total, checks.pending), (3, 1));
    assert_eq!(checks.conclusions.get("SUCCESS"), Some(&1));
    assert_eq!(checks.conclusions.get("ACTION_REQUIRED"), Some(&1));
    assert_eq!(pull.unresolved_review_threads, Some(2));
    let template = pull.template.as_ref().expect("template");
    assert_eq!(template.file, ".github/pull_request_template.md");
    assert_eq!(
        template.headings,
        ["Overview", "Approach", "Testing & validation"]
    );
    assert_eq!(template.missing_from_body, ["Approach"]);
    let hits = row.forbidden.as_ref().expect("scan ran");
    assert_eq!(hits.len(), 1, "was: {hits:?}");
    assert_eq!(
        (hits[0].file.as_str(), hits[0].line, hits[0].term.as_str()),
        ("alpha.py", 1, "acme-corp")
    );
    assert_eq!(report.problems, Vec::<String>::new());
}

#[test]
fn an_uppercase_pull_request_template_is_read_too() {
    // Given: upstream names its template `.github/PULL_REQUEST_TEMPLATE.md`, as
    // several registered upstreams do, and a pull whose body lacks one heading.
    let lab = Lab::new();
    lab.upstream_trunk_file(
        ".github/PULL_REQUEST_TEMPLATE.md",
        "# Summary\n\n# Checklist\n",
    );
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    let entry = origin_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let tip = commit_at(&lab, "feat/alpha");
    let fake = FakeForge {
        pull_requests: BTreeMap::from([(
            BranchName::new("feat/alpha"),
            pulls::pull_request_with_head(9, "OPEN", "feat/alpha", tip.as_str()),
        )]),
        bodies: BTreeMap::from([(9, "# summary\nwhat changed\n".to_owned())]),
        ..FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state");
    let store = Store::open(state.path().join("state.json")).expect("store");

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: Some(&fake),
        cache_root: None,
    });

    let row = report
        .branches
        .iter()
        .find(|row| row.branch == "feat/alpha")
        .expect("row");
    let template = row
        .pull
        .as_ref()
        .and_then(|pull| pull.template.as_ref())
        .expect("the uppercase template was read");
    assert_eq!(template.file, ".github/PULL_REQUEST_TEMPLATE.md");
    assert_eq!(template.headings, ["Summary", "Checklist"]);
    assert_eq!(
        template.missing_from_body,
        ["Checklist"],
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
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let state = tempfile::tempdir().expect("state");
    let store = Store::open(state.path().join("state.json")).expect("store");

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: None,
        cache_root: None,
    });

    let row = |name: &str| {
        report
            .branches
            .iter()
            .find(|row| row.branch == name)
            .expect(name)
    };
    assert_eq!(row("feat/moved").tip_matches_origin, Some(false));
    assert!(row("feat/moved").origin_tip.is_some());
    assert_ne!(
        row("feat/moved").origin_tip.as_deref(),
        Some(row("feat/moved").tip.as_str())
    );
    assert_eq!(row("feat/unpushed").tip_matches_origin, None);
    assert_eq!(row("feat/unpushed").origin_tip, None);
}

#[test]
fn a_fork_only_branch_is_exempt_from_the_forbidden_scan_and_no_config_means_no_scan() {
    // Given: a branch whose diff names the configured term.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.py", "# wired for acme-corp's IaC\n");
    lab.branch("feat/beta", "beta.py", "# plain\n");

    // (1) The term is configured and the branch is stated fork-only: no scan.
    let mut entry = origin_entry(&lab);
    entry.forbidden = vec!["acme-corp".to_owned()];
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let state = tempfile::tempdir().expect("state");
    let mut store = Store::open_for_update(state.path().join("state.json")).expect("store");
    store.mark_fork_only(
        &BranchTarget::new(RepoName::new("demo"), BranchName::new("feat/alpha")),
        "test",
    );

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: None,
        cache_root: None,
    });

    let alpha = report
        .branches
        .iter()
        .find(|row| row.branch == "feat/alpha")
        .expect("feat/alpha row");
    assert!(alpha.fork_only);
    assert!(alpha.forbidden.is_none(), "exempt: {alpha:?}");
    let beta = report
        .branches
        .iter()
        .find(|row| row.branch == "feat/beta")
        .expect("feat/beta row");
    assert!(!beta.fork_only);
    assert_eq!(
        beta.forbidden.as_deref(),
        Some(&[][..]),
        "scanned, nothing found: {beta:?}"
    );
    drop(store);

    // (2) No term configured: no scan on any row, and nothing is fork-only.
    let entry = origin_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let state = tempfile::tempdir().expect("state");
    let store = Store::open(state.path().join("state.json")).expect("store");

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: None,
        cache_root: None,
    });

    assert!(!report.branches.is_empty());
    for row in &report.branches {
        assert!(row.forbidden.is_none(), "not configured: {row:?}");
        assert!(!row.fork_only, "{row:?}");
    }
}

#[test]
fn audit_without_github_still_reports_local_branch_facts() {
    // Given: one pushed and one unpushed branch, and no forge transport.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let entry = origin_entry(&lab);
    let fork = lab::lab_fork(&lab, "demo", &entry);
    let state = tempfile::tempdir().expect("state");
    let store = Store::open(state.path().join("state.json")).expect("store");

    let report = audit::gather(&audit::AuditInput {
        fork: &fork,
        store: &store,
        forge: None,
        cache_root: None,
    });

    assert!(!report.branches.is_empty());
    let row = |name: &str| {
        report
            .branches
            .iter()
            .find(|row| row.branch == name)
            .expect(name)
    };
    assert_eq!(
        row("feat/alpha").tip,
        commit_at(&lab, "feat/alpha").as_str()
    );
    assert_eq!(row("feat/alpha").tip_matches_origin, Some(true));
    assert_eq!(row("feat/beta").tip_matches_origin, None);
    for row in &report.branches {
        assert!(row.pull.is_none(), "no forge, no pull facts: {row:?}");
    }
    assert!(
        report
            .problems
            .iter()
            .any(|problem| problem == "open pull-head reconciliation was skipped (--no-github)"),
        "was: {:?}",
        report.problems
    );
}
