# Bidirectional gate before a raw rebase

Read this before any `jj rebase -b`, `-s` or `-r` of a branch in a managed fork. The gate checks both directions: the release merges the branch is built on, and the release merges built on the commits a rebase would move. Rewriting a pushed release or retention merge goes unnoticed until a push force-moves its ref, with no error.

1. Set `REFS` to `bookmarks(glob:"release/*") | remote_bookmarks(glob:"release/*") | bookmarks(glob:"keep/*") | remote_bookmarks(glob:"keep/*")`. The `release/*` glob covers dated cuts, every series' cuts and cuts not yet reaped. If the `repos.toml` entry sets `release_branch = "<name>"`, add `| bookmarks(exact:"<name>") | remote_bookmarks(exact:"<name>")`. Use the fork's retention prefix if it is not `keep/`.
2. Set `MOVED` to `roots((<trunk>@upstream | (::<branch> & (REFS)))..<branch>)`: the branch's own commits, without any release merge it is built on.
3. Run `jj log -r 'MOVED:: & (REFS)'`.
4. Empty: run `jj rebase -s 'MOVED' -d <trunk>@upstream`. It moves only the branch's own commits, keeps their change ids, and leaves every release merge on its commit id. Do not use `-b` or `-s '<ref>..<branch>'` instead: `-b` takes along a release merge the branch is built on, and `-s` with a range gives each commit in it the destination as parent.
5. Not empty: the branch's commits feed the merges listed. Run `knives release members`; it shows the release in hand only.
   - Listed there: move the branch only with `knives release rebase`, which moves the merge and each member's commits that the target's history does not hold, and replays recorded conflict resolutions.
   - Not listed: an older cut or a retention ref needs its exact commit, for a frozen pin or local descendants. Make no raw rebase. Get the release owner's sign-off, or wait until its last local descendant goes and `knives release reap` retires it.
