//! Spec R11's placement: what one version of a path lacks of a member's
//! change there, each line at its place in that version ([`place`]), and
//! the lines of a change that spec R11 compares for an advanced member
//! ([`change_lines`]).
//!
//! The placement runs R9's reading ([`super::MemberLines::read_path`])
//! against the version's text, and keeps the place of each lost entry as
//! the reading finds it ([`super::Out`]).

use std::ops::Range;

use super::{
    BINARY_FILE, ContentChange, EMPTY_FILE, Lost, LostKind, Out, SkippedRange, Value, bit_text,
    entry, file_change, is_binary, is_blank, line_hunks, made_of, meet, number, shown,
};

/// Where a lost entry lies in the version it was read against, as spec
/// R11 places it ([`place`]).
pub(super) enum Place {
    /// The whole path: a path read whole, or its executable bit.
    Path,
    /// The version's lines that the survival diff puts in the entry's
    /// place, each a half-open range of 0-based line indices, read as the
    /// gap interval it covers ([`meet`]); an empty range is the gap where a
    /// deletion leaves nothing.
    Lines(Vec<Range<usize>>),
}

/// One version of a path, as spec R11 places a member's lines in it: the
/// release in hand, an edit's candidate, or the plain merge of the edit's
/// new parents ([`place`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    /// What the version holds at the path. A conflict that jj writes as
    /// text is that text, in jj's snapshot style, as a file; any other
    /// conflict is [`Value::Conflict`].
    pub value: Value,
    /// The conflict regions of that text ([`conflict_regions`]).
    pub regions: Vec<Range<usize>>,
    /// Whether the version holds an unresolved conflict at the path.
    pub conflicted: bool,
}

/// A line, or a path, of a member's change that a version lacks, placed in
/// that version (spec R11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedLost {
    pub lost: Lost,
    /// Whether it is at a conflict there: its place meets a conflict region
    /// of the version, the region's edge included, or, for a path read
    /// whole or a mode, the version holds a conflict at the path.
    pub at_conflict: bool,
}

/// What spec R11 reads of a member's change at one path in one version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placed {
    /// Each line, or path, of the change that the version lacks, placed,
    /// and each range the word step did not follow.
    Lines {
        lost: Vec<PlacedLost>,
        word_step_skipped: Vec<SkippedRange>,
    },
    /// Nothing gives a place: the version is not text, or the member's tip
    /// holds a conflict at the path. Every line of the change there is at a
    /// conflict.
    AtConflict,
}

/// Spec R11's placement: what `version` lacks of a member's change at
/// `path`, from `fork` to `tip`, each line at its place there.
///
/// The survival diff is R9's whole rule
/// ([`super::MemberLines::read_path`]), its word step and R8 included, run
/// against the version's text. A conflicted release log can hold
/// `## [Unreleased]` more than once, and then R8 holds no line. A line that
/// the survival diff deletes or replaces is placed at the version's lines
/// of that hunk. A removed line that the version puts back is placed at the
/// version's line that holds it again.
/// Either is at a conflict where its place meets a conflict region, the
/// region's edge included. A path read whole, and a mode, are at a conflict
/// where the version holds a conflict at the path. A version that does not
/// hold the path reads as empty text. A version that is not text (a
/// symlink, a submodule, a file whose first 8000 bytes hold a NUL byte, a
/// conflict jj does not write as text), and a tip that holds a conflict at
/// the path, give no place. A line the version holds is not listed: it is
/// held, wherever it is. What R9 reports does not change.
pub fn place(path: &str, (fork, tip): (&Value, &Value), version: &Version) -> Placed {
    let text = match &version.value {
        Value::Absent => true,
        Value::File { content, .. } => !is_binary(content),
        Value::Symlink { .. } | Value::Submodule { .. } | Value::Conflict => false,
    };
    if !text || *tip == Value::Conflict {
        return Placed::AtConflict;
    }
    let (mut lost, mut held_by_words, mut word_step_skipped) = (Vec::new(), Vec::new(), Vec::new());
    let mut places = Vec::new();
    if *tip != version.value {
        let mut out = Out {
            lost: &mut lost,
            held_by_words: &mut held_by_words,
            word_step_skipped: &mut word_step_skipped,
            places: Some(&mut places),
        };
        out.read_change(path, (fork, tip, &version.value));
    }
    let lost = lost
        .into_iter()
        .zip(places)
        .map(|(lost, place)| {
            let at_conflict = match place {
                Place::Path => version.conflicted,
                Place::Lines(ranges) => ranges
                    .iter()
                    .any(|range| version.regions.iter().any(|region| meet(range, region))),
            };
            PlacedLost { lost, at_conflict }
        })
        .collect();
    Placed::Lines {
        lost,
        word_step_skipped,
    }
}

/// The conflict regions of `text`, a file that jj wrote in its snapshot
/// style with markers `marker_len` characters long.
///
/// Each region runs from the line that opens a conflict (`<` repeated)
/// through the line that closes it (`>` repeated), as a half-open range of
/// 0-based line indices. A marker line is read as jj reads one: a run of its
/// character at least `marker_len` long, then whitespace or the line's end.
/// A region left open runs to the end of the text.
pub fn conflict_regions(text: &[u8], marker_len: usize) -> Vec<Range<usize>> {
    let mut regions = Vec::new();
    let mut open = None;
    let mut lines = 0;
    for (index, line) in text.split_inclusive(|byte| *byte == b'\n').enumerate() {
        lines = index + 1;
        match (open, marker(line, marker_len)) {
            (None, Some(b'<')) => open = Some(index),
            (Some(start), Some(b'>')) => {
                regions.push(start..lines);
                open = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        regions.push(start..lines);
    }
    regions
}

/// The character of `line` where it is a conflict marker at least `len`
/// characters long ([`conflict_regions`]): `<` or `>`.
fn marker(line: &[u8], len: usize) -> Option<u8> {
    let first = *line.first().filter(|&&byte| matches!(byte, b'<' | b'>'))?;
    let run = line.iter().take_while(|&&byte| byte == first).count();
    (run >= len && line.get(run).is_none_or(u8::is_ascii_whitespace)).then_some(first)
}

/// Each line, or path, of a member's change at `path`, from `fork` to
/// `tip`.
///
/// Each is the lost entry it would be in a version that holds none of the
/// change: each non-blank added line, each non-blank removed line, a mode,
/// and a path read whole. A file change is
/// read as R9's rule reads it ([`file_change`]). A tip that holds a
/// conflict gives none. Spec R11 compares an advanced member's lines with
/// those of its old tip by these, by path, kind and text.
pub fn change_lines(path: &str, fork: &Value, tip: &Value) -> Vec<Lost> {
    let mut lines = Vec::new();
    if tip == fork {
        return lines;
    }
    let Value::File {
        executable,
        content,
    } = tip
    else {
        lines.extend(made_of(tip).map(|made| entry(path, LostKind::File, None, Some(made))));
        return lines;
    };
    let change = file_change(fork, (*executable, content));
    if change.bit_changed {
        lines.push(entry(
            path,
            LostKind::Mode,
            None,
            Some(bit_text(*executable)),
        ));
    }
    let fork_text = match change.content {
        ContentChange::Unchanged => return lines,
        ContentChange::Binary => {
            lines.push(entry(
                path,
                LostKind::File,
                None,
                Some(BINARY_FILE.to_owned()),
            ));
            return lines;
        }
        ContentChange::Empty => {
            lines.push(entry(
                path,
                LostKind::File,
                None,
                Some(EMPTY_FILE.to_owned()),
            ));
            return lines;
        }
        ContentChange::Text(fork_text) => fork_text,
    };
    let fork_lines: Vec<&[u8]> = fork_text.split_inclusive(|byte| *byte == b'\n').collect();
    let tip_lines: Vec<&[u8]> = content.split_inclusive(|byte| *byte == b'\n').collect();
    for hunk in line_hunks(fork_text, content) {
        for index in hunk.new.clone() {
            if let Some(&text) = tip_lines.get(index).filter(|text| !is_blank(text)) {
                let line = Some(number(index));
                lines.push(entry(path, LostKind::Added, line, Some(shown(text))));
            }
        }
        let removed = fork_lines.get(hunk.old.clone()).unwrap_or_default();
        for (offset, &text) in removed.iter().enumerate() {
            if !is_blank(text) {
                let line = Some(number(hunk.old.start + offset));
                lines.push(entry(path, LostKind::Removed, line, Some(shown(text))));
            }
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::tests::{file, file_with_bit};
    use super::*;

    /// `content` as a version of a path with no conflict, as spec R11 reads
    /// it.
    fn clean(content: &str) -> Version {
        Version {
            value: file(content),
            regions: Vec::new(),
            conflicted: false,
        }
    }

    /// The regions of a version that has one conflict region, the lines
    /// `start..end`.
    fn region(start: usize, end: usize) -> Vec<Range<usize>> {
        std::iter::once(start..end).collect()
    }

    /// A placed line: its kind, line and text, and whether it is at a
    /// conflict.
    type PlacedRow<'l> = (LostKind, Option<u32>, Option<&'l str>, bool);

    /// Each line a placement lists ([`PlacedRow`]); `None` where nothing
    /// gives a place.
    fn placed(reading: &Placed) -> Option<Vec<PlacedRow<'_>>> {
        match reading {
            Placed::AtConflict => None,
            Placed::Lines { lost, .. } => Some(
                lost.iter()
                    .map(|line| {
                        let lost = &line.lost;
                        (lost.kind, lost.line, lost.text.as_deref(), line.at_conflict)
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn a_deleted_line_is_at_a_conflict_where_its_gap_meets_a_region_or_its_edge() {
        // Given: a member that adds `x` after `a`, and a version that deletes
        // it, so its place is the gap between `a` and `b`
        let (fork, tip) = (file("a\nb\nc\nd\n"), file("a\nx\nb\nc\nd\n"));
        let version = |regions: Vec<Range<usize>>| Version {
            value: file("a\nb\nc\nd\n"),
            regions,
            conflicted: true,
        };
        let at = |regions| place("f.txt", (&fork, &tip), &version(regions));
        let row = |at_conflict| Some(vec![(LostKind::Added, Some(2), Some("x"), at_conflict)]);

        // Then: a region that starts or ends at that gap touches its edge, so
        // the line is at a conflict; a region away from it leaves it at none
        assert_eq!(placed(&at(region(1, 2))), row(true));
        assert_eq!(placed(&at(region(0, 1))), row(true));
        assert_eq!(placed(&at(region(2, 4))), row(false));
    }

    #[test]
    fn a_line_replaced_inside_a_snapshot_conflict_is_at_it_and_one_kept_inside_is_held() {
        // Given: a member that adds `x`, and a version whose conflict, written
        // in jj's snapshot style, stands where `x` stood
        let (fork, tip) = (file("a\nb\nc\n"), file("a\nx\nb\nc\n"));
        let snapshot = |side: &str| {
            format!(
                "a\n<<<<<<< conflict 1 of 1\n+++++++ side #1\n{side}\n------- base\n+++++++ side \
                 #2\nz\n>>>>>>> conflict 1 of 1 ends\nb\nc\n"
            )
        };
        let version = |text: String| Version {
            regions: conflict_regions(text.as_bytes(), 7),
            value: file(&text),
            conflicted: true,
        };
        let replaced = version(snapshot("y"));
        assert_eq!(replaced.regions, region(1, 8));

        // Then: the line the conflict replaces is lost at it
        assert_eq!(
            placed(&place("f.txt", (&fork, &tip), &replaced)),
            Some(vec![(LostKind::Added, Some(2), Some("x"), true)])
        );
        // And: a line the survival diff keeps inside the conflict is held
        assert_eq!(
            placed(&place("f.txt", (&fork, &tip), &version(snapshot("x")))),
            Some(vec![])
        );
    }

    #[test]
    fn a_removed_hunk_put_back_at_a_region_is_at_a_conflict() {
        // Given: a member that removes `r`, and a version that puts it back
        let (fork, tip) = (file("a\nr\nb\nc\nd\n"), file("a\nb\nc\nd\n"));
        let version = |regions: Vec<Range<usize>>| Version {
            value: file("a\nr\nb\nc\nd\n"),
            regions,
            conflicted: true,
        };
        let at = |regions| place("f.txt", (&fork, &tip), &version(regions));
        let row = |at_conflict| Some(vec![(LostKind::Removed, Some(2), Some("r"), at_conflict)]);

        // Then: placed at the line put back: at a conflict where a region
        // holds it, and at none where the region is away from it
        assert_eq!(placed(&at(region(1, 2))), row(true));
        assert_eq!(placed(&at(region(3, 5))), row(false));
    }

    #[test]
    fn a_version_that_is_not_text_or_a_conflicted_tip_gives_no_place() {
        let (fork, tip) = (file("a\n"), file("a\nx\n"));
        let version = |value| Version {
            value,
            regions: Vec::new(),
            conflicted: false,
        };

        // A symlink, a submodule, a binary file and a conflict that is not
        // text give no place
        for value in [
            Value::Symlink {
                target: "t".to_owned(),
            },
            Value::Submodule {
                commit: "c".to_owned(),
            },
            file_with_bit(false, b"a\0x\n"),
            Value::Conflict,
        ] {
            let read = place("f.txt", (&fork, &tip), &version(value.clone()));
            assert_eq!(read, Placed::AtConflict, "{value:?}");
        }
        // Nor does a tip that holds a conflict at the path
        let read = place("f.txt", (&fork, &Value::Conflict), &clean("a\n"));
        assert_eq!(read, Placed::AtConflict);
        // A version that lacks the path reads as empty text: the added line
        // is lost, at no conflict
        assert_eq!(
            placed(&place("f.txt", (&fork, &tip), &version(Value::Absent))),
            Some(vec![(LostKind::Added, Some(2), Some("x"), false)])
        );
    }

    #[test]
    fn a_path_read_whole_is_at_a_conflict_only_where_the_version_holds_one() {
        // Given: a member that deletes `f.txt`
        let (fork, tip) = (file("a\n"), Value::Absent);
        let conflicted = Version {
            value: file("a\n"),
            regions: region(0, 1),
            conflicted: true,
        };

        // Then: a version that still holds the file lacks the deletion, at a
        // conflict only where the version holds one there
        assert_eq!(
            placed(&place("f.txt", (&fork, &tip), &clean("a\n"))),
            Some(vec![(LostKind::File, None, Some("deleted"), false)])
        );
        assert_eq!(
            placed(&place("f.txt", (&fork, &tip), &conflicted)),
            Some(vec![(LostKind::File, None, Some("deleted"), true)])
        );
    }

    #[test]
    fn a_snapshot_with_two_unreleased_headings_holds_no_line_by_r8() {
        // Given: a member that adds `- a` at the top of `[Unreleased]`, and
        // versions that hold it further down that section
        let fork = file("# C\n## [Unreleased]\n- z1\n- z2\n- z3\n## [1.0]\n");
        let tip = file("# C\n## [Unreleased]\n- a\n- z1\n- z2\n- z3\n## [1.0]\n");
        let one = clean("# C\n## [Unreleased]\n- z1\n- z2\n- z3\n- a\n## [1.0]\n");
        let two = clean("# C\n## [Unreleased]\n- z1\n- z2\n- z3\n- a\n## [1.0]\n## [Unreleased]\n");

        // Then: with one `[Unreleased]` heading, R8 holds the moved line
        assert_eq!(
            placed(&place("CHANGELOG.md", (&fork, &tip), &one)),
            Some(vec![])
        );
        // And: with two, R8 holds no line, and it reads as lost
        assert_eq!(
            placed(&place("CHANGELOG.md", (&fork, &tip), &two)),
            Some(vec![(LostKind::Added, Some(3), Some("- a"), false)])
        );
    }

    #[test]
    fn conflict_regions_run_from_each_opening_marker_through_its_closing_one() {
        // Given: a snapshot with two conflicts, the second left open, and,
        // before the first, two lines that only look like markers: one too
        // short, one with no space after its run
        let text = b"a\n<<<<<< short\n<<<<<<<b\n<<<<<<< conflict 1 of 2\n>>>>>>> conflict 1 \
                     of 2 ends\nc\n<<<<<<< conflict 2 of 2\nd\n";

        // Then: neither look-alike opens a region; each region runs through
        // its closing marker, and the open one to the end of the text
        assert_eq!(conflict_regions(text, 7), [3..5, 6..8]);
        // And: with longer markers, these lines are text
        assert_eq!(conflict_regions(text, 8), Vec::<Range<usize>>::new());
    }

    #[test]
    fn change_lines_lists_each_line_and_path_of_a_change() {
        let rows = |lines: Vec<Lost>| -> Vec<(LostKind, Option<u32>, Option<String>)> {
            lines
                .into_iter()
                .map(|lost| (lost.kind, lost.line, lost.text))
                .collect()
        };
        let text = |value: &str| Some(value.to_owned());

        // A changed line, its removed line, and an added line; a blank line
        // is no line of the change
        assert_eq!(
            rows(change_lines(
                "f.txt",
                &file("a\nb\nc\n"),
                &file("a\nB\nc\nd\n\n")
            )),
            [
                (LostKind::Added, Some(2), text("B")),
                (LostKind::Removed, Some(2), text("b")),
                (LostKind::Added, Some(4), text("d")),
            ]
        );
        // A deletion and a mode are paths read whole
        assert_eq!(
            rows(change_lines("f.txt", &file("a\n"), &Value::Absent)),
            [(LostKind::File, None, text("deleted"))]
        );
        assert_eq!(
            rows(change_lines(
                "x.sh",
                &file_with_bit(false, b"a\n"),
                &file_with_bit(true, b"a\n")
            )),
            [(LostKind::Mode, None, text("executable yes"))]
        );
        // A tip that holds a conflict gives none
        assert_eq!(
            change_lines("f.txt", &file("a\n"), &Value::Conflict),
            Vec::<Lost>::new()
        );
    }
}
