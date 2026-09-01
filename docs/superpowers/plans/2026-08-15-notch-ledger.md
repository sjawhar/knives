# The Notch Ledger Implementation Plan (PR 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A persistent, timestamped, per-repo record of what agents did and decided in a fork, written automatically by the commands that already witness it and by hand through one new command, so the next agent reads why a branch exists instead of rediscovering it by archaeology.

**Architecture:** One append-only JSON-lines file per repository at `~/.config/knives/ledger/<repo>.jsonl`, beside `state.json`. A new `src/ledger.rs` owns the entry type, the file, and the exclusive-create lock — the same `StoreLock` idiom `src/store.rs` already uses, whose `acquire` grows a sibling `acquire_at` because a ledger's lock is `<repo>.jsonl.lock` rather than `<repo>.lock`. A `Scribe` binds "which repo, which checkout, which owner, which ledger" once per command run, so each of the six mutating paths appends with one call. `knives notch` reads and writes it. `knives status` reads it once per repository and carries the newest entry per branch as a ninth table column and a JSON field. Nothing derived is ever stored: entries are past-tense events and judgments anchored to the subject's tip at write time.

**Tech Stack:** Rust edition 2024 (rust-version 1.90), clap 4 derive, serde/serde_json, jiff 0.2 (timestamps; the `serde` feature stays off — `ts` is a string), thiserror 2, jj-lib =0.43.0 pinned. No new dependency.

**Spec:** `docs/superpowers/specs/2026-08-15-notch-ledger-design.md` — read it first, sections "PR 1: the notch ledger" (1.1 to 1.7) and "Out of scope, recorded".

> **Revised 2026-08-16 — storage superseded by Task 11.** The JSONL file, `StoreLock::acquire_at` and the `<repo>.jsonl.lock` lockfile described in the **Architecture** paragraph above were built as planned (Tasks 1–8), then replaced mid-implementation by decision with Sami: one markdown file per entry with TOML frontmatter, no lock at all. Spec §1.2 "Storage (revised 2026-08-16)" is the authority; **Task 11** is the swap. Run Task 11 before Tasks 9 and 10.

## Global Constraints

Values copied from the spec. Every task's requirements implicitly include this section.

_Revised 2026-08-16 (spec §1.2 "Storage (revised 2026-08-16)", Task 11): the storage bullets below — **Approach A, settled**, **Storage path**, **No rotation** (the ~200-byte figure; an entry is now ~300 bytes), and **Locking** — are superseded. The storage is one immutable markdown file per entry under `~/.config/knives/ledger/<repo>/`, TOML frontmatter between `+++` fences, one atomic `create_new` per write, no lockfile, lexicographic filename order as the chronology. The **Exit codes** bullet keeps its rule and shifts its noun: 3 is still "the ledger exists but cannot be read" — the ledger being the directory and its entry files. Task 11 is the swap; the bullets stay as the record of what Tasks 1–8 were built against._

- **Approach A, settled:** "append-only JSONL ledger per repo, beside `state.json`". Markdown-per-subject and a growing `state.json` were considered and rejected.
- **Storage path:** `~/.config/knives/ledger/<repo>.jsonl`. Lockfile `<repo>.jsonl.lock`. "Append-only: no entry is ever rewritten or deleted." "Within a file, file order is authoritative even if clocks skew across writers."
- **No rotation, no retention policy:** "entries are ~200 bytes and growth is irrelevant on any horizon that matters here."
- **Locking:** "Appends hold an exclusive-create lockfile (`<repo>.jsonl.lock`), the same idiom as `StoreLock`, so concurrent agents cannot interleave partial lines."
- **`kind` is two values, not three:** `event` (a machine observed a knives command) or `note` (an agent asserted something). "Asking writing agents to self-classify judgment-versus-note is a decision burden with no read-time payoff."
- **`anchor` is never caller-supplied:** "the subject's tip commit at write time; omitted when unresolvable (branch since deleted) — the entry stays valid." It is the anti-rot mechanism.
- **`pr` is "stamped from `tracked_pulls` only; never a forge call on the write path".**
- **`owner` uses `current_owner()` — "same resolution as claims (`KNIVES_OWNER` → Claude session ID → active-owner lookup → OS user)".**
- **`ts` is "UTC timestamp, RFC 3339".**
- **Schema evolution:** "entries are never rewritten, readers ignore unknown fields, writers may add fields. No version number."
- **Never stores derived state:** "The ledger therefore stores events and judgments with anchors, and never stores derived state — consistent with the existing store doctrine ('compute anything cheap; what lives here is intent')."
- **Failure is loud:** "A ledger append failure fails the command loudly. No silent half-write."
- **Releases are first-class subjects:** "A release ref name is a subject like any branch name."
- **`--repo <name>` on both moods**, "matching the existing convention ('takes its repo from where you stand, name one when you are somewhere else')".
- **Output:** "JSON by default for agents, prose for humans, exactly as every other command (`--json` / `--text`)."
- **Exit codes:** "0 fine, 2 usage, 3 when the ledger file exists but cannot be read."
- **Status breadcrumb:** "JSON: `notch: {ts, kind, text}` (absent when the subject has none). Text: one truncated token at the end of the branch line, e.g. `"superseded by #1157…" (3d)`. One token, nothing else."
- **No hook injection.** "Reading the ledger is intentional. The OpenCode plugin does not change." Nothing under `plugin/`, `omp/` or `hooks/` is touched by this PR.
- **Out of scope, and absent from every task below:** unowned-release-content detection at cut time; pin-vs-tip equality per fork; release ref integrity; status text legibility; ledger backup/sync; hook injection of ledger content; per-PR promise-thread tracking against the forge.

Repository constraints:

- **jj, not git.** All version control through `jj`. Never run a git mutation command in this repo.
- **One commit per PR. There are no commit steps in this plan.** Work accumulates in `@`; the coordinator describes it once and owns every push. A task ends when its tests pass.
- **Gates, from the repo root:** `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`. Treat any clippy warning as a failure: `[lints.clippy]` in `Cargo.toml` denies `all`, warns `pedantic` and `nursery`, and denies `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `as_underscore`.
- **`clippy.toml` thresholds bind the design:** `too-many-arguments-threshold = 4` (a 5-argument function needs an `#[allow(..., reason = "...")]`), `too-many-lines-threshold = 100`, `cognitive-complexity-threshold = 25`. `allow-unwrap-in-tests`, `allow-expect-in-tests`, `allow-panic-in-tests` and `allow-print-in-tests` are all true, so `.unwrap()`/`.expect()` are fine inside `#[cfg(test)]` and nowhere else. `clippy::indexing_slicing` is NOT exempt in tests: test modules carry `#![allow(clippy::indexing_slicing, reason = "indexing a result in a test is the assertion; a panic is the failure")]`.
- **Identity guard:** `tests/no_hardcoded_identity.rs` scans `src/`, `plugin/`, `docs/`, `skills/`, `hooks/` for a forge host written with a trailing slash and for project-family literals. This plan file lives under `docs/`, so those literals stay out of it too. Test URLs use `forge.invalid` or `example.test`.
- **House style:** doc comments state current behavior and the failure that motivated it, never history. Test names are sentences. Given/When/Then comments in tests. Rendering stays pure — every command builds a `String` and one call site prints it.
- **`// allow: SIZE_OK: <n> lines - <reason>` markers** sit at the top of `src/main.rs` (2050) and `src/commands/claim.rs` (292). Both files grow in this PR; update the counts in those two lines to the new `wc -l` values.

## Hardening ledger

_(empty at the start; the coordinator records hardening findings here as they are resolved)_

---

## File structure

| File | Responsibility |
|---|---|
| `src/ledger.rs` (new) | `Entry`, `Kind`, `LedgerError`, `Ledger` (path, append, read), `Filter`/`select`/`newest_for`/`age`, `Scribe`/`Draft`. The only module that knows the ledger's shape or its file. |
| `src/store.rs` (modify) | `StoreLock::acquire_at`, so the ledger locks by exact path. |
| `src/lib.rs` (modify) | `pub mod ledger;` plus its line in the crate-level module map. |
| `src/commands/notch.rs` (new) | The `notch` command: `Request`, `Report`, `read`, `render`, `run`. |
| `src/commands.rs` (modify) | `pub mod notch;` |
| `src/cli.rs` (modify) | `Command::Notch` and its parser tests. |
| `src/main.rs` (modify) | Dispatch for `Notch`; `scribe_for`; auto-events in `run_finish`, `run_track`, `run_depends`, the release-cut path, and `run_sync`'s scribe. |
| `src/commands/claim.rs` (modify) | Auto-events in `run_claim` and `run_release`. |
| `src/commands/start.rs` (modify) | Auto-event in `run`. |
| `src/commands/sync.rs` (modify) | `sync_repo` takes a `&Scribe` and records each transition. |
| `src/commands/status.rs` (modify) | `LastNotch`, `BranchRow::notch`, `Options::ledger`, `notches_from_ledger`, `notch_cell`, `add_releases`, `gather`, `DivergentInput`/`divergent_rows`, `branch_table`. |
| `tests/notch_command.rs` (new) | The command through the real binary: both output modes, `--repo` from outside, exit codes. |
| `tests/jj_integration.rs` (modify) | Auto-events fire with the right subject, owner and anchor; the status breadcrumb end to end. |
| `skills/fork-work/SKILL.md`, `skills/using-knives/SKILL.md`, `skills/pr-preflight/SKILL.md`, `README.md`, `docs/design.md` (modify) | Agents are told to read and write notches, and the past-tense-only doctrine is written down. |

### Functions this PR touches in `src/commands/status.rs`

PR 2 (`docs/superpowers/plans/2026-08-15-status-speed.md`) also changes this file. **This PR merges first.** The complete list of what this PR touches there, so PR 2's rebase is mechanical:

- `struct BranchRow` — one field added (`notch`).
- `BranchRow::bare` — one field initialised.
- `struct Options` — one field added (`ledger`).
- `struct LastNotch` — new.
- `fn notches_from_ledger` — new.
- `fn notch_cell` — new; `const NOTCH_TEXT` — new.
- `fn add_releases` — new (extracted from `gather`, which is within a handful of lines of `too-many-lines-threshold = 100`).
- `fn gather` — the release-scan block collapses into `add_releases`; one `notches` read is added before the branch loop; one field is added to the `BranchRow` literal and one to the `DivergentInput` literal.
- `struct DivergentInput` and `fn divergent_rows` — one field added, one field set.
- `fn branch_table` — eight columns become nine.
- `mod tests` — `branch_rows_render_as_an_aligned_table_with_a_header` and `an_empty_cell_never_shifts_its_neighbours` learn the ninth column; new breadcrumb tests.

The overlap with PR 2 is `fn gather` and `struct Options`. PR 2 adds a different field to `Options` and moves work out of `gather`'s loop while keeping the `notch` line this PR puts in the `BranchRow` literal.

### Task dependencies

```
T1 ledger core ──┬─> T2 read/filter ──┬─> T4 notch command ──> (T10 docs)
                 │                    └─> T9 status breadcrumb
                 └─> T3 Scribe ───────┬─> T5 claim/start/finish
                                      ├─> T6 track/depends
                                      ├─> T7 release cut
                                      └─> T8 sync
```

- **T1** blocks everything.
- **T2** and **T3** are independent of each other and can run in parallel.
- **T4**, **T9** need T2 (T4 also needs T3 for its write mood).
- **T5**, **T6**, **T7**, **T8** need T3 only and are independent of each other — four parallel lanes.
- **T10** is documentation of the surface T4 and T9 define; it can be written in parallel from this plan, since the surface is fully specified here.

- **T11** (added 2026-08-16) needs T1–T8 — all implemented — and **must land before T9 and T10 execute**, so the breadcrumb and the docs are built on the markdown storage rather than on the JSONL it retires. T11 overlaps T9 on `tests/jj_integration.rs` and T10 on the storage story; do not run either in parallel with it.

---

### Task 1: The ledger file — entry, append, read, lock

_**Superseded storage (2026-08-16).** This task was implemented as written, and Task 11 then swapped the layer underneath it: the JSONL file became one markdown file per entry with TOML frontmatter, the `<repo>.jsonl.lock` lockfile and `StoreLock::acquire_at` were retired, and `LedgerError` traded `Lock` for `Collision` and per-line positions for per-file paths. The body below is the historical record of what was built; Task 11 is the storage layer as it now stands._

**Files:**
- Create: `src/ledger.rs`
- Modify: `src/store.rs` (`impl StoreLock`, around line 130)
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `crate::config::default_config_path()`, `crate::ids::RepoName`, `crate::store::{StoreError, StoreLock}`.
- Produces:
  - `pub fn default_ledger_path(repo: &RepoName) -> PathBuf`
  - `pub enum Kind { Event, Note }` — `Copy`, `Serialize`, `Deserialize` as lowercase, `Display`.
  - `pub struct Entry { pub ts: String, pub owner: String, pub subject: Option<String>, pub kind: Kind, pub text: String, pub evidence: Vec<String>, pub anchor: Option<String>, pub pr: Option<u64> }`
  - `pub enum LedgerError { Read, Write, Parse, Timestamp, Lock, Serialise }`
  - `pub struct Ledger` with `pub fn for_repo(repo: &RepoName) -> Self`, `pub const fn at(path: PathBuf) -> Self`, `pub fn path(&self) -> &Path`, `pub fn append(&self, entry: &Entry) -> Result<(), LedgerError>`, `pub fn entries(&self) -> Result<Vec<Entry>, LedgerError>`
  - `pub(crate) fn StoreLock::acquire_at(path: &Path) -> Result<StoreLock, StoreError>`

- [ ] **Step 1: Write the failing unit tests** at the bottom of the new `src/ledger.rs` (write the file with only these tests plus `use super::*;` first if you prefer a red run; the module itself is Step 3).

```rust
#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn entry(subject: Option<&str>, text: &str) -> Entry {
        Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "session-owner".to_owned(),
            subject: subject.map(str::to_owned),
            kind: Kind::Note,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: Some("6c42fe71".to_owned()),
            pr: None,
        }
    }

    #[test]
    fn an_entry_round_trips_through_the_file_in_write_order() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo.jsonl"));

        ledger.append(&entry(Some("feat/alpha"), "first")).unwrap();
        ledger.append(&entry(Some("feat/beta"), "second")).unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].text, "first");
        assert_eq!(read[1].text, "second");
        assert_eq!(read[0].subject.as_deref(), Some("feat/alpha"));
        assert_eq!(read[0].kind, Kind::Note);
        assert_eq!(read[0].anchor.as_deref(), Some("6c42fe71"));
    }

    #[test]
    fn a_ledger_that_does_not_exist_yet_is_empty_rather_than_an_error() {
        // A repository nobody has notched is the normal case, not a failure.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("never-written.jsonl"));
        assert!(ledger.entries().unwrap().is_empty());
    }

    #[test]
    fn an_absent_subject_pr_and_anchor_survive_as_absent() {
        // A repo-level entry has no subject; an entry about a deleted branch has no
        // anchor. Neither may come back as an empty string, which would read as a
        // branch named "".
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo.jsonl"));
        let bare = Entry {
            anchor: None,
            ..entry(None, "the fork needs a release cut before Friday")
        };
        ledger.append(&bare).unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read[0].subject, None);
        assert_eq!(read[0].anchor, None);
        assert_eq!(read[0].pr, None);
        // And: absent fields are omitted from the line rather than written as null,
        // so an entry stays the ~200 bytes the design budgeted.
        let text = std::fs::read_to_string(ledger.path()).unwrap();
        assert!(!text.contains("null"), "was: {text}");
        assert!(!text.contains("subject"), "was: {text}");
    }

    #[test]
    fn a_field_this_version_does_not_know_is_ignored_rather_than_rejected() {
        // Entries are never rewritten, so a newer binary may add a field and an
        // older one must still read the line. That is the whole evolution story.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo.jsonl");
        std::fs::write(
            &path,
            "{\"ts\":\"2026-08-15T22:14:03Z\",\"owner\":\"x\",\"kind\":\"event\",\
             \"text\":\"claimed\",\"from_the_future\":{\"k\":\"v\"}}\n",
        )
        .unwrap();

        let read = Ledger::at(path).entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].kind, Kind::Event);
        assert_eq!(read[0].text, "claimed");
    }

    #[test]
    fn a_newline_in_an_entrys_text_stays_one_line() {
        // One entry is one line. A pasted multi-line reason must not become two
        // records, one of which does not parse.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo.jsonl"));
        ledger
            .append(&entry(Some("feat/alpha"), "parked\nby the owner"))
            .unwrap();

        let text = std::fs::read_to_string(ledger.path()).unwrap();
        assert_eq!(text.lines().count(), 1, "was: {text}");
        assert_eq!(
            ledger.entries().unwrap()[0].text,
            "parked\nby the owner"
        );
    }

    #[test]
    fn a_line_that_is_not_an_entry_is_reported_with_its_number() {
        // A ledger the tool cannot read must not read as a ledger with nothing in
        // it: that is the silent-empty failure this whole record exists to prevent.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo.jsonl");
        let good = "{\"ts\":\"2026-08-15T22:14:03Z\",\"owner\":\"x\",\"kind\":\"note\",\"text\":\"a\"}";
        std::fs::write(&path, format!("{good}\nnot json at all\n")).unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Parse { line: 2, .. }),
            "was: {error}"
        );
    }

    #[test]
    fn an_unreadable_timestamp_is_reported_rather_than_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo.jsonl");
        std::fs::write(
            &path,
            "{\"ts\":\"last tuesday\",\"owner\":\"x\",\"kind\":\"note\",\"text\":\"a\"}\n",
        )
        .unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Timestamp { line: 1, .. }),
            "was: {error}"
        );
    }

    #[test]
    fn a_second_writer_cannot_append_while_the_first_holds_the_lock() {
        // Two agents appending at once must not interleave. The store's own lock
        // proved this class of bug real; the ledger takes the same guard, named for
        // the file it guards rather than for that file's stem.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo.jsonl");
        let ledger = Ledger::at(path.clone());
        ledger.append(&entry(Some("feat/alpha"), "first")).unwrap();

        let held = crate::store::StoreLock::acquire_at(&path.with_file_name("a-repo.jsonl.lock"))
            .expect("take the ledger lock");
        let blocked = ledger.append(&entry(Some("feat/alpha"), "second"));
        assert!(
            matches!(blocked, Err(LedgerError::Lock { .. })),
            "a second writer got in"
        );

        drop(held);
        ledger.append(&entry(Some("feat/alpha"), "second")).unwrap();
        assert_eq!(ledger.entries().unwrap().len(), 2);
    }

    #[test]
    fn two_writers_appending_at_once_lose_no_line_and_interleave_none() {
        // The test above proves the guard refuses; this proves the guard is
        // enough. Two agents notching the same branch at the same moment is the
        // ordinary case on a machine running several of them, and a lost or
        // half-written line is a hole in the one record meant to be trusted
        // later. `entries` rejects any line that does not parse, so its
        // succeeding IS the no-interleaving assertion.
        const EACH: usize = 40;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo.jsonl");

        std::thread::scope(|scope| {
            for writer in 0..2 {
                let path = path.clone();
                let _ = scope.spawn(move || {
                    let ledger = Ledger::at(path);
                    for index in 0..EACH {
                        // Retried here because the guard's contract is to refuse
                        // rather than to queue: a caller that wants the write
                        // waits for it.
                        let mut written = false;
                        for _ in 0..500 {
                            match ledger
                                .append(&entry(Some("feat/alpha"), &format!("{writer}:{index}")))
                            {
                                Ok(()) => {
                                    written = true;
                                    break;
                                }
                                Err(LedgerError::Lock { .. }) => {
                                    std::thread::sleep(std::time::Duration::from_millis(1));
                                }
                                Err(other) => panic!("append failed: {other}"),
                            }
                        }
                        assert!(written, "writer {writer} never got the lock");
                    }
                });
            }
        });

        let entries = Ledger::at(path).entries().unwrap();
        assert_eq!(entries.len(), EACH * 2, "lines were lost");
        for writer in 0..2 {
            for index in 0..EACH {
                let wanted = format!("{writer}:{index}");
                assert!(
                    entries.iter().any(|entry| entry.text == wanted),
                    "missing: {wanted}"
                );
            }
        }
    }

    #[test]
    fn a_repos_ledger_sits_beside_the_state_file_in_its_own_directory() {
        let _lock = crate::config::test_support::environment_lock();
        let environment =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        environment.set("KNIVES_CONFIG_HOME", "/tmp/knives-home");
        assert_eq!(
            default_ledger_path(&RepoName::new("a-repo")),
            std::path::PathBuf::from("/tmp/knives-home/ledger/a-repo.jsonl")
        );
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib ledger::`
Expected: FAIL — the compiler cannot find the `ledger` module (`error[E0433]` / `failed to resolve`), because `src/lib.rs` does not declare it yet.

- [ ] **Step 3: Add `StoreLock::acquire_at`** in `src/store.rs`. Replace the existing `impl StoreLock { ... }` block (currently `pub(crate) fn acquire` alone) with:

```rust
impl StoreLock {
    /// Beside the file it guards, named for that file's stem: `state.json` is
    /// guarded by `state.lock`.
    pub(crate) fn acquire(target: &Path) -> Result<Self, StoreError> {
        Self::acquire_at(&target.with_extension("lock"))
    }

    /// At an exact path, for a guarded file whose lock is not named after its
    /// stem. The ledger's is `<repo>.jsonl.lock`, which `with_extension` would
    /// have named `<repo>.lock` — a different file from the one it says it holds.
    pub(crate) fn acquire_at(path: &Path) -> Result<Self, StoreError> {
        let path = path.to_owned();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: parent.to_owned(),
                source,
            })?;
        }
        // A short wait, then give up loudly. Blocking forever on a stale lock
        // would be worse than saying so.
        for _ in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(source) => return Err(StoreError::Write { path, source }),
            }
        }
        Err(StoreError::Locked { path })
    }
}
```

- [ ] **Step 4: Write `src/ledger.rs`** — everything above the `#[cfg(test)] mod tests` block from Step 1:

```rust
//! What agents did and decided here, in order, forever.
//!
//! [`crate::store`] holds current intent and is rewritten whole on every change:
//! `knives finish` deletes the claim that said why a branch exists, and nothing
//! remembers it afterwards. Agents then rediscover a mysterious branch by
//! archaeology, or draw a conclusion from a stale one.
//!
//! One append-only JSON-lines file per repository, beside `state.json`. An entry
//! is an event (this tool observed one of its own commands) or a note (an agent
//! asserted something), anchored to the subject's tip at write time. That anchor
//! is why the record does not rot: a reader who sees the tip has moved since
//! knows to re-verify rather than inherit the conclusion. Nothing derived is
//! stored — a recorded past-tense judgment stays true, while a cached
//! disposition goes wrong the moment upstream moves.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::default_config_path;
use crate::ids::RepoName;
use crate::store::{StoreError, StoreLock};

/// Where a repository's ledger lives: its own directory beside `state.json`, one
/// file per repo, so a fork's history is one file to read, copy or keep.
pub fn default_ledger_path(repo: &RepoName) -> PathBuf {
    default_config_path()
        .with_file_name("ledger")
        .join(format!("{repo}.jsonl"))
}

/// Who put an entry there.
///
/// Two values, not three. The question a reader asks is whether a machine
/// observed this or an agent asserted it; a supersession or a parking arrives as
/// an event through `finish --superseded-by` and `start --why`, and everything an
/// agent asserts is a note. Asking a writing agent to grade its own entry as
/// judgment-versus-note is a decision with no read-time payoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Event,
    Note,
}

impl std::fmt::Display for Kind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Event => "event",
            Self::Note => "note",
        })
    }
}

/// One line of the ledger.
///
/// Unknown fields are ignored rather than rejected: entries are never rewritten,
/// so a newer binary may add a field and an older one must still read the line.
/// That is the whole schema-evolution story, and it is why there is no version
/// number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// When it was written, RFC 3339 UTC.
    pub ts: String,
    /// Resolved exactly as a claim's owner is.
    pub owner: String,
    /// The ref this is about — a branch or a release name. Absent for an entry
    /// about the repository itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub kind: Kind,
    pub text: String,
    /// Free strings backing the entry: commit ids, `file:line`, `<repo>#<number>`,
    /// URLs, and they may name other repositories. Every audit claim that
    /// survived red-teaming cited one; every false finding lacked one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<String>,
    /// The subject's tip when this was written, absent when it did not resolve.
    ///
    /// Never caller-supplied. A branch deleted since leaves the entry valid with
    /// no anchor; a tip that has moved tells the reader to re-verify.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    /// The pull request stated for the subject, from `tracked_pulls` only. Never
    /// a forge call: this is a write path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("appending to {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} line {line} is not a ledger entry: {source}")]
    Parse {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },
    #[error("{path} line {line} has an unreadable timestamp `{ts}`")]
    Timestamp {
        path: PathBuf,
        line: usize,
        ts: String,
    },
    #[error("could not lock the ledger: {source}")]
    Lock {
        #[from]
        source: StoreError,
    },
    #[error("serialising a ledger entry: {source}")]
    Serialise {
        #[from]
        source: serde_json::Error,
    },
}

/// One repository's ledger file.
#[derive(Debug, Clone)]
pub struct Ledger {
    path: PathBuf,
}

impl Ledger {
    /// A repository's ledger at the default location.
    pub fn for_repo(repo: &RepoName) -> Self {
        Self::at(default_ledger_path(repo))
    }

    /// At an exact path, for a test or for a caller with its own config home.
    pub const fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one entry, holding the lock for the write.
    ///
    /// The record and its newline go out as one buffer: the lock is what stops
    /// two agents interleaving, and the single write is what keeps a reader —
    /// which never takes the lock, because reading must not block a report —
    /// from seeing half a record. `serde_json` escapes control characters, so one
    /// entry is exactly one line however many newlines its text contains.
    pub fn append(&self, entry: &Entry) -> Result<(), LedgerError> {
        let mut line = serde_json::to_string(entry)?;
        line.push('\n');
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| LedgerError::Write {
                path: parent.to_owned(),
                source,
            })?;
        }
        let _lock = StoreLock::acquire_at(&lock_path(&self.path))?;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|source| LedgerError::Write {
                path: self.path.clone(),
                source,
            })?;
        file.write_all(line.as_bytes())
            .map_err(|source| LedgerError::Write {
                path: self.path.clone(),
                source,
            })
    }

    /// Every entry, in file order.
    ///
    /// File order is authoritative even when clocks skew across writers. A ledger
    /// that does not exist yet is empty rather than an error: a repository nobody
    /// has notched is the normal case. A line that does not parse IS an error,
    /// because a ledger the tool cannot read must not read as a ledger with
    /// nothing in it.
    pub fn entries(&self) -> Result<Vec<Entry>, LedgerError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = std::fs::read_to_string(&self.path).map_err(|source| LedgerError::Read {
            path: self.path.clone(),
            source,
        })?;
        text.lines()
            .enumerate()
            .filter(|(_, line)| !line.trim().is_empty())
            .map(|(index, line)| self.parse_line(index + 1, line))
            .collect()
    }

    fn parse_line(&self, line: usize, text: &str) -> Result<Entry, LedgerError> {
        let entry: Entry = serde_json::from_str(text).map_err(|source| LedgerError::Parse {
            path: self.path.clone(),
            line,
            source,
        })?;
        // Checked here rather than at every reader: a timestamp nothing can order
        // is a corrupt record, and one loud error beats a breadcrumb with no age.
        if entry.ts.parse::<jiff::Timestamp>().is_err() {
            return Err(LedgerError::Timestamp {
                path: self.path.clone(),
                line,
                ts: entry.ts,
            });
        }
        Ok(entry)
    }
}

/// `<repo>.jsonl.lock`, beside the file it guards.
fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".lock");
    PathBuf::from(name)
}
```

- [ ] **Step 5: Declare the module** in `src/lib.rs`. Add to the crate doc comment's module map, after the `[jj]` bullet:

```rust
//! - [`ledger`] is the only module that knows what a notch is: an append-only
//!   record per repository of what happened and what was decided, which is the
//!   half of state that [`store`] deletes when intent changes.
```

and add `pub mod ledger;` to the module list, between `pub mod jj;` and `pub mod pins;`.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib ledger::`
Expected: PASS, 10 tests.

- [ ] **Step 7: Confirm the store's own lock tests still pass and the gates are clean**

Run: `cargo test --lib store:: && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS; no clippy output.

---

### Task 2: Reading a ledger — filter, newest, age

_**Superseded storage (2026-08-16).** Task 11 swapped the storage underneath this task. `Filter`, `select`, `newest_for` and `age` survive byte-for-byte — they operate on `Vec<Entry>` and never knew the file shape — but where this task's prose says "file order", read "lexicographic entry-filename order, which is stamp order". Historical record; see Task 11._

**Files:**
- Modify: `src/ledger.rs`

**Interfaces:**
- Consumes: `Entry` (Task 1).
- Produces:
  - `pub struct Filter<'a> { pub subject: Option<&'a str>, pub pr: Option<u64>, pub limit: Option<usize> }` — `Default`, `Copy`.
  - `pub fn select<'a>(entries: &'a [Entry], filter: &Filter<'_>) -> (Vec<&'a Entry>, usize)` — matches oldest first, plus how many matched before the limit.
  - `pub fn newest_for<'a>(entries: &'a [Entry], subject: &str) -> Option<&'a Entry>`
  - `pub fn age(ts: &str, now: jiff::Timestamp) -> Option<String>` — `now`, `12m`, `4h`, `3d`.

- [ ] **Step 1: Write the failing tests** — append to `mod tests` in `src/ledger.rs`:

```rust
    fn stamped(subject: Option<&str>, pr: Option<u64>, text: &str) -> Entry {
        Entry {
            pr,
            ..entry(subject, text)
        }
    }

    #[test]
    fn a_subject_filter_keeps_only_that_refs_chronology() {
        let entries = vec![
            stamped(Some("feat/alpha"), None, "one"),
            stamped(Some("feat/beta"), None, "two"),
            stamped(Some("feat/alpha"), None, "three"),
            stamped(None, None, "repo-level"),
        ];
        let (selected, matched) = select(
            &entries,
            &Filter {
                subject: Some("feat/alpha"),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 2);
        let texts: Vec<&str> = selected.iter().map(|e| e.text.as_str()).collect();
        assert_eq!(texts, ["one", "three"], "oldest first, nothing else");
    }

    #[test]
    fn a_release_ref_is_a_subject_like_any_branch() {
        // Releases are first-class subjects: the audit of what a cut contained is
        // filed under the cut's own name.
        let entries = vec![stamped(Some("release/2026-08-15"), None, "cut with 3 parents")];
        let (selected, _) = select(
            &entries,
            &Filter {
                subject: Some("release/2026-08-15"),
                ..Filter::default()
            },
        );
        assert_eq!(selected.len(), 1);
    }

    #[test]
    fn a_pull_request_filter_reads_the_stamped_field_only() {
        let entries = vec![
            stamped(Some("feat/alpha"), Some(1157), "one"),
            stamped(Some("feat/alpha"), None, "mentions #1157 in its text only"),
        ];
        let (selected, matched) = select(
            &entries,
            &Filter {
                pr: Some(1157),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 1);
        assert_eq!(selected[0].text, "one");
    }

    #[test]
    fn a_limit_keeps_the_newest_and_reports_how_many_it_did_not_show() {
        // A window that silently drops the older half is how a reader concludes a
        // branch has no history.
        let entries: Vec<Entry> = (0..25)
            .map(|index| stamped(Some("feat/alpha"), None, &format!("entry {index}")))
            .collect();
        let (selected, matched) = select(
            &entries,
            &Filter {
                limit: Some(20),
                ..Filter::default()
            },
        );
        assert_eq!(matched, 25);
        assert_eq!(selected.len(), 20);
        assert_eq!(selected[0].text, "entry 5");
        assert_eq!(selected[19].text, "entry 24");
    }

    #[test]
    fn the_newest_entry_for_a_subject_is_the_last_one_in_the_file() {
        // File order, not timestamp order: two agents' clocks may disagree and the
        // file is the only thing that cannot.
        let entries = vec![
            Entry {
                ts: "2026-08-15T23:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "written first, clock ahead")
            },
            Entry {
                ts: "2026-08-15T22:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "written second, clock behind")
            },
            stamped(Some("feat/beta"), None, "another branch"),
        ];
        assert_eq!(
            newest_for(&entries, "feat/alpha").map(|e| e.text.as_str()),
            Some("written second, clock behind")
        );
        assert_eq!(newest_for(&entries, "feat/never-notched"), None);
    }

    #[test]
    fn an_age_is_the_shortest_form_that_is_still_true() {
        let now: jiff::Timestamp = "2026-08-15T12:00:00Z".parse().unwrap();
        assert_eq!(age("2026-08-15T11:59:31Z", now).as_deref(), Some("now"));
        assert_eq!(age("2026-08-15T11:48:00Z", now).as_deref(), Some("12m"));
        assert_eq!(age("2026-08-15T08:00:00Z", now).as_deref(), Some("4h"));
        assert_eq!(age("2026-08-12T12:00:00Z", now).as_deref(), Some("3d"));
        // A clock that ran backwards is not a negative age.
        assert_eq!(age("2026-08-15T12:00:30Z", now).as_deref(), Some("now"));
        // Only reachable for an entry assembled by hand: `entries` rejects these.
        assert_eq!(age("last tuesday", now), None);
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib ledger::`
Expected: FAIL — `cannot find function 'select' in this scope`, `cannot find struct 'Filter'`.

- [ ] **Step 3: Implement** — add to `src/ledger.rs` after `impl Ledger`, before `fn lock_path`:

```rust
/// Which entries a read wants.
#[derive(Debug, Default, Clone, Copy)]
pub struct Filter<'a> {
    /// Only entries about this ref.
    pub subject: Option<&'a str>,
    /// Only entries stamped with this pull request.
    pub pr: Option<u64>,
    /// Keep at most this many, the newest of them. `None` keeps everything.
    pub limit: Option<usize>,
}

/// Entries matching `filter`, oldest first, and how many matched before the limit.
///
/// The count travels with the result so a truncated read can say so: a window
/// that silently drops the older half of a branch's history is how a reader
/// concludes the history is short.
pub fn select<'a>(entries: &'a [Entry], filter: &Filter<'_>) -> (Vec<&'a Entry>, usize) {
    let matched: Vec<&Entry> = entries
        .iter()
        .filter(|entry| {
            filter
                .subject
                .is_none_or(|wanted| entry.subject.as_deref() == Some(wanted))
        })
        .filter(|entry| filter.pr.is_none_or(|wanted| entry.pr == Some(wanted)))
        .collect();
    let matched_count = matched.len();
    let skipped = filter
        .limit
        .map_or(0, |limit| matched_count.saturating_sub(limit));
    (matched.into_iter().skip(skipped).collect(), matched_count)
}

/// The newest entry about `subject`, by file order.
///
/// File order rather than timestamp order, because two agents' clocks can
/// disagree and the file cannot.
pub fn newest_for<'a>(entries: &'a [Entry], subject: &str) -> Option<&'a Entry> {
    entries
        .iter()
        .rev()
        .find(|entry| entry.subject.as_deref() == Some(subject))
}

/// How long ago, in the shortest form that is still true: `now`, `12m`, `4h`, `3d`.
///
/// `None` when the timestamp does not parse. [`Ledger::entries`] rejects those at
/// the boundary, so this answers `None` only for an entry assembled by hand.
pub fn age(ts: &str, now: jiff::Timestamp) -> Option<String> {
    let then = ts.parse::<jiff::Timestamp>().ok()?;
    let seconds = now.as_second().saturating_sub(then.as_second()).max(0);
    Some(if seconds < 60 {
        "now".to_owned()
    } else if seconds < 3600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3600)
    } else {
        format!("{}d", seconds / 86_400)
    })
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ledger::`
Expected: PASS, 16 tests.

- [ ] **Step 5: Gates**

Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: no output.

---

### Task 3: `Scribe` — the writer the mutating commands use

**Files:**
- Modify: `src/ledger.rs`

**Interfaces:**
- Consumes: `Entry`, `Kind`, `Ledger` (Task 1); `crate::jj::Repo::open`/`resolve_commit`.
- Produces:
  - `pub struct Draft<'a> { pub subject: Option<&'a str>, pub kind: Kind, pub text: String, pub evidence: Vec<String>, pub pr: Option<u64> }`
  - `pub struct Scribe` with `pub const fn new(ledger: Ledger, repo: RepoName, path: PathBuf, owner: String) -> Self`, `pub fn repo(&self) -> &RepoName`, `pub fn record(&self, draft: &Draft<'_>) -> Result<Entry, LedgerError>`, `pub fn event(&self, subject: Option<&str>, text: String, pr: Option<u64>) -> Result<Entry, LedgerError>`

Owner resolution stays at the call sites (`crate::commands::claim::current_owner`), so this module never reaches into `commands` and every call site shows that a notch's owner is a claim's owner.

- [ ] **Step 1: Write the failing tests** — append to `mod tests` in `src/ledger.rs`:

```rust
    fn scribe(dir: &std::path::Path) -> Scribe {
        Scribe::new(
            Ledger::at(dir.join("ledger").join("a-repo.jsonl")),
            RepoName::new("a-repo"),
            dir.join("not-a-repository"),
            "session-owner".to_owned(),
        )
    }

    #[test]
    fn an_event_stamps_the_fields_no_caller_supplies() {
        let dir = tempfile::tempdir().unwrap();
        let scribe = scribe(dir.path());

        let written = scribe
            .event(Some("feat/alpha"), "claimed: fixing the parser".to_owned(), Some(4545))
            .unwrap();

        assert_eq!(written.kind, Kind::Event);
        assert_eq!(written.owner, "session-owner");
        assert_eq!(written.subject.as_deref(), Some("feat/alpha"));
        assert_eq!(written.pr, Some(4545));
        assert!(
            written.ts.parse::<jiff::Timestamp>().is_ok(),
            "was: {}",
            written.ts
        );
        // And: it is on disk, not just returned.
        assert_eq!(scribe.ledger.entries().unwrap(), vec![written]);
    }

    #[test]
    fn an_anchor_is_omitted_when_the_subject_does_not_resolve() {
        // A branch deleted since, a reaped release ref, or a path that is not a
        // repository. None of them invalidates the entry, so none of them may
        // fail the write.
        let dir = tempfile::tempdir().unwrap();
        let written = scribe(dir.path())
            .event(Some("feat/long-gone"), "claim released".to_owned(), None)
            .unwrap();
        assert_eq!(written.anchor, None);
    }

    #[test]
    fn a_note_carries_its_evidence_and_a_repo_level_entry_has_no_subject() {
        let dir = tempfile::tempdir().unwrap();
        let written = scribe(dir.path())
            .record(&Draft {
                subject: None,
                kind: Kind::Note,
                text: "the release remote is out of date".to_owned(),
                evidence: vec!["06d778b9".to_owned(), "a-repo#1157".to_owned()],
                pr: None,
            })
            .unwrap();

        assert_eq!(written.kind, Kind::Note);
        assert_eq!(written.subject, None);
        assert_eq!(written.evidence, ["06d778b9", "a-repo#1157"]);
    }

    #[test]
    fn an_append_that_cannot_be_written_is_an_error_rather_than_a_shrug() {
        // A ledger append failure fails its command loudly: both files live in one
        // directory, and a write that can fail one can fail the other.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger").join("a-repo.jsonl");
        std::fs::create_dir_all(&path).expect("a directory where the file should be");
        let blocked = Scribe::new(
            Ledger::at(path),
            RepoName::new("a-repo"),
            dir.path().to_owned(),
            "session-owner".to_owned(),
        )
        .event(Some("feat/alpha"), "claimed".to_owned(), None);
        assert!(matches!(blocked, Err(LedgerError::Write { .. })), "was: {blocked:?}");
    }
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib ledger::`
Expected: FAIL — `cannot find struct 'Scribe'`, `cannot find struct 'Draft'`.

- [ ] **Step 3: Implement** — add to `src/ledger.rs` after `pub fn age`:

```rust
/// An entry before its automatic fields are stamped.
#[derive(Debug)]
pub struct Draft<'a> {
    /// The ref this is about, or nothing for an entry about the repository.
    pub subject: Option<&'a str>,
    pub kind: Kind,
    pub text: String,
    pub evidence: Vec<String>,
    /// The pull request stated for the subject, read from the store by the
    /// caller. Never a forge call: a round trip here would make every claim,
    /// track and sync pay for a network hop to record what it just did.
    pub pr: Option<u64>,
}

/// Where automatic events go, and who is writing them.
///
/// Bound once per command rather than threaded as four arguments: every event a
/// single run records has the same repository, checkout, owner and ledger.
#[derive(Debug)]
pub struct Scribe {
    ledger: Ledger,
    repo: RepoName,
    /// The checkout whose refs anchor entries.
    path: PathBuf,
    owner: String,
}

impl Scribe {
    pub const fn new(ledger: Ledger, repo: RepoName, path: PathBuf, owner: String) -> Self {
        Self {
            ledger,
            repo,
            path,
            owner,
        }
    }

    pub const fn repo(&self) -> &RepoName {
        &self.repo
    }

    /// Append `draft`, stamping the fields no caller supplies.
    pub fn record(&self, draft: &Draft<'_>) -> Result<Entry, LedgerError> {
        let entry = Entry {
            ts: jiff::Timestamp::now().to_string(),
            owner: self.owner.clone(),
            subject: draft.subject.map(str::to_owned),
            kind: draft.kind,
            text: draft.text.clone(),
            evidence: draft.evidence.clone(),
            anchor: self.anchor(draft.subject),
            pr: draft.pr,
        };
        self.ledger.append(&entry)?;
        Ok(entry)
    }

    /// Record that this tool did something, as part of doing it.
    pub fn event(
        &self,
        subject: Option<&str>,
        text: String,
        pr: Option<u64>,
    ) -> Result<Entry, LedgerError> {
        self.record(&Draft {
            subject,
            kind: Kind::Event,
            text,
            evidence: Vec::new(),
            pr,
        })
    }

    /// The subject's tip now, or nothing when it does not resolve.
    ///
    /// One local repository open per append. A branch deleted since, a reaped
    /// release ref, and a checkout that is not a repository all land here, and
    /// none of them is a reason to lose the entry.
    fn anchor(&self, subject: Option<&str>) -> Option<String> {
        let subject = subject?;
        crate::jj::Repo::open(&self.path)
            .ok()?
            .resolve_commit(subject)
            .ok()
            .map(|commit| commit.as_str().to_owned())
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib ledger::`
Expected: PASS, 20 tests.

- [ ] **Step 5: Gates**

Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: no output. (`Scribe::new` takes exactly four arguments, which is `too-many-arguments-threshold`, not past it.)

---

### Task 4: The `notch` command

**Files:**
- Create: `src/commands/notch.rs`
- Create: `tests/notch_command.rs`
- Modify: `src/commands.rs`
- Modify: `src/cli.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `Ledger`, `Filter`, `select`, `Entry`, `Kind`, `Draft`, `Scribe` (Tasks 1–3); `crate::commands::claim::current_owner`; `crate::store::{Store, default_state_path}`; `crate::config::{load, default_config_path}`; `crate::cli::Exit`.
- Produces:
  - `pub struct Request<'a> { pub repo: &'a RepoName, pub subject: Option<&'a str>, pub message: Option<&'a str>, pub evidence: &'a [String], pub pr: Option<u64> }`
  - `pub struct Report { pub repo: String, pub wrote: Option<Entry>, pub entries: Vec<Entry>, pub matched: usize }`
  - `pub fn read(ledger: &Ledger, repo: &RepoName, filter: &Filter<'_>) -> Result<Report, LedgerError>`
  - `pub fn render(report: &Report) -> String`
  - `pub fn run(request: &Request<'_>, json: bool) -> anyhow::Result<Exit>`
  - `cli::Command::Notch { subject: Option<String>, message: Option<String>, evidence: Vec<String>, pr: Option<u64>, repo: Option<String> }`

**Two resolved readings of the spec, both deliberate:**
1. `-m` with no subject writes a **repo-level** entry. The data model says "absent = repo-level entry", and if `-m` required a subject no writer could ever produce one and the field would be dead. Reading is still unambiguous, because a read never passes `-m`.
2. The last-20 window applies **only to the unfiltered view**. "Bare `knives notch` prints recent entries across the current repo (last 20). With a subject, the full chronology" — a caller who named a subject or a pull request asked for that chronology, so `--pr` is not windowed either. When the window truncates, the output says so.

- [ ] **Step 1: Write the failing CLI tests** in a new `tests/notch_command.rs`:

```rust
#![allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    reason = "a fixture or assertion that cannot proceed IS the test failure"
)]

//! The command through the real binary: both output modes, `--repo` from
//! outside the repository, and the exit codes the house rules fix.

use std::path::Path;
use std::process::{Command, Output};

/// A config home with one managed repo whose checkout is `path`.
fn home(path: &Path) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.a-repo]\npath = \"{}\"\nupstream = \"https://forge.invalid/org/work.git\"\norigin = \"https://forge.invalid/ours/work.git\"\n",
            path.display()
        ),
    )
    .expect("write registry");
    home
}

fn knives(home: &tempfile::TempDir, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(args)
        .current_dir(cwd)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "session-owner")
        .output()
        .expect("run knives")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn a_note_written_from_outside_the_repo_is_read_back_in_both_modes() {
    // Given: a config home naming one repo, and a cwd that is not it — the case
    // the --repo flag exists for: you learn something about the library fork
    // while standing in the consumer fork.
    let checkout = tempfile::tempdir().expect("checkout");
    let home = home(checkout.path());
    let elsewhere = tempfile::tempdir().expect("somewhere else");

    // When: a note is written for that repo by name
    let wrote = knives(
        &home,
        elsewhere.path(),
        &[
            "--text",
            "notch",
            "feat/log-queue",
            "-m",
            "superseded by #1157; upstream wanted the trait approach",
            "--evidence",
            "06d778b9",
            "--repo",
            "a-repo",
        ],
    );

    // Then: it succeeded and said what it recorded
    assert_eq!(wrote.status.code(), Some(0), "{}", String::from_utf8_lossy(&wrote.stderr));
    assert!(stdout(&wrote).contains("feat/log-queue"), "was: {}", stdout(&wrote));

    // And: the prose read shows the entry, its kind and its evidence
    let text = knives(&home, elsewhere.path(), &["--text", "notch", "--repo", "a-repo"]);
    let shown = stdout(&text);
    assert!(shown.contains("note"), "was: {shown}");
    assert!(shown.contains("superseded by #1157"), "was: {shown}");
    assert!(shown.contains("06d778b9"), "was: {shown}");

    // And: the JSON read carries the same facts as fields
    let json = knives(&home, elsewhere.path(), &["--json", "notch", "--repo", "a-repo"]);
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("notch --json emits JSON");
    assert_eq!(parsed["repo"], "a-repo");
    assert_eq!(parsed["matched"], 1);
    assert_eq!(parsed["entries"][0]["kind"], "note");
    assert_eq!(parsed["entries"][0]["owner"], "session-owner");
    assert_eq!(parsed["entries"][0]["subject"], "feat/log-queue");
    assert_eq!(parsed["entries"][0]["evidence"][0], "06d778b9");
    // The checkout is a temporary directory, not a repository, so the subject's
    // tip does not resolve and the entry says so by omission.
    assert!(parsed["entries"][0].get("anchor").is_none());
}

#[test]
fn a_subject_read_shows_that_refs_chronology_and_a_bare_read_windows_the_repo() {
    let checkout = tempfile::tempdir().expect("checkout");
    let home = home(checkout.path());

    for index in 0..22 {
        let text = format!("entry {index}");
        let subject = if index % 2 == 0 { "feat/alpha" } else { "feat/beta" };
        let wrote = knives(
            &home,
            checkout.path(),
            &["--text", "notch", subject, "-m", &text, "--repo", "a-repo"],
        );
        assert_eq!(wrote.status.code(), Some(0));
    }

    // A bare read windows to the newest 20 and says how many it did not show.
    let bare = knives(&home, checkout.path(), &["--json", "notch", "--repo", "a-repo"]);
    let parsed: serde_json::Value = serde_json::from_slice(&bare.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 22);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 20);
    assert_eq!(parsed["entries"][0]["text"], "entry 2");

    // A subject read is not windowed: it is that ref's whole chronology.
    let subject = knives(
        &home,
        checkout.path(),
        &["--json", "notch", "feat/alpha", "--repo", "a-repo"],
    );
    let parsed: serde_json::Value = serde_json::from_slice(&subject.stdout).expect("JSON");
    assert_eq!(parsed["matched"], 11);
    assert_eq!(parsed["entries"].as_array().expect("array").len(), 11);
}

#[test]
fn an_unreadable_ledger_is_incomplete_and_an_unknown_repo_is_usage() {
    let checkout = tempfile::tempdir().expect("checkout");
    let home = home(checkout.path());

    // Given: a ledger file with a line that is not an entry
    let ledger = home.path().join("ledger");
    std::fs::create_dir_all(&ledger).expect("ledger directory");
    std::fs::write(ledger.join("a-repo.jsonl"), "not json at all\n").expect("corrupt ledger");

    // When / Then: reading it cannot answer, and says so with exit 3
    let broken = knives(&home, checkout.path(), &["--text", "notch", "--repo", "a-repo"]);
    assert_eq!(broken.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("line 1"),
        "was: {}",
        String::from_utf8_lossy(&broken.stderr)
    );

    // And: a repo nobody manages is a usage error, naming the ones we do
    let unknown = knives(&home, checkout.path(), &["--text", "notch", "--repo", "nope"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&unknown.stderr).contains("a-repo"),
        "was: {}",
        String::from_utf8_lossy(&unknown.stderr)
    );
}

#[test]
fn a_read_of_a_repo_with_no_ledger_yet_is_success_and_says_so() {
    let checkout = tempfile::tempdir().expect("checkout");
    let home = home(checkout.path());
    let empty = knives(&home, checkout.path(), &["--text", "notch", "--repo", "a-repo"]);
    assert_eq!(empty.status.code(), Some(0));
    assert!(stdout(&empty).contains("no notches"), "was: {}", stdout(&empty));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test notch_command`
Expected: FAIL — the binary rejects `notch` (`error: unrecognized subcommand 'notch'`), so every assertion on exit code 0 fails.

- [ ] **Step 3: Add the parser variant** in `src/cli.rs`, in `enum Command`, after the `Depends { ... }` variant and before `Release { ... }`:

```rust
    /// Read what agents did and decided here, or add to it.
    ///
    /// One command, two moods: bare it reads, `-m` writes. Reading is
    /// intentional — nothing injects notches into a session — so the bare form
    /// answers the question an agent actually has, which is what happened here
    /// lately. A subject is a ref name: a branch, or a release, which is a
    /// subject like any other.
    Notch {
        /// The branch or release ref this is about. Omit it to read the whole
        /// repository, or to write an entry about the repository itself.
        subject: Option<String>,
        /// Record this text as a note. Without it, `notch` reads.
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        /// A commit id, `file:line`, `<repo>#<number>` or URL backing the note.
        /// Repeatable, and it may name another repo.
        #[arg(long, requires = "message")]
        evidence: Vec<String>,
        /// Read only entries stamped with this pull request number.
        #[arg(long = "pr", conflicts_with = "message")]
        pr: Option<u64>,
        /// Registry name. Defaults to the repo you are standing in.
        #[arg(long)]
        repo: Option<String>,
    },
```

- [ ] **Step 4: Extend the parser tests** in `src/cli.rs`'s `mod tests`. Add these entries to the `invocations` vector in `every_designed_command_is_reachable`, after the `track` line:

```rust
            vec!["knives", "notch"],
            vec!["knives", "notch", "feat/alpha"],
            vec!["knives", "notch", "feat/alpha", "-m", "superseded"],
            vec!["knives", "notch", "--pr", "1157"],
            vec!["knives", "notch", "release/2026-08-15", "--repo", "a-repo"],
```

and add this test after `sync_can_skip_forge_lookups`:

```rust
    #[test]
    fn notch_refuses_a_read_filter_on_a_write_and_evidence_without_one() {
        // The two moods must stay separable at the parser: a write that also
        // carried a read filter, or evidence with nothing to attach it to, would
        // have to guess what was meant.
        assert!(
            Cli::try_parse_from(["knives", "notch", "feat/a", "-m", "x", "--pr", "7"]).is_err(),
            "a write with a read filter parsed"
        );
        assert!(
            Cli::try_parse_from(["knives", "notch", "feat/a", "--evidence", "06d778b9"]).is_err(),
            "evidence with nothing to attach it to parsed"
        );
        // And: a repo-level note needs no subject, so the model's absent subject
        // is reachable.
        assert!(Cli::try_parse_from(["knives", "notch", "-m", "the fork needs a cut"]).is_ok());
    }
```

- [ ] **Step 5: Write `src/commands/notch.rs`**

```rust
//! `knives notch`: read what happened here, or add to it.
//!
//! Two moods on one command, split by `-m`, because reading and writing the same
//! record are the same act from opposite ends. Reading is intentional: nothing
//! injects notches into a session, so the bare form has to answer the question an
//! agent actually has — what happened in this fork lately — rather than making
//! them name a subject they do not know yet.

use crate::cli::Exit;
use crate::config::{default_config_path, load};
use crate::ids::{BranchName, BranchTarget, RepoName};
use crate::ledger::{Draft, Entry, Filter, Kind, Ledger, LedgerError, Scribe, select};
use crate::store::{Store, default_state_path};

/// How many entries a bare read shows.
///
/// A cap on the unfiltered view only: a reader who named a subject or a pull
/// request asked for that chronology and gets all of it.
const RECENT: usize = 20;

#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub repo: String,
    /// The entry this run appended, when it wrote one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrote: Option<Entry>,
    /// Entries read, oldest first. Empty on a write.
    pub entries: Vec<Entry>,
    /// How many matched before the newest-first window was applied, so a
    /// truncated read never reads as a short history.
    pub matched: usize,
}

/// What one invocation asks for.
#[derive(Debug)]
pub struct Request<'a> {
    pub repo: &'a RepoName,
    pub subject: Option<&'a str>,
    /// Present for a write, absent for a read.
    pub message: Option<&'a str>,
    pub evidence: &'a [String],
    pub pr: Option<u64>,
}

/// Entries for one repository, filtered.
pub fn read(
    ledger: &Ledger,
    repo: &RepoName,
    filter: &Filter<'_>,
) -> Result<Report, LedgerError> {
    let entries = ledger.entries()?;
    let (selected, matched) = select(&entries, filter);
    Ok(Report {
        repo: repo.to_string(),
        wrote: None,
        entries: selected.into_iter().cloned().collect(),
        matched,
    })
}

pub fn render(report: &Report) -> String {
    if let Some(entry) = &report.wrote {
        return wrote_line(&report.repo, entry);
    }
    if report.entries.is_empty() {
        return format!("{}  no notches yet", report.repo);
    }
    let mut lines = vec![format!("{}  {} notch(es)", report.repo, report.matched)];
    if report.matched > report.entries.len() {
        lines.push(format!(
            "  showing the newest {} of {}",
            report.entries.len(),
            report.matched
        ));
    }
    for entry in &report.entries {
        lines.push(format!(
            "  {}  {:<5}  {}",
            entry.ts,
            entry.kind,
            heading(entry)
        ));
        lines.push(format!("    {}", entry.text.replace('\n', "\n    ")));
        if !entry.evidence.is_empty() {
            lines.push(format!("    evidence  {}", entry.evidence.join(", ")));
        }
    }
    lines.join("\n")
}

/// Subject, anchor and stated pull request on one line, each omitted when absent.
fn heading(entry: &Entry) -> String {
    let mut parts = vec![
        entry
            .subject
            .clone()
            .unwrap_or_else(|| "(this repo)".to_owned()),
    ];
    if let Some(anchor) = &entry.anchor {
        parts.push(format!("@{}", short(anchor)));
    }
    if let Some(number) = entry.pr {
        parts.push(format!("#{number}"));
    }
    parts.join("  ")
}

fn wrote_line(repo: &str, entry: &Entry) -> String {
    let subject = entry
        .subject
        .clone()
        .unwrap_or_else(|| format!("{repo} itself"));
    match &entry.anchor {
        Some(anchor) => format!("notched {subject} at {}", short(anchor)),
        None => format!("notched {subject}"),
    }
}

/// Short form for display. Full ids are correct and unreadable.
fn short(id: &str) -> String {
    id.chars().take(12).collect()
}

pub fn run(request: &Request<'_>, json: bool) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(request.repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!(
            "unknown repo {}; known: {}",
            request.repo,
            known.join(", ")
        );
        return Ok(Exit::Usage);
    };
    let ledger = Ledger::for_repo(request.repo);
    let report = match request.message {
        Some(text) => {
            // The store is read, never written: the ledger has its own lock, and a
            // notch changes no intent.
            let store = Store::open(default_state_path())?;
            let pr = request.subject.and_then(|subject| {
                store.tracked_pull(&BranchTarget::new(
                    request.repo.clone(),
                    BranchName::new(subject),
                ))
            });
            let owner = crate::commands::claim::current_owner(&std::env::current_dir()?)?;
            let scribe = Scribe::new(
                ledger,
                request.repo.clone(),
                entry.path.clone(),
                owner,
            );
            let written = scribe.record(&Draft {
                subject: request.subject,
                kind: Kind::Note,
                text: text.to_owned(),
                evidence: request.evidence.to_vec(),
                pr,
            })?;
            Report {
                repo: request.repo.to_string(),
                wrote: Some(written),
                entries: Vec::new(),
                matched: 0,
            }
        }
        None => read(
            &ledger,
            request.repo,
            &Filter {
                subject: request.subject,
                pr: request.pr,
                limit: (request.subject.is_none() && request.pr.is_none()).then_some(RECENT),
            },
        )?,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{}", render(&report));
    }
    Ok(Exit::Ok)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn entry(subject: Option<&str>, kind: Kind, text: &str) -> Entry {
        Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "session-owner".to_owned(),
            subject: subject.map(str::to_owned),
            kind,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: Some("6c42fe71aaaaaaaa".to_owned()),
            pr: Some(1157),
        }
    }

    #[test]
    fn a_read_names_the_subject_the_anchor_and_the_stated_pull_request() {
        let report = Report {
            repo: "a-repo".to_owned(),
            wrote: None,
            entries: vec![entry(
                Some("feat/log-queue"),
                Kind::Note,
                "superseded by #1157",
            )],
            matched: 1,
        };
        let text = render(&report);
        assert!(text.contains("feat/log-queue"), "was: {text}");
        assert!(text.contains("@6c42fe71aaaa"), "was: {text}");
        assert!(text.contains("#1157"), "was: {text}");
        assert!(text.contains("note"), "was: {text}");
        assert!(text.contains("superseded by #1157"), "was: {text}");
    }

    #[test]
    fn a_truncated_read_says_how_many_it_did_not_show() {
        // A window that does not announce itself is how a reader concludes a
        // branch has no older history.
        let report = Report {
            repo: "a-repo".to_owned(),
            wrote: None,
            entries: vec![entry(Some("feat/alpha"), Kind::Event, "claimed")],
            matched: 57,
        };
        assert!(
            render(&report).contains("showing the newest 1 of 57"),
            "was: {}",
            render(&report)
        );
    }

    #[test]
    fn an_empty_read_says_so_rather_than_printing_a_bare_repo_name() {
        let report = Report {
            repo: "a-repo".to_owned(),
            wrote: None,
            entries: Vec::new(),
            matched: 0,
        };
        assert_eq!(render(&report), "a-repo  no notches yet");
    }

    #[test]
    fn a_repo_level_entry_is_headed_by_the_repo_rather_than_an_empty_subject() {
        let report = Report {
            repo: "a-repo".to_owned(),
            wrote: Some(Entry {
                anchor: None,
                pr: None,
                ..entry(None, Kind::Note, "the fork needs a cut")
            }),
            entries: Vec::new(),
            matched: 0,
        };
        assert_eq!(render(&report), "notched a-repo itself");
    }
}
```

- [ ] **Step 6: Declare the module** in `src/commands.rs`: add `pub mod notch;` between `pub mod init;` and `pub mod preflight;`.

- [ ] **Step 7: Wire dispatch** in `src/main.rs`. Add `notch` to the `use knives::commands::{...}` list, and add this arm to the `match cli.command` in `dispatch()`, after the `Command::Depends { .. }` arm:

```rust
        Command::Notch {
            subject,
            message,
            evidence,
            pr,
            repo,
        } => {
            let Some(name) = one_repo(repo.as_deref())? else {
                return Ok(Exit::Usage);
            };
            notch::run(
                &notch::Request {
                    repo: &name,
                    subject: subject.as_deref(),
                    message: message.as_deref(),
                    evidence: &evidence,
                    pr,
                },
                json,
            )
        }
```

- [ ] **Step 8: Run the tests**

Run: `cargo test --test notch_command && cargo test --lib notch:: && cargo test --lib cli::`
Expected: PASS — 4 integration tests, 4 unit tests, and the cli suite including `notch_refuses_a_read_filter_on_a_write_and_evidence_without_one`.

- [ ] **Step 9: Gates**

Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: no output. Update the `// allow: SIZE_OK:` line count at the top of `src/main.rs` to its new `wc -l src/main.rs` value.

---

### Task 5: Automatic events for claiming and handing back

**Files:**
- Modify: `src/commands/claim.rs` (`run_claim` around line 121, `run_release` around line 152)
- Modify: `src/commands/start.rs` (`run`, the tail after `store.save()`)
- Modify: `src/main.rs` (`run_finish` around line 1276; add `scribe_for`)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `Scribe::new`, `Ledger::for_repo`, `Scribe::event` (Task 3); `current_owner`.
- Produces:
  - `fn scribe_for(repo: &RepoName, entry: &knives::config::RepoEntry) -> anyhow::Result<Scribe>` in `src/main.rs`, used by this task and by Tasks 6, 7 and 8.
  - `fn release_event(had: bool, superseded_by: Option<&str>) -> Option<String>` in `src/main.rs`.
  - Event texts, fixed here so readers and tests agree: `claimed: {why}`, `claim released`, `claim released; superseded by {new}`, and `superseded by {new}` for a supersession recorded on a branch nobody held. A `finish` that released nothing and recorded nothing writes no entry at all.

`claim::run_claim` and `claim::run_release` are not reachable from `cli::Command` today — `start` and `finish` absorbed them — but they are public API performing the same mutation, so they record the same events. Their coverage is a unit test; `start` and `finish` are covered through the binary.

- [ ] **Step 1: Write the failing tests.** First, in `src/commands/claim.rs`'s `mod tests`, after `current_owner_falls_back_to_user_when_the_session_id_is_blank`:

```rust
    #[test]
    fn taking_a_claim_records_why_in_the_ledger() {
        // The one "why" this tool records is a claim's, and `finish` deletes it.
        // Without an event here the reason a branch exists dies with the claim.
        let _lock = environment_lock();
        let home = tempfile::tempdir().unwrap();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
        ]);
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.set("KNIVES_OWNER", "session-owner");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        std::fs::write(
            home.path().join("repos.toml"),
            "[repos.a-repo]\npath = \"/tmp/knives-not-a-repository\"\n\
             upstream = \"https://forge.invalid/org/work.git\"\n\
             origin = \"https://forge.invalid/ours/work.git\"\n",
        )
        .unwrap();

        let exit = run_claim(&ClaimRequest {
            target: names(),
            why: "fixing the parser",
            fork_only: false,
        })
        .unwrap();
        assert_eq!(exit, Exit::Ok);

        let entries = crate::ledger::Ledger::for_repo(&crate::ids::RepoName::new("a-repo"))
            .entries()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, crate::ledger::Kind::Event);
        assert_eq!(entries[0].owner, "session-owner");
        assert_eq!(entries[0].subject.as_deref(), Some("feat/alpha"));
        assert_eq!(entries[0].text, "claimed: fixing the parser");
        // The registered path is not a repository, so the subject's tip does not
        // resolve and the entry records that by omitting the anchor rather than
        // by failing.
        assert_eq!(entries[0].anchor, None);
    }

    #[test]
    fn releasing_a_claim_records_where_the_work_went_when_it_went_somewhere() {
        let _lock = environment_lock();
        let home = tempfile::tempdir().unwrap();
        let environment = EnvironmentGuard::capture(&[
            "KNIVES_CONFIG_HOME",
            "KNIVES_OWNER",
            "CLAUDE_CODE_SESSION_ID",
        ]);
        environment.set("KNIVES_CONFIG_HOME", home.path().to_str().unwrap());
        environment.set("KNIVES_OWNER", "session-owner");
        environment.remove("CLAUDE_CODE_SESSION_ID");
        std::fs::write(
            home.path().join("repos.toml"),
            "[repos.a-repo]\npath = \"/tmp/knives-not-a-repository\"\n\
             upstream = \"https://forge.invalid/org/work.git\"\n\
             origin = \"https://forge.invalid/ours/work.git\"\n",
        )
        .unwrap();
        let _ = run_claim(&ClaimRequest {
            target: names(),
            why: "fixing the parser",
            fork_only: false,
        })
        .unwrap();

        let exit = run_release(&names(), Some("feat/replacement")).unwrap();
        assert_eq!(exit, Exit::Ok);

        let entries = crate::ledger::Ledger::for_repo(&crate::ids::RepoName::new("a-repo"))
            .entries()
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[1].text,
            "claim released; superseded by feat/replacement"
        );
    }
```

Then, in `tests/jj_integration.rs`, add these two tests beside the existing `start` tests (search for `"run start"`):

```rust
#[test]
fn starting_and_finishing_a_branch_leaves_its_reason_in_the_ledger() {
    // Given: a managed fork and a config home
    let lab = lab::Lab::new();
    let (home, _consumer) = release_test_home(&lab);

    // When: a branch is started through the binary with a reason
    let started = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "start",
            "feat/alpha",
            "--repo",
            "demo",
            "--why",
            "carrying the queue fix",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "session-owner")
        .output()
        .expect("run start");
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );

    // Then: the ledger holds the claim event, anchored at the branch tip
    let ledger = knives::ledger::Ledger::at(
        home.path().join("ledger").join("demo.jsonl"),
    );
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].kind, knives::ledger::Kind::Event);
    assert_eq!(entries[0].owner, "session-owner");
    assert_eq!(entries[0].subject.as_deref(), Some("feat/alpha"));
    assert_eq!(entries[0].text, "claimed: carrying the queue fix");
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    assert_eq!(entries[0].anchor.as_deref(), Some(tip.as_str()));

    // When: it is handed back naming its successor
    let finished = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args([
            "--text",
            "finish",
            "feat/alpha",
            "--repo",
            "demo",
            "--superseded-by",
            "feat/replacement",
        ])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "session-owner")
        .output()
        .expect("run finish");
    assert!(
        finished.status.success(),
        "finish failed: {}",
        String::from_utf8_lossy(&finished.stderr)
    );

    // Then: the supersession is recorded as an event rather than only as state
    let entries = ledger.entries().expect("read ledger");
    assert_eq!(entries.len(), 2, "was: {entries:?}");
    assert_eq!(
        entries[1].text,
        "claim released; superseded by feat/replacement"
    );
}

/// A claim written straight into the store, so a `finish` test starts from a held
/// branch without `start` putting its own event in the ledger first.
fn hold_claim(home: &tempfile::TempDir, branch: &str) {
    let mut store = Store::open_for_update(home.path().join("state.json")).expect("open store");
    let _ = store.claim(
        &knives::ids::BranchTarget::new(
            knives::ids::RepoName::new("demo"),
            BranchName::new(branch),
        ),
        "session-owner",
        "carrying the queue fix",
    );
    store.save().expect("save store");
}

fn knives_finish(lab: &lab::Lab, home: &tempfile::TempDir, args: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_knives"));
    command.args(["--text", "finish"]);
    command.args(args);
    command
        .args(["--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "session-owner")
        .output()
        .expect("run finish")
}

#[test]
fn finishing_a_held_branch_without_a_successor_records_only_the_release() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);
    hold_claim(&home, "feat/alpha");

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "claim released");
}

#[test]
fn finishing_a_branch_nobody_held_records_no_release_that_never_happened() {
    // The ledger is the one record meant to be trusted months later, and an event
    // is a past-tense fact this tool observed. `finish` on an unheld branch
    // releases nothing — the command's own prose already says "was not held" —
    // so an entry claiming a release is a fabrication in the audit trail.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(&lab, &home, &["feat/alpha"]);
    assert!(finished.status.success());
    assert!(
        String::from_utf8_lossy(&finished.stdout).contains("was not held"),
        "was: {}",
        String::from_utf8_lossy(&finished.stdout)
    );

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    assert!(entries.is_empty(), "a release that never happened: {entries:?}");
}

#[test]
fn finishing_an_unheld_branch_still_records_the_supersession_it_did_record() {
    // Two acts, and either can happen alone: `--superseded-by` writes a
    // supersession into the store whether or not a claim was held, so the entry
    // says that and not the release that did not happen.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let (home, _consumer) = release_test_home(&lab);

    let finished = knives_finish(
        &lab,
        &home,
        &["feat/alpha", "--superseded-by", "feat/replacement"],
    );
    assert!(finished.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1, "was: {entries:?}");
    assert_eq!(entries[0].text, "superseded by feat/replacement");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib claim:: && cargo test --test jj_integration starting_and_finishing && cargo test --test jj_integration finishing_a`
Expected: FAIL — `crate::ledger::Ledger` is reachable but no command writes to it, so `entries.len()` is 0 where 1 or 2 is asserted. `finishing_a_branch_nobody_held_records_no_release_that_never_happened` passes at this point and must keep passing: it is the regression net for the false event, not a red test.

- [ ] **Step 3: Record the event in `run_claim`** (`src/commands/claim.rs`). Add the imports `use crate::ledger::{Ledger, Scribe};` beside the existing `use crate::store::...`, and change the `ClaimOutcome::Taken` arm and the tail:

```rust
    let exit = match &outcome {
        ClaimOutcome::Taken { .. } => {
            let _ = store.claim(&request.target, &owner, request.why);
            if request.fork_only {
                // Without the mark, a branch we deliberately keep with no
                // upstream pull request reads as an error in every report.
                store.mark_fork_only(&request.target, request.why);
            }
            store.save()?;
            // After the state write, because the ledger records what happened and
            // nothing happened until the claim was saved. A failure here fails the
            // command: both files live in one directory, and a write that can fail
            // one can fail the other.
            // A repo the registry does not know has no checkout to anchor against,
            // and an entry with no anchor is still a valid entry.
            let path = registry
                .get(&request.target.repo)
                .map_or_else(std::path::PathBuf::new, |entry| entry.path.clone());
            Scribe::new(
                Ledger::for_repo(&request.target.repo),
                request.target.repo.clone(),
                path,
                owner.clone(),
            )
            .event(
                Some(request.target.branch.as_str()),
                format!("claimed: {}", request.why),
                store.tracked_pull(&request.target),
            )?;
            Exit::Ok
        }
        ClaimOutcome::AlreadyYours { .. } => Exit::Ok,
        ClaimOutcome::HeldByAnother { .. } | ClaimOutcome::UnknownRepo { .. } => Exit::Usage,
    };
```

and `run_release`:

```rust
pub fn run_release(target: &BranchTarget, superseded_by: Option<&str>) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let mut store = Store::open_for_update(default_state_path())?;
    if !store.release_claim(target) {
        eprintln!("no claim on {target}");
        return Ok(Exit::Usage);
    }
    if let Some(replacement) = superseded_by {
        store.supersede(target, replacement);
    }
    let pr = store.tracked_pull(target);
    store.save()?;
    let owner = current_owner(&std::env::current_dir()?)?;
    // A repo the registry does not know has no checkout to anchor against, and an
    // entry with no anchor is still a valid entry.
    let path = registry
        .get(&target.repo)
        .map_or_else(std::path::PathBuf::new, |entry| entry.path.clone());
    Scribe::new(
        Ledger::for_repo(&target.repo),
        target.repo.clone(),
        path,
        owner,
    )
    .event(
        Some(target.branch.as_str()),
        superseded_by.map_or_else(
            || "claim released".to_owned(),
            |replacement| format!("claim released; superseded by {replacement}"),
        ),
        pr,
    )?;
    println!("released {target}");
    Ok(Exit::Ok)
}
```

- [ ] **Step 4: Record the event in `start::run`** (`src/commands/start.rs`). Add `use crate::ledger::{Ledger, Scribe};`, and replace the tail after `add_workspace(...)?;`:

```rust
    let reason = why.unwrap_or("started work");
    let target = BranchTarget::new(repo_name.clone(), branch.clone());
    let _ = store.claim(&target, &owner, reason);
    let pr = store.tracked_pull(&target);
    store.save()?;
    Scribe::new(
        Ledger::for_repo(repo_name),
        repo_name.clone(),
        entry.path.clone(),
        owner.clone(),
    )
    .event(
        Some(branch.as_str()),
        format!("claimed: {reason}"),
        pr,
    )?;

    println!(
        "workspace {} based on {base_revision} ({base_label})\nclaimed {repo_name}/{branch} for {owner}",
        destination.display()
    );
    Ok(Exit::Ok)
```

- [ ] **Step 5: Add `scribe_for` and record the event in `run_finish`** (`src/main.rs`). Add `use knives::ledger::{Draft, Kind, Ledger, Scribe};` to the imports, and this helper immediately above `fn run_finish`:

```rust
/// The ledger writer for a command acting on `entry`.
///
/// The owner is resolved exactly as a claim's is, so one agent's events and its
/// claims carry the same name and a reader can join them.
fn scribe_for(repo: &RepoName, entry: &knives::config::RepoEntry) -> anyhow::Result<Scribe> {
    let owner = knives::commands::claim::current_owner(&std::env::current_dir()?)?;
    Ok(Scribe::new(
        Ledger::for_repo(repo),
        repo.clone(),
        entry.path.clone(),
        owner,
    ))
}
```

Then in `run_finish`, replace the block from `let mut store = ...` through `store.save()?;`:

```rust
    let mut store = Store::open_for_update(default_state_path())?;
    let had = store.release_claim(target);
    if let Some(new) = superseded_by {
        store.supersede(target, new);
    }
    let pr = store.tracked_pull(target);
    store.save()?;
    // What happened, and nothing else. This command runs happily on a branch
    // nobody held — it says "was not held" and forgets the workspace anyway —
    // and an event asserting a release would be a false fact in the one record
    // that exists to be believed later.
    if let Some(text) = release_event(had, superseded_by) {
        scribe_for(&target.repo, entry)?.event(Some(target.branch.as_str()), text, pr)?;
    }
```

and add this beside `scribe_for`:

```rust
/// What a `finish` did, or nothing when it did nothing.
///
/// Releasing a claim and recording a supersession are two acts and either can
/// happen alone: a `finish` on an unheld branch releases no claim, and one with
/// `--superseded-by` still records where the work went.
fn release_event(had: bool, superseded_by: Option<&str>) -> Option<String> {
    match (had, superseded_by) {
        (true, Some(replacement)) => Some(format!("claim released; superseded by {replacement}")),
        (true, None) => Some("claim released".to_owned()),
        (false, Some(replacement)) => Some(format!("superseded by {replacement}")),
        (false, None) => None,
    }
}
```

Not `const fn`, though clippy's `missing_const_for_fn` will not ask for one: `format!` allocates.

- [ ] **Step 6: Run the tests**

Run: `cargo test --lib claim:: && cargo test --test jj_integration starting_and_finishing && cargo test --test jj_integration finishing_a`
Expected: PASS — four tests, including the two that assert a `finish` records only what it did.

- [ ] **Step 7: Confirm nothing else regressed, and the gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS. Update the `// allow: SIZE_OK:` counts at the top of `src/main.rs` and `src/commands/claim.rs`.

---

### Task 6: Automatic events for `track` and `depends`

**Files:**
- Modify: `src/main.rs` (`run_track` around line 1321, `run_depends` around line 1362)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `scribe_for` (Task 5), `Scribe::event`.
- Produces: event texts `stated as #{n}`, `stated as having no upstream pull request`, `pull request statement forgotten`, `no pull request statement to forget`, `requires {list}`.
- Produces the stamping rule for these events: `pr` is the number the entry is **about**. `--pr <n>` stamps `Some(n)`, because the event that creates an association is the one `knives notch --pr <n>` most needs to find; `--forget` stamps the number it withdrew; every other event stamps whatever is stated at the time.

`run_track` does not load the registry today; it must, because an entry's anchor comes from the checkout's refs. `run_depends` already loads it.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs`:

```rust
#[test]
fn stating_a_pull_request_and_a_dependency_leaves_both_statements_in_the_ledger() {
    // Given: a managed fork with a branch, and a sibling repo to depend on
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.demo]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/work.git\"\n\
             [repos.sibling]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/other.git\"\n",
            lab.work.display(),
            lab.upstream.display(),
            lab.work.display(),
            lab.upstream.display(),
        ),
    )
    .expect("write registry");
    let knives = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(args)
            .current_dir(&lab.work)
            .env("KNIVES_CONFIG_HOME", home.path())
            .env("KNIVES_OWNER", "session-owner")
            .output()
            .expect("run knives")
    };

    // When: the branch's pull request is stated, then a dependency, then the
    // statement is withdrawn
    assert!(knives(&["--text", "track", "feat/alpha", "--pr", "4545"]).status.success());
    assert!(
        knives(&["--text", "depends", "feat/alpha", "--on", "sibling#49"])
            .status
            .success()
    );
    assert!(knives(&["--text", "track", "feat/alpha", "--forget"]).status.success());

    // Then: all three statements are in order, anchored, and the stated pull
    // request is stamped on the entries written while it was stated
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    let texts: Vec<&str> = entries.iter().map(|entry| entry.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "stated as #4545",
            "requires sibling#49",
            "pull request statement forgotten"
        ],
        "was: {entries:?}"
    );
    assert!(entries.iter().all(|entry| entry.subject.as_deref() == Some("feat/alpha")));
    // Each entry is stamped with the number it is about: the one that created the
    // association, the one recorded while it stood, and the one it withdrew.
    assert_eq!(
        entries.iter().map(|entry| entry.pr).collect::<Vec<_>>(),
        [Some(4545), Some(4545), Some(4545)],
        "was: {entries:?}"
    );
    let tip = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("tip");
    assert!(entries.iter().all(|entry| entry.anchor.as_deref() == Some(tip.as_str())));

    // And: the whole chronology of that number is findable BY that number, which
    // is the only thing the stamped field is for. Stamping the pre-change value
    // on the statement event would have returned two of the three.
    let filtered = knives(&["--json", "notch", "--pr", "4545"]);
    let parsed: serde_json::Value =
        serde_json::from_slice(&filtered.stdout).expect("notch --json emits JSON");
    assert_eq!(parsed["matched"], 3, "was: {parsed}");
}

#[test]
fn a_fork_only_statement_is_recorded_as_the_decision_it_is() {
    let lab = lab::Lab::new();
    lab.branch("feat/ci-only", "ci.yml", "on: push\n");
    let (home, _consumer) = release_test_home(&lab);

    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "track", "feat/ci-only", "--fork-only", "--repo", "demo"])
        .current_dir(&lab.work)
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_OWNER", "session-owner")
        .output()
        .expect("run track");
    assert!(output.status.success());

    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "stated as having no upstream pull request");
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration stating_a_pull_request_and_a_dependency && cargo test --test jj_integration a_fork_only_statement`
Expected: FAIL — the ledger file does not exist, so `entries()` returns an empty vector and the `texts` comparison fails.

- [ ] **Step 3: Implement in `run_track`** (`src/main.rs`) — replace the whole function:

```rust
/// State or forget which pull request a branch belongs to.
fn run_track(
    target: &BranchTarget,
    pr: Option<u64>,
    fork_only: bool,
    forget: bool,
) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(&target.repo) else {
        eprintln!("unknown repo {}", target.repo);
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    // Read before the change, so a withdrawal is still filed under the number it
    // withdrew.
    let stated = store.tracked_pull(target);
    // Each branch stamps the number its entry is ABOUT, not whatever happened to
    // be stated a moment earlier. The event that creates an association is the
    // one `knives notch --pr <n>` most needs to find, and stamping the prior
    // value there — usually nothing — would hide it from the only filter the
    // field exists for.
    let (text, stamped) = if fork_only {
        store.mark_fork_only(target, "stated with `knives track --fork-only`");
        (
            "stated as having no upstream pull request".to_owned(),
            stated,
        )
    } else if forget {
        let had = store.untrack_pull(target);
        (
            if had {
                "pull request statement forgotten".to_owned()
            } else {
                "no pull request statement to forget".to_owned()
            },
            stated,
        )
    } else {
        let Some(number) = pr else {
            eprintln!("give --pr <number>, or --forget");
            return Ok(Exit::Usage);
        };
        store.track_pull(target, number);
        (format!("stated as #{number}"), Some(number))
    };
    store.save()?;
    scribe_for(&target.repo, entry)?.event(Some(target.branch.as_str()), text.clone(), stamped)?;
    println!("{target} {}", spoken(&text));
    Ok(Exit::Ok)
}

/// The prose form of a `track` outcome, which reads about the branch rather than
/// about the statement.
fn spoken(text: &str) -> String {
    match text {
        "stated as having no upstream pull request" => {
            "deliberately has no upstream pull request".to_owned()
        }
        "pull request statement forgotten" => {
            "is back to inferring its pull request".to_owned()
        }
        "no pull request statement to forget" => "had no stated pull request".to_owned(),
        stated => stated.replacen("stated as ", "is ", 1),
    }
}
```

- [ ] **Step 4: Implement in `run_depends`** (`src/main.rs`) — replace the tail of the function, from `let mut store = ...`:

```rust
    // Resolved before anything is written. Dispatch already validated this name
    // through `one_repo`, so an absent entry is an invariant violation rather
    // than a user error — and the one thing not to do with it is mutate the
    // store and then quietly skip the ledger, which would leave a dependency
    // recorded and unexplained.
    let Some(entry) = registry.get(&target.repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!(
            "unknown repo {}; known: {}",
            target.repo,
            known.join(", ")
        );
        return Ok(Exit::Usage);
    };
    let mut store = Store::open_for_update(default_state_path())?;
    store.add_dependencies(target, &requirements);
    let pr = store.tracked_pull(target);
    store.save()?;
    let listed: Vec<String> = requirements.iter().map(ToString::to_string).collect();
    scribe_for(&target.repo, entry)?.event(
        Some(target.branch.as_str()),
        format!("requires {}", listed.join(", ")),
        pr,
    )?;
    println!("{target} now requires {}", listed.join(", "));
    Ok(Exit::Ok)
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --test jj_integration stating_a_pull_request_and_a_dependency && cargo test --test jj_integration a_fork_only_statement`
Expected: PASS.

- [ ] **Step 6: Confirm the existing prose did not change meaning, and the gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS. If a test asserted on `track`'s old prose (`is #4545`, `is back to inferring its pull request`, `deliberately has no upstream pull request`), `spoken` reproduces each of those strings exactly; a failure here means one was reworded, which is not this task's business.

---

### Task 7: The release cut records its parent set

**Files:**
- Modify: `src/main.rs` (`run_release`, the `if let Some(name) = cut_name` block around lines 1590–1620)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `scribe_for` (Task 5), `Scribe::record`, `Draft`, `Kind`.
- Produces: an event whose subject is the release ref name, text `cut {name} as {short} with {n} parent(s): {branch}@{short}, ...`, and evidence `[created, parent commit ids...]`.

This is the audit-of-record the spec asks for: the full parent set with branch names and commit ids, in the ledger rather than in prose somebody remembers to write into a commit description.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs`, beside the other cut tests:

```rust
#[test]
fn a_release_cut_records_its_whole_parent_set_under_the_release_name() {
    // The audit-of-record: which branches, at which commits, went into which cut.
    // Nine composition losses were found by content comparison and none by
    // metadata, because nothing recorded what a cut had contained.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    let (home, _consumer) = release_test_home(&lab);
    let alpha = Repo::open(&lab.work)
        .expect("open")
        .resolve_commit("feat/alpha")
        .expect("alpha tip");

    // When: a first cut is taken through the binary
    let output = knives_release(&lab, &home, &["cut", "release/2026-08-15"]);
    assert!(
        output.status.success() || output.status.code() == Some(1),
        "cut failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Then: the release ref is the subject, and every member is named with its
    // commit
    let entries = knives::ledger::Ledger::at(home.path().join("ledger").join("demo.jsonl"))
        .entries()
        .expect("read ledger");
    let cut = entries
        .iter()
        .find(|entry| entry.subject.as_deref() == Some("release/2026-08-15"))
        .unwrap_or_else(|| panic!("no cut entry: {entries:?}"));
    assert_eq!(cut.kind, knives::ledger::Kind::Event);
    assert!(cut.text.contains("feat/alpha"), "was: {}", cut.text);
    assert!(cut.text.contains("feat/beta"), "was: {}", cut.text);
    assert!(cut.text.contains("2 parent(s)"), "was: {}", cut.text);
    // And: the commit ids are evidence, so a later reader can check rather than
    // trust the prose
    assert!(
        cut.evidence.iter().any(|reference| reference == alpha.as_str()),
        "was: {:?}",
        cut.evidence
    );
    // And: the entry is anchored at the release it names
    let created = Repo::open(&lab.work)
        .expect("reopen")
        .resolve_commit("release/2026-08-15")
        .expect("release tip");
    assert_eq!(cut.anchor.as_deref(), Some(created.as_str()));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration a_release_cut_records_its_whole_parent_set`
Expected: FAIL — `no cut entry: []`.

- [ ] **Step 3: Implement** in `src/main.rs`'s `run_release`. Immediately after `let created = release::publish_cut(candidate, &request.name, &scheme)?;` and before the `worst = worst.worst(report_completed_cut(` call, insert:

```rust
            // The composition's audit-of-record, recorded as part of taking it:
            // which branches at which commits became this cut. Prose in a commit
            // description is what this replaces, and prose is what nobody wrote.
            let members = carried
                .iter()
                .map(|(source, commit)| format!("{source}@{}", short12(commit)))
                .collect::<Vec<_>>()
                .join(", ");
            let mut evidence = vec![created.as_str().to_owned()];
            evidence.extend(carried.iter().map(|(_, commit)| commit.as_str().to_owned()));
            scribe_for(&repo, &entry)?.record(&Draft {
                subject: Some(&name),
                kind: Kind::Event,
                text: format!(
                    "cut {name} as {} with {} parent(s): {members}",
                    short12(&created),
                    carried.len()
                ),
                evidence,
                pr: None,
            })?;
```

`short12` is the existing helper in `src/main.rs`; `carried` is `&[(String, knives::ids::CommitId)]` in scope, holding each member's source name and commit. A cut is a release ref, not a branch, so nothing stamps a pull request on it.

- [ ] **Step 4: Run the test**

Run: `cargo test --test jj_integration a_release_cut_records_its_whole_parent_set`
Expected: PASS.

- [ ] **Step 5: Confirm the release suite and the gates**

Run: `cargo test --test jj_integration release && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS.

---

### Task 8: Automatic events for `sync` transitions

**Files:**
- Modify: `src/commands/sync.rs` (`sync_repo` around line 168, and its `mod tests` call sites at lines 651, 684, 710, 741, 769, 800, 836, 870)
- Modify: `src/main.rs` (`run_sync` around line 1922)
- Modify: `tests/jj_integration.rs` (the `sync_repo` call at line 1079)

**Interfaces:**
- Consumes: `Scribe::event` (Task 3), `scribe_for` (Task 5).
- Produces: `pub fn sync_repo(name: &RepoName, entry: &RepoEntry, store: &mut Store, forge: Option<&dyn Forge>, scribe: &Scribe) -> anyhow::Result<Report>` — one new parameter, five in total, so the function carries `#[allow(clippy::too_many_arguments, reason = ...)]`.
- Event texts, one per transition: `#{number} merged`, `#{number} closed`, `#{number} advanced to {short}`. Nothing is recorded for `unchanged` or `new`: a first sighting is not something that happened, and an unchanged pull request is the absence of an event.

The subject is the branch whose head the pull request is, when this fork carries one. A tracked number with no branch of ours — a foreign parent — is a repo-level entry, and its number lives in the text, because `pr` is stamped from `tracked_pulls` only.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs`:

```rust
#[test]
fn sync_records_one_event_for_each_pull_request_that_moved() {
    // Given: three tracked pull requests — one merged, one closed, one whose head
    // advanced — and one that did not move
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state) in [
        (10, "feat/merged", "MERGED"),
        (11, "feat/closed", "CLOSED"),
        (12, "feat/moved", "OPEN"),
        (13, "feat/still", "OPEN"),
    ] {
        let _ = pull_requests.insert(
            BranchName::new(branch),
            PullRequest {
                number,
                state: state.to_owned(),
                head_ref_name: branch.to_owned(),
                head_ref_oid: format!("head-{number}"),
                ..PullRequest::default()
            },
        );
    }
    let forge = knives::forge::FakeForge {
        pull_requests,
        ..knives::forge::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let mut store =
        Store::open_for_update(state.path().join("state.json")).expect("open store");
    store.record_pull_head(&name, 12, "older");
    store.record_pull_head(&name, 13, "head-13");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo.jsonl"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work.clone(),
        "session-owner".to_owned(),
    );

    // When: sync classifies them
    let report = sync::sync_repo(&name, &entry, &mut store, Some(&forge), &scribe)
        .expect("sync report");
    assert_eq!(report.rows.len(), 4, "was: {report:?}");

    // Then: exactly the three that moved are in the ledger, each under its branch
    let entries = ledger.entries().expect("read ledger");
    let recorded: Vec<(Option<&str>, &str)> = entries
        .iter()
        .map(|entry| (entry.subject.as_deref(), entry.text.as_str()))
        .collect();
    assert_eq!(
        recorded,
        [
            (Some("feat/merged"), "#10 merged"),
            (Some("feat/closed"), "#11 closed"),
            (Some("feat/moved"), "#12 advanced to head-12"),
        ],
        "was: {entries:?}"
    );
    assert!(entries.iter().all(|entry| entry.owner == "session-owner"));
    assert!(
        entries.iter().all(|entry| entry.kind == knives::ledger::Kind::Event),
        "sync observed these; it did not assert them"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration sync_records_one_event`
Expected: FAIL to compile — `sync_repo` takes 4 arguments, 5 supplied.

- [ ] **Step 3: Implement in `sync_repo`** (`src/commands/sync.rs`). Add `use crate::ledger::Scribe;` to the imports, and change the signature and the row-recording block:

```rust
#[allow(
    clippy::too_many_arguments,
    reason = "a sync names its repo, its entry, the state it advances, the forge it asks and the ledger it records to; bundling them hides which of the five it mutates"
)]
pub fn sync_repo(
    name: &RepoName,
    entry: &RepoEntry,
    store: &mut Store,
    forge: Option<&dyn Forge>,
    scribe: &Scribe,
) -> anyhow::Result<Report> {
```

Inside the `for (number, label) in tracked` loop, replace the `report.rows.push(Row { ... });` statement with:

```rust
        let transition = classify_pull(
            seen.get(&number.to_string()).map(String::as_str),
            &current,
            &state,
        );
        // The branch whose head this is, when this fork carries one. A tracked
        // number with no branch of ours is a foreign release parent, so its entry
        // is about the repository and names the number in its text.
        let subject = pull_requests
            .iter()
            .find(|(_, pull_request)| pull_request.number == number)
            .map(|(branch, _)| branch.to_string());
        if let Some(text) = transition_text(number, transition, &current) {
            let pr = subject.as_deref().map(|branch| {
                BranchTarget::new(name.clone(), BranchName::new(branch))
            });
            scribe.event(
                subject.as_deref(),
                text,
                pr.and_then(|target| store.tracked_pull(&target)),
            )?;
        }
        report.rows.push(Row {
            number,
            label,
            state: transition,
        });
```

and add this helper above `sync_repo`:

```rust
/// What to record about a pull request that moved, and nothing for one that did not.
///
/// `unchanged` is the absence of an event, and `new` is a first sighting rather
/// than something that happened: recording either would fill a fork's history
/// with one line per pull request per run.
fn transition_text(number: u64, state: PullState, head: &str) -> Option<String> {
    match state {
        PullState::Merged => Some(format!("#{number} merged")),
        PullState::Closed => Some(format!("#{number} closed")),
        PullState::Advanced => Some(format!(
            "#{number} advanced to {}",
            head.chars().take(12).collect::<String>()
        )),
        PullState::Unchanged | PullState::New => None,
    }
}
```

Add `BranchTarget` to the `use crate::ids::{...}` line in `src/commands/sync.rs`.

- [ ] **Step 4: Give `run_sync` a scribe** (`src/main.rs`) — replace its loop:

```rust
    let mut worst = Exit::Ok;
    for (name, entry) in chosen {
        let scribe = scribe_for(&name, &entry)?;
        let report = sync::sync_repo(&name, &entry, &mut store, forge, &scribe)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            println!("{}", sync::render(&report));
        }
        worst = worst.worst(sync::exit_for(&report));
    }
    Ok(worst)
```

- [ ] **Step 5: Update the nine existing `sync_repo` call sites.** Each gains a scribe writing into the test's own temporary directory, so no test touches a real config home. In `src/commands/sync.rs`'s `mod tests` and `mod tracking_tests`, add this helper beside `local_entry`:

```rust
    /// A scribe writing into the fixture's own directory. Every test that calls
    /// `sync_repo` needs one, and none of them may reach the real config home.
    fn test_scribe(temp: &TempDir, name: &RepoName) -> crate::ledger::Scribe {
        crate::ledger::Scribe::new(
            crate::ledger::Ledger::at(temp.path().join("ledger.jsonl")),
            name.clone(),
            temp.path().to_owned(),
            "a-test".to_owned(),
        )
    }
```

Then at each of lines 651, 684, 710, 741, 769, 800, 836 and 870, add `&test_scribe(&temp, &repo_name)` (or `&test_scribe(&temp, &RepoName::new("test-repo"))` where the name is inline) as the fifth argument. In `tests/jj_integration.rs` at line 1079, the call becomes:

```rust
    let scribe = knives::ledger::Scribe::new(
        knives::ledger::Ledger::at(lab.work.join("ledger.jsonl")),
        name.clone(),
        lab.work.clone(),
        "a-test".to_owned(),
    );
    let report = sync::sync_repo(&name, &entry, &mut store, Some(&StateUnavailableForge), &scribe)
        .expect("sync report");
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --test jj_integration sync_records_one_event && cargo test --lib sync::`
Expected: PASS.

- [ ] **Step 7: Whole suite and gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS.

---

### Task 9: The status breadcrumb

_**Storage note (2026-08-16).** Task 11 swapped the ledger to one markdown file per entry before this task runs. The snippets below still execute — `Ledger::at` takes any path, which is now a directory — but adjust the fixtures while implementing: drop the `.jsonl` suffix from every ledger path (`demo.jsonl` → `demo`, `a-repo.jsonl` → `a-repo`), and build the corrupt-ledger fixture as a garbage entry file inside the ledger directory — `std::fs::create_dir_all(&path)` then `std::fs::write(path.join("20260815T221403.000000000Z-0000.md"), "not a ledger entry at all\n")` — the exact shape Task 11 Step 6 gives `tests/notch_command.rs`. The `notch` surface this task builds is unchanged._

**Files:**
- Modify: `src/commands/status.rs`
- Modify: `src/main.rs` (`run_status`, the `status::Options` literal around line 1484)
- Modify: `tests/jj_integration.rs` (the five `status::Options` literals at lines 2482, 2537, 2585, 2650, 2701)

**Interfaces:**
- Consumes: `Ledger`, `Entry`, `Kind`, `newest_for`, `age` (Tasks 1–2).
- Produces:
  - `pub struct LastNotch { pub ts: String, pub kind: crate::ledger::Kind, pub text: String }` with `fn of(entry: &Entry) -> Self`
  - `BranchRow::notch: Option<LastNotch>`
  - `Options::ledger: Option<&'a Ledger>`
  - `fn notches_from_ledger(ledger: Option<&Ledger>, report: &mut Report) -> Vec<Entry>`
  - `fn notch_cell(row: &BranchRow) -> String`, `const NOTCH_TEXT: usize = 32`
  - `fn add_releases(report: &mut Report, repo: &Repo, tips: &BookmarkTips, entry: &RepoEntry) -> anyhow::Result<()>`
  - a ninth branch-table column, `notch`

- [ ] **Step 1: Write the failing tests.** In `src/commands/status.rs`'s `mod tests`, add:

```rust
    #[test]
    fn a_branchs_newest_notch_is_one_token_at_the_end_of_its_line() {
        // Status text is already dense: the breadcrumb is one token, and its
        // legibility overhaul is separate work.
        let mut row = row("feat/log-queue", None, None);
        row.notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "superseded by #1157".to_owned(),
        });
        let lines = branch_table(&[row]);
        assert!(lines[0].contains("notch"), "header: {}", lines[0]);
        assert!(
            lines[1].ends_with("\"superseded by #1157\" (now)"),
            "was: {}",
            lines[1]
        );
    }

    #[test]
    fn a_long_or_multi_line_notch_cannot_break_the_table() {
        // An entry's text is free prose that may run to a paragraph and may carry
        // newlines. One stray newline destroys every column below it.
        let mut row = row("feat/alpha", None, None);
        row.notch = Some(LastNotch {
            ts: jiff::Timestamp::now().to_string(),
            kind: crate::ledger::Kind::Note,
            text: "parked by the owner\nuntil the trait lands upstream, which may be weeks"
                .to_owned(),
        });
        let lines = branch_table(&[row]);
        assert_eq!(lines.len(), 2, "was: {lines:?}");
        assert!(!lines[1].contains('\n'));
        assert!(lines[1].contains('…'), "truncation is marked: {}", lines[1]);
        assert!(
            lines[1].contains("parked by the owner until"),
            "newlines collapse to spaces: {}",
            lines[1]
        );
    }

    #[test]
    fn a_branch_with_no_notch_renders_the_empty_placeholder() {
        let lines = branch_table(&[row("feat/alpha", None, None)]);
        assert!(lines[1].ends_with(" -"), "was: {}", lines[1]);
        assert_eq!(columns(&lines[1]).len(), 9, "was: {}", lines[1]);
    }

    #[test]
    fn a_ledger_that_cannot_be_read_is_an_unanswered_question_not_an_absence() {
        // A report that quietly showed no breadcrumbs would say this fork's
        // history was never written.
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("a-repo.jsonl");
        std::fs::write(&path, "not json at all\n").expect("corrupt ledger");
        let ledger = crate::ledger::Ledger::at(path);
        let mut report = Report::default();

        let notches = notches_from_ledger(Some(&ledger), &mut report);

        assert!(notches.is_empty());
        assert_eq!(report.problems.len(), 1, "was: {report:?}");
        assert!(report.problems[0].contains("ledger"), "was: {report:?}");
        assert_eq!(exit_for(&report), Exit::Incomplete);
    }
```

and update the two alignment tests: in `an_empty_cell_never_shifts_its_neighbours` change both `assert_eq!(header_offsets.len(), 8, ...)` and `assert_eq!(columns(&lines[2]).len(), 8, ...)` to `9`.

Then, in `tests/jj_integration.rs`, add the end-to-end test:

```rust
#[test]
fn status_carries_each_branchs_newest_notch_in_json_and_in_text() {
    // Given: a fork with a branch and two notches on it, the second the newest
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let name = knives::ids::RepoName::new("demo");
    let entry = RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let ledger = knives::ledger::Ledger::at(state.path().join("demo.jsonl"));
    let scribe = knives::ledger::Scribe::new(
        ledger.clone(),
        name.clone(),
        lab.work.clone(),
        "session-owner".to_owned(),
    );
    scribe
        .event(Some("feat/alpha"), "claimed: carrying the fix".to_owned(), None)
        .expect("first notch");
    scribe
        .event(Some("feat/alpha"), "claim released; superseded by feat/next".to_owned(), None)
        .expect("second notch");
    scribe
        .event(Some("feat/unrelated"), "claimed: something else".to_owned(), None)
        .expect("other branch");

    // When: status gathers with the ledger available
    let report = status::gather(
        &name,
        &entry,
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: None,
            registry: None,
            ledger: Some(&ledger),
        },
    )
    .expect("gather");

    // Then: the newest entry for that branch is on its row, and nobody else's is
    let alpha = report
        .branches
        .iter()
        .find(|row| row.name.as_str() == "feat/alpha")
        .expect("the branch has a row");
    let last = alpha.notch.as_ref().expect("a breadcrumb");
    assert_eq!(last.text, "claim released; superseded by feat/next");
    assert_eq!(last.kind, knives::ledger::Kind::Event);

    // And: it survives serialisation under the name the design fixed
    let json = serde_json::to_value(&report).expect("report serialises");
    let rows = json["branches"].as_array().expect("branches");
    let row = rows
        .iter()
        .find(|row| row["name"] == "feat/alpha")
        .expect("row");
    assert_eq!(row["notch"]["kind"], "event");
    assert_eq!(row["notch"]["text"], "claim released; superseded by feat/next");
    assert!(row["notch"]["ts"].is_string());

    // And: the branch line carries one token for it
    let text = status::render(&report, false);
    assert!(
        text.contains("\"claim released; superseded by fe…\""),
        "was: {text}"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib status:: && cargo test --test jj_integration status_carries_each_branchs`
Expected: FAIL to compile — `BranchRow` has no field `notch`, `Options` has no field `ledger`, `LastNotch` and `notches_from_ledger` do not exist.

- [ ] **Step 3: Add the types and the row field** in `src/commands/status.rs`. Add `use crate::ledger::{Entry as Notch, Ledger, newest_for};` to the imports. Add to `struct BranchRow`, after `stated_pull`:

```rust
    /// The newest ledger entry about this branch, when it has one.
    ///
    /// A local file read the tool already sits beside, and the difference between
    /// a reader running one more command and a reader concluding a branch was
    /// never explained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notch: Option<LastNotch>,
```

Add `notch: None,` to `BranchRow::bare`'s literal, and after the `StatedPull` struct:

```rust
/// The part of a ledger entry a branch row carries.
///
/// Three fields, not the entry: a row is not the place to re-print an owner, an
/// anchor and a list of evidence that `knives notch <branch>` shows in full.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LastNotch {
    pub ts: String,
    pub kind: crate::ledger::Kind,
    pub text: String,
}

impl LastNotch {
    fn of(entry: &Notch) -> Self {
        Self {
            ts: entry.ts.clone(),
            kind: entry.kind,
            text: entry.text.clone(),
        }
    }
}
```

Add to `struct Options`:

```rust
    /// This repository's ledger, for the per-branch breadcrumb. `None` reads none.
    pub ledger: Option<&'a Ledger>,
```

- [ ] **Step 4: Add the reader and the release-scan extraction.** After `fn note_fetched_heads`, add:

```rust
/// Every notch in this repository's ledger, read once for the whole report.
///
/// One local file read per repository rather than one per branch. A ledger that
/// exists and cannot be read is an unanswered question rather than an absence:
/// a report that quietly showed no breadcrumbs would say this fork's history was
/// never written.
fn notches_from_ledger(ledger: Option<&Ledger>, report: &mut Report) -> Vec<Notch> {
    let Some(ledger) = ledger else {
        return Vec::new();
    };
    match ledger.entries() {
        Ok(entries) => entries,
        Err(error) => {
            report.problems.push(format!("ledger unavailable: {error}"));
            Vec::new()
        }
    }
}

/// Fold the release scan into a report.
///
/// Extracted from `gather` for the same reason `scan_releases` was: that function
/// sits within a few lines of the file's hundred-line limit, and the breadcrumb
/// adds to it.
fn add_releases(
    report: &mut Report,
    repo: &Repo,
    tips: &BookmarkTips,
    entry: &RepoEntry,
) -> anyhow::Result<()> {
    // Releases are scanned local AND remote: what a consumer pins is the remote
    // ref, and scanning only local silently skipped the actually-pinned release.
    let (names, findings, skipped) = scan_releases(
        repo,
        &ReleaseScan {
            path: &entry.path,
            tips,
            scheme: &entry.release_scheme(),
            publish_remote: entry.publish_remote(),
        },
    )?;
    report.releases = names;
    report.findings.extend(findings);
    if skipped > 0 {
        report
            .notes
            .push(format!("{skipped} superseded release(s) not scanned"));
    }
    Ok(())
}
```

- [ ] **Step 5: Change `gather`.** Replace the block from the `// Releases are scanned local AND remote` comment through the `if skipped > 0 { ... }` block with:

```rust
    add_releases(&mut report, &repo, &tips, entry)?;
```

Add, immediately after `let (branches, fetched_heads) = maintained_branches(&tips, trunk, &scheme);`:

```rust
    let notches = notches_from_ledger(options.ledger, &mut report);
```

Compute the breadcrumb before the row literal, because that literal's first field is
`name: branch` and a moved branch cannot be borrowed afterwards. Insert this line
immediately above `report.branches.push(BranchRow {`:

```rust
        let notch = newest_for(&notches, branch.as_str()).map(LastNotch::of);
```

and add the field `notch,` to that literal, after `stated_pull,`. Then add
`notches: &notches,` to the `DivergentInput { ... }` literal.

- [ ] **Step 6: Give divergent rows their breadcrumb too.** Add to `struct DivergentInput`:

```rust
    notches: &'a [Notch],
```

and in `divergent_rows`'s `BranchRow { ... }` literal, before the `..BranchRow::bare(branch, None)` line:

```rust
            notch: newest_for(input.notches, branch.as_str()).map(LastNotch::of),
```

- [ ] **Step 7: Add the cell and the ninth column.** After `fn flags_cell`, add:

```rust
/// How much of a notch's text a branch line carries.
const NOTCH_TEXT: usize = 32;

/// The newest notch on this branch, as one token.
///
/// Truncated and whitespace-collapsed because an entry's text is free prose that
/// may run to a paragraph and may contain newlines, and this is a table cell: one
/// stray newline destroys every column below it.
fn notch_cell(row: &BranchRow) -> String {
    let Some(notch) = &row.notch else {
        return "-".to_owned();
    };
    let collapsed = notch.text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut shown: String = collapsed.chars().take(NOTCH_TEXT).collect();
    if collapsed.chars().count() > NOTCH_TEXT {
        shown.push('…');
    }
    match crate::ledger::age(&notch.ts, jiff::Timestamp::now()) {
        Some(age) => format!("\"{shown}\" ({age})"),
        None => format!("\"{shown}\""),
    }
}
```

Then replace `fn branch_table` with its nine-column form:

```rust
fn branch_table(rows: &[BranchRow]) -> Vec<String> {
    const HEADER: [&str; 9] = [
        "branch", "tip", "push", "pr", "review", "checks", "landed", "flags", "notch",
    ];

    let cells: Vec<[String; 9]> = rows
        .iter()
        .map(|row| {
            [
                branch_cell(row),
                tip_cell(row),
                push_cell(row),
                pull_request_cell(row),
                review_cell(row),
                checks_cell(row),
                landed_cell(row),
                flags_cell(row),
                notch_cell(row),
            ]
        })
        .collect();
    let mut widths = HEADER.map(str::len);
    for row in &cells {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.len());
        }
    }
    let format_row = |cells: [&str; 9]| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = cells;
        let [
            branch_width,
            tip_width,
            push_width,
            pull_request_width,
            review_width,
            checks_width,
            landed_width,
            flags_width,
            notch_width,
        ] = widths;
        format!(
            "    {branch:<branch_width$}  {tip:<tip_width$}  {push:<push_width$}  {pull_request:<pull_request_width$}  {review:<review_width$}  {checks:<checks_width$}  {landed:<landed_width$}  {flags:<flags_width$}  {notch:<notch_width$}"
        )
        .trim_end()
        .to_owned()
    };
    let mut lines = vec![format_row(HEADER)];
    lines.extend(cells.iter().map(|row| {
        let [
            branch,
            tip,
            push,
            pull_request,
            review,
            checks,
            landed,
            flags,
            notch,
        ] = row.each_ref();
        format_row([
            branch.as_str(),
            tip.as_str(),
            push.as_str(),
            pull_request.as_str(),
            review.as_str(),
            checks.as_str(),
            landed.as_str(),
            flags.as_str(),
            notch.as_str(),
        ])
    }));
    lines
}
```

- [ ] **Step 8: Pass the ledger from `run_status`** (`src/main.rs`). Add before the loop:

```rust
    let mut worst = Exit::Ok;
    let mut first = true;
    for (name, entry) in chosen {
        let ledger = knives::ledger::Ledger::for_repo(&name);
        let report = status::gather(
            &name,
            &entry,
            &store,
            &status::Options {
                probe,
                forge,
                registry: Some(&registry),
                ledger: Some(&ledger),
            },
        )?;
```

- [ ] **Step 9: Update the five `Options` literals** in `tests/jj_integration.rs` (lines 2482, 2537, 2585, 2650, 2701) by adding `ledger: None,` after `registry: None,` — those tests are about detectors, and a repository with no ledger is what they mean.

- [ ] **Step 10: Run the tests**

Run: `cargo test --lib status:: && cargo test --test jj_integration status_carries_each_branchs`
Expected: PASS, including the two updated alignment tests.

- [ ] **Step 11: Whole suite and gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS with no clippy output. If `clippy::too_many_lines` fires on `gather`, `add_releases` did not absorb the whole release-scan block — re-check Step 5.

---

### Task 10: Skills and docs

_**Storage note (2026-08-16).** The storage paragraphs quoted in this task's skill and doc text describe the superseded JSONL layer (`<repo>.jsonl`, "one JSON object per line", `<repo>.jsonl.lock`, "~200 bytes"). Write the revised storage instead, from spec §1.2 (revised 2026-08-16) and Task 11: one markdown file per entry with TOML frontmatter between `+++` fences under `~/.config/knives/ledger/<repo>/`; entry files are immutable — never rewritten, never deleted; a write is one atomic `create_new` and there is no lockfile; reads scan the directory in lexicographic filename order, which is chronological; an entry is ~300 bytes, no rotation, no retention; unknown frontmatter keys are ignored; exit 3 remains "the ledger exists and cannot be read" — the directory or an entry file within it. Everything else in this task — the two kinds, evidence, anchors, the past-tense doctrine, the skill structure — stands as written._

**Files:**
- Modify: `skills/fork-work/SKILL.md`
- Modify: `skills/using-knives/SKILL.md`
- Modify: `skills/pr-preflight/SKILL.md`
- Modify: `README.md`
- Modify: `docs/design.md`

**Interfaces:**
- Consumes: the command surface from Task 4 and the breadcrumb from Task 9. Nothing consumes this task.

The identity guard scans `skills/` and `docs/`: no forge host with a trailing slash, no project-family literals. Use `forge.invalid` if an example needs a URL.

- [ ] **Step 1: `skills/fork-work/SKILL.md`** — insert a new section between "Then find out what is going on in it" and "Get your own working copy the managed way":

```markdown
## Then read the notches

```
knives notch
```

What agents did and decided here lately: claims taken and handed back, pull requests
stated, dependencies recorded, releases cut, and whatever anyone thought worth writing
down. Before you touch a branch you do not understand, ask about that branch:

```
knives notch <branch>
```

Every entry carries the branch's tip at the time it was written. That is the part to read
carefully: an entry saying "superseded by #1157" at a commit the branch has since moved
past is a reason to re-check, not a conclusion to inherit. A weird branch nobody can
explain is exactly what this answers, and the reason it exists is usually one line long.

When you make a call worth remembering — this is superseded, the owner parked it, you
promised a reviewer something, you re-homed a pull request onto another branch — record it
before you move on:

```
knives notch <branch> -m "what you decided and why" --evidence <commit-or-ref>
```

Cite something. Every audit claim that survived review cited a commit or a `file:line`;
every false one did not.
```

- [ ] **Step 2: `skills/using-knives/SKILL.md`** — insert a full reference section between `### knives depends <branch> --on <repo>#<number>` and `### knives release [REPO]`:

```markdown
### `knives notch [SUBJECT]`

The record of what happened in this fork and what was decided, one append-only file per
repository beside the state file. `knives status` deletes nothing, but `knives finish` does:
it removes the claim that said why a branch exists. The ledger is where that survives.

Two moods on one command. Bare, it reads:

```
knives notch                      # the newest 20 entries across this repo
knives notch <branch>             # that ref's whole chronology, oldest first
knives notch release/2026-08-15   # a release is a subject like any branch
knives notch --pr 4545            # only entries stamped with that pull request
knives notch --repo other         # a repo you are not standing in
```

With `-m`, it writes:

```
knives notch <branch> -m "superseded by #1157; upstream wanted the trait approach" \
  --evidence 06d778b9 --evidence other-repo#1157
knives notch -m "this fork needs a cut before the pin moves"   # about the repo itself
```

`--repo` works in both moods, and it is the flag for the case that keeps happening: you
are standing in the consumer fork when you learn something about the library fork, and the
entry belongs in the library's ledger.

#### What an entry holds

| Field | Written by | Content |
|---|---|---|
| `ts` | automatically | when it was written, RFC 3339 UTC |
| `owner` | automatically | the same identity a claim gets |
| `subject` | you | the ref it is about; absent for an entry about the repository |
| `kind` | automatically | `event` when a knives command observed it, `note` when an agent asserted it |
| `text` | you, or the command | the entry itself |
| `evidence` | you, optional | commit ids, `file:line`, `<repo>#<number>`, URLs, and they may name other repos |
| `anchor` | automatically | the subject's tip at write time, absent when it did not resolve |
| `pr` | automatically | the pull request `knives track` states for the subject, if any |

Two kinds, not three. The question a reader has is whether a machine observed this or an
agent asserted it. Supersessions and parkings arrive as events, through `finish
--superseded-by` and `start --why`; everything you assert by hand is a note.

`anchor` is why this record does not rot. A stored disposition goes wrong the moment
upstream moves: a branch census that inferred "not a release parent, therefore unhomed"
produced 54 findings of which 5 were false, and a parity audit's finding was true at its
recorded commit and stale two hours later. A past-tense entry anchored to a commit stays
true. So the ledger holds events and judgments, never derived state — if a detector can
compute it, do not write it down.

#### What writes entries without being asked

Every command that already witnesses something records it as part of doing it. A failed
append fails the command: the ledger and the state file live in one directory, and a write
that can fail one can fail the other.

| Command | Entry |
|---|---|
| `start`, `claim` | `claimed: <why>` on the branch |
| `finish`, `release-claim` | `claim released`, or `claim released; superseded by <branch>` |
| `track --pr/--fork-only/--forget` | the statement that changed |
| `depends --on` | `requires <repo>#<number>` |
| `release cut` | the whole parent set, branch names and commit ids, under the release's own name — the audit of what that cut contained |
| `sync` | one entry per tracked pull request that merged, closed or advanced |

Nothing is recorded for a pull request that did not move, and nothing injects any of this
into a session: reading the ledger is intentional, and that is the point.

#### In `knives status`

Each branch row carries its newest entry. In JSON that is `notch: {ts, kind, text}`,
absent when the branch has none; in text it is one truncated token at the end of the line,
`"superseded by #1157…" (3d)`. It is a local file read, so it costs nothing.

#### Storage and exit codes

`~/.config/knives/ledger/<repo>.jsonl`, one JSON object per line, append-only: no entry is
ever rewritten or deleted, and within a file the order of lines is authoritative even if
two agents' clocks disagree. Appends hold `<repo>.jsonl.lock` so concurrent agents cannot
interleave. There is no rotation and no retention policy — an entry is about 200 bytes.

Readers ignore fields they do not know, so a newer binary can add one and an older one
still reads the line. There is no version number and there never needs to be.

`0` fine, `2` a usage error, `3` when the ledger exists and cannot be read — which is
deliberately not the same as a repository nobody has notched yet, and that one is `0` with
`no notches yet`.
```

Also add `notch` to the status branch-table column list in that file: after the `flags` bullet in "#### Branch table columns", add

```markdown
9. `notch`: the newest ledger entry for this branch as one truncated token with its age (`"superseded by #1157…" (3d)`), or `-` when there is none. `knives notch <branch>` prints it in full.
```

- [ ] **Step 3: `skills/pr-preflight/SKILL.md`** — add a step between "## Step 2: Verification Checklist" and "## Step 3: Execution", and renumber Execution to Step 4:

```markdown
## Step 3: Record What You Promised

A pull request review is a conversation with a person who will not be here next session,
and a promise made in a review thread is invisible to the next agent. Before opening the
pull request, and again after every review round that leaves you owing something, record
it:

```bash
knives notch <branch> -m "promised the maintainer we would split the config change out" \
  --evidence <repo>#<number>
```

Promises belong in notches, not in a session that ends. `knives notch <branch>` before you
answer a review is how you find out what you already owe. Which review threads are still
unanswered is a different question, derived from the forge, and not this.
```

- [ ] **Step 4: `README.md`** — add a row to the Commands table after the `knives depends` row:

```markdown
| `knives notch` | what agents did and decided here, and add to it |
```

and a section after "## What it checks":

```markdown
## What it remembers

Everything above is computed on demand and nothing is cached. One thing cannot be computed:
why. `knives finish` deletes the claim that said why a branch exists, and after that the
only honest answer to "what is this branch" is archaeology.

So each repository has a ledger — one append-only JSON-lines file beside the state file —
and every command that witnesses something writes to it as part of doing it: claims taken
and handed back, pull requests stated, dependencies recorded, the full parent set of every
release cut, and each tracked pull request that merged, closed or advanced. Agents add
their own judgments by hand:

```
knives notch feat/log-queue -m "superseded by #1157; upstream wanted the trait approach" \
  --evidence 06d778b9
knives notch feat/log-queue
```

Every entry records the subject's tip at the time it was written, which is what keeps the
record from rotting: a conclusion recorded against a commit the branch has since moved
past is a reason to re-check rather than something to inherit. The ledger holds what
happened and what was decided. It never holds anything a detector can recompute.

`knives status` carries each branch's newest entry on its row, so the question "what is
this weird branch" is usually answered before you ask it.
```

- [ ] **Step 5: `docs/design.md`** — three edits.

In "## State", add a fifth bullet and a closing paragraph:

```markdown
- **what happened, and what was decided**: an append-only ledger per repo, beside the state
  file. Everything above is current intent, rewritten whole on each change; `knives finish`
  deletes the one "why" the tool records. The ledger is the past tense: events this tool
  observed in its own commands, and judgments an agent asserted, each anchored to the
  subject's tip at write time.

The ledger's rule is past tense only. Stored dispositions rot and recorded judgments do
not: a census that inferred "not a release parent, therefore unhomed" produced 54 findings
of which 5 were false, and a parity audit's finding was true at its recorded commit and
stale two hours later after an in-place repair. An entry says what happened, at which
commit, according to whom, and never what is currently the case. Anything currently the
case is a detector's job, and the detectors are cheap.
```

In "## Command surface", add to the code block after the `knives depends` line:

```
knives notch [SUBJECT]         read what happened here (bare: newest 20; a subject: its whole
                               chronology); -m writes a note, --evidence backs it
```

And in the same section's prose, after the paragraph about `--json`, add:

```markdown
`knives notch` is the one command with two moods, split by `-m`: bare it reads, `-m` writes.
Reading is intentional and nothing injects notches into a session, so the bare form has to
answer the question an agent actually has rather than making them name a subject they do
not know yet. The `status` breadcrumb — each branch's newest entry, one token on its line —
is the other half of that: the record is no use if reading it requires knowing it exists.
```

- [ ] **Step 6: Verify the identity guard and that the skills still parse**

Run: `cargo test --test no_hardcoded_identity`
Expected: PASS. A failure names the file and the literal.

- [ ] **Step 7: Read the three skills end to end as a fresh agent would**

Run: `cargo test`
Expected: PASS. Then read `skills/fork-work/SKILL.md` and confirm the new section sits between the status section and the `knives start` section, and that nothing above it now contradicts it — the "Read the claims before you touch anything" line is still true and now has a companion.

---

### Task 11: Storage swap — JSONL to markdown entries

**Decision record (2026-08-16, settled with Sami mid-implementation, spec §1.2 "Storage (revised 2026-08-16)"):** markdown is easier to search and easier to git-track. Tasks 1–8 landed on JSONL as planned; this task swaps the storage layer underneath the API they built and leaves that API intact — `Entry`, `Kind`, `Filter`, `select`, `newest_for`, `age`, `Scribe`, `Draft`, `Ledger::{for_repo, at, path, append, entries}` all keep their signatures — so the command code Tasks 4–8 wrote does not change at all. Only `src/ledger.rs` internals, one `src/store.rs` method this PR itself added, test fixtures, and wording move. The `--json` output of `notch` and the future `status` breadcrumb are untouched: `Entry`'s serde derives are the report surface and stay exactly as they are.

**The shape.** One file per entry: `~/.config/knives/ledger/<repo>/<stamp>-<suffix>.md`. `<stamp>` is the entry's `ts` compacted to a filename at nanosecond precision (`20260815T221403.123456789Z` — always nine subsecond digits); `<suffix>` is four random hex characters. Every column is fixed width, so lexicographic filename order is chronological order, and that order is what `entries` returns. The file is TOML frontmatter between `+++` lines — every 1.1 field except `text` — then the text as the markdown body:

```markdown
+++
ts = "2026-08-15T22:14:03.123456789Z"
owner = "session-owner"
subject = "feat/log-queue"
kind = "note"
evidence = ["06d778b9"]
anchor = "6c42fe71"
pr = 1157
+++
superseded by #1157; upstream wanted the trait approach
```

Entry files are immutable — never rewritten, never deleted. A write is one atomic `create_new` and there is **no lockfile**: two writers write two files, so there is nothing to interleave and `StoreLock::acquire_at` loses its only consumer and is deleted. A `create_new` collision — same nanosecond, same suffix — is a loud `LedgerError::Collision`, not a retry loop. An unparseable entry file is a loud error naming the file; unknown frontmatter keys are ignored (schema evolution unchanged); files without the `.md` extension are not entries and are ignored, so an editor's or sync tool's droppings cannot poison the record. Absent optional fields are omitted from the frontmatter exactly as they were omitted from the JSON line.

**Two facts verified against the locked dependencies, so do not re-derive them:**
- `toml::to_string` (toml 0.9.12) ends its output with a newline, so `format!("+++\n{frontmatter}+++\n{text}\n")` assembles a well-formed file.
- toml 0.9.12 serializes a string containing newlines as a *multi-line* TOML string whose lines land verbatim inside the frontmatter — an evidence string holding a `+++` line puts a bare `+++` line **inside** the frontmatter. The closing fence is therefore *the first `+++` line whose preceding block parses as TOML*, never a plain "split at the first `+++`". A regression test below pins this.

**Accepted race, named so review does not rediscover it:** between `create_new` and the single `write_all` there is a microseconds-wide window in which a concurrent read sees a file that does not parse and errors loudly. That is the designed behavior — a loud, retryable read beats a lock on every append — and it is the shape the revised spec fixes ("a write is one atomic `create_new`; no lockfile is needed at all"). Similarly decided: `LedgerError::Collision` has no dedicated unit test, because forcing a collision would need a suffix-injection seam that would exist only for the test; the concurrency test asserts it never fires in the ordinary case, and the mapping is a two-line match arm.

**Randomness without a `rand` dependency:** the four hex characters come from `RandomState` hashing the stamp and a process-wide counter — the exact idiom `src/hook/guidance.rs::envelope_nonce` already uses. Per-process random keys make cross-process suffixes independent; the counter separates same-instant calls within one process. Sixteen bits is enough for its only job: disambiguating two writers inside the same nanosecond.

**Write order within a process is read order.** Two stamps drawn back-to-back can be equal on a coarse platform clock, and equal stamps would leave read order to the random filename suffix — which would flake every ordered assertion over entries one command writes. `Scribe::record` therefore draws from `monotonic_now()`: `Timestamp::now()` bumped one nanosecond past the last stamp this process handed out whenever the clock has not advanced. That single stamp feeds both the frontmatter `ts` and the filename, so the two never disagree. Across processes the wall clock is the order, as before — sequential commands are milliseconds apart, and genuinely concurrent writers have no meaningful order to preserve; the suffix plus `create_new` keeps those from colliding.

**Files:**
- Modify: `src/ledger.rs` (module doc; `default_ledger_path`; `Entry` doc; `LedgerError`; `Ledger::{append, entries}`; new private `Frontmatter`, `entry_file_name`, `parse_file`, `split_fenced`; delete `parse_line` and `lock_path`; rewrite the storage half of `mod tests`)
- Modify: `src/store.rs` (delete `StoreLock::acquire_at`, restore `acquire`'s inline body — lines ~136–172)
- Modify: `src/commands/sync.rs` (one fixture path in `mod tests`, line ~730)
- Modify: `tests/jj_integration.rs` (12 fixture paths)
- Modify: `tests/notch_command.rs` (the corrupt-ledger fixture and its assertion)

**Interfaces:**
- Consumes: everything Tasks 1–3 produced; `toml = "0.9"` (already a dependency — `src/config.rs` parses the registry with it: `toml::from_str` at ~438, `toml::to_string_pretty` at ~494); `jiff::Timestamp::{strftime, subsec_nanosecond, as_nanosecond}` (jiff 0.2.35; `Timestamp::strftime` formats in UTC and "will never error or panic").
- Produces (what T9, T10 and later readers rely on):
  - `pub fn default_ledger_path(repo: &RepoName) -> PathBuf` — now `~/.config/knives/ledger/<repo>`, a **directory**.
  - `Ledger::{for_repo, at, path, append, entries}` — signatures unchanged; `path()` is the directory.
  - `pub enum LedgerError { Read, Write, Parse, Timestamp, Collision, Serialise }` — `Parse { path: PathBuf, detail: String }` names a file, not a line; `Timestamp { path: PathBuf, ts: String }`; `Collision { path: PathBuf }` is new; `Lock` retires with the lockfile; `Serialise` wraps `toml::ser::Error` now.
  - The entry-file shape above — what T10's skill text must describe.

- [ ] **Step 1: Rewrite the storage half of `mod tests` in `src/ledger.rs`**

The pure Task 2 and Task 3 tests never knew the file shape and stay byte-for-byte: `a_subject_filter_keeps_only_that_refs_chronology`, `a_release_ref_is_a_subject_like_any_branch`, `a_pull_request_filter_reads_the_stamped_field_only`, `a_limit_keeps_the_newest_and_reports_how_many_it_did_not_show`, `an_age_is_the_shortest_form_that_is_still_true`, `an_event_stamps_the_fields_no_caller_supplies`, `an_anchor_is_omitted_when_the_subject_does_not_resolve`, `a_note_carries_its_evidence_and_a_repo_level_entry_has_no_subject`. The `entry` and `stamped` helpers stay unchanged too.

**1a. Delete these eight tests** — each is replaced below or retired with the mechanism it proved:
- `an_entry_round_trips_through_the_file_in_write_order` → replaced by `entries_read_back_in_stamp_order_whatever_the_write_order` (order now comes from the stamp, not from append order, so the fixture needs distinct timestamps).
- `a_field_this_version_does_not_know_is_ignored_rather_than_rejected` → replaced by `a_frontmatter_key_this_version_does_not_know_is_ignored_rather_than_rejected`.
- `a_newline_in_an_entrys_text_stays_one_line` → the one-entry-one-line constraint died with JSONL; replaced by `a_multi_line_text_round_trips_verbatim`.
- `a_line_that_is_not_an_entry_is_reported_with_its_number` → lines are gone; replaced by `a_file_that_is_not_an_entry_is_reported_by_name`.
- `an_unreadable_timestamp_is_reported_rather_than_rendered` → split into the read-side `an_unreadable_timestamp_in_a_file_is_reported_rather_than_rendered` and the new write-side `an_entry_with_an_unreadable_timestamp_cannot_be_written`.
- `a_second_writer_cannot_append_while_the_first_holds_the_lock` → retired with the lock; there is no lock to hold.
- `two_writers_appending_at_once_lose_no_line_and_interleave_none` → replaced by `concurrent_writers_produce_distinct_files_that_all_parse` (same guarantee, no retry loop — there is nothing to retry).
- `a_repos_ledger_sits_beside_the_state_file_in_its_own_directory` → replaced by `a_repos_ledger_is_a_directory_beside_the_state_file`.

**1b. In the `scribe` helper, change the `Ledger::at` line** to the directory path:

```rust
    fn scribe(dir: &std::path::Path) -> Scribe {
        Scribe::new(
            Ledger::at(dir.join("ledger").join("a-repo")),
            RepoName::new("a-repo"),
            dir.join("not-a-repository"),
            "session-owner".to_owned(),
        )
    }
```

**1c. Add two helpers** beside `entry`:

```rust
    fn entry_at(ts: &str, subject: Option<&str>, text: &str) -> Entry {
        Entry {
            ts: ts.to_owned(),
            ..entry(subject, text)
        }
    }

    /// The one entry file in `dir`, for a test that inspects what was written.
    fn only_file(dir: &Path) -> PathBuf {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .map(|dirent| dirent.unwrap().path())
            .collect();
        assert_eq!(files.len(), 1, "expected exactly one entry file: {files:?}");
        files.remove(0)
    }
```

**1d. Change one path** in `a_ledger_that_does_not_exist_yet_is_empty_rather_than_an_error`: `Ledger::at(dir.path().join("never-written"))` — the rest of the test stays.

**1e. Add the new and replacement tests:**

```rust
    #[test]
    fn entries_read_back_in_stamp_order_whatever_the_write_order() {
        // Chronology lives in the filename stamp now, not in a shared file's
        // append order: whoever wrote first by clock reads first, even when
        // the later entry hit the disk earlier.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));

        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.000000002Z",
                Some("feat/beta"),
                "second",
            ))
            .unwrap();
        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.000000001Z",
                Some("feat/alpha"),
                "first",
            ))
            .unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].text, "first");
        assert_eq!(read[1].text, "second");
        assert_eq!(read[0].subject.as_deref(), Some("feat/alpha"));
        assert_eq!(read[0].kind, Kind::Note);
        assert_eq!(read[0].anchor.as_deref(), Some("6c42fe71"));
    }

    #[test]
    fn an_absent_subject_pr_and_anchor_survive_as_absent() {
        // A repo-level entry has no subject; an entry about a deleted branch has
        // no anchor. Neither may come back as an empty string, which would read
        // as a branch named "".
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let bare = Entry {
            anchor: None,
            ..entry(None, "the fork needs a release cut before Friday")
        };
        ledger.append(&bare).unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read[0].subject, None);
        assert_eq!(read[0].anchor, None);
        assert_eq!(read[0].pr, None);
        // And: absent fields are omitted from the frontmatter rather than
        // written as some empty stand-in, so nothing reads back as present.
        let text = std::fs::read_to_string(only_file(ledger.path())).unwrap();
        assert!(!text.contains("subject"), "was: {text}");
        assert!(!text.contains("anchor"), "was: {text}");
    }

    #[test]
    fn a_frontmatter_key_this_version_does_not_know_is_ignored_rather_than_rejected() {
        // Entry files are never rewritten, so a newer binary may add a key and
        // an older one must still read the file. That is the whole evolution
        // story.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("20260815T221403.000000000Z-0000.md"),
            "+++\nts = \"2026-08-15T22:14:03Z\"\nowner = \"x\"\nkind = \"event\"\n\
             from_the_future = \"v\"\n+++\nclaimed\n",
        )
        .unwrap();

        let read = Ledger::at(path).entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].kind, Kind::Event);
        assert_eq!(read[0].text, "claimed");
    }

    #[test]
    fn a_multi_line_text_round_trips_verbatim() {
        // The body IS the text — no escaping layer to get wrong in either
        // direction — and a fence-looking line inside the text must stay text
        // rather than truncate the body.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let text = "parked\nby the owner\n+++\nthat line is prose, not a fence";
        ledger.append(&entry(Some("feat/alpha"), text)).unwrap();
        assert_eq!(ledger.entries().unwrap()[0].text, text);
    }

    #[test]
    fn an_evidence_string_containing_a_fence_line_still_round_trips() {
        // TOML serializes a string with newlines as a multi-line string whose
        // lines land verbatim inside the frontmatter — possibly a bare `+++`.
        // The closing fence is therefore the first `+++` line whose block
        // parses as TOML, not the first `+++` line outright; this is the test
        // that breaks if that rule regresses to a plain split.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let sneaky = Entry {
            evidence: vec!["quoted from the review:\n+++\ndo not merge".to_owned()],
            ..entry(Some("feat/alpha"), "promised a follow-up")
        };
        ledger.append(&sneaky).unwrap();
        assert_eq!(ledger.entries().unwrap()[0], sneaky);
    }

    #[test]
    fn a_file_that_is_not_an_entry_is_reported_by_name() {
        // A ledger the tool cannot read must not read as a ledger with nothing
        // in it: that is the silent-empty failure this whole record exists to
        // prevent. One bad file fails the read, and the error names the file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        Ledger::at(path.clone())
            .append(&entry(Some("feat/alpha"), "fine"))
            .unwrap();
        std::fs::write(
            path.join("20990101T000000.000000000Z-dead.md"),
            "not a ledger entry at all\n",
        )
        .unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(
                &error,
                LedgerError::Parse { path, .. }
                    if path.ends_with("20990101T000000.000000000Z-dead.md")
            ),
            "was: {error}"
        );
    }

    #[test]
    fn a_file_without_the_md_extension_is_not_an_entry() {
        // An editor's or a sync tool's droppings beside the entries are not
        // entries and not a reason to refuse the whole record.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        Ledger::at(path.clone())
            .append(&entry(Some("feat/alpha"), "fine"))
            .unwrap();
        std::fs::write(path.join(".20990101.md.swp"), "junk").unwrap();

        let read = Ledger::at(path).entries().unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].text, "fine");
    }

    #[test]
    fn an_unreadable_timestamp_in_a_file_is_reported_rather_than_rendered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(
            path.join("20260815T221403.000000000Z-0000.md"),
            "+++\nts = \"last tuesday\"\nowner = \"x\"\nkind = \"note\"\n+++\na\n",
        )
        .unwrap();

        let error = Ledger::at(path).entries().unwrap_err();
        assert!(
            matches!(&error, LedgerError::Timestamp { ts, .. } if ts == "last tuesday"),
            "was: {error}"
        );
    }

    #[test]
    fn an_entry_with_an_unreadable_timestamp_cannot_be_written() {
        // The filename stamp derives from `ts`, so a timestamp nothing can
        // order is refused at the write rather than discovered at some later
        // read of the whole directory.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        let bad = Entry {
            ts: "last tuesday".to_owned(),
            ..entry(Some("feat/alpha"), "a")
        };
        let error = ledger.append(&bad).unwrap_err();
        assert!(
            matches!(&error, LedgerError::Timestamp { ts, .. } if ts == "last tuesday"),
            "was: {error}"
        );
    }

    #[test]
    fn filenames_carry_the_stamp_so_lexicographic_order_is_chronological() {
        // `entries` sorts by name and nothing else; the fixed-width stamp is
        // the property that makes that sort a chronology. A second boundary is
        // the trap: 22:14:04 with no subsecond digits must still sort after
        // 22:14:03.999999999, so the stamp always carries nine digits.
        let dir = tempfile::tempdir().unwrap();
        let ledger = Ledger::at(dir.path().join("a-repo"));
        ledger
            .append(&entry_at(
                "2026-08-15T22:14:03.999999999Z",
                Some("feat/alpha"),
                "earlier",
            ))
            .unwrap();
        ledger
            .append(&entry_at("2026-08-15T22:14:04Z", Some("feat/alpha"), "later"))
            .unwrap();

        let read = ledger.entries().unwrap();
        assert_eq!(read[0].text, "earlier");
        assert_eq!(read[1].text, "later");

        let mut names: Vec<String> = std::fs::read_dir(ledger.path())
            .unwrap()
            .map(|dirent| dirent.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert!(
            names[0].starts_with("20260815T221403.999999999Z-"),
            "was: {}",
            names[0]
        );
        assert!(
            names[1].starts_with("20260815T221404.000000000Z-"),
            "was: {}",
            names[1]
        );
        assert!(names[0].ends_with(".md"), "was: {}", names[0]);
    }

    #[test]
    fn concurrent_writers_produce_distinct_files_that_all_parse() {
        // JSONL needed a lockfile so two agents could not interleave one shared
        // file. A file per entry needs none: `create_new` either wins a fresh
        // name or errors, so the assertions left worth making are that nothing
        // is lost, nothing collides and everything parses when several agents
        // notch at once — the ordinary case on a machine running many.
        const WRITERS: usize = 4;
        const EACH: usize = 25;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a-repo");

        std::thread::scope(|scope| {
            for writer in 0..WRITERS {
                let path = path.clone();
                let _ = scope.spawn(move || {
                    let ledger = Ledger::at(path);
                    for index in 0..EACH {
                        // Real writers draw from the monotonic scribe clock; these
                        // do too, so stamps are process-unique and a collision cannot happen.
                        let mut record =
                            entry(Some("feat/alpha"), &format!("{writer}:{index}"));
                        record.ts = monotonic_now().to_string();
                        ledger.append(&record).unwrap();
                    }
                });
            }
        });

        let files = std::fs::read_dir(&path).unwrap().count();
        assert_eq!(files, WRITERS * EACH, "every append is its own file");
        let entries = Ledger::at(path).entries().unwrap();
        assert_eq!(entries.len(), WRITERS * EACH, "every file parses");
        for writer in 0..WRITERS {
            for index in 0..EACH {
                let wanted = format!("{writer}:{index}");
                assert!(
                    entries.iter().any(|entry| entry.text == wanted),
                    "missing: {wanted}"
                );
            }
        }
    }

    #[test]
    fn a_repos_ledger_is_a_directory_beside_the_state_file() {
        let _lock = crate::config::test_support::environment_lock();
        let environment =
            crate::config::test_support::EnvironmentGuard::capture(&["KNIVES_CONFIG_HOME"]);
        environment.set("KNIVES_CONFIG_HOME", "/tmp/knives-home");
        assert_eq!(
            default_ledger_path(&RepoName::new("a-repo")),
            std::path::PathBuf::from("/tmp/knives-home/ledger/a-repo")
        );
    }
```

**1f. Replace the fixture in `an_append_that_cannot_be_written_is_an_error_rather_than_a_shrug`** — the old test put a directory where the file should be; the new one puts a file where the directory should be:

```rust
    #[test]
    fn an_append_that_cannot_be_written_is_an_error_rather_than_a_shrug() {
        // A ledger append failure fails its command loudly: the ledger and the
        // state file live in one config home, and a write that can fail one
        // can fail the other. Here a stray file squats on the directory name.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ledger").join("a-repo");
        std::fs::create_dir_all(dir.path().join("ledger")).unwrap();
        std::fs::write(&path, "a file where the ledger directory should be").unwrap();
        let blocked = Scribe::new(
            Ledger::at(path),
            RepoName::new("a-repo"),
            dir.path().to_owned(),
            "session-owner".to_owned(),
        )
        .event(Some("feat/alpha"), "claimed".to_owned(), None);
        assert!(
            matches!(blocked, Err(LedgerError::Write { .. })),
            "was: {blocked:?}"
        );
    }
```

**1g. Rename `the_newest_entry_for_a_subject_is_the_last_one_in_the_file`** — there is no file; the helper's contract is the order it is given:

```rust
    #[test]
    fn the_newest_entry_for_a_subject_is_the_last_one_given() {
        // Last in the order given, which on disk is stamp order: the helper
        // itself never reorders, so a hand-built slice keeps its own order.
        let entries = vec![
            Entry {
                ts: "2026-08-15T23:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "given first, clock ahead")
            },
            Entry {
                ts: "2026-08-15T22:00:00Z".to_owned(),
                ..stamped(Some("feat/alpha"), None, "given second, clock behind")
            },
            stamped(Some("feat/beta"), None, "another branch"),
        ];
        assert_eq!(
            newest_for(&entries, "feat/alpha").map(|e| e.text.as_str()),
            Some("given second, clock behind")
        );
        assert_eq!(newest_for(&entries, "feat/never-notched"), None);
    }
```

**1h. Add the two clock tests** — the monotonic guarantee directly, and the write-order contract it exists for:

```rust
    #[test]
    fn stamps_drawn_back_to_back_strictly_advance() {
        // A tight loop outpaces the clock's real granularity somewhere; the
        // bump keeps every stamp strictly later than the one before anyway.
        let mut previous = monotonic_now();
        for _ in 0..1000 {
            let next = monotonic_now();
            assert!(next > previous, "stamps must advance: {next} <= {previous}");
            previous = next;
        }
    }

    #[test]
    fn entries_written_back_to_back_read_back_in_write_order() {
        // One hundred writes as fast as the machine can make them: equal
        // wall-clock stamps would hand the order to the random suffix, and the
        // monotonic bump is what forbids equal stamps within a process.
        let dir = tempfile::tempdir().unwrap();
        let scribe = scribe(dir.path());
        for index in 0..100 {
            scribe
                .event(Some("feat/alpha"), format!("entry {index}"), None)
                .unwrap();
        }
        let texts: Vec<String> = scribe
            .ledger
            .entries()
            .unwrap()
            .into_iter()
            .map(|entry| entry.text)
            .collect();
        let wanted: Vec<String> = (0..100).map(|index| format!("entry {index}")).collect();
        assert_eq!(texts, wanted);
    }
```

- [ ] **Step 2: Run the ledger tests to watch them fail**

Run: `cargo test --lib ledger`
Expected: compile error — `cannot find function 'monotonic_now' in this scope` (the concurrency test and both clock tests reference it; Step 3k adds it). Nothing in the new tests names `LedgerError::Collision`, so there is no missing-variant error; were the clock tests absent, the rest would compile against the JSONL implementation and fail behaviorally instead — a ledger path is now a directory (`an_absent_subject_pr_and_anchor_survive_as_absent` panics listing a file as one, `a_repos_ledger_is_a_directory_beside_the_state_file` sees the old `.jsonl` path) and order now comes from the stamp (`entries_read_back_in_stamp_order_whatever_the_write_order` reads back write order instead).

- [ ] **Step 3: Swap the storage implementation in `src/ledger.rs`**

**3a. Replace the module doc** (the `//!` block at the top of the file):

```rust
//! What agents did and decided here, in order, forever.
//!
//! [`crate::store`] holds current intent and is rewritten whole on every change:
//! `knives finish` deletes the claim that said why a branch exists, and nothing
//! remembers it afterwards. Agents then rediscover a mysterious branch by
//! archaeology, or draw a conclusion from a stale one.
//!
//! One directory of immutable markdown files per repository, beside
//! `state.json` — each entry its own file, TOML frontmatter between `+++`
//! fences, the text as the body. An entry is an event (this tool observed one
//! of its own commands) or a note (an agent asserted something), anchored to
//! the subject's tip at write time. That anchor is why the record does not
//! rot: a reader who sees the tip has moved since knows to re-verify rather
//! than inherit the conclusion. Nothing derived is stored — a recorded
//! past-tense judgment stays true, while a cached disposition goes wrong the
//! moment upstream moves.
```

**3b. Replace the `use` block** — the store's lock and serde_json leave; the hash-based suffix and `OsStr` arrive:

```rust
use std::collections::hash_map::RandomState;
use std::ffi::OsStr;
use std::hash::{BuildHasher, Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::config::default_config_path;
use crate::ids::RepoName;
```

**3c. Replace `default_ledger_path`:**

```rust
/// Where a repository's ledger lives: a directory of entry files beside
/// `state.json`, one immutable file per entry, so concurrent writers never
/// share a file and a git history over the directory is pure additions.
pub fn default_ledger_path(repo: &RepoName) -> PathBuf {
    default_config_path()
        .with_file_name("ledger")
        .join(repo.to_string())
}
```

**3d. Replace the doc comment above `pub struct Entry`** (the struct body and its field attributes are unchanged):

```rust
/// One entry of the ledger.
///
/// Unknown frontmatter keys are ignored rather than rejected: entries are never
/// rewritten, so a newer binary may add a field and an older one must still
/// read the file. That is the whole schema-evolution story, and it is why there
/// is no version number.
///
/// The serde derives here are the `--json` report surface — their
/// skip-if-absent attributes are why an absent anchor is absent in JSON too.
/// The file surface is `Frontmatter`, which is this struct minus `text`.
```

**3e. Replace `LedgerError`** — `Parse` names a file instead of a line, `Timestamp` loses its line, `Lock` retires with the lockfile, `Collision` arrives, `Serialise` wraps toml:

```rust
#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is not a ledger entry: {detail}")]
    Parse { path: PathBuf, detail: String },
    #[error("{path} has an unreadable timestamp `{ts}`")]
    Timestamp { path: PathBuf, ts: String },
    #[error("{path} already exists: two writers drew the same nanosecond and suffix")]
    Collision { path: PathBuf },
    #[error("serialising a ledger entry: {source}")]
    Serialise {
        #[from]
        source: toml::ser::Error,
    },
}
```

**3f. Add `Frontmatter` directly below `Entry`:**

```rust
/// The machine surface of an entry file: every 1.1 field except the text,
/// which is the markdown body rather than a TOML value, so prose reads and
/// writes as prose.
///
/// Kept separate from [`Entry`] deliberately: `Entry`'s serde is the `--json`
/// report surface and must keep `text`; this is the file surface and must not.
#[derive(Debug, Serialize, Deserialize)]
struct Frontmatter {
    ts: String,
    owner: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    subject: Option<String>,
    kind: Kind,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pr: Option<u64>,
}

impl Frontmatter {
    fn of(entry: &Entry) -> Self {
        Self {
            ts: entry.ts.clone(),
            owner: entry.owner.clone(),
            subject: entry.subject.clone(),
            kind: entry.kind,
            evidence: entry.evidence.clone(),
            anchor: entry.anchor.clone(),
            pr: entry.pr,
        }
    }

    fn into_entry(self, text: String) -> Entry {
        Entry {
            ts: self.ts,
            owner: self.owner,
            subject: self.subject,
            kind: self.kind,
            text,
            evidence: self.evidence,
            anchor: self.anchor,
            pr: self.pr,
        }
    }
}
```

**3g. Replace `Ledger::append`:**

```rust
    /// Write one entry as one new immutable file.
    ///
    /// `create_new` is the whole concurrency story: two agents appending at
    /// the same moment write two different files, so there is nothing to
    /// interleave and no lock to hold. A filename collision — same nanosecond,
    /// same random suffix — errors loudly instead of retrying, because at that
    /// resolution a retry would paper over a broken clock or random source.
    /// Between the create and the single `write_all` a reader can glimpse a
    /// file that does not parse yet; it errors loudly and a re-run answers,
    /// which costs less than every append taking a lock ever did.
    pub fn append(&self, entry: &Entry) -> Result<(), LedgerError> {
        let ts: jiff::Timestamp = entry.ts.parse().map_err(|_| LedgerError::Timestamp {
            path: self.path.clone(),
            ts: entry.ts.clone(),
        })?;
        let contents = format!(
            "+++\n{}+++\n{}\n",
            toml::to_string(&Frontmatter::of(entry))?,
            entry.text
        );
        std::fs::create_dir_all(&self.path).map_err(|source| LedgerError::Write {
            path: self.path.clone(),
            source,
        })?;
        let path = self.path.join(entry_file_name(ts));
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(file) => file,
            Err(source) if source.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(LedgerError::Collision { path });
            }
            Err(source) => return Err(LedgerError::Write { path, source }),
        };
        file.write_all(contents.as_bytes())
            .map_err(|source| LedgerError::Write { path, source })
    }
```

**3h. Replace `Ledger::entries` and delete `Ledger::parse_line`:**

```rust
    /// Every entry, oldest first: lexicographic filename order, which the
    /// fixed-width stamp makes chronological order.
    ///
    /// A ledger directory that does not exist yet is empty rather than an
    /// error: a repository nobody has notched is the normal case. An entry
    /// file that does not parse IS an error, because a ledger the tool cannot
    /// read must not read as a ledger with nothing in it. Only `*.md` files
    /// are entries: an editor's or a sync tool's droppings beside them are
    /// ignored, not fatal.
    pub fn entries(&self) -> Result<Vec<Entry>, LedgerError> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let listing = std::fs::read_dir(&self.path).map_err(|source| LedgerError::Read {
            path: self.path.clone(),
            source,
        })?;
        let mut files = Vec::new();
        for dirent in listing {
            let path = dirent
                .map_err(|source| LedgerError::Read {
                    path: self.path.clone(),
                    source,
                })?
                .path();
            if path.extension() == Some(OsStr::new("md")) {
                files.push(path);
            }
        }
        files.sort();
        files.iter().map(|path| parse_file(path)).collect()
    }
```

**3i. Replace `newest_for`'s doc comment** (the function body is unchanged):

```rust
/// The newest entry about `subject`: the last match in the order `entries`
/// returns, which is stamp order on disk.
///
/// The stamp is the authority now that every entry is its own file — there is
/// no shared file whose append order could disagree with it, and two writers
/// inside the same nanosecond have no meaningful "newer" to preserve.
```

**3j. Delete `fn lock_path` and add the three private functions in its place** (bottom of the file, above `mod tests`):

```rust
/// `20260815T221403.123456789Z-4f2a.md`: the entry's timestamp compacted to a
/// filename at nanosecond precision, then four random hex characters. Every
/// column is fixed width, so lexicographic order over the directory is
/// chronological order — the property `entries` sorts by.
///
/// The suffix exists for two writers inside the same nanosecond. It comes from
/// `RandomState` hashing the stamp and a process-wide counter — the idiom
/// `src/hook/guidance.rs` already uses for its nonce — because the crate
/// carries no `rand` dependency and sixteen bits do not justify one.
fn entry_file_name(ts: jiff::Timestamp) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut hasher = RandomState::new().build_hasher();
    ts.as_nanosecond().hash(&mut hasher);
    COUNTER.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    let suffix = hasher.finish() & 0xffff;
    format!(
        "{}.{:09}Z-{suffix:04x}.md",
        ts.strftime("%Y%m%dT%H%M%S"),
        ts.subsec_nanosecond()
    )
}

/// One entry file: TOML frontmatter between `+++` fences, then the text.
fn parse_file(path: &Path) -> Result<Entry, LedgerError> {
    let text = std::fs::read_to_string(path).map_err(|source| LedgerError::Read {
        path: path.to_owned(),
        source,
    })?;
    let Some(rest) = text.strip_prefix("+++\n") else {
        return Err(LedgerError::Parse {
            path: path.to_owned(),
            detail: "missing the opening +++ fence".to_owned(),
        });
    };
    let (frontmatter, body) = split_fenced(rest).map_err(|detail| LedgerError::Parse {
        path: path.to_owned(),
        detail,
    })?;
    // Checked here rather than at every reader: a timestamp nothing can order
    // is a corrupt record, and one loud error beats a breadcrumb with no age.
    if frontmatter.ts.parse::<jiff::Timestamp>().is_err() {
        return Err(LedgerError::Timestamp {
            path: path.to_owned(),
            ts: frontmatter.ts,
        });
    }
    Ok(frontmatter.into_entry(body))
}

/// The frontmatter before the closing `+++` fence, and the body after it.
///
/// The closing fence is the first `+++` line whose preceding block parses as
/// TOML — not the first `+++` line outright, because a frontmatter value
/// containing a newline serializes as a multi-line TOML string, and such a
/// string may itself contain a bare `+++` line. A fence-looking line that does
/// not close a parseable block is part of the frontmatter and the scan moves
/// on; the body is returned verbatim minus the trailing newline `append` adds.
fn split_fenced(rest: &str) -> Result<(Frontmatter, String), String> {
    let mut front = String::new();
    let mut first_error: Option<toml::de::Error> = None;
    let mut lines = rest.split_inclusive('\n');
    while let Some(line) = lines.next() {
        if line == "+++\n" || line == "+++" {
            match toml::from_str::<Frontmatter>(&front) {
                Ok(frontmatter) => {
                    let mut body: String = lines.collect();
                    if body.ends_with('\n') {
                        body.pop();
                    }
                    return Ok((frontmatter, body));
                }
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                }
            }
        }
        front.push_str(line);
    }
    Err(first_error.map_or_else(
        || "missing the closing +++ fence".to_owned(),
        |error| error.to_string(),
    ))
}
```

**3k. Add `monotonic_now` beside them, and point `Scribe::record` at it** — the stamp must be process-monotonic so that write order within one command is read order, and the same stamp must feed both `ts` and the filename:

```rust
/// `Timestamp::now`, bumped to be strictly later than every stamp this process
/// has handed out.
///
/// Two draws back-to-back can be equal on a coarse platform clock, and equal
/// stamps would leave read order to the random filename suffix — the sync
/// tests assert the order of entries one command writes, so that order must
/// not be a coin toss. A clock that did not advance is nudged one nanosecond
/// past the last stamp instead. Across processes the wall clock is the order,
/// as it always was; the suffix and `create_new` cover genuinely concurrent
/// writers.
fn monotonic_now() -> jiff::Timestamp {
    static LAST: std::sync::Mutex<Option<jiff::Timestamp>> = std::sync::Mutex::new(None);
    let mut last = LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = jiff::Timestamp::now();
    let stamp = match *last {
        Some(previous) if now <= previous => previous + jiff::SignedDuration::from_nanos(1),
        _ => now,
    };
    *last = Some(stamp);
    stamp
}
```

(`Timestamp + SignedDuration` is jiff's panicking-on-overflow `Add` — unreachable this side of year 9999 — and `SignedDuration::from_nanos` is const; both verified in jiff 0.2.35.)

Then replace `Scribe::record` — one line changes, the `ts` stamp:

```rust
    /// Append `draft`, stamping the fields no caller supplies.
    pub fn record(&self, draft: &Draft<'_>) -> Result<Entry, LedgerError> {
        let entry = Entry {
            ts: monotonic_now().to_string(),
            owner: self.owner.clone(),
            subject: draft.subject.map(str::to_owned),
            kind: draft.kind,
            text: draft.text.clone(),
            evidence: draft.evidence.clone(),
            anchor: self.anchor(draft.subject),
            pr: draft.pr,
        };
        self.ledger.append(&entry)?;
        Ok(entry)
    }
```

Nothing else in the module changes: `Kind`, `Entry`'s fields, `Filter`, `select`, `age`, `Draft`, `Scribe::{new, repo, event, anchor}` (`record` moves to the monotonic clock in 3k) and `Ledger::{for_repo, at, path}` stay byte-for-byte.

- [ ] **Step 4: Run the ledger tests to watch them pass**

Run: `cargo test --lib ledger`
Expected: PASS — all of `mod tests`, including `entries_read_back_in_stamp_order_whatever_the_write_order`, `an_evidence_string_containing_a_fence_line_still_round_trips`, `filenames_carry_the_stamp_so_lexicographic_order_is_chronological`, `concurrent_writers_produce_distinct_files_that_all_parse`, `stamps_drawn_back_to_back_strictly_advance` and `entries_written_back_to_back_read_back_in_write_order`.

- [ ] **Step 5: Delete `StoreLock::acquire_at` in `src/store.rs`**

Its only consumers were `Ledger::append`'s lockfile (gone in Step 3) and `acquire` itself (this PR split it out in Task 1). Restore `acquire` to one inline body — replace the whole `impl StoreLock { ... }` block with:

```rust
impl StoreLock {
    /// Beside the file it guards, named for that file's stem: `state.json` is
    /// guarded by `state.lock`.
    pub(crate) fn acquire(target: &Path) -> Result<Self, StoreError> {
        let path = target.with_extension("lock");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| StoreError::Write {
                path: parent.to_owned(),
                source,
            })?;
        }
        // A short wait, then give up loudly. Blocking forever on a stale lock
        // would be worse than saying so.
        for _ in 0..50 {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(source) => return Err(StoreError::Write { path, source }),
            }
        }
        Err(StoreError::Locked { path })
    }
}
```

Run: `cargo test --lib store`
Expected: PASS. Then `grep -rn "acquire_at" src tests` — Expected: no output.

- [ ] **Step 6: Migrate the integration and CLI fixtures**

Mechanical path changes — a `Ledger` path is now a directory, so the `.jsonl` suffixes come off:

- `src/commands/sync.rs` line ~730: `Ledger::at(temp.path().join("ledger.jsonl"))` → `Ledger::at(temp.path().join("ledger"))`.
- `tests/jj_integration.rs`, twelve sites (lines ~1055, 1157, 1227, 1279, 1323, 1367, 2367, 2445, 2470, 2495, 2551, 2617): every `join("demo.jsonl")` → `join("demo")` and `join("ledger.jsonl")` → `join("ledger")`. No assertion changes: entries one command writes draw stamps from the scribe's monotonic clock (Step 3k), which is strictly increasing within a process, and the assertions that span several commands (for example the track/depends/forget triple) run them sequentially, milliseconds of wall clock apart — the ordered-equality assertions hold as written on both counts.
- `tests/notch_command.rs`, in `an_unreadable_ledger_is_incomplete_and_an_unknown_repo_is_usage`: replace the Given block and the stderr assertion — the exit-3 rule is unchanged ("the ledger exists and cannot be read"), the ledger now being the directory and its entry files, and the error names the file instead of a line:

```rust
    // Given: a ledger directory holding a file that is not an entry
    let ledger = home.path().join("ledger").join("a-repo");
    std::fs::create_dir_all(&ledger).expect("ledger directory");
    std::fs::write(
        ledger.join("20260815T221403.000000000Z-0000.md"),
        "not a ledger entry at all\n",
    )
    .expect("corrupt entry");

    // When / Then: reading it cannot answer, and says so with exit 3
    let broken = knives(
        &home,
        checkout.path(),
        &["--text", "notch", "--repo", "a-repo"],
    );
    assert_eq!(broken.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&broken.stderr).contains("0000.md"),
        "the error must name the entry file; was: {}",
        String::from_utf8_lossy(&broken.stderr)
    );
```

The rest of that test (the unknown-repo half asserting exit 2) is unchanged.

Run: `cargo test --test jj_integration --test notch_command`
Expected: PASS.

- [ ] **Step 7: Run the full gates**

Run: `cargo fmt --check && cargo clippy --all-targets && cargo test`
Expected: no formatting drift, no clippy warning (warnings are failures here), every test green. None of the files this task touches carries a `// allow: SIZE_OK` marker, so there are no line counts to update.

- [ ] **Step 8: Prove the old storage is gone**

Run: `grep -rEn "jsonl|acquire_at|lock_path|JSON-lines" src tests`
Expected: no output (exit 1). The `src/lib.rs` module-map line calling the ledger "an append-only record per repository" stays as is — still true: entry files are immutable and the directory only ever gains files.

---

## Self-review

Run against the spec after every task is complete.

**Spec coverage**

| Spec | Task |
|---|---|
| 1.1 data model: every field, `kind`'s two values, `anchor` never caller-supplied, `pr` from `tracked_pulls`, unknown-field tolerance, no version number | T1 |
| 1.2 storage path, append-only, `<repo>.jsonl.lock` via the `StoreLock` idiom, file order authoritative, no rotation *(superseded 2026-08-16 → T11)* | T1 |
| 1.2 storage (revised 2026-08-16): one markdown file per entry, TOML frontmatter between `+++` fences, atomic `create_new` with loud collision, no lockfile, lexicographic filename chronology, unparseable entry file loud, unknown keys ignored, ~300 bytes and no rotation | T11 |
| 1.3 auto-events: start/claim, finish/release-claim with `--superseded-by`, track's three moods, depends, release cut's parent set, sync's transitions; append failure fails loudly | T5, T6, T7, T8 |
| 1.4 the command: write form, read form, bare last-20, subject chronology, `--pr`, `--repo` both moods, JSON/text, exit 0/2/3 | T4 |
| 1.5 breadcrumb: `notch` in JSON, one token in text, no added runtime | T9 |
| 1.6 skills and docs: fork-work, using-knives, pr-preflight, README, design's past-tense doctrine | T10 |
| 1.7 tests: ledger unit tests (round trip, filters, lock contention — both a held lock and two concurrent appenders proving no lost or interleaved line, unknown fields, missing anchor) | T1, T2 *(lock-contention rows superseded 2026-08-16 → the create_new concurrency test in T11)* |
| 1.7 tests: integration per auto-event with subject, owner and anchor; release cut's parent set; sync via a fake forge | T5, T6, T7, T8 |
| 1.7 tests: CLI both modes, `--repo` from outside, exit codes | T4 |
| 1.7 tests: status `notch` in JSON, one-token text, absent cleanly | T9 |

**Out of scope, and absent:** no task detects unowned release content, compares pins to tips, checks release ref integrity, restyles status text, syncs or backs up the ledger, injects ledger content through a hook, or tracks promise threads against the forge.

**Event truthfulness:** every automatic event records something the command actually did. `finish` writes nothing when it released no claim and recorded no supersession (T5), `track` stamps the number its own entry is about so `notch --pr <n>` finds the event that created the association (T6, asserted end to end), and `sync` writes nothing for a pull request that did not move (T8). No path mutates the store and then skips the ledger: T6's `depends` resolves its registry entry before opening the store for update.

**Placeholders:** none. Every code step carries the code, every run step carries the command and the expected result, and the two spec ambiguities are resolved in Task 4's preamble rather than deferred.

**Type consistency:** `Ledger::at`/`for_repo`/`append`/`entries`/`path`, `Entry`'s eight fields, `Kind::{Event, Note}`, `LedgerError`'s six variants, `Filter`'s three fields, `select`'s `(Vec<&Entry>, usize)`, `newest_for`, `age(&str, Timestamp) -> Option<String>`, `Scribe::{new, repo, record, event}`, `Draft`'s five fields, `notch::{Request, Report, read, render, run}`, `LastNotch::{ts, kind, text}` and `LastNotch::of`, `Options::ledger`, `BranchRow::notch`, `notches_from_ledger`, `notch_cell`, `NOTCH_TEXT`, `add_releases`, `scribe_for`, `transition_text`, `spoken`, and `sync_repo`'s five parameters are spelled identically everywhere they appear above.

## Live-dogfood addendum (2026-08-16)

- `src/commands/notch.rs`, `src/cli.rs`, and `tests/notch_command.rs`: write
  responses contain only `wrote`; `--pr` stamps writes and falls back to
  `tracked_pulls`.
- `src/commands/status.rs` and `tests/jj_integration.rs`: surface repo-level
  notches in JSON and above the text branch table.
- `src/config.rs` and `tests/jj_integration.rs`: infer a registered repository
  from a sibling jj workspace's `.jj/repo` pointer.
- `src/main.rs` and `tests/jj_integration.rs`: record a release cut's
  previous-parent delta without changing the composition it cuts.
- `docs/superpowers/specs/2026-08-15-notch-ledger-design.md` and
  `skills/using-knives/SKILL.md`: document these command and status contracts.
