# Task 9 report

## Status

Implemented the status report data model and its text renderer as one compilable cut.

## Changed files

- `src/commands/status.rs` — replaced the report and row JSON model; folded raw findings into ordered groups; attached claims, observations, and workspace facts to rows; threaded the newest release and final forge timing into the report.
- `src/commands/status/rows.rs` — assembled the new row shape and implemented the first-match branch-state taxonomy.
- `src/commands/status/render.rs` — rendered the new report shape with problems before branches, grouped findings, claim/seen cells, typed push relations, and workspace leftovers.
- `src/commands/status/phases.rs` — kept raw health findings until the final grouping fold and migrated its row-assembly test.

`src/carriage.rs` was inspected and intentionally unchanged: it builds its own report and does not consume the removed status fields.

## Verification

- RED pin: `cargo test --lib status::tests::the_report_serializes_problems_before_branches_and_skips_absent_values` failed before implementation because the new report types and fields did not yet exist.
- Scoped renderer pin: `cargo test --lib status::render::tests` — 9 passed.
- Scoped taxonomy pin: `cargo test --lib status::rows::tests` — 3 passed.
- Gate: `cargo test --lib` — 450 passed.
- Smoke: `cargo run -- --text status oh-my-pi --no-github --no-landed` rendered the new text surface: headline metadata, `UNANSWERED` before the 11-column branch table, grouped findings, shortened claim owners, age cells, capped notch summaries, and workspace leftovers. It exited 3 because the live repository had one stale-working-copy problem and findings, which is the status command's expected exit contract.

The full integration suite is intentionally not the Task 9 gate; Task 10 migrates its old report-shape assertions.

## Hardening ledger

No shortcuts taken. No failing tests were weakened, skipped, or deleted.

## Review round 1

- RED pins: the new terminal-review, closed-draft, tracked-unavailable-pull, and activity-error regressions did not compile against the pre-fix interfaces: `state_for` had no final-pull-association argument and `claim_last_seen` did not exist.
- Green pins: the three targeted regressions each passed after the fix:
  - `status::rows::tests::terminal_pull_states_outrank_review_decisions_and_draft`
  - `status::rows::tests::a_tracked_unavailable_pull_is_not_no_pr`
  - `status::tests::activity_errors_keep_sidecar_observations_and_mark_unsighted_claims_within_window`
- Status suites: `cargo test --lib commands::status` — 27 passed.
- Gate: `cargo test --lib` — 453 passed.

The review changes gate review, conflict, draft, and check state on an open pull; derive `no-pr` from the final `PullCell`; and retain sidecar observations while reporting an activity-walk failure as `none-within-window`.

## Hardening ledger — review round 1

No shortcuts taken. No failing tests were weakened, skipped, or deleted.
