# The notch ledger, and a faster status

Two PRs, independent of each other. PR 1 is the notch ledger: a persistent,
timestamped, per-repo record of what agents did and decided, so the next agent
does not rediscover a mysterious branch by archaeology. PR 2 makes `knives
status` stop taking forever: batch the forge round trips, parallelize the
landed probes, parallelize repos under `--all`.

The problem, observed: agents keep forgetting what other agents worked on.
"There's this weird branch, I don't know what it is. We pushed to this other
branch and I don't remember why. I don't know why this decision was made."
The only "why" knives records today is `Claim.why`, and `knives finish`
deletes the claim. Everything else in `state.json` is current intent with no
history.

## Decisions settled with Sami (2026-08-15)

- **Storage (revised 2026-08-16, supersedes Approach A's JSONL): one markdown
  file per entry with TOML frontmatter**, under a per-repo directory beside
  `state.json`. Sami's call mid-implementation: markdown is easier to search,
  easier to git-track. File-per-entry is the forced shape — markdown per
  subject or per repo would mean concurrent prose appends, reintroducing the
  interleaving problem and making "no entry is ever rewritten" unenforceable.
  A markdown file per subject and a growing `state.json` were considered and
  rejected at the original design stage for the same reasons.
- **Name: `notch`** — notches carved in a handle are a tally of history. One
  command, two moods: bare reads, `-m` writes.
- **Releases are first-class subjects**, not just branches. A release ref name
  is a subject like any branch name.
- **No hook injection.** Reading the ledger is intentional. The OpenCode
  plugin does not change.
- **Skills get updated** so agents know to read and write notches
  (`fork-work`, `using-knives`, `pr-preflight`).
- **Status breadcrumb**: last notch per branch — a JSON field always, one
  truncated token in text. Zero added runtime (local file read).
- **Status performance is in scope** (PR 2). Status *text legibility* is out
  of scope, named and deferred deliberately.
- **Companion detectors are out of scope** and recorded below: unowned
  release content at cut time, pin-vs-tip equality, ref integrity.

## Evidence gathered (this session)

From the heaviest current user of this workflow (a peer maintenance session,
session-workspace/work-order-A, via Envoy 2026-08-15), who hand-maintained exactly this ledger
across 6 forks and ~29 PRs in a session-local markdown file that dies with the
session:

- **Nine composition losses** (features present in a deployed release,
  vanished from a later cut) were found by content comparison; zero by
  metadata. The worst class: release-homed content owned by no member branch,
  lost silently across four consecutive cuts.
- **Stored dispositions rot; recorded judgments do not.** A branch census that
  inferred "not a release parent ⇒ unhomed" produced 54 findings of which 5
  were false and were corrected only by content comparison. A parity audit's
  finding was true at its recorded SHA and stale two hours later after an
  in-place release repair. Past-tense entries anchored to SHAs stay true;
  present-tense caches go wrong the moment upstream moves. The ledger
  therefore stores events and judgments with anchors, and never stores
  derived state — consistent with the existing store doctrine ("compute
  anything cheap; what lives here is intent").
- **Evidence fields are load-bearing.** Every audit claim that survived
  red-teaming cited a SHA or file:line; every false finding lacked content
  evidence.
- **The hand-ledger's entries fell into two natural kinds**: mechanical
  events (pushed @SHA, cut with parents, PR merged) and judgments/notes
  (superseded-by with the superseding SHA, parked-by-owner with the claim
  quote, promises made to reviewers). knives already witnesses the first
  kind in its own commands and throws them away.
- **`status` is serial end to end** (`src/commands/status.rs`): per open PR,
  two `gh` subprocesses (`review_predates_head`, `checks` — each a process
  spawn plus an HTTPS round trip); per branch, one in-process jj replay
  probe (the comment at `maintained_branches` already knows: "each cost a
  landed probe, which is most of the runtime"); under `--all`, repos run one
  after another. fork A alone: 36 branches, ~9 open PRs ≈ 18 serial gh calls
  plus 36 serial probes.

## PR 1: the notch ledger

Files: new `src/ledger.rs`, new `src/commands/notch.rs`, wiring in
`src/cli.rs`; auto-event appends in `src/commands/{claim,start,sync,release}.rs`
and wherever `track`/`depends` are handled in `src/main.rs`; breadcrumb in
`src/commands/status.rs`; skill and doc updates.

### 1.1 Data model

One entry is one JSON line:

```json
{"ts":"2026-08-15T22:14:03Z","owner":"session-owner...","subject":"feat/log-queue",
 "kind":"note","text":"superseded by #1157; upstream wanted the trait approach",
 "evidence":["06d778b9","organization A/fork A#1157"],"anchor":"6c42fe71","pr":1157}
```

| Field | Written by | Content |
|---|---|---|
| `ts` | auto | UTC timestamp, RFC 3339 |
| `owner` | auto | `current_owner()` — same resolution as claims (`KNIVES_OWNER` → Claude session ID → active-owner lookup → OS user) |
| `subject` | caller | ref name — branch or release ref; absent = repo-level entry |
| `kind` | auto | `event` (machine observed a knives command) or `note` (an agent asserted something) |
| `text` | caller / command | the entry body |
| `evidence` | caller, optional | free strings: SHAs, `file:line`, `repo#N`, URLs; may reference other repos |
| `anchor` | auto | the subject's tip commit at write time; omitted when unresolvable (branch since deleted) — the entry stays valid |
| `pr` | caller on write, otherwise auto | `--pr <n>` stamps a written entry; without it, `tracked_pulls` supplies the fallback without a forge call |

`kind` is deliberately two values, not three. The read-time question the
census evidence teaches is "did a machine observe this or did an agent assert
it?" — supersessions and parking arrive as events via `finish
--superseded-by` and `start --why`; everything an agent asserts is a note.
Asking writing agents to self-classify judgment-versus-note is a decision
burden with no read-time payoff.

`anchor` is the anti-rot mechanism and is never caller-supplied: a reader sees
the entry was written when the branch was at `6c42fe71`, sees the tip has
moved, and knows to re-verify rather than inherit a stale conclusion.

Schema evolution: entries are never rewritten, readers ignore unknown fields,
writers may add fields. No version number.

### 1.2 Storage

One file per entry: `~/.config/knives/ledger/<repo>/<stamp>-<suffix>.md`,
beside `state.json` (and syncable or git-trackable later — it is just files;
each entry is a new file, so a git history of the ledger directory is pure
additions). `<stamp>` is the entry's UTC timestamp compacted to a filename
(nanosecond precision), `<suffix>` four random hex characters; the timestamp
also lives in frontmatter as `ts`, which is authoritative for display.

File shape: TOML frontmatter between `+++` lines carrying the structured
fields (`ts`, `owner`, `subject`, `kind`, `evidence`, `anchor`, `pr` — same
model as 1.1), then the entry text as the markdown body. The body is the
free-prose field; frontmatter is the machine surface. The `toml` crate is
already a dependency (registry parsing), so no new dependency and no
hand-rolled parser.

Append-only becomes: entry files are immutable — never rewritten, never
deleted. A write completes a temporary file in the ledger directory, then
atomically persists it without replacement to its final name; no lockfile is
needed at all (collisions at nanosecond-plus-random resolution are a loud
error, not a retry loop). Reads scan the repo's directory in lexicographic
filename order, which is chronological; an unparseable entry file is a loud
error, unknown frontmatter keys are ignored (schema evolution unchanged).

No rotation, no retention policy: entries are ~300 bytes and growth is
irrelevant on any horizon that matters here.

### 1.3 Automatic events

Commands that already witness the skeleton append an `event` as part of their
existing mutation:

| Command | Event recorded (subject) |
|---|---|
| `start` / `claim` | claimed, with `--why` (branch) |
| `finish` / `release-claim` | claim released; `--superseded-by` captured when given (branch) |
| `track --pr/--fork-only/--forget` | statement changed (branch) |
| `depends --on` | dependency recorded (branch) |
| `release <name>` cut | the full parent set, branch names + SHAs, and the delta from the previous cut's parent set (release ref) |
| `sync` | one event per tracked PR that transitioned merged / closed / advanced (branch) |

A ledger append failure fails the command loudly. No silent half-write: both
the state file and the ledger live in the same directory, and a write that
can fail one can fail the other.

### 1.4 The `notch` command

Write:

```
knives notch <subject> -m "text" [--evidence <ref>]... [--pr <n>] [--repo <name>]
```

Read:

```
knives notch [<subject>] [--pr <n>] [--repo <name>]
```

- Bare `knives notch` prints recent entries across the current repo (last
  20). With a subject, the full chronology for that branch or release ref.
- `--pr <n>` filters reads on the stamped `pr` field; with `-m`, it stamps the
  write explicitly and otherwise `tracked_pulls` supplies the fallback.
- `--repo <name>` on both moods selects a different repository when the fact
  belongs in its ledger.
- Write JSON is exactly `{ "wrote": { ... } }`; read JSON carries `repo`,
  `entries`, and `matched`. Text writes print only the notched line.
- Exit codes follow house rules: 0 fine, 2 usage, 3 when the ledger directory
  exists but cannot be read.

### 1.5 Status breadcrumb

`knives status` includes the newest ledger entry per branch:

- JSON: `last_notch: {ts, kind, text}` (absent when the subject has none).
- Text: one truncated token at the end of the branch line, e.g.
  `"superseded by #1157…" (3d)`.

Repo-level entries are separate: JSON carries
`repo_notches: {count, last: {ts, kind, text}}` when any exist, and text puts
`notches  <N> repo-level, newest: "<truncated>" (<age>)` above the branch table.
This is a local ledger read of a file the tool already has beside its state; it
adds no observable runtime.

### 1.6 Skills and docs

- `skills/fork-work/SKILL.md`: after "read the claims", add reading the
  notches — `knives notch <branch>` before touching a branch you do not
  understand — and writing one when you make a call worth remembering
  (superseded, parked, promised, re-homed a PR).
- `skills/using-knives/SKILL.md`: full command reference section for `notch`,
  including the two-kinds semantics and the push for `--evidence` on notes.
- `skills/pr-preflight/SKILL.md`: promises made to reviewers belong in
  notches.
- `README.md` and `docs/design.md`: the ledger is a fifth thing state
  answers; document the past-tense-only doctrine (events and judgments with
  anchors; never derived dispositions).

### 1.7 Tests

- Ledger unit tests: append/read round trip, subject and `--pr` filtering,
  lock contention (two writers, no lost or interleaved lines), unknown-field
  tolerance, missing-anchor entries.
- Integration tests in the existing `tests/jj_integration.rs` harness: each
  auto-event fires with the correct subject, owner, and anchor —
  claim/finish (with and without `--superseded-by`), track, depends, a
  release cut recording its parent set, sync transitions via `FakeForge`.
- CLI tests: both output modes; `--repo` from outside the repo; exit codes.
- Status tests: `last_notch` present in JSON, one-token text rendering,
  absent cleanly when no entries exist.

## PR 2: status speed

Files: `src/forge.rs` (batch call), `src/commands/status.rs` (parallel
probes, batched forge use), `src/main.rs` or wherever `--all` iterates repos.
No caching anywhere: the doctrine holds; the wins are round-trip elimination
and concurrency.

### 2.1 Baseline first

Instrument the phases (release scan / landed probes / forge calls) and record
a baseline on a real repo (fork A: 36 branches, ~9 open PRs) and on `--all`.
Every subsequent change is judged against these numbers. The instrumentation
can be a dev-only measurement; it does not need to ship, but if it is cheap
to keep behind `--verbose` or an env var, keep it.

### 2.2 Batch the forge

Replace the per-PR `review_predates_head` + `checks` subprocess pairs with
one `gh api graphql` call that fetches, for all our PR numbers at once, the
review timeline and `statusCheckRollup`. The `Forge` trait gains a batch
method (`pull_details(numbers) -> map`); `CliForge` implements it with the
GraphQL query; `FakeForge` implements it from its existing maps; the
per-number trait methods fold into it. Roughly 2×N+1 subprocesses become 2.
Expected biggest win.

### 2.3 Parallelize the landed probes

Each probe is an independent, read-only jj transaction that is dropped,
keyed by path + branch + upstream trunk. Run them on bounded scoped threads
(`std::thread::scope`; no new dependency unless the codebase already carries
one). Each thread opens its own repo handle — verify jj-lib's concurrent
read-only open behavior in a test before relying on it; jj's own model is
concurrent-safe by design, but the loaded-repo handle is not assumed `Sync`.

### 2.4 Parallelize `--all`

Repos are independent by construction; gather them concurrently and render
in registry order. Store reads are already snapshot-consistent (one locked
read at start).

### 2.5 Verification

Measured wall time before and after, on this machine, against the real fork A
repo and `--all`, recorded in the PR body. Not vibes. Correctness: the
existing integration suite passes unchanged — batching and parallelism must
not alter a single reported fact, token, or exit code.

## Out of scope, recorded

- **Unowned-release-content detection at cut time** — the strongest companion
  candidate (loss #9 class: release-homed content owned by no parent, lost
  silently across four cuts). A cut-time report "this release contains
  content owned by no member branch" kills the class. Separate line of work.
- **Pin-vs-tip equality per fork** and **release ref integrity** (old release
  refs are immutable; verify none moved) — cheap decisive detectors, same
  companion family.
- **Status text legibility** — separate complaint, separate work.
- **Ledger backup/sync** — the file-per-entry shape makes it trivial later (a git repo over the ledger directory sees only added files).
- **Hook injection of ledger content** — rejected, reading stays intentional.
- **Per-PR promise-thread tracking against the forge** — the promise itself
  is a note; "which reviewer threads are unanswered" is derived forge state
  and belongs to sync/status if it ever belongs anywhere.
