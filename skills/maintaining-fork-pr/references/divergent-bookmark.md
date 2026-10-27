# A divergent bookmark stops `knives start`

Read this when `knives start` refuses because the branch bookmark is divergent (step 1).

Compare the tips with `jj -R <path> --ignore-working-copy diff --from <a> --to <b>` (path from `knives repos`).

- Same content, one tip is the live PR's origin ref: apply only the same-content recovery that the refusal prints, then retry.
- Else: record the tips and ask the orchestrator. Never pick by recency.
