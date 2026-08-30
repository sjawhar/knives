# `fork` Implementation Plan

**Goal:** Implement everything in [design.md](design.md): nine commands, the OpenCode plugin, and the skill. Every command is wired end to end and run from a shell before it is done.

**Tech stack:** Rust 2024, `jj-lib` for reading repositories, `clap` for the command surface, `thiserror` for typed errors, `serde` + `toml` + `serde_json` for the registry and state. The plugin is TypeScript against the OpenCode plugin API. The skill is markdown.

## Why jj-lib rather than parsing the CLI

An earlier draft shelled out to `jj` and parsed its output. Four separate bugs came from that layer alone, all found by testing against real repositories:

- `jj` decorates the `bookmarks` template with a trailing `*` for unpushed local changes and `??` for a conflicted bookmark, so a naive split produced names matching nothing.
- Without `--no-graph`, rows arrive prefixed with node glyphs and interleaved with bare `|` and `~` lines, which parse as parents that do not exist.
- `jj bookmark list -T` emits one row per target, so a local bookmark and its remote-tracking counterpart arrive under the same name. On a real repository their tips genuinely differed, so release parents were compared against the wrong commit.
- Template field validity is discovered at runtime, not compile time.

Typed library access deletes that entire class. What remains typed at the boundary is enforced by [`ids.rs`](../src/ids.rs): a `ChangeId` is not a `CommitId`, and `BookmarkRef::Local` is not `BookmarkRef::Remote`.

## The jj-lib pin

```toml
jj-lib = { git = "https://forge.invalid/sjawhar/jj", rev = "66c5253e...", features = ["watchman"] }
```

Pinned to the exact revision the installed `jj` binary was built from, so the library reads repositories written by that binary and no other. jj-lib's API is explicitly unstable, so a floating version would break on any upgrade.

`watchman` is enabled to work around a latent bug at that revision, not because this tool watches files: `lib/src/gitattributes.rs` carries a bare `use tokio::sync::OnceCell` while `tokio` is optional and reachable only through that feature, so **jj-lib does not build with default features**. `jj-cli` hides it by defaulting `watchman` on.

## Global constraints

- No user name, organisation name, or upstream repository name appears in release surfaces. A guard test enforces it, keyed on `concat!("github", ".com")` because that catches any hard-coded remote URL without the test itself naming anyone.
- Remotes are addressed by **role**. `Role` is an enum, so an unknown role cannot be requested and no runtime lookup error is needed.
- `detect/` is pure: functions from parsed values to findings, no I/O. `jj.rs` is the only module that opens a repository; `forge/github.rs` is the only module that talks to a hosting service.
- **Never mutate a repo we do not own.** Read-only jj access passes `--ignore-working-copy`, so a busy workspace is not snapshotted.
- **Never `jj op restore` in a shared repo.** Restoring to a recorded operation discards any operation another agent performed since. The landed probe cleans up by abandoning exactly the commits it created, inside a guard that runs on every exit path.
- **One commit** for the whole plan.

## Verified facts this plan is built on

Confirmed against the live machine and real repositories, not recalled.

| Fact | Value |
|---|---|
| `jj` | 0.43.0, built from `sjawhar/jj` at `66c5253e` |
| `jj-lib` on crates.io | 0.43.0, matching, but the fork is the pin |
| `gh` / `cargo` / `rustc` | 2.96.0 / 1.97.1 / 1.97.1 |
| `jj duplicate` | accepts repeated `-d` destinations |
| `empty` and `conflict` | valid commit template keywords |
| `divergent()` | a real revset, and the managed repos have divergence today |
| `jj git push` | `-b` tracks a new bookmark; `--allow-new` does not exist at 0.43 |
| `refs/pull/N/head` on a plain bare repo | fetchable by a clone that never had the branch, so the lab needs no forge server |
| Publishing a pull ref | requires a push, not `update-ref`: upstream has never seen the objects |
| Landed | measured against the **upstream** trunk; our fork's trunk answers about the wrong repository |

## Structure

| Module | Owns |
|---|---|
| `ids.rs` | Semantic identifiers. Mixing a change id with a commit id, or a local bookmark with its remote counterpart, are the two mistakes this domain invites. |
| `detect/` | Four pure detectors: two workspaces on one change, stale release parent, landed, divergence. |
| `jj.rs` | The only module that opens a repository. Reads via jj-lib; the landed probe and workspace creation shell out, and say why. |
| `pins.rs` | Where a consumer pins a dated release. Five sites, four syntaxes, and a lockfile that percent-encodes the slash so the obvious grep finds nothing. |
| `forge.rs`, `forge/github.rs`, `forge/fake.rs` | The `Forge` trait and shared wire types, the CLI-backed implementation, and the fake. The CLI speaks to one hosting service, so a local server of a different kind would not exercise this path; a fake does. |
| `config.rs` | The registry. `KNIVES_CONFIG_HOME` wins over `XDG_CONFIG_HOME`, because redirecting the latter to isolate this tool also hides the forge CLI's credentials. |
| `store.rs` | Only what cannot be recomputed: claims, fork-only marks, foreign-parent rationale, supersession, last-seen pull heads. Unknown keys are written back untouched. |
| `cli.rs` | The command surface and the exit-code type. Argument shapes are enforced by the parser, not checked at runtime. |
| `commands/` | One module per command. Each splits a pure `render` from a thin `run`, because an earlier draft shipped a `render` with no caller and printed nothing while exiting zero. |

## Testing

Unit tests are pure and live beside their module. Integration tests build a real three-repo world in a temp directory: a bare upstream we cannot push to, a bare origin that is our fork, a jj clone of origin with upstream added, a git clone standing in for the maintainer, and a second jj clone because change ids are identical across disconnected clones and that is what makes divergence reproducible.

The rules that must be proven against real jj, not mocked:

- a squash merge lands content while the branch is not an ancestor of the trunk
- a remote rewrite strands a release parent, and a local rewrite does not
- the three landed outcomes
- the landed probe leaves no trace
- divergence after the same change is rewritten in two clones

## Close out

- `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` all clean
- the no-hard-coded-identity guard passes
- every command run against a real managed repo, read-only, with the output pasted into the final report
- one commit

## Deferred, with reasons

The spec's open questions, unchanged: closed-not-merged while the branch lives; a foreign pull ref advancing under a release; whether hard refusal (a shim) earns its cost, which waits for evidence that layers 1 to 3 were insufficient; workspace lifecycle beyond what `fork release` cleans up; codegraph integration, which must sync before querying because a stale index answers with silence.
