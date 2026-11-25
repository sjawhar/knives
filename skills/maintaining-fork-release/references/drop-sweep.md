# Drop sweep

Read this before any parent leaves a release candidate, or before a release-only member is dropped (SKILL.md step 3; stage 2 of [release-and-consumers.md](release-and-consumers.md)).

Run every command from the checkout root, so rows hold repo-relative paths (through `-R`, jj prints paths relative to your cwd). Keep scratch files in `t=$(mktemp -d)`.

## What to sweep

- A parent **leaves** when it is a `drop --why`, a member the cut's composition gate names, or an old-release parent a rebuild leaves out. Sweep gate members before `--allow-drop` accepts them.
- A parent stays when a candidate parent is it, carries its change id, or carries its first line.
- `<candidate>` is the composition in hand without that parent.
- Sweep the `heads()` of the leaving commits and the old commits of advanced members, together. Sweep as leaving only the leaving commits among them. A head's net diff holds its leaving ancestors.
- A non-head whose head removed its content still takes one row that names the removing commit: a consumer pinned to an older release loses that content. The count pass on the non-head's net diff, with its head as the candidate, prints those lines.
- Sweep an **advanced member** at the commit the old release carried, not at its new tip.
- Sweep a leaving **old-release merge** as all its parents, plus its own delta on each path it touches. A parent with no successor is leaving. A parent with one is an advanced member at the carried commit.
  - On a conflict-free path, the delta is `jj diff -r <merge>`.
  - On a conflicted path, the delta is the resolution lines that no parent has, read as added lines: `grep -vxFf parents merge`.
- Sweep the own delta of the old release commit the same way.

## The net diff

The unit is the parent's **net diff from what it shares with the candidate**, never its commits one at a time: per-commit reading judges states the series rewrote, such as every review fix-up.

```sh
jj -R <checkout> --ignore-working-copy diff --from '<fork>' --to <parent> --git   # <fork> = heads(::<parent> & ::<candidate>)
jj -R <checkout> --ignore-working-copy log -r '<candidate>..<parent>' --no-graph   # commit list for citations
```

- `<fork>` is the parent's own fork point, moved up to any commit of its series that a staying member sits on.
- Never use a newer upstream head or the old release's other parents as `<fork>`: content from two leaving parents then looks carried by each and disappears from both diffs.
- When `<fork>` names several heads, diff from their merge, built in git's object store, never as a jj commit that every agent's `jj log` shows. Never select one head by hand.

```sh
g="git --git-dir=$(jj -R <checkout> git root)"
$g merge-tree --write-tree <head1> <head2>            # line 1 is the tree id; exit 1 means a conflict to read by hand
$g commit-tree <tree> -p <head1> -p <head2> -m fork   # base for a third head
$g diff <tree> <parent>                               # the net diff
$g show <tree>:<path>                                 # the fork copy of a path
```

## Settle mechanically, then read the residue

1. `knives release members <candidate> --carries <commit>` settles a row only when it says carried. For a commit the candidate reaches, it reads the commit's change, from its fork point with the trunk, by its own lines, as `members <candidate> --verify` does: carried when the candidate holds every line that the change adds, does not put back a line that it removed, and holds none of its lines only by their words. A commit the candidate does not reach is replayed onto the candidate instead. A commit the candidate reaches that lacks a line, puts one back, or holds one only by its words, reads `merged-then-edited` (exit `3`): read that row per line. Ask each commit of the branch. A later commit that rewrote an earlier commit's line holds its own version of it, so it can read carried where the earlier commit does not.
2. Count **copies** per path: presence is not a count, because a lost `else:` would settle on an unrelated `else:`. Get the paths from the net diff with `--name-only`, and one path's hunks (`<path diff>` below) by appending `-- 'root:"<path>"'`.

   ```sh
   jj --ignore-working-copy file show -r <candidate> 'root:"<path>"' > "$t/cand"
   jj --ignore-working-copy file show -r <parent> 'root:"<path>"' > "$t/par"
   comm -23 <(sort "$t/par") <(sort "$t/cand") > "$t/fewer"
   comm -13 <(sort "$t/par") <(sort "$t/cand") > "$t/more"
   <path diff> | sed -n '/^@@/,$s/^+//p' | grep -xFf "$t/fewer"   # added copies the candidate lacks
   <path diff> | sed -n '/^@@/,$s/^-//p' | grep -xFf "$t/more"    # removed copies the candidate keeps
   ```

   Lines these do not print are settled. Their row cites the candidate lines.
3. For a path the candidate does not have, `jj --ignore-working-copy log -r '::<candidate>' -n 1 -- 'root:"<path>"'` names the removing change. `--summary` on it shows what it added. Run the pass on that path. The move is the replacement.
4. Rule first on added residue that names an identifier absent from the whole candidate, and on every removed residue line. A lost deletion names only what the candidate has.
5. Read a lockfile per package. It is settled when the candidate lock is current for its manifests, for uv by `uv lock --check` in an export. It must also have each package of the parent lock at the same or a later version. A missing or older package is residue.

One row can cover adjacent residue lines with one job. Split it where a name has no successor.

## Verdicts

Each line of the net diff gets a verdict in a `record:` notch row with its path and diff line numbers.

- **`carried` / `on trunk`**: the candidate holds the line at that path, as many copies as the parent. Where it holds fewer, the merge decides which copy. Cite the candidate lines and the commits that added them, from the candidate, never from a commit's own diff:

  ```sh
  jj --ignore-working-copy file annotate -r <candidate> -T 'commit.commit_id().short() ++ " " ++ line_number ++ ": " ++ content' <path>
  ```

  Use commit ids (a change id can be divergent) and a plain path (`file annotate` refuses `root:"..."`).
- **By replacement**: cite the candidate lines that do the job now, with their commits. Each name that the line defines or reads must have a successor among them. A neighbour with a different job is not a successor.
- State each value that a replacement changes: a default, a limit, an audience, a version. When an advanced member's successor changes it, the pin PR delta discloses it.
- "Superseded" and "landed upstream" are `carried` or `on trunk` with lines cited, never `dropped`.
- **`dropped`** cites the notch or ruling behind the commit that wrote the line (`file annotate -r <parent>`). A composition parent speaks only for the composition.
- Alternatively, `dropped` cites a search of each consumer's pinned revision that shows no use, including a configuration key or keyword the consumer sets and the fork reads. A literal name match is not this search.
- A **removed** line that the candidate still has needs a verdict: why the deletion is not necessary, or a lost deletion.
- A line with no verdict is a **loss to restore**, however much of its path or hunk is settled.

## Restoring a loss

- Restore by block and merge. Never type it. A **block** is the removed and added lines between two context lines.
- A block with a printed line goes back whole, with its settled lines. A settled line next to a loss can be part of the lost code.
- Put each block of the parent copy with no printed line back to its fork side. Then merge: `git merge-file -p "$t/cand" "$t/fork" "$t/restore"`, with the fork copy from `file show -r <fork>`. Rule on what it lands and on each conflict.
  - A line the candidate already holds at that place comes out once and is `carried`.
  - A place that the candidate rewrote is a conflict. Read it by hand, with the restore side as the lost block. `git merge-file --diff3` shows the full place.
  - A shared run of four or more lines with a letter or digit comes out once, between two conflicts. Each side needs it.
- Never use a two-way `diff` of parent and candidate. It pairs lines by text and loses parts of the block.
- The restore is a member, classified like others, never a drop. When the candidate does that work in a different place, put the restore there.

## Finished

The sweep is finished when each line that the passes print, the merge lands or a non-head holds has its own `record:` row: per path, those lines less the lines the rows name print nothing. One row per path, or a path list, does not do this, and non-head lines can be on paths that no swept diff lists.
