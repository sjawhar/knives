# changelog_union fixtures

Real `CHANGELOG.md` and `RELEASE_NOTES.md` content, cut down so a test keeps the real conflict
shape at a fraction of the size. Each file keeps its source's real prefix and inserted blocks.
Lines are counted from 1, and a block is placed by the line number its first line has in the
file that holds it.

## ygo/ — a pure union (every member is an insertion over the base)

Source: `theorem-labs/ygo`, series base `rc/v1.55.1-sami.10`.

| Directory | Source commit | What it is |
|---|---|---|
| `base/` | 42c0196a | the series base both files are measured from |
| `sami11/` | 97bc1414 | member `sami11` |
| `split-from-store/` | 9e82ec8e | member `rel/v1.55.1-split-from-store` |
| `client-count/` | 7f3fd8fe | member `rel/v1.55.1-client-count` |
| `split-one-path/` | 8e4e5479 | member `rel/v1.55.1-split-one-path`, a rewrite of 9e82ec8e |
| `union-sami11-split-from-store/` | - | the union knives writes for a composition of those two members |
| `union-client-count-split-from-store-sami11/` | - | the union for those three |

Every ygo file is cut at the same place in the base, so the hunks a merge sees are the real ones.
`base/CHANGELOG.md` is the base's 7-line header and the 29 lines after it (36 lines), and
`base/RELEASE_NOTES.md` the base's first 30 lines. Every other `CHANGELOG.md` here keeps that
header and ends with those 29 lines, and every other `RELEASE_NOTES.md` ends with those 30 lines.
Each member's `CHANGELOG.md` is the base with one block inserted at line 8, right after the
header, and each `RELEASE_NOTES.md` the base with one block inserted at line 1. The newest member
a write brings in is on top.

The blocks of `sami11/`, `split-from-store/` and `client-count/` each claim version `sami.11`, so
`union-sami11-split-from-store/` holds two `sami.11` headings per file and
`union-client-count-split-from-store-sami11/` three. The two
`union-` directories were checked against the published hand resolutions
`release/v1.55.1/2026-11-13` (23d983a9) and `release/v1.55.1/2026-11-14` (9c4cced4) at full size:
each published file equals the verbatim union except its renumbered version headings, one per
file in 23d983a9 (`sami.11` to `sami.12`) and two per file in 9c4cced4 (`sami.11` to `sami.12`
and to `sami.13`). knives writes the union verbatim and leaves the renumbering to a person, who
records it with `knives release adopt --why`.

`split-one-path/` is an insertion over the base, but not over `split-from-store/`: it adds a
`sami.14` block above its earlier block and renumbers that block's heading from `sami.11` to
`sami.12` (in `CHANGELOG.md`, `## [1.55.1-sami.11] — 2026-11-13` becomes
`## [1.55.1-sami.12] — 2026-11-13`). That change rewrites a line a release of `split-from-store`
holds, so a composition that advances to it is not a union and stays a person's conflict.

## oh-my-pi/ - not a union (the sides sit on different bases)

Source: `cybermonklabs/oh-my-pi`, `packages/coding-agent/CHANGELOG.md`.

| Directory | Source commit | What it is |
|---|---|---|
| `fork-point/` | ff0a986b | the member's fork point |
| `member/` | 300fcb44 | the member, a 5-line insertion over its fork point |
| `release/` | 95018849 | `sami`, an insertion over its own later base 9058ab9d |

These files are not cut as the ygo ones are: `fork-point/CHANGELOG.md` is 30 lines in all, and the
member inserts its 5 lines at line 16, under the fork point's first `### Fixed` heading.

`sami` is not an insertion over the member's fork point: upstream moved lines between the two
bases, so a three-way merge of these gives a conflict whose base region is not empty. knives
leaves that one to a person, as the union rule says.
