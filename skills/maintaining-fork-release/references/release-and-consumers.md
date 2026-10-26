# Finishing release and consumer verification

Read this before you change, cut or publish a release or a consumer pin (step 5). Do the five steps in order.

1. **Reconcile intended content.** Read the release and member notches again. Run `knives release --repo <repo>`, `knives release --repo <repo> members --census` and `knives consumers <repo>`.
   - For each maintained delta, record if the release must carry it, inherits it upstream, excludes it deliberately, or needs repair.
   - Prove carriage with `members <target> --carries <revision>`. A merged upstream PR or a source-text match is not proof. Investigate a conflicted probe. Do not drop it automatically.
2. **Compose the intended release** with `release advance`, `include`, `drop --why`, `rebase` and `cut`, under the integration barrier.
   - Advance repaired members to verified heads. Include an unreleased PR only when it is intended and ready to ship.
   - Read the real consumer pins before you select an in-place edit or a new cut. A consumer frozen on an older release does not block an edit of the current one.
   - When the tool refuses because every pin of the edited release is frozen, cut a new release. Make no parallel lineage and no forced ref move. Keep required old refs reachable. Never cut an identical release only for a new name.
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
   - Before you publish, run the **resolution-delta guard**. Each later `cut`, `advance` and `rebase` carries a recorded resolution forward. Thus a whole-file restore from one parent silently reverts the content of the other parents in that file.

     ```sh
     t=$(mktemp -d)                                                 # scratch outside the checkout, so jj does not snapshot it into the guard
     jj new '<release>-' -m guard                                   # fresh automerge of the release's parents
     guard=$(jj log -r @ --no-graph -T commit_id)                   # save its id: the restores below need it, and jj resolves an abandoned commit by id
     jj diff --from @ --to <release> --name-only | sort > "$t/delta"      # the recorded resolution delta
     jj resolve -r @ --list | awk '{print $1}' | sort > "$t/conflicted"   # the paths that genuinely conflict
     comm -23 "$t/delta" "$t/conflicted"                            # must print nothing, or only recorded semantic conflicts
     jj abandon @                                                    # drop the guard commit
     ```

   - For each printed path, put the automerge content back (`jj restore --from "$guard" --into <release> <path>`). Then resolve only the conflicted paths again, hunk by hunk.
   - Never resolve a release conflict with `jj restore --from <one parent>` of a whole file.
   - The exception is a **semantic conflict**: the members merge cleanly, but the composition fails a gate. Repair it on the release commit, never on a member, because a member must stay valid alone.
   - Record a semantic conflict in the release notch: path, the two members, the gate error and the hunk verbatim. `jj diff --from "$guard" --to <release> -- <path>` must be byte-equal to that hunk. Any other printed path is a defect.
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
   - Do the consumer pin, build and deployment that the goal requires. Never deploy when excluded, and never bypass credentials or approvals.
   - Verify the final resolved revision, and the running revision where required, and exercise the behavior there. Verify again after a relevant change.
   - Record `record:` and `verify:` on the release with candidate, published and consumed commits, and gate and runtime evidence.
   - **The pin PR body discloses the delta** under `## Production behavior delta`. For each member that the release adds or moves, state what production does differently, in plain numbers, and which deployments carry it. Mark upstream-pending members. Write `none` when nothing changes.
   - Write the delta from the member classification and notches, not from the release title. It is disclosure, not a gate.
