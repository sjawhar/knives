//! A member's own lines, and whether a release holds them: the rule of spec
//! R9 and its Design, with R8's clause for release logs.
//!
//! A member's own change is the diff from its fork point with the upstream
//! trunk to its tip (spec R6). The rule follows each line of that change
//! from the tip to the release, as `git blame --reverse` follows a line
//! forward, instead of replaying the change as one commit. For each path
//! that the change touches, it takes the member's hunks, the line diff of
//! the fork point and the tip, in the tip's lines, and the survival diff,
//! the line diff of the tip and the release.
//!
//! - A non-blank line that the member added is lost where the survival diff
//!   deletes it, unless R8 holds it: the release's one `[Unreleased]`
//!   section of a release log holds the same line. Where the survival diff
//!   replaces a range of the tip's lines with a range of the release's
//!   lines, the line is followed one level down, by its words.
//! - The non-blank lines of a hunk that the member removed are put back
//!   where the survival diff inserts all of them, in order, at the hunk's
//!   place.
//! - A path that is not text, a deletion, an empty added file and the
//!   executable bit are each read by a rule of their own
//!   ([`MemberLines::read_path`]).
//!
//! Like [`crate::detect`] and [`crate::changelog_union`], the module is
//! pure: it takes the values that [`crate::jj`] read from three trees, and
//! returns the reading. It opens no repository.

mod difflib;
mod words;

use std::collections::{BTreeSet, HashSet};
use std::ops::Range;

use jj_lib::diff::{ContentDiff, DiffHunkKind};

use crate::changelog_union;
use crate::ids::CommitId;

use difflib::{SequenceMatcher, Tag};

/// The most words that the word step follows on either side of a replaced
/// range.
///
/// A range with more is not followed by words: the member's lines in it
/// count as lost ([`LostKind::WordLimit`]), and the reading names the range
/// ([`MemberLines::word_step_skipped`]). The limit bounds the time of one
/// word diff.
pub const WORD_LIMIT: usize = 30_000;

/// The text of a [`LostKind::Conflict`] entry where the member's tip holds
/// the unresolved conflict.
pub const CONFLICT_AT_TIP: &str = "at the member's tip";

/// The text of a [`LostKind::Conflict`] entry where the release holds the
/// unresolved conflict.
pub const CONFLICT_IN_RELEASE: &str = "in the release";

/// The most characters of a line that a reading names, as the prototype of
/// the rule showed a line; a longer line is cut there, with `…` after it.
const TEXT_CAP: usize = 300;

/// How many bytes of a file are read for a NUL byte, as git's
/// `buffer_is_binary` reads them: a file with one there is not text.
const BINARY_PROBE: usize = 8000;

/// What one tree holds at one path, as the rule reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// No file: nothing at the path, or a directory.
    Absent,
    /// A regular file, with its executable bit and its content.
    File { executable: bool, content: Vec<u8> },
    /// A symbolic link, by its target.
    Symlink { target: String },
    /// A submodule, by the commit it records.
    Submodule { commit: String },
    /// An unresolved conflict.
    Conflict,
}

/// One path of a member's change, with what the member's fork point, its tip
/// and the release hold there.
///
/// The member's change is read with no rename detection, so a rename is two
/// paths: the old one, which the tip does not hold, and the new one, which
/// the fork point does not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathValues {
    /// The repository-internal path, with `/` between its components.
    pub path: String,
    pub fork: Value,
    pub tip: Value,
    pub release: Value,
}

/// One member's own lines against one release: what the release lacks of
/// them, and the lines it holds only by their words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MemberLines {
    /// The member, as the report that reads it names it.
    pub label: String,
    /// The member's tip.
    pub commit: CommitId,
    /// The member's fork point with the upstream trunk (spec R6).
    pub fork_point: CommitId,
    pub reading: Reading,
    /// Each of the member's own lines, or paths, that the release lacks, and
    /// each path that the rule cannot read.
    pub lost: Vec<Lost>,
    /// Each line that the release holds only by its words.
    pub held_by_words: Vec<HeldByWords>,
    /// Each replaced range that the word step did not follow.
    pub word_step_skipped: Vec<SkippedRange>,
}

/// Whether a release carries a member.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Reading {
    /// The release holds every one of the member's own lines.
    Carried,
    /// The release lacks a line, or a path, of the member's own change.
    Missing,
    /// A path of the member's change holds an unresolved conflict, in the
    /// release or at the member's tip, and the release lacks nothing else.
    Inconclusive,
    /// The release lacks only what the release before it lacked too
    /// ([`MemberLines::carry_forward`]).
    CarriedForward,
}

/// One line, or one path, of a member's own change that a release lacks, or
/// a path that the rule cannot read.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Lost {
    pub path: String,
    pub kind: LostKind,
    /// For an added line, its line number at the member's tip; for a hunk of
    /// removed lines, the fork point's line number of its first non-blank
    /// line. `None` for a path read whole.
    pub line: Option<u32>,
    /// For a line, its text. For a path read whole, what the member made of
    /// it (`deleted`, `a binary file`, `an empty file`, `a symlink to …`,
    /// `a submodule at …`); for a mode, `executable yes` or `executable no`;
    /// for a conflict, where it is (`in the release` or `at the member's
    /// tip`).
    pub text: Option<String>,
    /// Whether the release before this one lacks the same line too
    /// ([`MemberLines::carry_forward`]).
    pub carried_forward: bool,
}

/// Why a line or a path of a member's change counts as lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LostKind {
    /// A non-blank line that the member added, which the release does not
    /// hold.
    Added,
    /// The non-blank lines of a hunk that the member removed, which the
    /// release puts back at the hunk's place.
    Removed,
    /// A path read whole: not text, deleted, or an empty added file.
    File,
    /// The executable bit that the member changed.
    Mode,
    /// A line that the member added, in a replaced range of more than
    /// [`WORD_LIMIT`] words, which the word step did not follow.
    WordLimit,
    /// An unresolved conflict in the path, in the release or at the
    /// member's tip: the rule cannot read the path.
    Conflict,
}

/// A line of the member that the release holds only by its words: the
/// survival diff replaced it, and the word step holds it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct HeldByWords {
    pub path: String,
    /// Its line number at the member's tip.
    pub line: u32,
    pub text: String,
    /// The release's lines of the replaced range that hold its words.
    pub release_lines: Vec<ReleaseLine>,
}

/// A line of the release.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReleaseLine {
    pub line: u32,
    pub text: String,
}

/// A replaced range that the word step did not follow, because one of its
/// sides holds more than [`WORD_LIMIT`] words.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SkippedRange {
    pub path: String,
    /// The range's first and last line at the member's tip.
    pub first_line: u32,
    pub last_line: u32,
    /// The words of the range at the member's tip and in the release.
    pub tip_words: usize,
    pub release_words: usize,
}

impl MemberLines {
    /// The reading of a member whose change touches no path yet: carried.
    pub const fn new(label: String, commit: CommitId, fork_point: CommitId) -> Self {
        Self {
            label,
            commit,
            fork_point,
            reading: Reading::Carried,
            lost: Vec::new(),
            held_by_words: Vec::new(),
            word_step_skipped: Vec::new(),
        }
    }

    /// Reads one path of the member's change, and the member's reading
    /// again.
    ///
    /// - A path that holds an unresolved conflict at the member's tip or in
    ///   the release cannot be read: it is lost as a conflict, and the
    ///   member reads inconclusive unless it lacks something else.
    /// - A path where the release holds what the tip holds loses nothing.
    /// - A path that the member deletes, and one that the tip holds as a
    ///   symlink or a submodule, is read whole: the release lacks it unless
    ///   it holds the tip's value there.
    /// - A file: where the member changes its executable bit, the release's
    ///   file must have the tip's bit. A file that is not text, whose first
    ///   8000 bytes hold a NUL byte, is read whole: where the member changed
    ///   its content, the release's file must hold the tip's. A text file
    ///   that the member adds with no non-blank line is carried where the
    ///   release holds a file at that path. Every other text file is read
    ///   line by line; a version that holds no file there reads as empty.
    pub fn read_path(&mut self, values: &PathValues) {
        let path = values.path.as_str();
        if values.tip == Value::Conflict {
            let at = CONFLICT_AT_TIP.to_owned();
            self.lost
                .push(entry(path, LostKind::Conflict, None, Some(at)));
        } else if values.release == Value::Conflict {
            let at = CONFLICT_IN_RELEASE.to_owned();
            self.lost
                .push(entry(path, LostKind::Conflict, None, Some(at)));
        } else if values.tip != values.release {
            let mut out = Out {
                lines: self,
                places: None,
            };
            out.read_change(path, (&values.fork, &values.tip, &values.release));
        }
        self.reading = self.settled();
    }

    /// Carries forward, line by line, what `previous`, the reading of the
    /// same member, from the same fork point and tip, against the release
    /// before this one, lacks too: a lost entry is carried forward where
    /// `previous` lost one of the same path, kind and line. A member whose
    /// every lost line is carried forward reads carried forward, or
    /// inconclusive where a path cannot be read; one with any other lost
    /// line still reads missing.
    pub fn carry_forward(&mut self, previous: &Self) {
        for lost in &mut self.lost {
            lost.carried_forward = lost.kind != LostKind::Conflict
                && previous.lost.iter().any(|before| {
                    before.kind == lost.kind && before.path == lost.path && before.line == lost.line
                });
        }
        self.reading = self.settled();
    }

    /// Whether every path the member cannot be read at holds its unresolved
    /// conflict at the member's tip, and none in the release. `false` where
    /// no path holds a conflict.
    #[must_use]
    pub fn conflicted_only_at_tip(&self) -> bool {
        let mut conflicts = self
            .lost
            .iter()
            .filter(|lost| lost.kind == LostKind::Conflict)
            .peekable();
        conflicts.peek().is_some()
            && conflicts.all(|lost| lost.text.as_deref() == Some(CONFLICT_AT_TIP))
    }

    /// The reading that the lost entries give.
    fn settled(&self) -> Reading {
        let mut lines = self
            .lost
            .iter()
            .filter(|lost| lost.kind != LostKind::Conflict)
            .peekable();
        let unread = self.lost.iter().any(|lost| lost.kind == LostKind::Conflict);
        if lines.peek().is_none() {
            return if unread {
                Reading::Inconclusive
            } else {
                Reading::Carried
            };
        }
        if lines.all(|lost| lost.carried_forward) {
            if unread {
                Reading::Inconclusive
            } else {
                Reading::CarriedForward
            }
        } else {
            Reading::Missing
        }
    }
}

/// Where a reading puts what it finds: the member's reading, and, for spec
/// R11's placement, the place of each lost entry, in the order of the
/// entries ([`place`]).
struct Out<'o> {
    lines: &'o mut MemberLines,
    places: Option<&'o mut Vec<Place>>,
}

impl Out<'_> {
    /// Pushes `lost`, and its place where places are kept.
    fn lost(&mut self, lost: Lost, place: impl FnOnce() -> Place) {
        self.lines.lost.push(lost);
        if let Some(places) = self.places.as_deref_mut() {
            places.push(place());
        }
    }

    /// Reads a path where the release holds another value than the tip,
    /// and neither holds a conflict.
    fn read_change(&mut self, path: &str, (fork, tip, release): (&Value, &Value, &Value)) {
        if let Value::File {
            executable,
            content,
        } = tip
        {
            self.read_file((path, fork, release), (*executable, content));
        } else if let Some(made) = made_of(tip) {
            self.lost(entry(path, LostKind::File, None, Some(made)), || {
                Place::Path
            });
        }
    }

    /// Reads a path that the tip holds as a file, with its executable bit
    /// and content: the bit, then the content.
    fn read_file(
        &mut self,
        (path, fork, release): (&str, &Value, &Value),
        (executable, content): (bool, &[u8]),
    ) {
        let fork = file_of(fork);
        let release = file_of(release);
        if fork.is_some_and(|(bit, _)| bit != executable)
            && release.is_none_or(|(bit, _)| bit != executable)
        {
            let bit = bit_text(executable);
            self.lost(entry(path, LostKind::Mode, None, Some(bit)), || Place::Path);
        }
        if fork.is_some_and(|(_, before)| before == content) {
            // The member changed the bit alone.
            return;
        }
        if is_binary(content) {
            if release.is_none_or(|(_, held)| held != content) {
                let made = "a binary file".to_owned();
                self.lost(entry(path, LostKind::File, None, Some(made)), || {
                    Place::Path
                });
            }
            return;
        }
        if fork.is_none() && is_blank(content) {
            if release.is_none() {
                let made = "an empty file".to_owned();
                self.lost(entry(path, LostKind::File, None, Some(made)), || {
                    Place::Path
                });
            }
            return;
        }
        let empty: &[u8] = &[];
        let text = TextPath::new(
            path,
            fork.map_or(empty, |(_, before)| before),
            content,
            release.map_or(empty, |(_, held)| held),
        );
        text.read(self);
    }
}

/// Where a lost entry lies in the version it was read against, as spec
/// R11 places it ([`place`]).
enum Place {
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
/// The survival diff is R9's whole rule ([`MemberLines::read_path`]), its
/// word step and R8 included, run against the version's text. A conflicted
/// release log can hold `## [Unreleased]` more than once, and then R8 holds
/// no line. A line that the survival diff deletes or replaces is placed at
/// the version's lines of that hunk. A hunk of removed lines that the
/// version puts back is placed at the version's lines of the survival
/// hunks at the hunk's place. Either is at a conflict where its place meets
/// a conflict region, the region's edge included. A path read whole, and a
/// mode, are at a conflict where the version holds a conflict at the path.
/// A version that does not hold the path reads as empty text. A version
/// that is not text (a symlink, a submodule, a file whose first 8000 bytes
/// hold a NUL byte, a conflict jj does not write as text), and a tip that
/// holds a conflict at the path, give no place. A line the version holds is
/// not listed: it is held, wherever it is. What R9 reports does not change.
pub fn place(path: &str, (fork, tip): (&Value, &Value), version: &Version) -> Placed {
    let text = match &version.value {
        Value::Absent => true,
        Value::File { content, .. } => !is_binary(content),
        Value::Symlink { .. } | Value::Submodule { .. } | Value::Conflict => false,
    };
    if !text || *tip == Value::Conflict {
        return Placed::AtConflict;
    }
    let mut lines = MemberLines::new(String::new(), CommitId::new(""), CommitId::new(""));
    let mut places = Vec::new();
    if *tip != version.value {
        let mut out = Out {
            lines: &mut lines,
            places: Some(&mut places),
        };
        out.read_change(path, (fork, tip, &version.value));
    }
    let lost = lines
        .lost
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
        word_step_skipped: lines.word_step_skipped,
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
    for (index, line) in text.split_inclusive(|byte| *byte == b'\n').enumerate() {
        match (open, marker(line, marker_len)) {
            (None, Some(b'<')) => open = Some(index),
            (Some(start), Some(b'>')) => {
                regions.push(start..index + 1);
                open = None;
            }
            _ => {}
        }
    }
    if let Some(start) = open {
        regions.push(start..text.split_inclusive(|byte| *byte == b'\n').count());
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
/// change: each non-blank added line, each hunk of removed lines by its
/// first non-blank line, a mode, and a path read whole. A tip that holds a
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
    let before = file_of(fork);
    if before.is_some_and(|(bit, _)| bit != *executable) {
        lines.push(entry(
            path,
            LostKind::Mode,
            None,
            Some(bit_text(*executable)),
        ));
    }
    if before.is_some_and(|(_, text)| text == content.as_slice()) {
        return lines;
    }
    let made = if is_binary(content) {
        Some("a binary file")
    } else if before.is_none() && is_blank(content) {
        Some("an empty file")
    } else {
        None
    };
    if let Some(made) = made {
        lines.push(entry(path, LostKind::File, None, Some(made.to_owned())));
        return lines;
    }
    let empty: &[u8] = &[];
    let fork_text = before.map_or(empty, |(_, text)| text);
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
        if let Some((offset, text)) = removed.iter().enumerate().find(|(_, line)| !is_blank(line)) {
            let first = Some(number(hunk.old.start + offset));
            lines.push(entry(path, LostKind::Removed, first, Some(shown(text))));
        }
    }
    lines
}

/// What a member made of a path it does not leave a file: `deleted`, `a
/// symlink to …` or `a submodule at …`. `None` for a file or a conflict.
fn made_of(tip: &Value) -> Option<String> {
    match tip {
        Value::Absent => Some("deleted".to_owned()),
        Value::Symlink { target } => Some(format!("a symlink to {target}")),
        Value::Submodule { commit } => Some(format!("a submodule at {commit}")),
        Value::File { .. } | Value::Conflict => None,
    }
}

/// How a reading names an executable bit that the member set.
fn bit_text(executable: bool) -> String {
    if executable {
        "executable yes"
    } else {
        "executable no"
    }
    .to_owned()
}

/// A lost entry that no release before has been read for.
fn entry(path: &str, kind: LostKind, line: Option<u32>, text: Option<String>) -> Lost {
    Lost {
        path: path.to_owned(),
        kind,
        line,
        text,
        carried_forward: false,
    }
}

/// A file's executable bit and content; `None` for a value that is no file.
const fn file_of(value: &Value) -> Option<(bool, &[u8])> {
    match value {
        Value::File {
            executable,
            content,
        } => Some((*executable, content.as_slice())),
        _ => None,
    }
}

/// Whether `content` is not text, as git's `buffer_is_binary` reads it: its
/// first 8000 bytes hold a NUL byte.
fn is_binary(content: &[u8]) -> bool {
    content.iter().take(BINARY_PROBE).any(|&byte| byte == 0)
}

/// Whether `text` holds no byte but ASCII whitespace, as Python's
/// `bytes.strip` reads it, which is how the prototype of the rule told a
/// blank line: space, tab, line feed, carriage return, vertical tab and form
/// feed.
fn is_blank(text: &[u8]) -> bool {
    text.iter()
        .all(|&byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c'))
}

/// `line` without its line feed: a line as two lines compare.
fn bare(line: &[u8]) -> &[u8] {
    line.strip_suffix(b"\n").unwrap_or(line)
}

/// `line` as a reading names it: without its line feed, each sequence that
/// is not UTF-8 replaced, and cut at [`TEXT_CAP`] characters.
fn shown(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(bare(line));
    if let Some(head) = text
        .char_indices()
        .nth(TEXT_CAP)
        .and_then(|(cut, _)| text.get(..cut))
    {
        return format!("{head}…");
    }
    text.into_owned()
}

/// The 1-based number of the line at `index`.
fn number(index: usize) -> u32 {
    u32::try_from(index + 1).unwrap_or(u32::MAX)
}

/// Whether `needles` are all in `haystack`, in order, with other items
/// between them or not.
fn is_subsequence<T: PartialEq>(needles: &[T], haystack: &[T]) -> bool {
    let mut rest = haystack.iter();
    needles.iter().all(|needle| rest.any(|item| item == needle))
}

/// One hunk of a line diff: the lines it changes on each side, by 0-based
/// index, half open. Either side can be empty, at the gap where the other
/// side's lines go.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Hunk {
    old: Range<usize>,
    new: Range<usize>,
}

/// The hunks of the line diff of `old` and `new`, as jj diffs lines
/// ([`ContentDiff::by_line`]): lines compare byte for byte, each with its
/// line feed.
fn line_hunks(old: &[u8], new: &[u8]) -> Vec<Hunk> {
    let (old_starts, new_starts) = (line_starts(old), line_starts(new));
    ContentDiff::by_line([old, new])
        .hunk_ranges()
        .filter(|range| range.kind == DiffHunkKind::Different)
        .filter_map(|range| {
            let (old_bytes, new_bytes) = (range.ranges.first()?, range.ranges.get(1)?);
            Some(Hunk {
                old: lines_in(&old_starts, old_bytes),
                new: lines_in(&new_starts, new_bytes),
            })
        })
        .collect()
}

/// The byte offset where each line of `text` starts.
fn line_starts(text: &[u8]) -> Vec<usize> {
    text.split_inclusive(|byte| *byte == b'\n')
        .scan(0, |at, line| {
            let start = *at;
            *at += line.len();
            Some(start)
        })
        .collect()
}

/// The lines that `bytes`, a range of a line diff, spans. Its ends are line
/// starts, or the text's end.
fn lines_in(starts: &[usize], bytes: &Range<usize>) -> Range<usize> {
    let line = |offset: usize| starts.partition_point(|&start| start < offset);
    line(bytes.start)..line(bytes.end)
}

/// Whether two gap intervals meet. A run of lines `[start, end)` covers the
/// closed interval of gaps `[start, end]`, where gap `g` lies between lines
/// `g - 1` and `g`, so an empty run at `g` covers `[g, g]`; two runs whose
/// intervals share a gap meet, at an edge too.
const fn meet(one: &Range<usize>, other: &Range<usize>) -> bool {
    one.start <= other.end && other.start <= one.end
}

/// One text path, as the line rule reads it.
struct TextPath<'t> {
    path: &'t str,
    /// The lines of each version, each with its line feed.
    fork: Vec<&'t [u8]>,
    tip: Vec<&'t [u8]>,
    release: Vec<&'t [u8]>,
    /// The member's hunks: the line diff of the fork point and the tip.
    member: Vec<Hunk>,
    /// The survival diff: the line diff of the tip and the release.
    survival: Vec<Hunk>,
    /// For each line of the tip, the survival hunk that deletes or replaces
    /// it.
    gone: Vec<Option<usize>>,
    /// R8: where the path is a release log and the release's file has one
    /// `[Unreleased]` section, that section's lines, trailing whitespace
    /// cut ([`changelog_union::unreleased_lines`]).
    unreleased: Option<HashSet<&'t [u8]>>,
}

/// What the rule reads of one line that the member added and the survival
/// diff deletes or replaces.
enum Judged {
    /// The line is lost.
    Lost(LostKind),
    /// The line is held only by its words: the indices of the release's
    /// lines that hold them.
    HeldByWords(Vec<usize>),
}

/// A line that the member added and the survival diff deletes or replaces.
#[derive(Clone, Copy)]
struct Added<'t> {
    /// Its index at the tip.
    index: usize,
    text: &'t [u8],
    /// The line that it replaced, where the pairing pairs it with one.
    fork: Option<&'t [u8]>,
}

impl<'t> TextPath<'t> {
    fn new(path: &'t str, fork: &'t [u8], tip: &'t [u8], release: &'t [u8]) -> Self {
        let survival = line_hunks(tip, release);
        let tip_lines: Vec<&[u8]> = tip.split_inclusive(|byte| *byte == b'\n').collect();
        let mut gone = vec![None; tip_lines.len()];
        for (at, hunk) in survival.iter().enumerate() {
            for index in hunk.old.clone() {
                if let Some(slot) = gone.get_mut(index) {
                    *slot = Some(at);
                }
            }
        }
        Self {
            path,
            fork: fork.split_inclusive(|byte| *byte == b'\n').collect(),
            tip: tip_lines,
            release: release.split_inclusive(|byte| *byte == b'\n').collect(),
            member: line_hunks(fork, tip),
            survival,
            gone,
            unreleased: changelog_union::eligible(path)
                .then(|| changelog_union::unreleased_lines(release))
                .flatten(),
        }
    }

    /// Reads every hunk of the member: its added lines, then its removed
    /// lines, then the ranges that the word step did not follow.
    fn read(&self, out: &mut Out<'_>) {
        let mut steps: Vec<Option<WordStep<'t>>> = std::iter::repeat_with(|| None)
            .take(self.survival.len())
            .collect();
        for hunk in &self.member {
            self.read_added(hunk, &mut steps, out);
        }
        for hunk in &self.member {
            self.read_removed(hunk, out);
        }
        for (hunk, step) in self.survival.iter().zip(&steps) {
            if let Some(WordStep::Skipped {
                tip_words,
                release_words,
            }) = step
            {
                out.lines.word_step_skipped.push(SkippedRange {
                    path: self.path.to_owned(),
                    first_line: number(hunk.old.start),
                    // The 1-based number of the range's last line is the
                    // index one past it.
                    last_line: u32::try_from(hunk.old.end).unwrap_or(u32::MAX),
                    tip_words: *tip_words,
                    release_words: *release_words,
                });
            }
        }
    }

    /// Reads the non-blank lines that `hunk` adds: each that the survival
    /// diff keeps is held; each that R8 holds is held; each other is judged
    /// ([`Self::judge`]).
    fn read_added(&self, hunk: &Hunk, steps: &mut [Option<WordStep<'t>>], out: &mut Out<'_>) {
        let removed = self.fork.get(hunk.old.clone()).unwrap_or_default();
        let added = self.tip.get(hunk.new.clone()).unwrap_or_default();
        let mut pairs: Option<Vec<Option<usize>>> = None;
        for (offset, &text) in added.iter().enumerate() {
            let index = hunk.new.start + offset;
            let Some(survival) = self.gone.get(index).copied().flatten() else {
                continue;
            };
            if is_blank(text) || self.held_by_r8(text) {
                continue;
            }
            let pairs = pairs.get_or_insert_with(|| words::pair_hunk(removed, added));
            let fork = pairs
                .get(offset)
                .copied()
                .flatten()
                .and_then(|at| removed.get(at))
                .copied();
            let line = Added { index, text, fork };
            match self.judge(line, survival, steps) {
                Judged::Lost(kind) => out.lost(
                    entry(self.path, kind, Some(number(index)), Some(shown(text))),
                    // The place of a line the survival diff deletes or
                    // replaces: the release's lines of that hunk.
                    || {
                        Place::Lines(
                            self.survival
                                .get(survival)
                                .map(|hunk| vec![hunk.new.clone()])
                                .unwrap_or_default(),
                        )
                    },
                ),
                Judged::HeldByWords(holding) => out.lines.held_by_words.push(HeldByWords {
                    path: self.path.to_owned(),
                    line: number(index),
                    text: shown(text),
                    release_lines: holding
                        .into_iter()
                        .map(|at| ReleaseLine {
                            line: number(at),
                            text: shown(self.release.get(at).copied().unwrap_or_default()),
                        })
                        .collect(),
                }),
            }
        }
    }

    /// R8: whether `line` is a line of the release's one `[Unreleased]`
    /// section, trailing whitespace aside, in a release log.
    fn held_by_r8(&self, line: &[u8]) -> bool {
        self.unreleased
            .as_ref()
            .is_some_and(|held| held.contains(line.trim_ascii_end()))
    }

    /// Judges `line`, which the survival hunk at `survival` deletes or
    /// replaces. A deletion loses it. In a replaced range, the word diff of
    /// the range decides ([`WordDiff::loses`]), unless the range holds more
    /// than [`WORD_LIMIT`] words on either side.
    fn judge(
        &self,
        line: Added<'t>,
        survival: usize,
        steps: &mut [Option<WordStep<'t>>],
    ) -> Judged {
        let Some(hunk) = self.survival.get(survival) else {
            return Judged::Lost(LostKind::Added);
        };
        if hunk.new.is_empty() {
            return Judged::Lost(LostKind::Added);
        }
        let Some(slot) = steps.get_mut(survival) else {
            return Judged::Lost(LostKind::Added);
        };
        let step = slot.get_or_insert_with(|| {
            WordStep::of(
                self.tip.get(hunk.old.clone()).unwrap_or_default(),
                self.release.get(hunk.new.clone()).unwrap_or_default(),
            )
        });
        let WordStep::Followed(diff) = step else {
            return Judged::Lost(LostKind::WordLimit);
        };
        let offset = line.index - hunk.old.start;
        if diff.loses(offset, line) {
            return Judged::Lost(LostKind::Added);
        }
        let holding = diff.holding_lines(offset);
        Judged::HeldByWords(holding.into_iter().map(|at| hunk.new.start + at).collect())
    }

    /// Reads the non-blank lines that `hunk` removes: they are put back
    /// where the survival hunks at the hunk's place insert all of them, in
    /// order, with other lines between them or not. The hunk's place is the
    /// gaps between tip lines where the member removed them; a survival hunk
    /// is at that place where its tip lines' gaps meet those gaps
    /// ([`meet`]).
    fn read_removed(&self, hunk: &Hunk, out: &mut Out<'_>) {
        let removed: Vec<(usize, &[u8])> = self
            .fork
            .get(hunk.old.clone())
            .unwrap_or_default()
            .iter()
            .copied()
            .enumerate()
            .filter(|&(_, line)| !is_blank(line))
            .map(|(offset, line)| (hunk.old.start + offset, bare(line)))
            .collect();
        let Some(&(first, text)) = removed.first() else {
            return;
        };
        let at_place: Vec<&Hunk> = self
            .survival
            .iter()
            .filter(|survival| meet(&survival.old, &hunk.new))
            .collect();
        let inserted: Vec<&[u8]> = at_place
            .iter()
            .flat_map(|survival| self.release.get(survival.new.clone()).unwrap_or_default())
            .copied()
            .map(bare)
            .collect();
        let lines: Vec<&[u8]> = removed.iter().map(|&(_, line)| line).collect();
        if is_subsequence(&lines, &inserted) {
            out.lost(
                entry(
                    self.path,
                    LostKind::Removed,
                    Some(number(first)),
                    Some(shown(text)),
                ),
                // The place of the hunk: the release's lines of the
                // survival hunks there.
                || {
                    Place::Lines(
                        at_place
                            .iter()
                            .map(|survival| survival.new.clone())
                            .collect(),
                    )
                },
            );
        }
    }
}

/// The word step of one replaced range: its word diff, or the sizes that
/// put it over [`WORD_LIMIT`].
enum WordStep<'t> {
    Followed(WordDiff<'t>),
    Skipped {
        tip_words: usize,
        release_words: usize,
    },
}

/// The word survival diff of one replaced range: the words of the tip's
/// lines in the range against the words of the release's lines.
struct WordDiff<'t> {
    /// The words of each tip line of the range, as a range of the tip's
    /// words.
    spans: Vec<Range<usize>>,
    /// Whether the diff deletes or replaces each of the tip's words.
    lost: Vec<bool>,
    /// The release's word that each of the tip's words matches, in an
    /// `equal` run.
    to_release: Vec<Option<usize>>,
    /// The release's words, and the line of the range that holds each.
    release_words: Vec<&'t [u8]>,
    release_line: Vec<usize>,
    /// Each insert and replace: the tip's words it covers, and the release's
    /// words it inserts.
    inserts: Vec<(Range<usize>, Range<usize>)>,
}

impl<'t> WordStep<'t> {
    /// The word step of a range that replaces `tip`'s lines with
    /// `release`'s.
    fn of(tip: &[&'t [u8]], release: &[&'t [u8]]) -> Self {
        let mut tip_words = Vec::new();
        let mut spans = Vec::new();
        for &line in tip {
            let start = tip_words.len();
            tip_words.extend(words::tokens(line));
            spans.push(start..tip_words.len());
        }
        let mut release_words = Vec::new();
        let mut release_line = Vec::new();
        for (offset, &line) in release.iter().enumerate() {
            let line_words = words::tokens(line);
            release_line.extend(std::iter::repeat_n(offset, line_words.len()));
            release_words.extend(line_words);
        }
        if tip_words.len() > WORD_LIMIT || release_words.len() > WORD_LIMIT {
            return Self::Skipped {
                tip_words: tip_words.len(),
                release_words: release_words.len(),
            };
        }
        let mut lost = vec![false; tip_words.len()];
        let mut to_release = vec![None; tip_words.len()];
        let mut inserts = Vec::new();
        for opcode in SequenceMatcher::new(&tip_words, &release_words).opcodes() {
            if matches!(opcode.tag, Tag::Delete | Tag::Replace) {
                for slot in lost.get_mut(opcode.a.clone()).unwrap_or_default() {
                    *slot = true;
                }
            }
            if matches!(opcode.tag, Tag::Insert | Tag::Replace) {
                inserts.push((opcode.a.clone(), opcode.b.clone()));
            }
            if opcode.tag == Tag::Equal {
                for (at, matched) in opcode.a.zip(opcode.b) {
                    if let Some(slot) = to_release.get_mut(at) {
                        *slot = Some(matched);
                    }
                }
            }
        }
        Self::Followed(WordDiff {
            spans,
            lost,
            to_release,
            release_words,
            release_line,
            inserts,
        })
    }
}

impl<'t> WordDiff<'t> {
    /// The words of the tip line at `offset` in the range, as a range of
    /// the tip's words.
    fn span(&self, offset: usize) -> Range<usize> {
        self.spans.get(offset).cloned().unwrap_or_default()
    }

    /// Spec R9's three word tests, and its whitespace rule, for `line`, the
    /// tip line at `offset` in the range. The line is lost where the diff
    /// deletes or replaces one of the words that the member added to it;
    /// where it inserts, at the line, all of the words that the member
    /// removed from it, in order; where it keeps none of the line's words;
    /// or where the member changed the line in whitespace alone. The words
    /// that the member added are every word of a line that it added whole,
    /// and of a line that it changed, those that the word diff of the line
    /// it replaced and the new line inserts or replaces.
    fn loses(&self, offset: usize, line: Added<'t>) -> bool {
        let span = self.span(offset);
        let line_words = words::tokens(line.text);
        let (added, removed) = line.fork.map_or_else(
            || ((0..line_words.len()).collect(), Vec::new()),
            |fork| {
                let own = words::line_word_diff(&words::tokens(fork), &line_words);
                (own.added, own.removed)
            },
        );
        let whitespace_only = line
            .fork
            .is_some_and(|fork| bare(fork) != bare(line.text) && words::tokens(fork) == line_words);
        let added_lost = added
            .iter()
            .any(|&at| self.lost.get(span.start + at) == Some(&true));
        let put_back = !removed.is_empty() && is_subsequence(&removed, &self.inserted_at(&span));
        added_lost || whitespace_only || put_back || self.kept(&span) == 0
    }

    /// The release's words that the diff inserts at a tip line whose words
    /// are `span`: each insert and replace whose tip words' gaps meet the
    /// line's ([`meet`]), inside the line or at either edge of it.
    fn inserted_at(&self, span: &Range<usize>) -> Vec<&'t [u8]> {
        self.inserts
            .iter()
            .filter(|(tip, _)| meet(tip, span))
            .flat_map(|(_, release)| self.release_words.get(release.clone()).unwrap_or_default())
            .copied()
            .collect()
    }

    /// How many of the words `span` the diff keeps, in `equal` runs.
    fn kept(&self, span: &Range<usize>) -> usize {
        self.to_release
            .get(span.clone())
            .unwrap_or_default()
            .iter()
            .flatten()
            .count()
    }

    /// The lines of the release's range, by offset, that hold, through
    /// `equal` runs, words of the tip line at `offset`.
    fn holding_lines(&self, offset: usize) -> BTreeSet<usize> {
        self.to_release
            .get(self.span(offset))
            .unwrap_or_default()
            .iter()
            .flatten()
            .filter_map(|&at| self.release_line.get(at).copied())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a reading in a test is the assertion; a panic is the failure"
    )]

    use super::*;

    /// A file that is not executable, holding `content`.
    fn file(content: &str) -> Value {
        Value::File {
            executable: false,
            content: content.as_bytes().to_vec(),
        }
    }

    /// A file with the executable bit `executable`, holding `content`.
    fn file_with_bit(executable: bool, content: &[u8]) -> Value {
        Value::File {
            executable,
            content: content.to_vec(),
        }
    }

    /// A member's reading with nothing read yet.
    fn member() -> MemberLines {
        MemberLines::new(
            "feat/m".to_owned(),
            CommitId::new("tip"),
            CommitId::new("fork"),
        )
    }

    /// The reading of a member whose change touches only `path`, which its
    /// fork point, its tip and the release hold as given.
    fn read(path: &str, fork: Value, tip: Value, release: Value) -> MemberLines {
        let mut lines = member();
        lines.read_path(&PathValues {
            path: path.to_owned(),
            fork,
            tip,
            release,
        });
        lines
    }

    /// Each lost entry, as its kind, line and text.
    fn lost(lines: &MemberLines) -> Vec<(LostKind, Option<u32>, Option<&str>)> {
        lines
            .lost
            .iter()
            .map(|lost| (lost.kind, lost.line, lost.text.as_deref()))
            .collect()
    }

    /// A line held only by its words, as its line and text and the
    /// release's lines that hold its words.
    type WordHeld<'l> = (u32, &'l str, Vec<(u32, &'l str)>);

    /// Each line held only by its words ([`WordHeld`]).
    fn held(lines: &MemberLines) -> Vec<WordHeld<'_>> {
        lines
            .held_by_words
            .iter()
            .map(|held| {
                let release = held
                    .release_lines
                    .iter()
                    .map(|line| (line.line, line.text.as_str()))
                    .collect();
                (held.line, held.text.as_str(), release)
            })
            .collect()
    }

    #[test]
    fn an_added_line_the_release_keeps_is_held() {
        // Given: a member that adds `new`, and a release that keeps it and
        // adds a line of its own
        let lines = read(
            "f.txt",
            file("a\nb\n"),
            file("a\nnew\nb\n"),
            file("a\nnew\nb\nother\n"),
        );

        // Then: the member is carried, with nothing to name
        assert_eq!(lines.reading, Reading::Carried);
        assert_eq!(lost(&lines), []);
        assert_eq!(held(&lines), []);
    }

    #[test]
    fn an_added_line_the_release_deletes_is_lost_and_named() {
        // Given: a release that deletes the line the member added
        let lines = read("f.txt", file("a\nb\n"), file("a\nnew\nb\n"), file("a\nb\n"));

        // Then: the line is lost, by its tip line number and text
        assert_eq!(lines.reading, Reading::Missing);
        assert_eq!(lost(&lines), [(LostKind::Added, Some(2), Some("new"))]);
    }

    #[test]
    fn an_added_line_another_change_wrapped_is_held_by_its_words() {
        // Given: a member that adds a call, and a release that holds the
        // call wrapped in another (K11 (3))
        let lines = read(
            "lib.js",
            file("import x\nvalue = 1\nlog()\n"),
            file("import x\nvalue = 1\nrun(value, 2);\nlog()\n"),
            file("import x\nvalue = 1\nheld(run(value, 2));\nlog()\n"),
        );

        // Then: the member is carried, and the line is named as held only
        // by its words, with the release's line that holds them
        assert_eq!(lines.reading, Reading::Carried);
        assert_eq!(
            lines.held_by_words,
            [HeldByWords {
                path: "lib.js".to_owned(),
                line: 3,
                text: "run(value, 2);".to_owned(),
                release_lines: vec![ReleaseLine {
                    line: 3,
                    text: "held(run(value, 2));".to_owned(),
                }],
            }]
        );
    }

    #[test]
    fn a_removed_hunk_the_release_puts_back_whole_is_lost() {
        // Given: a member that removes `x` and `y`, and a release that
        // holds both again where they were (K11 (8))
        let lines = read(
            "f.txt",
            file("a\nx\ny\nb\n"),
            file("a\nb\n"),
            file("a\nx\ny\nb\n"),
        );

        // Then: the hunk is lost, by the fork point's line number of its
        // first line
        assert_eq!(lines.reading, Reading::Missing);
        assert_eq!(lost(&lines), [(LostKind::Removed, Some(2), Some("x"))]);
    }

    #[test]
    fn a_removed_hunk_the_release_puts_back_in_part_is_carried() {
        // Given: a member that removes `x` and `y`, and a release that
        // holds `x` again
        let part = read(
            "f.txt",
            file("a\nx\ny\nb\n"),
            file("a\nb\n"),
            file("a\nx\nb\n"),
        );
        // And: a member that removes a call and a lone `}`, and a release
        // that holds the `}` again (K11 (9))
        let brace = read(
            "f.txt",
            file("a\n  y();\n}\nb\n"),
            file("a\nb\n"),
            file("a\n}\nb\n"),
        );

        // Then: neither hunk is put back whole, so both members are carried
        for lines in [part, brace] {
            assert_eq!(lines.reading, Reading::Carried);
            assert_eq!(lost(&lines), []);
        }
    }

    #[test]
    fn a_removed_line_counts_as_put_back_only_at_the_hunks_place() {
        // Given: a member that removes the first line, and a release that
        // holds it again two lines further down
        let elsewhere = read(
            "f.txt",
            file("x\na\nb\nc\n"),
            file("a\nb\nc\n"),
            file("a\nb\nx\nc\n"),
        );
        // Then: it is not put back at the hunk's place, so it is carried
        assert_eq!(elsewhere.reading, Reading::Carried);

        // Given: a member that removes `x` between `a` and `b`, and a
        // release that replaces `a` with two lines, the second of them `x`
        let edge = read(
            "f.txt",
            file("a\nx\nb\n"),
            file("a\nb\n"),
            file("a2\nx\nb\n"),
        );
        // Then: the replaced range's gaps meet the hunk's at its edge, so
        // `x` is put back
        assert_eq!(lost(&edge), [(LostKind::Removed, Some(2), Some("x"))]);
    }

    /// A release log whose `[Unreleased]` section holds one `### Fixed`
    /// list: the fork point's, and the member's, which adds `- Member.`
    /// above the base entry.
    const LOG_FORK: &str = "## [Unreleased]\n\n### Fixed\n\n- Base.\n\n## [1.0.0]\n\n- Old.\n";
    const LOG_TIP: &str =
        "## [Unreleased]\n\n### Fixed\n\n- Member.\n- Base.\n\n## [1.0.0]\n\n- Old.\n";
    /// The release's log: another member's entry where the member wrote
    /// its own, and the member's entry joined below the base entry, with
    /// trailing spaces.
    const LOG_JOINED: &str = "## [Unreleased]\n\n### Fixed\n\n- Other.\n- Base.\n- Member.  \n\n\
                              ## [1.0.0]\n\n- Old.\n";

    #[test]
    fn a_release_log_entry_the_unreleased_section_holds_is_held_wherever_it_moved() {
        // Given: the member's entry, which the line diff loses at its
        // place, and which the release's one `[Unreleased]` section holds
        // further down (K9 (1))
        for name in ["CHANGELOG.md", "docs/RELEASE_NOTES.md"] {
            let lines = read(name, file(LOG_FORK), file(LOG_TIP), file(LOG_JOINED));

            // Then: R8 holds it
            assert_eq!(lines.reading, Reading::Carried, "{name}");
            assert_eq!(lost(&lines), [], "{name}");
        }

        // Given: the same lines in a file that is no release log (K9 (3))
        let notes = read("NOTES.md", file(LOG_FORK), file(LOG_TIP), file(LOG_JOINED));
        // Then: the entry is lost
        assert_eq!(
            lost(&notes),
            [(LostKind::Added, Some(5), Some("- Member."))]
        );
    }

    #[test]
    fn a_release_log_entry_is_not_held_by_two_unreleased_sections_or_a_released_one() {
        // Given: a release log with a second `## [Unreleased]` heading line
        let two = format!("{LOG_JOINED}\n## [Unreleased]\n");
        // And: a release log that holds the member's entry only in a
        // released section (K9 (2))
        let released = "## [Unreleased]\n\n### Fixed\n\n- Other.\n- Base.\n\n## [1.0.0]\n\n\
                        - Member.\n- Old.\n";

        for release in [two.as_str(), released] {
            // When: the member's entry is read against it
            let lines = read("CHANGELOG.md", file(LOG_FORK), file(LOG_TIP), file(release));

            // Then: R8 does not hold it
            assert_eq!(lines.reading, Reading::Missing, "{release}");
            assert_eq!(
                lost(&lines),
                [(LostKind::Added, Some(5), Some("- Member."))],
                "{release}"
            );
        }
    }

    #[test]
    fn a_conflict_in_the_members_path_makes_it_inconclusive() {
        // Given: a release that holds a conflict in the member's path
        // (K11 (11)), and a member whose tip holds one (K11 (18))
        let in_release = read("f.txt", file("a\n"), file("b\n"), Value::Conflict);
        let at_tip = read("f.txt", file("a\n"), Value::Conflict, file("b\n"));

        // Then: each reads inconclusive, and names the path and where the
        // conflict is
        assert_eq!(in_release.reading, Reading::Inconclusive);
        assert_eq!(
            lost(&in_release),
            [(LostKind::Conflict, None, Some("in the release"))]
        );
        assert_eq!(at_tip.reading, Reading::Inconclusive);
        assert_eq!(
            lost(&at_tip),
            [(LostKind::Conflict, None, Some("at the member's tip"))]
        );

        // Given: a member with such a path and a lost line in another path
        let mut both = member();
        both.read_path(&PathValues {
            path: "f.txt".to_owned(),
            fork: file("a\n"),
            tip: file("b\n"),
            release: Value::Conflict,
        });
        both.read_path(&PathValues {
            path: "g.txt".to_owned(),
            fork: file("a\n"),
            tip: file("a\nnew\n"),
            release: file("a\n"),
        });
        // Then: the lost line makes it missing
        assert_eq!(both.reading, Reading::Missing);
    }

    #[test]
    fn a_path_that_is_not_text_is_carried_where_the_release_holds_the_tips_value() {
        // Given: a binary file, a symlink and a submodule that the release
        // holds as the tip holds them
        let binary = read("a.bin", file("\0old"), file("\0new"), file("\0new"));
        let link = Value::Symlink {
            target: "a.txt".to_owned(),
        };
        let symlink = read("link", Value::Absent, link.clone(), link.clone());
        let sub = Value::Submodule {
            commit: "abc123".to_owned(),
        };
        let submodule = read("sub", Value::Absent, sub.clone(), sub.clone());
        // Then: each is carried
        for lines in [binary, symlink, submodule] {
            assert_eq!(lines.reading, Reading::Carried);
        }

        // Given: the release holds another version of each
        let binary = read("a.bin", file("\0old"), file("\0new"), file("\0other"));
        let other = Value::Symlink {
            target: "b.txt".to_owned(),
        };
        let symlink = read("link", Value::Absent, link, other);
        let submodule = read("sub", Value::Absent, sub, Value::Absent);
        // Then: each is lost as a file, with what the member made it
        assert_eq!(
            lost(&binary),
            [(LostKind::File, None, Some("a binary file"))]
        );
        assert_eq!(
            lost(&symlink),
            [(LostKind::File, None, Some("a symlink to a.txt"))]
        );
        assert_eq!(
            lost(&submodule),
            [(LostKind::File, None, Some("a submodule at abc123"))]
        );
    }

    #[test]
    fn a_deleted_file_is_carried_where_the_release_does_not_hold_it() {
        // Given: a member that deletes a file
        let gone = read("f.txt", file("x\n"), Value::Absent, Value::Absent);
        let kept = read("f.txt", file("x\n"), Value::Absent, file("x\n"));

        // Then: a release without it carries the member, and one that still
        // holds it does not
        assert_eq!(gone.reading, Reading::Carried);
        assert_eq!(lost(&kept), [(LostKind::File, None, Some("deleted"))]);
    }

    #[test]
    fn an_executable_bit_is_carried_where_the_release_has_the_tips_bit() {
        // Given: a member that sets the bit of a text file, and a release
        // that lacks it (K11 (10))
        let lacking = read(
            "run.sh",
            file_with_bit(false, b"x\n"),
            file_with_bit(true, b"x\n"),
            file_with_bit(false, b"x\n"),
        );
        // Then: the bit is lost, and named
        assert_eq!(lacking.reading, Reading::Missing);
        assert_eq!(
            lost(&lacking),
            [(LostKind::Mode, None, Some("executable yes"))]
        );

        // Given: a release that has the bit, and another text, which is not
        // the member's change
        let kept = read(
            "run.sh",
            file_with_bit(false, b"x\n"),
            file_with_bit(true, b"x\n"),
            file_with_bit(true, b"y\n"),
        );
        // And: a binary file whose bit the member clears, and a release
        // that cleared it too
        let binary = read(
            "tool.bin",
            file_with_bit(true, b"\0tool"),
            file_with_bit(false, b"\0tool"),
            file_with_bit(false, b"\0another tool"),
        );
        // Then: both are carried
        assert_eq!(kept.reading, Reading::Carried);
        assert_eq!(binary.reading, Reading::Carried);
    }

    #[test]
    fn an_added_file_with_no_non_blank_line_is_carried_where_the_release_holds_a_file() {
        // Given: a member that adds an empty file and one of blank lines
        for content in ["", "\n  \n"] {
            // When: the release holds a file there, or none
            let held = read("e.txt", Value::Absent, file(content), file("other\n"));
            let absent = read("e.txt", Value::Absent, file(content), Value::Absent);

            // Then: the first carries it, and the second does not
            assert_eq!(held.reading, Reading::Carried, "{content:?}");
            assert_eq!(
                lost(&absent),
                [(LostKind::File, None, Some("an empty file"))],
                "{content:?}"
            );
        }
    }

    #[test]
    fn a_range_of_more_than_30000_words_is_not_followed_by_words() {
        // Given: a member that changes line 2, and a release that replaces
        // it with a line holding its words and 30001 more (K11 (17))
        let fork = file("keep\nkey = 1\nend\n");
        let tip = file("keep\nkey = 2\nend\n");
        let over = format!("keep\nkey = 2{}\nend\n", " w".repeat(30_001));
        let lines = read("big.txt", fork.clone(), tip.clone(), file(&over));

        // Then: the line is lost, and the range is named as not followed
        assert_eq!(lines.reading, Reading::Missing);
        assert_eq!(
            lost(&lines),
            [(LostKind::WordLimit, Some(2), Some("key = 2"))]
        );
        assert_eq!(
            lines.word_step_skipped,
            [SkippedRange {
                path: "big.txt".to_owned(),
                first_line: 2,
                last_line: 2,
                tip_words: 3,
                release_words: 30_004,
            }]
        );

        // Given: the same with exactly 30000 words in the release's range
        let limit = format!("keep\nkey = 2{}\nend\n", " w".repeat(29_997));
        let lines = read("big.txt", fork, tip, file(&limit));
        // Then: the word step follows it, and holds the line by its words;
        // the release's line is named cut at 300 characters
        assert_eq!(lines.reading, Reading::Carried);
        assert_eq!(lines.word_step_skipped, []);
        let word_held = held(&lines);
        assert_eq!(word_held.len(), 1);
        let (line, text, release) = &word_held[0];
        assert_eq!((*line, *text), (2, "key = 2"));
        let (release_line, release_text) = release[0];
        assert_eq!(release_line, 2);
        assert_eq!(release_text.chars().count(), TEXT_CAP + 1);
        assert!(release_text.starts_with("key = 2 w w"), "{release_text}");
        assert!(release_text.ends_with('…'), "{release_text}");
    }

    #[test]
    fn a_removed_argument_put_back_beside_another_is_lost() {
        // Given: M changes `call(a, insecure=True)` to `call(a)`, and the
        // release holds `call(a, insecure=True, timeout=5)` (K11 (12))
        let lines = read(
            "lib.py",
            file("call(a, insecure=True)\n"),
            file("call(a)\n"),
            file("call(a, insecure=True, timeout=5)\n"),
        );

        // Then: the release inserts every word M removed at the line, so
        // the line is lost
        assert_eq!(lost(&lines), [(LostKind::Added, Some(1), Some("call(a)"))]);
    }

    #[test]
    fn a_whitespace_change_is_held_only_byte_for_byte() {
        // Given: M re-indents four lines; the release keeps their old
        // indentation, and N changed one of them (K11 (13))
        let fork = file("  a = 1\n  b = 2\n  c = 3\n  d = 4\n");
        let tip = file("    a = 1\n    b = 2\n    c = 3\n    d = 4\n");
        let lines = read(
            "lib.py",
            fork.clone(),
            tip.clone(),
            file("  a = 1\n  b = 2\n  c = 30\n  d = 4\n"),
        );
        // Then: each of the four lines is lost
        assert_eq!(
            lost(&lines),
            [
                (LostKind::Added, Some(1), Some("    a = 1")),
                (LostKind::Added, Some(2), Some("    b = 2")),
                (LostKind::Added, Some(3), Some("    c = 3")),
                (LostKind::Added, Some(4), Some("    d = 4")),
            ]
        );

        // Given: a release that keeps the lines byte for byte
        let kept = read(
            "lib.py",
            fork,
            tip,
            file("    a = 1\n    b = 2\n    c = 3\n    d = 4\nmore\n"),
        );
        // Then: they are held
        assert_eq!(kept.reading, Reading::Carried);
    }

    #[test]
    fn a_line_the_release_replaced_whole_keeps_none_of_its_words_and_is_lost() {
        // Given: M changes `call(a, b)` to `call(a)`, and the release holds
        // N's `pass` in its place (K11 (14))
        let lines = read(
            "lib.py",
            file("call(a, b)\n"),
            file("call(a)\n"),
            file("pass\n"),
        );

        // Then: the line is lost
        assert_eq!(lost(&lines), [(LostKind::Added, Some(1), Some("call(a)"))]);
    }

    #[test]
    fn the_fifth_runs_synthetic_checks_read_as_the_prototype_read_them() {
        // Given: an added `}` whose `}` the release keeps in `});`
        let brace = read(
            "f.js",
            file("a\nb\n"),
            file("a\n}\nb\n"),
            file("a\n});\nb\n"),
        );
        // Then: it is held by its words
        assert_eq!(held(&brace), [(2, "}", vec![(2, "});")])]);

        // Given: `x = f(a)` changed to `x = f(a);`, and the release holds
        // `y = g(b);`
        let semicolon = read(
            "f.js",
            file("x = f(a)\n"),
            file("x = f(a);\n"),
            file("y = g(b);\n"),
        );
        // Then: the member's `;` is kept, so it is held by its words
        assert_eq!(held(&semicolon), [(1, "x = f(a);", vec![(1, "y = g(b);")])]);

        // Given: `alpha beta gamma` changed to `alpha beta`, and the release
        // holds `delta epsilon`
        let none_kept = read(
            "f.txt",
            file("alpha beta gamma\n"),
            file("alpha beta\n"),
            file("delta epsilon\n"),
        );
        // Then: no word of it is kept, so it is lost
        assert_eq!(
            lost(&none_kept),
            [(LostKind::Added, Some(1), Some("alpha beta"))]
        );

        // Given: an added line whose words the release keeps and extends
        let extended = read(
            "f.js",
            file("a\nb\n"),
            file("a\nfoo(bar)\nb\n"),
            file("a\nfoo(bar, baz)\nb\n"),
        );
        // Then: it is held by its words
        assert_eq!(
            held(&extended),
            [(2, "foo(bar)", vec![(2, "foo(bar, baz)")])]
        );
        for lines in [brace, semicolon, extended] {
            assert_eq!(lines.reading, Reading::Carried);
        }
    }

    #[test]
    fn a_lost_line_the_release_before_lacked_too_is_carried_forward() {
        // Given: a member that adds two lines; the candidate lacks both,
        // and the previous release lacks the first
        let (fork, tip) = (file("a\n"), file("a\nl1\nl2\n"));
        let candidate = read("f.txt", fork.clone(), tip.clone(), file("a\n"));
        let previous = read("f.txt", fork.clone(), tip.clone(), file("a\nl2\n"));

        // When: the candidate carries forward what the previous lacked
        let mut one_more = candidate.clone();
        one_more.carry_forward(&previous);
        // Then: only the first line is carried forward, so it is missing
        let forward: Vec<bool> = one_more
            .lost
            .iter()
            .map(|lost| lost.carried_forward)
            .collect();
        assert_eq!(forward, [true, false]);
        assert_eq!(one_more.reading, Reading::Missing);

        // Given: a previous release that lacks both lines
        let both = read("f.txt", fork, tip, file("a\n"));
        // When/Then: the member reads carried forward
        let mut same = candidate;
        same.carry_forward(&both);
        assert_eq!(same.reading, Reading::CarriedForward);
    }

    #[test]
    fn the_reading_serializes_with_snake_case_fields_and_kebab_case_values() {
        // Given: a reading carried forward, with a line lost by the word
        // limit
        let over = format!("keep\nkey = 2{}\nend\n", " w".repeat(30_001));
        let (fork, tip) = (file("keep\nkey = 1\nend\n"), file("keep\nkey = 2\nend\n"));
        let mut lines = read("big.txt", fork.clone(), tip.clone(), file(&over));
        let previous = read("big.txt", fork, tip, file(&over));
        lines.carry_forward(&previous);

        // When: it is written as JSON
        let json = serde_json::to_value(&lines).expect("the reading serializes");

        // Then: its fields and values read as the report's
        assert_eq!(json["label"], "feat/m");
        assert_eq!(json["commit"], "tip");
        assert_eq!(json["fork_point"], "fork");
        assert_eq!(json["reading"], "carried-forward");
        assert_eq!(
            json["lost"],
            serde_json::json!([{
                "path": "big.txt",
                "kind": "word-limit",
                "line": 2,
                "text": "key = 2",
                "carried_forward": true,
            }])
        );
        assert_eq!(json["held_by_words"], serde_json::json!([]));
        assert_eq!(
            json["word_step_skipped"],
            serde_json::json!([{
                "path": "big.txt",
                "first_line": 2,
                "last_line": 2,
                "tip_words": 3,
                "release_words": 30_004,
            }])
        );
    }

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
        // Given: a snapshot with two conflicts, the second left open, and two
        // lines that only look like markers: one too short, one with no
        // space after its run
        let text = b"a\n<<<<<<< conflict 1 of 2\n<<<<<< short\n<<<<<<<b\n>>>>>>> conflict 1 of \
                     2 ends\nc\n<<<<<<< conflict 2 of 2\nd\n";

        // Then: each region runs through its closing marker, and the open one
        // to the end of the text
        assert_eq!(conflict_regions(text, 7), [1..5, 6..8]);
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
