# Mechanical Branch and Pull Request Checks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Report every mechanically-checkable fact about a branch and its pull request that an agent currently has to notice by hand, so nobody calls a conflicted or CI-red pull request ready to ship.

**Architecture:** `knives status` already gathers per-branch rows and emits observations. Each check below is one more fact on the row plus, where it needs attention, one more `FindingKind`. No check reasons about intent: every one is a forge field or a jj graph query. Reasoning stays with the agent reading the report.

**Tech Stack:** Rust 2024, `jj-lib` 0.43.0 (crates.io, exact pin), `clap` derive, `serde`, `insta`, `cargo nextest`, `basedpyright`-equivalent strictness via `clippy -D warnings`. Forge access is `gh` via `CliForge`.

## Global Constraints

- `cargo clippy --all-targets --all-features --workspace -- -D warnings` must pass. Notable local limits: `too-many-arguments-threshold = 4`, `too-many-lines-threshold = 100`, `cognitive-complexity-threshold = 25`. Bundle parameters into a struct rather than adding a fifth; extract a helper rather than growing a function past 100 lines.
- `cargo fmt --all -- --check`, `cargo machete`, `cargo deny check all` must pass.
- No new crate dependencies. `deny.toml` sets `unknown-git = "deny"` with no git sources allowed and `wildcards = "deny"`.
- Tests must not name a real forge URL in a release surface — `tests/no_hardcoded_identity.rs` scans the source, plugin, documentation, and skill directories. Build hosts with `concat!("github", ".com")` and use placeholder owners (`our-org`, `outsider`).
- knives reports facts and never advises. `Finding` has `kind`, `subject`, `detail` — there is deliberately no remedy field. Do not add one.
- Every new forge field goes in the single `PR_FIELDS` string so it costs no extra `gh` call —
  **except where measurement shows the field breaks that call.** `statusCheckRollup` does:
  added to `--state all --limit 300`, the query exceeds the forge's GraphQL budget and returns
  HTTP 504 on a real repository, 6 runs out of 6, so the report loses every pull-request fact
  it had. Without that field the same call succeeds 3 of 3, and per-pull-request rollup
  succeeds 3 of 3. So an expensive field is fetched per pull request, for the branches we
  actually render, exactly as `review_predates_head` already is. See Task 3.
- `UNKNOWN` from the forge is never treated as a failure. The forge computes mergeability and checks asynchronously; reporting "not worked out yet" as broken teaches the reader to ignore the report.
- **One commit for the whole plan.** This repository takes one commit per pull request, which
  overrides the per-task commits a TDD workflow would normally prescribe. Each task's final
  step is therefore a verification, not a commit; `jj` snapshots continuously, so the work is
  never at risk between tasks. Amend the single commit's description as tasks land.

## Interfaces that already exist

Referenced by several tasks; exact as of this plan.

```rust
// src/forge.rs
pub trait Forge {
    fn open_pull_requests(&self, repo: &Path)
        -> Result<BTreeMap<BranchName, PullRequest>, ForgeError>;
    fn review_predates_head(&self, repo: &Path, number: u64)
        -> Result<Option<bool>, ForgeError>;
    fn pull_request_state(&self, repo: &Path, number: u64)
        -> Result<Option<String>, ForgeError>;
}

pub struct PullRequest {
    pub number: u64,
    pub state: String,
    pub review_decision: String,
    pub head_ref_name: String,
    pub head_ref_oid: String,
    pub updated_at: String,
    pub is_draft: bool,
    pub url: String,
    pub head_repository_owner: Option<Account>,
    pub mergeable: String,
    pub merge_state_status: String,
}

pub struct FakeForge {
    pub pull_requests: BTreeMap<BranchName, PullRequest>,
    pub stale_reviews: Vec<u64>,
    pub vanished_states: BTreeMap<u64, String>,
}

// src/commands/status.rs
pub struct BranchRow {
    pub name: BranchName,
    pub tip: Option<CommitId>,      // None when the local bookmark is divergent
    pub origin_tip: Option<CommitId>,
    pub pull_request: Option<PullRequest>,
    pub landed: Option<LandedVerdict>,
    pub review_predates_head: Option<bool>,
    pub fork_only: bool,
    pub stated_pull: Option<StatedPull>,
}
fn branch_findings(rows: &[BranchRow]) -> Vec<Finding>;

// src/detect.rs
pub enum FindingKind {
    DoubleCheckout, StaleParent, Divergence, StaleReview,
    ClaimOverlap, UnmetDependency, Unmergeable,
}
impl Finding { pub fn new(kind: FindingKind, subject: Subject, detail: impl Into<String>) -> Self; }
```

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/forge.rs` | `gh` access, `PullRequest`, `PR_FIELDS`, `FakeForge` | Add `base_ref_name`, `checks`; add `ChecksSummary`; extend `FakeForge` |
| `src/detect.rs` | `FindingKind`, `Finding`, `Subject` | Add `ChecksFailing`, `WrongBase`, `Draft` variants |
| `src/commands/status.rs` | Per-branch gather + render + `branch_findings` | Ahead/behind split; render draft, checks, base; new findings |
| `src/jj.rs` | jj access | Add `descendants_of_in`, `branches_containing` |
| `src/detect/superseded.rs` | **new** — commits carried into another upstream branch | Create |
| `src/detect/overlap.rs` | **new** — two branches touching the same paths | Create |
| `tests/forge_contract.rs` | **new** — `gh` JSON shape vs our deserialiser | Create |

## Why a forge contract test (Task 0 exists for a reason)

Every forge bug found in the last week passed a green suite: `--state open` hid closed pull requests, ownership was inferred from a branch name, mergeability was never requested. All three were invisible because every forge test asserted against a JSON fixture written by the same person who wrote the assumption. `162 tests passing` caught none of them. Task 0 records real `gh` output once, as a checked-in fixture, so a field we stop requesting or a shape that changes fails a test instead of silently degrading a report.

---

### Task -1: Foundations the nine tasks lean on

Targeted, not a re-architecture. Each item below is tied to a task that would otherwise
trip over it: six of the nine tasks add code to `gather` and `render`, which both sit at
the clippy 100-line ceiling; two tasks say "add the new field to every literal, find them
with grep"; and one live bug sits exactly where Task 7 builds.

**Files:**
- Modify: `src/commands/sync.rs` (state classification bug, dead parameter)
- Modify: `src/commands/status.rs` (branch-line extraction, row construction)
- Modify: `src/forge.rs` (fixture dedup, method rename)
- Modify: `src/detect.rs` (stale doc, stale test)

- [ ] **Step 1: Fix the sync state classification (failing test first)**

`sync_repo` hardcodes `"OPEN"` for any tracked number found in the list. That was
correct when the list was `--state open` — vanishing meant merged or closed. The list is
`--state all` now, so merged and closed pull requests are IN it, and a merged tracked
pull request classifies as `unchanged` or `advanced` forever. That is the stale-fact
class this tool exists to prevent, and Task 7 builds on sync.

Requirement: use the listed pull request's own `state` field; keep the
`pull_request_state` fallback only for numbers absent from the list. Extract the state
selection into a pure function if that is what it takes to write a unit test that fails
before the fix and passes after: a listed MERGED pull request must classify as merged.
Note in the report what `knives sync` prints on a real repo afterwards — merged and
closed historical pull requests will now say so, and whether that is signal or noise is
a judgment for the owner, not this task.

- [ ] **Step 2: Extract the branch-line renderer**

`render` in `src/commands/status.rs` is at the 100-line ceiling and Tasks 1, 2 and 3
each add tokens to the branch line. Extract the per-row token builder (the `bits`
construction inside the branch loop) into its own function, e.g.
`fn branch_line(row: &BranchRow) -> String`. Behaviour identical: every existing test
stays green, unchanged.

- [ ] **Step 3: One way to build a bare `BranchRow`**

The row is built in three places — the `gather` loop, `divergent_rows`, and the `row()`
test helper. Add a constructor taking the values every site has (name and tip) and
defaulting the rest.

Use it at `divergent_rows` and `row()`, which genuinely want the defaults, via
struct-update syntax where a site sets more.

**Do not use it in `gather`.** `gather` keeps its explicit struct literal, deliberately.
Every field a later task adds to `BranchRow` is a field `gather` must COMPUTE — Task 2's
`ahead_of_origin` is derived there from the tips — and an explicit literal is what makes
the compiler refuse to build until `gather` decides how. Defaulting it there, whether by
constructor or by struct-update, converts a compile error into a silently wrong fact in
the report. So the dedup buys exactly what it should: the two sites that do not care
about a new field stop needing an edit, and the one site that must care still cannot
forget. (Amended after the Task -1 review found the first draft's "use it at all three
sites" traded that guard away two tasks before `ahead_of_origin` needs it.)

- [ ] **Step 4: One `PullRequest` fixture**

Five test literals across `src/commands/status.rs`, `src/commands/sync.rs` and
`src/forge.rs` spell out every field; Task 3 says "add the two new fields to every
literal". Add `#[cfg(test)] impl Default for PullRequest` — documented as fixture-only;
nothing under `tests/` constructs the type, verified — and rewrite the literals to
struct-update syntax, keeping the local helper functions. New forge fields then touch
one site.

- [ ] **Step 5: Rename `Forge::open_pull_requests` to `pull_requests`**

It fetches `--state all`; the name lies. Trait, both implementations, every call site.

- [ ] **Step 6: Hygiene**

- Delete the stale doc block on `Finding` in `src/detect.rs` that still says "remedy is
  not optional … the type makes omitting it impossible": it sits directly above the
  derive and contradicts the design decision recorded below it.
- Remove the dead `ours` parameter from `tracked_pull_requests` in
  `src/commands/sync.rs` (`let _ = ours;`), its call site, and its tests.
- Extend `every_kind_renders_a_stable_label` in `src/detect.rs` to cover every
  `FindingKind` variant: it lists five of seven, and the tasks after this add four more.

- [ ] **Step 7: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Also build the release binary and run `knives status` and `knives sync` against one real
repo; the output must be unchanged except for the sync states Step 1 corrects.

Then extend the single commit's description with what this task added: `refactor: foundations for the mechanical checks (sync state fix, render extraction, fixture dedup)`.
Do not create a second commit.

---

### Task 0: Pin the real `gh` JSON shape

**Files:**
- Create: `tests/fixtures/gh_pr_list.json`
- Create: `tests/forge_contract.rs`

**Interfaces:**
- Consumes: `knives::forge::{parse_pull_requests, PullRequest}`
- Produces: nothing for later tasks; a gate that every later task's new field must survive.

- [ ] **Step 1: Capture real forge output as a fixture**

Run, from any managed repo checkout:

```bash
cd ~/forks/libcore/default
gh pr list --state all --limit 3 --json \
  number,state,reviewDecision,headRefName,headRefOid,updatedAt,isDraft,url,headRepositoryOwner,mergeable,mergeStateStatus,baseRefName,statusCheckRollup \
  > /home/ubuntu/knives/default/tests/fixtures/gh_pr_list.json
```

Then scrub identity, because `tests/no_hardcoded_identity.rs` forbids forge URLs under `src/` and we keep fixtures to the same standard:

```bash
cd /home/ubuntu/knives/default
python3 - <<'PY'
import json, pathlib, re
p = pathlib.Path("tests/fixtures/gh_pr_list.json")
text = re.sub(r"https://[^\"]*github\.com", "https://forge.invalid", p.read_text())
data = json.loads(text)
for pr in data:
    if pr.get("headRepositoryOwner"):
        pr["headRepositoryOwner"]["login"] = "our-org"
p.write_text(json.dumps(data, indent=2) + "\n")
PY
```

- [ ] **Step 2: Write the failing test**

Create `tests/forge_contract.rs`:

```rust
//! The shape `gh` actually returns, not the shape we assumed.
//!
//! Every forge defect found so far passed a suite of hand-written fixtures: only open
//! pull requests were requested, ownership was inferred from a branch name, mergeability
//! was never asked for. Each was invisible because the fixture and the assumption had the
//! same author. This asserts against recorded real output instead.

use knives::forge::parse_pull_requests;

const RECORDED: &str = include_str!("fixtures/gh_pr_list.json");

#[test]
fn every_field_we_request_survives_a_real_payload() {
    let parsed = parse_pull_requests(RECORDED).expect("recorded gh output must deserialise");
    assert!(!parsed.is_empty(), "the fixture must contain pull requests");

    // A field silently dropped from PR_FIELDS deserialises as its default forever, and
    // the report degrades with nothing failing. Assert each is populated somewhere.
    assert!(parsed.iter().any(|pr| pr.number > 0), "number");
    assert!(parsed.iter().any(|pr| !pr.state.is_empty()), "state");
    assert!(parsed.iter().any(|pr| !pr.head_ref_name.is_empty()), "headRefName");
    assert!(parsed.iter().any(|pr| !pr.head_ref_oid.is_empty()), "headRefOid");
    assert!(parsed.iter().any(|pr| !pr.updated_at.is_empty()), "updatedAt");
    assert!(parsed.iter().any(|pr| !pr.url.is_empty()), "url");
    assert!(parsed.iter().any(|pr| !pr.mergeable.is_empty()), "mergeable");
    assert!(parsed.iter().any(|pr| pr.head_repository_owner.is_some()), "headRepositoryOwner");
}

#[test]
fn the_request_asks_for_every_field_the_type_holds() {
    // The failure this prevents: adding a field to PullRequest and forgetting PR_FIELDS,
    // so it deserialises as its default and every report quietly reads "not set".
    for field in [
        "number",
        "state",
        "reviewDecision",
        "headRefName",
        "headRefOid",
        "updatedAt",
        "isDraft",
        "url",
        "headRepositoryOwner",
        "mergeable",
        "mergeStateStatus",
    ] {
        assert!(
            knives::forge::requested_fields().contains(field),
            "PR_FIELDS is missing {field}"
        );
    }
}
```

- [ ] **Step 3: Run it to make sure it fails**

Run: `cargo test --test forge_contract 2>&1 | tail -20`
Expected: FAIL — `cannot find function requested_fields in module knives::forge`

- [ ] **Step 4: Expose the requested fields**

In `src/forge.rs`, directly below the `PR_FIELDS` constant:

```rust
/// The fields we ask the forge for.
///
/// Exposed so a test can check the type and the request have not drifted apart: a field
/// added to `PullRequest` but not here deserialises as its default forever, and the report
/// degrades with nothing failing.
pub fn requested_fields() -> &'static str {
    PR_FIELDS
}
```

- [ ] **Step 5: Run the tests and make sure they pass**

Run: `cargo test --test forge_contract`
Expected: PASS, 2 tests

- [ ] **Step 6: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `test(forge): assert against recorded gh output, not hand-written fixtures`.
Do not create a second commit.

---

### Task 1: Report draft pull requests

`is_draft` is already requested and already deserialised. Nothing renders it, so the cheapest possible "this is not ready" signal is discarded.

**Files:**
- Modify: `src/commands/status.rs` (branch line renderer, near the `review_decision` push)

**Interfaces:**
- Consumes: `BranchRow.pull_request: Option<PullRequest>` with `is_draft: bool`
- Produces: nothing; a rendered token only.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/commands/status.rs`:

```rust
#[test]
fn a_draft_pull_request_says_so() {
    // Already requested from the forge and already deserialised, and nothing rendered it,
    // so the cheapest "not ready" signal there is was being thrown away.
    let mut pr = pull_request(7);
    pr.is_draft = true;
    let report = Report {
        repo: "demo".to_owned(),
        branches: vec![row("feat/alpha", None, Some(pr))],
        ..Report::default()
    };

    assert!(render(&report, false).contains("draft"), "was: {}", render(&report, false));
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_draft_pull_request_says_so`
Expected: FAIL — the rendered line has no `draft`

- [ ] **Step 3: Render it**

In the branch-line builder, inside the `Some(pr)` arm, after the review-decision push:

```rust
                    if pr.is_draft {
                        bits.push("draft".to_owned());
                    }
```

- [ ] **Step 4: Run the tests and make sure they pass**

Run: `cargo test --lib a_draft_pull_request_says_so`
Expected: PASS

- [ ] **Step 5: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(status): report draft pull requests`.
Do not create a second commit.

---

### Task 2: Split ahead from behind

`origin_tip != tip` currently renders `(behind)` in both directions. Local ahead of origin means unpushed work; local behind means the report's landed verdict is judging content the pull request does not contain. They are different situations and printing one word for both is a live bug.

**Files:**
- Modify: `src/jj.rs` (add `is_ancestor`)
- Modify: `src/commands/status.rs` (branch-line renderer)

**Interfaces:**
- Consumes: `Repo::resolve_commit`
- Produces: `pub fn is_ancestor(&self, ancestor: &CommitId, descendant: &CommitId) -> Result<bool, JjError>` on `Repo`, used by Task 5.

- [ ] **Step 1: Write the failing test**

Add to `tests/jj_integration.rs`:

```rust
#[test]
fn ancestry_is_answered_in_both_directions() {
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");
    let base = repo.resolve_commit("main").expect("main");

    assert!(repo.is_ancestor(&base, &tip).expect("base is behind tip"));
    assert!(!repo.is_ancestor(&tip, &base).expect("tip is not behind base"));
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --test jj_integration ancestry_is_answered`
Expected: FAIL — `no method named is_ancestor`

- [ ] **Step 3: Implement ancestry**

Already written and verified while planning: it compiles, and against a real repository it
answers `Ok(true)` for `main` being an ancestor of a live branch and `Ok(false)` for the
reverse. `Index::is_ancestor` returns `IndexResult<bool>`, not `bool`, which the first draft
of this plan got wrong.

In `src/jj.rs`, as a method on `Repo`:

```rust
    /// Whether `ancestor` is reachable from `descendant`.
    ///
    /// Ahead and behind are different situations: local ahead of origin is unpushed work,
    /// local behind origin means a replay judges content the pull request does not have.
    /// The renderer printed one word for both until this existed.
    pub fn is_ancestor(
        &self,
        ancestor: &CommitId,
        descendant: &CommitId,
    ) -> Result<bool, JjError> {
        let ancestor = self.commit(ancestor.as_str())?;
        let descendant = self.commit(descendant.as_str())?;
        self.repo
            .index()
            .is_ancestor(ancestor.id(), descendant.id())
            .map_err(|error| JjError::Revision {
                revision: descendant.id().to_string(),
                detail: error.to_string(),
            })
    }
```

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --test jj_integration ancestry_is_answered`
Expected: PASS

- [ ] **Step 5: Write the failing renderer test**

Add to the `tests` module in `src/commands/status.rs`:

```rust
#[test]
fn ahead_of_origin_is_not_reported_as_behind_it() {
    // One word for both directions was a live bug: unpushed local work and a local copy
    // that is stale read identically, and only one of them invalidates a landed verdict.
    let mut ahead = row("feat/alpha", None, None);
    ahead.tip = Some(CommitId::new("aaaaaaaaaaaa"));
    ahead.origin_tip = Some(CommitId::new("bbbbbbbbbbbb"));
    ahead.ahead_of_origin = Some(true);
    let report = Report {
        repo: "demo".to_owned(),
        branches: vec![ahead],
        ..Report::default()
    };

    let out = render(&report, false);
    assert!(out.contains("unpushed-commits"), "was: {out}");
    assert!(!out.contains("(behind)"), "ahead is not behind: {out}");
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --lib ahead_of_origin_is_not_reported`
Expected: FAIL — no field `ahead_of_origin`

- [ ] **Step 7: Add the field and render it**

**Amended after the Task 2 review.** The first draft used `Option<bool>` and
`is_ancestor(origin, tip).ok()`. That has three states and history has four, and the two
extra cases came out inverted: `Ok(false)` is returned both when local is genuinely behind
AND when the histories have forked, so a true divergence rendered `(behind)` — the very
one-word-for-two-situations conflation this task exists to remove, surviving one branch
over. Meanwhile `Err` became `None` and rendered `(diverged)`, announcing a claim about
history when what actually happened was that a commit would not resolve. In a
fork-maintenance tool the forked case is the common one — a branch rewritten after being
pushed leaves local and origin mutually unreachable — and `(behind)` is exactly the word
that tells a reader to trust origin over their own unpushed work.

So the field carries the relation itself, and a failure to determine it goes to
`report.problems` rather than being rendered as a fact. That follows the precedent already
in this codebase: `LandedVerdict::Unjudged` renders `landed?` because refusing to judge
beats guessing.

In `src/commands/status.rs`, add:

```rust
/// How local relates to origin when the two differ.
///
/// Named states rather than a boolean because history has four cases and a boolean has
/// two: ahead, behind, forked, and could-not-tell. Collapsing the last two into a
/// boolean's `None` reported a fork as `(behind)`, which is the conflation this type
/// exists to prevent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OriginRelation {
    /// Local carries commits origin does not: unpushed work.
    Ahead,
    /// Origin carries commits local does not, so a replay judges content the pull request
    /// does not contain.
    Behind,
    /// Neither tip is reachable from the other. Usual cause is a rewrite after a push.
    Diverged,
}
```

and to `BranchRow`:

```rust
    /// How local relates to origin, when they differ and it could be determined. `None`
    /// means the tips match, there is no origin ref, or ancestry could not be resolved —
    /// and in that last case a problem is recorded, so the report says so rather than
    /// implying a relation.
    pub origin_relation: Option<OriginRelation>,
```

Populate it where the row is built, after `origin_tip` is read. Two ancestry queries, not
one — that is what separates behind from forked:

```rust
        let origin_relation = match &origin_tip {
            Some(origin) if origin != &tip => {
                match (repo.is_ancestor(origin, &tip), repo.is_ancestor(&tip, origin)) {
                    (Ok(true), _) => Some(OriginRelation::Ahead),
                    (Ok(false), Ok(true)) => Some(OriginRelation::Behind),
                    (Ok(false), Ok(false)) => Some(OriginRelation::Diverged),
                    // Report the failure instead of rendering a relation it did not establish.
                    (Err(error), _) | (Ok(false), Err(error)) => {
                        report.problems.push(format!(
                            "cannot tell how {branch} relates to origin: {error}"
                        ));
                        None
                    }
                }
            }
            Some(_) | None => None,
        };
```

Replace the origin arm of the branch-line renderer. The unresolved case says it is
unresolved; it does not name a relation:

```rust
    match (&row.origin_tip, &row.tip) {
        (None, _) => bits.push("unpushed".to_owned()),
        (Some(origin), Some(tip)) if origin != tip => {
            bits.push(match row.origin_relation {
                Some(OriginRelation::Ahead) => "unpushed-commits".to_owned(),
                Some(OriginRelation::Behind) => {
                    format!("origin={} (behind)", short(origin.as_str()))
                }
                Some(OriginRelation::Diverged) => {
                    format!("origin={} (diverged)", short(origin.as_str()))
                }
                None => format!("origin={} (unresolved)", short(origin.as_str())),
            });
        }
        (Some(_), _) => bits.push("pushed".to_owned()),
    }
```

`bare` supplies `origin_relation: None`, so `divergent_rows` and the `row()` test helper
need no edit. `gather` keeps its explicit literal and must populate the field — that is the
compile guard from Task -1 Step 3 doing its job.

Three things need a test that fails when they are removed: the ahead/behind distinction,
the behind/forked distinction, and **the problem the resolver reports when it cannot
determine a relation at all**. The third is easy to forget because it is a diagnostic rather
than a verdict — and a diagnostic nothing exercises is a line any future edit can delete in
silence, which is how the report loses the one sentence explaining an `(unresolved)` token.
Pass a commit id that cannot resolve and assert both halves: no relation, and a problem
naming the branch. The renderer test alone is not enough: the computation
in `gather` decides which of two opposite labels a reader sees, so an argument-order swap
in `is_ancestor` would invert every label silently. Pin the direction convention with an
integration test on real geometry (`lab.branch` + `lab.push_branch` + a further local
commit gives origin-as-ancestor-of-local).

- [ ] **Step 8: Run the whole suite**

Run: `cargo nextest run --all-targets --all-features --workspace`
Expected: PASS. `a_branch_whose_origin_tip_differs_is_shown_as_behind` may need its fixture to set `ahead_of_origin: Some(false)`; update it rather than weakening the assertion.

- [ ] **Step 9: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `fix(status): tell ahead of origin from behind it`.
Do not create a second commit.

---

### Task 3: Report CI status

**Files:**
- Modify: `src/forge.rs` (`PR_FIELDS`, `ChecksSummary`, `PullRequest.checks`, `FakeForge`)
- Modify: `src/detect.rs` (`FindingKind::ChecksFailing`)
- Modify: `src/commands/status.rs` (render + finding)
- Modify: `tests/forge_contract.rs` (assert the new field)

**Interfaces:**
- Produces: `PullRequest.checks: ChecksSummary` with `pub fn failing(&self) -> bool` and `pub fn ran(&self) -> bool`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/forge.rs`:

```rust
    #[test]
    fn a_failing_check_is_told_from_one_that_never_ran() {
        use super::{ChecksSummary, parse_pull_requests};
        // Never ran and failed are different problems. `UNKNOWN` and an empty rollup are
        // also different from a failure: the forge computes these asynchronously, so
        // treating "not yet" as "broken" would cry wolf on every push.
        let payload = r#"[{"number":1,"state":"OPEN","headRefName":"a","headRefOid":"x",
            "updatedAt":"","isDraft":false,"url":"","mergeable":"MERGEABLE",
            "statusCheckRollup":[{"conclusion":"FAILURE","name":"build"},
                                 {"conclusion":"SUCCESS","name":"lint"}]},
           {"number":2,"state":"OPEN","headRefName":"b","headRefOid":"y",
            "updatedAt":"","isDraft":false,"url":"","mergeable":"MERGEABLE",
            "statusCheckRollup":[]}]"#;
        let parsed = parse_pull_requests(payload).expect("parse");

        assert!(parsed[0].checks.failing(), "a FAILURE conclusion is failing");
        assert_eq!(parsed[0].checks.failed, vec!["build".to_owned()]);
        assert!(parsed[0].checks.ran());

        assert!(!parsed[1].checks.failing(), "an empty rollup is not a failure");
        assert!(!parsed[1].checks.ran(), "an empty rollup means nothing ran");
        assert_eq!(ChecksSummary::default().failing(), false);
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_failing_check_is_told_from_one`
Expected: FAIL — no field `checks`

- [ ] **Step 3: Implement the summary**

In `src/forge.rs`, extend `PR_FIELDS` to end with `,baseRefName,statusCheckRollup`, then add:

```rust
/// One check the forge ran, as much of it as we read.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    /// `SUCCESS`, `FAILURE`, `SKIPPED`, `CANCELLED`, or empty while still running.
    #[serde(default)]
    pub conclusion: String,
}

/// What the forge's checks say about a pull request.
///
/// A pull request with red CI reads as finished from every other angle, which is the case
/// this exists for. Never-ran is kept distinct from failed: they are different problems,
/// and an empty rollup on a fresh push is not a failure.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(transparent)]
pub struct ChecksSummary {
    pub runs: Vec<CheckRun>,
}

impl ChecksSummary {
    /// Checks the forge reported a failing conclusion for.
    pub fn failed_names(&self) -> Vec<String> {
        self.runs
            .iter()
            .filter(|run| {
                // Every conclusion the forge shows as red, not just the obvious three. A
                // check the reader would see failing but this predicate misses renders as
                // clean green CI, which is the exact false-clean this task exists to
                // prevent. `CheckConclusionState` adds STARTUP_FAILURE (the workflow never
                // started) and ACTION_REQUIRED; a StatusContext carries a `StatusState`
                // instead, whose red value is ERROR — what external CI posting commit
                // statuses emits for an aborted or infrastructure-failed build.
                // Chained comparisons rather than `matches!` over an uppercased copy:
                // same six values, and no per-run String allocation.
                run.conclusion.eq_ignore_ascii_case("FAILURE")
                    || run.conclusion.eq_ignore_ascii_case("TIMED_OUT")
                    || run.conclusion.eq_ignore_ascii_case("CANCELLED")
                    || run.conclusion.eq_ignore_ascii_case("STARTUP_FAILURE")
                    || run.conclusion.eq_ignore_ascii_case("ACTION_REQUIRED")
                    || run.conclusion.eq_ignore_ascii_case("ERROR")
            })
            .map(|run| run.name.clone())
            .collect()
    }

    pub fn failing(&self) -> bool {
        !self.failed_names().is_empty()
    }

    /// Whether the forge ran anything at all. Nothing having run is not a failure.
    pub fn ran(&self) -> bool {
        !self.runs.is_empty()
    }
}
```

Add to `PullRequest` **only the cheap field**:

```rust
    /// The branch this pull request targets.
    #[serde(default)]
    pub base_ref_name: String,
```

**`checks` does NOT go on `PullRequest`, and `statusCheckRollup` does NOT go in `PR_FIELDS`.**
Measured on a real repository: adding it to the `--state all --limit 300` list query returns
HTTP 504 six times out of six, which costs the report every pull-request fact it had — the
list call is how `state`, `mergeable`, ownership and everything else arrives. The same query
without the field succeeds 3 of 3, and `gh pr view <n> --json statusCheckRollup` succeeds 3
of 3.

This is not a new pattern. Expensive per-pull-request facts already live on `BranchRow`, not
on `PullRequest` — `review_predates_head: Option<bool>` is exactly this shape, fetched by
`Forge::review_predates_head` for the branches being rendered. Checks are the same kind of
fact and get the same treatment:

```rust
// on the Forge trait
    /// What the forge's checks say about one pull request.
    ///
    /// Per pull request rather than in the list query, because `statusCheckRollup` there
    /// exceeds the forge's GraphQL budget and fails the whole call. Called only for the
    /// branches we render, which is our own handful rather than the repository's hundreds.
    fn checks(&self, repo: &Path, number: u64) -> Result<Option<ChecksSummary>, ForgeError>;

// on BranchRow, beside review_predates_head
    /// What the forge's checks say, when they were asked for. `None` means not consulted —
    /// which is not the same as nothing having run, and must not render as a failure.
    pub checks: Option<ChecksSummary>,
```

`FakeForge` gets a `pub checks: BTreeMap<u64, ChecksSummary>` and returns the lookup.
`bare` supplies `checks: None`, so `divergent_rows` and the `row()` helper need no edit;
`gather` keeps its explicit literal and must populate it.

Only `base_ref_name` needs adding to `PullRequest` literals, and the `#[cfg(test)] Default`
covers those that use struct-update syntax.

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --lib a_failing_check_is_told_from_one`
Expected: PASS

- [ ] **Step 5: Write the failing finding test**

Add to the `tests` module in `src/commands/status.rs`:

```rust
#[test]
fn failing_checks_are_reported_and_an_empty_rollup_is_not() {
    let mut red = pull_request(11);
    red.checks = knives::forge::ChecksSummary {
        runs: vec![knives::forge::CheckRun {
            name: "build".to_owned(),
            conclusion: "FAILURE".to_owned(),
        }],
    };
    let findings = branch_findings(&[row("feat/alpha", None, Some(red))]);
    let found = findings
        .iter()
        .find(|finding| finding.kind == FindingKind::ChecksFailing)
        .expect("a failing check must be reported");
    assert!(found.detail.contains("build"), "name the check: {}", found.detail);

    // Nothing having run is not a failure; the forge runs these asynchronously.
    let quiet = pull_request(12);
    assert!(
        !branch_findings(&[row("feat/beta", None, Some(quiet))])
            .iter()
            .any(|finding| finding.kind == FindingKind::ChecksFailing)
    );
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --lib failing_checks_are_reported`
Expected: FAIL — no variant `ChecksFailing`

- [ ] **Step 7: Add the variant, the finding and the rendered token**

In `src/detect.rs`, add `ChecksFailing,` to `FindingKind` and `Self::ChecksFailing => "checks-failing",` to its `Display`.

In `branch_findings`, beside the `Unmergeable` block:

```rust
        if let (Some(pr), Some(checks)) = (row.pull_request.as_ref(), row.checks.as_ref())
            && checks.failing()
        {
            findings.push(Finding::new(
                FindingKind::ChecksFailing,
                Subject::PullRequest(pr.number),
                format!(
                    "#{} has failing checks: {}",
                    pr.number,
                    checks.failed_names().join(", ")
                ),
            ));
        }
```

In the branch-line renderer, inside the `Some(pr)` arm:

```rust
                    if row.checks.as_ref().is_some_and(ChecksSummary::failing) {
                        bits.push("checks-failing".to_owned());
                    } else if row.checks.as_ref().is_some_and(|c| !c.ran()) {
                        bits.push("no-checks".to_owned());
                    }
```

- [ ] **Step 8: Extend the contract test**

In `tests/forge_contract.rs`, add to `every_field_we_request_survives_a_real_payload`:

```rust
    assert!(
        parsed.iter().any(|pr| !pr.base_ref_name.is_empty()),
        "baseRefName"
    );
    assert!(
true,
        "statusCheckRollup is no longer requested in the list query; checks are fetched per pull request"
    );
```

and add `"baseRefName"` to the field list (NOT `"statusCheckRollup"` — it is deliberately not requested there) in `the_request_asks_for_every_field_the_type_holds`.

- [ ] **Step 9: Run everything**

Run: `cargo nextest run --all-targets --all-features --workspace && cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: PASS, clean

- [ ] **Step 10: Verify against a real repo, not a fixture**

```bash
cargo build --release
cd ~/forks/libcore/default
/home/ubuntu/knives/default/target/release/knives status --text 2>&1 | grep -E "checks-failing|no-checks" | head
gh pr list --repo <owner>/<repo> --state open --limit 100 \
  --json number,headRefName,statusCheckRollup,headRepositoryOwner \
  --jq '.[] | select(.headRepositoryOwner.login=="our-org")
        | "\(.number) \([.statusCheckRollup[]?.conclusion] | join(","))"'
```

Expected: the branches knives marks `checks-failing` are exactly those `gh` reports a `FAILURE` for. If they disagree, the deserialiser is wrong — fix it before committing.

- [ ] **Step 11: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(status): report failing checks, and tell them from checks that never ran`.
Do not create a second commit.

---

### Task 4: Report a pull request opened against the wrong base

A pull request aimed at a branch other than the one upstream expects sits there looking open
and healthy while the people who would review it never see it on their queue.

**Amended after the Task 4 review, which found the first draft's motivating example was one
this check cannot witness.** The draft said "targeting our fork's `main` instead of the
upstream default branch". Both branches are named `main`, and `base_ref_name` is a branch
name only — the forge offers `headRepository`, `headRepositoryOwner` and `isCrossRepository`,
but **no base-repository field at all**. Worse, measured: `gh` invoked from a fork checkout
resolves to the *upstream* repository (`gh repo set-default --view` in a managed checkout
prints `<owner>/<repo>`), so a pull request opened from our fork to our own fork lives on
origin and never appears in the queried list. The case is structurally unreachable here, not
merely unimplemented.

So this check witnesses exactly one thing: **a base branch whose NAME differs from the one
configured for the repo** — `develop`, or a stale `release/2026-07-28`. That is a real
mistake and worth reporting. Do not claim more than that in the code comments; a comment
asserting the fork-main rationale above code that cannot detect it is exactly the kind of
false statement this project treats as a defect.

Catching "opened against our own fork" needs a second query against origin's own pull request
list, where `head_repository_owner == ours && !is_cross_repository` identifies it. That is a
different check with its own forge call, and it is not this task.

`base_ref_name` arrived with Task 3.

**Files:**
- Modify: `src/detect.rs` (`FindingKind::WrongBase`)
- Modify: `src/commands/status.rs` (finding)
- Modify: `src/config.rs` (`RepoEntry::default_base`)

**Interfaces:**
- Consumes: `PullRequest.base_ref_name`, `Options.registry`
- Produces: `RepoEntry::default_base(&self) -> &str`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `src/config.rs`:

```rust
    #[test]
    fn the_expected_base_is_the_trunk_unless_stated() {
        // A pull request against our own fork's main never reaches the maintainer, so the
        // expected base has to be knowable. Configurable because not every upstream calls
        // its default branch main.
        let dir = tempfile::tempdir().unwrap();
        let plain = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n";
        let registry = load(&write(dir.path(), plain)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "main");

        let stated = "[repos.demo]\npath = \"/tmp/d\"\nupstream = \"u\"\norigin = \"o\"\n\
                      base = \"develop\"\n";
        let registry = load(&write(dir.path(), stated)).unwrap();
        assert_eq!(registry.repos["demo"].default_base(), "develop");
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib the_expected_base_is_the_trunk`
Expected: FAIL — no method `default_base`

- [ ] **Step 3: Implement it**

Add to `RepoEntry` in `src/config.rs`:

```rust
    /// The branch upstream expects pull requests against. Defaults to `main`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
```

and:

```rust
    /// The branch a pull request from this repo should target.
    ///
    /// Configurable because not every upstream calls its default branch `main`, and a
    /// pull request opened against our own fork never reaches the maintainer.
    pub fn default_base(&self) -> &str {
        self.base.as_deref().unwrap_or("main")
    }
```

Add `base: None,` to every `RepoEntry` literal:

```bash
grep -rn "RepoEntry {" src/ tests/ | grep -v "pub struct"
```

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --lib the_expected_base_is_the_trunk`
Expected: PASS

- [ ] **Step 5: Write the failing finding test**

Add to the `tests` module in `src/commands/status.rs`:

```rust
#[test]
fn a_pull_request_against_the_wrong_base_is_reported() {
    let mut wrong = pull_request(21);
    wrong.base_ref_name = "release/2026-07-28".to_owned();
    let findings = wrong_base_findings(&[row("feat/alpha", None, Some(wrong))], "main");
    let found = findings
        .iter()
        .find(|finding| finding.kind == FindingKind::WrongBase)
        .expect("a wrong base must be reported");
    assert!(found.detail.contains("release/2026-07-28"), "was: {}", found.detail);
    assert!(found.detail.contains("main"), "name the expected base: {}", found.detail);

    let mut right = pull_request(22);
    right.base_ref_name = "main".to_owned();
    assert!(wrong_base_findings(&[row("feat/beta", None, Some(right))], "main").is_empty());

    // Unknown is not wrong: an empty base means the forge did not say.
    let quiet = pull_request(23);
    assert!(wrong_base_findings(&[row("feat/gamma", None, Some(quiet))], "main").is_empty());
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --lib a_pull_request_against_the_wrong_base`
Expected: FAIL — no function `wrong_base_findings`

**Gate on the pull request being open.** The sibling three lines away already does
(`ChecksFailing` requires `pr.is_open()`), and for the same reason: pull requests are listed
`--state all`, rows come from live bookmarks, and "branch still here, pull request concluded"
is a state this tool exists to report. Without the gate a merged or closed pull request whose
base differed yields a permanent finding, and since any non-empty findings list returns
`Exit::Findings`, it holds a gate non-zero forever — about something already concluded, whose
stated justification ("never reaches the maintainer") is plainly false once it has merged.
Pin it with a test on a MERGED pull request.

- [ ] **Step 7: Implement it**

In `src/detect.rs` add `WrongBase,` to `FindingKind` and `Self::WrongBase => "wrong-base",` to its `Display`.

In `src/commands/status.rs`:

```rust
/// Pull requests aimed somewhere other than the branch upstream expects.
///
/// A pull request against our own fork's main never reaches the maintainer. An empty base
/// means the forge did not say, which is not the same as wrong.
fn wrong_base_findings(rows: &[BranchRow], expected: &str) -> Vec<Finding> {
    rows.iter()
        .filter_map(|row| {
            let pr = row.pull_request.as_ref()?;
            if pr.base_ref_name.is_empty() || pr.base_ref_name == expected {
                return None;
            }
            Some(Finding::new(
                FindingKind::WrongBase,
                Subject::PullRequest(pr.number),
                format!(
                    "#{} targets {}, not {expected}",
                    pr.number, pr.base_ref_name
                ),
            ))
        })
        .collect()
}
```

Call it in `gather`, beside `branch_findings`:

```rust
    report
        .findings
        .extend(wrong_base_findings(&report.branches, entry.default_base()));
```

- [ ] **Step 8: Run everything**

Run: `cargo nextest run --all-targets --all-features --workspace && cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: PASS, clean

- [ ] **Step 9: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(status): report a pull request opened against the wrong base`.
Do not create a second commit.

---

### Task 5: Report commits carried into another upstream branch

Your third superseded case: the maintainer made their own branch and your commits are in it, plus extras. Mechanically, your tip is an ancestor of some upstream ref that is not the trunk.

**Files:**
- Create: `src/detect/superseded.rs`
- Modify: `src/detect.rs` (declare the module, add `FindingKind::CarriedElsewhere`)
- Modify: `src/jj.rs` (`branches_containing`)
- Modify: `src/commands/status.rs` (call it)
- Modify: `tests/jj_integration.rs`

**Interfaces:**
- Consumes: `Repo::is_ancestor` from Task 2, `Repo::bookmark_tips`
- Produces: `Repo::branches_containing(&self, commit: &CommitId) -> Result<Vec<BookmarkRef>, JjError>`; `superseded::carried_elsewhere(branch: &BranchName, carriers: &[BookmarkRef]) -> Option<Finding>`

- [ ] **Step 1: Write the failing detector test**

Create `src/detect/superseded.rs`:

```rust
//! Work that has been carried somewhere else.
//!
//! The maintainer making their own branch out of your commits looks like nothing at all
//! from the branch's own point of view: it is not merged, not conflicted, and still open.
//! What it is, mechanically, is a tip reachable from some other reference.

use crate::detect::{Finding, FindingKind, Subject};
use crate::ids::{BookmarkRef, BranchName};

/// A branch whose tip is reachable from references other than its own.
///
/// Says where it was found and nothing about what it means: whether the maintainer took
/// the work, rebased it, or coincidentally landed the same content is exactly the judgment
/// this tool leaves to the reader.
pub fn carried_elsewhere(branch: &BranchName, carriers: &[BookmarkRef]) -> Option<Finding> {
    if carriers.is_empty() {
        return None;
    }
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();
    Some(Finding::new(
        FindingKind::CarriedElsewhere,
        Subject::Branch(branch.clone()),
        format!(
            "{branch}'s tip is also reachable from {}",
            named.join(", ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::RemoteName;

    #[test]
    fn a_tip_reachable_from_another_reference_is_reported_with_its_carriers() {
        let branch = BranchName::new("feat/alpha");
        let carrier = BookmarkRef::Remote {
            branch: BranchName::new("maintainer/rework"),
            remote: RemoteName::new("upstream"),
        };
        let finding = carried_elsewhere(&branch, &[carrier]).expect("a carrier is a finding");
        assert_eq!(finding.kind, FindingKind::CarriedElsewhere);
        assert!(finding.detail.contains("maintainer/rework@upstream"), "{}", finding.detail);
    }

    #[test]
    fn no_carriers_is_not_a_finding() {
        assert!(carried_elsewhere(&BranchName::new("feat/alpha"), &[]).is_none());
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib superseded`
Expected: FAIL — `file not found for module superseded` or no variant `CarriedElsewhere`

- [ ] **Step 3: Wire the module and variant**

In `src/detect.rs`: add `pub mod superseded;`, add `CarriedElsewhere,` to `FindingKind`, and `Self::CarriedElsewhere => "carried-elsewhere",` to its `Display`.

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --lib superseded`
Expected: PASS, 2 tests

- [ ] **Step 5: Write the failing jj test**

Add to `tests/jj_integration.rs`:

```rust
#[test]
fn a_tip_carried_into_another_branch_is_found() {
    // The maintainer's own branch built on our commits: not merged, not conflicted, still
    // open, and invisible from the branch's own point of view.
    let lab = lab::Lab::new();
    lab.branch("feat/alpha", "alpha.txt", "alpha\n");
    let repo = knives::jj::Repo::open(&lab.work).expect("open");
    let tip = repo.resolve_commit("feat/alpha").expect("tip");

    // A second bookmark whose history includes that tip.
    lab.jj_work(["bookmark", "create", "theirs/rework", "-r", "feat/alpha"]);
    lab.jj_work(["new", "theirs/rework", "-m", "extra work on top"]);
    lab.jj_work(["bookmark", "set", "theirs/rework", "-r", "@"]);

    let repo = knives::jj::Repo::open(&lab.work).expect("reopen");
    let carriers = repo.branches_containing(&tip).expect("carriers");
    let named: Vec<String> = carriers.iter().map(ToString::to_string).collect();

    assert!(named.iter().any(|name| name.contains("theirs/rework")), "was: {named:?}");
    assert!(
        !named.iter().any(|name| name == "feat/alpha"),
        "a branch does not carry itself: {named:?}"
    );
}
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --test jj_integration a_tip_carried_into_another_branch`
Expected: FAIL — `no method named branches_containing`

- [ ] **Step 7: Implement it**

In `src/jj.rs`, on `Repo`:

```rust
    /// Bookmarks whose history includes `commit`, excluding any pointing exactly at it.
    ///
    /// Answers where work went when it was not merged: a maintainer building their own
    /// branch on our commits leaves the branch itself untouched, so the only trace is that
    /// its tip is reachable from somewhere else.
    pub fn branches_containing(&self, commit: &CommitId) -> Result<Vec<BookmarkRef>, JjError> {
        let mut found = Vec::new();
        for (reference, tip) in self.bookmark_tips()? {
            if &tip == commit {
                continue;
            }
            // Our own releases carry our own branches BY CONSTRUCTION — a cut is a flat
            // octopus merge of these very tips — so every carried branch is trivially
            // reachable from every release containing it. Reporting that says nothing the
            // reader does not already know, and it buries the case this check exists for.
            // Measured on a real repository before this filter: 10 findings, every carrier a
            // release or a `@git` ref, zero true positives.
            //
            // `@git` is jj's internal git-tracking view rather than a remote, and is
            // excluded everywhere else in this codebase for the same reason.
            if crate::commands::status::is_our_release(&reference)
                || matches!(&reference, BookmarkRef::Remote { remote, .. } if remote.as_str() == "git")
            {
                continue;
            }
            if self.is_ancestor(commit, &tip)? {
                found.push(reference);
            }
        }
        Ok(found)
    }
```

- [ ] **Step 8: Run it to make sure it passes**

Run: `cargo test --test jj_integration a_tip_carried_into_another_branch`
Expected: PASS

- [ ] **Step 9: Call it from status**

In `src/commands/status.rs`, in `gather`, after the branch rows exist:

```rust
    // Only for branches we still carry: a branch already in the trunk is landed, which is
    // a different and already-reported thing.
    for row in &report.branches {
        let Some(tip) = row.tip.as_ref() else { continue };
        if row.landed == Some(LandedVerdict::InTrunk) {
            continue;
        }
        let carriers = repo
            .branches_containing(tip)?
            .into_iter()
            .filter(|reference| reference.branch() != &row.name)
            .collect::<Vec<_>>();
        if let Some(finding) = crate::detect::superseded::carried_elsewhere(&row.name, &carriers) {
            report.findings.push(finding);
        }
    }
```

If `gather` exceeds 100 lines, extract this loop into `fn carried_findings(report: &Report, repo: &Repo) -> anyhow::Result<Vec<Finding>>` and call that.

- [ ] **Step 10: Run everything**

Run: `cargo nextest run --all-targets --all-features --workspace && cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: PASS, clean

- [ ] **Step 11: Verify on a real repo**

```bash
cargo build --release
cd ~/forks/libcore/default
/home/ubuntu/knives/default/target/release/knives status --text 2>&1 | grep -A3 carried-elsewhere | head -20
```

Expected: **no carrier is one of our own releases, and none is a `@git` ref.** Before those
were filtered, this check produced 10 findings on a real repository of which all 10 were noise: a
release cut is an octopus merge of the branch tips, so it contains them by construction, and
`@git` is jj's internal view of the same refs — so each release appeared up to three times
(local, `@origin`, `@git`). What survives the filter is what the check is for: a tip reachable
from somebody else's branch.

Any branch still reported must be genuinely reachable from the named reference. Confirm one by
hand:

```bash
jj --ignore-working-copy log -r 'feat/<reported>::<named-carrier>' --no-graph -T 'commit_id.short()' | head
```

- [ ] **Step 12: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(status): report a tip carried into another branch`.
Do not create a second commit.

---

### Task 6: Report branches that would conflict with each other

Two of our branches touching the same paths conflict at cut time. `changed_files` already exists in `src/jj.rs`.

**Files:**
- Create: `src/detect/overlap.rs`
- Modify: `src/detect.rs` (module + `FindingKind::BranchOverlap`)
- Modify: `src/commands/status.rs` (call it)

**Interfaces:**
- Consumes: `knives::jj::changed_files(repo: &Path, revision: &str) -> Result<Vec<String>, JjError>`
- Produces: `overlap::branch_overlaps(touching: &BTreeMap<String, Vec<String>>) -> Vec<Finding>`

- [ ] **Step 1: Write the failing test**

Create `src/detect/overlap.rs`:

```rust
//! Branches that touch the same files.
//!
//! Two branches editing one file conflict when a release merges them, and the cut is a bad
//! time to find out. This is a path comparison and nothing more: whether the edits actually
//! conflict is a question for whoever reads the report.

use std::collections::BTreeMap;

use crate::detect::{Finding, FindingKind, Subject};

/// Files touched by more than one branch, one finding per file.
///
/// One finding per file rather than per pair: three branches on one file is one fact about
/// that file, and three findings saying nearly the same thing is how a report becomes
/// unreadable.
pub fn branch_overlaps(touching: &BTreeMap<String, Vec<String>>) -> Vec<Finding> {
    let mut by_file: BTreeMap<&String, Vec<&String>> = BTreeMap::new();
    for (branch, files) in touching {
        for file in files {
            by_file.entry(file).or_default().push(branch);
        }
    }
    by_file
        .into_iter()
        .filter(|(_, branches)| branches.len() > 1)
        .map(|(file, branches)| {
            let named: Vec<String> = branches.iter().map(ToString::to_string).collect();
            Finding::new(
                FindingKind::BranchOverlap,
                Subject::File(file.clone()),
                format!("{file} is touched by {}", named.join(", ")),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touching(pairs: &[(&str, &[&str])]) -> BTreeMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(branch, files)| {
                (
                    (*branch).to_owned(),
                    files.iter().map(|file| (*file).to_owned()).collect(),
                )
            })
            .collect()
    }

    #[test]
    fn one_file_touched_by_two_branches_is_one_finding_naming_both() {
        let findings = branch_overlaps(&touching(&[
            ("feat/a", &["src/lib.rs", "README.md"]),
            ("feat/b", &["src/lib.rs"]),
        ]));
        assert_eq!(findings.len(), 1, "one per file, not per pair: {findings:?}");
        assert!(findings[0].detail.contains("src/lib.rs"));
        assert!(findings[0].detail.contains("feat/a"));
        assert!(findings[0].detail.contains("feat/b"));
    }

    #[test]
    fn three_branches_on_one_file_stay_one_finding() {
        let findings = branch_overlaps(&touching(&[
            ("feat/a", &["src/lib.rs"]),
            ("feat/b", &["src/lib.rs"]),
            ("feat/c", &["src/lib.rs"]),
        ]));
        assert_eq!(findings.len(), 1);
        assert!(findings[0].detail.contains("feat/c"));
    }

    #[test]
    fn files_touched_by_one_branch_are_not_findings() {
        assert!(
            branch_overlaps(&touching(&[
                ("feat/a", &["src/lib.rs"]),
                ("feat/b", &["src/main.rs"])
            ]))
            .is_empty()
        );
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib overlap`
Expected: FAIL — `file not found for module overlap` or no variant `BranchOverlap`

- [ ] **Step 3: Wire the module and variant**

In `src/detect.rs`: `pub mod overlap;`, add `BranchOverlap,` to `FindingKind`, and `Self::BranchOverlap => "branch-overlap",` to its `Display`.

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --lib overlap`
Expected: PASS, 3 tests

Add a tree-diff helper beside `changed_files` in `src/jj.rs`, rather than changing that
function's signature — `src/commands/wip.rs` calls it with a single revision and should keep
working:

```rust
/// Files that differ between two commits.
///
/// A tree diff, not a revset range. `jj diff -r 'A..B'` fails with "Cannot diff revsets with
/// gaps in" whenever B is not a clean descendant of A, which on a fork is the common case;
/// `--from`/`--to` compares two trees and always has an answer.
pub fn changed_files_between(repo: &Path, from: &str, to: &str) -> Result<Vec<String>, JjError>
```

Same flags as `changed_files` otherwise: `--ignore-working-copy`, `--name-only`, sorted and
deduplicated output.

- [ ] **Step 5: Call it from status**

In `src/commands/status.rs`, in `gather`, gated on the probe option because it costs one `jj diff` per branch:

```rust
    // **Amended after the Task 6 review**, which found two silences here.
    //
    // No probe gate. The first draft hid this behind `options.probe`, the flag for the
    // expensive landed replay, on the assumption that one `jj diff` per branch was costly.
    // Measured, it is not: a full `status` with this check is indistinguishable from one
    // without. Worse, `--no-landed` is documented as skipping "the landed probe, which
    // replays onto the trunk and cleans up" — so the gate made that help text a lie by
    // suppressing an unrelated finding kind, and zero findings under the flag was
    // indistinguishable from "checked, nothing shared". A cheap, independent fact should not
    // be coupled to another check's flag.
    //
    // A failed diff is REPORTED, not discarded. This check's whole output is "these files are
    // shared", so a branch that contributes no paths cannot appear in any overlap — the
    // finding goes silently missing and nothing says coverage was partial. That is reachable:
    // `changed_files` errors on any non-zero `jj diff`, and `main@upstream..<branch>` yields
    // "Cannot diff revsets with gaps in" on real checkouts. `unmet_dependencies` in this same
    // file already handles the identical shape by pushing to `problems`; follow it.
    let mut touching: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut unanswered: Vec<String> = Vec::new();
    for row in &report.branches {
        let Some(_) = row.tip.as_ref() else {
            // A divergent bookmark earns its own Divergence finding, but that does not tell
            // the reader it was left out of the path comparison.
            unanswered.push(format!(
                "cannot compare paths for {}: it has no single tip",
                row.name
            ));
            continue;
        };
        // A revset RANGE — `A..B`, in any spelling, including one rooted at `fork_point` —
        // refuses with "Cannot diff revsets with gaps in" whenever the branch is not a clean
        // descendant of the trunk. On a real fork that is most branches: 10 of 14 on
        // one real fork, and before the error was reported they were dropped silently, so the
        // check compared 4 branches while appearing to compare all of them.
        //
        // The fix is not a better range, it is not a range at all. `jj diff --from X --to Y`
        // is a TREE diff between two commits, which always has an answer. From the fork point
        // to the tip, that is exactly the files this branch changed since it left the trunk.
        //
        // Measured on the branch that fails hardest: the range form errors, the tree-diff form
        // returns 13 files.
        let from = format!("fork_point({UPSTREAM_TRUNK} | {0})", row.name);
        match crate::jj::changed_files_between(&entry.path, &from, row.name.as_str()) {
            Ok(files) => {
                let _ = touching.insert(row.name.to_string(), files);
            }
            Err(error) => {
                unanswered.push(format!("cannot compare paths for {}: {error}", row.name));
            }
        }
    }
    report.problems.extend(unanswered);
    report
        .findings
        .extend(crate::detect::overlap::branch_overlaps(&touching));
```

- [ ] **Step 6: Run everything**

Run: `cargo nextest run --all-targets --all-features --workspace && cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: PASS, clean

- [ ] **Step 7: Verify on a real repo and check the noise**

```bash
cargo build --release
cd ~/forks/libcore/default
time /home/ubuntu/knives/default/target/release/knives status --text 2>&1 | grep -c branch-overlap
```

Expected: a count, and a runtime you can live with. If a shared file like a changelog or lockfile dominates the output, that is the signal to stop here and discuss an ignore list rather than inventing one.

- [ ] **Step 8: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(status): report branches touching the same files`.
Do not create a second commit.

---

### Task 7: Report new comments since the last sync

Deliberately last: "unread" needs a reader, "new since we last looked" does not. `sync` already records per-pull-request state between runs, which is where the marker belongs.

**Files:**
- Modify: `src/forge.rs` (`Forge::newest_comment`, `CliForge`, `FakeForge`)
- Modify: `src/store.rs` (`comment_marks`, `record_comment_mark`)
- Modify: `src/commands/sync.rs` (report new comments)

**Interfaces:**
- Produces: `Forge::newest_comment(&self, repo: &Path, number: u64) -> Result<Option<String>, ForgeError>` returning an ISO-8601 timestamp; `Store::comment_mark(&self, repo: &RepoName, number: u64) -> Option<&str>`; `Store::record_comment_mark(&mut self, repo: &RepoName, number: u64, at: &str)`

- [ ] **Step 1: Write the failing store test**

Add to the `tests` module in `src/store.rs`:

```rust
    #[test]
    fn a_comment_mark_round_trips_and_is_scoped_to_its_repo() {
        // "Unread" would need a reader; "new since we last looked" needs only a mark, and
        // sync already records per-pull-request state between runs.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        {
            let mut store = Store::open_for_update(path.clone()).unwrap();
            store.record_comment_mark(&RepoName::new("ai"), 7, "2026-07-30T00:00:00Z");
            store.save().unwrap();
        }
        let store = Store::open(path).unwrap();
        assert_eq!(
            store.comment_mark(&RepoName::new("ai"), 7),
            Some("2026-07-30T00:00:00Z")
        );
        assert_eq!(store.comment_mark(&RepoName::new("fork A"), 7), None);
    }
```

- [ ] **Step 2: Run it to make sure it fails**

Run: `cargo test --lib a_comment_mark_round_trips`
Expected: FAIL — no method `record_comment_mark`

- [ ] **Step 3: Implement the store side**

Add to `State` in `src/store.rs`:

```rust
    /// When we last saw a comment on a pull request, keyed `<repo>#<number>`.
    ///
    /// Enough to answer "new since we last looked", which is the mechanical half of
    /// "unread": no reader identity, and nothing to be wrong about beyond a timestamp.
    #[serde(default)]
    pub comment_marks: BTreeMap<String, String>,
```

and the accessors:

```rust
    pub fn comment_mark(&self, repo: &RepoName, number: u64) -> Option<&str> {
        self.state
            .comment_marks
            .get(&format!("{repo}#{number}"))
            .map(String::as_str)
    }

    pub fn record_comment_mark(&mut self, repo: &RepoName, number: u64, at: &str) {
        let _ = self
            .state
            .comment_marks
            .insert(format!("{repo}#{number}"), at.to_owned());
    }
```

- [ ] **Step 4: Run it to make sure it passes**

Run: `cargo test --lib a_comment_mark_round_trips`
Expected: PASS

- [ ] **Step 5: Write the failing forge test**

Add to the `tests` module in `src/forge.rs`:

```rust
    #[test]
    fn the_newest_comment_is_the_latest_of_both_kinds() {
        use super::parse_newest_comment;
        // Review comments and issue comments are separate lists on the same pull request,
        // and reading only one silently halves the answer.
        let payload = r#"{"comments":[{"createdAt":"2026-07-20T00:00:00Z"}],
                          "reviews":[{"submittedAt":"2026-07-28T00:00:00Z"}]}"#;
        assert_eq!(
            parse_newest_comment(payload).unwrap().as_deref(),
            Some("2026-07-28T00:00:00Z")
        );

        let empty = r#"{"comments":[],"reviews":[]}"#;
        assert_eq!(parse_newest_comment(empty).unwrap(), None);
    }
```

- [ ] **Step 6: Run it to make sure it fails**

Run: `cargo test --lib the_newest_comment_is_the_latest`
Expected: FAIL — no function `parse_newest_comment`

- [ ] **Step 7: Implement the forge side**

In `src/forge.rs`:

```rust
#[derive(Deserialize)]
struct Timestamped {
    #[serde(default, alias = "submittedAt", alias = "createdAt")]
    at: String,
}

#[derive(Deserialize)]
struct CommentPayload {
    #[serde(default)]
    comments: Vec<Timestamped>,
    #[serde(default)]
    reviews: Vec<Timestamped>,
}

/// The newest of a pull request's comments and reviews.
///
/// Both, because they are separate lists on the same pull request and reading one halves
/// the answer.
pub fn parse_newest_comment(payload: &str) -> Result<Option<String>, ForgeError> {
    let parsed: CommentPayload = serde_json::from_str(payload)?;
    Ok(parsed
        .comments
        .iter()
        .chain(parsed.reviews.iter())
        .map(|item| item.at.clone())
        .filter(|at| !at.is_empty())
        .max())
}
```

**Consult checks only for pull requests where they can still matter.** The sibling this
mirrors, `review_predates_head_for`, guards on `!review_decision.is_empty()`; this needs the
equivalent. The list is `--state all`, so without a guard `no-checks` lands on a closed,
abandoned pull request whose branch is still local — announcing "CI has not run yet" about
something that will never run again, which is the same cry-wolf failure as treating UNKNOWN
as broken, at the other end of the lifecycle. It also wastes a `gh pr view` per settled
branch. Guard on the pull request being OPEN.

Add to the `Forge` trait, and implement on `CliForge` with
`Self::run(repo, &["pr", "view", &number.to_string(), "--json", "comments,reviews"])`
then `parse_newest_comment(&payload)`. On `FakeForge`, add
`pub newest_comments: BTreeMap<u64, String>` and return the lookup.

- [ ] **Step 8: Run it to make sure it passes**

Run: `cargo test --lib the_newest_comment_is_the_latest`
Expected: PASS

- [ ] **Step 9: Report it in sync**

In `src/commands/sync.rs`, for each tracked pull request, after its state is classified:

```rust
        // Only when the forge answered. A pull request we could not ask about is not one
        // with no new comments.
        if let Ok(Some(newest)) = forge.newest_comment(&entry.path, number) {
            let seen = store.comment_mark(&repo, number).unwrap_or("");
            if newest.as_str() > seen {
                report
                    .notes
                    .push(format!("#{number} has comment activity newer than the last sync"));
            }
            store.record_comment_mark(&repo, number, &newest);
        }
```

- [ ] **Step 10: Run everything**

Run: `cargo nextest run --all-targets --all-features --workspace && cargo clippy --all-targets --all-features --workspace -- -D warnings`
Expected: PASS, clean

- [ ] **Step 11: Verify twice on a real repo**

```bash
cargo build --release
cd ~/forks/sandbox-runner/default
/home/ubuntu/knives/default/target/release/knives sync --text 2>&1 | grep "comment activity"
/home/ubuntu/knives/default/target/release/knives sync --text 2>&1 | grep -c "comment activity"
```

Expected: the first run reports activity, the second reports none — the mark advanced. If the second run repeats the first, the mark is not being saved.

- [ ] **Step 12: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `feat(sync): report comment activity newer than the last sync`.
Do not create a second commit.

---

### Task 8: Update the reference documents

**Files:**
- Modify: `skills/using-knives/SKILL.md`
- Modify: `docs/design.md`

- [ ] **Step 1: Document every new token and finding**

In `skills/using-knives/SKILL.md`, under `knives status`, extend the branch-line description with: `draft`, `checks-failing`, `no-checks`, `unpushed-commits`, `behind-base`, `CONFLICTING`. Add to the findings list: `checks-failing`, `wrong-base`, `carried-elsewhere`, `branch-overlap`. State plainly that `no-checks` and an empty rollup are not failures, and that `carried-elsewhere` says where a tip was found and nothing about what it means.

- [ ] **Step 2: Record the checks in the design doc**

In `docs/design.md`, under `## Detection rules`, add one line per new check naming the field or graph query it rests on, so a reader can tell at a glance that none of them reasons.

- [ ] **Step 3: Confirm no stale command names**

Run: `grep -rnE 'knives (cut|wip|claim|release-claim) ' docs/ skills/ | grep -v release`
Expected: no output.

- [ ] **Step 4: Verify the tree is clean and record the work**

```bash
cargo fmt --all -- --check && \
  cargo clippy --all-targets --all-features --workspace -- -D warnings && \
  cargo nextest run --all-targets --all-features --workspace
```

Then extend the single commit's description with what this task added: `docs: record the mechanical branch and pull request checks`.
Do not create a second commit.

---

## Self-Review

**Spec coverage.** Divergence and would-conflict-with-upstream were already built and tested and are not in this plan. The nine items from the discussion map as: divergent (done), conflicts with upstream (done), conflicts with another WIP branch (Task 6), superseded by a push to your branch (partly done via ahead/behind, sharpened by Task 2), superseded by a rewrite (done via divergence), superseded by commits carried elsewhere (Task 5), changes-requested and no-reviews (done), failing CI (Task 3), unread comments (Task 7). The unlisted additions are draft (Task 1), wrong base (Task 4), and ahead-vs-behind (Task 2). The forge testing gap is Task 0.

**Not covered, deliberately.** "Approved, mergeable and not merged" and "branch identical to another branch" were raised but are not tasks: both are one-line additions once Tasks 3 and 5 exist, and neither has been discussed enough to specify. Raise them after Task 5 rather than guessing.

**Type consistency.** `ChecksSummary::failed_names()` is a method, not a field — the Task 3 test uses the method. `Repo::is_ancestor` is introduced in Task 2 and consumed by Task 5's `branches_containing`; Task 5 must not be started before Task 2. `RepoEntry::base` is the stored field and `default_base()` the accessor. `BranchRow::origin_relation` (an `Option<OriginRelation>`, amended from the first draft's `Option<bool>`) is added in Task 2; `bare` supplies its default, so only `gather`'s explicit literal needs it.

**Ordering.** Task -1 first (foundations the others assume), then Task 0, so every later field is covered by the contract test. Task 2 before Task 5. Tasks 1, 3, 4 are independent. Task 6 is independent. Task 7 last. Task 8 after all of them.
