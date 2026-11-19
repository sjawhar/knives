# The reviewer packet

Read this before you write `<scratch>/reviewer-packet.md` (step 6). Each reviewer gets the packet unedited, so it carries the reviewer rules.

The packet holds:

- Repo, PR, branch, original PR head, expected branch tip, candidate head and the claimed workspace.
- The `placement:` notch. The reviewer challenges its evidence, not only whether the patch passes.
- The full diff and commit list from the fork point to the candidate; the recon notch, the inherited notches and the working list.
- The draft body and replies, the configured scan terms, the exact gate commands and the full log paths.
- Reviewer rules: review the whole diff for correctness, security and regressions, also when the owner reports no repairs. Confirm each recon and inherited obligation, the scope of each repair, the truth of the body, the red/green evidence and each remaining forbidden hit. Reproduce claims; do not only read the owner's logs. Make no commit, notch, bookmark or forge change. Report a gate-generated change; do not fold it in.
- The fresh-context reviewer's first line: `PASS (no repairs)`, `PASS per fix (k/k)` or `FAIL: fix|body|drift|placement`. Then evidence per question, `none` where a question has no object.

The thermonuclear reviewers keep their own format. Record each of their verdicts as PASS or FAIL, with their text verbatim beneath it; a blocking finding is a FAIL.
