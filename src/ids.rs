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

/// Whether a release reference is one of ours.
///
/// `repos` and `status` each grew their own version of this, and `repos`
/// promptly picked an upstream release as ours. Only local releases and the
/// `origin` or `release` remotes are ours: `upstream` is somebody else's cut,
/// while `git` is jj's internal tracking view rather than a remote.
/// Naming is scheme-dependent: dated releases share a prefix, while fixed releases
/// are the one configured branch.
pub fn is_our_release(reference: &BookmarkRef, scheme: &ReleaseScheme) -> bool {
    if !is_release_name(reference.branch(), scheme) {
        return false;
    }
    match reference {
        BookmarkRef::Local(_) => true,
        BookmarkRef::Remote { remote, .. } => matches!(remote.as_str(), "origin" | "release"),
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
    pub const fn branch(&self) -> &BranchName {
        match self {
            Self::Local(branch) | Self::Remote { branch, .. } => branch,
        }
    }

    pub const fn is_local(&self) -> bool {
        matches!(self, Self::Local(_))
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
        // The fixed branch is a cut wherever we publish it, never on upstream.
        assert!(is_our_release(&local("integration"), &fixed));
        assert!(is_our_release(&remote("integration", "origin"), &fixed));
        assert!(is_our_release(&remote("integration", "release"), &fixed));
        assert!(!is_our_release(&remote("integration", "upstream"), &fixed));
        assert!(!is_our_release(&remote("integration", "git"), &fixed));
        // Under Fixed, a dated name is not one of this repo's releases.
        assert!(!is_our_release(&local("release/2026-07-29"), &fixed));
        // Dated behavior is unchanged.
        assert!(is_our_release(
            &local("release/2026-07-29"),
            &ReleaseScheme::Dated
        ));
        assert!(!is_our_release(
            &local("integration"),
            &ReleaseScheme::Dated
        ));
    }
}
