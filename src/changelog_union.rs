//! The changelog resolution a release write writes itself.
//!
//! A release composition conflicts in `CHANGELOG.md` and `RELEASE_NOTES.md`
//! for a reason that is not a disagreement: every member adds its own section
//! at the top of the file, so the sides insert different text in the same
//! place and none of them touches what another wrote. The resolution is the
//! same every time — keep what every side inserted — so a person who writes
//! it by hand writes nothing a machine could not have written. This module
//! writes it, and leaves to a person the conflicts where a side changed what
//! another side's text says.
//!
//! The rule is deliberately narrow. A union exists only where every side's
//! change at the conflicting place is an insertion: the moment one side
//! rewrote or dropped lines another side kept, the file is a real
//! disagreement and no concatenation of the sides is its resolution. The
//! union never renumbers or rewrites a line, so two blocks claiming one
//! version heading survive into it, and [`Union::first_lines`] names them so
//! a reader sees that at once and settles it with `knives release adopt
//! --why`.
//!
//! At each conflicting place the union writes the earlier release's block
//! last where that release inserted its own text there ([`side_order`]), and
//! the other blocks before it in jj's term order. It writes each block
//! whole, byte for byte, as knives 4.0.1 did, except at the one shape a
//! reader is sure of: in a file with one `## [Unreleased]` heading line, the
//! place is inside that section ([`Above`]), and every block written there
//! is plain `### ` lists ([`plain_lists`]). There, for each `### ` heading,
//! such as `### Fixed` ([`joined_blocks`]), the lists of the last block that
//! has the heading stay as that block wrote them, and every other block's
//! lists of that heading join its first one, above that list's own entries. A heading is the same only where its text
//! is the same: one that differs by case or by a trailing space is another
//! heading. Entry lines are kept byte for byte, and a joined list's heading
//! and its blank lines are written once. The check that a member is carried
//! ([`masks`]) reads a joined entry by the same rule, so a join knives wrote
//! never reads as a member's entry lost. Where the place is not plain lists
//! in a file's one `[Unreleased]` section, each block is written whole, and
//! [`Union::left_twice`] names each `### ` heading that two of the blocks
//! there leave in one section, for a person to join. A heading that one
//! block written whole holds twice stays as that block wrote it, and that
//! line does not name it.
//!
//! After every release write and every `adopt` that records a tree, knives
//! reads each release log in the tree that the command writes or records,
//! changed by it or not.
//! It skips a file that is still conflicted there, and a file with no single
//! `## [Unreleased]` heading. For each `### ` heading that the `[Unreleased]`
//! section holds more than once ([`repeated_headings`]), it prints one line
//! that names the file and the heading, once for each file and heading. It
//! names a heading that [`Union::left_twice`] names too. It changes no line
//! for it: a clean merge can leave such a heading, and a person joins it.
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
use std::cell::OnceCell;
use std::collections::HashMap;

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
    /// one heading, which the union never settles, and the reason a reader is
    /// given this list at all. A block whose every list joined another
    /// block's list ([`joined_blocks`]) is left with blank lines only, and is
    /// neither written nor named.
    pub first_lines: Vec<Vec<u8>>,
    /// Each `### ` heading line, without its line terminator, that a place
    /// the union wrote whole leaves in one section in more than one block
    /// ([`name_left_twice`]): once each, in the order the union met it.
    pub left_twice: Vec<Vec<u8>>,
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
/// composition with no earlier release gets. At each conflicting place the
/// blocks are written whole, or, in a file with one `## [Unreleased]`
/// heading line, where the place is inside that section and every block
/// there is plain `### ` lists, their lists are joined ([`place_blocks`]).
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
    let (union, joined) = written_union(&hunks, &order, true)?;
    // The text above each place it joined held one `## [Unreleased]`
    // heading line, and a join takes none away. Where the file it wrote
    // holds another below, the check that a member is carried reads no
    // joined entry ([`unreleased_runs`]), so the union is written again
    // with every block whole: a join knives wrote never reads as lost.
    if joined && unreleased_runs(&union.content).is_none() {
        return written_union(&hunks, &order, false).map(|(union, _)| union);
    }
    Some(union)
}

/// The union of `hunks`, a file's merge hunks, with the sides of each
/// conflicting place in `order` ([`side_order`]), and whether a place joined
/// lists. A place joins its lists only where `join` holds
/// ([`place_blocks`]). `None` where the conflict is not one a union
/// resolves ([`union_of`]).
fn written_union<B: AsRef<[u8]>>(
    hunks: &[Merge<B>],
    order: &[usize],
    join: bool,
) -> Option<(Union, bool)> {
    let mut union = Union {
        content: Vec::new(),
        first_lines: Vec::new(),
        left_twice: Vec::new(),
    };
    let mut joined = false;
    // What the text written so far says about the next conflicting place.
    let mut above = Above::default();
    // Whether the bytes written last are an inserted block whose last line
    // has no newline. Nothing may follow such a block: its last line and the
    // next one would run together into a line no side wrote, and the union
    // adds no byte to keep them apart. So such a block is written only where
    // it ends the file, and any byte after it leaves the conflict to a person.
    let mut unterminated = false;
    for hunk in hunks {
        if let Some(resolved) = hunk.as_resolved() {
            if unterminated {
                return None;
            }
            union.content.extend_from_slice(resolved.as_ref());
            continue;
        }
        // A base region that is not empty is text one side kept and another
        // changed: a disagreement, and a person's.
        if hunk
            .removes()
            .any(|base_region| !base_region.as_ref().is_empty())
        {
            return None;
        }
        // Dropped per hunk, not per file: a side that inserted the same text
        // as another side contributes one block here, while a side that
        // inserted the same text somewhere else contributes one there too.
        let mut inserted: Vec<&[u8]> = Vec::new();
        for &side in order {
            // A hunk carries the sides of the file it was cut from, so this
            // holds; `?` says so without a branch that cannot be reached.
            let text = hunk.get_add(side)?.as_ref();
            if !text.is_empty() && !inserted.contains(&text) {
                inserted.push(text);
            }
        }
        let (blocks, joined_here) = place_blocks(&inserted, &mut above, &mut union, join);
        joined |= joined_here;
        for block in blocks {
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
    Some((union, joined))
}

/// The blocks that one conflicting place writes: `inserted`, each side's
/// text once, in the order the union writes them ([`union_of`]), below the
/// text that `union` holds so far, which `above` reads; and whether their
/// lists joined. Where `join` holds, the place is inside the one
/// `[Unreleased]` section of that text ([`Above::reads_unreleased`]), and
/// every block is plain `### ` lists ([`plain_lists`]), their lists are
/// joined ([`joined_blocks`]). Anywhere else each block is written whole,
/// byte for byte, and each `### ` heading that two of the blocks leave in one
/// section goes in the union's `left_twice` ([`name_left_twice`]).
fn place_blocks<'a>(
    inserted: &[&'a [u8]],
    above: &mut Above,
    union: &mut Union,
    join: bool,
) -> (Vec<Cow<'a, [u8]>>, bool) {
    if inserted.len() > 1 {
        let inside = above.reads_unreleased(&union.content);
        let plain: Option<Vec<PlainBlock<'a>>> = if join && inside {
            inserted.iter().copied().map(plain_lists).collect()
        } else {
            None
        };
        if let Some(blocks) = plain {
            return (joined_blocks(&blocks), true);
        }
        name_left_twice(inserted, above.fence, &mut union.left_twice);
    }
    (
        inserted.iter().map(|block| Cow::Borrowed(*block)).collect(),
        false,
    )
}

/// Add to `left_twice` each `### ` heading that `blocks`, written whole at
/// one place in this order, leave in one section in more than one block. A
/// section starts at each `#` or `##` heading and at each setext underline,
/// and a line in a code fence starts nothing; `fence` is the fence open at
/// the place. A heading already in `left_twice` is not added again.
fn name_left_twice(blocks: &[&[u8]], mut fence: Fence, left_twice: &mut Vec<Vec<u8>>) {
    let mut section = 0_usize;
    // The first block that put each heading in each section.
    let mut first: HashMap<(usize, &[u8]), usize> = HashMap::new();
    for (index, block) in blocks.iter().enumerate() {
        for line in block.split_inclusive(|byte| *byte == b'\n') {
            match fence.read(line) {
                Kind::Section => section += 1,
                Kind::List => {
                    let heading = first_line(line);
                    let met = *first.entry((section, heading)).or_insert(index);
                    if met != index && !left_twice.iter().any(|left| left.as_slice() == heading) {
                        left_twice.push(heading.to_vec());
                    }
                }
                Kind::Code | Kind::Text => {}
            }
        }
    }
}

/// What the text a union has written says about the conflicting place after
/// it ([`Above::reads_unreleased`]). The union reads that text once, line by
/// line, as it writes it.
#[derive(Default)]
struct Above {
    /// How much of the text is read: up to the end of its last whole line.
    read: usize,
    /// The code fence open after the text read.
    fence: Fence,
    /// How many `## [Unreleased]` heading lines the text read holds.
    unreleased: usize,
    /// Whether the nearest `#` or `##` heading or setext underline in the
    /// text read is a `## [Unreleased]` heading line.
    in_unreleased: bool,
}

impl Above {
    /// Whether the place after `written`, the text the union has written so
    /// far, is inside its one `[Unreleased]` section: `written` holds one
    /// `## [Unreleased]` heading line, trailing whitespace aside, and no
    /// other `#` or `##` heading or setext underline follows that line; no
    /// code fence is open after `written`; and `written` ends a line. Only
    /// the text past what an earlier call read is read.
    fn reads_unreleased(&mut self, written: &[u8]) -> bool {
        let unread = written.get(self.read..).unwrap_or_default();
        for line in unread.split_inclusive(|byte| *byte == b'\n') {
            if !line.ends_with(b"\n") {
                break;
            }
            self.read += line.len();
            if self.fence.read(line) == Kind::Section {
                let unreleased = is_unreleased_heading(line);
                self.unreleased += usize::from(unreleased);
                self.in_unreleased = unreleased;
            }
        }
        self.read == written.len()
            && self.fence.0.is_none()
            && self.unreleased == 1
            && self.in_unreleased
    }
}

/// The code fence open after the lines read so far, as its character and
/// the length of its run; `None` where no fence is open ([`Fence::read`]).
#[derive(Clone, Copy, Default)]
struct Fence(Option<(u8, usize)>);

impl Fence {
    /// What `line`, the line after those read so far, is as Markdown reads
    /// it. A fence opens at a line of three or more backticks or tildes
    /// ([`fence_opening`]), and closes at a line of the same character, no
    /// shorter, with nothing but whitespace around it.
    fn read(&mut self, line: &[u8]) -> Kind {
        if let Some((mark, run)) = self.0 {
            let rest = line.trim_ascii();
            if rest.len() >= run && rest.iter().all(|&byte| byte == mark) {
                self.0 = None;
            }
            return Kind::Code;
        }
        if let Some(opened) = fence_opening(line) {
            self.0 = Some(opened);
            return Kind::Code;
        }
        if line.starts_with(b"### ") {
            Kind::List
        } else if atx_level(line).is_some_and(|level| level < 3) || is_setext_underline(line) {
            Kind::Section
        } else {
            Kind::Text
        }
    }
}

/// What a line is as Markdown reads it ([`Fence::read`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A code fence line, or a line inside a fence.
    Code,
    /// A `#` or `##` heading, up to three spaces in ([`atx_level`]), or a
    /// setext underline ([`is_setext_underline`]): where a section starts.
    Section,
    /// A `### ` heading at the start of the line: where a list starts.
    List,
    /// Any other line.
    Text,
}

/// A side's block of plain `### ` lists, in its parts ([`plain_lists`]).
/// Each part is a run of whole lines of the block's text.
struct PlainBlock<'a> {
    /// The block's text.
    text: &'a [u8],
    /// The blank lines before its first list.
    lead: &'a [u8],
    /// Its lists, in the order of its lines.
    lists: Vec<List<'a>>,
}

/// A `### ` list of a block ([`PlainBlock`]).
struct List<'a> {
    /// Its heading line.
    heading: &'a [u8],
    /// The blank lines between its heading and its first entry; all its blank
    /// lines where it has no entry.
    above: &'a [u8],
    /// Its entries: its lines from the first that is not blank to the last.
    entries: &'a [u8],
    /// The blank lines after its last entry.
    below: &'a [u8],
}

impl<'a> List<'a> {
    /// Its heading's text ([`first_line`]), by which it finds its target.
    fn name(&self) -> &'a [u8] {
        first_line(self.heading)
    }
}

/// Where a list lies in its block's text ([`plain_lists`]).
struct Bounds {
    /// The start of its heading line.
    heading: usize,
    /// The end of its heading line.
    body: usize,
    /// The start of its first entry and the end of its last, where it has
    /// one.
    entries: Option<(usize, usize)>,
}

/// `text`, a side's block, as plain `### ` lists, or `None` where it is any
/// other shape. A plain block is blank lines, then lists: a `### ` heading
/// at the start of a line, then lines that are each blank, a list item
/// ([`is_list_item`]) or a continuation that starts with whitespace. It
/// holds no other heading of `#` to `######` up to three spaces in
/// ([`atx_level`]), no code fence ([`fence_opening`]) and no setext
/// underline ([`is_setext_underline`]), and its last line ends with a
/// newline, so a line can follow it.
fn plain_lists(text: &[u8]) -> Option<PlainBlock<'_>> {
    if !text.ends_with(b"\n") {
        return None;
    }
    let mut bounds: Vec<Bounds> = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive(|byte| *byte == b'\n') {
        let end = start + line.len();
        if line.starts_with(b"### ") {
            bounds.push(Bounds {
                heading: start,
                body: end,
                entries: None,
            });
        } else if atx_level(line).is_some()
            || fence_opening(line).is_some()
            || is_setext_underline(line)
        {
            return None;
        } else if !is_blank(line) {
            // A line that is not blank before the first heading is prose.
            let list = bounds.last_mut()?;
            if !(is_list_item(line) || line.first().is_some_and(u8::is_ascii_whitespace)) {
                return None;
            }
            let first = list.entries.map_or(start, |(first, _)| first);
            list.entries = Some((first, end));
        }
        start = end;
    }
    let ends = bounds
        .iter()
        .skip(1)
        .map(|next| next.heading)
        .chain([text.len()]);
    let lists = bounds
        .iter()
        .zip(ends)
        .map(|(list, end)| {
            let (first, last) = list.entries.unwrap_or((end, end));
            Some(List {
                heading: text.get(list.heading..list.body)?,
                above: text.get(list.body..first)?,
                entries: text.get(first..last)?,
                below: text.get(last..end)?,
            })
        })
        .collect::<Option<Vec<List<'_>>>>()?;
    Some(PlainBlock {
        text,
        lead: text.get(..bounds.first()?.heading)?,
        lists,
    })
}

/// The blocks that one conflicting place writes where every block there is
/// plain `### ` lists inside the `[Unreleased]` section of a file with one
/// `## [Unreleased]` heading line ([`union_of`]), in the order the union
/// writes them. Each `### ` heading there has one target: the first list
/// under it in the last block that has it, the same text. Each list of an
/// earlier block under that heading joins the target: its entries go after
/// the target's heading and the blank lines that follow it, above the
/// target's own entries, in the order the union writes the blocks, so what
/// the write brings in goes above what the earlier release published. Entry
/// lines are kept byte for byte. The joining list's heading and its blank
/// lines are not written, so the heading and the blank lines around the
/// entries are written once. Where the target has no entries of its own, its
/// blank lines are written below the joined entries too, so a blank line
/// still parts them from the next heading. A block keeps the blank lines
/// before its first list; a block whose every list joined another is left
/// with those alone, and writes nothing. One block can be the target under
/// one heading and lose its list under another, and a second list under one
/// heading in the target's block stays where it is. A list finds its target
/// by its heading's text, so no list is read once for each other list.
fn joined_blocks<'a>(blocks: &[PlainBlock<'a>]) -> Vec<Cow<'a, [u8]>> {
    // A block read later takes over each heading it has, so a heading's
    // target is in the last block that has it.
    let mut targets: HashMap<&'a [u8], Target<'a>> = HashMap::new();
    for (index, block) in blocks.iter().enumerate() {
        for (at, list) in block.lists.iter().enumerate() {
            if targets
                .get(list.name())
                .is_some_and(|target| target.block == index)
            {
                continue;
            }
            targets.insert(
                list.name(),
                Target {
                    block: index,
                    list: at,
                    entries: Vec::new(),
                },
            );
        }
    }
    blocks
        .iter()
        .enumerate()
        .map(|(index, block)| joined_block(index, block, &mut targets))
        .collect()
}

/// The target of a `### ` heading at a conflicting place
/// ([`joined_blocks`]).
struct Target<'a> {
    /// The index of the block that holds it, in the order the union writes
    /// the blocks.
    block: usize,
    /// The index of its list in that block.
    list: usize,
    /// The entries that join it, in the order the union writes their blocks.
    entries: Vec<&'a [u8]>,
}

/// What a list of a block becomes at a conflicting place
/// ([`joined_block`]).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// It is written as its side wrote it.
    Stays,
    /// Its entries join its heading's target, in a later block.
    Joins,
    /// It is its heading's target, and earlier blocks' entries join it.
    Takes,
}

/// `block`, the `index`th block that the union writes at a conflicting
/// place, as [`joined_blocks`] writes it: less each list whose target is in
/// a later block, its entries added to that target's, and with the entries
/// that earlier blocks added to each of its lists that is a target.
fn joined_block<'a>(
    index: usize,
    block: &PlainBlock<'a>,
    targets: &mut HashMap<&'a [u8], Target<'a>>,
) -> Cow<'a, [u8]> {
    let fates: Vec<Fate> = block
        .lists
        .iter()
        .enumerate()
        .map(|(at, list)| match targets.get(list.name()) {
            Some(target) if target.block > index => Fate::Joins,
            Some(target)
                if target.block == index && target.list == at && !target.entries.is_empty() =>
            {
                Fate::Takes
            }
            _ => Fate::Stays,
        })
        .collect();
    if fates.iter().all(|fate| *fate == Fate::Stays) {
        return Cow::Borrowed(block.text);
    }
    // A block that keeps a list keeps the blank lines before its first list;
    // a block whose every list joined another is left with those alone.
    let keeps = fates.iter().any(|fate| *fate != Fate::Joins);
    let mut written = Vec::new();
    if keeps {
        written.reserve(block.text.len());
        written.extend_from_slice(block.lead);
    }
    for (list, fate) in block.lists.iter().zip(fates) {
        match (fate, targets.get_mut(list.name())) {
            (Fate::Joins, Some(target)) => {
                if !list.entries.is_empty() {
                    target.entries.push(list.entries);
                }
            }
            (Fate::Takes, Some(target)) => {
                written.extend_from_slice(list.heading);
                written.extend_from_slice(list.above);
                for entries in &target.entries {
                    written.extend_from_slice(entries);
                }
                written.extend_from_slice(list.entries);
                // A list with no entries holds all its blank lines above
                // them; written below the joined entries too, they part
                // those entries from what follows.
                if list.entries.is_empty() {
                    written.extend_from_slice(list.above);
                }
                written.extend_from_slice(list.below);
            }
            _ => {
                for part in [list.heading, list.above, list.entries, list.below] {
                    written.extend_from_slice(part);
                }
            }
        }
    }
    if keeps {
        Cow::Owned(written)
    } else {
        Cow::Borrowed(&[])
    }
}

/// The level of an ATX heading line: one to six `#`, up to three spaces in,
/// then a space, a tab or the line's end. `None` for any other line.
fn atx_level(line: &[u8]) -> Option<usize> {
    let indent = line
        .iter()
        .take(4)
        .take_while(|&&byte| byte == b' ')
        .count();
    let rest = line.get(indent..).filter(|_| indent < 4)?;
    let level = rest.iter().take_while(|&&byte| byte == b'#').count();
    let ends = rest
        .get(level)
        .is_none_or(|&byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    ((1..=6).contains(&level) && ends).then_some(level)
}

/// The character and the run length of the code fence that `line` opens:
/// three or more backticks or tildes after any leading whitespace. `None`
/// for any other line.
fn fence_opening(line: &[u8]) -> Option<(u8, usize)> {
    let rest = line.trim_ascii_start();
    let mark = *rest.first().filter(|&&byte| matches!(byte, b'`' | b'~'))?;
    let run = rest.iter().take_while(|&&byte| byte == mark).count();
    (run >= 3).then_some((mark, run))
}

/// Whether `line`, less the whitespace around it, is a run of `=` or a run
/// of `-`: the underline that makes the line above it a heading.
fn is_setext_underline(line: &[u8]) -> bool {
    let rest = line.trim_ascii();
    rest.first()
        .is_some_and(|&mark| matches!(mark, b'=' | b'-') && rest.iter().all(|&byte| byte == mark))
}

/// Whether `line` starts a list item: `- `, `* ` or `+ `, or digits then
/// `. ` or `) `.
fn is_list_item(line: &[u8]) -> bool {
    let digits = line.iter().take_while(|byte| byte.is_ascii_digit()).count();
    let marks: &[u8] = if digits == 0 { b"-*+" } else { b".)" };
    line.get(digits).is_some_and(|mark| marks.contains(mark)) && line.get(digits + 1) == Some(&b' ')
}

/// Whether `runs`, the lists of a target's one `[Unreleased]` section
/// ([`unreleased_runs`]), hold `block`, a member's inserted block, as a
/// union writes it where the block's lists joined other blocks' lists
/// ([`joined_blocks`]): the block is plain `### ` lists ([`plain_lists`]),
/// and the entries of each of its lists are a whole run of lines under the
/// same heading there, byte for byte.
fn holds_joined(runs: &[Run<'_>], block: &[u8]) -> bool {
    plain_lists(block).is_some_and(|block| {
        block
            .lists
            .iter()
            .all(|list| listed_under(runs, list.name(), &lines(list.entries)))
    })
}

/// Whether `entries` are a whole run of the lines of one of `runs` under
/// `heading`; for a heading with no entries, whether one of `runs` is under
/// that heading.
fn listed_under(runs: &[Run<'_>], heading: &[u8], entries: &[&[u8]]) -> bool {
    runs.iter().filter(|run| run.heading == heading).any(|run| {
        entries.is_empty()
            || run
                .lines
                .windows(entries.len())
                .any(|window| window == entries)
    })
}

/// Lines of a target that stand under one `### ` heading inside its one
/// `[Unreleased]` section ([`unreleased_runs`]).
struct Run<'a> {
    /// The heading's text ([`first_line`]).
    heading: &'a [u8],
    /// The lines, each with its terminator, in the target's order.
    lines: Vec<&'a [u8]>,
}

/// The runs of `target`'s lines under each `### ` heading of its one
/// `[Unreleased]` section, or `None` where `target` holds no
/// `## [Unreleased]` heading line or more than one
/// ([`is_unreleased_heading`]). The section runs from that heading line to
/// the next `#` or `##` heading or setext underline, and a `### ` heading at
/// the start of a line opens a list there ([`Fence::read`]). A run is lines
/// of one list outside any code fence: a fence ends a run, and the list's
/// lines after it start another. Any other heading ends the list. A list
/// with no lines is one empty run, so its heading is still read.
fn unreleased_runs(target: &[u8]) -> Option<Vec<Run<'_>>> {
    let mut fence = Fence::default();
    let mut headings = 0_usize;
    let mut inside = false;
    let mut runs: Vec<Run<'_>> = Vec::new();
    // The heading of the list that the next line stands in, and whether a
    // fence since its last line makes that line start a run of its own.
    let mut list: Option<&[u8]> = None;
    let mut split = false;
    for line in target.split_inclusive(|byte| *byte == b'\n') {
        match fence.read(line) {
            Kind::Section => {
                inside = is_unreleased_heading(line);
                headings += usize::from(inside);
                if headings > 1 {
                    return None;
                }
                list = None;
            }
            Kind::List => {
                list = inside.then(|| first_line(line));
                if let Some(heading) = list {
                    runs.push(Run {
                        heading,
                        lines: Vec::new(),
                    });
                }
                split = false;
            }
            Kind::Code => split = true,
            Kind::Text if atx_level(line).is_some() => list = None,
            Kind::Text => {
                if let Some(heading) = list {
                    if split {
                        runs.push(Run {
                            heading,
                            lines: Vec::new(),
                        });
                        split = false;
                    }
                    if let Some(run) = runs.last_mut() {
                        run.lines.push(line);
                    }
                }
            }
        }
    }
    (headings == 1).then_some(runs)
}

/// Each `### ` heading line that the one `[Unreleased]` section of
/// `content`, a release log, holds more than once, and how many times.
///
/// A heading is its line without the line terminator ([`first_line`]), in
/// the order the section first holds each. The section is read as
/// [`unreleased_runs`] reads it ([`Fence::read`]): a `### ` line inside a
/// code fence is no heading, and a file with no `## [Unreleased]` heading
/// line, or more than one, has no one section and holds none. Every `### `
/// heading in the section is a sibling of the others, as markdownlint's
/// `MD024` rule with `siblings_only` reads them. A heading is the same only
/// where its text is the same: one that differs by case or by a trailing
/// space is another heading. Whoever placed the lines, a union, a clean
/// merge, a member or a person, every release write and every `adopt` that
/// records a tree names each heading here and changes no line for it
/// ([`crate::jj::repeated_headings`]).
pub fn repeated_headings(content: &[u8]) -> Vec<(&[u8], usize)> {
    let mut fence = Fence::default();
    let mut unreleased = 0_usize;
    let mut inside = false;
    let mut counts: HashMap<&[u8], usize> = HashMap::new();
    // Each heading once, in the order the section first holds it.
    let mut order: Vec<&[u8]> = Vec::new();
    for line in content.split_inclusive(|byte| *byte == b'\n') {
        match fence.read(line) {
            Kind::Section => {
                inside = is_unreleased_heading(line);
                unreleased += usize::from(inside);
            }
            Kind::List if inside => {
                let heading = first_line(line);
                let count = counts.entry(heading).or_insert(0);
                if *count == 0 {
                    order.push(heading);
                }
                *count += 1;
            }
            Kind::List | Kind::Code | Kind::Text => {}
        }
    }
    if unreleased != 1 {
        return Vec::new();
    }
    order
        .into_iter()
        .filter_map(|heading| {
            counts
                .get(heading)
                .filter(|count| **count > 1)
                .map(|&count| (heading, count))
        })
        .collect()
}

/// `block`'s lines, each with its terminator.
fn lines(block: &[u8]) -> Vec<&[u8]> {
    block.split_inclusive(|byte| *byte == b'\n').collect()
}

/// Whether a line holds nothing but whitespace.
fn is_blank(line: &[u8]) -> bool {
    line.iter().all(u8::is_ascii_whitespace)
}

/// Whether `line` is the `## [Unreleased]` heading line, trailing
/// whitespace aside.
fn is_unreleased_heading(line: &[u8]) -> bool {
    line.trim_ascii_end() == b"## [Unreleased]"
}

/// Whether `target` already holds what `member` added to `base`.
///
/// A carriage check replays a member's change onto a target and reads what is
/// left over. A member whose block a release's union holds verbatim has
/// nothing left over, but the replay still conflicts, because the union's
/// other blocks sit where the member's block ends. This says what such a
/// replay means: a conflict every side of which only inserted, and whose
/// member side the target holds as a whole run of its own lines, or holds as
/// a union writes it where its lists joined another block's: the entries
/// of each of its lists as one run of lines, in its order, under one list
/// of the same `### ` heading inside the target's one `[Unreleased]`
/// section ([`holds_joined`]), is the target carrying that block — the
/// conflict is masked. The lines can be another member's or the target's
/// own: no occurrence is counted. Lines in a released section count only as
/// part of a whole run, and a target with no `## [Unreleased]` heading or
/// more than one holds no joined entry. A member whose lines the target
/// holds neither way is not carried, and says so.
pub fn masks(target: &[u8], base: &[u8], member: &[u8], options: &MergeOptions) -> bool {
    let replay = Merge::from_removes_adds([base], [target, member]);
    match merge_hunks(&replay, options) {
        MergeResult::Resolved(content) => content.as_slice() == target,
        MergeResult::Conflict(hunks) => {
            let mut replayed: Vec<u8> = Vec::new();
            // The target's `[Unreleased]` lists, read once, at the first
            // hunk that needs them.
            let unreleased = OnceCell::new();
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
                    && !unreleased
                        .get_or_init(|| unreleased_runs(target))
                        .as_deref()
                        .is_some_and(|runs| holds_joined(runs, member_side.as_slice()))
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
    fn a_list_under_a_heading_no_other_block_has_is_a_block_of_its_own() {
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

        // Then: no other block has `### Added`, so the member's list is its
        // own block, above the earlier release's, as before
        assert_bytes(
            &union.content,
            &unreleased("### Added\n\n- Member feature.\n\n### Fixed\n\n- Earlier fix.\n\n"),
        );

        // When: a fresh composition, with no earlier release, unions the two
        // members
        let contents = merge_of([base.as_slice()], [added.as_slice(), fixed.as_slice()]);
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: no other block has `### Added`, so that list is its own
        // block, in term order, as before
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
    fn a_fresh_composition_of_three_writes_each_heading_once_in_the_last_block_that_has_it() {
        // Given: an `[Unreleased]` section that two members each gave a
        // `### Fixed` list and a third gave an `### Added` list, each from
        // the same base
        let options = merge_options();
        let base = unreleased("");
        let alpha = unreleased("### Fixed\n\n- Alpha's fix.\n\n");
        let beta = unreleased("### Fixed\n\n- Beta's fix.\n\n");
        let gamma = unreleased("### Added\n\n- Gamma's feature.\n\n");
        let contents = merge_of(
            [base.as_slice(), base.as_slice()],
            [alpha.as_slice(), beta.as_slice(), gamma.as_slice()],
        );

        // When: a fresh composition, with no earlier release, unions them
        let union = union_of(&contents, None, &options).expect("every side only inserted");

        // Then: the block written last has no `### Fixed`, so alpha's list
        // joins beta's, the last block that has that heading; each heading
        // is written once, each line as its side wrote it, byte for byte
        assert_bytes(
            &union.content,
            &unreleased(
                "### Fixed\n\n- Alpha's fix.\n- Beta's fix.\n\n### Added\n\n- Gamma's feature.\n\n",
            ),
        );
        assert_eq!(headings(&union), ["### Fixed", "### Added"]);

        // And: the content check reads each side as carried by that union,
        // alpha's list joined to a block that is not the last one
        for side in [&alpha, &beta, &gamma] {
            assert!(masks(&union.content, &base, side, &options));
        }
    }

    #[test]
    fn a_heading_the_earlier_release_lacks_joins_the_last_member_block_that_has_it() {
        // Given: an `[Unreleased]` section that the earlier release gave an
        // `### Added` list, and two members that each gave it a `### Fixed`
        // list, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let earlier = unreleased("### Added\n\n- Earlier feature.\n\n");
        let alpha = unreleased("### Fixed\n\n- Alpha's fix.\n\n");
        let beta = unreleased("### Fixed\n\n- Beta's fix.\n\n");
        let contents = merge_of(
            [base.as_slice(), base.as_slice()],
            [earlier.as_slice(), alpha.as_slice(), beta.as_slice()],
        );

        // When: the write unions them, naming the earlier release's content
        let union = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("every side only inserted");

        // Then: the earlier release's block, written last, has no
        // `### Fixed`, so alpha's list joins beta's, the last member block
        // that has it: `### Fixed` is written once, and the earlier release's
        // block is written as it was
        assert_bytes(
            &union.content,
            &unreleased(
                "### Fixed\n\n- Alpha's fix.\n- Beta's fix.\n\n### Added\n\n- Earlier feature.\n\n",
            ),
        );
        assert_eq!(headings(&union), ["### Fixed", "### Added"]);

        // And: the content check reads each side as carried by that union
        for side in [&earlier, &alpha, &beta] {
            assert!(masks(&union.content, &base, side, &options));
        }
    }

    #[test]
    fn a_block_can_be_the_target_under_one_heading_and_lose_its_list_under_another() {
        // Given: an `[Unreleased]` section that alpha gave a `### Fixed`
        // list, beta an `### Added` and a `### Fixed` list, and gamma an
        // `### Added` list, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let alpha = unreleased("### Fixed\n\n- Alpha's fix.\n\n");
        let beta = unreleased("### Added\n\n- Beta's feature.\n\n### Fixed\n\n- Beta's fix.\n\n");
        let gamma = unreleased("### Added\n\n- Gamma's feature.\n\n");
        let contents = merge_of(
            [base.as_slice(), base.as_slice()],
            [alpha.as_slice(), beta.as_slice(), gamma.as_slice()],
        );

        // When: a fresh composition, with no earlier release, unions them
        let union = union_of(&contents, None, &options).expect("every side only inserted");

        // Then: beta's block is the last that has `### Fixed`, so alpha's
        // list joins beta's there, and gamma's is the last that has
        // `### Added`, so beta's `### Added` list joins gamma's; each heading
        // is written once, each line as its side wrote it, byte for byte
        assert_bytes(
            &union.content,
            &unreleased(
                "### Fixed\n\n- Alpha's fix.\n- Beta's fix.\n\n\
                 ### Added\n\n- Beta's feature.\n- Gamma's feature.\n\n",
            ),
        );
        assert_eq!(headings(&union), ["### Fixed", "### Added"]);

        // And: the content check reads each side as carried by that union
        for side in [&alpha, &beta, &gamma] {
            assert!(masks(&union.content, &base, side, &options));
        }
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

    /// The headings a union names as left twice ([`Union::left_twice`]).
    fn left(union: &Union) -> Vec<String> {
        union
            .left_twice
            .iter()
            .map(|line| String::from_utf8_lossy(line).into_owned())
            .collect()
    }

    #[test]
    fn a_block_with_a_code_fence_is_written_whole_and_its_heading_named_as_left_twice() {
        // Given: an `[Unreleased]` section that the earlier release gave a
        // `### Changed` list, and a member that gave it a `### Changed` list
        // whose entry holds a fenced TOML sample that opens with a `#`
        // comment line, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let fenced = "### Changed\n\n- New config:\n\n```toml\n# the new key\nkey = 1\n```\n\n";
        let own = "### Changed\n\n- E\n\n";
        let (member, earlier) = (unreleased(fenced), unreleased(own));
        let contents = merge_of([base.as_slice()], [earlier.as_slice(), member.as_slice()]);

        // When: the write unions them, naming the earlier release's content
        let union = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("both sides only inserted");

        // Then: a block there holds a code fence, so each block is written
        // whole, byte for byte, the member's above the earlier release's,
        // and `### Changed` is named as left twice
        assert_bytes(&union.content, &unreleased(&format!("{fenced}{own}")));
        assert_eq!(headings(&union), ["### Changed", "### Changed"]);
        assert_eq!(left(&union), ["### Changed"]);
    }

    #[test]
    fn a_block_that_opens_a_version_section_is_written_whole() {
        // Given: an `[Unreleased]` section that a member gave a `### Fixed`
        // list and then a released `## [1.1.0]` section, and that the
        // earlier release gave a `### Fixed` list, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let released = "### Fixed\n\n- A (unreleased)\n\n## [1.1.0]\n\n- Released.\n\n";
        let own = "### Fixed\n\n- B\n\n";
        let (member, earlier) = (unreleased(released), unreleased(own));
        let contents = merge_of([base.as_slice()], [earlier.as_slice(), member.as_slice()]);

        // When: the write unions them, naming the earlier release's content,
        // so the earlier release's block is written last
        let union = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("both sides only inserted");

        // Then: a block there holds a `##` heading, so each block is written
        // whole, byte for byte, and the member's unreleased entry stays above
        // `## [1.1.0]`; the two `### Fixed` lists stand in two sections, so
        // neither is named as left twice
        assert_bytes(&union.content, &unreleased(&format!("{released}{own}")));
        assert_eq!(headings(&union), ["### Fixed", "### Fixed"]);
        assert_eq!(left(&union), Vec::<String>::new());
    }

    #[test]
    fn a_block_with_prose_before_its_first_list_is_written_whole() {
        // Given: an `[Unreleased]` section that alpha gave a line of prose
        // and a `### Fixed` list, and beta a `### Fixed` list, each from the
        // same base
        let options = merge_options();
        let base = unreleased("");
        let prose = "Alpha's notes.\n\n### Fixed\n\n- Alpha's fix.\n\n";
        let fixed = "### Fixed\n\n- Beta's fix.\n\n";
        let (alpha, beta) = (unreleased(prose), unreleased(fixed));
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: alpha's block is not plain lists, so each block is written
        // whole, byte for byte, in term order, and `### Fixed` is named as
        // left twice
        assert_bytes(&union.content, &unreleased(&format!("{prose}{fixed}")));
        assert_eq!(headings(&union), ["Alpha's notes.", "### Fixed"]);
        assert_eq!(left(&union), ["### Fixed"]);
    }

    #[test]
    fn a_block_with_a_setext_underline_is_written_whole() {
        // Given: an `[Unreleased]` section that alpha gave a `### Fixed`
        // list, and beta a `### Fixed` list whose entry an indented `---`
        // line underlines, which makes it a heading, each from the same base
        let options = merge_options();
        let base = unreleased("");
        let fixed = "### Fixed\n\n- Alpha's fix.\n\n";
        let underlined = "### Fixed\n\n- Beta's fix\n  ---\n\n";
        let (alpha, beta) = (unreleased(fixed), unreleased(underlined));
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: beta's block is not plain lists, so each block is written
        // whole, byte for byte, in term order, and `### Fixed` is named as
        // left twice
        assert_bytes(&union.content, &unreleased(&format!("{fixed}{underlined}")));
        assert_eq!(left(&union), ["### Fixed"]);
    }

    #[test]
    fn a_place_outside_the_unreleased_section_is_written_whole() {
        // Given: a changelog whose `[1.0.0]` section two members each gave a
        // `### Fixed` list, right under its heading, each from the same base
        let options = merge_options();
        let file = |inserted: &str| {
            format!(
                "# Changelog\n\n## [Unreleased]\n\n- Next.\n\n## [1.0.0]\n{inserted}\
                 ## [0.9.0]\n\n- Old.\n"
            )
            .into_bytes()
        };
        let alpha_list = "\n### Fixed\n\n- Alpha's fix.\n\n";
        let beta_list = "\n### Fixed\n\n- Beta's fix.\n\n";
        let (base, alpha, beta) = (file(""), file(alpha_list), file(beta_list));
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the nearest heading above the place is `## [1.0.0]`, so each
        // block is written whole, byte for byte, in term order, and
        // `### Fixed` is named as left twice
        assert_bytes(&union.content, &file(&format!("{alpha_list}{beta_list}")));
        assert_eq!(left(&union), ["### Fixed"]);
    }

    #[test]
    fn a_file_with_two_unreleased_headings_is_written_whole() {
        // Given: a changelog with two `## [Unreleased]` headings, under the
        // second of which two members each gave a `### Fixed` list, each
        // from the same base
        let options = merge_options();
        let file = |inserted: &str| {
            format!(
                "# Changelog\n\n## [Unreleased]\n\n- Old.\n\n## [Unreleased]\n{inserted}\
                 ## [1.0.0]\n\n- First.\n"
            )
            .into_bytes()
        };
        let alpha_list = "\n### Fixed\n\n- Alpha's fix.\n\n";
        let beta_list = "\n### Fixed\n\n- Beta's fix.\n\n";
        let (base, alpha, beta) = (file(""), file(alpha_list), file(beta_list));
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the text above the place holds `## [Unreleased]` twice, so
        // it has no one `[Unreleased]` section: each block is written whole,
        // byte for byte, in term order, and `### Fixed` is named as left
        // twice
        assert_bytes(&union.content, &file(&format!("{alpha_list}{beta_list}")));
        assert_eq!(left(&union), ["### Fixed"]);
    }

    #[test]
    fn a_second_unreleased_heading_below_the_place_keeps_every_block_whole() {
        // Given: a changelog whose `[Unreleased]` section two members each
        // gave a `### Fixed` list, and which holds a stray second
        // `## [Unreleased]` heading below its `[1.0.0]` section, each from the
        // same base
        let options = merge_options();
        let file = |inserted: &str| {
            [
                unreleased(inserted),
                b"\n## [Unreleased]\n\n- Stale.\n".to_vec(),
            ]
            .concat()
        };
        let alpha_list = "### Fixed\n\n- Alpha's fix.\n\n";
        let beta_list = "### Fixed\n\n- Beta's fix.\n\n";
        let (base, alpha, beta) = (file(""), file(alpha_list), file(beta_list));
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: the text above the place holds one `## [Unreleased]`, but the
        // file holds two, so each block is written whole, byte for byte, in
        // term order, and `### Fixed` is named as left twice
        assert_bytes(&union.content, &file(&format!("{alpha_list}{beta_list}")));
        assert_eq!(headings(&union), ["### Fixed", "### Fixed"]);
        assert_eq!(left(&union), ["### Fixed"]);

        // And: the content check reads each side as carried by that union,
        // as a whole run of its lines
        for side in [&alpha, &beta] {
            assert!(masks(&union.content, &base, side, &options));
        }
    }

    /// A changelog whose `## [Unreleased]` heading line is followed at once
    /// by `unreleased`, with no blank line of the file's own between them,
    /// so each side's text there is one insertion, its blank lines included.
    fn tight(unreleased: &str) -> Vec<u8> {
        format!("# Changelog\n\n## [Unreleased]\n{unreleased}## [1.0.0]\n\n- First.\n").into_bytes()
    }

    #[test]
    fn entries_that_join_a_list_with_none_of_its_own_keep_a_blank_line_before_what_follows() {
        // Given: an `[Unreleased]` section that alpha gave a `### Fixed`
        // list, and beta a `### Fixed` heading with no entries, above or
        // below an `### Added` list, each from the same base
        let options = merge_options();
        let base = tight("");
        let alpha = tight("\n### Fixed\n\n- Alpha's fix.\n\n");
        for (beta, joined) in [
            (
                "\n### Fixed\n\n### Added\n\n- Beta's feature.\n\n",
                "\n### Fixed\n\n- Alpha's fix.\n\n### Added\n\n- Beta's feature.\n\n",
            ),
            (
                "\n### Added\n\n- Beta's feature.\n\n### Fixed\n\n",
                "\n### Added\n\n- Beta's feature.\n\n### Fixed\n\n- Alpha's fix.\n\n",
            ),
        ] {
            let beta_file = tight(beta);
            let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta_file.as_slice()]);

            // When: a fresh composition unions them
            let union = union_of(&contents, None, &options).expect("both sides only inserted");

            // Then: alpha's entry joins beta's empty `### Fixed` list, and
            // the blank line under that heading is written below the entry
            // too, so a blank line parts it from what follows, byte for byte
            assert_bytes(&union.content, &tight(joined));
            assert_eq!(union.first_lines.len(), 1, "{beta:?}");
            assert_eq!(left(&union), Vec::<String>::new());
        }
    }

    #[test]
    fn a_block_whose_first_list_joins_another_keeps_the_blank_line_before_it() {
        // Given: an `[Unreleased]` section that alpha gave a blank line, then
        // a `### Fixed` and an `### Added` list, and beta a blank line and a
        // `### Fixed` list, each from the same base
        let options = merge_options();
        let base = tight("");
        let alpha = tight("\n### Fixed\n\n- Alpha's fix.\n\n### Added\n\n- Alpha's feature.\n\n");
        let beta = tight("\n### Fixed\n\n- Beta's fix.\n\n");
        let contents = merge_of([base.as_slice()], [alpha.as_slice(), beta.as_slice()]);

        // When: a fresh composition unions them
        let union = union_of(&contents, None, &options).expect("both sides only inserted");

        // Then: alpha's `### Fixed` list joins beta's, and alpha's block
        // keeps the blank line before it, so a blank line still parts
        // `## [Unreleased]` from `### Added`, byte for byte
        assert_bytes(
            &union.content,
            &tight(
                "\n### Added\n\n- Alpha's feature.\n\n\
                 \n### Fixed\n\n- Alpha's fix.\n- Beta's fix.\n\n",
            ),
        );
        assert_eq!(left(&union), Vec::<String>::new());
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
    fn a_joined_entry_is_carried_only_under_its_heading_in_the_one_unreleased_section() {
        // Given: a changelog whose released `[1.0.0]` section lists
        // `- Bump dependencies.` under `### Changed`, and a member that adds
        // that same line under `### Changed` in `[Unreleased]`
        let options = merge_options();
        let file = |unreleased: &str| {
            format!(
                "## [Unreleased]\n\n{unreleased}## [1.0.0]\n\n### Changed\n\n\
                 - Bump dependencies.\n- Other.\n"
            )
            .into_bytes()
        };
        let base = file("");
        let member = file("### Changed\n\n- Bump dependencies.\n\n");

        // When: the target's `[Unreleased]` holds an earlier fix and not the
        // member's entry
        let dropped = file("### Fixed\n\n- Earlier fix.\n\n");
        // Then: it does not carry the member: the same line under the same
        // heading in the released section counts for nothing
        assert!(!masks(&dropped, &base, &member, &options));

        // When: the target is the union that joined the member's entry to
        // the earlier release's `### Changed` list in `[Unreleased]`
        let earlier = file("### Changed\n\n- Earlier change.\n\n");
        let contents = merge_of([base.as_slice()], [earlier.as_slice(), member.as_slice()]);
        let joined = union_of(&contents, Some(earlier.as_slice()), &options)
            .expect("both sides only inserted")
            .content;
        assert_bytes(
            &joined,
            &file("### Changed\n\n- Bump dependencies.\n- Earlier change.\n\n"),
        );
        // Then: it carries the member
        assert!(masks(&joined, &base, &member, &options));

        // When: that target holds a second `## [Unreleased]` heading
        let twice = [joined.as_slice(), b"\n## [Unreleased]\n\n- Stale.\n"].concat();
        // Then: it holds no joined entry, so it does not carry the member
        assert!(!masks(&twice, &base, &member, &options));

        // When: the line stands under `### Changed` in `[Unreleased]`, but
        // inside a code fence
        let fenced = file("### Changed\n\n- Earlier change:\n\n```\n- Bump dependencies.\n```\n\n");
        // Then: it is code, not an entry, so the target does not carry the
        // member
        assert!(!masks(&fenced, &base, &member, &options));
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
    fn a_heading_the_one_unreleased_section_holds_more_than_once_is_counted() {
        // Given: a changelog whose `[Unreleased]` section holds what each
        // case writes, above a released `[1.0.0]` section with one
        // `### Added` list
        let file = |unreleased: &str| {
            format!(
                "# Changelog\n\n## [Unreleased]\n\n{unreleased}## [1.0.0] - 2026-08-01\n\n\
                 ### Added\n\n- The first release.\n"
            )
            .into_bytes()
        };

        // When: the section holds `### Added` twice and `### Fixed` once
        let twice = file("### Added\n\n- New.\n\n### Added\n\n- Old.\n\n### Fixed\n\n- A fix.\n\n");
        // Then: `### Added` is counted twice, and the released section's
        // `### Added` is not counted
        assert_eq!(repeated_headings(&twice), [(b"### Added".as_slice(), 2)]);

        // When: the section holds `### Fixed` three times and `### Added`
        // twice
        let three = file(
            "### Fixed\n\n- One.\n\n### Added\n\n- A.\n\n### Fixed\n\n- Two.\n\n\
             ### Fixed\n\n- Three.\n\n### Added\n\n- B.\n\n",
        );
        // Then: each is counted, in the order the section first holds it
        assert_eq!(
            repeated_headings(&three),
            [(b"### Fixed".as_slice(), 3), (b"### Added".as_slice(), 2)]
        );
    }

    #[test]
    fn a_heading_only_a_released_section_or_a_code_fence_repeats_is_not_counted() {
        // Given: a changelog whose released `[1.0.0]` section holds
        // `### Fixed` twice, and whose `[Unreleased]` section holds it once
        let released: &[u8] = b"## [Unreleased]\n\n### Fixed\n\n- New.\n\n## [1.0.0]\n\n\
                                ### Fixed\n\n- One.\n\n### Fixed\n\n- Two.\n";
        // When/Then: nothing is counted
        assert_eq!(repeated_headings(released), Vec::<(&[u8], usize)>::new());

        // Given: an `[Unreleased]` section with one `### Added` list whose
        // entry holds a fenced sample with a `### Added` line
        let fenced: &[u8] =
            b"## [Unreleased]\n\n### Added\n\n- A sample:\n\n```md\n### Added\n```\n";
        // When/Then: the fenced line is code, so nothing is counted
        assert_eq!(repeated_headings(fenced), Vec::<(&[u8], usize)>::new());
    }

    #[test]
    fn a_file_with_no_one_unreleased_section_or_headings_of_other_text_counts_nothing() {
        // Given: a changelog with no `## [Unreleased]` heading, one with two,
        // and one whose `[Unreleased]` headings differ by case or by a
        // trailing space
        let none: &[u8] =
            b"# Changelog\n\n## [1.0.0]\n\n### Added\n\n- One.\n\n### Added\n\n- Two.\n";
        let two: &[u8] = b"## [Unreleased]\n\n### Added\n\n- One.\n\n### Added\n\n- Two.\n\n\
                           ## [Unreleased]\n\n- Stale.\n";
        let near: &[u8] =
            b"## [Unreleased]\n\n### Added\n\n- One.\n\n### added\n\n- Two.\n\n### Added \n\n- Three.\n";

        // When/Then: none of them counts a heading
        for content in [none, two, near] {
            assert_eq!(
                repeated_headings(content),
                Vec::<(&[u8], usize)>::new(),
                "{}",
                String::from_utf8_lossy(content)
            );
        }
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
