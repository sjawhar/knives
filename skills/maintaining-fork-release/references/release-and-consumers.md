# Finishing release and consumer verification

Read this whenever you do step 5. Do the five steps in order.

1. **Reconcile intended content.** Read the release and member notches again. Run `knives release --repo <repo>`, `knives release --repo <repo> members --census` and `knives consumers <repo>`.
   - For each maintained delta, record if the release must carry it, inherits it upstream, excludes it deliberately, or needs repair.
   - Prove carriage with `members <target> --carries <revision>`. A merged upstream PR or a source-text match is not proof. Investigate a conflicted probe. Do not drop it automatically.
2. **Compose the intended release** with `release advance`, `include`, `drop --why`, `rebase` and `cut`, under the integration barrier.
   - Advance repaired members to verified heads. Include an unreleased PR only when it is intended and ready to ship.
   - Read the real consumer pins before you select an in-place edit or a new cut. A consumer frozen on an older release does not block an edit of the current one.
   - When the tool refuses because every pin of the edited release is frozen, or because the release is published (a tag or a `record: published` note names its commit), cut a new release. Make no parallel lineage and no forced ref move. Keep required old refs reachable. Never cut an identical release only for a new name, except to edit a frozen or published release: that identical cut is the composition you then edit.
   - **Classify each member in the release notch before you compose it**, as one of:
     - `upstream-PR <url>`: an open or merged upstream PR carries it.
     - `upstream-pending, placement <notch anchor or commit>`: its `placement:` notch and commit body record the UPSTREAM verdict and the defect or capability gap. They also record the reproduction or red→green test that its upstream PR opens from.
     - `neither`.
   - Drop a `neither` member (`drop --why`) or return it to its author. Never compose it.
   - A valid classification (`upstream-PR`, `upstream-pending`) is disclosure, not a gate. An `upstream-pending` member ships when the release is gated and published. The consumer pins it without a wait for the upstream PR.
   - Apply the ownership test of `fork-work` again: *if upstream main replaced the fork tomorrow, would anyone but us miss this?*
   - Operating policy (job lifetimes and reaping, caps, quotas, schedules, alerting) lives in the consumer. A fork member repairs the library's own machinery or adds a capability that nothing outside the library can give.
   - A policy-shaped member is a missing capability (UPSTREAM, with its PR in the same work) or CONSUMER work, which is `neither`. There is no third class.
   - **Sweep each parent that leaves the candidate, line by line, before you cut or publish.** Read and follow [drop sweep](drop-sweep.md) before any parent leaves. Sweep all of them, not only those a reviewer named.
   - Do not make a drop whose sweep is not finished. Keep the parent or its rebuild.
   - A heading, `--why`, `--allow-drop`, the drop note or a quiet orphan gate says nothing about the content behind a parent. A member's own content can be superseded while a line that reaches the release only through it is not.
3. **Verify the composed candidate.** Run all applicable gates and exercise the changed public surface on the candidate. Verify every member and integration resolution.
   - Before you publish, run the **resolution-delta guard**. Each later `cut`, `include`, `drop`, `advance` and `rebase` carries a recorded resolution forward. Thus a whole-file restore from one parent silently reverts the content of the other parents in that file.

     ```sh
     t=$(mktemp -d)                                                 # scratch outside the checkout, so jj does not snapshot it into the guard
     jj new '<release>-' -m guard                                   # fresh automerge of the release's parents
     guard=$(jj log -r @ --no-graph -T commit_id)                   # save its id: the restores below need it, and jj resolves an abandoned commit by id
     jj diff --from @ --to <release> --name-only | sort > "$t/delta"      # the recorded resolution delta
     jj resolve -r @ --list | awk '{print $1}' | sort > "$t/conflicted"   # the paths that genuinely conflict
     comm -23 "$t/delta" "$t/conflicted"                            # must print nothing, or only recorded semantic conflicts
     jj abandon @                                                    # drop the guard commit
     ```

   - A `CHANGELOG.md` or `RELEASE_NOTES.md` that knives wrote as a union is expected. The guard's automerge leaves that file conflicted, so it is in both lists, and the guard does not print it. Two blocks that claim one version stay as their members wrote them: renumbering them is yours.
   - For each printed path, put the automerge content back (`jj restore --from "$guard" --into <release> <path>`). Then resolve only the conflicted paths again, hunk by hunk.
   - Never resolve a release conflict with `jj restore --from <one parent>` of a whole file.
   - The exception is a **semantic conflict**: the members merge cleanly, but the composition fails a gate. Repair it on the release commit, never on a member, because a member must stay valid alone.
   - Record a semantic conflict in the release notch: path, the two members, the gate error and the hunk verbatim. `jj diff --from "$guard" --to <release> -- <path>` must be byte-equal to that hunk. Any other printed path is a defect.
   - After any hand resolution, restore, renumber or repair on the release commit, record it before the next `cut`, `include`, `drop`, `advance` or `rebase`, which refuse until then: `knives release adopt <release>`. A change outside the conflicted paths, such as a restore above or a semantic repair, needs `--why '<what was restored and why>'`. Read the output and the counts: `adopt` exits `1` only when it measured against a recorded write, never against a fresh merge, and found a member's added line lost from a file that the baseline left without a conflict and that is not a release log, or none of a member's blocks left in a release log or a conflicted file. A block cut down there is reported and exits `0`. Then cut, and run `knives audit <repo>`: no `release-drift` finding remains for the release. One exception: after the published remedy, when the published commit was itself the all-carrier hand edit (a resolution made on top of the release and pushed under its name), one true `release-drift` line stays on the old published name, and it needs no action (`using-knives`, reports reference).
   - A release composed outside knives, by hand or by a script, has no recorded write for `adopt` to replace. Record it first with `knives release cut <release> --at <commit>`: the commit becomes the name's recorded cut, and no commit is made. Record the older name first. Then `knives audit <repo>` on every machine, after its next ledger pull, shows no `has no recorded cut or edit event` note for the name.
   - For a pure restructure, require tree identity with the verified reference. Otherwise, account for the exact intended delta.
   - Run **absolute consumer capability contracts**. Comparisons to the previous cut and all structural gates accept a stated drop as expected.
   - Find the contract tests: search the consumer's test trees for the pinned package's name and for the symbols the pin exposes.
   - Run the static tests with the candidate in place of the pin (a `file://` pin, relocked). Live tests need a deployment. Before publish, run them only on a dev stack deployed from the candidate. Otherwise, run them on the pin after publish.
   - A required capability recorded in `knives notch` and lost earlier is a defect to repair, not a baseline.
   - **Judge test failures against a control, not against zero**. Run the same command in the same environment on a pristine export of the target trunk that the suite can run in. Never use a fork workspace. For example, use `git archive <target> | tar -x` and install dependencies.
   - Compare failure sets by test name, not chunk label. A candidate-only failure counts when it occurs in isolation on the candidate and not on the control. A test file only the candidate has counts when it fails deterministically in isolation.
   - A scrubbed run (`env -i`, no ambient credentials, plain `TERM`) is a cheap first test.
4. **Verify the consumer chain.** Identify consumer trunk pins, resolved lock commits, build inputs, submodule or binary artifacts, and intermediate packages that inject their own dependencies.
   - A direct pin does not control a runner's separately installed version. Verify the whole chain against the candidate.
   - Use the project's deployment and verification guidance, not an unrelated upstream dev environment. Local gates, a changed lockfile, a published branch and deployed behavior are separate evidence.
5. **Publish and consume deliberately.** A local cut or edit is not published. A published ref is not consumed until the authoritative consumer resolves it.
   - Publish to the registry's publish remote (`release`, otherwise `origin`). Verify the remote ref with `knives pushed`.
   - A release branch that the publish remote moves only forward (a fixed release branch) needs the fast-forward rebase, `adopt`, then a plain `git push` (`using-knives`, releases reference, "Publishing"):

     ```sh
     jj git fetch --remote <publish-remote>
     old=$(jj log -r '<release>@<publish-remote>' --no-graph -T commit_id)   # the published head
     pre=$(jj log -r <release> --no-graph -T commit_id)                       # the write knives made
     jj rebase -r <release> -d <parent-1> -d <parent-2> -d "$old"             # each parent as its own -d, in order, then the published head
     jj restore --from "$pre" --into <release>                                # put the write's tree back
     JJ_USER='<author>' JJ_EMAIL='<email>' jj metaedit --update-author <release>   # only when the author must change
     knives release adopt <release> --why "fast-forward of $pre onto the published head $old"
     git push <publish-remote> "$(jj log -r <release> --no-graph -T commit_id):refs/heads/<release>"
     ```

   - Run `adopt` after the last rewrite, the author change included, and before the push, while the publish ref still names the old head. Then the old head reads as a carrier, and the ledger's newest write is the commit the push publishes.
   - Change the author only with `jj metaedit`, which keeps jj's `change-id` header. Git plumbing drops it.
   - Push with no `+` and no `--force`, and stop when git refuses. Never use `jj git push` for this branch: it moves the remote like `git push --force-with-lease`, so it hides the non-fast-forward refusal that is the branch's only backstop.
   - `adopt` starts the ledger sweep, which pushes a shared ledger by itself. A refused push therefore leaves a recorded write that the fork does not hold, and nothing in the ledger says that the fork published it. The next pod reads the published head, which an earlier `adopt` recorded, without a refusal; its audit reports `release-drift` against the newest recorded write, which it lacks, until its first write or an `adopt`.
   - Without the `adopt` step, nothing records the pushed head. When the head's parents match a recorded write that the next pod does not hold, that pod cannot check the head: its first write goes ahead, prints that knives could not check the release and marks its ledger event `; unchecked: W absent`. One `adopt` records the head and ends the mark. After two pushes in a row without `adopt`, a fresh pod that holds neither recorded write still reads the older pushed copy as a carrier by its change id, so the head matches the newer write, which that pod does not hold, and the write goes ahead unchecked in the same way. `adopt` records it.
   - Do the consumer pin, build and deployment that the goal requires. Never deploy when excluded, and never bypass credentials or approvals.
   - Verify the final resolved revision, and the running revision where required, and exercise the behavior there. Verify again after a relevant change.
   - Record `record:` and `verify:` on the release with candidate, published and consumed commits, and gate and runtime evidence.
   - **The pin PR body discloses the delta** under `## Production behavior delta`. For each member that the release adds or moves, state what production does differently, in plain numbers, and which deployments carry it. Mark upstream-pending members. Write `none` when nothing changes.
   - Write the delta from the member classification and notches, not from the release title. It is disclosure, not a gate.
