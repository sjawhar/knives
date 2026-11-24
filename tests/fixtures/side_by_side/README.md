# side_by_side fixtures

Real text where several members of a release add lines at one place, cut down so a test keeps
the real conflict shape at a fraction of the size.

## critical-bugs/

Rocq's `dev/doc/critical-bugs.md` in three versions. Each version is cut down to the two places
where the three differ, each with the three lines before it and the three lines after it, which
all three versions hold.

| File | What it is |
|---|---|
| `base.md` | the base of a release series |
| `member.md` | a member that adds one entry to the table of contents and three entries below it |
| `release.md` | a release of that member and others |

The members' entries conflict at both places. A hand resolution kept the member's entries
first at each place, with the other members' entries after them: 5 lines where the member adds
1, and 66 lines where it adds 41. `src/changelog_union.rs` reads these files
(`holds_insertions`).
