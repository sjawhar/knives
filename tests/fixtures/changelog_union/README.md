# changelog_union fixtures

Real `CHANGELOG.md` and `RELEASE_NOTES.md` content, cut down so a test keeps the real conflict
shape at a fraction of the size. Each file keeps its source's real prefix and inserted blocks and
is cut after the same 30 lines of the base's suffix, so the hunks a merge sees are the real ones.

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

Each member's `CHANGELOG.md` is the base with one block inserted at line 7, and each
`RELEASE_NOTES.md` the base with one block inserted at line 1. The newest member a write brings in
is on top.

The two `union-` directories were checked against the published hand resolutions
`release/v1.55.1/2026-11-13` (23d983a9) and `release/v1.55.1/2026-11-14` (9c4cced4) at full size:
each published file equals the verbatim union except one renumbered version heading per file
(`sami.11` to `sami.12`, and to `sami.13` for the second). knives writes the union verbatim and
leaves the renumbering to a person, who records it with `knives release adopt --why`.

`split-one-path/` rewrote its own block rather than adding one, so a composition that advances to
it is not a union and stays a person's conflict.

## oh-my-pi/ - not a union (the sides sit on different bases)

Source: `cybermonklabs/oh-my-pi`, `packages/coding-agent/CHANGELOG.md`.

| Directory | Source commit | What it is |
|---|---|---|
| `fork-point/` | ff0a986b | the member's fork point |
| `member/` | 300fcb44 | the member, a 5-line insertion over its fork point |
| `release/` | 95018849 | `sami`, an insertion over its own later base 9058ab9d |

`sami` is not an insertion over the member's fork point: upstream moved lines between the two
bases, so a three-way merge of these gives a conflict whose base region is not empty. knives
leaves that one to a person, as the union rule says.
