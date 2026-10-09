#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! `knives ledger migrate` through the real binary: the branch statements an
//! older knives kept in `state.json` move onto the ledger, once.

#[path = "common/lab.rs"]
mod lab;

use std::path::Path;
use std::process::Output;

use knives::ledger::{Entry, Ledger};
use knives::statement::{Statement, StatementKind};

/// A state file as an older knives left it: every statement kind, a second
/// fork, and keys this version keeps.
const LEGACY_STATE: &str = r#"{
  "claims": {},
  "tracked_pulls": {"a-repo/feat/alpha": 4545, "b-repo/feat/beta": 7},
  "fork_only": {"a-repo/ci/glue": "stated with `knives track --fork-only`"},
  "superseded": {"a-repo/feat/old": "feat/alpha"},
  "dependencies": {"a-repo/feat/alpha": ["a-repo#3", "b-repo#7"]},
  "release_included": {"a-repo/feat/alpha": "stated"}
}"#;

fn migrate(home: &Path) -> Output {
    lab::knives_command(home, home, home, &["--json", "ledger", "migrate"])
        .env("KNIVES_OWNER", "ses_migrate")
        .output()
        .expect("run knives ledger migrate")
}

fn report(output: &Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON report")
}

fn entries(home: &Path, repo: &str) -> Vec<Entry> {
    Ledger::at(home.join("ledger").join(repo))
        .entries()
        .expect("read the ledger")
}

/// Each statement entry in `entries` as `(subject, kind, value)`.
fn statements(entries: &[Entry]) -> Vec<(String, StatementKind, Option<String>)> {
    entries
        .iter()
        .filter_map(|entry| {
            let Statement { kind, value } = entry.statement.clone()?;
            Some((entry.subject.clone()?, kind, value))
        })
        .collect()
}

#[test]
fn a_second_migration_writes_nothing_and_leaves_one_entry_per_statement() {
    // Given: an older knives's state file; a ledger whose only entry about
    // a-repo's feat/alpha is an event whose prose reads like the statement but
    // states nothing; and b-repo's ledger already stating feat/beta's pull
    // request exactly as the state file does.
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("state.json"), LEGACY_STATE).expect("write state");
    Ledger::at(home.path().join("ledger").join("a-repo"))
        .append(&Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "ses_older".to_owned(),
            subject: Some("feat/alpha".to_owned()),
            kind: knives::ledger::Kind::Event,
            disposition: None,
            statement: None,
            text: "stated as #4545".to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: Some(4545),
            parents: Vec::new(),
        })
        .expect("append the prose event");
    lab::state_on_ledger(
        home.path(),
        "b-repo",
        "feat/beta",
        StatementKind::Pull,
        Some("7"),
    );

    // When: the migration runs
    let first = report(&migrate(home.path()));

    // Then: every statement the ledger did not already make became one
    // statement entry, and the state file no longer holds the maps, but keeps
    // what this version still uses.
    assert_eq!(first["wrote"], 4, "{first}");
    assert_eq!(first["already"], 1, "{first}");
    assert_eq!(first["problems"], serde_json::json!([]), "{first}");
    let a_repo = statements(&entries(home.path(), "a-repo"));
    let expected_a = [
        ("feat/alpha", StatementKind::Pull, Some("4545")),
        (
            "ci/glue",
            StatementKind::ForkOnly,
            Some("stated with `knives track --fork-only`"),
        ),
        ("feat/old", StatementKind::Superseded, Some("feat/alpha")),
        (
            "feat/alpha",
            StatementKind::Depends,
            Some("a-repo#3,b-repo#7"),
        ),
    ]
    .map(|(subject, kind, value)| (subject.to_owned(), kind, value.map(str::to_owned)));
    assert_eq!(a_repo, expected_a);
    assert_eq!(
        statements(&entries(home.path(), "b-repo")),
        [(
            "feat/beta".to_owned(),
            StatementKind::Pull,
            Some("7".to_owned())
        )]
    );
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    for map in ["tracked_pulls", "fork_only", "superseded", "dependencies"] {
        assert!(state.get(map).is_none(), "{map} remained: {state}");
    }
    assert_eq!(state["release_included"]["a-repo/feat/alpha"], "stated");

    // And: a migrated statement stamps the pull request its branch states, and
    // a store reads it back as the statement it was.
    let alpha_depends = entries(home.path(), "a-repo")
        .into_iter()
        .find(|entry| {
            entry.subject.as_deref() == Some("feat/alpha")
                && entry
                    .statement
                    .as_ref()
                    .is_some_and(|statement| statement.kind == StatementKind::Depends)
        })
        .expect("the depends entry");
    assert_eq!(alpha_depends.pr, Some(4545));
    assert_eq!(alpha_depends.owner, "ses_migrate");
    let repo = knives::ids::RepoName::new("a-repo");
    let store = knives::store::Store::open(home.path().join("state.json"), &[&repo])
        .expect("open the store");
    let alpha = knives::ids::BranchTarget::new(repo, knives::ids::BranchName::new("feat/alpha"));
    assert_eq!(store.tracked_pull(&alpha), Some(4545));
    assert_eq!(store.dependencies(&alpha).len(), 2);

    // When: the old maps come back (a second machine's copy, or an older
    // binary's write) and the migration runs again
    std::fs::write(home.path().join("state.json"), LEGACY_STATE).expect("restore state");
    let second = report(&migrate(home.path()));

    // Then: it wrote nothing, and every statement still has exactly one entry
    assert_eq!(second["wrote"], 0, "{second}");
    assert_eq!(second["already"], 5, "{second}");
    assert_eq!(statements(&entries(home.path(), "a-repo")), expected_a);
    assert_eq!(statements(&entries(home.path(), "b-repo")).len(), 1);
}

#[test]
fn a_ledger_that_cannot_be_read_keeps_every_statement_in_the_state_file() {
    // Given: a state file with statements for a fork whose ledger is broken
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(home.path().join("state.json"), LEGACY_STATE).expect("write state");
    let broken = home.path().join("ledger").join("b-repo");
    std::fs::create_dir_all(&broken).expect("ledger directory");
    std::fs::write(
        broken.join("20260815T221403.000000000Z-0000.md"),
        "not a ledger entry\n",
    )
    .expect("broken entry");

    // When: the migration runs
    let output = migrate(home.path());

    // Then: it says what failed, exits incomplete, and drops nothing
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(
        report["problems"][0]
            .as_str()
            .is_some_and(|problem| problem.starts_with("b-repo: 1 statement(s) not migrated")),
        "{report}"
    );
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert_eq!(state["tracked_pulls"]["b-repo/feat/beta"], 7);
}

#[test]
fn an_unmigrated_state_file_is_refused_naming_the_command_and_opens_after_migrating() {
    // Given: an older knives's state file, holding a claim and statements,
    // in a config home whose registry manages one fork.
    let lab = lab::Lab::new();
    let (home, _consumer) = lab::release_test_home(&lab);
    let legacy = r#"{
  "claims": {"demo/feat/claimed": {"repo": "demo", "branch": "feat/claimed",
    "owner": "agent-one", "why": "porting", "started": "2026-01-01T00:00:00Z", "files": []}},
  "tracked_pulls": {"demo/feat/claimed": 4545}
}"#;
    std::fs::write(home.path().join("state.json"), legacy).expect("write state");
    let status = || {
        lab::knives_command(
            &lab.work,
            home.path(),
            lab.temp_path(),
            &["--json", "status", "demo", "--no-github", "--no-landed"],
        )
        .output()
        .expect("run knives status")
    };
    let hook = |cwd: &Path| {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["hook", "claude-code"])
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("HOME", home.path())
            .env("JJ_CONFIG", "/dev/null")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn hook");
        let event = serde_json::json!({
            "session_id": "migrate-test",
            "hook_event_name": "SessionStart",
            "source": "startup",
            "cwd": cwd,
        });
        std::io::Write::write_all(
            &mut child.stdin.take().expect("stdin"),
            event.to_string().as_bytes(),
        )
        .expect("write event");
        child.wait_with_output().expect("wait for hook")
    };

    // When: status runs, and an agent session starts in the fork.
    let refused = status();
    let noticed = hook(&lab.work);

    // Then: status exits incomplete naming the command verbatim, and the
    // session is told so in its context rather than shown an empty roster.
    let errors = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(refused.status.code(), Some(3), "{errors}");
    assert!(errors.contains("Run `knives ledger migrate`"), "{errors}");
    assert!(noticed.status.success());
    let context = String::from_utf8_lossy(&noticed.stdout);
    assert!(
        context.contains("Run `knives ledger migrate`")
            && context.contains("do not assume a branch is free"),
        "{context}"
    );
    assert!(!context.contains("No branch is claimed"), "{context}");

    // When: the migration runs, and the same two commands run again.
    report(&migrate(home.path()));
    let opened = status();
    let claimed = hook(&lab.work);

    // Then: status answers with the migrated statement and the hook shows
    // the claim the state file still holds.
    let report: serde_json::Value = serde_json::from_slice(&opened.stdout).unwrap_or_else(|_| {
        panic!("{}", String::from_utf8_lossy(&opened.stderr));
    });
    assert!(report.get("problems").is_none(), "{report}");
    let context = String::from_utf8_lossy(&claimed.stdout);
    assert!(context.contains("feat/claimed (agent-one"), "{context}");
}
