//! A port of Python's `difflib.SequenceMatcher` (3.13) with no junk and
//! `autojunk=False`: no element of the second sequence is ever left out as
//! junk or as a popular element.
//!
//! Spec R9's Design defines the word similarity of two lines as
//! `SequenceMatcher.ratio` computes it, and the prototype that the spec
//! takes ran this matcher for each of its word diffs. So the port keeps
//! Python's tie-breaking: of the longest matches, the one that starts first
//! in `a`, and of those the one that starts first in `b`. The unit tests of
//! [`super::words`] hold it to cases captured from Python 3.13.5
//! (`tests/fixtures/difflib.json`).
//!
//! A ratio is an exact fraction ([`Ratio`]). Python computes it as a float,
//! `2.0 * matches / length`. Two different fractions of the lengths that
//! this rule meets lie much further apart than the float's rounding error,
//! so a comparison of the fractions gives the answer that a comparison of
//! Python's floats gives.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;
use std::ops::Range;

#[cfg(test)]
thread_local! {
    /// The lengths of the two sequences of each matcher made on this
    /// thread, in the order they were made ([`made`]). Tests run on threads
    /// of their own, so each test reads its own matchers.
    static MADE: std::cell::RefCell<Vec<(usize, usize)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The lengths of the two sequences of each matcher made on this thread
/// since the last call, which takes them: what a test reads to see which
/// lines a reading compared.
#[cfg(test)]
pub(super) fn made() -> Vec<(usize, usize)> {
    MADE.take()
}

/// `2 * matches / length`, as Python's `difflib` computes a ratio, kept as
/// an exact fraction; 1 where both sequences are empty.
#[derive(Debug, Clone, Copy)]
pub(super) struct Ratio {
    numerator: u64,
    /// Never 0.
    denominator: u64,
}

impl Ratio {
    /// One half: the least word similarity at which spec R9 pairs a removed
    /// line with an added one.
    pub(super) const HALF: Self = Self {
        numerator: 1,
        denominator: 2,
    };

    /// The ratio of `matches` matching elements in two sequences whose
    /// lengths add up to `length`.
    pub(super) fn new(matches: usize, length: usize) -> Self {
        if length == 0 {
            return Self {
                numerator: 1,
                denominator: 1,
            };
        }
        Self {
            numerator: count(matches).saturating_mul(2),
            denominator: count(length),
        }
    }
}

impl PartialEq for Ratio {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Ratio {}

impl PartialOrd for Ratio {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Ratio {
    fn cmp(&self, other: &Self) -> Ordering {
        let ours = u128::from(self.numerator) * u128::from(other.denominator);
        let theirs = u128::from(other.numerator) * u128::from(self.denominator);
        ours.cmp(&theirs)
    }
}

/// `n` as a `u64`. No sequence in memory is longer.
fn count(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

/// A run of equal elements: `a[a..a + size] == b[b..b + size]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Match {
    pub(super) a: usize,
    pub(super) b: usize,
    pub(super) size: usize,
}

/// What an opcode does to turn `a` into `b`, as Python's `get_opcodes`
/// names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tag {
    Replace,
    Delete,
    Insert,
    Equal,
}

/// One step of turning `a` into `b`: `a[a]` becomes `b[b]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Opcode {
    pub(super) tag: Tag,
    pub(super) a: Range<usize>,
    pub(super) b: Range<usize>,
}

/// Python's `SequenceMatcher(None, a, b, autojunk=False)`.
#[derive(Debug)]
pub(super) struct SequenceMatcher<'s, T> {
    a: &'s [T],
    b: &'s [T],
    /// Each element of `b`, with every index that holds it, in ascending
    /// order: Python's `b2j`, which nothing prunes here.
    b2j: HashMap<&'s T, Vec<usize>>,
}

impl<'s, T: Eq + Hash> SequenceMatcher<'s, T> {
    pub(super) fn new(a: &'s [T], b: &'s [T]) -> Self {
        #[cfg(test)]
        MADE.with_borrow_mut(|made| made.push((a.len(), b.len())));
        let mut b2j: HashMap<&'s T, Vec<usize>> = HashMap::new();
        for (j, element) in b.iter().enumerate() {
            b2j.entry(element).or_default().push(j);
        }
        Self { a, b, b2j }
    }

    /// Python's `find_longest_match` over `a[window_a]` and `b[window_b]`:
    /// the longest run of equal elements, the one that starts first in `a`
    /// where several are as long, and of those the one that starts first in
    /// `b`. A run of size 0 starts at both windows' starts.
    ///
    /// Python then extends the run it found over equal elements at each
    /// end, and over junk. With no junk, the run is already as long as it
    /// can be: an equal element before it, or after it, inside the windows,
    /// would have made the dynamic program find a longer run. So those loops
    /// never move it here, and the port leaves them out.
    fn find_longest_match(
        &self,
        window_a: Range<usize>,
        window_b: Range<usize>,
        runs: &mut Runs,
    ) -> Match {
        let mut best = Match {
            a: window_a.start,
            b: window_b.start,
            size: 0,
        };
        for i in window_a {
            let at = self.a.get(i).and_then(|element| self.b2j.get(element));
            let list = at.map_or(&[][..], Vec::as_slice);
            // `list` is ascending: skip the indices before the window.
            let skip = list.partition_point(|&j| j < window_b.start);
            for &j in list.get(skip..).unwrap_or_default() {
                if j >= window_b.end {
                    break;
                }
                let size = runs.ending_before(j) + 1;
                runs.set(j, size);
                if size > best.size {
                    best = Match {
                        a: i + 1 - size,
                        b: j + 1 - size,
                        size,
                    };
                }
            }
            runs.next_row();
        }
        // The last row's runs, cleared for the next call.
        runs.next_row();
        best
    }

    /// Python's `get_matching_blocks`: the runs of equal elements, in order,
    /// adjacent runs joined, and the sentinel `(len(a), len(b), 0)` last.
    pub(super) fn matching_blocks(&self) -> Vec<Match> {
        let (la, lb) = (self.a.len(), self.b.len());
        let mut runs = Runs::new(lb);
        let mut queue = vec![(0..la, 0..lb)];
        let mut found = Vec::new();
        while let Some((window_a, window_b)) = queue.pop() {
            let longest = self.find_longest_match(window_a.clone(), window_b.clone(), &mut runs);
            if longest.size == 0 {
                continue;
            }
            found.push(longest);
            if window_a.start < longest.a && window_b.start < longest.b {
                queue.push((window_a.start..longest.a, window_b.start..longest.b));
            }
            let (a_end, b_end) = (longest.a + longest.size, longest.b + longest.size);
            if a_end < window_a.end && b_end < window_b.end {
                queue.push((a_end..window_a.end, b_end..window_b.end));
            }
        }
        found.sort_unstable();
        let mut blocks = Vec::new();
        let mut current = Match {
            a: 0,
            b: 0,
            size: 0,
        };
        for block in found {
            if current.a + current.size == block.a && current.b + current.size == block.b {
                current.size += block.size;
            } else {
                if current.size > 0 {
                    blocks.push(current);
                }
                current = block;
            }
        }
        if current.size > 0 {
            blocks.push(current);
        }
        blocks.push(Match {
            a: la,
            b: lb,
            size: 0,
        });
        blocks
    }

    /// Python's `get_opcodes`: the steps that turn `a` into `b`, in order.
    pub(super) fn opcodes(&self) -> Vec<Opcode> {
        let mut opcodes = Vec::new();
        let (mut i, mut j) = (0, 0);
        for block in self.matching_blocks() {
            let tag = match (i < block.a, j < block.b) {
                (true, true) => Some(Tag::Replace),
                (true, false) => Some(Tag::Delete),
                (false, true) => Some(Tag::Insert),
                (false, false) => None,
            };
            if let Some(tag) = tag {
                opcodes.push(Opcode {
                    tag,
                    a: i..block.a,
                    b: j..block.b,
                });
            }
            (i, j) = (block.a + block.size, block.b + block.size);
            if block.size > 0 {
                opcodes.push(Opcode {
                    tag: Tag::Equal,
                    a: block.a..i,
                    b: block.b..j,
                });
            }
        }
        opcodes
    }

    /// Python's `ratio`: twice the matching elements over both lengths.
    pub(super) fn ratio(&self) -> Ratio {
        let matches = self.matching_blocks().iter().map(|block| block.size).sum();
        Ratio::new(matches, self.a.len() + self.b.len())
    }

    /// Python's `quick_ratio`: an upper bound on [`Self::ratio`], from the
    /// elements that the two sequences hold, in any order.
    pub(super) fn quick_ratio(&self) -> Ratio {
        let mut available: HashMap<&T, usize> = self
            .b2j
            .iter()
            .map(|(element, at)| (*element, at.len()))
            .collect();
        let mut matches = 0;
        for element in self.a {
            if let Some(left) = available.get_mut(element).filter(|left| **left > 0) {
                *left -= 1;
                matches += 1;
            }
        }
        Ratio::new(matches, self.a.len() + self.b.len())
    }
}

/// Python's `real_quick_ratio` of two sequences `a_len` and `b_len` long:
/// an upper bound on [`SequenceMatcher::quick_ratio`], from the two lengths
/// alone, so it needs no matcher.
pub(super) fn real_quick_ratio(a_len: usize, b_len: usize) -> Ratio {
    Ratio::new(a_len.min(b_len), a_len + b_len)
}

/// The run lengths of [`SequenceMatcher::find_longest_match`]'s dynamic
/// program: for each index `j` of `b`, the length of the run that ends at
/// `b[j]` and at the element of `a` before the one being read (`before`),
/// and at the one being read (`now`); 0 where no run ends there. Python
/// keeps the two rows in two dicts. Each row here remembers the indices it
/// set, so clearing it costs what filling it cost, and one pair of rows
/// serves every call of one [`SequenceMatcher::matching_blocks`].
struct Runs {
    before: Vec<usize>,
    now: Vec<usize>,
    set_before: Vec<usize>,
    set_now: Vec<usize>,
}

impl Runs {
    /// Rows for a `b` of `length` elements, every run 0.
    fn new(length: usize) -> Self {
        Self {
            before: vec![0; length],
            now: vec![0; length],
            set_before: Vec::new(),
            set_now: Vec::new(),
        }
    }

    /// The length of the run that ends at `b[j - 1]` in the row before; 0
    /// for `j == 0`.
    fn ending_before(&self, j: usize) -> usize {
        j.checked_sub(1)
            .and_then(|at| self.before.get(at))
            .copied()
            .unwrap_or(0)
    }

    /// Records a run of `size` that ends at `b[j]` in the row being read.
    fn set(&mut self, j: usize, size: usize) {
        if let Some(slot) = self.now.get_mut(j) {
            *slot = size;
            self.set_now.push(j);
        }
    }

    /// Ends the row being read: it becomes the row before, and the row
    /// before it is cleared.
    fn next_row(&mut self) {
        for &j in &self.set_before {
            if let Some(slot) = self.before.get_mut(j) {
                *slot = 0;
            }
        }
        self.set_before.clear();
        std::mem::swap(&mut self.before, &mut self.now);
        std::mem::swap(&mut self.set_before, &mut self.set_now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratios_compare_as_fractions() {
        // Given: ratios of different lengths
        let one_half = Ratio::new(1, 4);
        let two_quarters = Ratio::new(2, 8);
        let more = Ratio::new(3, 10);

        // When/Then: equal fractions are equal, and a larger one is larger
        assert_eq!(one_half, Ratio::HALF);
        assert_eq!(one_half, two_quarters);
        assert!(more > Ratio::HALF);
        assert!(Ratio::new(0, 3) < Ratio::HALF);
        // And: two empty sequences are alike, as Python reads them
        assert_eq!(Ratio::new(0, 0), Ratio::new(1, 2));
    }
}
