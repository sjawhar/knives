# Questions past your PR

Read this when a question reaches past your PR (placement, membership, a compatible repair, coordination, or something only the human can decide), when you need a new credential, or when another actor reparents your branch.

- Placement, membership, compatible repairs and coordination go to the orchestrator, with evidence and your recommendation. Apply a recorded choice; never make the human choose again.
- Only missing authority, a change of direction, or a conflict with the user's intent goes to the human, as a worked question: the ask, the record, what you would do and why, and what changes if they choose otherwise. A maintainer's design question is never one of these.
- Get a new credential through the existing approval mechanism. Do not route around a denial or a timeout.
- Another actor reparents your branch: update the stale workspace once, and record the old and new tips and your reparented repair commits. Do not push. Give the pending candidate to the orchestrator, which picks the head from content evidence and resumes you. Then run review and the affected tests again: a PASS on the old tree does not survive a rebase, even one with no conflicts.
- Finish independent work while a dependency waits.
