//! Words, as spec R9's Design reads them, and the two uses of them that
//! come before the word survival diff: the pairing of a member hunk's lines
//! ([`pair_hunk`]) and the member's own words on a line it changed
//! ([`line_word_diff`]).
//!
//! The words of a line are the matches of `\w+|[^\w\s]`, punctuation
//! included, as Python's `re` reads that pattern for a `str` pattern over
//! the line decoded with `surrogateescape`, which is how the prototype that
//! the spec takes reads them. Python's `\w` is `_` and each character for
//! which `str.isalnum()` is true, and its `\s` each character for which
//! `str.isspace()` is true. Rust's `char::is_alphanumeric` and
//! `char::is_whitespace` differ from both, and so does the `regex` crate's
//! `\w`, so the two sets are tables copied from Python 3.13.5
//! ([`WORD`], [`SPACE`]). A byte that is not part of valid UTF-8 decodes
//! to a lone surrogate of its own, which is neither a word character nor a
//! space: it is a word of its own. A word is a slice of the line, and words
//! compare by their bytes, as the decoded strings compare.

use std::cmp::Ordering;

use super::difflib::{Ratio, SequenceMatcher, Tag, real_quick_ratio};

/// The words of `line`, in order, each a slice of it.
pub(super) fn tokens(line: &[u8]) -> Vec<&[u8]> {
    let mut words = Vec::new();
    let mut offset = 0;
    for chunk in line.utf8_chunks() {
        let valid = chunk.valid();
        // Where the run of word characters read last starts, while it has
        // not ended.
        let mut run: Option<usize> = None;
        for (at, character) in valid.char_indices() {
            let start = offset + at;
            if is_word(character) {
                if run.is_none() {
                    run = Some(start);
                }
                continue;
            }
            if let Some(begin) = run.take() {
                words.extend(line.get(begin..start));
            }
            if !is_space(character) {
                words.extend(line.get(start..start + character.len_utf8()));
            }
        }
        offset += valid.len();
        if let Some(begin) = run.take() {
            words.extend(line.get(begin..offset));
        }
        let invalid = chunk.invalid().len();
        for at in offset..offset + invalid {
            words.extend(line.get(at..=at));
        }
        offset += invalid;
    }
    words
}

/// The pairing of spec R9's Design for one member hunk: for each line that
/// the hunk adds, the line it removed that the added line replaced, by
/// index into `removed`; `None` for a line it adds whole.
///
/// A hunk that removes as many lines as it adds pairs them by position.
/// Otherwise each removed line, in order, takes one added line from a
/// window that starts one line after the previous pair and holds as many
/// lines as the difference of the two counts plus three: the first whose
/// word similarity with it reaches one half, or a later one whose
/// similarity is strictly higher. A removed line that finds none leaves the
/// window where it was. This is the prototype's `pair_hunk`, step by step,
/// with its early exits on the quick ratios.
pub(super) fn pair_hunk(removed: &[&[u8]], added: &[&[u8]]) -> Vec<Option<usize>> {
    if removed.is_empty() || added.is_empty() {
        return vec![None; added.len()];
    }
    if removed.len() == added.len() {
        return (0..added.len()).map(Some).collect();
    }
    let added_words: Vec<Vec<&[u8]>> = added.iter().map(|line| tokens(line)).collect();
    let window = removed.len().abs_diff(added.len()) + 3;
    let mut pairs = vec![None; added.len()];
    let mut next = 0;
    for (index, line) in removed.iter().enumerate() {
        let words = tokens(line);
        let mut best: Option<usize> = None;
        let mut best_ratio = Ratio::HALF;
        for (candidate, candidate_words) in added_words.iter().enumerate().skip(next).take(window) {
            if real_quick_ratio(words.len(), candidate_words.len()) < best_ratio {
                continue;
            }
            let matcher = SequenceMatcher::new(&words, candidate_words);
            if matcher.quick_ratio() < best_ratio {
                continue;
            }
            let ratio = matcher.ratio();
            if ratio > best_ratio || (best.is_none() && ratio >= best_ratio) {
                best = Some(candidate);
                best_ratio = ratio;
            }
        }
        if let Some(candidate) = best {
            if let Some(slot) = pairs.get_mut(candidate) {
                *slot = Some(index);
            }
            next = candidate + 1;
        }
    }
    pairs
}

/// A member's own words on a line that it changed ([`line_word_diff`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct OwnWords<'l> {
    /// The new line's words that the change inserted or replaced, by index,
    /// in order.
    pub(super) added: Vec<usize>,
    /// The old line's words that the change deleted or replaced, in order.
    pub(super) removed: Vec<&'l [u8]>,
}

/// The word diff of `old`, the words of a line that a member replaced, and
/// `new`, the words of the line that replaced it: the words that the member
/// added to the line and the words that it removed from it.
pub(super) fn line_word_diff<'l>(old: &[&'l [u8]], new: &[&'l [u8]]) -> OwnWords<'l> {
    let mut own = OwnWords {
        added: Vec::new(),
        removed: Vec::new(),
    };
    for opcode in SequenceMatcher::new(old, new).opcodes() {
        if matches!(opcode.tag, Tag::Insert | Tag::Replace) {
            own.added.extend(opcode.b.clone());
        }
        if matches!(opcode.tag, Tag::Delete | Tag::Replace) {
            own.removed
                .extend(old.get(opcode.a).unwrap_or_default().iter().copied());
        }
    }
    own
}

/// Whether `character` is a word character, as Python's `\w` reads it.
fn is_word(character: char) -> bool {
    in_table(WORD, character)
}

/// Whether `character` is a space, as Python's `\s` reads it.
fn is_space(character: char) -> bool {
    in_table(SPACE, character)
}

/// Whether one of the ranges of `table` holds `character`.
fn in_table(table: &[(char, char)], character: char) -> bool {
    table
        .binary_search_by(|&(low, high)| {
            if high < character {
                Ordering::Less
            } else if low > character {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// The space characters of Python 3.13.5 (Unicode 15.1.0): each one for
/// which `str.isspace()` is true. Sorted, disjoint, inclusive ranges,
/// copied from `tests/fixtures/difflib.json` (`space_char_ranges`), which a
/// unit test compares with this table.
const SPACE: &[(char, char)] = &[
    ('\u{9}', '\u{d}'),
    ('\u{1c}', '\u{20}'),
    ('\u{85}', '\u{85}'),
    ('\u{a0}', '\u{a0}'),
    ('\u{1680}', '\u{1680}'),
    ('\u{2000}', '\u{200a}'),
    ('\u{2028}', '\u{2029}'),
    ('\u{202f}', '\u{202f}'),
    ('\u{205f}', '\u{205f}'),
    ('\u{3000}', '\u{3000}'),
];

/// The word characters of Python 3.13.5 (Unicode 15.1.0): `_`, and each
/// one for which `str.isalnum()` is true. Sorted, disjoint, inclusive
/// ranges, copied from `tests/fixtures/difflib.json` (`word_char_ranges`),
/// which a unit test compares with this table.
const WORD: &[(char, char)] = &[
    ('\u{30}', '\u{39}'),
    ('\u{41}', '\u{5a}'),
    ('\u{5f}', '\u{5f}'),
    ('\u{61}', '\u{7a}'),
    ('\u{aa}', '\u{aa}'),
    ('\u{b2}', '\u{b3}'),
    ('\u{b5}', '\u{b5}'),
    ('\u{b9}', '\u{ba}'),
    ('\u{bc}', '\u{be}'),
    ('\u{c0}', '\u{d6}'),
    ('\u{d8}', '\u{f6}'),
    ('\u{f8}', '\u{2c1}'),
    ('\u{2c6}', '\u{2d1}'),
    ('\u{2e0}', '\u{2e4}'),
    ('\u{2ec}', '\u{2ec}'),
    ('\u{2ee}', '\u{2ee}'),
    ('\u{370}', '\u{374}'),
    ('\u{376}', '\u{377}'),
    ('\u{37a}', '\u{37d}'),
    ('\u{37f}', '\u{37f}'),
    ('\u{386}', '\u{386}'),
    ('\u{388}', '\u{38a}'),
    ('\u{38c}', '\u{38c}'),
    ('\u{38e}', '\u{3a1}'),
    ('\u{3a3}', '\u{3f5}'),
    ('\u{3f7}', '\u{481}'),
    ('\u{48a}', '\u{52f}'),
    ('\u{531}', '\u{556}'),
    ('\u{559}', '\u{559}'),
    ('\u{560}', '\u{588}'),
    ('\u{5d0}', '\u{5ea}'),
    ('\u{5ef}', '\u{5f2}'),
    ('\u{620}', '\u{64a}'),
    ('\u{660}', '\u{669}'),
    ('\u{66e}', '\u{66f}'),
    ('\u{671}', '\u{6d3}'),
    ('\u{6d5}', '\u{6d5}'),
    ('\u{6e5}', '\u{6e6}'),
    ('\u{6ee}', '\u{6fc}'),
    ('\u{6ff}', '\u{6ff}'),
    ('\u{710}', '\u{710}'),
    ('\u{712}', '\u{72f}'),
    ('\u{74d}', '\u{7a5}'),
    ('\u{7b1}', '\u{7b1}'),
    ('\u{7c0}', '\u{7ea}'),
    ('\u{7f4}', '\u{7f5}'),
    ('\u{7fa}', '\u{7fa}'),
    ('\u{800}', '\u{815}'),
    ('\u{81a}', '\u{81a}'),
    ('\u{824}', '\u{824}'),
    ('\u{828}', '\u{828}'),
    ('\u{840}', '\u{858}'),
    ('\u{860}', '\u{86a}'),
    ('\u{870}', '\u{887}'),
    ('\u{889}', '\u{88e}'),
    ('\u{8a0}', '\u{8c9}'),
    ('\u{904}', '\u{939}'),
    ('\u{93d}', '\u{93d}'),
    ('\u{950}', '\u{950}'),
    ('\u{958}', '\u{961}'),
    ('\u{966}', '\u{96f}'),
    ('\u{971}', '\u{980}'),
    ('\u{985}', '\u{98c}'),
    ('\u{98f}', '\u{990}'),
    ('\u{993}', '\u{9a8}'),
    ('\u{9aa}', '\u{9b0}'),
    ('\u{9b2}', '\u{9b2}'),
    ('\u{9b6}', '\u{9b9}'),
    ('\u{9bd}', '\u{9bd}'),
    ('\u{9ce}', '\u{9ce}'),
    ('\u{9dc}', '\u{9dd}'),
    ('\u{9df}', '\u{9e1}'),
    ('\u{9e6}', '\u{9f1}'),
    ('\u{9f4}', '\u{9f9}'),
    ('\u{9fc}', '\u{9fc}'),
    ('\u{a05}', '\u{a0a}'),
    ('\u{a0f}', '\u{a10}'),
    ('\u{a13}', '\u{a28}'),
    ('\u{a2a}', '\u{a30}'),
    ('\u{a32}', '\u{a33}'),
    ('\u{a35}', '\u{a36}'),
    ('\u{a38}', '\u{a39}'),
    ('\u{a59}', '\u{a5c}'),
    ('\u{a5e}', '\u{a5e}'),
    ('\u{a66}', '\u{a6f}'),
    ('\u{a72}', '\u{a74}'),
    ('\u{a85}', '\u{a8d}'),
    ('\u{a8f}', '\u{a91}'),
    ('\u{a93}', '\u{aa8}'),
    ('\u{aaa}', '\u{ab0}'),
    ('\u{ab2}', '\u{ab3}'),
    ('\u{ab5}', '\u{ab9}'),
    ('\u{abd}', '\u{abd}'),
    ('\u{ad0}', '\u{ad0}'),
    ('\u{ae0}', '\u{ae1}'),
    ('\u{ae6}', '\u{aef}'),
    ('\u{af9}', '\u{af9}'),
    ('\u{b05}', '\u{b0c}'),
    ('\u{b0f}', '\u{b10}'),
    ('\u{b13}', '\u{b28}'),
    ('\u{b2a}', '\u{b30}'),
    ('\u{b32}', '\u{b33}'),
    ('\u{b35}', '\u{b39}'),
    ('\u{b3d}', '\u{b3d}'),
    ('\u{b5c}', '\u{b5d}'),
    ('\u{b5f}', '\u{b61}'),
    ('\u{b66}', '\u{b6f}'),
    ('\u{b71}', '\u{b77}'),
    ('\u{b83}', '\u{b83}'),
    ('\u{b85}', '\u{b8a}'),
    ('\u{b8e}', '\u{b90}'),
    ('\u{b92}', '\u{b95}'),
    ('\u{b99}', '\u{b9a}'),
    ('\u{b9c}', '\u{b9c}'),
    ('\u{b9e}', '\u{b9f}'),
    ('\u{ba3}', '\u{ba4}'),
    ('\u{ba8}', '\u{baa}'),
    ('\u{bae}', '\u{bb9}'),
    ('\u{bd0}', '\u{bd0}'),
    ('\u{be6}', '\u{bf2}'),
    ('\u{c05}', '\u{c0c}'),
    ('\u{c0e}', '\u{c10}'),
    ('\u{c12}', '\u{c28}'),
    ('\u{c2a}', '\u{c39}'),
    ('\u{c3d}', '\u{c3d}'),
    ('\u{c58}', '\u{c5a}'),
    ('\u{c5d}', '\u{c5d}'),
    ('\u{c60}', '\u{c61}'),
    ('\u{c66}', '\u{c6f}'),
    ('\u{c78}', '\u{c7e}'),
    ('\u{c80}', '\u{c80}'),
    ('\u{c85}', '\u{c8c}'),
    ('\u{c8e}', '\u{c90}'),
    ('\u{c92}', '\u{ca8}'),
    ('\u{caa}', '\u{cb3}'),
    ('\u{cb5}', '\u{cb9}'),
    ('\u{cbd}', '\u{cbd}'),
    ('\u{cdd}', '\u{cde}'),
    ('\u{ce0}', '\u{ce1}'),
    ('\u{ce6}', '\u{cef}'),
    ('\u{cf1}', '\u{cf2}'),
    ('\u{d04}', '\u{d0c}'),
    ('\u{d0e}', '\u{d10}'),
    ('\u{d12}', '\u{d3a}'),
    ('\u{d3d}', '\u{d3d}'),
    ('\u{d4e}', '\u{d4e}'),
    ('\u{d54}', '\u{d56}'),
    ('\u{d58}', '\u{d61}'),
    ('\u{d66}', '\u{d78}'),
    ('\u{d7a}', '\u{d7f}'),
    ('\u{d85}', '\u{d96}'),
    ('\u{d9a}', '\u{db1}'),
    ('\u{db3}', '\u{dbb}'),
    ('\u{dbd}', '\u{dbd}'),
    ('\u{dc0}', '\u{dc6}'),
    ('\u{de6}', '\u{def}'),
    ('\u{e01}', '\u{e30}'),
    ('\u{e32}', '\u{e33}'),
    ('\u{e40}', '\u{e46}'),
    ('\u{e50}', '\u{e59}'),
    ('\u{e81}', '\u{e82}'),
    ('\u{e84}', '\u{e84}'),
    ('\u{e86}', '\u{e8a}'),
    ('\u{e8c}', '\u{ea3}'),
    ('\u{ea5}', '\u{ea5}'),
    ('\u{ea7}', '\u{eb0}'),
    ('\u{eb2}', '\u{eb3}'),
    ('\u{ebd}', '\u{ebd}'),
    ('\u{ec0}', '\u{ec4}'),
    ('\u{ec6}', '\u{ec6}'),
    ('\u{ed0}', '\u{ed9}'),
    ('\u{edc}', '\u{edf}'),
    ('\u{f00}', '\u{f00}'),
    ('\u{f20}', '\u{f33}'),
    ('\u{f40}', '\u{f47}'),
    ('\u{f49}', '\u{f6c}'),
    ('\u{f88}', '\u{f8c}'),
    ('\u{1000}', '\u{102a}'),
    ('\u{103f}', '\u{1049}'),
    ('\u{1050}', '\u{1055}'),
    ('\u{105a}', '\u{105d}'),
    ('\u{1061}', '\u{1061}'),
    ('\u{1065}', '\u{1066}'),
    ('\u{106e}', '\u{1070}'),
    ('\u{1075}', '\u{1081}'),
    ('\u{108e}', '\u{108e}'),
    ('\u{1090}', '\u{1099}'),
    ('\u{10a0}', '\u{10c5}'),
    ('\u{10c7}', '\u{10c7}'),
    ('\u{10cd}', '\u{10cd}'),
    ('\u{10d0}', '\u{10fa}'),
    ('\u{10fc}', '\u{1248}'),
    ('\u{124a}', '\u{124d}'),
    ('\u{1250}', '\u{1256}'),
    ('\u{1258}', '\u{1258}'),
    ('\u{125a}', '\u{125d}'),
    ('\u{1260}', '\u{1288}'),
    ('\u{128a}', '\u{128d}'),
    ('\u{1290}', '\u{12b0}'),
    ('\u{12b2}', '\u{12b5}'),
    ('\u{12b8}', '\u{12be}'),
    ('\u{12c0}', '\u{12c0}'),
    ('\u{12c2}', '\u{12c5}'),
    ('\u{12c8}', '\u{12d6}'),
    ('\u{12d8}', '\u{1310}'),
    ('\u{1312}', '\u{1315}'),
    ('\u{1318}', '\u{135a}'),
    ('\u{1369}', '\u{137c}'),
    ('\u{1380}', '\u{138f}'),
    ('\u{13a0}', '\u{13f5}'),
    ('\u{13f8}', '\u{13fd}'),
    ('\u{1401}', '\u{166c}'),
    ('\u{166f}', '\u{167f}'),
    ('\u{1681}', '\u{169a}'),
    ('\u{16a0}', '\u{16ea}'),
    ('\u{16ee}', '\u{16f8}'),
    ('\u{1700}', '\u{1711}'),
    ('\u{171f}', '\u{1731}'),
    ('\u{1740}', '\u{1751}'),
    ('\u{1760}', '\u{176c}'),
    ('\u{176e}', '\u{1770}'),
    ('\u{1780}', '\u{17b3}'),
    ('\u{17d7}', '\u{17d7}'),
    ('\u{17dc}', '\u{17dc}'),
    ('\u{17e0}', '\u{17e9}'),
    ('\u{17f0}', '\u{17f9}'),
    ('\u{1810}', '\u{1819}'),
    ('\u{1820}', '\u{1878}'),
    ('\u{1880}', '\u{1884}'),
    ('\u{1887}', '\u{18a8}'),
    ('\u{18aa}', '\u{18aa}'),
    ('\u{18b0}', '\u{18f5}'),
    ('\u{1900}', '\u{191e}'),
    ('\u{1946}', '\u{196d}'),
    ('\u{1970}', '\u{1974}'),
    ('\u{1980}', '\u{19ab}'),
    ('\u{19b0}', '\u{19c9}'),
    ('\u{19d0}', '\u{19da}'),
    ('\u{1a00}', '\u{1a16}'),
    ('\u{1a20}', '\u{1a54}'),
    ('\u{1a80}', '\u{1a89}'),
    ('\u{1a90}', '\u{1a99}'),
    ('\u{1aa7}', '\u{1aa7}'),
    ('\u{1b05}', '\u{1b33}'),
    ('\u{1b45}', '\u{1b4c}'),
    ('\u{1b50}', '\u{1b59}'),
    ('\u{1b83}', '\u{1ba0}'),
    ('\u{1bae}', '\u{1be5}'),
    ('\u{1c00}', '\u{1c23}'),
    ('\u{1c40}', '\u{1c49}'),
    ('\u{1c4d}', '\u{1c7d}'),
    ('\u{1c80}', '\u{1c88}'),
    ('\u{1c90}', '\u{1cba}'),
    ('\u{1cbd}', '\u{1cbf}'),
    ('\u{1ce9}', '\u{1cec}'),
    ('\u{1cee}', '\u{1cf3}'),
    ('\u{1cf5}', '\u{1cf6}'),
    ('\u{1cfa}', '\u{1cfa}'),
    ('\u{1d00}', '\u{1dbf}'),
    ('\u{1e00}', '\u{1f15}'),
    ('\u{1f18}', '\u{1f1d}'),
    ('\u{1f20}', '\u{1f45}'),
    ('\u{1f48}', '\u{1f4d}'),
    ('\u{1f50}', '\u{1f57}'),
    ('\u{1f59}', '\u{1f59}'),
    ('\u{1f5b}', '\u{1f5b}'),
    ('\u{1f5d}', '\u{1f5d}'),
    ('\u{1f5f}', '\u{1f7d}'),
    ('\u{1f80}', '\u{1fb4}'),
    ('\u{1fb6}', '\u{1fbc}'),
    ('\u{1fbe}', '\u{1fbe}'),
    ('\u{1fc2}', '\u{1fc4}'),
    ('\u{1fc6}', '\u{1fcc}'),
    ('\u{1fd0}', '\u{1fd3}'),
    ('\u{1fd6}', '\u{1fdb}'),
    ('\u{1fe0}', '\u{1fec}'),
    ('\u{1ff2}', '\u{1ff4}'),
    ('\u{1ff6}', '\u{1ffc}'),
    ('\u{2070}', '\u{2071}'),
    ('\u{2074}', '\u{2079}'),
    ('\u{207f}', '\u{2089}'),
    ('\u{2090}', '\u{209c}'),
    ('\u{2102}', '\u{2102}'),
    ('\u{2107}', '\u{2107}'),
    ('\u{210a}', '\u{2113}'),
    ('\u{2115}', '\u{2115}'),
    ('\u{2119}', '\u{211d}'),
    ('\u{2124}', '\u{2124}'),
    ('\u{2126}', '\u{2126}'),
    ('\u{2128}', '\u{2128}'),
    ('\u{212a}', '\u{212d}'),
    ('\u{212f}', '\u{2139}'),
    ('\u{213c}', '\u{213f}'),
    ('\u{2145}', '\u{2149}'),
    ('\u{214e}', '\u{214e}'),
    ('\u{2150}', '\u{2189}'),
    ('\u{2460}', '\u{249b}'),
    ('\u{24ea}', '\u{24ff}'),
    ('\u{2776}', '\u{2793}'),
    ('\u{2c00}', '\u{2ce4}'),
    ('\u{2ceb}', '\u{2cee}'),
    ('\u{2cf2}', '\u{2cf3}'),
    ('\u{2cfd}', '\u{2cfd}'),
    ('\u{2d00}', '\u{2d25}'),
    ('\u{2d27}', '\u{2d27}'),
    ('\u{2d2d}', '\u{2d2d}'),
    ('\u{2d30}', '\u{2d67}'),
    ('\u{2d6f}', '\u{2d6f}'),
    ('\u{2d80}', '\u{2d96}'),
    ('\u{2da0}', '\u{2da6}'),
    ('\u{2da8}', '\u{2dae}'),
    ('\u{2db0}', '\u{2db6}'),
    ('\u{2db8}', '\u{2dbe}'),
    ('\u{2dc0}', '\u{2dc6}'),
    ('\u{2dc8}', '\u{2dce}'),
    ('\u{2dd0}', '\u{2dd6}'),
    ('\u{2dd8}', '\u{2dde}'),
    ('\u{2e2f}', '\u{2e2f}'),
    ('\u{3005}', '\u{3007}'),
    ('\u{3021}', '\u{3029}'),
    ('\u{3031}', '\u{3035}'),
    ('\u{3038}', '\u{303c}'),
    ('\u{3041}', '\u{3096}'),
    ('\u{309d}', '\u{309f}'),
    ('\u{30a1}', '\u{30fa}'),
    ('\u{30fc}', '\u{30ff}'),
    ('\u{3105}', '\u{312f}'),
    ('\u{3131}', '\u{318e}'),
    ('\u{3192}', '\u{3195}'),
    ('\u{31a0}', '\u{31bf}'),
    ('\u{31f0}', '\u{31ff}'),
    ('\u{3220}', '\u{3229}'),
    ('\u{3248}', '\u{324f}'),
    ('\u{3251}', '\u{325f}'),
    ('\u{3280}', '\u{3289}'),
    ('\u{32b1}', '\u{32bf}'),
    ('\u{3400}', '\u{4dbf}'),
    ('\u{4e00}', '\u{a48c}'),
    ('\u{a4d0}', '\u{a4fd}'),
    ('\u{a500}', '\u{a60c}'),
    ('\u{a610}', '\u{a62b}'),
    ('\u{a640}', '\u{a66e}'),
    ('\u{a67f}', '\u{a69d}'),
    ('\u{a6a0}', '\u{a6ef}'),
    ('\u{a717}', '\u{a71f}'),
    ('\u{a722}', '\u{a788}'),
    ('\u{a78b}', '\u{a7ca}'),
    ('\u{a7d0}', '\u{a7d1}'),
    ('\u{a7d3}', '\u{a7d3}'),
    ('\u{a7d5}', '\u{a7d9}'),
    ('\u{a7f2}', '\u{a801}'),
    ('\u{a803}', '\u{a805}'),
    ('\u{a807}', '\u{a80a}'),
    ('\u{a80c}', '\u{a822}'),
    ('\u{a830}', '\u{a835}'),
    ('\u{a840}', '\u{a873}'),
    ('\u{a882}', '\u{a8b3}'),
    ('\u{a8d0}', '\u{a8d9}'),
    ('\u{a8f2}', '\u{a8f7}'),
    ('\u{a8fb}', '\u{a8fb}'),
    ('\u{a8fd}', '\u{a8fe}'),
    ('\u{a900}', '\u{a925}'),
    ('\u{a930}', '\u{a946}'),
    ('\u{a960}', '\u{a97c}'),
    ('\u{a984}', '\u{a9b2}'),
    ('\u{a9cf}', '\u{a9d9}'),
    ('\u{a9e0}', '\u{a9e4}'),
    ('\u{a9e6}', '\u{a9fe}'),
    ('\u{aa00}', '\u{aa28}'),
    ('\u{aa40}', '\u{aa42}'),
    ('\u{aa44}', '\u{aa4b}'),
    ('\u{aa50}', '\u{aa59}'),
    ('\u{aa60}', '\u{aa76}'),
    ('\u{aa7a}', '\u{aa7a}'),
    ('\u{aa7e}', '\u{aaaf}'),
    ('\u{aab1}', '\u{aab1}'),
    ('\u{aab5}', '\u{aab6}'),
    ('\u{aab9}', '\u{aabd}'),
    ('\u{aac0}', '\u{aac0}'),
    ('\u{aac2}', '\u{aac2}'),
    ('\u{aadb}', '\u{aadd}'),
    ('\u{aae0}', '\u{aaea}'),
    ('\u{aaf2}', '\u{aaf4}'),
    ('\u{ab01}', '\u{ab06}'),
    ('\u{ab09}', '\u{ab0e}'),
    ('\u{ab11}', '\u{ab16}'),
    ('\u{ab20}', '\u{ab26}'),
    ('\u{ab28}', '\u{ab2e}'),
    ('\u{ab30}', '\u{ab5a}'),
    ('\u{ab5c}', '\u{ab69}'),
    ('\u{ab70}', '\u{abe2}'),
    ('\u{abf0}', '\u{abf9}'),
    ('\u{ac00}', '\u{d7a3}'),
    ('\u{d7b0}', '\u{d7c6}'),
    ('\u{d7cb}', '\u{d7fb}'),
    ('\u{f900}', '\u{fa6d}'),
    ('\u{fa70}', '\u{fad9}'),
    ('\u{fb00}', '\u{fb06}'),
    ('\u{fb13}', '\u{fb17}'),
    ('\u{fb1d}', '\u{fb1d}'),
    ('\u{fb1f}', '\u{fb28}'),
    ('\u{fb2a}', '\u{fb36}'),
    ('\u{fb38}', '\u{fb3c}'),
    ('\u{fb3e}', '\u{fb3e}'),
    ('\u{fb40}', '\u{fb41}'),
    ('\u{fb43}', '\u{fb44}'),
    ('\u{fb46}', '\u{fbb1}'),
    ('\u{fbd3}', '\u{fd3d}'),
    ('\u{fd50}', '\u{fd8f}'),
    ('\u{fd92}', '\u{fdc7}'),
    ('\u{fdf0}', '\u{fdfb}'),
    ('\u{fe70}', '\u{fe74}'),
    ('\u{fe76}', '\u{fefc}'),
    ('\u{ff10}', '\u{ff19}'),
    ('\u{ff21}', '\u{ff3a}'),
    ('\u{ff41}', '\u{ff5a}'),
    ('\u{ff66}', '\u{ffbe}'),
    ('\u{ffc2}', '\u{ffc7}'),
    ('\u{ffca}', '\u{ffcf}'),
    ('\u{ffd2}', '\u{ffd7}'),
    ('\u{ffda}', '\u{ffdc}'),
    ('\u{10000}', '\u{1000b}'),
    ('\u{1000d}', '\u{10026}'),
    ('\u{10028}', '\u{1003a}'),
    ('\u{1003c}', '\u{1003d}'),
    ('\u{1003f}', '\u{1004d}'),
    ('\u{10050}', '\u{1005d}'),
    ('\u{10080}', '\u{100fa}'),
    ('\u{10107}', '\u{10133}'),
    ('\u{10140}', '\u{10178}'),
    ('\u{1018a}', '\u{1018b}'),
    ('\u{10280}', '\u{1029c}'),
    ('\u{102a0}', '\u{102d0}'),
    ('\u{102e1}', '\u{102fb}'),
    ('\u{10300}', '\u{10323}'),
    ('\u{1032d}', '\u{1034a}'),
    ('\u{10350}', '\u{10375}'),
    ('\u{10380}', '\u{1039d}'),
    ('\u{103a0}', '\u{103c3}'),
    ('\u{103c8}', '\u{103cf}'),
    ('\u{103d1}', '\u{103d5}'),
    ('\u{10400}', '\u{1049d}'),
    ('\u{104a0}', '\u{104a9}'),
    ('\u{104b0}', '\u{104d3}'),
    ('\u{104d8}', '\u{104fb}'),
    ('\u{10500}', '\u{10527}'),
    ('\u{10530}', '\u{10563}'),
    ('\u{10570}', '\u{1057a}'),
    ('\u{1057c}', '\u{1058a}'),
    ('\u{1058c}', '\u{10592}'),
    ('\u{10594}', '\u{10595}'),
    ('\u{10597}', '\u{105a1}'),
    ('\u{105a3}', '\u{105b1}'),
    ('\u{105b3}', '\u{105b9}'),
    ('\u{105bb}', '\u{105bc}'),
    ('\u{10600}', '\u{10736}'),
    ('\u{10740}', '\u{10755}'),
    ('\u{10760}', '\u{10767}'),
    ('\u{10780}', '\u{10785}'),
    ('\u{10787}', '\u{107b0}'),
    ('\u{107b2}', '\u{107ba}'),
    ('\u{10800}', '\u{10805}'),
    ('\u{10808}', '\u{10808}'),
    ('\u{1080a}', '\u{10835}'),
    ('\u{10837}', '\u{10838}'),
    ('\u{1083c}', '\u{1083c}'),
    ('\u{1083f}', '\u{10855}'),
    ('\u{10858}', '\u{10876}'),
    ('\u{10879}', '\u{1089e}'),
    ('\u{108a7}', '\u{108af}'),
    ('\u{108e0}', '\u{108f2}'),
    ('\u{108f4}', '\u{108f5}'),
    ('\u{108fb}', '\u{1091b}'),
    ('\u{10920}', '\u{10939}'),
    ('\u{10980}', '\u{109b7}'),
    ('\u{109bc}', '\u{109cf}'),
    ('\u{109d2}', '\u{10a00}'),
    ('\u{10a10}', '\u{10a13}'),
    ('\u{10a15}', '\u{10a17}'),
    ('\u{10a19}', '\u{10a35}'),
    ('\u{10a40}', '\u{10a48}'),
    ('\u{10a60}', '\u{10a7e}'),
    ('\u{10a80}', '\u{10a9f}'),
    ('\u{10ac0}', '\u{10ac7}'),
    ('\u{10ac9}', '\u{10ae4}'),
    ('\u{10aeb}', '\u{10aef}'),
    ('\u{10b00}', '\u{10b35}'),
    ('\u{10b40}', '\u{10b55}'),
    ('\u{10b58}', '\u{10b72}'),
    ('\u{10b78}', '\u{10b91}'),
    ('\u{10ba9}', '\u{10baf}'),
    ('\u{10c00}', '\u{10c48}'),
    ('\u{10c80}', '\u{10cb2}'),
    ('\u{10cc0}', '\u{10cf2}'),
    ('\u{10cfa}', '\u{10d23}'),
    ('\u{10d30}', '\u{10d39}'),
    ('\u{10e60}', '\u{10e7e}'),
    ('\u{10e80}', '\u{10ea9}'),
    ('\u{10eb0}', '\u{10eb1}'),
    ('\u{10f00}', '\u{10f27}'),
    ('\u{10f30}', '\u{10f45}'),
    ('\u{10f51}', '\u{10f54}'),
    ('\u{10f70}', '\u{10f81}'),
    ('\u{10fb0}', '\u{10fcb}'),
    ('\u{10fe0}', '\u{10ff6}'),
    ('\u{11003}', '\u{11037}'),
    ('\u{11052}', '\u{1106f}'),
    ('\u{11071}', '\u{11072}'),
    ('\u{11075}', '\u{11075}'),
    ('\u{11083}', '\u{110af}'),
    ('\u{110d0}', '\u{110e8}'),
    ('\u{110f0}', '\u{110f9}'),
    ('\u{11103}', '\u{11126}'),
    ('\u{11136}', '\u{1113f}'),
    ('\u{11144}', '\u{11144}'),
    ('\u{11147}', '\u{11147}'),
    ('\u{11150}', '\u{11172}'),
    ('\u{11176}', '\u{11176}'),
    ('\u{11183}', '\u{111b2}'),
    ('\u{111c1}', '\u{111c4}'),
    ('\u{111d0}', '\u{111da}'),
    ('\u{111dc}', '\u{111dc}'),
    ('\u{111e1}', '\u{111f4}'),
    ('\u{11200}', '\u{11211}'),
    ('\u{11213}', '\u{1122b}'),
    ('\u{1123f}', '\u{11240}'),
    ('\u{11280}', '\u{11286}'),
    ('\u{11288}', '\u{11288}'),
    ('\u{1128a}', '\u{1128d}'),
    ('\u{1128f}', '\u{1129d}'),
    ('\u{1129f}', '\u{112a8}'),
    ('\u{112b0}', '\u{112de}'),
    ('\u{112f0}', '\u{112f9}'),
    ('\u{11305}', '\u{1130c}'),
    ('\u{1130f}', '\u{11310}'),
    ('\u{11313}', '\u{11328}'),
    ('\u{1132a}', '\u{11330}'),
    ('\u{11332}', '\u{11333}'),
    ('\u{11335}', '\u{11339}'),
    ('\u{1133d}', '\u{1133d}'),
    ('\u{11350}', '\u{11350}'),
    ('\u{1135d}', '\u{11361}'),
    ('\u{11400}', '\u{11434}'),
    ('\u{11447}', '\u{1144a}'),
    ('\u{11450}', '\u{11459}'),
    ('\u{1145f}', '\u{11461}'),
    ('\u{11480}', '\u{114af}'),
    ('\u{114c4}', '\u{114c5}'),
    ('\u{114c7}', '\u{114c7}'),
    ('\u{114d0}', '\u{114d9}'),
    ('\u{11580}', '\u{115ae}'),
    ('\u{115d8}', '\u{115db}'),
    ('\u{11600}', '\u{1162f}'),
    ('\u{11644}', '\u{11644}'),
    ('\u{11650}', '\u{11659}'),
    ('\u{11680}', '\u{116aa}'),
    ('\u{116b8}', '\u{116b8}'),
    ('\u{116c0}', '\u{116c9}'),
    ('\u{11700}', '\u{1171a}'),
    ('\u{11730}', '\u{1173b}'),
    ('\u{11740}', '\u{11746}'),
    ('\u{11800}', '\u{1182b}'),
    ('\u{118a0}', '\u{118f2}'),
    ('\u{118ff}', '\u{11906}'),
    ('\u{11909}', '\u{11909}'),
    ('\u{1190c}', '\u{11913}'),
    ('\u{11915}', '\u{11916}'),
    ('\u{11918}', '\u{1192f}'),
    ('\u{1193f}', '\u{1193f}'),
    ('\u{11941}', '\u{11941}'),
    ('\u{11950}', '\u{11959}'),
    ('\u{119a0}', '\u{119a7}'),
    ('\u{119aa}', '\u{119d0}'),
    ('\u{119e1}', '\u{119e1}'),
    ('\u{119e3}', '\u{119e3}'),
    ('\u{11a00}', '\u{11a00}'),
    ('\u{11a0b}', '\u{11a32}'),
    ('\u{11a3a}', '\u{11a3a}'),
    ('\u{11a50}', '\u{11a50}'),
    ('\u{11a5c}', '\u{11a89}'),
    ('\u{11a9d}', '\u{11a9d}'),
    ('\u{11ab0}', '\u{11af8}'),
    ('\u{11c00}', '\u{11c08}'),
    ('\u{11c0a}', '\u{11c2e}'),
    ('\u{11c40}', '\u{11c40}'),
    ('\u{11c50}', '\u{11c6c}'),
    ('\u{11c72}', '\u{11c8f}'),
    ('\u{11d00}', '\u{11d06}'),
    ('\u{11d08}', '\u{11d09}'),
    ('\u{11d0b}', '\u{11d30}'),
    ('\u{11d46}', '\u{11d46}'),
    ('\u{11d50}', '\u{11d59}'),
    ('\u{11d60}', '\u{11d65}'),
    ('\u{11d67}', '\u{11d68}'),
    ('\u{11d6a}', '\u{11d89}'),
    ('\u{11d98}', '\u{11d98}'),
    ('\u{11da0}', '\u{11da9}'),
    ('\u{11ee0}', '\u{11ef2}'),
    ('\u{11f02}', '\u{11f02}'),
    ('\u{11f04}', '\u{11f10}'),
    ('\u{11f12}', '\u{11f33}'),
    ('\u{11f50}', '\u{11f59}'),
    ('\u{11fb0}', '\u{11fb0}'),
    ('\u{11fc0}', '\u{11fd4}'),
    ('\u{12000}', '\u{12399}'),
    ('\u{12400}', '\u{1246e}'),
    ('\u{12480}', '\u{12543}'),
    ('\u{12f90}', '\u{12ff0}'),
    ('\u{13000}', '\u{1342f}'),
    ('\u{13441}', '\u{13446}'),
    ('\u{14400}', '\u{14646}'),
    ('\u{16800}', '\u{16a38}'),
    ('\u{16a40}', '\u{16a5e}'),
    ('\u{16a60}', '\u{16a69}'),
    ('\u{16a70}', '\u{16abe}'),
    ('\u{16ac0}', '\u{16ac9}'),
    ('\u{16ad0}', '\u{16aed}'),
    ('\u{16b00}', '\u{16b2f}'),
    ('\u{16b40}', '\u{16b43}'),
    ('\u{16b50}', '\u{16b59}'),
    ('\u{16b5b}', '\u{16b61}'),
    ('\u{16b63}', '\u{16b77}'),
    ('\u{16b7d}', '\u{16b8f}'),
    ('\u{16e40}', '\u{16e96}'),
    ('\u{16f00}', '\u{16f4a}'),
    ('\u{16f50}', '\u{16f50}'),
    ('\u{16f93}', '\u{16f9f}'),
    ('\u{16fe0}', '\u{16fe1}'),
    ('\u{16fe3}', '\u{16fe3}'),
    ('\u{17000}', '\u{187f7}'),
    ('\u{18800}', '\u{18cd5}'),
    ('\u{18d00}', '\u{18d08}'),
    ('\u{1aff0}', '\u{1aff3}'),
    ('\u{1aff5}', '\u{1affb}'),
    ('\u{1affd}', '\u{1affe}'),
    ('\u{1b000}', '\u{1b122}'),
    ('\u{1b132}', '\u{1b132}'),
    ('\u{1b150}', '\u{1b152}'),
    ('\u{1b155}', '\u{1b155}'),
    ('\u{1b164}', '\u{1b167}'),
    ('\u{1b170}', '\u{1b2fb}'),
    ('\u{1bc00}', '\u{1bc6a}'),
    ('\u{1bc70}', '\u{1bc7c}'),
    ('\u{1bc80}', '\u{1bc88}'),
    ('\u{1bc90}', '\u{1bc99}'),
    ('\u{1d2c0}', '\u{1d2d3}'),
    ('\u{1d2e0}', '\u{1d2f3}'),
    ('\u{1d360}', '\u{1d378}'),
    ('\u{1d400}', '\u{1d454}'),
    ('\u{1d456}', '\u{1d49c}'),
    ('\u{1d49e}', '\u{1d49f}'),
    ('\u{1d4a2}', '\u{1d4a2}'),
    ('\u{1d4a5}', '\u{1d4a6}'),
    ('\u{1d4a9}', '\u{1d4ac}'),
    ('\u{1d4ae}', '\u{1d4b9}'),
    ('\u{1d4bb}', '\u{1d4bb}'),
    ('\u{1d4bd}', '\u{1d4c3}'),
    ('\u{1d4c5}', '\u{1d505}'),
    ('\u{1d507}', '\u{1d50a}'),
    ('\u{1d50d}', '\u{1d514}'),
    ('\u{1d516}', '\u{1d51c}'),
    ('\u{1d51e}', '\u{1d539}'),
    ('\u{1d53b}', '\u{1d53e}'),
    ('\u{1d540}', '\u{1d544}'),
    ('\u{1d546}', '\u{1d546}'),
    ('\u{1d54a}', '\u{1d550}'),
    ('\u{1d552}', '\u{1d6a5}'),
    ('\u{1d6a8}', '\u{1d6c0}'),
    ('\u{1d6c2}', '\u{1d6da}'),
    ('\u{1d6dc}', '\u{1d6fa}'),
    ('\u{1d6fc}', '\u{1d714}'),
    ('\u{1d716}', '\u{1d734}'),
    ('\u{1d736}', '\u{1d74e}'),
    ('\u{1d750}', '\u{1d76e}'),
    ('\u{1d770}', '\u{1d788}'),
    ('\u{1d78a}', '\u{1d7a8}'),
    ('\u{1d7aa}', '\u{1d7c2}'),
    ('\u{1d7c4}', '\u{1d7cb}'),
    ('\u{1d7ce}', '\u{1d7ff}'),
    ('\u{1df00}', '\u{1df1e}'),
    ('\u{1df25}', '\u{1df2a}'),
    ('\u{1e030}', '\u{1e06d}'),
    ('\u{1e100}', '\u{1e12c}'),
    ('\u{1e137}', '\u{1e13d}'),
    ('\u{1e140}', '\u{1e149}'),
    ('\u{1e14e}', '\u{1e14e}'),
    ('\u{1e290}', '\u{1e2ad}'),
    ('\u{1e2c0}', '\u{1e2eb}'),
    ('\u{1e2f0}', '\u{1e2f9}'),
    ('\u{1e4d0}', '\u{1e4eb}'),
    ('\u{1e4f0}', '\u{1e4f9}'),
    ('\u{1e7e0}', '\u{1e7e6}'),
    ('\u{1e7e8}', '\u{1e7eb}'),
    ('\u{1e7ed}', '\u{1e7ee}'),
    ('\u{1e7f0}', '\u{1e7fe}'),
    ('\u{1e800}', '\u{1e8c4}'),
    ('\u{1e8c7}', '\u{1e8cf}'),
    ('\u{1e900}', '\u{1e943}'),
    ('\u{1e94b}', '\u{1e94b}'),
    ('\u{1e950}', '\u{1e959}'),
    ('\u{1ec71}', '\u{1ecab}'),
    ('\u{1ecad}', '\u{1ecaf}'),
    ('\u{1ecb1}', '\u{1ecb4}'),
    ('\u{1ed01}', '\u{1ed2d}'),
    ('\u{1ed2f}', '\u{1ed3d}'),
    ('\u{1ee00}', '\u{1ee03}'),
    ('\u{1ee05}', '\u{1ee1f}'),
    ('\u{1ee21}', '\u{1ee22}'),
    ('\u{1ee24}', '\u{1ee24}'),
    ('\u{1ee27}', '\u{1ee27}'),
    ('\u{1ee29}', '\u{1ee32}'),
    ('\u{1ee34}', '\u{1ee37}'),
    ('\u{1ee39}', '\u{1ee39}'),
    ('\u{1ee3b}', '\u{1ee3b}'),
    ('\u{1ee42}', '\u{1ee42}'),
    ('\u{1ee47}', '\u{1ee47}'),
    ('\u{1ee49}', '\u{1ee49}'),
    ('\u{1ee4b}', '\u{1ee4b}'),
    ('\u{1ee4d}', '\u{1ee4f}'),
    ('\u{1ee51}', '\u{1ee52}'),
    ('\u{1ee54}', '\u{1ee54}'),
    ('\u{1ee57}', '\u{1ee57}'),
    ('\u{1ee59}', '\u{1ee59}'),
    ('\u{1ee5b}', '\u{1ee5b}'),
    ('\u{1ee5d}', '\u{1ee5d}'),
    ('\u{1ee5f}', '\u{1ee5f}'),
    ('\u{1ee61}', '\u{1ee62}'),
    ('\u{1ee64}', '\u{1ee64}'),
    ('\u{1ee67}', '\u{1ee6a}'),
    ('\u{1ee6c}', '\u{1ee72}'),
    ('\u{1ee74}', '\u{1ee77}'),
    ('\u{1ee79}', '\u{1ee7c}'),
    ('\u{1ee7e}', '\u{1ee7e}'),
    ('\u{1ee80}', '\u{1ee89}'),
    ('\u{1ee8b}', '\u{1ee9b}'),
    ('\u{1eea1}', '\u{1eea3}'),
    ('\u{1eea5}', '\u{1eea9}'),
    ('\u{1eeab}', '\u{1eebb}'),
    ('\u{1f100}', '\u{1f10c}'),
    ('\u{1fbf0}', '\u{1fbf9}'),
    ('\u{20000}', '\u{2a6df}'),
    ('\u{2a700}', '\u{2b739}'),
    ('\u{2b740}', '\u{2b81d}'),
    ('\u{2b820}', '\u{2cea1}'),
    ('\u{2ceb0}', '\u{2ebe0}'),
    ('\u{2ebf0}', '\u{2ee5d}'),
    ('\u{2f800}', '\u{2fa1d}'),
    ('\u{30000}', '\u{3134a}'),
    ('\u{31350}', '\u{323af}'),
];

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a fixture in a test is the assertion; a panic is the failure"
    )]

    use serde_json::Value;

    use super::*;

    const FIXTURES: &str = include_str!("../../tests/fixtures/difflib.json");

    /// The fixtures, parsed.
    fn fixtures() -> Value {
        serde_json::from_str(FIXTURES).expect("the fixtures parse")
    }

    /// The fixtures' list at `key`.
    fn cases<'v>(fixtures: &'v Value, key: &str) -> &'v [Value] {
        fixtures
            .get(key)
            .and_then(Value::as_array)
            .map_or_else(|| panic!("the fixtures hold no list {key}"), Vec::as_slice)
    }

    /// A list of strings.
    fn strings(value: &Value) -> Vec<String> {
        value
            .as_array()
            .expect("a list")
            .iter()
            .map(|item| item.as_str().expect("a string").to_owned())
            .collect()
    }

    /// The bytes that `hex`, two hex digits a byte, spells.
    fn bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).expect("a hex byte"))
            .collect()
    }

    /// A table of the fixtures, as `char` ranges.
    fn table(fixtures: &Value, key: &str) -> Vec<(char, char)> {
        cases(fixtures, key)
            .iter()
            .map(|range| {
                let ends: Vec<char> = range
                    .as_array()
                    .expect("a range")
                    .iter()
                    .map(|end| {
                        let point = u32::try_from(end.as_u64().expect("a code point"))
                            .expect("a code point");
                        char::from_u32(point).expect("a scalar value")
                    })
                    .collect();
                (ends[0], ends[1])
            })
            .collect()
    }

    /// A list of numbers, each read as an index.
    fn indices(value: &[Value]) -> Vec<usize> {
        value
            .iter()
            .map(|item| usize::try_from(item.as_u64().expect("a number")).expect("an index"))
            .collect()
    }

    /// Python's name of an opcode's tag.
    const fn tag_name(tag: Tag) -> &'static str {
        match tag {
            Tag::Replace => "replace",
            Tag::Delete => "delete",
            Tag::Insert => "insert",
            Tag::Equal => "equal",
        }
    }

    #[test]
    fn ratio_matches_cpython() {
        // Given: the 200 matcher cases captured from Python 3.13.5's
        // `SequenceMatcher(None, a, b, autojunk=False)`
        let fixtures = fixtures();
        let matcher_cases = cases(&fixtures, "matcher");
        assert_eq!(matcher_cases.len(), 200);

        for case in matcher_cases {
            // When: the port matches the same two token lists
            let (a, b) = (strings(&case["a"]), strings(&case["b"]));
            let matcher = SequenceMatcher::new(&a, &b);

            // Then: its matching blocks are Python's
            let blocks: Vec<Vec<usize>> = matcher
                .matching_blocks()
                .iter()
                .map(|block| vec![block.a, block.b, block.size])
                .collect();
            let expected_blocks: Vec<Vec<usize>> = cases(case, "matching_blocks")
                .iter()
                .map(|block| indices(block.as_array().expect("a block")))
                .collect();
            assert_eq!(blocks, expected_blocks, "{case}");

            // And: its opcodes are Python's
            let opcodes: Vec<(String, Vec<usize>)> = matcher
                .opcodes()
                .iter()
                .map(|opcode| {
                    let at = vec![opcode.a.start, opcode.a.end, opcode.b.start, opcode.b.end];
                    (tag_name(opcode.tag).to_owned(), at)
                })
                .collect();
            let expected: Vec<(String, Vec<usize>)> = cases(case, "opcodes")
                .iter()
                .map(|opcode| {
                    let (tag, at) = opcode
                        .as_array()
                        .map(Vec::as_slice)
                        .and_then(<[Value]>::split_first)
                        .expect("an opcode");
                    (tag.as_str().expect("a tag").to_owned(), indices(at))
                })
                .collect();
            assert_eq!(opcodes, expected, "{case}");

            // And: its ratio is the fraction that Python's float names:
            // twice the matching words over both lengths, 1 where both are
            // empty. Every fraction of these lengths lies more than 1e-4
            // from every other, so the float names one fraction, whatever
            // the last bit the JSON parser reads.
            let common: usize = expected_blocks.iter().map(|block| block[2]).sum();
            let length = a.len() + b.len();
            let fraction = if length == 0 {
                1.0
            } else {
                let twice = u32::try_from(2 * common).expect("a short case");
                f64::from(twice) / f64::from(u32::try_from(length).expect("a short case"))
            };
            let python = case["ratio"].as_f64().expect("a ratio");
            assert!((fraction - python).abs() < 1e-12, "{case}");
            let ratio = matcher.ratio();
            assert_eq!(ratio, Ratio::new(common, length), "{case}");

            // And: the quick ratios bound it from above, as Python's do
            assert!(matcher.quick_ratio() >= ratio, "{case}");
            assert!(
                real_quick_ratio(a.len(), b.len()) >= matcher.quick_ratio(),
                "{case}"
            );
        }
    }

    #[test]
    fn the_tables_are_cpythons() {
        // Given: the word and space ranges Python 3.13.5 reads
        let fixtures = fixtures();
        assert_eq!(fixtures["unicode_version"], "15.1.0");

        // When/Then: the tables hold the same ranges, 749 and 10 of them
        let words = table(&fixtures, "word_char_ranges");
        let spaces = table(&fixtures, "space_char_ranges");
        assert_eq!(words.len(), 749);
        assert_eq!(spaces.len(), 10);
        assert_eq!(WORD, words.as_slice());
        assert_eq!(SPACE, spaces.as_slice());
    }

    #[test]
    fn tokens_match_cpython() {
        // Given: the 37 lines Python's `re.findall(r"\w+|[^\w\s]", line)`
        // read: ASCII, Latin with a combining accent, Devanagari, Hebrew
        // points, the undertie, ZWJ, U+001C and U+001F, Roman numerals,
        // circled letters, Arabic-Indic digits, no-break and ideographic
        // spaces, CJK and emoji
        let fixtures = fixtures();
        let lines = cases(&fixtures, "tokens");
        assert_eq!(lines.len(), 37);

        for case in lines {
            // When: the line's words are read
            let line = case["line"].as_str().expect("a line");
            let words: Vec<&[u8]> = tokens(line.as_bytes());

            // Then: they are Python's
            let expected = strings(&case["tokens"]);
            let expected: Vec<&[u8]> = expected.iter().map(String::as_bytes).collect();
            assert_eq!(words, expected, "{line:?}");
        }
    }

    #[test]
    fn each_byte_that_is_not_utf8_is_a_word_of_its_own() {
        // Given: the 5 lines that are not valid UTF-8, which Python decodes
        // with `surrogateescape`
        let fixtures = fixtures();
        let lines = cases(&fixtures, "byte_tokens");
        assert_eq!(lines.len(), 5);

        for case in lines {
            // When: the line's words are read
            let line = bytes(case["line_hex"].as_str().expect("a line"));
            let words: Vec<&[u8]> = tokens(&line);

            // Then: they are the bytes of Python's words
            let expected: Vec<Vec<u8>> = strings(&case["tokens_hex"])
                .iter()
                .map(|hex| bytes(hex))
                .collect();
            assert_eq!(words, expected, "{line:?}");
        }
    }

    #[test]
    fn pairs_as_the_prototype_pairs() {
        // Given: the 6 hunks the prototype's own `pair_hunk` paired
        let fixtures = fixtures();
        let hunks = cases(&fixtures, "pairing");
        assert_eq!(hunks.len(), 6);

        for case in hunks {
            // When: the port pairs the same hunk
            let removed = strings(&case["removed"]);
            let added = strings(&case["added"]);
            let removed: Vec<&[u8]> = removed.iter().map(String::as_bytes).collect();
            let added: Vec<&[u8]> = added.iter().map(String::as_bytes).collect();
            let pairs = pair_hunk(&removed, &added);

            // Then: each added line pairs with the prototype's removed line
            let mut expected = vec![None; added.len()];
            let map = case["pairs_added_to_removed"].as_object().expect("a map");
            for (at, from) in map {
                let at: usize = at.parse().expect("an added offset");
                let from = usize::try_from(from.as_u64().expect("a removed offset"))
                    .expect("a removed offset");
                expected[at] = Some(from);
            }
            assert_eq!(pairs, expected, "{case}");
        }
    }

    #[test]
    fn a_changed_line_names_the_words_it_added_and_removed() {
        // Given: `call(a, insecure=True)` changed to `call(a, timeout=5)`
        let old = tokens(b"call(a, insecure=True)");
        let new = tokens(b"call(a, timeout=5)");

        // When: the word diff of the two lines is read
        let own = line_word_diff(&old, &new);

        // Then: the new line's `timeout` and `5` are the member's, and the
        // old line's `insecure` and `True` are what it removed
        let added: Vec<&[u8]> = own.added.iter().map(|&at| new[at]).collect();
        assert_eq!(added, [b"timeout".as_slice(), b"5"]);
        assert_eq!(own.removed, [b"insecure".as_slice(), b"True"]);
    }
}
