# The reviewer packet

Read this before you write `<per-PR-scratch>/reviewer-packet.md` (step 6). An orchestrator reads it before it relays a packet.

The packet contains:

- Repo, PR, branch, original PR head, expected branch tip, candidate head and claimed workspace.
- The `placement:` notch. The reviewer challenges its evidence, not only whether the patch passes.
- The full fork-point-to-candidate diff and commit list. The recon notch, inherited notch context and working list.
- Draft body and replies, configured scan terms, exact gate commands and full log paths.
- Reviewer rules: examine the **whole diff** yourself for correctness, security and regressions, also when the owner reports no repairs. Confirm every recon and inherited obligation, the scope of each repair, body truth, red/green evidence and remaining forbidden hits. Reproduce applicable claims, not only the owner's logs. Make no commit, notch, bookmark or forge change. Report any gate-generated change, never fold it in.
- Required first-line verdict: `PASS (no repairs)`, `PASS per fix (k/k)`, or `FAIL: fix|body|drift|placement`. Then evidence per question and any new finding. A question with no object says `none`.
