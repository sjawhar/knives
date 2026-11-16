# Changelog

## [Unreleased]

### Fixed

- Subagents on models that fill in every task-tool field can dispatch ordinary tasks again while `task.perCallModel` is `off`: an empty per-call `model`, `sourceSession` or `outputSchema` now counts as left out, as does an empty `task` or `taskFile` beside a real value of the other; empty bundled seance overrides remain invalid even after a custom agent is removed mid-session ([#18149](https://github.com/can1357/oh-my-pi/pull/18149) by [@sjawhar](https://github.com/sjawhar))
- A subagent started early while a batch `task` call is still streaming is no longer cancelled and run a second time under a `-2` name because a later item in the same call left a field blank ([#18149](https://github.com/can1357/oh-my-pi/pull/18149) by [@sjawhar](https://github.com/sjawhar))
- A batch `task` call that sets `isolated` at the top level is refused and told to set `tasks[].isolated`, instead of silently starting its subagents unisolated and then running them a second time ([#18149](https://github.com/can1357/oh-my-pi/pull/18149) by [@sjawhar](https://github.com/sjawhar))
- A `task` call whose `name` or `agent` is not a string reports the shape instead of failing with a `TypeError` ([#18149](https://github.com/can1357/oh-my-pi/pull/18149) by [@sjawhar](https://github.com/sjawhar))
- Fixed the status line pinning multiple CPU cores in worktrees created by `jj workspace add`. Their git index carries no file stat data, so every status call re-read and re-hashed the whole worktree (measured 2.2s and 47s of CPU per call on a 94k-file checkout) and leaked a `git-lfs filter-process` per call; the refreshed stat data is now written back, so the first call repairs the index and later calls are ~80x cheaper. Status refreshes also back off to five times the last call's duration, and a status call slower than a second is logged with its repository.
- Fixed quitting or restarting a long session stalling while it wrote the whole un-retired transcript; a saved session, a signal exit or a terminal disconnect now writes only the newest 2,000 rows to scrollback, and `omp --resume` restores a saved session's older messages (notices such as errors older than those rows are not kept) ([#13799](https://github.com/can1357/oh-my-pi/pull/13799) by [@sjawhar](https://github.com/sjawhar)).
- Fixed opening the external editor from a fullscreen view, such as `/annotate` or plan review, stalling for many seconds on a long session ([#13799](https://github.com/can1357/oh-my-pi/pull/13799) by [@sjawhar](https://github.com/sjawhar)).
- `/pause` now stops automatic session titles, composer predictions and idle recaps too; a held title is written once you resume ([#17399](https://github.com/can1357/oh-my-pi/pull/17399) by [@sjawhar](https://github.com/sjawhar))
- `/pause` now also holds the main turn when the pause lands while its credential resolves, and ending a session no longer leaves a held side request to be paid for on resume ([#17399](https://github.com/can1357/oh-my-pi/pull/17399) by [@sjawhar](https://github.com/sjawhar))
- `/pause` now also holds every other request a session makes for itself — compaction and handoff summaries (including the provider-native and remote paths), memory extraction and consolidation, judge calls, edit auto-repair, speech rewriting, commit and changelog messages, and the model requests a running tool makes — and a connection prewarm is skipped while paused instead of opening a socket ([#17399](https://github.com/can1357/oh-my-pi/pull/17399) by [@sjawhar](https://github.com/sjawhar))
### Added

- Sessions can now be stored in PostgreSQL, MySQL, or SQLite instead of JSONL files: set `session.storage: sql` and point `session.sql.dsnFile` at a file holding the connection string (or set `OMP_SESSION_STORAGE=sql` and `OMP_SESSION_SQL_DSN_FILE`); `--resume`, `--continue`, the session picker, and listing follow the same store, and joining a collab session needs file storage. ([#13416](https://github.com/can1357/oh-my-pi/pull/13416) by [@sjawhar](https://github.com/sjawhar))
- `omp gc --restore <session id or archive path>` brings an archived session back to its own path with its artifacts, and names what restoring cannot bring back

### Changed

- Merging a task's changes back (three-way patch apply, cherry-picking its branch, and restoring stashed edits) now reports a conflict when both sides edit adjacent lines, as git does, instead of merging both edits ([#16410](https://github.com/can1357/oh-my-pi/pull/16410) by [@sjawhar](https://github.com/sjawhar))
- `async.maxJobs` now sets the cap on background jobs running at once in a session and its subagents without the fixed 100 ceiling, and `0` means no limit; the default stays 100
- A session left working in a detached tmux pane no longer spends a slice of a core animating a screen nobody can see: the status line drops to one frame a second and the terminal-title spinner pauses until a client is watching again ([#17404](https://github.com/can1357/oh-my-pi/pull/17404) by [@sjawhar](https://github.com/sjawhar))
- When saving pruned tool output fails, the pruning now stays applied in the live context, and the session file catches up on the next successful save.

## [18.18.6] - 2026-11-09

### Added

- In Tern, a turn's long-press menu on a phone can edit and resend a prompt, retry the turn, open the rewind page on it, or fork the session from it into a new session
- A rewind from the transcript says how many turns it dropped and offers Undo on its toast in Tern, which puts the turns and your draft back

### Changed

- Browser `role/` selectors in Chrome tabs now match the role and name Chrome's accessibility tree reports, so they reach open shadow roots and skip hidden elements; in Tern every name `observe()` reports selects its element again ([#18632](https://github.com/can1357/oh-my-pi/pull/18632) by [@will-bogusz](https://github.com/will-bogusz))

### Fixed

- A late usage-limit error from a previous provider no longer blocks or cools down the selected provider; when the old provider has spent its retry budget, a replay-safe turn still tries the selected model without the old quota delay ([#17098](https://github.com/can1357/oh-my-pi/issues/17098), [#17571](https://github.com/can1357/oh-my-pi/pull/17571) by [@Bensonluo](https://github.com/Bensonluo))
- Multi-question ask forms in Tern show Submit on the last question tab, not only on the Review tab
- MCP `headers` and stdio `env` values that use `!command` now run again on a 401/403 (the request is retried once), on `/mcp reconnect` and other reconnects, and on `/mcp reload`, so servers whose command mints short-lived tokens keep working past expiry instead of staying 401 until restart ([#18631](https://github.com/can1357/oh-my-pi/issues/18631))
- Pausing an unapproved plan no longer tells the agent that the draft was approved or to continue executing it, and pausing with `/plan` closes an open plan review so it can no longer save and execute the plan ([#18610](https://github.com/can1357/oh-my-pi/issues/18610)).
- Turning off `lsp.enabled` now also stops `write` and `edit` from starting language servers for diagnostics or formatting; previously only the `lsp` tool was disabled ([#18205](https://github.com/can1357/oh-my-pi/issues/18205), [#18490](https://github.com/can1357/oh-my-pi/pull/18490) by [@yuzu-octopus](https://github.com/yuzu-octopus))
- Fixed background tabs omp borrows (relay `app.target`, `app.cdp_url`) ignoring omp's clicks and keys or timing out on hover and drag; the page reads as visible while omp drives it and hidden again once released ([#17831](https://github.com/can1357/oh-my-pi/pull/17831) by [@will-bogusz](https://github.com/will-bogusz))
- Fixed the transcript jumping at the screen's top edge while scrolled back during a streamed reply, and the screen dropping two rows when a reply finished ([#12307](https://github.com/can1357/oh-my-pi/issues/12307), [#16117](https://github.com/can1357/oh-my-pi/pull/16117) by [@galtamam-cmyk](https://github.com/galtamam-cmyk))
- Focusing a subagent or returning to the main session no longer loses or freezes the next assistant reply when a fast tool turn finishes before the previous reply's links finish resolving, and messages and tool runs that arrive during the switch show exactly once, in order ([#18564](https://github.com/can1357/oh-my-pi/pull/18564) by [@eggpeat](https://github.com/eggpeat))
- `omp gc --strip` now keeps every response of the latest replay turn (including earlier tool-call steps joined by their tool results), reference-only images, and the transcript's file permissions ([#18636](https://github.com/can1357/oh-my-pi/pull/18636) by [@eggpeat](https://github.com/eggpeat))
- Session files whose entries were written without ids are re-indexed when loaded, and migrating old sessions no longer gives two entries the same id ([#18636](https://github.com/can1357/oh-my-pi/pull/18636) by [@eggpeat](https://github.com/eggpeat))
- `wait` and `write proc://<id>/kill` now say how a settled job's result reached the caller: delivered by the call itself (it finished while the call ran), handed over by this snapshot (it superseded the job's queued completion push, so no duplicate follows), or genuinely late — the last keeps its "not auto-delivered; recovered by this snapshot" warning and adds how long before the call it had finished ([#16052](https://github.com/can1357/oh-my-pi/issues/16052), [#18635](https://github.com/can1357/oh-my-pi/pull/18635) by [@yuzu-octopus](https://github.com/yuzu-octopus))
- Fixed `ask` notes added with `n` right after picking an answer landing on the previous question instead of the one shown; a Tern click then Note still notes the answer just clicked, and the Note button names it ([#18317](https://github.com/can1357/oh-my-pi/pull/18317) by [@DonMikone](https://github.com/DonMikone))

## [18.18.4] - 2026-11-09

### Added

- SDK: `PromptOptions.followUps` queues prompts to run after the prompt, in order; if the prompt never reaches the agent they come back with it in `DroppedPrompt.followUps`

### Changed

- The `task` tool no longer advertises a per-call `model` field unless `task.perCallModel` is `on` or the bundled seance is spawnable; a `model` sent anyway is still refused before dispatch ([#18554](https://github.com/can1357/oh-my-pi/pull/18554) by [@xiangnan0811](https://github.com/xiangnan0811))

### Fixed

- `/usage`, session statistics, and `omp stats` now include session title spend from both the default fork title and the tiny title model, and tiny-model titles count toward `--max-cost` ([#18177](https://github.com/can1357/oh-my-pi/issues/18177), [#18491](https://github.com/can1357/oh-my-pi/pull/18491) by [@yuzu-octopus](https://github.com/yuzu-octopus))
- Retries after `--model pi/<role>` (or `@<role>`, `<role>`) now walk that role's `retry.fallbackChains` entry instead of the `default` chain, including on `--continue` ([#18389](https://github.com/can1357/oh-my-pi/issues/18389), [#18534](https://github.com/can1357/oh-my-pi/pull/18534) by [@harshaygadekar](https://github.com/harshaygadekar))
- Fixed a `->` / `=>` list sent to an idle session running its second item before its first; if the first item can't start (Esc, a refused or failed setup), the whole list goes back to the editor ([#10802](https://github.com/can1357/oh-my-pi/issues/10802))
