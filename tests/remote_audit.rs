#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::commands::audit;
use knives::ids::{BookmarkRef, BranchName, RemoteName};
use knives::jj::Repo;
use knives::store::Store;
use lab::{Lab, lab_entry};
use std::process::Command;
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
