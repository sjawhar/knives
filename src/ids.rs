//! Semantic identifiers.
//!
//! Each of these is a distinct type rather than a `String` because mixing them
//! is a real bug class in this domain, not a hypothetical one. A change id and a
//! commit id look identical, are both short hex-ish strings, and mean entirely
//! different things: one change can exist as several commits, which is exactly
//! what divergence is. Likewise a local bookmark and its remote-tracking
//! counterpart share a name but point at different commits.

use std::fmt;

macro_rules! string_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

string_id!(
    ChangeId,
    "A jj change. Stable across rewrites, and identical across disconnected clones, which is why the same change rewritten in two places collides."
);
string_id!(CommitId, "One concrete commit. A change may have several.");
string_id!(RepoName, "A managed repo's name in the registry.");
string_id!(
    RemoteName,
    "A remote's name, which this tool only ever derives from a role."
);
string_id!(
    BranchName,
    "A bookmark name with no remote qualifier and no decoration."
);
string_id!(WorkspaceName, "A jj workspace.");

impl ChangeId {
    /// The prefix this program shows; see [`short_id`].
    pub fn short(&self) -> &str {
        short_id(&self.0)
    }
}

impl CommitId {
    /// The prefix this program shows; see [`short_id`].
    pub fn short(&self) -> &str {
        short_id(&self.0)
    }
}

impl BranchName {
    /// A branch name as typed on the command line, checked to name a bookmark.
    ///
    /// `start`, `finish`, `track` and `depends` derive a workspace directory from
    /// the name, so a name that is not a bookmark is a path: before the workspace
    /// identity check, `finish ..` flattened to the checkout's grandparent and
    /// `finish ''` to its parent, and removed them. Rejected here so the degenerate
    /// spellings never reach a path at all and the message is knives', not jj's.
    pub fn parse(value: &str) -> Result<Self, String> {
        if value.is_empty() {
            return Err("a branch name is required".to_owned());
        }
        if value.starts_with('-') {
            return Err(format!(
                "{value:?} is not a branch name; it reads as an option"
            ));
        }
        if value.split('/').any(|segment| {
            segment.is_empty() || segment == "." || segment == ".." || segment.starts_with('.')
        }) {
            return Err(format!(
                "{value:?} is not a branch name: every `/`-separated segment must be non-empty \
                 and must not start with `.`"
            ));
        }
        Ok(Self::new(value))
    }
}

/// The first twelve characters of an identifier: what jj shows, enough to be
/// unique in a fork, short enough to read in a line of prose. A shorter id is
/// returned whole.
///
/// For an id the program holds as text — a forge oid, a lockfile lock, a
/// recorded anchor. [`CommitId::short`] and [`ChangeId::short`] are this for
/// the ids it has typed. Only identifiers are shortened this way: a truncated
/// branch name or file path names something that does not exist.
pub fn short_id(id: &str) -> &str {
    id.char_indices().nth(12).map_or(id, |(end, _)| &id[..end])
}

/// A branch in a particular repo.
///
/// These two always travel together: every claim, mark, and supersession is
/// keyed by the pair. Passing them separately duplicated the key formatting
/// across six store methods and pushed several signatures past four arguments.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub struct BranchTarget {
    pub repo: RepoName,
    pub branch: BranchName,
}

impl BranchTarget {
    pub const fn new(repo: RepoName, branch: BranchName) -> Self {
        Self { repo, branch }
    }
}

impl fmt::Display for BranchTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.repo, self.branch)
    }
}

/// A bookmark as it appears on a commit: local, or tracking a remote.
///
/// Keeping these apart in the type system is the fix for a bug that shipped in
/// an earlier draft. `jj` renders both under one name, so a tip map keyed by
/// bare string silently took whichever row came last. On a real repo the local
/// and remote tips genuinely differed, so release parents were compared against
/// the wrong commit.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
pub enum BookmarkRef {
    Local(BranchName),
    Remote {
        branch: BranchName,
        remote: RemoteName,
    },
}

pub const RELEASE_PREFIX: &str = "release/";

/// Whether a release reference is one of ours for the configured publishing role.
///
/// Local releases are always ours. Remote releases are ours only when their
/// remote is the configured publish remote: an `origin` release-shaped ref is
/// a misplaced branch, rather than a release, in a split-release repository.
/// `upstream` remains somebody else's cut, while `git` is jj's internal tracking
/// view rather than a remote.
/// Naming is scheme-dependent: dated releases share a prefix, while fixed releases
/// are the one configured branch.
pub fn is_our_release(
    reference: &BookmarkRef,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> bool {
    if !is_release_name(reference.branch(), scheme) {
        return false;
    }
    match reference {
        BookmarkRef::Local(_) => true,
        BookmarkRef::Remote { remote, .. } => remote.as_str() == publish_remote,
    }
}

/// How this fork names its releases.
///
/// Derived from configuration and matched exhaustively at every release-aware
/// site, so the compiler forces each of them — including ones added later — to
/// answer "what does this mean when the release is one fixed branch?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseScheme {
    /// Dated `release/YYYY-MM-DD[.n]` cuts. The default, and the historical behavior.
    Dated,
    /// One integration branch that is rebuilt and advanced in place. The branch's
    /// previous position plays the role of the previous release.
    Fixed(BranchName),
}

/// Whether a branch name is a release under this scheme: the dated prefix, or the
/// one fixed integration branch.
pub fn is_release_name(branch: &BranchName, scheme: &ReleaseScheme) -> bool {
    match scheme {
        ReleaseScheme::Dated => branch.as_str().starts_with(RELEASE_PREFIX),
        ReleaseScheme::Fixed(name) => branch == name,
    }
}

/// Parse `release/YYYY-MM-DD[.N]`, the one shape our dated cuts take.
///
/// Stricter than [`is_release_name`] on purpose: the reaper enumerates release
/// refs on any remote, where upstream's own `release/0.3.190` style branches
/// also live, and a prefix test would hand those to `bookmark forget`. Returns
/// the `(date, suffix)` ordering key so "newest" is one `max_by_key` away.
/// The suffix follows `u32::from_str` semantics rather than strict digits, so
/// leading `+` signs and zeroes parse; every such name is still ours by shape,
/// leaving the reap safety property unaffected.
pub fn strict_dated_release(name: &str) -> Option<(String, u32)> {
    let bare = name.strip_prefix(RELEASE_PREFIX)?;
    let (date, suffix) = match bare.split_once('.') {
        Some((date, suffix)) => (date, suffix.parse::<u32>().ok()?),
        None => (bare, 0),
    };
    let bytes = date.as_bytes();
    let dated = bytes.len() == 10
        && bytes.get(4) == Some(&b'-')
        && bytes.get(7) == Some(&b'-')
        && date
            .bytes()
            .enumerate()
            .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit());
    dated.then(|| (date.to_owned(), suffix))
}

impl BookmarkRef {
    /// The inverse of `Display`: `branch@remote` is a remote ref, anything else
    /// a local one. One parser for the grammar, so a display form crossing a
    /// module boundary as a string comes back as the same value.
    pub fn parse(text: &str) -> Self {
        match text.rsplit_once('@') {
            Some((branch, remote)) if !branch.is_empty() && !remote.is_empty() => Self::Remote {
                branch: BranchName::new(branch),
                remote: RemoteName::new(remote),
            },
            _ => Self::Local(BranchName::new(text)),
        }
    }

    pub const fn branch(&self) -> &BranchName {
        match self {
            Self::Local(branch) | Self::Remote { branch, .. } => branch,
        }
    }

    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
    }

    /// Whether this is jj's own `@git` tracking view of a bookmark rather than
    /// a ref on a remote. It mirrors the local bookmark and names nothing of its
    /// own, so it is not a remote a branch can be fetched from or exist only on.
    pub fn is_git_view(&self) -> bool {
        matches!(self, Self::Remote { remote, .. } if remote.is_git_view())
    }
}

impl RemoteName {
    /// Whether this names jj's `git` tracking view rather than a remote.
    pub fn is_git_view(&self) -> bool {
        self.as_str() == "git"
    }
}

impl fmt::Display for BookmarkRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(branch) => write!(f, "{branch}"),
            Self::Remote { branch, remote } => write!(f, "{branch}@{remote}"),
        }
    }
}

/// The pull request number a `pr-<n>` bookmark refers to.
///
/// Fetching a pull request head creates a bookmark named for its number, not for
/// the branch the pull request came from. Matching pull requests by branch name
/// therefore never found them, so every fetched head was reported as a branch with
/// no pull request, advising us to open one for a pull request that was already
/// open. On one real repository that was 17 of 89 findings.
pub fn pull_number_from_bookmark(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("pr-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Something a branch cannot land before: a pull request in some managed repo.
///
/// Written `<repo>#<number>`. The repo is named because dependencies cross forks,
/// which is the case that motivated this: a change in one fork needing a pull request
/// in a sibling, where dropping one from a release without the other ships a release
/// that cannot work.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct Requirement {
    pub repo: RepoName,
    pub number: u64,
}

impl Requirement {
    pub fn parse(text: &str) -> Option<Self> {
        let (repo, number) = text.split_once('#')?;
        if repo.is_empty() {
            return None;
        }
        Some(Self {
            repo: RepoName::new(repo),
            number: number.parse().ok()?,
        })
    }
}

impl std::fmt::Display for Requirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.repo, self.number)
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    #[test]
    fn a_requirement_round_trips_and_refuses_nonsense() {
        use super::Requirement;
        let parsed = Requirement::parse("swe#4545").unwrap();
        assert_eq!(parsed.repo.as_str(), "swe");
        assert_eq!(parsed.number, 4545);
        assert_eq!(parsed.to_string(), "swe#4545");
        assert!(Requirement::parse("swe").is_none());
        assert!(Requirement::parse("#4545").is_none());
        assert!(Requirement::parse("swe#abc").is_none());
    }

    #[test]
    fn a_branch_name_from_the_command_line_must_name_a_bookmark() {
        use super::BranchName;
        // Given: names a shell can hand over that are not bookmarks. Before the
        // workspace identity check, `finish ..` flattened to the checkout's
        // grandparent and `finish ''` to its parent — and removed them.
        for nonsense in ["", ".", "..", "-r", "feat/../x", "a//b", "feat/.hidden/x"] {
            assert!(
                BranchName::parse(nonsense).is_err(),
                "{nonsense:?} was accepted"
            );
        }
        for name in ["main", "feat/alpha", "release/2026-08-15.1", "fix-1", "a.b"] {
            assert_eq!(
                BranchName::parse(name).map(|branch| branch.as_str().to_owned()),
                Ok(name.to_owned())
            );
        }
    }
    #[test]
    fn a_fetched_pull_request_head_names_its_number() {
        use super::pull_number_from_bookmark;
        assert_eq!(pull_number_from_bookmark("pr-4671"), Some(4671));
        assert_eq!(pull_number_from_bookmark("pr-"), None);
        assert_eq!(pull_number_from_bookmark("pr-abc"), None);
        // A real branch that merely starts with the same letters is not a head.
        assert_eq!(pull_number_from_bookmark("pr-fix/thing"), None);
        assert_eq!(pull_number_from_bookmark("feat/alpha"), None);
    }

    #[test]
    fn short_id_takes_twelve_characters_and_never_splits_one() {
        use super::{CommitId, short_id};
        // Given: a full commit id, an already-short one, and text a notch
        // anchor could carry after sanitising: a multibyte character at the cut,
        // which a byte slice would split.
        assert_eq!(short_id("0123456789abcdef0123"), "0123456789ab");
        assert_eq!(short_id("0123456789ab"), "0123456789ab");
        assert_eq!(short_id("abc"), "abc");
        assert_eq!(short_id(""), "");
        assert_eq!(short_id("0123456789a\u{fffd}bc"), "0123456789a\u{fffd}");
        // Then: a typed id shortens the same way, without allocating.
        assert_eq!(
            CommitId::new("0123456789abcdef0123").short(),
            "0123456789ab"
        );
    }

    #[test]
    fn only_our_dated_shape_parses_as_a_dated_release() {
        use super::strict_dated_release;
        // Ours, with and without a same-day suffix.
        assert_eq!(
            strict_dated_release("release/2026-08-05"),
            Some(("2026-08-05".to_owned(), 0))
        );
        assert_eq!(
            strict_dated_release("release/2026-08-05.2"),
            Some(("2026-08-05".to_owned(), 2))
        );
        // Upstream's semver release branches are NOT ours to reap.
        assert_eq!(strict_dated_release("release/0.3.190"), None);
        // Shape violations.
        assert_eq!(strict_dated_release("release/"), None);
        assert_eq!(strict_dated_release("release/2026-8-5"), None);
        assert_eq!(strict_dated_release("release/2026-08-05."), None);
        assert_eq!(strict_dated_release("release/2026-08-05.x"), None);
        assert_eq!(strict_dated_release("release/2026-08-05.1.2"), None);
        assert_eq!(strict_dated_release("feat/2026-08-05"), None);
    }

    use super::*;

    #[test]
    fn a_local_and_a_remote_bookmark_of_one_branch_are_different_values() {
        // Given: one branch name reachable two ways
        let branch = BranchName::new("feat/alpha");
        // When: it is expressed as a local ref and as a remote-tracking ref
        let local = BookmarkRef::Local(branch.clone());
        let remote = BookmarkRef::Remote {
            branch,
            remote: RemoteName::new("origin"),
        };
        // Then: they are distinct, so a map cannot silently collapse them
        assert_ne!(local, remote);
        assert_eq!(local.branch(), remote.branch());
        assert!(local.is_local());
        assert!(!remote.is_local());
    }

    #[test]
    fn a_remote_bookmark_displays_with_its_remote() {
        let reference = BookmarkRef::Remote {
            branch: BranchName::new("feat/alpha"),
            remote: RemoteName::new("origin"),
        };
        assert_eq!(reference.to_string(), "feat/alpha@origin");
    }

    #[test]
    fn under_a_fixed_scheme_the_fixed_branch_is_the_release_and_dated_names_are_not() {
        use super::{BookmarkRef, BranchName, ReleaseScheme, RemoteName, is_our_release};
        let fixed = ReleaseScheme::Fixed(BranchName::new("integration"));
        let local = |name: &str| BookmarkRef::Local(BranchName::new(name));
        let remote = |name: &str, r: &str| BookmarkRef::Remote {
            branch: BranchName::new(name),
            remote: RemoteName::new(r),
        };
        // Local releases are ours; tracked refs are ours only on the role that
        // publishes them.
        assert!(is_our_release(&local("integration"), &fixed, "release"));
        assert!(is_our_release(
            &remote("integration", "origin"),
            &fixed,
            "origin"
        ));
        assert!(is_our_release(
            &remote("integration", "release"),
            &fixed,
            "release"
        ));
        assert!(!is_our_release(
            &remote("integration", "origin"),
            &fixed,
            "release"
        ));
        assert!(!is_our_release(
            &remote("integration", "upstream"),
            &fixed,
            "origin"
        ));
        assert!(!is_our_release(
            &remote("integration", "git"),
            &fixed,
            "origin"
        ));
        // Under Fixed, a dated name is not one of this repo's releases.
        assert!(!is_our_release(
            &local("release/2026-07-29"),
            &fixed,
            "origin"
        ));
        // Dated behavior is unchanged.
        assert!(is_our_release(
            &local("release/2026-07-29"),
            &ReleaseScheme::Dated,
            "origin"
        ));
        assert!(!is_our_release(
            &local("integration"),
            &ReleaseScheme::Dated,
            "origin"
        ));
    }
}
