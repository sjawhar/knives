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
            email: None,
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
    let repo = knives::ids::UpstreamName::new("a-repo");
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

/// A config home whose registry names two forks with forge upstreams: `demo`,
/// kept under `acme/demo`, and `acme`, kept under `acme/acme`, whose registry
/// key is the owner directory both now share.
fn renaming_home() -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("config home");
    std::fs::write(
        home.path().join("repos.toml"),
        "[repos.demo]\nupstream = \"https://forge.invalid/Acme/Demo\"\n\
         origin = \"https://forge.invalid/ours/demo\"\n\n\
         [repos.acme]\nupstream = \"https://forge.invalid/acme/acme.git\"\n\
         origin = \"https://forge.invalid/ours/acme\"\n",
    )
    .expect("write registry");
    home
}

/// One note in `home`'s ledger directory `fork`, written at `ts`; the path of
/// its file.
fn note(home: &Path, fork: &str, ts: &str, text: &str) -> std::path::PathBuf {
    let ledger = Ledger::at(home.join("ledger").join(fork));
    ledger
        .append(&Entry {
            ts: ts.to_owned(),
            owner: "ses_older".to_owned(),
            email: None,
            subject: None,
            kind: knives::ledger::Kind::Note,
            disposition: None,
            statement: None,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: None,
            pr: None,
            parents: Vec::new(),
        })
        .expect("append a note");
    let mut files: Vec<_> = std::fs::read_dir(home.join("ledger").join(fork))
        .expect("read the ledger directory")
        .map(|dirent| dirent.expect("dirent").path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    files.sort();
    files.pop().expect("the note's file")
}

/// A state file as a knives that kept forks under their registry keys left
/// it, with every map that names a fork, and one legacy statement map.
const ALIASED_STATE: &str = r#"{
  "claims": {"demo/feat/x": {"repo": "demo", "branch": "feat/x", "owner": "agent-one",
    "why": "porting", "started": "2026-01-01T00:00:00Z", "files": []}},
  "comment_marks": {"demo#7": "2026-01-02T00:00:00Z"},
  "pull_states": {"demo#7": "OPEN", "acme#3": "MERGED"},
  "foreign_parents": {"demo/99": "a maintainer's fix"},
  "conventions": {"demo/AGENTS.md": "abc", "acme/CONTRIBUTING.md": "def"},
  "pull_heads": {"demo": {"7": "head-7"}},
  "tracked_pulls": {"demo/feat/x": 7},
  "dependencies": {"demo/feat/x": ["acme#3", "demo#8"]}
}"#;

/// A sightings sidecar as the same knives left it: workspaces keyed by the
/// registry key, one already under its upstream name (written by a newer
/// knives after an older one), and one sighting under both, the upstream
/// name's being the later.
const ALIASED_SEEN: &str = r#"{
  "owners": {"harness-session": {"agent-one": "2026-01-01T00:00:00Z"}},
  "workspaces": {
    "demo/feat-x": "2026-01-02T00:00:00Z",
    "acme/demo/feat-x": "2026-01-05T00:00:00Z",
    "demo/feat-y": "2026-01-04T00:00:00Z",
    "acme/fix-z": "2026-01-03T00:00:00Z"
  }
}"#;

/// Each `(from, to)` path a migration report says it moved.
fn moved_paths(migrated: &serde_json::Value) -> Vec<(String, String)> {
    migrated["moved"]
        .as_array()
        .expect("moved")
        .iter()
        .map(|moved| {
            (
                moved["from"].as_str().expect("from").to_owned(),
                moved["to"].as_str().expect("to").to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_migration_moves_each_forks_ledger_and_state_keys_to_its_upstream_name() {
    // Given: a state file and ledger an older knives left, keeping each fork
    // under its registry key.
    let home = renaming_home();
    std::fs::write(home.path().join("state.json"), ALIASED_STATE).expect("write state");
    std::fs::write(home.path().join("seen.json"), ALIASED_SEEN).expect("write sightings");
    let demo_note = note(home.path(), "demo", "2026-01-01T00:00:00Z", "about demo");
    let acme_note = note(home.path(), "acme", "2026-01-01T00:00:01Z", "about acme");
    let demo_bytes = std::fs::read(&demo_note).expect("read demo's note");
    let acme_bytes = std::fs::read(&acme_note).expect("read acme's note");

    // When: the migration runs.
    let migrated = report(&migrate(home.path()));

    // Then: each fork's ledger directory moved, byte for byte, to its
    // upstream's `<owner>/<name>`, and the report names the old path.
    let ledger = home.path().join("ledger");
    let name = |path: &Path| path.file_name().expect("a file name").to_owned();
    let moved_demo = ledger.join("acme/demo").join(name(&demo_note));
    let moved_acme = ledger.join("acme/acme").join(name(&acme_note));
    assert_eq!(
        std::fs::read(&moved_demo).ok(),
        Some(demo_bytes),
        "{migrated}"
    );
    assert_eq!(
        std::fs::read(&moved_acme).ok(),
        Some(acme_bytes),
        "{migrated}"
    );
    assert!(!ledger.join("demo").exists(), "{migrated}");
    assert!(!acme_note.exists(), "{migrated}");
    let moved = moved_paths(&migrated);
    let path = |relative: &str| ledger.join(relative).display().to_string();
    assert_eq!(
        moved,
        [
            (path("acme"), path("acme/acme")),
            (path("demo"), path("acme/demo"))
        ],
        "{migrated}"
    );

    // And: every state key that named a fork names its upstream now, each
    // reported, and the legacy statements reached the moved ledger with the
    // repository inside each requirement renamed.
    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert_eq!(
        state["claims"]["acme/demo/feat/x"]["repo"], "acme/demo",
        "{state}"
    );
    assert_eq!(
        state["comment_marks"]["acme/demo#7"], "2026-01-02T00:00:00Z",
        "{state}"
    );
    assert_eq!(state["pull_states"]["acme/demo#7"], "OPEN", "{state}");
    assert_eq!(state["pull_states"]["acme/acme#3"], "MERGED", "{state}");
    assert_eq!(
        state["foreign_parents"]["acme/demo/99"], "a maintainer's fix",
        "{state}"
    );
    assert_eq!(
        state["conventions"]["acme/demo/AGENTS.md"], "abc",
        "{state}"
    );
    assert_eq!(
        state["conventions"]["acme/acme/CONTRIBUTING.md"], "def",
        "{state}"
    );
    assert_eq!(state["pull_heads"]["acme/demo"]["7"], "head-7", "{state}");
    assert_eq!(
        migrated["renamed"].as_array().map(Vec::len),
        Some(8),
        "{migrated}"
    );
    assert_eq!(
        statements(&entries(home.path(), "acme/demo")),
        [
            (
                "feat/x".to_owned(),
                StatementKind::Pull,
                Some("7".to_owned())
            ),
            (
                "feat/x".to_owned(),
                StatementKind::Depends,
                Some("acme/acme#3,acme/demo#8".to_owned())
            ),
        ]
    );
    // And: the migrated state file opens, though `ledger/acme` is still a
    // directory: the owner directory `acme/acme` and `acme/demo` live in.
    knives::store::Store::open(home.path().join("state.json"), &[])
        .expect("a migrated state file opens");

    // When: the migration runs again.
    let state_before = std::fs::read(home.path().join("state.json")).expect("read state");
    let seen_before = std::fs::read(home.path().join("seen.json")).expect("read sightings");
    let again = report(&migrate(home.path()));

    // Then: it moved, renamed and wrote nothing, and left both files alone.
    assert_eq!(again["moved"], serde_json::json!([]), "{again}");
    assert_eq!(again["renamed"], serde_json::json!([]), "{again}");
    assert_eq!(again["sightings"], serde_json::json!([]), "{again}");
    assert_eq!(again["wrote"], 0, "{again}");
    assert_eq!(
        std::fs::read(home.path().join("state.json")).expect("read state"),
        state_before
    );
    assert_eq!(
        std::fs::read(home.path().join("seen.json")).expect("read sightings"),
        seen_before
    );
}

#[test]
fn a_migration_moves_each_workspace_sighting_to_the_forks_upstream_name() {
    // Given: a sightings sidecar an older knives left, keying workspaces by
    // each fork's registry key.
    let home = renaming_home();
    std::fs::write(home.path().join("seen.json"), ALIASED_SEEN).expect("write sightings");

    // When: the migration runs.
    let migrated = report(&migrate(home.path()));

    // Then: each workspace sighting an older knives keyed by the registry
    // key is under the upstream name, each reported; where both were there
    // the later stays; one already under its upstream name is untouched, and
    // so are the owner sightings.
    let seen: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join("seen.json")).expect("read sightings"),
    )
    .expect("parse sightings");
    assert_eq!(
        seen["workspaces"],
        serde_json::json!({
            "acme/demo/feat-x": "2026-01-05T00:00:00Z",
            "acme/demo/feat-y": "2026-01-04T00:00:00Z",
            "acme/acme/fix-z": "2026-01-03T00:00:00Z",
        }),
        "{seen}"
    );
    assert_eq!(
        seen["owners"],
        serde_json::json!({"harness-session": {"agent-one": "2026-01-01T00:00:00Z"}}),
        "{seen}"
    );
    assert_eq!(
        migrated["sightings"],
        serde_json::json!([
            {"from": "acme/fix-z", "to": "acme/acme/fix-z"},
            {"from": "demo/feat-x", "to": "acme/demo/feat-x"},
            {"from": "demo/feat-y", "to": "acme/demo/feat-y"},
        ]),
        "{migrated}"
    );
}

#[test]
fn an_entry_already_at_its_new_name_is_not_moved_over() {
    // Given: demo's ledger under its registry key, and its upstream directory
    // already holding one of the same entries (pulled from a machine that
    // migrated first) and a different file under another entry's name.
    let home = renaming_home();
    let kept = note(
        home.path(),
        "demo",
        "2026-01-01T00:00:00Z",
        "the same entry",
    );
    let clash = note(
        home.path(),
        "demo",
        "2026-01-01T00:00:01Z",
        "this machine's",
    );
    let upstream = home.path().join("ledger/acme/demo");
    std::fs::create_dir_all(&upstream).expect("create the upstream directory");
    let name = |path: &Path| path.file_name().expect("a file name").to_owned();
    std::fs::copy(&kept, upstream.join(name(&kept))).expect("copy the shared entry");
    std::fs::write(upstream.join(name(&clash)), "another machine's bytes\n")
        .expect("write a clashing file");

    // When: the migration runs.
    let output = migrate(home.path());

    // Then: the duplicate is dropped, the clash is left on both sides and
    // reported, and the run is incomplete.
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    let migrated: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert!(!kept.exists(), "{migrated}");
    assert!(clash.exists(), "{migrated}");
    assert_eq!(
        std::fs::read_to_string(upstream.join(name(&clash))).expect("read the clash"),
        "another machine's bytes\n"
    );
    let problems = migrated["problems"].as_array().expect("problems");
    assert!(
        problems.iter().any(|problem| problem
            .as_str()
            .is_some_and(|text| text.contains(&name(&clash).to_string_lossy().into_owned()))),
        "{migrated}"
    );
}
