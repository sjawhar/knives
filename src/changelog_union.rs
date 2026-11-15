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
//! union is also verbatim — knives never renumbers — so two blocks claiming
//! one version heading survive into it, and [`Union::first_lines`] names them
//! so a reader sees that at once and settles it with `knives release adopt
//! --why`.
//!
//! Everything here is bytes in, bytes out: no repository, no file, no
//! `String`. A changelog carries CRLF line endings and, on an old project,
//! bytes that are not UTF-8, and a release write must hand back what it was
//! given.

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
    /// The file's new content, the sides' text verbatim.
    pub content: Vec<u8>,
    /// The first line of each block the union wrote, in the order written,
    /// without its line terminator. Two equal entries are two blocks claiming
    /// one heading, which is the one thing a verbatim union cannot settle and
    /// the reason a reader is given this list at all.
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
/// is where a person writing the resolution by hand puts them. It is found by
/// value and not by index because [`Merge::simplify`] cancels terms, so no
/// index names it reliably; where it has been cancelled no side matches and
/// every side keeps the order jj gave it, which is also what a fresh
/// composition with no earlier release gets.
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
    let order = side_order(contents, earlier_release);
    let mut union = Union {
        content: Vec::new(),
        first_lines: Vec::new(),
    };
    for hunk in &hunks {
        if let Some(resolved) = hunk.as_resolved() {
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
        let mut written: Vec<&[u8]> = Vec::new();
        for &side in &order {
            // A hunk carries the sides of the file it was cut from, so this
            // holds; `?` says so without a branch that cannot be reached.
            let inserted = hunk.get_add(side)?.as_slice();
            if inserted.is_empty() || written.contains(&inserted) {
                continue;
            }
            written.push(inserted);
            union.first_lines.push(first_line(inserted).to_vec());
            union.content.extend_from_slice(inserted);
        }
    }
    Some(union)
}

/// Whether `target` already holds what `member` added to `base`.
///
/// A carriage check replays a member's change onto a target and reads what is
/// left over. A member whose block a release's union holds verbatim has
/// nothing left over, but the replay still conflicts, because the union's
/// other blocks sit where the member's block ends. This says what such a
/// replay means: a conflict every side of which only inserted, and whose
/// member side the target holds as a whole run of its own lines, is the
/// target carrying that block — the conflict is masked. A member whose lines
/// the target lost is not carried, and says so.
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
                if !is_line_run(target, member_side.as_slice()) {
                    return false;
                }
                replayed.extend_from_slice(target_side);
            }
            replayed == target
        }
    }
}

/// The side indices in the order the union writes them: jj's term order, with
/// the side holding the earlier release's content last.
fn side_order<T: AsRef<[u8]>>(contents: &Merge<T>, earlier_release: Option<&[u8]>) -> Vec<usize> {
    let last = earlier_release
        .and_then(|published| contents.adds().position(|side| side.as_ref() == published));
    let mut order: Vec<usize> = (0..contents.num_sides())
        .filter(|side| Some(*side) != last)
        .collect();
    order.extend(last);
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
