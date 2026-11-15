# Changelog

## [Unreleased]

### Added

- Added the default-off `providers.openaiResponsesWebsockets` setting for catalog-supported standard Responses models, independently of Codex; model/request `false` always disables it ([#17354](https://github.com/can1357/oh-my-pi/pull/17354) by [@TatsukiMeng](https://github.com/TatsukiMeng))
- Pinned sessions show a pin beside their title in the status line and the band composer's title, and `/unpin [session id]` unpins a session ([#17848](https://github.com/can1357/oh-my-pi/pull/17848) by [@SuperUserA](https://github.com/SuperUserA))
- Added a fullscreen extension link browser for picking links already in terminal scrollback: press Alt+O again (or Page Up) while link tags are shown, or Alt+O with no extension link on screen ([#16476](https://github.com/can1357/oh-my-pi/pull/16476) by [@materemias](https://github.com/materemias))
- Added a `cache` status-line segment (in the `full` and `nerd` presets) showing before you send whether the next message reads the prompt cache; hover it in Tern or open `/session` for why not and how much it re-sends ([#12677](https://github.com/can1357/oh-my-pi/issues/12677)) ([#17496](https://github.com/can1357/oh-my-pi/pull/17496) by [@will-bogusz](https://github.com/will-bogusz))
- Added a `:level` suffix to `^` model tags, such as `^openai-codex/gpt-6-astra:high`, that sets the tagged subagent's initial effort. Typing `:` after a mention lists the model's supported levels. The task tool's `effort` hint and `task.maxEffort` do not override the tagged level, and a per-call `model` for that agent fails as a conflict ([#17574](https://github.com/can1357/oh-my-pi/pull/17574) by [@a-glapinski](https://github.com/a-glapinski))
- RPC `bash` results now say when the output was cut: `elidedBytes`/`elidedLines` for a dropped middle, and `columnTruncatedLines`, `columnTruncatedRange`, `columnDroppedBytes` and `columnMax` for lines cut to the column cap. The Go, Python and Rust SDKs decode them ([#17957](https://github.com/can1357/oh-my-pi/pull/17957) by [@H4vC](https://github.com/H4vC))

### Fixed

- Long bash output cut in the middle no longer splits a line at the `[…N ln elided…]` marker, and its "Showing lines …" notice now names the lines actually shown, ending at the output's real last line ([#17957](https://github.com/can1357/oh-my-pi/pull/17957) by [@H4vC](https://github.com/H4vC))
- A bash command that prints nothing and succeeds now returns `(no output, exit code 0)` instead of a bare `(no output)`, so an empty diff or search no longer reads like a failed command ([#17957](https://github.com/can1357/oh-my-pi/pull/17957) by [@H4vC](https://github.com/H4vC))
- Added `tui.motion` (`full` / `reduced` / `none`, also `--motion` and `PI_MOTION`), one switch over spinners, the thinking pulse, shimmer and the status-line blink/fade for many-pane or reduced-motion use; a `tui.spinnerInterval` you set still wins over the preset ([#14240](https://github.com/can1357/oh-my-pi/pull/14240) by [@mccraigmccraig](https://github.com/mccraigmccraig))
- Added `tui.spinnerInterval` (also `--spinner-interval` and `PI_SPINNER_INTERVAL`) to slow every spinner, the terminal title included, or pin them static with `0` ([#14240](https://github.com/can1357/oh-my-pi/pull/14240) by [@mccraigmccraig](https://github.com/mccraigmccraig))
- Added `tui.maxFps` (also `--max-fps` and `PI_MAX_FPS`) to cap how often the TUI repaints; keystroke echo keeps 30 fps ([#14240](https://github.com/can1357/oh-my-pi/pull/14240) by [@mccraigmccraig](https://github.com/mccraigmccraig))

### Changed

- A CLI flag, session-only settings write or SDK override now outranks the setting's environment variable, so `PI_MOTION=none omp --motion full` runs with full motion; env vars still outrank every settings file
- Updated abort error handling for JS and Python tools to propagate original abort reasons
- Subagents now move into worker threads when many run at once, so their model streaming and tool work no longer competes with the TUI: up to 3 concurrent subagents run on the main thread, 4 add a worker, and each doubling (8, 16, 32) adds one more, up to the new `task.workerThreads` (default 4; `0` keeps every subagent on the main thread). Each subagent goes where the fewest are running; a needed worker starts while the model is still streaming the spawning call and exits a minute after its last subagent finishes or parks. IRC, the Agent Hub and focus view, `agent://`/`history://`/`proc://`, parking and revival, async jobs, cost caps, account pools and OAuth refresh, runtime model providers, MCP, memory, telemetry, `PI_PROXY` and logs keep working across the thread boundary. If omp's own files change while it runs (a source checkout or an in-place update), new subagents run on the main thread until omp restarts

### Fixed
- Fixed a second `ask` note on a later single-select question reopening and replacing the previous question's note ([#17979](https://github.com/can1357/oh-my-pi/issues/17979))

