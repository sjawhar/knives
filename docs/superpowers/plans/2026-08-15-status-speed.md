# Status Speed Implementation Plan (PR 2)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `knives status` stops taking forever — the per-pull-request forge subprocess pairs become one batched query, the per-branch landed probes run concurrently, and `--all` gathers repositories at the same time — while every reported fact, token and exit code stays exactly what it was.

**Architecture:** Three round-trip and concurrency changes, no caching anywhere: the doctrine that nothing derived is stored holds, and the wins are round-trip elimination and concurrency. `Forge` gains one batch method, `pull_details(numbers)`, which `CliForge` answers with `gh repo view` plus one `gh api graphql` call carrying an aliased field per number, and into which the per-number `review_predates_head` and `checks` methods fold entirely. `status`'s branch loop is split so that both dominating phases run over the whole branch list before any row is assembled: the forge in one call, the landed probes on bounded scoped threads that each open their own repository handle. `--all` runs one thread per repository and renders in registry order. Phase timings sit behind `KNIVES_TIMING`, so every claim in this PR is a measured number rather than a belief.

**Tech Stack:** Rust edition 2024 (rust-version 1.90), `std::thread::scope` (no new dependency), clap 4 derive, serde/serde_json, jj-lib =0.43.0 pinned, `gh` for forge access.

**Spec:** `docs/superpowers/specs/2026-08-15-notch-ledger-design.md` — read it first, section "PR 2: status speed" (2.1 to 2.5) and "Out of scope, recorded".

## Global Constraints

Values copied from the spec. Every task's requirements implicitly include this section.

- **No caching anywhere:** "the doctrine holds; the wins are round-trip elimination and concurrency."
- **Baseline first:** "Instrument the phases (release scan / landed probes / forge calls) and record a baseline on a representative repository (36 branches, ~9 open pull requests) and on `--all`. Every subsequent change is judged against these numbers."
- **The instrumentation may ship:** "it does not need to ship, but if it is cheap to keep behind `--verbose` or an env var, keep it."
- **The batch shape is fixed:** "one `gh api graphql` call that fetches, for all our PR numbers at once, the review timeline and `statusCheckRollup`. The `Forge` trait gains a batch method (`pull_details(numbers) -> map`); `CliForge` implements it with the GraphQL query; `FakeForge` implements it from its existing maps; the per-number trait methods fold into it. Roughly 2×N+1 subprocesses become 2."
- **Probe concurrency:** "Each probe is an independent, read-only jj transaction that is dropped, keyed by path + branch + upstream trunk. Run them on bounded scoped threads (`std::thread::scope`; no new dependency unless the codebase already carries one). Each thread opens its own repo handle — verify jj-lib's concurrent read-only open behavior in a test before relying on it; jj's own model is concurrent-safe by design, but the loaded-repo handle is not assumed `Sync`."
- **`--all`:** "Repos are independent by construction; gather them concurrently and render in registry order. Store reads are already snapshot-consistent (one locked read at start)."
- **Verification:** "Measured wall time before and after, on this machine, against the representative repository and `--all`, recorded in the PR body. Not vibes. Correctness: the existing integration suite passes unchanged — batching and parallelism must not alter a single reported fact, token, or exit code."
- **Files the spec names:** `src/forge.rs` (batch call), `src/commands/status.rs` (parallel probes, batched forge use), `src/main.rs` or wherever `--all` iterates repos.
- **Out of scope, and absent from every task below:** unowned-release-content detection at cut time; pin-vs-tip equality per fork; release ref integrity; **status text legibility** — "separate complaint, separate work", so no column is renamed, reordered, widened or dropped here; ledger backup/sync; hook injection of ledger content; per-PR promise-thread tracking against the forge.

Repository constraints:

- **jj, not git.** All version control through `jj`. Never run a git mutation command in this repo.
- **One commit per PR. There are no commit steps in this plan.** Work accumulates in `@`; the coordinator describes it once and owns every push. A task ends when its tests pass.
- **Gates, from the repo root:** `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`. Treat any clippy warning as a failure: `[lints.clippy]` in `Cargo.toml` denies `all`, warns `pedantic` and `nursery`, and denies `unwrap_used`, `expect_used`, `panic`, `indexing_slicing`, `as_underscore`.
- **`clippy.toml` thresholds bind the design:** `too-many-arguments-threshold = 4` (a five-argument function needs an `#[allow(..., reason = "...")]`), `too-many-lines-threshold = 100`, `cognitive-complexity-threshold = 25`. `allow-unwrap-in-tests`, `allow-expect-in-tests`, `allow-panic-in-tests` and `allow-print-in-tests` are true, so `.unwrap()`/`.expect()` are fine inside `#[cfg(test)]` and nowhere else. `clippy::indexing_slicing` is NOT exempt in tests: test modules carry `#![allow(clippy::indexing_slicing, reason = "indexing a result in a test is the assertion; a panic is the failure")]`.
- **Identity guard:** `tests/no_hardcoded_identity.rs` scans `src/`, `plugin/`, `docs/`, `skills/`, `hooks/` for a forge host written with a trailing slash and for project-family literals. This plan file lives under `docs/`, so those literals stay out of it too; the recorded GraphQL fixture goes under `tests/fixtures/`, which is not scanned, and is scrubbed the same way `tests/fixtures/gh_pr_list.json` is.
- **House style:** doc comments state current behavior and the failure that motivated it, never history. Test names are sentences. Given/When/Then comments in tests. Rendering stays pure — every command builds a `String` and one call site prints it; the timing line is the one thing written to stderr, and it is written from `main.rs`.
- **`// allow: SIZE_OK: <n> lines - <reason>` marker** sits at the top of `src/main.rs` (2050). That file grows here; update the count to the new `wc -l src/main.rs`.

## Hardening ledger

_(empty at the start; the coordinator records hardening findings here as they are resolved)_

---

## Merge order and the overlap with PR 1

**PR 1 (`docs/superpowers/plans/2026-08-15-notch-ledger.md`) merges first.** This plan is written against `src/commands/status.rs` **as PR 1 leaves it**, which means:

- `struct BranchRow` has a `notch: Option<LastNotch>` field. Every row literal below sets it. Do not remove it.
- `struct Options` has a `ledger: Option<&'a Ledger>` field. Every `Options` literal below sets it, and this plan adds a second field, `workers: usize`.
- `fn add_releases(report, repo, tips, entry)` exists, extracted from `gather` by PR 1. This plan depends on that extraction for its line budget: `gather` was 103 non-comment lines against a hundred-line clippy threshold, PR 1's extraction takes it to about 88, and Task 1 here extracts the branch loop as well.
- `fn branch_table` renders nine columns. **This plan does not touch it**, or any other rendering function: status text legibility is out of scope.
- The imports at the top of the file read `use crate::ledger::{Entry as Notch, Ledger, newest_for};`. `Notch` below is that alias — a ledger entry — and `LastNotch::of(&Notch)` is PR 1's constructor for the row field.

### Functions this PR touches in `src/commands/status.rs`

- `struct Options` — one field added (`workers`). PR 1 added `ledger` to the same struct; the rebase adds two lines to one struct.
- `fn gather` — **touched once, in Task 1**: it becomes a three-line delegation to a new `gather_timed`, which holds what `gather` holds today plus the phase clocks, minus the branch loop.
- `fn branch_rows` and `struct RowInput` — new in Task 1, holding the branch loop that `gather` holds today. Tasks 3 and 5 change only this function, not `gather`.
- `fn review_predates_head_for`, `fn checks_for` — removed in Task 3, replaced by `review_predates_head_from` and `checks_from`, which read the batch map instead of calling the forge.
- `fn landed_verdict` — **unchanged**, including its signature: it still takes `&Options`, which is why Task 2's `Forge: Send + Sync` matters.
- New: `struct Timings`, `fn timing_enabled`, `fn gather_timed`, `type CarriedPull`, `fn carried_pulls`, `fn detail_numbers`, `fn pull_details_from_forge`, `fn review_predates_head_from`, `fn checks_from`, `fn landed_verdicts`.

The whole overlap with PR 1 is therefore `struct Options` and `fn gather`, and in `gather` it is one extraction that carries PR 1's two added lines (`notches_from_ledger` and `notches: &notches`) through unchanged while moving the `notch` line into `branch_rows` with the rest of the loop.

### Task dependencies

```
T1 baseline + Timings ──────────────────────────────┐
T4 concurrent-open verification ────────────────────┤
T2 Forge::pull_details ──> T3 status batches ──> T5 parallel probes ──> T6 parallel --all ──> T7 measured verification
```

- **T1** and **T4** are independent of everything and of each other — two parallel lanes that can start immediately. T1 must finish before the baseline numbers mean anything, so run it first in its lane.
- **T4** is a capability probe. If it fails, stop and report: the whole of T5 rests on it.
- **T2 → T3 → T5 → T6** is a chain. T3 and T5 both restructure `branch_rows`, so they cannot run in parallel; T6 sets the `workers` field T5 introduces.
- **T7** is last and needs all of them.

---

### Task 1: Phase timings, and the baseline they exist to record

**Files:**
- Modify: `src/commands/status.rs`
- Modify: `src/main.rs` (`run_status` around line 1462)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `add_releases`, `notches_from_ledger`, `LastNotch`, `Options::ledger` (all from PR 1).
- Produces:
  - `pub struct Timings { pub releases: Duration, pub forge: Duration, pub probes: Duration, pub total: Duration }` with `pub fn line(&self, repo: &str) -> String`
  - `pub fn timing_enabled() -> bool`
  - `pub fn gather_timed(name: &RepoName, entry: &RepoEntry, store: &Store, options: &Options<'_>) -> anyhow::Result<(Report, Timings)>`
  - `pub fn gather(...) -> anyhow::Result<Report>` — unchanged signature, now a delegation
  - `struct RowInput<'a>` with fields `name, entry, repo, tips, store, options, branches, pull_requests, notches, upstream_trunk`
  - `fn branch_rows(input: RowInput<'_>, report: &mut Report, timings: &mut Timings) -> anyhow::Result<Vec<String>>` — returns the branches whose landed state could not be judged

An environment variable rather than `--verbose`, because `--verbose` already means something on this command: it selects one line per finding instead of one per kind. Timings are not a report field either — they measure this run rather than describing the repository, and putting them in the JSON would change the contract for every reader who did not ask.

- [ ] **Step 1: Write the failing tests.** In `src/commands/status.rs`'s `mod tests`:

```rust
    #[test]
    fn a_timing_line_names_every_phase_it_measured() {
        // The numbers this PR is judged against. A line that reported only a total
        // could not say which phase a change actually moved.
        let timings = Timings {
            releases: std::time::Duration::from_millis(12),
            forge: std::time::Duration::from_millis(3400),
            probes: std::time::Duration::from_millis(8100),
            total: std::time::Duration::from_millis(11_600),
        };
        let line = timings.line("a-repo");
        assert!(line.contains("a-repo"), "was: {line}");
        assert!(line.contains("releases 12ms"), "was: {line}");
        assert!(line.contains("forge 3400ms"), "was: {line}");
        assert!(line.contains("probes 8100ms"), "was: {line}");
        assert!(line.contains("total 11600ms"), "was: {line}");
    }
```

and in `tests/jj_integration.rs`:

```rust
#[test]
fn a_measured_gather_reports_the_same_report_and_a_total_that_covers_its_phases() {
    // Given: a fork with branches to probe and releases to scan
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.octopus("release/2026-08-15", "feat/alpha", "feat/beta");
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
    let options = || knives::commands::status::Options {
        probe: true,
        forge: None,
        registry: None,
        ledger: None,
    };

    // When: the same repository is gathered with and without measurement
    let plain = status::gather(&name, &entry, &store, &options()).expect("gather");
    let (measured, timings) =
        status::gather_timed(&name, &entry, &store, &options()).expect("gather_timed");

    // Then: the report is the same one, and the total covers the phases it timed
    assert_eq!(
        status::render(&plain, true),
        status::render(&measured, true),
        "measuring changed the report"
    );
    assert!(
        timings.total >= timings.releases + timings.probes,
        "total {:?} does not cover releases {:?} plus probes {:?}",
        timings.total,
        timings.releases,
        timings.probes
    );
    assert!(
        timings.probes > std::time::Duration::ZERO,
        "two branches were probed and the probe phase measured nothing"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib status::tests::a_timing_line && cargo test --test jj_integration a_measured_gather_reports`
Expected: FAIL — `cannot find struct 'Timings'`, `cannot find function 'gather_timed'`.

- [ ] **Step 3: Add `Timings` and `timing_enabled`** in `src/commands/status.rs`, immediately after `struct Options`:

```rust
/// Where a status run spent its time.
///
/// Not a report field: it measures this run rather than describing the
/// repository, and every number would change the JSON contract for readers who
/// did not ask. Printed to stderr when `KNIVES_TIMING` is set — an environment
/// variable rather than `--verbose`, because that flag already selects how
/// findings are grouped.
#[derive(Debug, Default, Clone, Copy)]
pub struct Timings {
    /// Scanning releases for stale parents.
    pub releases: std::time::Duration,
    /// Every forge round trip: the pull request list, and the per-pull-request
    /// details.
    pub forge: std::time::Duration,
    /// Replaying branches onto the upstream trunk.
    pub probes: std::time::Duration,
    pub total: std::time::Duration,
}

impl Timings {
    pub fn line(&self, repo: &str) -> String {
        format!(
            "timing {repo}: releases {}ms forge {}ms probes {}ms total {}ms",
            self.releases.as_millis(),
            self.forge.as_millis(),
            self.probes.as_millis(),
            self.total.as_millis()
        )
    }
}

/// Whether phase timings were asked for.
pub fn timing_enabled() -> bool {
    std::env::var_os("KNIVES_TIMING").is_some()
}
```

- [ ] **Step 4: Extract the branch loop** into `branch_rows`. Add, immediately before `pub fn gather`:

```rust
/// Everything the branch table needs from one repository.
struct RowInput<'a> {
    name: &'a RepoName,
    entry: &'a RepoEntry,
    repo: &'a Repo,
    tips: &'a BookmarkTips,
    store: &'a Store,
    options: &'a Options<'a>,
    branches: Vec<(BranchName, CommitId)>,
    pull_requests: &'a BTreeMap<BranchName, PullRequest>,
    notches: &'a [Notch],
    upstream_trunk: &'a str,
}

/// The branch rows, and the branches whose landed state could not be judged.
///
/// Extracted from `gather` because the two phases that dominate a status run —
/// the forge round trips and the landed probes — are driven over the whole branch
/// list, and one function that both drives them and assembles the rest of a
/// report is past what a reviewer holds at once.
fn branch_rows(
    input: RowInput<'_>,
    report: &mut Report,
    timings: &mut Timings,
) -> anyhow::Result<Vec<String>> {
    let mut unjudged: Vec<String> = Vec::new();
    for (branch, tip) in input.branches {
        let pull_request = pull_request_for(&branch, input.pull_requests);
        let phase = std::time::Instant::now();
        let review_predates_head =
            review_predates_head_for(input.options.forge, input.entry, pull_request.as_ref(), report);
        let checks = checks_for(input.options.forge, input.entry, pull_request.as_ref(), report);
        timings.forge += phase.elapsed();
        let origin_tip = input
            .tips
            .get(&BookmarkRef::Remote {
                branch: branch.clone(),
                remote: crate::ids::RemoteName::new("origin"),
            })
            .cloned();
        let origin_relation = record_origin_relation(
            report,
            &branch,
            relation_to_origin(input.repo, &tip, origin_tip.as_ref()),
        );
        let phase = std::time::Instant::now();
        let landed = landed_verdict(
            &input.entry.path,
            &branch,
            (&tip, origin_tip.as_ref()),
            input.options,
            input.upstream_trunk,
        )?;
        timings.probes += phase.elapsed();
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let target = BranchTarget::new(input.name.clone(), branch.clone());
        let stated_pull = stated_pull_for(&target, input.store, input.entry, input.options);
        let notch = newest_for(input.notches, branch.as_str()).map(LastNotch::of);
        report.branches.push(BranchRow {
            name: branch,
            tip: Some(tip),
            origin_tip,
            origin_relation,
            pull_request,
            landed,
            review_predates_head,
            checks,
            fork_only: input.store.is_fork_only(&target),
            stated_pull,
            notch,
        });
    }
    Ok(unjudged)
}
```

- [ ] **Step 5: Replace `gather` with `gather_timed` plus a delegation.** Replace the whole of `pub fn gather` with:

```rust
/// The report, and where the run spent its time.
///
/// One function rather than two paths, so a measured run and an unmeasured one
/// cannot drift: `gather` is this with the measurement dropped.
pub fn gather_timed(
    name: &RepoName,
    entry: &RepoEntry,
    store: &Store,
    options: &Options<'_>,
) -> anyhow::Result<(Report, Timings)> {
    let started = std::time::Instant::now();
    let mut timings = Timings::default();
    let repo = Repo::open(&entry.path)?;
    let mut report = Report {
        repo: name.to_string(),
        ..Report::default()
    };
    let tips = repo.bookmark_tips()?;
    record_repository_health(&mut report, &repo, &entry.path, &tips)?;
    let trunk = entry.trunk();
    let scheme = entry.release_scheme();
    let upstream_trunk = entry.upstream_trunk();

    let phase = std::time::Instant::now();
    add_releases(&mut report, &repo, &tips, entry)?;
    timings.releases = phase.elapsed();

    let (branches, fetched_heads) = maintained_branches(&tips, trunk, &scheme);
    let notches = notches_from_ledger(options.ledger, &mut report);
    note_fetched_heads(&mut report, fetched_heads);
    let phase = std::time::Instant::now();
    let pull_requests = pull_requests_from_forge(options.forge, entry, &mut report);
    timings.forge = phase.elapsed();

    let unjudged = branch_rows(
        RowInput {
            name,
            entry,
            repo: &repo,
            tips: &tips,
            store,
            options,
            branches,
            pull_requests: &pull_requests,
            notches: &notches,
            upstream_trunk: &upstream_trunk,
        },
        &mut report,
        &mut timings,
    )?;

    report.branches.extend(divergent_rows(&DivergentInput {
        repo: &repo,
        tips: &tips,
        name,
        entry,
        store,
        options,
        pull_requests: &pull_requests,
        notches: &notches,
    })?);
    report
        .branches
        .sort_by(|left, right| left.name.cmp(&right.name));
    report
        .findings
        .extend(carried_findings(&report, &repo, trunk, &scheme)?);
    add_branch_overlap_findings(&mut report, entry);

    add_claims(&mut report, &repo, name, store);

    let derived = branch_findings(&report.branches);
    report.findings.extend(derived);
    report
        .findings
        .extend(wrong_base_findings(&report.branches, entry.default_base()));
    report.problems.extend(unjudged_note(&unjudged));
    add_dependency_findings(&mut report, name, store, options);
    timings.total = started.elapsed();
    Ok((report, timings))
}

pub fn gather(
    name: &RepoName,
    entry: &RepoEntry,
    store: &Store,
    options: &Options<'_>,
) -> anyhow::Result<Report> {
    gather_timed(name, entry, store, options).map(|(report, _)| report)
}
```

- [ ] **Step 6: Print the line from `main.rs`.** In `run_status`, replace the loop body's gather-and-print with:

```rust
    for (name, entry) in chosen {
        let ledger = knives::ledger::Ledger::for_repo(&name);
        let (report, timings) = status::gather_timed(
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
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            if !first {
                println!();
            }
            first = false;
            println!("{}", status::render(&report, verbose));
        }
        // stderr, so a timed run's stdout is still the report a script parses.
        if status::timing_enabled() {
            eprintln!("{}", timings.line(name.as_str()));
        }
        worst = worst.worst(status::exit_for(&report));
    }
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --lib status:: && cargo test --test jj_integration a_measured_gather_reports`
Expected: PASS.

- [ ] **Step 8: Confirm nothing regressed and the line budget holds**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS, no clippy output. In particular no `clippy::too_many_lines` on `gather_timed` (about 45 non-comment lines) or `branch_rows` (about 45).

- [ ] **Step 9: Record the baseline.** Build once in release, because a debug build makes jj-lib's index work dominate everything and the ratios stop meaning anything.

Run: `cargo build --release`

Then find the largest managed fork and measure it. The spec measured a representative repository (36 branches, ~9 open pull requests); confirm what this machine actually has and report whichever you measured:

Run: `./target/release/knives repos`

From inside that repository's checkout:

```bash
cd <the checkout knives repos printed>
gh auth status
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --json > /dev/null
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --json --no-github > /dev/null
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --all --json > /dev/null
~/knives/default/target/release/knives status --text > /tmp/knives-before.txt
echo "exit $?" >> /tmp/knives-before.txt
```

Then keep the binary itself, so Task 7 can re-measure the before and after back to back against one live forge rather than minutes apart:

```bash
cp ~/knives/default/target/release/knives /tmp/knives-baseline-bin
```

The report, the exit code and the binary are the three things Task 7 compares against. Without them the final real-surface comparison has nothing to compare to, and an upstream change between the two runs — a review landing, CI turning green — is indistinguishable from a regression this PR caused.

Expected: three timing lines per repository on stderr plus the shell's `real` figures, and `/tmp/knives-before.txt` holding the report followed by its `exit <n>` line. Record, verbatim, for Task 7 to compare against and for the PR body: the repository name, its branch count and open pull request count (`status --json` gives both), and for each of the three invocations the `real` wall time and the four phase numbers. If `gh auth status` fails, say so immediately and record the `--no-github` numbers only — that is a credential this plan cannot supply, and the probe half of the work is still measurable.

---

### Task 2: `Forge::pull_details` — one round trip for every pull request

**Files:**
- Modify: `src/forge.rs`
- Modify: `src/commands/sync.rs` (the two test forges, around lines 507–595, and their six constructions)
- Modify: `tests/jj_integration.rs` (`StateUnavailableForge`, lines 27–70)
- Modify: `tests/forge_contract.rs`
- Create: `tests/fixtures/gh_pull_details.json`

**Interfaces:**
- Produces:
  - `pub struct PullDetails { pub review_predates_head: Option<bool>, pub checks: Option<ChecksSummary> }` — `Debug, Clone, Default, PartialEq, Eq`
  - `pub trait Forge: Send + Sync` with `fn pull_details(&self, repo: &Path, numbers: &[u64]) -> Result<BTreeMap<u64, PullDetails>, ForgeError>` replacing `review_predates_head` and `checks`
  - `pub fn pull_details_query(numbers: &[u64]) -> String`
  - `pub fn parse_pull_details(payload: &str) -> Result<BTreeMap<u64, PullDetails>, ForgeError>`
  - `pub fn parse_repo_target(payload: &str) -> Result<(String, String), ForgeError>`
  - `ForgeError::{Target, Query}` — two new variants
- Removed, with every caller migrated: `Forge::review_predates_head`, `Forge::checks`, `pub fn parse_checks`, `pub fn compare_review_to_head`, `struct CheckRollup`, `struct ReviewAges`.

`Send + Sync` on the trait is what lets `Options` cross a thread boundary in Tasks 5 and 6. All five implementors already satisfy it except `ErroringForge`, whose `RefCell<Vec<u64>>` becomes a `Mutex<Vec<u64>>` here.

Two subprocesses, not one: `gh api graphql` has no repository context of its own, so the owner and name come from `gh repo view`, resolved exactly the way `gh pr list` resolves the repository today. That is the "2×N+1 subprocesses become 2" the spec predicts.

- [ ] **Step 1: Write the failing tests.** Replace, in `src/forge.rs`'s `mod tests`, the six tests that exercised the removed decoders — `a_failing_check_is_told_from_one_that_never_ran`, `an_error_status_context_is_not_silently_treated_as_still_running`, `an_omitted_rollup_means_checks_were_asked_for_but_nothing_ran`, `a_review_older_than_the_newest_commit_is_stale`, `a_review_newer_than_the_newest_commit_is_current`, `nothing_to_compare_is_none_rather_than_false` — and the two `FakeForge` tests, with these:

```rust
    /// The batch reply's shape, with one pull request per aliased field.
    fn details_payload(entries: &str) -> String {
        format!("{{\"data\":{{\"repository\":{{{entries}}}}}}}")
    }

    #[test]
    fn a_failing_check_is_told_from_one_that_never_ran_and_from_one_not_asked_about() {
        // Three states, and conflating any two of them misreports a pull request:
        // red CI, green-or-nothing-ran, and never consulted.
        let payload = details_payload(
            "\"p11\":{\"number\":11,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":\
             {\"contexts\":{\"nodes\":[\
             {\"__typename\":\"CheckRun\",\"conclusion\":\"FAILURE\",\"name\":\"build\"},\
             {\"__typename\":\"CheckRun\",\"conclusion\":\"SUCCESS\",\"name\":\"lint\"}]}}}}]},\
             \"p12\":{\"number\":12,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":null}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        let failing = details[&11].checks.as_ref().expect("consulted");
        assert!(failing.failing(), "a FAILURE conclusion is failing");
        assert_eq!(failing.failed_names(), vec!["build".to_owned()]);
        assert!(failing.ran());
        let quiet = details[&12].checks.as_ref().expect("consulted");
        assert!(!quiet.failing(), "an absent rollup is not a failure");
        assert!(!quiet.ran(), "an absent rollup means nothing ran");
        assert!(
            !details.contains_key(&13),
            "a number the reply did not carry is not consulted, not empty"
        );
    }

    #[test]
    fn an_error_status_context_is_not_silently_treated_as_still_running() {
        // External CI posting commit statuses reports an aborted build this way, and
        // missing it made a red pull request read as clean green.
        let payload = details_payload(
            "\"p11\":{\"number\":11,\"rollup\":{\"nodes\":[{\"commit\":{\"statusCheckRollup\":\
             {\"contexts\":{\"nodes\":[{\"__typename\":\"StatusContext\",\"context\":\"legacy-ci\",\
             \"state\":\"ERROR\"}]}}}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        let checks = details[&11].checks.as_ref().expect("consulted");
        assert!(checks.failing(), "an ERROR state is failing");
        assert_eq!(checks.failed_names(), vec!["legacy-ci".to_owned()]);
    }

    #[test]
    fn a_review_is_stale_current_or_incomparable_and_the_three_stay_distinct() {
        // A review four days older than the branch head sent an agent to rewrite
        // already-fixed code. "No review exists" is not "the review is current".
        let payload = details_payload(
            "\"p1\":{\"number\":1,\"reviews\":{\"nodes\":[{\"submittedAt\":\"2026-07-01T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}},\
             \"p2\":{\"number\":2,\"reviews\":{\"nodes\":[{\"submittedAt\":\"2026-07-03T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}},\
             \"p3\":{\"number\":3,\"reviews\":{\"nodes\":[]},\
             \"commits\":{\"nodes\":[{\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}}",
        );

        let details = parse_pull_details(&payload).expect("parse");

        assert_eq!(details[&1].review_predates_head, Some(true));
        assert_eq!(details[&2].review_predates_head, Some(false));
        assert_eq!(details[&3].review_predates_head, None);
    }

    #[test]
    fn the_newest_review_and_the_newest_commit_decide_it_rather_than_the_last_listed() {
        // The reply's node order is the forge's business, not ours.
        let payload = details_payload(
            "\"p1\":{\"number\":1,\"reviews\":{\"nodes\":[\
             {\"submittedAt\":\"2026-07-05T00:00:00Z\"},{\"submittedAt\":\"2026-07-01T00:00:00Z\"}]},\
             \"commits\":{\"nodes\":[\
             {\"commit\":{\"committedDate\":\"2026-07-04T00:00:00Z\"}},\
             {\"commit\":{\"committedDate\":\"2026-07-02T00:00:00Z\"}}]}}",
        );
        assert_eq!(
            parse_pull_details(&payload).expect("parse")[&1].review_predates_head,
            Some(false)
        );
    }

    #[test]
    fn a_query_the_forge_rejected_is_an_error_rather_than_an_empty_answer() {
        // A partial answer that read as "nothing to compare" would render a red
        // pull request as clean, which is the whole failure class this raises for.
        let payload =
            "{\"data\":null,\"errors\":[{\"message\":\"Could not resolve to a Repository\"}]}";
        let error = parse_pull_details(payload).expect_err("errors must not be swallowed");
        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");
        assert!(
            error.to_string().contains("Could not resolve"),
            "was: {error}"
        );
    }

    #[test]
    fn a_reply_with_neither_errors_nor_a_repository_answers_nothing_loudly() {
        // The silent-fallback shape: no errors, no data, so every requested fact
        // would come back absent and every red pull request would read as clean.
        let error = parse_pull_details("{\"data\":{}}")
            .expect_err("a reply about nothing is not an answer");
        assert!(matches!(&error, ForgeError::Query { .. }), "was: {error}");

        // But a repository that answered `null` for a number it does not have IS
        // an answer: that number was not consulted, and the boundary between the
        // two cases is the whole point.
        let present = parse_pull_details("{\"data\":{\"repository\":{\"p9\":null}}}")
            .expect("a repository that resolved is an answer");
        assert!(present.is_empty());
    }

    #[test]
    fn the_batch_query_asks_about_every_number_and_nothing_else() {
        let query = pull_details_query(&[1157, 4545]);
        assert!(query.contains("pullRequest(number: 1157)"), "was: {query}");
        assert!(query.contains("pullRequest(number: 4545)"), "was: {query}");
        assert!(query.contains("statusCheckRollup"), "was: {query}");
        assert!(query.contains("submittedAt"), "was: {query}");
        assert!(query.contains("committedDate"), "was: {query}");
        // Every entry repeats its own number, so alias names are not load-bearing.
        assert!(query.contains("number"), "was: {query}");
    }

    #[test]
    fn a_repository_the_forge_will_not_split_into_owner_and_name_is_an_error() {
        assert_eq!(
            parse_repo_target("{\"nameWithOwner\":\"our-org/some-repo\"}").expect("split"),
            ("our-org".to_owned(), "some-repo".to_owned())
        );
        let error = parse_repo_target("{\"nameWithOwner\":\"bare\"}")
            .expect_err("a name with no owner cannot be queried");
        assert!(matches!(&error, ForgeError::Target { .. }), "was: {error}");
    }

    #[test]
    fn the_fake_answers_a_review_only_for_a_pull_request_it_knows() {
        let fake = FakeForge::default();
        let details = fake.pull_details(Path::new("/tmp"), &[7]).expect("details");
        assert_eq!(details[&7].review_predates_head, None);
        assert_eq!(details[&7].checks, None);
    }

    #[test]
    fn the_fake_reports_checks_only_when_they_were_supplied() {
        // Given: one pull request with a returned check rollup
        let checks = ChecksSummary {
            runs: vec![CheckRun {
                name: "build".to_owned(),
                conclusion: "FAILURE".to_owned(),
            }],
        };
        let fake = FakeForge {
            checks: BTreeMap::from([(7, checks.clone())]),
            ..FakeForge::default()
        };

        // When: both are asked about in one call
        let details = fake
            .pull_details(Path::new("/tmp"), &[7, 8])
            .expect("details");

        // Then: unknown means not consulted, not an empty rollup
        assert_eq!(details[&7].checks, Some(checks));
        assert_eq!(details[&8].checks, None);
    }

    #[test]
    fn the_fake_reports_a_stale_review_for_a_pull_request_it_knows_is_stale() {
        let mut pull_requests = BTreeMap::new();
        let _ = pull_requests.insert(
            BranchName::new("feat/alpha"),
            PullRequest {
                number: 7,
                ..PullRequest::default()
            },
        );
        let fake = FakeForge {
            pull_requests,
            stale_reviews: vec![7],
            ..FakeForge::default()
        };
        let details = fake.pull_details(Path::new("/tmp"), &[7]).expect("details");
        assert_eq!(details[&7].review_predates_head, Some(true));
    }
```

Then, in `tests/forge_contract.rs`, replace the `use` line and the `recorded_check_rollups_match_the_per_pull_request_decoder` test:

```rust
use knives::forge::{
    parse_pull_details, parse_pull_requests, pull_details_query, pull_request_list_args,
    requested_fields,
};
```

```rust
#[test]
fn recorded_check_rollups_match_the_batch_decoder() {
    // Given: every recorded list payload rollup, including its forge-only fields,
    // reshaped as the batch reply carries them
    let recorded: Vec<Value> = serde_json::from_str(RECORDED).expect("recorded JSON is valid");
    let mut saw_empty = false;

    // When: each is decoded through the batch parser
    for (index, pull_request) in recorded.iter().enumerate() {
        let rollup = pull_request
            .get("statusCheckRollup")
            .expect("recorded pull request has a rollup");
        let payload = serde_json::json!({"data": {"repository": {
            "p0": {
                "number": index + 1,
                "rollup": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": rollup}}}}]}
            }
        }}})
        .to_string();
        let details = parse_pull_details(&payload).expect("recorded rollup must deserialise");
        let checks = details
            .values()
            .next()
            .and_then(|detail| detail.checks.clone())
            .expect("a decoded pull request was consulted");
        saw_empty |= checks.runs.is_empty();
        assert!(
            !checks.failing(),
            "recorded rollup must not be falsely classified as failing: {checks:?}"
        );
    }

    // Then: the empty rollup stays a consulted, never-ran result
    assert!(saw_empty, "the recording includes an empty rollup");

    // StatusContext is the variant this recording lacks. Reusing one recorded
    // CheckRun keeps its real unknown fields while proving an in-flight conclusion
    // remains non-failing.
    let mut in_flight = recorded
        .first()
        .and_then(|pull_request| pull_request.get("statusCheckRollup"))
        .and_then(Value::as_array)
        .and_then(|rollup| rollup.first())
        .cloned()
        .expect("recorded pull request has a check run");
    *in_flight
        .as_object_mut()
        .and_then(|check| check.get_mut("conclusion"))
        .expect("recorded check run has a conclusion") = Value::String(String::new());
    let payload = serde_json::json!({"data": {"repository": {
        "p0": {
            "number": 1,
            "rollup": {"nodes": [{"commit": {"statusCheckRollup": {"contexts": {"nodes": [in_flight]}}}}]}
        }
    }}})
    .to_string();
    let checks = parse_pull_details(&payload)
        .expect("in-flight rollup must deserialise")
        .remove(&1)
        .and_then(|detail| detail.checks)
        .expect("consulted");
    assert!(checks.ran());
    assert!(!checks.failing());
}

#[test]
fn a_recorded_batch_payload_decodes_every_field_the_query_asks_for() {
    // The defect this prevents: a query field added and the decoder not, so the
    // report reads "nothing to compare" forever while the forge answered.
    const RECORDED_DETAILS: &str = include_str!("fixtures/gh_pull_details.json");
    let details = parse_pull_details(RECORDED_DETAILS).expect("recorded batch output decodes");
    assert!(!details.is_empty(), "the fixture must carry pull requests");
    assert!(
        details
            .values()
            .any(|detail| detail.review_predates_head.is_some()),
        "no recorded pull request had a review to compare: {details:?}"
    );
    assert!(
        details
            .values()
            .any(|detail| detail.checks.as_ref().is_some_and(|checks| checks.ran())),
        "no recorded pull request had checks: {details:?}"
    );
    // And: the query and the recording describe the same reply.
    let query = pull_details_query(&[1]);
    for field in ["submittedAt", "committedDate", "statusCheckRollup", "number"] {
        assert!(query.contains(field), "the query dropped {field}");
        assert!(RECORDED_DETAILS.contains(field), "the recording lacks {field}");
    }
}

/// Prints the batch query so a real reply can be recorded into
/// `tests/fixtures/gh_pull_details.json`. Re-run it whenever the query changes:
/// the fixture is only a contract while it is the forge's own answer to this
/// exact query.
#[test]
#[ignore = "a recording tool, not a check; see the status-speed plan's verification task"]
fn print_the_batch_query() {
    println!("{}", pull_details_query(&[1, 2]));
}
```

and create `tests/fixtures/gh_pull_details.json` as a hand-shaped reply, so the test above is a real gate from this task onward. Task 7 replaces it with a recorded one. Every field the query asks for appears here, including `statusCheckRollup` with both context variants:

```json
{
  "data": {
    "repository": {
      "p1128": {
        "number": 1128,
        "reviews": {
          "nodes": [
            { "submittedAt": "2026-07-28T09:12:44Z" },
            { "submittedAt": "2026-07-29T11:02:03Z" }
          ]
        },
        "commits": {
          "nodes": [
            { "commit": { "committedDate": "2026-07-27T18:41:10Z" } },
            { "commit": { "committedDate": "2026-07-30T02:20:55Z" } }
          ]
        },
        "rollup": {
          "nodes": [
            {
              "commit": {
                "statusCheckRollup": {
                  "contexts": {
                    "nodes": [
                      { "__typename": "CheckRun", "name": "build", "conclusion": "SUCCESS" },
                      { "__typename": "StatusContext", "context": "legacy-ci", "state": "SUCCESS" }
                    ]
                  }
                }
              }
            }
          ]
        }
      },
      "p1124": {
        "number": 1124,
        "reviews": { "nodes": [] },
        "commits": {
          "nodes": [{ "commit": { "committedDate": "2026-07-29T19:44:19Z" } }]
        },
        "rollup": {
          "nodes": [{ "commit": { "statusCheckRollup": null } }]
        }
      }
    }
  }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --lib forge:: && cargo test --test forge_contract`
Expected: FAIL — `cannot find function 'parse_pull_details'`, `cannot find function 'pull_details_query'`, `no method named 'pull_details'`.

- [ ] **Step 3: Add the type, the two error variants and the trait method** in `src/forge.rs`. Add after `impl PullRequest`'s block:

```rust
/// What one round trip answers about a pull request beyond its list fields.
///
/// A number the forge did not answer for is absent from the map rather than
/// present with defaults: "not consulted" and "nothing to compare" are different
/// facts, and rendering the first as the second reports a red pull request as
/// clean.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullDetails {
    /// Whether the newest review predates the newest commit. `None` means there
    /// was nothing to compare, which must never render as "the review is current".
    pub review_predates_head: Option<bool>,
    /// What the forge's checks say. `None` means the forge reported no rollup for
    /// this pull request at all.
    pub checks: Option<ChecksSummary>,
}
```

Add to `enum ForgeError`:

```rust
    /// The forge named a repository that cannot be split into owner and name.
    #[error("the forge reported the repository as `{named}`, which is not `<owner>/<name>`")]
    Target { named: String },
    /// The forge answered with errors instead of data. Raised rather than read as
    /// "no details": a partial answer that reads as "nothing to compare" would
    /// render a red pull request as clean.
    #[error("the forge rejected the query: {detail}")]
    Query { detail: String },
```

Replace the trait's declaration line and its two per-number methods:

```rust
/// The pull request half of a hosting service.
///
/// `Send + Sync` because `status` gathers repositories concurrently and probes
/// branches on scoped threads, and both share one forge.
pub trait Forge: Send + Sync {
    /// Pull requests in every state, indexed by head branch name.
    fn pull_requests(&self, repo: &Path) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError>;

    /// Review age and check state for many pull requests in one round trip.
    ///
    /// This replaces a per-pull-request pair — a review-timeline query and a check
    /// rollup query — each of which cost a process spawn plus an HTTPS round trip.
    /// A repository with nine open pull requests spent eighteen serial calls where
    /// one query now answers, and that was most of what made `status` slow. The
    /// rollup is asked for here rather than in the list query because there it
    /// exceeds the forge's GraphQL budget and fails the whole call.
    ///
    /// A number the forge does not answer for is absent from the map. Callers
    /// must keep that distinct from an empty answer.
    fn pull_details(
        &self,
        repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError>;

    /// The state of one pull request by number, whatever that state is.
    ///
    /// Resolving a tracked number absent from the pull request list is the only way to tell
    /// "merged" from "closed" from "we stopped tracking it", and those need different actions.
    /// Called only for the few that vanished, so the common run costs one query.
    fn pull_request_state(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError>;

    fn newest_comment(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError>;
}
```

- [ ] **Step 4: Implement it for `CliForge`.** In `impl Forge for CliForge`, replace `review_predates_head` and `checks` with:

```rust
    fn pull_details(
        &self,
        repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
        if numbers.is_empty() {
            return Ok(BTreeMap::new());
        }
        // Two subprocesses, not one: the GraphQL endpoint has no repository
        // context of its own, so the owner and name come from the same resolution
        // `gh pr list` uses — whatever the remotes and the resolved-repository
        // markers say, rather than a second guess of our own.
        let named = Self::run(repo, &["repo", "view", "--json", "nameWithOwner"])?;
        let (owner, name) = parse_repo_target(&named)?;
        let payload = Self::run(
            repo,
            &[
                "api",
                "graphql",
                "-f",
                &format!("owner={owner}"),
                "-f",
                &format!("name={name}"),
                "-f",
                &format!("query={}", pull_details_query(numbers)),
            ],
        )?;
        parse_pull_details(&payload)
    }
```

- [ ] **Step 5: Add the query builder and the parsers.** Replace `struct CheckRollup` and `pub fn parse_checks` with the query builder and the target parser, and replace `struct ReviewAges` and `pub fn compare_review_to_head` with the batch parser. `struct Dated` and `struct Committed` stay — the batch reply carries the same two fields under the same names.

```rust
/// One aliased field per number, so the reply carries exactly the pull requests
/// asked about and nothing else.
///
/// Alias names are not load-bearing: every entry repeats its own `number` and the
/// parser keys on that, so a forge that normalises aliases cannot silently
/// reassign a rollup to the wrong pull request. `commits(last: 1)` is where the
/// rollup lives — a pull request has no rollup of its own, only its head commit
/// does — and the connections are bounded because an unbounded one is a rejected
/// query rather than a slow one.
pub fn pull_details_query(numbers: &[u64]) -> String {
    let fields: String = numbers
        .iter()
        .map(|number| format!("p{number}: pullRequest(number: {number}) {{ ...details }}"))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "query($owner: String!, $name: String!) {{ \
         repository(owner: $owner, name: $name) {{ {fields} }} }} \
         fragment details on PullRequest {{ number \
         reviews(last: 100) {{ nodes {{ submittedAt }} }} \
         commits(last: 100) {{ nodes {{ commit {{ committedDate }} }} }} \
         rollup: commits(last: 1) {{ nodes {{ commit {{ statusCheckRollup {{ \
         contexts(first: 100) {{ nodes {{ __typename \
         ... on CheckRun {{ name conclusion }} \
         ... on StatusContext {{ context state }} }} }} }} }} }} }} }}"
    )
}

#[derive(Deserialize)]
struct RepoTarget {
    #[serde(rename = "nameWithOwner")]
    name_with_owner: String,
}

/// The owner and name to query, from the forge's own answer about this checkout.
pub fn parse_repo_target(payload: &str) -> Result<(String, String), ForgeError> {
    let target: RepoTarget = serde_json::from_str(payload)?;
    target
        .name_with_owner
        .split_once('/')
        .map(|(owner, name)| (owner.to_owned(), name.to_owned()))
        .ok_or(ForgeError::Target {
            named: target.name_with_owner,
        })
}

#[derive(Deserialize)]
struct Nodes<T> {
    #[serde(default)]
    nodes: Vec<T>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: Committed,
}

#[derive(Deserialize)]
struct RollupNode {
    commit: RollupHolder,
}

#[derive(Deserialize)]
struct RollupHolder {
    #[serde(default, rename = "statusCheckRollup")]
    rollup: Option<Contexts>,
}

#[derive(Deserialize)]
struct Contexts {
    #[serde(default)]
    contexts: Option<Nodes<CheckRun>>,
}

#[derive(Deserialize)]
struct DetailsPayload {
    number: u64,
    #[serde(default)]
    reviews: Option<Nodes<Dated>>,
    #[serde(default)]
    commits: Option<Nodes<CommitNode>>,
    #[serde(default)]
    rollup: Option<Nodes<RollupNode>>,
}

#[derive(Deserialize)]
struct DetailsData {
    #[serde(default)]
    repository: Option<BTreeMap<String, Option<DetailsPayload>>>,
}

#[derive(Deserialize)]
struct QueryFailure {
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct DetailsEnvelope {
    #[serde(default)]
    data: Option<DetailsData>,
    #[serde(default)]
    errors: Vec<QueryFailure>,
}

/// Review age and check state per pull request, from one batch reply.
///
/// A review four days older than the branch head sent an agent to rewrite
/// already-fixed code; that comparison is why the review timeline is asked for at
/// all. Errors in the reply are raised rather than read as an empty answer,
/// because an empty answer renders as "nothing to compare" and "no checks", which
/// is how a red pull request reads as clean.
pub fn parse_pull_details(payload: &str) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
    let envelope: DetailsEnvelope = serde_json::from_str(payload)?;
    if !envelope.errors.is_empty() {
        return Err(ForgeError::Query {
            detail: envelope
                .errors
                .iter()
                .map(|failure| failure.message.clone())
                .collect::<Vec<_>>()
                .join("; "),
        });
    }
    let mut details = BTreeMap::new();
    // No errors AND no repository is not an empty answer. This is only ever
    // called for a non-empty query — `CliForge::pull_details` returns early
    // otherwise — so a reply carrying neither is a reply about nothing, and
    // reporting it as "nothing to compare, no checks ran" is exactly how a red
    // pull request reads as clean.
    let Some(repository) = envelope.data.and_then(|data| data.repository) else {
        return Err(ForgeError::Query {
            detail: "the reply carried neither errors nor a repository".to_owned(),
        });
    };
    for payload in repository.into_values().flatten() {
        let newest_review = payload
            .reviews
            .iter()
            .flat_map(|list| list.nodes.iter())
            .filter_map(|review| review.submitted_at.as_deref())
            .max();
        let newest_commit = payload
            .commits
            .iter()
            .flat_map(|list| list.nodes.iter())
            .map(|node| node.commit.committed_date.as_str())
            .max();
        let review_predates_head = match (newest_review, newest_commit) {
            (Some(review), Some(commit)) => Some(review < commit),
            _ => None,
        };
        // Always `Some` for a pull request the reply carried: it was consulted,
        // and an absent rollup means nothing ran rather than nobody asked.
        let checks = Some(ChecksSummary {
            runs: payload
                .rollup
                .iter()
                .flat_map(|list| list.nodes.iter())
                .filter_map(|node| node.commit.rollup.as_ref())
                .filter_map(|rollup| rollup.contexts.as_ref())
                .flat_map(|contexts| contexts.nodes.iter())
                .cloned()
                .collect(),
        });
        let _ = details.insert(
            payload.number,
            PullDetails {
                review_predates_head,
                checks,
            },
        );
    }
    Ok(details)
}
```

- [ ] **Step 6: Implement it for `FakeForge`.** In `impl Forge for FakeForge`, replace `review_predates_head` and `checks` with:

```rust
    fn pull_details(
        &self,
        _repo: &Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullDetails>, ForgeError> {
        Ok(numbers
            .iter()
            .map(|number| {
                let known = self
                    .pull_requests
                    .values()
                    .any(|pull_request| pull_request.number == *number);
                (
                    *number,
                    PullDetails {
                        // A pull request the fake does not know has nothing to
                        // compare, exactly as the real forge answers for one whose
                        // timeline it cannot see.
                        review_predates_head: known.then(|| self.stale_reviews.contains(number)),
                        checks: self.checks.get(number).cloned(),
                    },
                )
            })
            .collect())
    }
```

- [ ] **Step 7: Migrate the three test forges.** In `src/commands/sync.rs`'s `mod tests`, change `use std::cell::RefCell;` to `use std::sync::Mutex;`, change `ErroringForge`'s field to `comment_calls: Mutex<Vec<u64>>`, and change its `newest_comment` body's first statement to:

```rust
            if let Ok(mut calls) = self.comment_calls.lock() {
                calls.push(number);
            }
```

Replace each of `ErroringForge`'s and `PullListUnavailable`'s `review_predates_head` and `checks` methods with:

```rust
        fn pull_details(
            &self,
            _repo: &Path,
            _numbers: &[u64],
        ) -> Result<BTreeMap<u64, crate::forge::PullDetails>, ForgeError> {
            Ok(BTreeMap::new())
        }
```

Change the six `comment_calls: RefCell::new(Vec::new()),` constructions (lines 681, 736, 765, 796, 831, 865) to `comment_calls: Mutex::new(Vec::new()),`, and the read at line 877 to:

```rust
        assert!(forge.comment_calls.lock().expect("lock").is_empty());
```

In `tests/jj_integration.rs`, replace `StateUnavailableForge`'s `review_predates_head` and `checks` with:

```rust
    fn pull_details(
        &self,
        _repo: &std::path::Path,
        _numbers: &[u64],
    ) -> Result<BTreeMap<u64, knives::forge::PullDetails>, ForgeError> {
        Ok(BTreeMap::new())
    }
```

- [ ] **Step 8: Point `status` at the new method, minimally.** `review_predates_head_for` and `checks_for` are replaced wholesale in Task 3; here they only need to compile and keep answering what they answer today. Replace both:

```rust
fn review_predates_head_for(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    pull_request: Option<&PullRequest>,
    report: &mut Report,
) -> Option<bool> {
    match (forge, pull_request) {
        (Some(forge), Some(pull_request)) if !pull_request.review_decision.is_empty() => {
            match forge.pull_details(&entry.path, &[pull_request.number]) {
                Ok(details) => details
                    .get(&pull_request.number)
                    .and_then(|detail| detail.review_predates_head),
                Err(error) => {
                    report.problems.push(format!(
                        "review age for #{} unavailable: {error}",
                        pull_request.number
                    ));
                    None
                }
            }
        }
        _ => None,
    }
}

fn checks_for(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    pull_request: Option<&PullRequest>,
    report: &mut Report,
) -> Option<ChecksSummary> {
    match (forge, pull_request) {
        (Some(forge), Some(pull_request)) if pull_request.is_open() => {
            match forge.pull_details(&entry.path, &[pull_request.number]) {
                Ok(details) => details
                    .get(&pull_request.number)
                    .and_then(|detail| detail.checks.clone()),
                Err(error) => {
                    report.problems.push(format!(
                        "checks for #{} unavailable: {error}",
                        pull_request.number
                    ));
                    None
                }
            }
        }
        _ => None,
    }
}
```

- [ ] **Step 9: Run the tests**

Run: `cargo test --lib forge:: && cargo test --test forge_contract && cargo test --lib sync::`
Expected: PASS.

- [ ] **Step 10: Whole suite and gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS with no clippy output. A `dead_code` warning here means a decoder was left behind: `CheckRollup`, `ReviewAges`, `parse_checks` and `compare_review_to_head` are all gone.

---

### Task 3: `status` asks the forge once

**Files:**
- Modify: `src/commands/status.rs`
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `Forge::pull_details`, `PullDetails` (Task 2); `RowInput`, `branch_rows`, `Timings` (Task 1).
- Produces:
  - `type CarriedPull = (BranchName, CommitId, Option<CommitId>, Option<PullRequest>)`
  - `fn carried_pulls(branches: Vec<(BranchName, CommitId)>, pull_requests: &BTreeMap<BranchName, PullRequest>, tips: &BookmarkTips) -> Vec<CarriedPull>`
  - `fn detail_numbers(carried: &[CarriedPull]) -> Vec<u64>`
  - `fn pull_details_from_forge(forge: Option<&dyn Forge>, entry: &RepoEntry, numbers: &[u64], report: &mut Report) -> BTreeMap<u64, PullDetails>`
  - `fn review_predates_head_from(details: Option<&PullDetails>, pull_request: Option<&PullRequest>) -> Option<bool>`
  - `fn checks_from(details: Option<&PullDetails>, pull_request: Option<&PullRequest>) -> Option<ChecksSummary>`
- Removed: `fn review_predates_head_for`, `fn checks_for`.

One behaviour does change, and it is the only one: a forge failure used to record two problems per pull request (`review age for #N unavailable`, `checks for #N unavailable`) and now records one for the batch (`review age and checks unavailable`). Both make the report `Exit::Incomplete`, no test asserts on the old strings, and on a working forge the two reports are identical.

- [ ] **Step 1: Write the failing tests.** In `tests/jj_integration.rs`, add the fake, the shared entry helper, and the two tests:

```rust
/// A forge whose list works and whose batch does not, for the one behaviour that
/// batching changes: how a details failure is reported.
struct DetailsUnavailableForge {
    pull_requests: BTreeMap<BranchName, PullRequest>,
}

impl Forge for DetailsUnavailableForge {
    fn pull_requests(
        &self,
        _repo: &std::path::Path,
    ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        Ok(self.pull_requests.clone())
    }

    fn pull_details(
        &self,
        _repo: &std::path::Path,
        _numbers: &[u64],
    ) -> Result<BTreeMap<u64, knives::forge::PullDetails>, ForgeError> {
        Err(ForgeError::Command {
            command: "gh api graphql".to_owned(),
            dir: "/repo".to_owned(),
            code: 1,
            stderr: "unavailable".to_owned(),
        })
    }

    fn pull_request_state(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }

    fn newest_comment(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
}

/// A registry entry for the lab's work checkout, which stands in for origin.
fn lab_entry(lab: &lab::Lab) -> RepoEntry {
    RepoEntry {
        path: lab.work.clone(),
        upstream: lab.upstream.display().to_string(),
        origin: lab.work.display().to_string(),
        base: None,
        release: None,
        release_branch: None,
        test_count_command: None,
        consumers: Vec::new(),
    }
}

#[test]
fn one_batch_answers_review_age_and_checks_for_every_branch_at_once() {
    // Given: two branches with open pull requests, one stale-reviewed and red, one
    // clean, and a third whose pull request is closed
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state, decision) in [
        (11, "feat/alpha", "OPEN", "CHANGES_REQUESTED"),
        (12, "feat/beta", "OPEN", "APPROVED"),
        (13, "feat/gamma", "CLOSED", ""),
    ] {
        let _ = pull_requests.insert(
            BranchName::new(branch),
            PullRequest {
                number,
                state: state.to_owned(),
                review_decision: decision.to_owned(),
                head_ref_name: branch.to_owned(),
                ..PullRequest::default()
            },
        );
    }
    let forge = knives::forge::FakeForge {
        pull_requests,
        stale_reviews: vec![11],
        checks: BTreeMap::from([
            (
                11,
                ChecksSummary {
                    runs: vec![knives::forge::CheckRun {
                        name: "build".to_owned(),
                        conclusion: "FAILURE".to_owned(),
                    }],
                },
            ),
            // Supplied and empty: consulted, with nothing having run. The fake
            // answers with the facts it was given, so supplying the entry is how
            // "consulted" is expressed and omitting it is how "not consulted" is.
            (12, ChecksSummary::default()),
        ]),
        ..knives::forge::FakeForge::default()
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    // When: status gathers
    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            registry: None,
            ledger: None,
        },
    )
    .expect("gather");

    // Then: each branch carries the facts the per-pull-request calls used to fetch
    let row = |name: &str| {
        report
            .branches
            .iter()
            .find(|row| row.name.as_str() == name)
            .unwrap_or_else(|| panic!("no row for {name}: {report:?}"))
            .clone()
    };
    assert_eq!(row("feat/alpha").review_predates_head, Some(true));
    assert!(
        row("feat/alpha")
            .checks
            .as_ref()
            .is_some_and(ChecksSummary::failing)
    );
    assert_eq!(row("feat/beta").review_predates_head, Some(false));
    assert_eq!(
        row("feat/beta").checks,
        Some(ChecksSummary::default()),
        "consulted with nothing running is not the same as unconsulted"
    );
    // And: a settled pull request is neither asked about nor reported on
    assert_eq!(row("feat/gamma").review_predates_head, None);
    assert_eq!(row("feat/gamma").checks, None);
    assert!(report.problems.is_empty(), "was: {report:?}");
}

/// Records what the batch was asked for, so "once, with exactly these numbers"
/// is asserted rather than assumed.
struct CountingForge {
    pull_requests: BTreeMap<BranchName, PullRequest>,
    asked: std::sync::Mutex<Vec<Vec<u64>>>,
}

impl Forge for CountingForge {
    fn pull_requests(
        &self,
        _repo: &std::path::Path,
    ) -> Result<BTreeMap<BranchName, PullRequest>, ForgeError> {
        Ok(self.pull_requests.clone())
    }

    fn pull_details(
        &self,
        _repo: &std::path::Path,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, knives::forge::PullDetails>, ForgeError> {
        if let Ok(mut asked) = self.asked.lock() {
            asked.push(numbers.to_vec());
        }
        Ok(BTreeMap::new())
    }

    fn pull_request_state(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }

    fn newest_comment(
        &self,
        _repo: &std::path::Path,
        _number: u64,
    ) -> Result<Option<String>, ForgeError> {
        Ok(None)
    }
}

#[test]
fn the_forge_is_asked_once_for_the_whole_report_with_one_entry_per_number() {
    // The point of the batch, stated as a contract rather than as a hope: one
    // call for the repository, carrying each number once, and only the numbers
    // the per-branch calls would have asked about.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    lab.branch("feat/beta", "beta.txt", "beta\n");
    lab.branch("feat/gamma", "gamma.txt", "gamma\n");
    lab.branch("feat/delta", "delta.txt", "delta\n");
    let mut pull_requests = BTreeMap::new();
    for (number, branch, state, decision) in [
        (12, "feat/beta", "OPEN", "APPROVED"),
        (11, "feat/alpha", "OPEN", ""),
        // Settled with nobody having reviewed it: neither phase asked about this
        // one before, so neither does the batch.
        (13, "feat/gamma", "CLOSED", ""),
    ] {
        let _ = pull_requests.insert(
            BranchName::new(branch),
            PullRequest {
                number,
                state: state.to_owned(),
                review_decision: decision.to_owned(),
                head_ref_name: branch.to_owned(),
                ..PullRequest::default()
            },
        );
    }
    let forge = CountingForge {
        pull_requests,
        asked: std::sync::Mutex::new(Vec::new()),
    };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            registry: None,
            ledger: None,
        },
    )
    .expect("gather");
    assert_eq!(report.branches.len(), 4, "was: {report:?}");

    let asked = forge.asked.lock().expect("lock");
    assert_eq!(asked.len(), 1, "the forge was asked {} times: {asked:?}", asked.len());
    assert_eq!(
        asked[0],
        vec![11, 12],
        "sorted, each number once, and only the ones worth asking about"
    );
}

#[test]
fn a_batch_the_forge_refused_is_one_unanswered_question_not_a_clean_report() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let mut pull_requests = BTreeMap::new();
    let _ = pull_requests.insert(
        BranchName::new("feat/alpha"),
        PullRequest {
            number: 11,
            state: "OPEN".to_owned(),
            review_decision: "APPROVED".to_owned(),
            head_ref_name: "feat/alpha".to_owned(),
            ..PullRequest::default()
        },
    );
    let forge = DetailsUnavailableForge { pull_requests };
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");

    let report = status::gather(
        &knives::ids::RepoName::new("demo"),
        &lab_entry(&lab),
        &store,
        &knives::commands::status::Options {
            probe: false,
            forge: Some(&forge),
            registry: None,
            ledger: None,
        },
    )
    .expect("gather");

    assert_eq!(report.problems.len(), 1, "was: {report:?}");
    assert!(
        report.problems[0].contains("review age and checks unavailable"),
        "was: {report:?}"
    );
    assert_eq!(status::exit_for(&report), knives::cli::Exit::Incomplete);
    let row = &report.branches[0];
    assert_eq!(row.review_predates_head, None, "a refused answer is not 'current'");
    assert_eq!(row.checks, None, "a refused answer is not 'nothing ran'");
}
```

Note on why these fixtures reach the report at all: `FakeForge`'s pull requests carry no `headRepositoryOwner`, so ownership matching would drop them — except that `ours_only` fails open when it can parse an owner out of no remote, and the lab's `origin` is a filesystem path with no forge host in it. Every existing status test that pairs `FakeForge` with a `lab::Lab` relies on the same path; if a future lab gives its remotes forge-shaped URLs, these fixtures need a `head_repository_owner` instead.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration one_batch_answers_review_age && cargo test --test jj_integration a_batch_the_forge_refused`
Expected: the first PASSES already (the facts are unchanged; it is the regression net for this task), the second FAILS on the problem count — `DetailsUnavailableForge` is asked once per phase per branch today, so it produces two problems whose text names `#11` twice. Read the failure and confirm it says 2, not 1.

- [ ] **Step 3: Add the pre-pass helpers.** In `src/commands/status.rs`, add `PullDetails` to the `use crate::forge::{...}` line, and add these after `fn note_fetched_heads`:

```rust
/// One maintained branch, its tip, where origin has it, and the pull request it
/// refers to.
type CarriedPull = (BranchName, CommitId, Option<CommitId>, Option<PullRequest>);

/// Every branch paired with what the row loop needs before it starts.
///
/// Built up front because both phases that dominate a run — the forge round trip
/// and the landed probes — go over the whole list at once now, and a loop that
/// discovers its own inputs one at a time is exactly what made them serial.
fn carried_pulls(
    branches: Vec<(BranchName, CommitId)>,
    pull_requests: &BTreeMap<BranchName, PullRequest>,
    tips: &BookmarkTips,
) -> Vec<CarriedPull> {
    branches
        .into_iter()
        .map(|(branch, tip)| {
            let origin_tip = tips
                .get(&BookmarkRef::Remote {
                    branch: branch.clone(),
                    remote: crate::ids::RemoteName::new("origin"),
                })
                .cloned();
            let pull_request = pull_request_for(&branch, pull_requests);
            (branch, tip, origin_tip, pull_request)
        })
        .collect()
}

/// The pull requests worth asking the forge about.
///
/// Exactly the ones the per-branch calls asked about: a review age only when the
/// forge recorded a review decision, checks only while the pull request is open.
/// Asking about more would be a behaviour change dressed as an optimisation.
fn detail_numbers(carried: &[CarriedPull]) -> Vec<u64> {
    let mut numbers: Vec<u64> = carried
        .iter()
        .filter_map(|(_, _, _, pull_request)| pull_request.as_ref())
        .filter(|pull_request| pull_request.is_open() || !pull_request.review_decision.is_empty())
        .map(|pull_request| pull_request.number)
        .collect();
    // Sorted and duplicate-free because the query builds one aliased field per
    // number and a repeated alias is a rejected query, not a slow one. No two
    // rows can name one number today — `maintained_branches` drops the `pr-<n>`
    // bookmarks that are the only other way to reach a number — and this keeps
    // the query's shape independent of that staying true.
    numbers.sort_unstable();
    numbers.dedup();
    numbers
}

/// Review age and check state for every pull request in this report, in one call.
fn pull_details_from_forge(
    forge: Option<&dyn Forge>,
    entry: &RepoEntry,
    numbers: &[u64],
    report: &mut Report,
) -> BTreeMap<u64, PullDetails> {
    let Some(forge) = forge else {
        return BTreeMap::new();
    };
    if numbers.is_empty() {
        return BTreeMap::new();
    }
    match forge.pull_details(&entry.path, numbers) {
        Ok(details) => details,
        Err(error) => {
            report
                .problems
                .push(format!("review age and checks unavailable: {error}"));
            BTreeMap::new()
        }
    }
}

/// Whether the newest review predates the branch head, when there was a review to
/// compare.
///
/// Gated as the per-pull-request call was: an empty review decision means the
/// forge recorded no review, and `None` must never render as "current".
fn review_predates_head_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<bool> {
    let pull_request = pull_request?;
    if pull_request.review_decision.is_empty() {
        return None;
    }
    details?.review_predates_head
}

/// What the forge's checks say, for an open pull request that was consulted.
///
/// Settled pull requests are not asked about and not reported on: a closed one's
/// recorded rollup is obsolete the moment it closes.
fn checks_from(
    details: Option<&PullDetails>,
    pull_request: Option<&PullRequest>,
) -> Option<ChecksSummary> {
    let pull_request = pull_request?;
    if !pull_request.is_open() {
        return None;
    }
    details?.checks.clone()
}
```

- [ ] **Step 4: Delete `review_predates_head_for` and `checks_for`** from `src/commands/status.rs`.

- [ ] **Step 5: Rewrite `branch_rows`** to drive the batch:

```rust
fn branch_rows(
    input: RowInput<'_>,
    report: &mut Report,
    timings: &mut Timings,
) -> anyhow::Result<Vec<String>> {
    let carried = carried_pulls(input.branches, input.pull_requests, input.tips);

    let phase = std::time::Instant::now();
    let details = pull_details_from_forge(
        input.options.forge,
        input.entry,
        &detail_numbers(&carried),
        report,
    );
    timings.forge += phase.elapsed();

    let mut unjudged: Vec<String> = Vec::new();
    for (branch, tip, origin_tip, pull_request) in carried {
        let detail = pull_request
            .as_ref()
            .and_then(|pull_request| details.get(&pull_request.number));
        let review_predates_head = review_predates_head_from(detail, pull_request.as_ref());
        let checks = checks_from(detail, pull_request.as_ref());
        let origin_relation = record_origin_relation(
            report,
            &branch,
            relation_to_origin(input.repo, &tip, origin_tip.as_ref()),
        );
        let phase = std::time::Instant::now();
        let landed = landed_verdict(
            &input.entry.path,
            &branch,
            (&tip, origin_tip.as_ref()),
            input.options,
            input.upstream_trunk,
        )?;
        timings.probes += phase.elapsed();
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let target = BranchTarget::new(input.name.clone(), branch.clone());
        let stated_pull = stated_pull_for(&target, input.store, input.entry, input.options);
        let notch = newest_for(input.notches, branch.as_str()).map(LastNotch::of);
        report.branches.push(BranchRow {
            name: branch,
            tip: Some(tip),
            origin_tip,
            origin_relation,
            pull_request,
            landed,
            review_predates_head,
            checks,
            fork_only: input.store.is_fork_only(&target),
            stated_pull,
            notch,
        });
    }
    Ok(unjudged)
}
```

- [ ] **Step 6: Run the tests**

Run: `cargo test --test jj_integration one_batch_answers_review_age && cargo test --test jj_integration a_batch_the_forge_refused`
Expected: PASS.

- [ ] **Step 7: Confirm no reported fact changed, and the gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS. Every existing status test asserts on rendered tokens and exit codes; a failure here means a fact moved, which this task must not do.

---

### Task 4: Verify jj-lib's concurrent read-only open before relying on it

**Files:**
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `knives::jj::probe_landed`, `knives::detect::landed::RebaseOutcome` — both already imported in this file.
- Produces: nothing. This is the measurement Task 5 rests on.

This task is independent of every other one and can run first. It has no red phase, because it measures a library's behaviour rather than adding one: run it and read the answer. **If it fails, stop and report it** — the whole of Task 5 rests on this, and the answer would be a different design rather than a retry.

- [ ] **Step 1: Write the capability test** in `tests/jj_integration.rs`:

```rust
#[test]
fn jj_lib_answers_the_same_probe_from_many_threads_as_from_one() {
    // Every parallel landed probe opens its own repository handle and replays
    // inside a transaction it drops. jj's own model is concurrent-safe by design,
    // but the loaded-repo handle is not assumed Sync, so this is measured rather
    // than believed. Nothing is written: a dropped transaction never becomes an
    // operation, so no thread can observe another's.
    let lab = lab::Lab::new();
    for index in 0..8 {
        lab.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    let branches: Vec<BranchName> = (0..8)
        .map(|index| BranchName::new(format!("feat/b{index}")))
        .collect();

    // When: the same probes run serially and then all at once
    let serial: Vec<RebaseOutcome> = branches
        .iter()
        .map(|branch| probe_landed(&lab.work, branch, "main@upstream").expect("serial probe"))
        .collect();
    let concurrent: Vec<RebaseOutcome> = std::thread::scope(|scope| {
        let handles: Vec<_> = branches
            .iter()
            .map(|branch| {
                scope.spawn(move || {
                    probe_landed(&lab.work, branch, "main@upstream").expect("concurrent probe")
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a probe thread panicked"))
            .collect()
    });

    // Then: every answer is identical and in the same order
    assert_eq!(
        concurrent, serial,
        "a concurrent probe answered differently from a serial one"
    );
    assert!(
        serial
            .iter()
            .all(|outcome| *outcome == RebaseOutcome::CleanNonEmpty),
        "the fixture's unmerged branches should all be unlanded: {serial:?}"
    );
    // And: the repository is still readable afterwards, so nothing leaked a lock
    // or a half-written operation.
    assert!(
        Repo::open(&lab.work)
            .expect("reopen after concurrent probes")
            .bookmark_tips()
            .expect("read tips")
            .len()
            > 8
    );
}
```

- [ ] **Step 2: Run it**

Run: `cargo test --test jj_integration jj_lib_answers_the_same_probe -- --nocapture`
Expected: PASS. A failure — a panic inside a thread, a differing verdict, or an unreadable repository afterwards — means the design in Task 5 cannot stand; stop and report exactly what failed, with the output.

- [ ] **Step 3: Gates**

Run: `cargo clippy --all-targets && cargo fmt --check`
Expected: no output.

---

### Task 5: Probe the branches concurrently

**Files:**
- Modify: `src/commands/status.rs`
- Modify: `src/jj.rs` (`enum JjError`)
- Modify: `src/main.rs` (`run_status`)
- Modify: `tests/jj_integration.rs` (every `status::Options` literal)

**Interfaces:**
- Consumes: `Forge: Send + Sync` (Task 2) — which is what makes `Options` `Sync` and lets `landed_verdict` keep taking `&Options` inside a thread; `branch_rows`, `CarriedPull` (Tasks 1, 3); the answer from Task 4.
- Produces:
  - `Options::workers: usize`
  - `fn landed_verdicts(path: &Path, carried: &[CarriedPull], options: &Options<'_>, upstream_trunk: &str) -> Vec<Result<Option<LandedVerdict>, JjError>>`
  - `JjError::ProbePanic { branch: String }`
  - `fn parallelism() -> usize` in `src/main.rs`
- `fn landed_verdict` is unchanged.

- [ ] **Step 1: Write the failing test** in `tests/jj_integration.rs`:

```rust
#[test]
fn parallel_landed_probes_answer_exactly_what_serial_ones_did() {
    // The comment on `maintained_branches` already said these were most of the
    // runtime. Parallelising them may not change one reported fact, so the proof
    // is the two reports rendering identically.
    let lab = lab::Lab::new();
    for index in 0..6 {
        lab.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    lab.publish_pull("feat/b0", 1);
    lab.squash_merge_pull(1, None);
    lab.fetch_work();
    let entry = lab_entry(&lab);
    let state = tempfile::tempdir().expect("state directory");
    let store = Store::open(state.path().join("state.json")).expect("open store");
    let options = |workers: usize| knives::commands::status::Options {
        probe: true,
        forge: None,
        registry: None,
        ledger: None,
        workers,
    };
    let name = knives::ids::RepoName::new("demo");

    // When: the same repository is gathered serially and on several threads
    let serial = status::gather(&name, &entry, &store, &options(1)).expect("serial gather");
    let parallel = status::gather(&name, &entry, &store, &options(8)).expect("parallel gather");

    // Then: not one token differs, including the landed column
    assert_eq!(
        status::render(&serial, true),
        status::render(&parallel, true),
        "parallelism changed the report"
    );
    assert_eq!(status::exit_for(&serial), status::exit_for(&parallel));
    assert_eq!(
        parallel
            .branches
            .iter()
            .map(|row| row.landed)
            .collect::<Vec<_>>(),
        serial
            .branches
            .iter()
            .map(|row| row.landed)
            .collect::<Vec<_>>()
    );
    assert!(
        parallel
            .branches
            .iter()
            .any(|row| row.landed == Some(knives::detect::LandedVerdict::InTrunk)),
        "the merged branch must still be judged in-trunk: {parallel:?}"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration parallel_landed_probes_answer`
Expected: FAIL to compile — `struct 'Options' has no field named 'workers'`.

- [ ] **Step 3: Add the error variant** in `src/jj.rs`, to `enum JjError`:

```rust
    #[error("the landed probe for `{branch}` panicked")]
    ProbePanic { branch: String },
```

- [ ] **Step 4: Add the field** to `struct Options` in `src/commands/status.rs`:

```rust
    /// How many threads the landed probes may use. `1` is serial.
    ///
    /// Set below the machine's parallelism when several repositories are gathered
    /// at once, so `--all` cannot multiply one repository's probe threads by the
    /// size of the registry.
    pub workers: usize,
```

- [ ] **Step 5: Add the parallel driver.** In `src/commands/status.rs`, after `fn landed_verdict`:

```rust
/// Landed verdicts for every branch, probed concurrently, in branch order.
///
/// Each probe opens its own repository handle and replays inside a transaction it
/// drops: nothing is shared between threads and nothing is written, so no probe
/// can observe another's. Verified against jj-lib rather than assumed — see
/// `jj_lib_answers_the_same_probe_from_many_threads_as_from_one`.
///
/// Bounded by chunking the branch list rather than by a work queue, because the
/// bound is the point and a queue would be a dependency. Results come back in the
/// order the branches went in, so this is the serial report.
fn landed_verdicts(
    path: &std::path::Path,
    carried: &[CarriedPull],
    options: &Options<'_>,
    upstream_trunk: &str,
) -> Vec<Result<Option<LandedVerdict>, JjError>> {
    // Nothing to spawn for: `landed_verdict` answers `None` without probing when
    // the probe is off, and an empty list has no chunks.
    if !options.probe || carried.is_empty() {
        return carried.iter().map(|_| Ok(None)).collect();
    }
    let workers = options.workers.clamp(1, carried.len());
    let chunk = carried.len().div_ceil(workers);
    std::thread::scope(|scope| {
        let handles: Vec<(&[CarriedPull], _)> = carried
            .chunks(chunk)
            .map(|slice| {
                (
                    slice,
                    scope.spawn(move || {
                        slice
                            .iter()
                            .map(|(branch, tip, origin_tip, _)| {
                                landed_verdict(
                                    path,
                                    branch,
                                    (tip, origin_tip.as_ref()),
                                    options,
                                    upstream_trunk,
                                )
                            })
                            .collect::<Vec<_>>()
                    }),
                )
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|(slice, handle)| {
                handle.join().unwrap_or_else(|_| {
                    slice
                        .iter()
                        .map(|(branch, ..)| {
                            Err(JjError::ProbePanic {
                                branch: branch.to_string(),
                            })
                        })
                        .collect()
                })
            })
            .collect()
    })
}
```

- [ ] **Step 6: Drive it from `branch_rows`.** Replace the function with:

```rust
fn branch_rows(
    input: RowInput<'_>,
    report: &mut Report,
    timings: &mut Timings,
) -> anyhow::Result<Vec<String>> {
    let carried = carried_pulls(input.branches, input.pull_requests, input.tips);

    let phase = std::time::Instant::now();
    let details = pull_details_from_forge(
        input.options.forge,
        input.entry,
        &detail_numbers(&carried),
        report,
    );
    timings.forge += phase.elapsed();

    let phase = std::time::Instant::now();
    let verdicts = landed_verdicts(
        &input.entry.path,
        &carried,
        input.options,
        input.upstream_trunk,
    );
    timings.probes = phase.elapsed();

    let mut unjudged: Vec<String> = Vec::new();
    for (verdict, (branch, tip, origin_tip, pull_request)) in verdicts.into_iter().zip(carried) {
        // Propagated in branch order, so a probe failure reports the same branch
        // and the same message it did when the probes ran one at a time.
        let landed = verdict?;
        if landed == Some(LandedVerdict::Unjudged) {
            unjudged.push(branch.to_string());
        }
        let detail = pull_request
            .as_ref()
            .and_then(|pull_request| details.get(&pull_request.number));
        let review_predates_head = review_predates_head_from(detail, pull_request.as_ref());
        let checks = checks_from(detail, pull_request.as_ref());
        let origin_relation = record_origin_relation(
            report,
            &branch,
            relation_to_origin(input.repo, &tip, origin_tip.as_ref()),
        );
        let target = BranchTarget::new(input.name.clone(), branch.clone());
        let stated_pull = stated_pull_for(&target, input.store, input.entry, input.options);
        let notch = newest_for(input.notches, branch.as_str()).map(LastNotch::of);
        report.branches.push(BranchRow {
            name: branch,
            tip: Some(tip),
            origin_tip,
            origin_relation,
            pull_request,
            landed,
            review_predates_head,
            checks,
            fork_only: input.store.is_fork_only(&target),
            stated_pull,
            notch,
        });
    }
    Ok(unjudged)
}
```

- [ ] **Step 7: Set the field at every construction.** In `src/main.rs`, add this helper beside `run_status`:

```rust
/// How many threads to run at once, from the machine's own answer.
fn parallelism() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}
```

and add `workers: parallelism(),` to `run_status`'s `status::Options` literal.

In `tests/jj_integration.rs`, add `workers: 1,` to every `status::Options` literal that does not already set it — the five detector tests, plus the ones Tasks 1 and 3 added. Serial is what those tests mean: they assert on facts, and a fact that needed a thread to appear would be the bug.

- [ ] **Step 8: Run the tests**

Run: `cargo test --test jj_integration parallel_landed_probes_answer`
Expected: PASS.

- [ ] **Step 9: Whole suite and gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS with no clippy output.

---

### Task 6: Gather repositories concurrently under `--all`

**Files:**
- Modify: `src/main.rs` (`run_status`)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `gather_timed`, `Timings`, `timing_enabled` (Task 1); `Options::workers`, `parallelism` (Task 5); `Forge: Send + Sync` (Task 2).
- Produces: nothing new. `run_status` becomes concurrent, and the probe worker budget is divided so nesting cannot multiply.

Repositories are independent by construction, and the store was read once under a lock before any of this, so every thread sees one snapshot. Rendering stays in registry order: a report that shuffled with thread scheduling would be a different report every run.

- [ ] **Step 1: Write the failing test.** `run_status` lives in the binary, so this goes through the binary, in `tests/jj_integration.rs`:

```rust
#[test]
fn status_all_reports_every_repo_in_registry_order() {
    // Given: two managed forks in one registry, named so registry order and
    // completion order can differ — the small one finishes first and must still
    // print second.
    let first = lab::Lab::new();
    first.branch("feat/alpha", "alpha.txt", "alpha\n");
    let second = lab::Lab::new();
    for index in 0..6 {
        second.branch(
            &format!("feat/b{index}"),
            &format!("b{index}.txt"),
            "content\n",
        );
    }
    let home = tempfile::tempdir().expect("create config home");
    std::fs::write(
        home.path().join("repos.toml"),
        format!(
            "[repos.aardvark]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/one.git\"\n\
             [repos.zebra]\npath = \"{}\"\nupstream = \"{}\"\norigin = \"https://forge.invalid/acme/two.git\"\n",
            second.work.display(),
            second.upstream.display(),
            first.work.display(),
            first.upstream.display(),
        ),
    )
    .expect("write registry");

    // When: every repo is reported at once, from outside both of them
    let elsewhere = tempfile::tempdir().expect("somewhere else");
    let output = Command::new(env!("CARGO_BIN_EXE_knives"))
        .args(["--text", "status", "--all", "--no-github", "--no-landed"])
        .current_dir(elsewhere.path())
        .env("KNIVES_CONFIG_HOME", home.path())
        .env("KNIVES_TIMING", "1")
        .output()
        .expect("run status --all");

    // Then: both are present, in registry order, whichever finished first
    let text = String::from_utf8_lossy(&output.stdout);
    let aardvark = text.find("aardvark").expect("first repo reported");
    let zebra = text.find("zebra").expect("second repo reported");
    assert!(
        aardvark < zebra,
        "repos were rendered out of registry order: {text}"
    );
    assert!(text.contains("feat/b5"), "was: {text}");
    assert!(text.contains("feat/alpha"), "was: {text}");
    // And: the timing lines go to stderr, so a script's stdout is still a report
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("timing aardvark:"), "was: {errors}");
    assert!(errors.contains("timing zebra:"), "was: {errors}");
    assert!(!text.contains("timing "), "timings leaked into stdout: {text}");

    // And: `--all` is exactly each repository's own report, in registry order,
    // joined the way the serial loop joined them. That is the equality
    // concurrency has to preserve, and it is the only serial baseline available
    // once this lands, because the binary has no switch for the old shape.
    // `--no-landed` makes it exact: with no probes, a single repo's larger probe
    // budget cannot change a single token.
    let alone = |repo: &str| {
        let output = Command::new(env!("CARGO_BIN_EXE_knives"))
            .args(["--text", "status", "--repo", repo, "--no-github", "--no-landed"])
            .current_dir(elsewhere.path())
            .env("KNIVES_CONFIG_HOME", home.path())
            .output()
            .expect("run status --repo");
        String::from_utf8_lossy(&output.stdout).trim_end().to_owned()
    };
    assert_eq!(
        text.trim_end(),
        format!("{}\n\n{}", alone("aardvark"), alone("zebra")),
        "--all is not each repo's own report in registry order"
    );
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --test jj_integration status_all_reports_every_repo`
Expected: PASS, all three assertions — registry order, timing lines on stderr, and `--all` equalling each repository's own report joined in order. All three are already true of the serial loop, which is exactly why this test is written now: it captures the serial behaviour as the baseline, and once Step 3 lands there is no serial binary left to compare against. Confirm it passes before Step 3, so any failure afterwards is unambiguously the concurrency.

- [ ] **Step 3: Gather concurrently.** In `src/main.rs`'s `run_status`, replace everything after `let forge: Option<&dyn Forge> = if use_forge { Some(&cli_forge) } else { None };` with the following. `chosen` stays a `Vec` and is borrowed by `chunks` rather than consumed, so each thread's closure captures a `&[(RepoName, RepoEntry)]` slice, which is `Copy`:

```rust
    // Bounded on both axes, because they multiply: repositories are chunked
    // across at most `repo_workers` threads, and each of those divides the
    // machine's parallelism among its probes. Spawning one thread per repository
    // instead would put a ten-repo registry's probe threads at ten times the
    // budget, and this work is index reads and repository handles, not idle
    // waiting. Chunked rather than queued for the same reason the probes are: the
    // bound is the point and a queue would be a dependency.
    let repo_workers = chosen.len().clamp(1, parallelism());
    let probe_workers = (parallelism() / repo_workers).max(1);
    let chunk = chosen.len().div_ceil(repo_workers).max(1);
    let gathered: Vec<anyhow::Result<(RepoName, status::Report, status::Timings)>> =
        std::thread::scope(|scope| {
            let handles: Vec<(&[(RepoName, knives::config::RepoEntry)], _)> = chosen
                .chunks(chunk)
                .map(|slice| {
                    (
                        slice,
                        scope.spawn(move || {
                            slice
                                .iter()
                                .map(|(name, entry)| {
                                    // Per repository, because each has its own ledger.
                                    let ledger = knives::ledger::Ledger::for_repo(name);
                                    let (report, timings) = status::gather_timed(
                                        name,
                                        entry,
                                        &store,
                                        &status::Options {
                                            probe,
                                            forge,
                                            registry: Some(&registry),
                                            ledger: Some(&ledger),
                                            workers: probe_workers,
                                        },
                                    )?;
                                    Ok((name.clone(), report, timings))
                                })
                                .collect::<Vec<_>>()
                        }),
                    )
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|(slice, handle)| {
                    handle.join().unwrap_or_else(|_| {
                        slice
                            .iter()
                            .map(|(name, _)| Err(anyhow::anyhow!("gathering {name} panicked")))
                            .collect()
                    })
                })
                .collect()
        });

    let mut worst = Exit::Ok;
    let mut first = true;
    for gathered in gathered {
        let (name, report, timings) = gathered?;
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            if !first {
                println!();
            }
            first = false;
            println!("{}", status::render(&report, verbose));
        }
        // stderr, so a timed run's stdout is still the report a script parses.
        if status::timing_enabled() {
            eprintln!("{}", timings.line(name.as_str()));
        }
        worst = worst.worst(status::exit_for(&report));
    }
    Ok(worst)
```

One consequence, stated rather than discovered: a gather that fails no longer stops the others from running, only from being printed. The repositories before the failure still print, in order, and the exit code is still the error's, so nothing a caller observes changed — the wasted work is wasted, not the answer.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test jj_integration status_all_reports_every_repo`
Expected: PASS.

- [ ] **Step 5: Whole suite and gates**

Run: `cargo test && cargo clippy --all-targets && cargo fmt --check`
Expected: PASS with no clippy output. Update the `// allow: SIZE_OK:` count at the top of `src/main.rs` to its new `wc -l src/main.rs` value.

---

### Task 7: Measured verification

**Files:**
- Modify: `tests/fixtures/gh_pull_details.json` (recorded from a real reply, replacing the hand-shaped one)

**Interfaces:**
- Consumes: everything above, and the baseline numbers Task 1 recorded.
- Produces: the before-and-after wall times, and a recorded batch payload. Both go to the coordinator for the PR body.

- [ ] **Step 1: Record a real batch reply.** Print the query, then ask the forge with it:

```bash
cd /home/ubuntu/knives/default
cargo test --test forge_contract print_the_batch_query -- --ignored --nocapture
```

Take the printed query, replace its two placeholder numbers with two real pull request numbers from the repository measured in Task 1 — one with a review and green or red checks, one without a review, so the fixture exercises both branches of the parser (`knives status --json` lists the numbers and their review decisions) — and record the reply:

```bash
cd <the checkout measured in Task 1>
gh repo view --json nameWithOwner
gh api graphql -f owner=<owner> -f name=<name> -f query='<the printed query>' \
  > /home/ubuntu/knives/default/tests/fixtures/gh_pull_details.json
```

Then scrub it the way `tests/fixtures/gh_pr_list.json` is scrubbed — that fixture's own test, `the_recorded_payload_is_scrubbed`, states the rules: no real forge host, no real owner login, no real URLs. This reply carries timestamps, check names and conclusions and no logins or URLs, so the scrub is normally a no-op; check it and say so rather than assuming.

- [ ] **Step 2: Confirm the recorded reply decodes**

Run: `cargo test --test forge_contract a_recorded_batch_payload_decodes_every_field`
Expected: PASS. A failure here is the finding this fixture exists for: the real reply's shape differs from what the parser expects, and the parser is wrong, not the forge. Fix the parser, not the fixture.

- [ ] **Step 3: Measure the after**

Run: `cargo build --release`

Then, from the same checkout and with the same three invocations Task 1 used:

```bash
cd <the checkout measured in Task 1>
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --json > /dev/null
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --json --no-github > /dev/null
time KNIVES_TIMING=1 ~/knives/default/target/release/knives status --all --json > /dev/null
```

Expected: the `forge` phase down to roughly one round trip's worth from `2N+1`, the `probes` phase down by roughly the worker count, and `--all` down by roughly the repository count, all against Task 1's recorded numbers. Report the six wall times (three before, three after) and the phase numbers for each, and name the repository, its branch count and its open pull request count. If `gh` was unauthenticated in Task 1 it is unauthenticated now: report the `--no-github` pair, and say plainly that the forge half is unmeasured and why.

- [ ] **Step 4: Confirm not one fact moved**

Run: `cargo test`
Expected: PASS, whole suite. Then compare a rendered report against the pre-change one on the real repository:

```bash
cd <the checkout measured in Task 1>
~/knives/default/target/release/knives status --text > /tmp/knives-after.txt
echo "exit $?" >> /tmp/knives-after.txt
diff /tmp/knives-before.txt /tmp/knives-after.txt && echo "identical, exit code included"
```

Expected: no diff, exit code line included. One difference is permitted and one only — a forge that refuses the batch now says `review age and checks unavailable` once in the `unanswered` section instead of two messages per pull request. Anything else in the branch table's nine columns, the grouped findings, the claims block or the exit code is a fact this PR moved, which it must not do; report it rather than accepting it.

`/tmp/knives-before.txt` was written minutes or hours earlier against a live forge, so a genuine upstream change since then — a review landing, CI turning green — looks exactly like a regression. If the diff is non-empty, settle which it is before concluding anything, using the binary Task 1 saved:

```bash
/tmp/knives-baseline-bin status --text > /tmp/knives-before-again.txt
echo "exit $?" >> /tmp/knives-before-again.txt
diff /tmp/knives-before-again.txt /tmp/knives-after.txt
```

Back to back against one forge state, that diff is this PR's doing and nothing else.

- [ ] **Step 5: Report**

State, for the coordinator to put in the PR body: the measured before-and-after wall times and phase numbers from Task 1 Step 9 and Step 3 above; the repository they were measured on, with its branch and open-pull-request counts; the subprocess count before and after (`2N+1` against 2, with N named); and that the suite passes unchanged. Not vibes.

---

## Self-review

**Spec coverage**

| Spec | Task |
|---|---|
| 2.1 instrument the release scan, landed probes and forge calls; record a baseline on a real repo and on `--all`; keep it behind a flag or env var if cheap | T1 |
| 2.2 one `gh api graphql` call for all our numbers, review timeline and check rollup; `Forge` gains `pull_details(numbers) -> map`; `CliForge` implements it with GraphQL; `FakeForge` from its existing maps; the per-number methods fold in; 2×N+1 becomes 2 | T2 |
| 2.2 status uses the batch instead of the per-branch pair | T3 |
| 2.3 bounded `std::thread::scope`, no new dependency, each thread its own handle, jj-lib's concurrent open verified in a test first | T4, T5 |
| 2.4 repos gathered concurrently, rendered in registry order, one snapshot-consistent store read | T6 |
| 2.5 measured wall time before and after on this machine, against the real repo and `--all`; the existing suite passes unchanged; not one reported fact, token or exit code altered | T1 (before), T7 (after), T3/T5/T6 (equality tests) |

**Out of scope, and absent:** no task renames, reorders or rewords a status column or line — status text legibility is separate work, and the one wording change is a problem message that appears only when the forge refuses, named in T3 and re-checked in T7. No task adds a cache, a companion detector, a pin comparison, a ref-integrity check, ledger sync, hook injection, or promise-thread tracking.

**Loud failure:** the batch parser errors on a reply carrying neither errors nor a repository (T2) rather than reporting every requested fact as absent, and a batch the forge refused becomes one problem and `Exit::Incomplete` rather than a clean report (T3). Nothing in this PR converts an unexpected reply into a quiet default.

**Equivalence coverage:** landed probes are proven serial-versus-parallel by exact render comparison (T5); the batch is proven to be one call carrying each number once, and only the numbers the per-branch calls asked about (T3); `--all` is proven to equal each repository's own report joined in registry order (T6), which is the only serial baseline left once the loop is concurrent; and T7 diffs the whole rendered report and exit code against the baseline Task 1 saved.

**Placeholders:** none. Every code step carries the code; every run step carries the command and what it should print, including the two steps whose expected result is PASS-before-the-change and which say why. The recording of the real GraphQL fixture is a numbered step with exact commands, and the hand-shaped fixture keeps its test a real gate until then. The one thing this plan cannot supply is forge authentication; Task 1 Step 9 and Task 7 Step 3 each say exactly what to report if it is missing rather than working around it.

**Type consistency:** `Timings`'s four `Duration` fields and `line(&str)`, `timing_enabled()`, `gather_timed(...) -> (Report, Timings)`, `gather(...) -> Report`, `RowInput`'s ten fields, `branch_rows(RowInput, &mut Report, &mut Timings) -> anyhow::Result<Vec<String>>`, `PullDetails::{review_predates_head, checks}`, `Forge::pull_details(&self, &Path, &[u64]) -> Result<BTreeMap<u64, PullDetails>, ForgeError>`, `pull_details_query(&[u64]) -> String`, `parse_pull_details(&str)`, `parse_repo_target(&str) -> Result<(String, String), ForgeError>`, `ForgeError::{Target, Query}`, `CarriedPull`, `carried_pulls`, `detail_numbers`, `pull_details_from_forge`, `review_predates_head_from`, `checks_from`, `landed_verdicts`, `Options::workers`, `JjError::ProbePanic { branch }` and `…
