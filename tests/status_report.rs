//! What a `knives status` report says.
//!
//! Registry order, a double cut, a merged branch found by the landed probe,
//! empty diffs and deleted heads settled by the forge, branch overlap after
//! upstream advances, carriers for closed pulls, and each branch's newest
//! notch — in text and in JSON.

#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/forge_shim.rs"]
mod forge_shim;
#[path = "common/lab.rs"]
mod lab;
#[path = "common/pulls.rs"]
mod pulls;

use forge_shim::{install_snapshot_gh, path_with_gh_shim, pull_record_with_fields};
use knives::commands::status;
use knives::config::RepoEntry;
use knives::ids::BranchName;
use knives::store::Store;
use lab::{
    Lab, commit_at, home_after_first_cut, knives_release, lab_entry, release_test_home,
    without_forge_elapsed,
};
use std::process::Command;

#[test]
fn status_all_reports_every_repo_in_registry_order() {
    // Given: two managed forks in one registry, named so registry order and
    // completion order can differ — the small one finishes first and must still
    // print second.
    let first = Lab::new();
    first.branch("feat/alpha", "alpha.txt", "alpha\n");
    let second = Lab::new();
    for index in 0..6 {
        second.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    // Checkouts are found by scanning one `$HOME`, so the second lab's checkout
    // moves under the first's root; its remotes are absolute paths to its own
    // bare repositories and survive the move.
    std::fs::rename(&second.work, first.temp_path().join("aardvark")).expect("move checkout");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.aardvark]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/one.git\"\n\
             [repos.zebra]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/two.git\"\n",
            second.upstream.display(),
            first.upstream.display(),
        ),
    )
    .expect("write registry");

    // When: every repo is reported at once, from outside both of them.
    let elsewhere = tempfile::tempdir().expect("somewhere else");
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "--all", "--no-github", "--no-landed"])
        .current_dir(elsewhere.path())
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", first.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("KNIVES_TIMING", "1")
        .output()
        .expect("run status --all");

    // Then: both are present, in registry order, whichever finished first.
    let text = String::from_utf8_lossy(&output.stdout);
    let aardvark = text.find("aardvark").expect("first repo reported");
    let zebra = text.find("zebra").expect("second repo reported");
    assert!(
        aardvark < zebra,
        "repos were rendered out of registry order: {text}"
    );
    assert!(text.contains("feat/b5"), "was: {text}");
    assert!(text.contains("feat/alpha"), "was: {text}");
    // And: the timing lines go to stderr, so a script's stdout is still a report.
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("timing aardvark:"), "was: {errors}");
    assert!(errors.contains("timing zebra:"), "was: {errors}");
    assert!(
        !text.contains("timing "),
        "timings leaked into stdout: {text}"
    );

    // And: `--all` is exactly each repository's own report, in registry order,
    // joined the way the serial loop joined them. `--no-landed` makes it exact:
    // with no probes, a single repo's larger probe budget cannot change a token.
    let alone = |repo: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["--text", "status", repo, "--no-github", "--no-landed"])
            .current_dir(elsewhere.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", first.temp_path())
            .env("JJ_CONFIG", "/dev/null")
            .output()
            .expect("run status for one repo");
        String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned()
    };
    assert_eq!(
        without_forge_elapsed(text.trim_end()),
        without_forge_elapsed(&format!("{}\n\n{}", alone("aardvark"), alone("zebra"))),
        "--all is not each repo's own report in registry order"
    );
}

#[test]
fn status_and_plan_report_a_double_cut() {
    // Given: a published cut, then a local release name moved sideways to a
    // sibling with different content. Both the local and origin names are in
    // the release trust boundary.
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = home_after_first_cut(&lab);
    lab.push_branch("release/2026-08-04");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    knives::jj::set_bookmark_anywhere(&lab.work, "release/2026-08-04", "feat/beta")
        .expect("move local release sideways");
    lab.jj_work(["workspace", "update-stale"]);

    // When: status and the bare release command observe the two named cuts.
    let status = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-github", "--no-landed"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run status");
    let plan = knives_release(&lab, &home, &[]);

    // Then: both read the changed trees as the same release name cut twice.
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert_eq!(
        status.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "status missed the double cut: {status_text}\n{}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status_text.contains("double-cut") && status_text.contains("release/2026-08-04"),
        "status did not name the double cut: {status_text}"
    );
    let plan_text = String::from_utf8_lossy(&plan.stdout);
    assert_eq!(
        plan.status.code(),
        Some(i32::from(knives::cli::Exit::Findings.code())),
        "plan missed the double cut: {plan_text}\n{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        plan_text.contains("release/2026-08-04 names both")
            && plan_text.contains("their trees differ"),
        "plan did not carry the double-cut finding: {plan_text}"
    );

    // Given: another published cut whose local name is rebuilt with `jj
    // duplicate`, retaining the release tree while getting a different commit.
    let rebuilt = Lab::new();
    rebuilt.branch("feat/alpha", "alpha.txt", "alpha\n");
    rebuilt.branch("feat/beta", "beta.txt", "beta\n");
    let (rebuilt_home, _consumer) = home_after_first_cut(&rebuilt);
    rebuilt.push_branch("release/2026-08-04");
    let published = commit_at(&rebuilt, "release/2026-08-04");
    rebuilt.jj_work(["duplicate", "release/2026-08-04"]);
    let duplicated = knives::jj::commits_matching(&rebuilt.work, "all()")
        .expect("list duplicated release candidates")
        .into_iter()
        .find(|candidate| {
            candidate != &published
                && knives::jj::changed_files_between(
                    &rebuilt.work,
                    published.as_str(),
                    candidate.as_str(),
                )
                .expect("compare duplicate tree")
                .is_empty()
        })
        .expect("find the duplicated release");
    knives::jj::set_bookmark_anywhere(&rebuilt.work, "release/2026-08-04", duplicated.as_str())
        .expect("point local release at duplicated cut");
    rebuilt.jj_work(["workspace", "update-stale"]);

    // When: the same two commands compare the rebuilt cut to its published copy.
    let rebuilt_status = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-github", "--no-landed"])
        .current_dir(&rebuilt.work)
        .env("KNIVES_CONFIG_HOME", rebuilt_home.path())
        .env("HOME", rebuilt.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run rebuilt status");
    let rebuilt_plan = knives_release(&rebuilt, &rebuilt_home, &[]);

    // Then: identical trees are a note, not the changed-content finding.
    let rebuilt_status_text = String::from_utf8_lossy(&rebuilt_status.stdout);
    assert!(
        rebuilt_status.status.success(),
        "status treated an identical rebuild as a finding: {rebuilt_status_text}\n{}",
        String::from_utf8_lossy(&rebuilt_status.stderr)
    );
    assert!(
        rebuilt_status_text
            .contains("release/2026-08-04 names two commits with identical trees (a rebuilt cut)"),
        "status omitted the rebuilt-cut note: {rebuilt_status_text}"
    );
    let rebuilt_plan_text = String::from_utf8_lossy(&rebuilt_plan.stdout);
    assert!(
        rebuilt_plan.status.success(),
        "plan treated an identical rebuild as a finding: {rebuilt_plan_text}\n{}",
        String::from_utf8_lossy(&rebuilt_plan.stderr)
    );
    assert!(
        rebuilt_plan_text
            .contains("release/2026-08-04 names two commits with identical trees (a rebuilt cut)"),
        "plan omitted the rebuilt-cut note: {rebuilt_plan_text}"
    );
}

#[test]
fn status_with_the_landed_probe_reports_a_merged_branch_and_leaves_no_trace() {
    // The probe path through `knives status` end to end. It is exercised here and
    // deliberately never against a live shared repository, because it mutates.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.push_branch("feat/alpha");
    lab.publish_pull("feat/alpha", 7);
    lab.squash_merge_pull(7, None);

    let entry = knives::config::RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    let before = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");
    let report = knives::commands::status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: true,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");
    let after = lab.revision(&lab.work, "children(main@upstream)", "commit_id ++ \"\\n\"");

    // The merged branch reads as already in the trunk. Stated on the branch itself
    // rather than as a finding: it is a fact about the branch, not something wrong.
    let verdicts: Vec<_> = report
        .branches
        .iter()
        .filter_map(|row| row.landed)
        .collect();
    assert!(
        verdicts.contains(&knives::detect::landed::LandedVerdict::InTrunk),
        "expected the squash-merged branch to read as in-trunk: {verdicts:?}"
    );

    // And the probe left the repository as it found it.
    assert_eq!(before, after, "the probe left commits behind");
}

#[test]
fn status_reports_empty_diff_and_deleted_head_from_completed_facts() {
    // Given: an open branch pull whose completed fact row reports no diff and no head ref
    let lab = Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    let pulls = format!(
        "[{}]",
        pull_record_with_fields(
            7,
            "OPEN",
            "feat/alpha",
            r#","additions":0,"deletions":0,"changedFiles":0,"headRef":null"#,
        )
    );
    let shim = tempfile::tempdir().expect("create forge shim directory");
    install_snapshot_gh(shim.path(), &pulls, &[]);

    // When: the real status binary consumes the completed snapshot
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "demo", "--no-landed"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .env("XDG_CACHE_HOME", shim.path().join("cache"))
        .env("PATH", path_with_gh_shim(shim.path()))
        .output()
        .expect("run status with a forge shim");

    // Then: both answered incidents remain visible, but absent merge facts make
    // the status incomplete rather than green or merely findings-only.
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        output.status.code(),
        Some(i32::from(knives::cli::Exit::Incomplete.code())),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("empty-diff"), "stdout: {stdout}");
    assert!(stdout.contains("deleted-head-ref"), "stdout: {stdout}");
    assert!(
        stdout.contains("forge did not report mergeable"),
        "stdout: {stdout}"
    );
}

#[test]
fn status_reports_branch_overlap_after_upstream_advances_without_landed_probe() {
    // Given: two maintained branches from a trunk revision that upstream has since advanced past
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "shared.txt", "alpha\n");
    lab.branch("feat/beta", "shared.txt", "beta\n");
    lab.advance_upstream("upstream advanced past the branches\n");
    let entry = knives::config::RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let store_path = lab
        .work
        .parent()
        .expect("lab work directory has a parent")
        .join("state.json");
    let store = knives::store::Store::open(store_path).expect("store");
    let name = knives::ids::RepoName::new("a-repo");

    // When: status deliberately skips only the landed replay
    let report = knives::commands::status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the grouped finding retains the shared path and both participants.
    let overlap = report
        .findings
        .iter()
        .find(|finding| finding.kind == knives::detect::FindingKind::BranchOverlap)
        .expect("the shared file is reported even without the landed probe");
    assert_eq!(overlap.items.len(), 1, "was: {overlap:?}");
    assert_eq!(
        overlap.subjects().collect::<Vec<_>>(),
        ["shared.txt: feat/alpha, feat/beta"]
    );
}

#[test]
fn status_reports_a_branch_carried_elsewhere() {
    // Given: an open branch whose tip is an ancestor of another local bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status gathers the branch report
    let report = knives::commands::status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the grouped finding retains the branch and its carrier.
    let carrier = report
        .findings
        .iter()
        .find(|finding| finding.kind == knives::detect::FindingKind::CarriedElsewhere)
        .expect("the branch carrier is reported");
    assert_eq!(carrier.items.len(), 1, "was: {carrier:?}");
    assert_eq!(
        carrier.subjects().collect::<Vec<_>>(),
        ["feat/alpha: theirs/rework"]
    );
}

#[test]
fn status_reports_a_carrier_for_a_closed_pull_request() {
    // Given: a closed pull request whose branch is reachable from another bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);
    let entry = knives::config::RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");
    let forge = knives::forge::fake::FakeForge {
        pull_requests: std::iter::once((
            BranchName::new("feat/alpha"),
            pulls::pull_request(7, "CLOSED", "feat/alpha"),
        ))
        .collect(),
        ..knives::forge::fake::FakeForge::default()
    };

    // When: status gathers the branch report with the closed pull request
    let report = knives::commands::status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: forge state does not suppress the branch or its carrier.
    let carrier = report
        .findings
        .iter()
        .find(|finding| finding.kind == knives::detect::FindingKind::CarriedElsewhere)
        .expect("the branch carrier is reported");
    assert_eq!(carrier.items.len(), 1, "was: {carrier:?}");
    assert_eq!(
        carrier.subjects().collect::<Vec<_>>(),
        ["feat/alpha: theirs/rework"]
    );
}

#[test]
fn status_does_not_report_trunk_as_a_carrier_without_landed_probe() {
    // Given: a branch whose tip is reachable from the local trunk bookmark
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.jj_work([
        "new",
        "-r",
        "main@origin",
        "-r",
        "feat/alpha",
        "-m",
        "trunk carries feature",
    ]);
    lab.jj_work(["bookmark", "set", "main", "-r", "@"]);
    lab.jj_work(["new"]);
    let entry = knives::config::RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let name = knives::ids::RepoName::new("a-repo");
    let temp = std::env::temp_dir().join(format!("knives-status-{}", std::process::id()));
    let store = knives::store::Store::open(temp.join("state.json")).expect("store");

    // When: status skips the landed probe
    let report = knives::commands::status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: trunk is never a carrier finding, even without an InTrunk verdict.
    assert!(!report.findings.iter().any(|finding| {
        finding.kind == knives::detect::FindingKind::CarriedElsewhere
            && finding.subjects().any(|subject| subject == "feat/alpha")
    }));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario asserting the notch in the row, the JSON, and the text together"
)]
fn status_carries_each_branchs_newest_notch_in_json_and_in_text() {
    // Given: a fork with a note followed by a newer machine event on one branch.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry {
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
        workspaces: None,
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work.clone(),
        "ses_fff688".to_owned(),
    );
    scribe
        .record(&knives::ledger::Draft {
            subject: Some("feat/alpha"),
            kind: knives::ledger::Kind::Note,
            disposition: None,
            text: "human conclusion".to_owned(),
            evidence: Vec::new(),
            pr: None,
            parents: Vec::new(),
        })
        .expect("human note");
    scribe
        .event(
            Some("feat/alpha"),
            "claim released; superseded by feat/next".to_owned(),
            None,
        )
        .expect("machine event");
    scribe
        .event(
            Some("feat/unrelated"),
            "claimed: something else".to_owned(),
            None,
        )
        .expect("other branch");

    // When: status gathers with the ledger available
    let report = status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: Some(&ledger),
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the human note wins over the newer event, while the row still says
    // how many ledger entries it condensed.
    let alpha = report
        .branches
        .iter()
        .find(|row| row.name.as_str() == "feat/alpha")
        .expect("the branch has a row");
    let last = alpha.notch.as_ref().expect("a breadcrumb");
    assert_eq!(last.text, "human conclusion");
    assert_eq!(last.kind, knives::ledger::Kind::Note);
    assert_eq!(last.count, 2);

    // And: it survives serialisation under the name the design fixed
    let json = serde_json::to_value(&report).expect("report serialises");
    let rows = json["branches"].as_array().expect("branches");
    let row = rows
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .expect("row");
    assert_eq!(row["notch"]["kind"], "note");
    assert_eq!(row["notch"]["text"], "human conclusion");
    assert_eq!(row["notch"]["count"], 2);
    assert!(row["notch"]["ts"].is_string());
    let beta = rows
        .iter()
        .find(|row| row["name"] == "feat/beta")
        .expect("branch without a notch");
    assert!(
        beta.get("notch").is_none(),
        "no notch is absent, not null: {beta}"
    );

    // And: the branch line carries one token for it and its masked sibling,
    // anchored to the tip the note was written against
    let anchor = row["notch"]["anchor"]
        .as_str()
        .expect("a note on a resolvable branch records its anchor");
    assert_eq!(anchor.len(), 12, "anchor is the short id: {anchor}");
    let text = status::render::render(&report, false);
    assert!(
        text.contains(&format!("\"human conclusion\" (now @{anchor})+1")),
        "was: {text}"
    );
    assert!(report.repo_notches.is_none(), "was: {report:?}");
    assert!(
        json.get("repo_notches").is_none(),
        "absent when no repo-level entry: {json}"
    );
    assert!(
        !text.contains("repo-level"),
        "no repo-level summary without a repo-level entry: {text}"
    );
}

#[test]
fn status_carries_repo_level_notches_in_json_and_text() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = lab_entry(&lab);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work.clone(),
        "ses_fff688".to_owned(),
    );
    scribe
        .event(None, "release remote needs a refresh".to_owned(), None)
        .expect("repo-level notch");

    let report = status::gather(
        &lab::lab_fork(&lab, name.as_str(), &entry),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: Some(&ledger),
            workers: 1,
        },
    )
    .expect("gather");

    let json = serde_json::to_value(&report).expect("report serialises");
    assert_eq!(json["repo_notches"]["count"], 1);
    assert_eq!(json["repo_notches"]["last"]["kind"], "event");
    assert_eq!(
        json["repo_notches"]["last"]["text"],
        "release remote needs a refresh"
    );
    let text = status::render::render(&report, false);
    assert!(
        text.contains("notches  1 repo-level, newest: \"release remote needs a refresh\""),
        "was: {text}"
    );
}

#[test]
fn status_reports_a_repo_level_immutable_heads_rule_that_differs_from_the_forks() {
    // Given: somebody stated their own rule in the repository's jj config
    let lab = Lab::new();
    lab.jj_work([
        "config",
        "set",
        "--repo",
        "revset-aliases.\"immutable_heads()\"",
        "trunk() | tags() | bookmarks(exact:\"keep\")",
    ]);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    // When: status gathers without a forge
    let report = status::gather(
        &lab::lab_fork(&lab, "demo", &lab_entry(&lab)),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the disagreement is one finding naming both rules
    let rule = report
        .findings
        .iter()
        .find(|finding| finding.kind == knives::detect::FindingKind::ImmutableHeadsRule)
        .expect("a differing repo-level rule is reported");
    assert_eq!(rule.items.len(), 1, "was: {rule:?}");
    let detail = &rule.items[0].detail;
    assert!(
        detail.contains("= `trunk() | tags() | bookmarks(exact:\"keep\")`;")
            && detail.contains(
                "under `trunk() | tags() | remote_bookmarks(exact:\"main\", exact:\"upstream\") | remote_bookmarks(exact:\"main\", exact:\"origin\")`"
            ),
        "both rules must be named: {detail}"
    );
}

#[test]
fn status_is_silent_about_the_forks_own_immutable_heads_rule() {
    // Given: the rule `knives start` writes
    let lab = Lab::new();
    let (home, _consumer) = release_test_home(&lab);
    let started = lab::knives_start(&lab, &home, "feat/beta");
    assert!(started.status.success(), "{started:?}");
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    // When: status gathers without a forge
    let report = status::gather(
        &lab::lab_fork(&lab, "demo", &lab_entry(&lab)),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            cache: None,
            registry: None,
            ledger: None,
            workers: 1,
        },
    )
    .expect("gather");

    // Then: the fork's own rule is not a finding
    assert!(
        !report
            .findings
            .iter()
            .any(|finding| finding.kind == knives::detect::FindingKind::ImmutableHeadsRule),
        "was: {:?}",
        report.findings
    );
}
