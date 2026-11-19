# Owners: dispatch, review relay, handback and decisions

Read this before you dispatch a `maintaining-fork-pr` owner, and when an owner sends a packet, a handback or a `decision:` (step 3, step 4).

## The dispatch

Each dispatch holds, in order:

1. The whole `maintaining-fork-pr` skill, with its `references/`.
2. Registry name, upstream slug, PR number and URL, branch and its captured status row. For a release member, also the member row and parent commit.
3. The relevant notch entries in full, or a readable saved path, never your summary.
4. Upstream and project guidance with source paths, recorded publication constraints, and the registry's `forbidden` terms and outbound-prose rules, or "none configured" (an empty list is not a privacy waiver).
5. The whole `pr-preflight` skill, your reviewer-relay address and the per-PR scratch path.
6. The branch's `placement:` notch, or "none". For upstream-bound work you propose, complete the `fork-work` challenge first.

Name the owner's model in the dispatch record.

## Reviewer relay

1. Check the packet's repo/PR identity and its original, expected and candidate heads against the dispatch.
2. Send it back when it has no `placement:` notch, offers approval or tests in place of evidence, or its PR evidence is incomplete for the judged head: a first page, elided output, a `null` thread count, `first:100` with more pages.
3. Dispatch the reviewers that `maintaining-fork-pr` step 6 names, each fresh, each with the unedited packet, each on the whole PR diff, also when recon found nothing.
4. Relay each verdict verbatim within 20 minutes. Do not soften a FAIL. Send a malformed verdict back to its reviewer.
5. After an owner's timeout handback, the verdict is yours to get. Then resume the owner at verification, with its original, expected and candidate revisions.

## Handback

- Read each owner's notches (`knives notch <branch> --repo <repo>`) and `knives status <repo>` yourself. `handback:` and a released claim end the worker's ownership, not the task.
- Accept a thread disposition only as `-> <commit>`, `declined: <reason>` or `already answered <reply-url>`.
- Record open promises with the reply URL; they are not done. Check that a new reply is published before you report it addressed.

## Decisions that reach the human

You are the last filter before a question reaches the maintainer of record. Before you forward an owner's `decision:`:

1. Read the branch's whole chain. If an earlier note answers it, write a `decision:` notch that quotes that note and re-dispatch the owner with the answer. The human never sees it.
2. Read and quote the maintainer's words, not the owner's summary.
3. Forward only what needs authority you lack: money, an upstream commitment, removal of what the human asked for. Missing confidence is your work. Bring the quoted ask, the record, what you would do and why, and what changes under the alternative, in the repository's terms.
4. Record the ruling as a `decision:` notch on the branch, with its reasoning. Record a standing rule also on the repository (`knives notch --repo <repo> -m 'decision: ...' --evidence ...`), which every dispatch reads.
