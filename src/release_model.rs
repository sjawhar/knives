//! Release-domain rules shared by command verbs and reports.
//!
//! This module owns facts about release names, recorded cuts, and consumer pins.
//! Commands gather their I/O and render their answers around these rules.

use std::collections::BTreeSet;
use std::path::Path;

use crate::config::{RepoEntry, Role};
use crate::detect::{BookmarkTips, Finding, FindingKind, Subject};
use crate::ids::{
    BookmarkRef, CommitId, RELEASE_PREFIX, ReleaseScheme, is_our_release, is_release_name,
};
use crate::jj::{self, OriginTrunk, Repo};
use crate::ledger::{Entry, Kind};
use crate::pins::{PIN_FILES, Pin, scan};

/// Scan evidence for one consumer checkout.
#[derive(Debug, Default)]
pub struct ConsumerScan {
    pub pins: Vec<Pin>,
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

struct ConsumerPinScope<'a> {
    slug: Option<&'a str>,
    scheme: &'a ReleaseScheme,
}

/// Scan a consumer checkout for pins of one repo's releases.
///
/// Scoped by `slug`, the repository's name as it appears in a dependency line. These
/// forks cut releases on one dated scheme, so `release/2026-07-28` exists in several of
/// them at once; an unscoped scan attributed a sibling's pin to this repo, which reads
/// as "pinned at the newest cut" when it is not pinned here at all. `None` keeps every
/// pin, for a caller that genuinely wants the whole file.
pub fn scan_consumer_for(
    consumer: &Path,
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) -> ConsumerScan {
    let mut result = ConsumerScan::default();
    let scope = ConsumerPinScope { slug, scheme };
    match jj::origin_trunk(consumer) {
        Ok(OriginTrunk::Reference(branch)) => {
            let mut checkout_lag = None;
            for name in PIN_FILES {
                match jj::file_at_ref(consumer, &branch, name) {
                    Ok(Some((text, behind))) => {
                        extend_scanned_pins(&mut result, name, &text, &scope);
                        checkout_lag = checkout_lag.or_else(|| (behind > 0).then_some(behind));
                    }
                    Ok(None) => {}
                    Err(error) => result
                        .problems
                        .push(format!("could not read {name} at {branch}: {error}")),
                }
            }
            if let Some(behind) = checkout_lag {
                result.notes.push(format!(
                    "{} checkout is {behind} commit(s) behind its {branch}",
                    consumer.display()
                ));
            }
        }
        Ok(OriginTrunk::Missing) => {
            extend_working_copy_pins(&mut result, consumer, &scope);
            result.notes.push(format!(
                "{}: no origin trunk resolved; pins read from the working copy",
                consumer.display()
            ));
        }
        Ok(OriginTrunk::NotRepository) => {
            extend_working_copy_pins(&mut result, consumer, &scope);
            result.notes.push(format!(
                "{}: not a repository; pins read from the working copy",
                consumer.display()
            ));
        }
        Err(error) => {
            extend_working_copy_pins(&mut result, consumer, &scope);
            result.notes.push(format!(
                "{}: could not resolve its origin trunk ({error}); pins read from the working copy",
                consumer.display()
            ));
            result
                .problems
                .push(format!("could not resolve origin trunk: {error}"));
        }
    }
    result
}

fn extend_working_copy_pins(
    result: &mut ConsumerScan,
    consumer: &Path,
    scope: &ConsumerPinScope<'_>,
) {
    for name in PIN_FILES {
        match std::fs::read_to_string(consumer.join(name)) {
            Ok(text) => extend_scanned_pins(result, name, &text, scope),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => result
                .problems
                .push(format!("could not read {name}: {error}")),
        }
    }
}

fn extend_scanned_pins(
    result: &mut ConsumerScan,
    file: &str,
    text: &str,
    scope: &ConsumerPinScope<'_>,
) {
    let parsed = scan(file, text, scope.scheme);
    result.pins.extend(
        parsed
            .pins
            .into_iter()
            .filter(|pin| scope.slug.is_none_or(|slug| pin.source.contains(slug))),
    );
    result.problems.extend(
        parsed
            .problems
            .into_iter()
            .filter(|problem| scope.slug.is_none_or(|slug| problem.source.contains(slug)))
            .map(|problem| problem.to_string()),
    );
}

/// The repository's name as it appears in a dependency line, e.g. `sandbox-runner`.
pub fn repo_slug(entry: &RepoEntry) -> Option<String> {
    let last = entry.remote(Role::Origin).rsplit('/').next()?;
    let trimmed = last.trim_end_matches(".git");
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The release the next cut carries: the local composition in hand, preferred
/// over the publish remote so unpushed release edits remain part of the cut.
pub fn previous_release_for_cut(
    entry: &RepoEntry,
    tips: &BookmarkTips,
) -> Option<(String, CommitId)> {
    let scheme = entry.release_scheme();
    newest_release(tips, &scheme, entry.publish_remote())
        .map(|(reference, commit)| (reference.to_string(), commit))
}

/// Every locally held, non-release, non-trunk branch and its current tip.
pub fn carried_branches(
    repo: &Repo,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> anyhow::Result<Vec<(String, CommitId)>> {
    Ok(carried_from_tips(&repo.bookmark_tips()?, trunk, scheme))
}

/// Every locally held, non-release, non-trunk branch and its current tip.
pub fn carried_from_tips(
    tips: &BookmarkTips,
    trunk: &str,
    scheme: &ReleaseScheme,
) -> Vec<(String, CommitId)> {
    tips.iter()
        .filter_map(|(reference, commit)| match reference {
            BookmarkRef::Local(branch)
                if !is_release_name(branch, scheme) && branch.as_str() != trunk =>
            {
                Some((branch.to_string(), commit.clone()))
            }
            BookmarkRef::Local(_) | BookmarkRef::Remote { .. } => None,
        })
        .collect()
}

/// The newest release under the configured scheme and publish remote.
pub fn newest_release(
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> Option<(BookmarkRef, CommitId)> {
    match scheme {
        ReleaseScheme::Dated => tips
            .iter()
            .filter(|(reference, _)| is_our_release(reference, scheme, publish_remote))
            .max_by_key(|(reference, _)| {
                (
                    release_order(reference.branch().as_str()),
                    u8::from(reference.is_local()),
                )
            })
            .map(|(reference, commit)| (reference.clone(), commit.clone())),
        ReleaseScheme::Fixed(fixed) => tips
            .iter()
            .filter(|(reference, _)| match reference {
                BookmarkRef::Local(branch) => branch == fixed,
                BookmarkRef::Remote { branch, remote } => {
                    branch == fixed && remote.as_str() == publish_remote
                }
            })
            .max_by_key(|(reference, _)| u8::from(reference.is_local()))
            .map(|(reference, commit)| (reference.clone(), commit.clone())),
    }
}

/// Order a dated release name so numeric suffixes compare numerically.
pub fn release_order(name: &str) -> (String, u32) {
    let bare = name.strip_prefix(RELEASE_PREFIX).unwrap_or(name);
    match bare.split_once('.') {
        Some((date, suffix)) => (date.to_owned(), suffix.parse().unwrap_or(0)),
        None => (bare.to_owned(), 0),
    }
}

/// Detect release names that refer to different trees in trusted refs.
pub fn double_cut_findings(
    repo_path: &Path,
    tips: &BookmarkTips,
    scheme: &ReleaseScheme,
    publish_remote: &str,
) -> anyhow::Result<(Vec<Finding>, Vec<String>)> {
    let disagreements =
        crate::detect::double_cut::same_name_disagreements(tips, scheme, publish_remote);
    if disagreements.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }

    let mut findings = Vec::new();
    let mut notes = Vec::new();
    for (name, references) in disagreements {
        let mut commits: BTreeSet<CommitId> =
            references.into_iter().map(|(_, commit)| commit).collect();
        let Some(first) = commits.pop_first() else {
            anyhow::bail!("double-cut disagreement for {name} named no commits");
        };
        let mut changed = BTreeSet::new();
        let mut different = None;
        for other in commits {
            let files = jj::changed_files_between(repo_path, first.as_str(), other.as_str())?;
            if !files.is_empty() && different.is_none() {
                different = Some(other);
            }
            changed.extend(files);
        }
        if changed.is_empty() {
            notes.push(format!(
                "{name} names two commits with identical trees (a rebuilt cut)"
            ));
        } else if let Some(different) = different {
            findings.push(Finding::new(
                FindingKind::DoubleCut,
                Subject::Branch(name.clone()),
                format!(
                    "{name} names both {} and {}, and their trees differ ({} files)",
                    short(&first),
                    short(&different),
                    changed.len()
                ),
            ));
        } else {
            anyhow::bail!("double-cut disagreement for {name} had no tree comparison");
        }
    }
    Ok((findings, notes))
}

/// The composition a previous cut's ledger event recorded.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordedCut {
    pub name: String,
    /// The commit created by this cut, stored as the first evidence item.
    pub commit: CommitId,
    pub members: Vec<CommitId>,
}

/// The newest structural cut event, optionally scoped to a release name.
pub fn last_recorded_cut(entries: &[Entry], subject: Option<&str>) -> Option<RecordedCut> {
    entries.iter().rev().find_map(|entry| {
        let entry_subject = entry.subject.as_deref()?;
        if subject.is_some_and(|subject| subject != entry_subject)
            || entry.kind != Kind::Event
            || !entry.text.starts_with(&format!("cut {entry_subject} as "))
        {
            return None;
        }
        let (commit, members) = entry.evidence.split_first()?;
        if members.is_empty() {
            return None;
        }
        Some(RecordedCut {
            name: entry_subject.to_owned(),
            commit: CommitId::new(commit.as_str()),
            members: members
                .iter()
                .map(|sha| CommitId::new(sha.as_str()))
                .collect(),
        })
    })
}

fn short(value: &CommitId) -> String {
    value.as_str().chars().take(12).collect()
}
