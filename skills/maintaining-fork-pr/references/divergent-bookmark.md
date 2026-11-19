# A divergent bookmark stops `knives start`

Read this when `knives start` refuses because the branch bookmark is divergent (step 1).

1. Compare the tips with `jj -R <path> --ignore-working-copy diff --from <a> --to <b>` (path from `knives repos`).
2. Same content, and one tip is the live PR head: set the bookmark to that tip with the command the refusal prints, then run `start` again.
3. Otherwise record the tips and ask the orchestrator. Never pick by recency: that can lose the live PR's commits.
