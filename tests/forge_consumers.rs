#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::commands::repos;
use knives::config::RepoEntry;
use knives::consumer_pins::{ConsumerHeadMemo, scan_consumer_for};
use knives::forge::ConsumerHead;
use knives::jj::Repo;
use lab::{Lab, commit_at, knives_release, operation_ids, release_parents, release_test_home};
use std::collections::BTreeMap;
use std::process::Command;

fn reset_consumer_to_origin(consumer: &std::path::Path) {
    let status = Command::new("git")
        .args(["reset", "--hard", "origin/main"])
        .current_dir(consumer)
        .status()
        .expect("reset consumer to origin");
    assert!(status.success(), "reset consumer to origin");
}

fn rename_consumer_remote(consumer: &std::path::Path, from: &str, to: &str) {
    let status = Command::new("git")
        .args(["remote", "rename", from, to])
        .current_dir(consumer)
        .status()
        .expect("rename consumer remote");
    assert!(status.success(), "rename consumer remote");
}
#[test]
fn a_fixed_pin_locked_to_an_ancestor_is_behind() {
    let lab = Lab::new();
    lab.branch("integration", "base.txt", "base\n");
    let repo = Repo::open(&lab.work).expect("open ancestor");
    let ancestor = repo.resolve_commit("integration").expect("ancestor");
    lab.jj_work(["new", "-r", "integration", "-m", "advance integration"]);
    std::fs::write(lab.work.join("advance.txt"), "advance\n").expect("advance integration");
    lab.jj_work(["bookmark", "set", "integration", "-r", "@"]);
    lab.jj_work(["new"]);

    let consumer = "acme/consumer";
    let commit = "aaaaaaaaaaaaaaaa";
    let locked: String = ancestor.as_str().chars().take(12).collect();
    let forge = knives::forge::fake::FakeForge {
        heads: BTreeMap::from([(
            consumer.to_owned(),
            ConsumerHead {
                branch: "main".to_owned(),
                commit: commit.to_owned(),
            },
        )]),
        files: BTreeMap::from([(
            (consumer.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
            format!("url = \"https://forge.invalid/o/repo.git?branch=integration#{locked}\"\n"),
        )]),
        ..knives::forge::fake::FakeForge::default()
    };
    let heads = ConsumerHeadMemo::default();
    let entry = RepoEntry {
        release_branch: Some("integration".to_owned()),
        consumers: vec![consumer.to_owned()],
        ..RepoEntry::new(
            "https://forge.invalid/up/repo.git",
            "https://forge.invalid/o/repo.git",
        )
    };
    let repo = Repo::open(&lab.work).expect("open advanced branch");

    let fork = lab::lab_fork(&lab, "demo", &entry);
    let lag = repos::pin_lag(&fork, None, Some(&repo), &forge, None, &heads);

    assert!(
        lag.lag.as_ref().is_some_and(|lag| lag.contains(&locked)),
        "lag: {:?}",
        lag.lag
    );
}

#[test]
fn a_consumer_checkout_parked_behind_its_origin_does_not_produce_a_false_behind() {
    // Given: a consumer repo whose origin trunk pins the newest release while
    // the checkout's working copy still shows an older pin — the exact state
    // that produced false BEHIND findings twice.
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    // When: the consumer is scanned.
    let scan = scan_consumer_for(&consumer, Some("tool"), &knives::ids::ReleaseScheme::Dated);

    // Then: the pin is the origin trunk's, and the checkout's lag is a note.
    assert_eq!(scan.pins.len(), 1, "was: {:?}", scan.pins);
    assert_eq!(scan.pins[0].reference, "release/2026-07-28");
    assert!(
        scan.notes.iter().any(|note| note.contains("behind")),
        "the stale checkout is annotated, not silently trusted: {:?}",
        scan.notes
    );
    assert!(scan.problems.is_empty());
}

#[test]
fn a_dev_trunk_consumer_checkout_uses_its_origin_head_pin() {
    let lab = Lab::with_trunk("dev");
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );

    let scan = scan_consumer_for(&consumer, Some("tool"), &knives::ids::ReleaseScheme::Dated);

    assert_eq!(scan.pins.len(), 1, "was: {:?}", scan.pins);
    assert_eq!(scan.pins[0].reference, "release/2026-07-28");
    assert!(
        scan.notes.iter().any(|note| note.contains("origin/dev")),
        "the origin default branch is preserved: {:?}",
        scan.notes
    );
    assert!(scan.problems.is_empty());
}

#[test]
fn a_consumer_without_an_origin_remote_uses_its_current_working_copy_pin() {
    let lab = Lab::new();
    let consumer = lab.consumer_with_pin_history(
        "uv.lock",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-20\"\n",
        "url = \"https://forge.invalid/o/tool.git?rev=release%2F2026-07-28\"\n",
    );
    reset_consumer_to_origin(&consumer);
    rename_consumer_remote(&consumer, "origin", "upstream");

    let scan = scan_consumer_for(&consumer, Some("tool"), &knives::ids::ReleaseScheme::Dated);

    assert_eq!(scan.pins.len(), 1, "was: {:?}", scan.pins);
    assert_eq!(scan.pins[0].reference, "release/2026-07-28");
    assert_eq!(
        scan.notes,
        vec![format!(
            "{}: no origin trunk resolved; pins read from the working copy",
            consumer.display()
        )]
    );
    assert!(scan.problems.is_empty());
}
#[test]
fn consumers_reports_stale_and_behind_locks() {
    let lab = Lab::new();
    lab.branch("release/2026-08-04", "release.txt", "first\n");
    lab.push_branch("release/2026-08-04");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::write(
        consumer.path().join("uv.lock"),
        "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-04#deadbeef\" }\n",
    )
    .expect("write frozen pin");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");
    let knives = || {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args([
                "--text",
                "consumers",
                "demo",
                "--consumer",
                consumer.path().to_str().expect("utf-8 consumer path"),
            ])
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", lab.temp_path())
            .env("JJ_CONFIG", "/dev/null")
            .output()
            .expect("run consumers")
    };

    let stale = knives();

    assert_eq!(stale.status.code(), Some(1), "stderr: {:?}", stale.stderr);
    assert!(
        String::from_utf8_lossy(&stale.stdout).contains("stale lock: expected @"),
        "stdout: {}",
        String::from_utf8_lossy(&stale.stdout)
    );

    lab.branch("release/2026-08-05", "release.txt", "second\n");
    lab.push_branch("release/2026-08-05");

    let behind = knives();

    assert_eq!(behind.status.code(), Some(1), "stderr: {:?}", behind.stderr);
    assert!(
        String::from_utf8_lossy(&behind.stdout).contains("behind: newest is release/2026-08-05"),
        "stdout: {}",
        String::from_utf8_lossy(&behind.stdout)
    );
}

#[test]
fn consumers_reports_a_missing_local_path_as_incomplete() {
    let lab = Lab::new();
    lab.branch("release/2026-08-04", "release.txt", "first\n");
    lab.push_branch("release/2026-08-04");
    let home = tempfile::tempdir().expect("create config home");
    let missing = home.path().join("gone");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "consumers",
            "demo",
            "--consumer",
            missing.to_str().expect("utf-8 missing consumer path"),
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run consumers");

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("PROBLEM: not found"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn consumers_leaves_pins_unclassified_when_live_release_refs_fail() {
    let lab = Lab::new();
    lab.branch("release/2026-08-05", "release.txt", "published\n");
    lab.push_branch("release/2026-08-05");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::write(
        consumer.path().join("uv.lock"),
        "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-05#deadbeef\" }\n",
    )
    .expect("write frozen pin");
    let home = tempfile::tempdir().expect("create config home");
    let unavailable = home.path().join("unavailable-release-remote");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\n",
            lab.upstream.display(),
            unavailable.display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "consumers",
            "demo",
            "--consumer",
            consumer.path().to_str().expect("utf-8 consumer path"),
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run consumers");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(stdout.contains("unclassified"), "stdout: {stdout}");
    assert!(
        !stdout.contains("stale lock:") && !stdout.contains("behind:"),
        "live remote failure must not derive a local verdict: {stdout}"
    );
}

#[test]
fn consumers_reports_an_unreadable_pin_file_as_incomplete() {
    let lab = Lab::new();
    lab.branch("release/2026-08-05", "release.txt", "published\n");
    lab.push_branch("release/2026-08-05");
    let consumer = tempfile::tempdir().expect("create consumer");
    std::fs::create_dir(consumer.path().join("uv.lock")).expect("create unreadable pin file");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/tool.git\"\nrelease = \"{}\"\n",
            lab.upstream.display(),
            lab.temp_origin().display(),
        ),
    )
    .expect("write registry");

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "consumers",
            "demo",
            "--consumer",
            consumer.path().to_str().expect("utf-8 consumer path"),
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("HOME", lab.temp_path())
        .env("JJ_CONFIG", "/dev/null")
        .output()
        .expect("run consumers");
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(3), "stderr: {:?}", output.stderr);
    assert!(
        stdout.contains("PROBLEM: could not read uv.lock"),
        "stdout: {stdout}"
    );
    assert!(
        !stdout.contains("does not pin demo"),
        "a failed file scan must not make a no-pin claim: {stdout}"
    );
}
#[test]
fn mutating_release_commands_refuse_plan_problems_before_writing() {
    // Given: a release that would otherwise be mutable, and a --consumer path that
    // is not there — a mistyped path is a question the plan cannot answer, not an
    // empty scan.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, consumer) = release_test_home(&lab);
    lab.advance_upstream("upstream advance\n");
    let missing = consumer.with_file_name("consumer-that-was-never-cloned");
    std::fs::write(
        home.path().join("local-consumer"),
        missing.display().to_string(),
    )
    .expect("point the release helpers at the missing consumer");
    let operations_before = operation_ids(&lab.work);

    // When: every mutating release verb sees an incomplete consumer plan.
    for args in [
        ["rebase", "main@upstream"].as_slice(),
        ["include", "feat/gamma"].as_slice(),
        ["cut", "release/2026-08-05"].as_slice(),
    ] {
        let output = knives_release(&lab, &home, args);

        // Then: each exits incomplete after reporting the plan, before a jj write.
        assert_eq!(output.status.code(), Some(3), "output: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            stdout.contains("consumer-that-was-never-cloned: not found"),
            "output: {output:?}"
        );
        assert!(
            !stdout.contains("nothing pins this release"),
            "a consumer that could not be consulted must not make a no-pin claim: {stdout}"
        );
        assert_eq!(
            operation_ids(&lab.work),
            operations_before,
            "{args:?} wrote despite plan problems"
        );
    }
}

#[test]
fn no_recorded_consumers_is_an_answer_not_a_refusal() {
    // Given: a fork whose registry entry records no consumer and whose caller
    // passes none — a tool fork consumed by an install, not by a lockfile, has
    // no consumer path to hand over.
    let lab = Lab::new();
    let release = "release/2026-08-04";
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.octopus(release, "feat/alpha", "feat/beta");
    let (home, _consumer) = release_test_home(&lab);
    std::fs::remove_file(home.path().join("local-consumer")).expect("remove local consumer");

    // When: the plan is asked, then an edit is applied.
    let plan = knives_release(&lab, &home, &[]);
    let planned = String::from_utf8_lossy(&plan.stdout);
    let include = knives_release(&lab, &home, &["include", "feat/gamma"]);

    // Then: nothing recorded pins the release, which is an answer: the plan says
    // so as a note and exits clean, and the edit applies.
    assert_eq!(plan.status.code(), Some(0), "plan: {plan:?}");
    assert!(
        planned.contains("! no consumers recorded"),
        "the plan must still say no consumer is recorded: {planned}"
    );
    assert!(
        !planned.contains("!! no consumers recorded"),
        "an unrecorded consumer is not a problem the plan could not answer: {planned}"
    );
    assert!(
        planned.contains("nothing pins this release: either is safe"),
        "{planned}"
    );
    assert_eq!(include.status.code(), Some(0), "include: {include:?}");
    assert!(
        release_parents(&lab, release).contains(&commit_at(&lab, "feat/gamma")),
        "the include was refused on an answered question"
    );
}
