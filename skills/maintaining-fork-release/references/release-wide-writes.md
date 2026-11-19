# Release-wide writes

Read this before a release-wide write: a release rebase, a member rehome an owner handed back as `rehome needed`, or a hand resolution on the release commit (step 4).

## The barrier

1. Stop admitting owners for that repository.
2. Wait until every affected member claim is released. Hold the release claim while you integrate: it is the write lease for release refs, not leave to push an owner's PR head.
3. Check again just before the write that no member claim exists. A new external claim means back off. Other repositories continue.

## Rebase and check each member

- Pick the target from the need: `knives release --repo <repo> rebase [<target>]`. Record `rehome:` with the target and the resulting release commit.
- Exit `0` is not enough. Each affected member must be on the target, keep its patch and have no conflicts:

  ```sh
  knives release --repo <repo> members <release> --verify
  jj -R <checkout> --ignore-working-copy log -r '<target> ~ ::<member-commit>' --no-graph   # per member; empty: the target is an ancestor
  ```

  Content carriage does not prove that a member moved. A member still on the old base is unresolved work.
- A member with no conflict left can still be broken. At each rebased tip, conflicted or clean, run the type checker and its own tests, and audit the new trunk call sites against what the member changed. Repair a break on the member, not on the release commit.

## Hand resolutions on the release commit

- knives resolves only the release-log union (`CHANGELOG.md`, `RELEASE_NOTES.md`). Every other release conflict, a renumber of two blocks that claim one version, and a repair of the composition are yours, on the release commit.
- Record each with `knives release adopt <release>` before the next write, with `--why` for a change outside the conflicted paths. Read its counts: a renumbered heading line is expected; any other missing line is a lost change to repair.
- Follow any remedy `adopt` or a refusal prints, step by step, in its order. The `using-knives` releases reference explains each: "knives release adopt", "When knives cannot check the release in hand", "The published remedy".

## After the write

- Resume verification for all affected PR heads. Never push a rewritten head without its claim and fresh verification. Keep its original, expected and candidate heads separate in the re-dispatch.
- If an external actor moves a branch under an owner, inspect the content from old to new. For trunk-only movement, record the original head, the new head and the reparented candidate commits. The owner then reviews and tests again before it pushes.
- A verdict covers one tree. A conflict with the user's intent needs a real decision.
