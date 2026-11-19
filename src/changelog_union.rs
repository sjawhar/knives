//! The changelog resolution a release write writes itself.
//!
//! A release composition conflicts in `CHANGELOG.md` and `RELEASE_NOTES.md`
//! for a reason that is not a disagreement: every member adds its own section
//! at the top of the file, so the sides insert different text in the same
//! place and none of them touches what another wrote. The resolution is the
//! same every time — keep every block — so a person who writes it by hand
//! writes nothing a machine could not have written. This module writes it,
//! and leaves to a person the conflicts where a side changed what another
//! side's text says.
//!
//! The rule is deliberately narrow. A union exists only where every side's
//! change at the conflicting place is an insertion: the moment one side
//! rewrote or dropped lines another side kept, the file is a real
//! disagreement and no concatenation of the sides is its resolution. The
//! union is also verbatim line by line — knives never renumbers or rewrites
//! a line — so two blocks claiming one version heading survive into it, and
//! [`Union::first_lines`] names them so a reader sees that at once and
//! settles it with `knives release adopt --why`.
//!
//! One heading is written once. At each conflicting place, the block that
//! the union writes last is the one that the other blocks' lists join: the
//! earlier release's block where that release inserted its own text there,
//! as the union writes that block last ([`side_order`]), else the last
//! block in jj's term order. Where that block already has a `### ` heading,
//! such as `### Fixed` in its `[Unreleased]` section, a list of entries that
//! another block puts under that same heading, in the same section, joins
//! its list rather than repeating the heading ([`joined_blocks`]). A heading
//! is the same only where its text is the same: one that differs by case or
//! by a trailing space is another heading. Each entry line is written as
//! its side wrote it, and the entries keep the union's order: the entries
//! of the other blocks first, in the order the union writes them, then the
//! entries the last block already holds. So the entries the write brings in
//! go above the entries that the earlier release already published. A
//! heading that the last block lacks is written in its own block, as before.
//!
//! One insertion is left to a person too: a side's block whose last line has
//! no newline, where something would follow it. Written verbatim, that line
//! and the next would run together into one line no side wrote, and the union
//! never adds a byte to keep them apart. Such a block is unioned only where it
//! ends the file.
//!
//! Everything here is bytes in, bytes out: no repository, no file, no
//! `String`. A changelog carries CRLF line endings and, on an old project,
//! bytes that are not UTF-8, and a release write must hand back what it was
//! given.

use std::borrow::Cow;

use jj_lib::files::{MergeResult, merge_hunks};
use jj_lib::merge::Merge;
use jj_lib::tree_merge::MergeOptions;

/// Whether a path is a release log whose conflicts this module may resolve.
///
/// The two names are fixed and there is no setting for them: these are the
/// files a fork's releases collide in, and a repository that keeps its notes
/// under another name is a reason to add a setting then, not a reason to
/// carry one no repository uses. The path is the repository-internal string
/// ([`jj_lib::repo_path::RepoPath::as_internal_file_string`]), whose
/// separator is always `/`, so the name is its last component and the
/// directory it sits in does not matter.
pub fn eligible(path: &str) -> bool {
    let name = path.rsplit_once('/').map_or(path, |(_, name)| name);
    matches!(name, "CHANGELOG.md" | "RELEASE_NOTES.md")
}

/// One unioned file: the bytes to write, and what the union put in them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Union {
    /// The file's new content, each line of it a line that a side wrote.
    pub content: Vec<u8>,
    /// The first line of each block the union wrote, in the order written,
    /// without its line terminator. Two equal entries are two blocks claiming
    /// one heading, which is the one thing a verbatim union cannot settle and
    /// the reason a reader is given this list at all. A block whose every
    /// list joined the block written last ([`joined_blocks`]) is written in
    /// that block, and is not named on its own.
    pub first_lines: Vec<Vec<u8>>,
}

/// The union of a file's conflicting terms, or `None` when the conflict is
/// not one a union resolves.
///
/// `contents` holds the file's term contents as the composition's tree merge
/// has them: one add per side, with the bases they are measured from between.
/// `options` is the repository's own merge configuration
/// ([`jj_lib::store::Store::merge_options`]), because a union that split the
/// file into hunks differently from the merge it stands in for would be
/// resolving a conflict the repository does not have.
///
/// `earlier_release` is the file's content in the release this write
/// replaces, where there is one. That side is what the fork already
/// published, so its blocks go under the blocks this write brings in, which
/// is where a person writing the resolution by hand puts them. It is found
/// by value and not by index because [`Merge::simplify`] cancels terms, so
/// no index names it reliably; where it has been cancelled no side matches
/// and every side keeps the order jj gave it, which is also what a fresh
/// composition with no earlier release gets. At each conflicting place, a
/// list under a heading that the block written last already has joins that
/// block's list ([`joined_blocks`]): the earlier release's block where it
/// is written there, else the last side's.
pub fn union_of<T: AsRef<[u8]>>(
    contents: &Merge<T>,
    earlier_release: Option<&[u8]>,
    options: &MergeOptions,
) -> Option<Union> {
    let MergeResult::Conflict(hunks) = merge_hunks(contents, options) else {
        // A merge that resolves is the tree merge's own answer, and the write
        // has nothing to put in its place.
        return None;
    };
    let earlier = earlier_release
        .and_then(|published| contents.adds().position(|side| side.as_ref() == published));
    let order = side_order(contents.num_sides(), earlier);
    let mut union = Union {
        content: Vec::new(),
        first_lines: Vec::new(),
    };
    // Whether the bytes written last are an inserted block whose last line
    // has no newline. Nothing may follow such a block: its last line and the
    // next one would run together into a line no side wrote, and the union
    // adds no byte to keep them apart. So such a block is written only where
    // it ends the file, and any byte after it leaves the conflict to a person.
    let mut unterminated = false;
    for hunk in &hunks {
        if let Some(resolved) = hunk.as_resolved() {
            if unterminated {
                return None;
            }
            union.content.extend_from_slice(resolved);
            continue;
        }
        // A base region that is not empty is text one side kept and another
        // changed: a disagreement, and a person's.
        if hunk.removes().any(|base_region| !base_region.is_empty()) {
            return None;
        }
        // Dropped per hunk, not per file: a side that inserted the same text
        // as another side contributes one block here, while a side that
        // inserted the same text somewhere else contributes one there too.
        let mut inserted: Vec<&[u8]> = Vec::new();
        for &side in &order {
            // A hunk carries the sides of the file it was cut from, so this
            // holds; `?` says so without a branch that cannot be reached.
            let text = hunk.get_add(side)?.as_slice();
            if !text.is_empty() && !inserted.contains(&text) {
                inserted.push(text);
            }
        }
        for block in joined_blocks(&inserted) {
            if block.is_empty() {
                continue;
            }
            if unterminated {
                return None;
            }
            union.first_lines.push(first_line(&block).to_vec());
            union.content.extend_from_slice(&block);
            unterminated = !block.ends_with(b"\n");
        }
    }
    Some(union)
}

/// The blocks that one conflicting place of a union writes: `inserted`, each
/// side's text once, in the order the union writes them. The last of them
/// is the join target: the earlier release's block where it is written
/// there, else the last side's ([`union_of`]). Each list of another block
/// whose `### ` heading the target already has, the same text, joins the
/// target's list there.
///
/// A list is a `### ` heading and the lines under it, up to the next
/// heading, in the section open at that place: before the block's first
/// `# ` or `## ` heading ([`heading_lists`]). Its entries, its lines less the
/// blank lines around them, go under the target's heading, after the blank
/// lines that follow it and before the entries it already holds: the union
/// writes the other blocks' entries in the order it writes those blocks,
/// above the entries of the block it writes last, so what the write brings
/// in goes above what the earlier release published. The heading is written
/// once, so the other block loses the list, with the blank lines before it
/// where nothing else comes before it; a block left with only blank lines
/// writes nothing. A block whose last line has no newline is written as it
/// is: its last entry could not be followed by another.
fn joined_blocks<'a>(inserted: &[&'a [u8]]) -> Vec<Cow<'a, [u8]>> {
    let Some((&last, brought)) = inserted
        .split_last()
        .filter(|(_, brought)| !brought.is_empty())
    else {
        return inserted.iter().map(|block| Cow::Borrowed(*block)).collect();
    };
    let last_lines = lines(last);
    // A heading with no newline ends the last block, and nothing can follow
    // it.
    let mut targets: Vec<Joining<'_, 'a>> = heading_lists(&last_lines)
        .into_iter()
        .filter_map(|(heading, end)| {
            let line = last_lines
                .get(heading)
                .filter(|line| line.ends_with(b"\n"))?;
            let blanks = last_lines
                .iter()
                .take(end)
                .skip(heading + 1)
                .take_while(|line| is_blank(line))
                .count();
            Some(Joining {
                heading: first_line(line),
                at: heading + 1 + blanks,
                entries: Vec::new(),
            })
        })
        .collect();
    if targets.is_empty() {
        return inserted.iter().map(|block| Cow::Borrowed(*block)).collect();
    }
    let mut blocks: Vec<Cow<'a, [u8]>> = Vec::with_capacity(inserted.len());
    for &block in brought {
        blocks.push(without_joined_lists(block, &mut targets));
    }
    if targets.iter().all(|target| target.entries.is_empty()) {
        blocks.push(Cow::Borrowed(last));
        return blocks;
    }
    let mut written = Vec::with_capacity(last.len());
    for at in 0..=last_lines.len() {
        for target in targets.iter().filter(|target| target.at == at) {
            written.extend(target.entries.iter().copied().flatten());
        }
        if let Some(line) = last_lines.get(at) {
            written.extend_from_slice(line);
        }
    }
    blocks.push(Cow::Owned(written));
    blocks
}

/// A list of the block that a union writes last at a conflicting place,
/// which other blocks' lists join ([`joined_blocks`]).
struct Joining<'h, 'a> {
    /// Its heading's text ([`first_line`]).
    heading: &'h [u8],
    /// The index of the line that the joining entries go before: the line
    /// after its heading and the blank lines that follow it.
    at: usize,
    /// The joining entries, in the order the union writes their blocks.
    entries: Vec<&'a [u8]>,
}

/// `block`, one side's block at a conflicting place, less each list whose
/// heading one of `targets`, the lists of the block written last there,
/// has: that list's entries are added to that target's ([`joined_blocks`]).
fn without_joined_lists<'a>(block: &'a [u8], targets: &mut [Joining<'_, 'a>]) -> Cow<'a, [u8]> {
    if !block.ends_with(b"\n") {
        return Cow::Borrowed(block);
    }
    let Some(taken) = take_lists(&lines(block), |text| {
        targets.iter().any(|target| target.heading == text)
    }) else {
        return Cow::Borrowed(block);
    };
    for list in taken.lists {
        if let Some(target) = targets
            .iter_mut()
            .find(|target| target.heading == list.heading)
        {
            target.entries.extend(list.entries);
        }
    }
    if taken.rest.iter().all(|line| is_blank(line)) {
        return Cow::Owned(Vec::new());
    }
    Cow::Owned(taken.rest.concat())
}

/// A `### ` list of a block ([`take_lists`]).
struct List<'a> {
    /// Its heading's text ([`first_line`]).
    heading: &'a [u8],
    /// Its entries: its lines, less the blank lines around them.
    entries: Vec<&'a [u8]>,
}

/// A block's lines split by [`take_lists`].
struct Taken<'a> {
    /// The lists taken.
    lists: Vec<List<'a>>,
    /// The block's other lines.
    rest: Vec<&'a [u8]>,
}

/// The `### ` lists of `block_lines`, a block's lines, that `wanted` takes
/// by their heading's text ([`heading_lists`]), and the block's other lines.
/// A list taken goes with the blank lines before it where nothing else comes
/// before it. `None` where no list is taken.
fn take_lists<'a>(
    block_lines: &[&'a [u8]],
    mut wanted: impl FnMut(&[u8]) -> bool,
) -> Option<Taken<'a>> {
    let mut kept = vec![true; block_lines.len()];
    let mut lists = Vec::new();
    for (index, (heading, end)) in heading_lists(block_lines).into_iter().enumerate() {
        let Some(text) = block_lines
            .get(heading)
            .map(|&line| first_line(line))
            .filter(|&text| wanted(text))
        else {
            continue;
        };
        let body: Vec<&[u8]> = block_lines
            .iter()
            .take(end)
            .skip(heading + 1)
            .copied()
            .collect();
        let first = body
            .iter()
            .position(|line| !is_blank(line))
            .unwrap_or(body.len());
        let last = body
            .iter()
            .rposition(|line| !is_blank(line))
            .map_or(first, |last| last + 1);
        lists.push(List {
            heading: text,
            entries: body.get(first..last).unwrap_or_default().to_vec(),
        });
        let from = if index == 0 && block_lines.iter().take(heading).all(|line| is_blank(line)) {
            0
        } else {
            heading
        };
        for keep in kept.iter_mut().take(end).skip(from) {
            *keep = false;
        }
    }
    if lists.is_empty() {
        return None;
    }
    let rest = block_lines
        .iter()
        .zip(&kept)
        .filter(|(_, keep)| **keep)
        .map(|(line, _)| *line)
        .collect();
    Some(Taken { lists, rest })
}

/// Whether `target` holds `block`, a member's inserted block, as a union
/// writes it where the block's lists joined the block written last
/// ([`joined_blocks`]): the entries of each of its `### ` lists as a whole
/// run of the target's lines under that heading, the nearest heading above
/// them, and the rest of the block as a whole run of the target's lines, or
/// nothing but blank lines.
fn holds_joined(target: &[u8], block: &[u8]) -> bool {
    let Some(taken) = take_lists(&lines(block), |_| true) else {
        return false;
    };
    let target_lines = lines(target);
    taken
        .lists
        .iter()
        .all(|list| listed_under(&target_lines, list.heading, &list.entries))
        && (taken.rest.iter().all(|line| is_blank(line))
            || is_line_run(target, &taken.rest.concat()))
}

/// Whether `entries` are a whole run of `target_lines` whose nearest heading
/// above them is `heading`; for a heading with no entries, whether a line of
/// the target is that heading.
fn listed_under(target_lines: &[&[u8]], heading: &[u8], entries: &[&[u8]]) -> bool {
    if entries.is_empty() {
        return target_lines.iter().any(|line| first_line(line) == heading);
    }
    target_lines
        .windows(entries.len())
        .enumerate()
        .any(|(start, window)| {
            window == entries
                && target_lines
                    .iter()
                    .take(start)
                    .rev()
                    .find(|line| heading_level(line).is_some())
                    .is_some_and(|line| first_line(line) == heading)
        })
}

/// `block`'s lines, each with its terminator.
fn lines(block: &[u8]) -> Vec<&[u8]> {
    block.split_inclusive(|byte| *byte == b'\n').collect()
}

/// Whether a line holds nothing but whitespace.
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// The level of a heading line, `# ` to `### `, or `None` for any other
/// line.
fn heading_level(line: &[u8]) -> Option<usize> {
    let level = line.iter().take_while(|byte| **byte == b'#').count();
    ((1..=3).contains(&level) && line.get(level) == Some(&b' ')).then_some(level)
}

/// Each `### ` list of `lines`, a block's lines, in the section open at the
/// place the block was inserted: before the block's first `# ` or `## `
/// heading. A list is its heading's index and the end of its lines, at the
/// next `### ` heading or the end of that section.
fn heading_lists(lines: &[&[u8]]) -> Vec<(usize, usize)> {
    let open = lines
        .iter()
        .position(|line| heading_level(line).is_some_and(|level| level < 3))
        .unwrap_or(lines.len());
    let headings: Vec<usize> = lines
        .iter()
        .take(open)
        .enumerate()
        .filter(|(_, line)| heading_level(line) == Some(3))
        .map(|(at, _)| at)
        .collect();
    let ends = headings
        .iter()
        .skip(1)
        .copied()
        .chain(std::iter::once(open));
    headings.iter().copied().zip(ends).collect()
}

/// Whether `target` already holds what `member` added to `base`.
///
/// A carriage check replays a member's change onto a target and reads what is
/// left over. A member whose block a release's union holds verbatim has
/// nothing left over, but the replay still conflicts, because the union's
/// other blocks sit where the member's block ends. This says what such a
/// replay means: a conflict every side of which only inserted, and whose
/// member side the target holds as a whole run of its own lines, or holds as
/// a union writes it where its lists joined another block's
/// ([`holds_joined`]), is the target carrying that block — the conflict is
/// masked. A member whose lines the target lost is not carried, and says so.
pub fn masks(target: &[u8], base: &[u8], member: &[u8], options: &MergeOptions) -> bool {
    let replay = Merge::from_removes_adds([base], [target, member]);
    match merge_hunks(&replay, options) {
        MergeResult::Resolved(content) => content.as_slice() == target,
        MergeResult::Conflict(hunks) => {
            let mut replayed: Vec<u8> = Vec::new();
            for hunk in &hunks {
                if let Some(resolved) = hunk.as_resolved() {
                    replayed.extend_from_slice(resolved);
                    continue;
                }
                if hunk.removes().any(|base_region| !base_region.is_empty()) {
                    return false;
                }
                let (Some(target_side), Some(member_side)) = (hunk.get_add(0), hunk.get_add(1))
                else {
                    return false;
                };
                if !is_line_run(target, member_side.as_slice())
                    && !holds_joined(target, member_side.as_slice())
                {
                    return false;
                }
                replayed.extend_from_slice(target_side);
            }
            replayed == target
        }
    }
}

/// The side indices in the order the union writes them: jj's term order, with
/// `earlier`, the side holding the earlier release's content, last.
fn side_order(sides: usize, earlier: Option<usize>) -> Vec<usize> {
    let mut order: Vec<usize> = (0..sides).filter(|side| Some(*side) != earlier).collect();
    order.extend(earlier);
    order
}

/// A block's first line, without its terminator, for a reader comparing
/// headings. A CR is cut with the LF so a file with CRLF line endings reads
/// like any other here.
fn first_line(block: &[u8]) -> &[u8] {
    let line = block.split(|byte| *byte == b'\n').next().unwrap_or(block);
    line.strip_suffix(b"\r").unwrap_or(line)
}

/// Whether `text` occurs in `target` as a whole number of consecutive lines.
///
/// Line-aligned rather than any substring, because a block the target carries
/// is the block's own lines: text that lands inside a line of the target, or
/// that the target holds with a line of it missing, is not the block the
/// member wrote. Empty text is vacuously carried — the member inserted
/// nothing there.
fn is_line_run(target: &[u8], text: &[u8]) -> bool {
    if text.is_empty() {
        return true;
    }
    let ends_a_line = text.ends_with(b"\n");
    target
        .windows(text.len())
        .enumerate()
        .any(|(start, window)| {
            window == text
                && start
                    .checked_sub(1)
                    .is_none_or(|before| target.get(before) == Some(&b'\n'))
                && (ends_a_line || start + text.len() == target.len())
        })
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::expect_used,
        reason = "a union a test demands and does not get is the failure"
    )]

    use jj_lib::files::FileMergeHunkLevel;
    use jj_lib::merge::SameChange;

    use super::*;

    const BASE_LOG: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/base/CHANGELOG.md");
    const BASE_NOTES: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/base/RELEASE_NOTES.md");
    const SAMI11_LOG: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/sami11/CHANGELOG.md");
    const SAMI11_NOTES: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/sami11/RELEASE_NOTES.md");
    const SPLIT_FROM_STORE_LOG: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/split-from-store/CHANGELOG.md");
    const SPLIT_FROM_STORE_NOTES: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/split-from-store/RELEASE_NOTES.md");
    const CLIENT_COUNT_LOG: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/client-count/CHANGELOG.md");
    const CLIENT_COUNT_NOTES: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/client-count/RELEASE_NOTES.md");
    const SPLIT_ONE_PATH_LOG: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/ygo/split-one-path/CHANGELOG.md");
    const UNION_TWO_LOG: &[u8] = include_bytes!(
        "../tests/fixtures/changelog_union/ygo/union-sami11-split-from-store/CHANGELOG.md"
    );
    const UNION_TWO_NOTES: &[u8] = include_bytes!(
        "../tests/fixtures/changelog_union/ygo/union-sami11-split-from-store/RELEASE_NOTES.md"
    );
    const UNION_THREE_LOG: &[u8] = include_bytes!(
        "../tests/fixtures/changelog_union/ygo/union-client-count-split-from-store-sami11/CHANGELOG.md"
    );
    const UNION_THREE_NOTES: &[u8] = include_bytes!(
        "../tests/fixtures/changelog_union/ygo/union-client-count-split-from-store-sami11/RELEASE_NOTES.md"
    );
    const OMP_FORK_POINT: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/oh-my-pi/fork-point/CHANGELOG.md");
    const OMP_MEMBER: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/oh-my-pi/member/CHANGELOG.md");
    const OMP_RELEASE: &[u8] =
        include_bytes!("../tests/fixtures/changelog_union/oh-my-pi/release/CHANGELOG.md");

    /// jj's own defaults for `merge.hunk-level` and `merge.same-change`, which
    /// is what a repository hands the caller unless it was configured
    /// otherwise.
    fn merge_options() -> MergeOptions {
        MergeOptions {
            hunk_level: FileMergeHunkLevel::Line,
            same_change: SameChange::Accept,
        }
    }

    /// A merge of byte-string literals, so a test writes its terms as it
    /// reads them rather than casting each one.
    fn merge_of<'a, const BASES: usize, const SIDES: usize>(
        bases: [&'a [u8]; BASES],
        sides: [&'a [u8]; SIDES],
    ) -> Merge<&'a [u8]> {
        Merge::from_removes_adds(bases, sides)
    }

    /// The union's bytes, with a readable rendering when they are wrong.
    fn assert_bytes(got: &[u8], want: &[u8]) {
        assert!(
            got == want,
            "union differs:\n{}",
            String::from_utf8_lossy(got)
        );
    }

    fn headings(union: &Union) -> Vec<String> {
        union
            .first_lines
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    #[test]
    fn two_members_union_with_the_earlier_releases_block_under_the_new_one() {
        // Given: the series base and two members, each of which inserted its
        // own section, where sami11's file is what the earlier release holds
        // and split-from-store is what this write brings in
        let options = merge_options();
        for (base, earlier, brought_in, expected, expected_headings) in [
            (
                BASE_LOG,
                SAMI11_LOG,
                SPLIT_FROM_STORE_LOG,
                UNION_TWO_LOG,
                [
                    "## [1.55.1-sami.11] — 2026-11-13",
                    "## [1.55.1-sami.11] — 2026-11-12",
                ],
            ),
            (
                BASE_NOTES,
                SAMI11_NOTES,
                SPLIT_FROM_STORE_NOTES,
                UNION_TWO_NOTES,
                ["## v1.55.1-sami.11", "## v1.55.1-sami.11"],
            ),
        ] {
            // When: the write unions the two sides, naming the earlier
            // release's content
            let contents = Merge::from_removes_adds([base], [earlier, brought_in]);
            let union = union_of(&contents, Some(earlier), &options)
                .expect("both sides only inserted their own section");

            // Then: the bytes are the published file, the block this write
            // brings in on top, and the two blocks are reported by their first
            // lines so a reader sees they claim one version
            assert_bytes(&union.content, expected);
            assert_eq!(headings(&union), expected_headings);
        }
    }

    #[test]
    fn three_members_union_in_term_order_when_no_side_is_the_earlier_release() {
        // Given: the series base and three members, and an earlier release
        // whose file is itself a union, so no side holds it
        let options = merge_options();
        for (base, members, earlier, expected) in [
            (
                BASE_LOG,
                [CLIENT_COUNT_LOG, SPLIT_FROM_STORE_LOG, SAMI11_LOG],
                UNION_TWO_LOG,
                UNION_THREE_LOG,
            ),
            (
                BASE_NOTES,
                [CLIENT_COUNT_NOTES, SPLIT_FROM_STORE_NOTES, SAMI11_NOTES],
                UNION_TWO_NOTES,
                UNION_THREE_NOTES,
            ),
        ] {
            // When: the write unions all three
            let contents = Merge::from_removes_adds([base, base], members);
            let union = union_of(&contents, Some(earlier), &options)
                .expect("all three sides only inserted their own section");

            // Then: the bytes are the published file, the members in the order
            // the composition holds them, newest on top
            assert_bytes(&union.content, expected);
            assert_eq!(union.first_lines.len(), 3);
        }
    }

    #[test]
    fn sides_on_different_bases_are_not_a_union() {
        // Given: the oh-my-pi member and the `sami` release, which sit on
        // different bases, so the release is not an insertion over the
        // member's fork point
        let options = merge_options();
        let contents = Merge::from_removes_adds([OMP_FORK_POINT], [OMP_RELEASE, OMP_MEMBER]);

        // When: the write tries to union them
        let union = union_of(&contents, Some(OMP_RELEASE), &options);

        // Then: there is none — a hunk's base region is not empty, so the
        // sides disagree about text neither of them merely added
        assert_eq!(union, None);
    }

    #[test]
    fn a_member_that_rewrote_its_own_block_is_not_a_union() {
        // Given: a composition that advances a member from split-from-store to
        // split-one-path, which rewrote the section it had published rather
        // than adding one, while the earlier release still holds the old one
        let options = merge_options();
        let contents =
            Merge::from_removes_adds([SPLIT_FROM_STORE_LOG], [UNION_TWO_LOG, SPLIT_ONE_PATH_LOG]);

        // When: the write tries to union them
        let union = union_of(&contents, Some(UNION_TWO_LOG), &options);

        // Then: there is none, because the rewrite's hunk removes lines the
        // other side kept
        assert_eq!(union, None);
    }

    #[test]
    fn an_empty_side_contributes_nothing() {
        // Given: a file neither base holds (an absent term, empty at the
        // content level), added by two sides while a third adds nothing
        let options = merge_options();
        let contents = merge_of([b"", b""], [b"alpha\n", b"", b"beta\n"]);

        // When: the write unions them
        let union = union_of(&contents, None, &options).expect("every side only inserted");

        // Then: the two sides that inserted are written, and the empty one is
        // neither written nor reported
        assert_bytes(&union.content, b"alpha\nbeta\n");
        assert_eq!(headings(&union), ["alpha", "beta"]);
    }

    #[test]
    fn an_insertion_at_the_end_keeps_a_missing_final_newline() {
        // Given: two sides appending to the end of a file, the second of them
        // without a final newline
        let options = merge_options();
        let contents = merge_of([b"a\n"], [b"a\nb\n", b"a\nc"]);

        // When: the write unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the bytes are the two inserts verbatim, and no newline is
        // invented for the file's last line
        assert_bytes(&union.content, b"a\nb\nc");
        assert_eq!(headings(&union), ["b", "c"]);
    }

    #[test]
    fn two_blocks_with_no_final_newline_are_not_a_union() {
        // Given: two sides that each append a block at the end of a file,
        // neither of them with a final newline
        let options = merge_options();
        let contents = merge_of(
            [b"# Changelog\n"],
            [b"# Changelog\n## a\n- a", b"# Changelog\n## b\n- b"],
        );

        // When: the write tries to union them, in either order
        // Then: there is none, because the last line of the block written
        // first and the first line of the other would be written as one line
        assert_eq!(union_of(&contents, None, &options), None);
        assert_eq!(
            union_of(&contents, Some(b"# Changelog\n## a\n- a"), &options),
            None
        );
    }

    #[test]
    fn a_block_with_no_final_newline_is_unioned_only_where_it_ends_the_file() {
        // Given: two sides appending to the end of a file, the first of them
        // in jj's term order without a final newline
        let options = merge_options();
        let contents = merge_of([b"a\n"], [b"a\nb", b"a\nc\n"]);

        // When: that block would be written first
        // Then: there is none, because the other side's block would follow it
        // on its last line
        assert_eq!(union_of(&contents, None, &options), None);

        // When: the earlier release holds that block, so it is written last
        let union = union_of(&contents, Some(b"a\nb"), &options).expect("both sides only inserted");
        // Then: it ends the file, verbatim
        assert_bytes(&union.content, b"a\nc\nb");
        assert_eq!(headings(&union), ["c", "b"]);
    }

    #[test]
    fn crlf_line_endings_survive() {
        // Given: a file with CRLF line endings that two sides insert into
        let options = merge_options();
        let contents = merge_of([b"a\r\n"], [b"x\r\na\r\n", b"y\r\na\r\n"]);

        // When: the write unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: every CR is still there, and a heading is reported without it
        assert_bytes(&union.content, b"x\r\ny\r\na\r\n");
        assert_eq!(headings(&union), ["x", "y"]);
    }

    #[test]
    fn bytes_that_are_not_utf8_survive() {
        // Given: sides inserting Latin-1 text, which is not UTF-8
        let options = merge_options();
        let contents = merge_of([b"a\n"], [b"caf\xe9\na\n", b"na\xefve\na\n"]);

        // When: the write unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the bytes come back as they went in, undecoded
        assert_bytes(&union.content, b"caf\xe9\nna\xefve\na\n");
        assert_eq!(
            union.first_lines,
            [b"caf\xe9".to_vec(), b"na\xefve".to_vec()]
        );
    }

    #[test]
    fn two_sides_inserting_the_same_text_write_it_once() {
        // Given: three sides, two of which inserted byte-equal text
        let options = merge_options();
        let contents = merge_of([b"a\n", b"a\n"], [b"x\na\n", b"x\na\n", b"y\na\n"]);

        // When: the write unions them
        let union = union_of(&contents, None, &options).expect("every side only inserted");

        // Then: the shared block is written once, and reported once
        assert_bytes(&union.content, b"x\ny\na\n");
        assert_eq!(headings(&union), ["x", "y"]);
    }

    #[test]
    fn the_earlier_releases_side_is_written_last_and_only_when_a_side_holds_it() {
        // Given: two sides inserting at the same place, in jj's term order
        let options = merge_options();
        let contents = merge_of([b"a\n"], [b"x\na\n", b"y\na\n"]);

        // When: the earlier release's file is the first side's
        let union =
            union_of(&contents, Some(b"x\na\n"), &options).expect("both sides only inserted");
        // Then: that side goes under the one this write brings in
        assert_bytes(&union.content, b"y\nx\na\n");

        // When: there is no earlier release, a fresh composition
        let union = union_of(&contents, None, &options).expect("both sides only inserted");
        // Then: jj's term order stands
        assert_bytes(&union.content, b"x\ny\na\n");

        // When: the earlier release's side has been cancelled, so no side
        // holds its content
        let union =
            union_of(&contents, Some(b"z\na\n"), &options).expect("both sides only inserted");
        // Then: jj's term order stands there too
        assert_bytes(&union.content, b"x\ny\na\n");
    }

    /// A changelog whose `[Unreleased]` section holds `unreleased`.
    fn unreleased(unreleased: &str) -> Vec<u8> {
        format!("# Changelog\n\n## [Unreleased]\n\n{unreleased}## [1.0.0]\n\n- First.\n")
            .into_bytes()
    }

    #[test]
    fn a_list_under_a_heading_the_earlier_release_has_joins_that_list() {
        // Given: an `[Unreleased]` section that the earlier release gave a
        // `### Fixed` list of one entry, and a member that gave it a
        // `### Fixed` list of two, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let earlier = unreleased("### Fixed\n\n- Earlier fix.\n\n");
        let member = unreleased("### Fixed\n\n- Member fix one.\n- Member fix two.\n\n");
        let contents = merge_of([base.as_slice()], [earlier.as_slice(), member.as_slice()]);

        // When: the write unions them, naming the earlier release's content
        let union = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("both sides only inserted");

        // Then: the heading is written once, in one block, with the member's
        // entries above the earlier release's, each line as its side wrote it
        assert_bytes(
            &union.content,
            &unreleased("### Fixed\n\n- Member fix one.\n- Member fix two.\n- Earlier fix.\n\n"),
        );
        assert_eq!(union.first_lines.len(), 1);

        // And: the content check reads each side as carried by that union
        assert!(masks(&union.content, &base, &member, &options));
        assert!(masks(&union.content, &base, &earlier, &options));
    }

    #[test]
    fn a_list_under_a_heading_the_last_block_lacks_is_a_block_of_its_own() {
        // Given: an `[Unreleased]` section that the earlier release gave a
        // `### Fixed` list, a member that gave it an `### Added` list, and
        // one that gave it a `### Fixed` list, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let earlier = unreleased("### Fixed\n\n- Earlier fix.\n\n");
        let added = unreleased("### Added\n\n- Member feature.\n\n");
        let fixed = unreleased("### Fixed\n\n- Member fix.\n\n");

        // When: the write unions the earlier release and the first member
        let contents = merge_of([base.as_slice()], [earlier.as_slice(), added.as_slice()]);
        let union = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("both sides only inserted");

        // Then: the member's list is its own block, above the earlier
        // release's, as before
        assert_bytes(
            &union.content,
            &unreleased("### Added\n\n- Member feature.\n\n### Fixed\n\n- Earlier fix.\n\n"),
        );

        // When: a fresh composition, with no earlier release, unions the two
        // members
        let contents = merge_of([base.as_slice()], [added.as_slice(), fixed.as_slice()]);
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the block written last has no `### Added` heading, so that
        // list is its own block, in term order, as before
        assert_bytes(
            &union.content,
            &unreleased("### Added\n\n- Member feature.\n\n### Fixed\n\n- Member fix.\n\n"),
        );
        assert_eq!(union.first_lines.len(), 2);
    }

    #[test]
    fn a_fresh_composition_writes_once_a_heading_that_two_sides_put_lists_under() {
        // Given: an `[Unreleased]` section that two members each gave a
        // `### Fixed` list, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let alpha = unreleased("### Fixed\n\n- Alpha's fix.\n\n");
        let beta = unreleased("### Fixed\n\n- Beta's first fix.\n- Beta's second fix.\n\n");
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);
        let joined = unreleased(
            "### Fixed\n\n- Alpha's fix.\n- Beta's first fix.\n- Beta's second fix.\n\n",
        );

        // When: a fresh composition, with no earlier release, unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the heading is written once, in the block written last, with
        // the entries in the order the union writes the blocks, each line as
        // its side wrote it, byte for byte
        assert_bytes(&union.content, &joined);
        assert_eq!(union.first_lines.len(), 1);

        // And: the content check reads each side as carried by that union
        assert!(masks(&union.content, &base, &alpha, &options));
        assert!(masks(&union.content, &base, &beta, &options));

        // When: the earlier release's side has been cancelled, so no side
        // holds its content
        let gone = unreleased("### Fixed\n\n- Gone.\n\n");
        let union =
            union_of(&contents, Some(gone.as_slice()), &options).expect("both sides only inserted");

        // Then: the lists join the block written last there too
        assert_bytes(&union.content, &joined);
    }

    #[test]
    fn a_heading_that_differs_by_case_or_a_trailing_space_is_a_list_of_its_own() {
        // Given: a member that gave an `[Unreleased]` section a `### Fixed`
        // list, and members that gave it a list under `### fixed`, or under
        // `### Fixed ` with a trailing space, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let fixed = "### Fixed\n\n- A fix.\n\n";
        for other in [
            "### fixed\n\n- Another fix.\n\n",
            "### Fixed \n\n- Another fix.\n\n",
        ] {
            let (first, last) = (unreleased(other), unreleased(fixed));
            let contents = merge_of([base.as_slice()], [first.as_slice(), last.as_slice()]);

            // When: a fresh composition unions the two
            let union = union_of(&contents, None, &options).expect("both sides only inserted");

            // Then: the two headings are two lists, each in its own block, in
            // term order, each line as its side wrote it
            assert_bytes(&union.content, &unreleased(&format!("{other}{fixed}")));
            assert_eq!(union.first_lines.len(), 2, "{other:?}");
        }
    }

    #[test]
    fn a_target_holding_a_members_block_masks_its_conflict() {
        // Given: the release whose union holds sami11's section verbatim, the
        // series base, and the member itself
        let options = merge_options();

        // When: the member's change is replayed onto that release
        // Then: the conflict is masked — the target carries the block
        assert!(masks(UNION_TWO_LOG, BASE_LOG, SAMI11_LOG, &options));

        // When: the target lost one line of that block
        let short = without_line(UNION_TWO_LOG, b"  is a signed delta:");
        // Then: it no longer carries what the member wrote
        assert!(!masks(&short, BASE_LOG, SAMI11_LOG, &options));

        // When: the member rewrote a line of the base instead of inserting
        // Then: nothing is masked, whatever the target holds
        assert!(!masks(
            b"# log\n\n## new\n\n## old\n",
            b"# log\n\n## old\n",
            b"# log\n\n## renamed\n",
            &options
        ));
    }

    #[test]
    fn a_target_that_ran_a_block_into_the_next_does_not_mask_it() {
        // Given: two members that each appended a block with no final
        // newline, and a target that wrote both with the last line of one run
        // into the first line of the other
        let options = merge_options();
        let base: &[u8] = b"# Changelog\n";
        let member_a: &[u8] = b"# Changelog\n## a\n- a";
        let member_b: &[u8] = b"# Changelog\n## b\n- b";
        let run_together: &[u8] = b"# Changelog\n## b\n- b## a\n- a";

        // When: either member's change is replayed onto that target
        // Then: neither is masked, because neither block is a whole run of
        // the target's lines
        assert!(!masks(run_together, base, member_a, &options));
        assert!(!masks(run_together, base, member_b, &options));

        // When: the target ends with one member's block, as a union writes it
        // Then: that member is masked
        assert!(masks(
            b"# Changelog\n## b\n- b\n## a\n- a",
            base,
            member_a,
            &options
        ));
    }

    #[test]
    fn only_the_two_release_logs_are_eligible() {
        // Given: paths a tree diff hands the caller
        // When/Then: the two names are eligible in any directory, and nothing
        // else is, whatever it is named after
        assert!(eligible("CHANGELOG.md"));
        assert!(eligible("docs/RELEASE_NOTES.md"));
        assert!(!eligible("CHANGELOG.md.bak"));
        assert!(!eligible("changelog.md"));
        assert!(!eligible("src/changelog_union.rs"));
    }

    #[test]
    fn a_merge_that_resolves_is_not_a_union() {
        // Given: two sides that made the same insertion, which jj resolves
        let options = merge_options();
        let contents = merge_of([b"a\n"], [b"x\na\n", b"x\na\n"]);

        // When: the write asks for a union
        let union = union_of(&contents, None, &options);

        // Then: there is none, because there is no conflict to resolve
        assert_eq!(union, None);
    }

    /// The content without the first line that starts with `needle`, which is
    /// how a test builds a target that lost a line of a member's block.
    fn without_line(content: &[u8], needle: &[u8]) -> Vec<u8> {
        let mut kept: Vec<u8> = Vec::new();
        let mut dropped = false;
        for line in content.split_inclusive(|byte| *byte == b'\n') {
            if !dropped && line.starts_with(needle) {
                dropped = true;
                continue;
            }
            kept.extend_from_slice(line);
        }
        assert!(dropped, "the fixture no longer holds {needle:?}");
        kept
    }
}
