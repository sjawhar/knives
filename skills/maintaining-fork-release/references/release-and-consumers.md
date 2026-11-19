# Finishing release and consumer verification

Read this whenever you do step 5. Do the five stages in order.

1. **Reconcile intended content.** Read the release and member notches again. Run `knives release --repo <repo>`, `knives release --repo <repo> members --census` and `knives consumers <repo>`.
   - For each maintained delta, record whether the release must carry it, inherits it upstream, excludes it deliberately, or needs repair.
   - Prove carriage with `knives release members <target> --carries <revision>`. A merged upstream PR or a source-text match is not proof. Investigate a `conflicted` probe; do not drop it automatically.
2. **Compose the intended release** with `include`, `advance`, `drop --why`, `rebase` and `cut`, behind the barrier in [release-wide-writes.md](release-wide-writes.md).
   - Advance repaired members to verified heads. Include an unreleased PR only when it is intended and ready to ship.
   - Read the real consumer pins before you choose an in-place edit or a new cut. When an edit is refused (frozen pins, a published release), follow the remedy the refusal names. Make no parallel lineage and no forced ref move, and keep required old refs reachable.
   - Classify each member (SKILL.md step 5), and apply the `fork-work` ownership test again. A policy-shaped member is a missing capability (UPSTREAM, with its PR in the same work) or consumer work, which is `neither`.
   - Before any parent leaves the candidate, read [drop-sweep.md](drop-sweep.md) and finish its sweep for every leaving parent, not only those a reviewer named. Without a finished sweep, keep the parent or its rebuild.
   - A heading, `--why`, `--allow-drop`, the drop note or a quiet orphan gate says nothing about the content behind a parent. A member's own content can be superseded while a line that reaches the release only through it is not.
3. **Verify the composed candidate.** Run all applicable gates, exercise the changed public surface, and verify every member and integration resolution.
   - Before you publish, run the resolution-delta guard. Each later write carries a recorded resolution forward, so a whole-file restore from one parent silently reverts the other parents' content in that file.

     ```sh
     t=$(mktemp -d)                                                 # scratch outside the checkout, so jj does not snapshot it
     jj new '<release>-' -m guard                                   # fresh automerge of the release's parents
     guard=$(jj log -r @ --no-graph -T commit_id)                   # jj resolves the abandoned guard by this id
     jj diff --from @ --to <release> --name-only | sort > "$t/delta"      # the recorded resolution delta
     jj resolve -r @ --list | awk '{print $1}' | sort > "$t/conflicted"   # the paths that genuinely conflict
     comm -23 "$t/delta" "$t/conflicted"                            # must print nothing, or only recorded semantic conflicts
     jj abandon @
     ```

   - A release log that knives wrote as a union stays conflicted in the automerge, so the guard does not print it.
   - For each printed path, put the automerge back (`jj restore --from "$guard" --into <release> <path>`), then resolve only the conflicted paths again, hunk by hunk. Never resolve a release conflict with a whole-file `jj restore --from <one parent>`.
   - The one allowed print is a **semantic conflict**: the members merge cleanly, but the composition fails a gate. Repair it on the release commit, never on a member, which must stay valid alone. Record in the release notch the path, the two members, the gate error and the hunk verbatim. `jj diff --from "$guard" --to <release> -- <path>` must be byte-equal to that hunk.
   - Record each hand change with `knives release adopt` ([release-wide-writes.md](release-wide-writes.md)). Then cut, and run `knives audit <repo>`: no `release-drift` remains for the release, except the one line the published remedy can leave on the old name (`using-knives`, reports reference).
   - For a pure restructure, require tree identity with the verified reference. Otherwise, account for the exact intended delta.
   - Run absolute consumer capability contracts: comparisons to the previous cut and structural gates accept a stated drop. Find them by searching the consumer's test trees for the pinned package's name and the symbols it exposes.
   - Run static tests with the candidate in place of the pin (a `file://` pin, relocked). Run live tests on a dev stack deployed from the candidate, or on the pin after publish.
   - A required capability recorded in `knives notch` and lost earlier is a defect to repair, not a baseline.
   - Judge test failures against a control, not against zero: the same command in the same environment on a pristine export of the target trunk (`git archive <target> | tar -x`, dependencies installed), never a fork workspace. Compare failure sets by test name. A candidate-only failure counts when it fails in isolation on the candidate and not on the control. A scrubbed run (`env -i`, no ambient credentials, plain `TERM`) is a cheap first test.
4. **Verify the consumer chain**: trunk pins, resolved lock commits, build inputs, submodule or binary artifacts, and intermediate packages that inject their own dependencies.
   - A direct pin does not control a runner's separately installed version.
   - Use the project's deployment and verification guidance. Local gates, a changed lockfile, a published branch and deployed behavior are separate evidence.
5. **Publish and consume deliberately.** A local cut is not published. A published ref is not consumed until the authoritative consumer resolves it.
   - Publish to the registry's publish remote and check the remote ref with `knives pushed <release>`. A fixed release branch takes the fast-forward rebase, `adopt` and a plain `git push`, in the order of `using-knives`, releases reference, "Publishing".
   - Do the consumer pin, build and deployment the goal requires. Never deploy when excluded, and never go around credentials or approvals.
   - Verify the final resolved revision, and the running revision where required, and exercise the behavior there. Verify again after a relevant change.
   - Record `record:` and `verify:` on the release with the candidate, published and consumed commits, and gate and runtime evidence.
   - The consumer's pin PR body discloses the delta under `## Production behavior delta`, as disclosure, not a gate. For each member the release adds or moves, state what production does differently, in plain numbers, and which deployments carry it. Mark upstream-pending members; write `none` when nothing changes. Write it from the member classifications and notches, not from the release title.
