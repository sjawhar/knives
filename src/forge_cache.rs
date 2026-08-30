//! The per-upstream forge cache. Discovery data only: no fact in this file is
//! ever surfaced in a report without a same-run live fetch (spec I2).
use crate::detect::LandedVerdict;
use crate::forge::{PullFacts, PullSummary, RepoIdentity};

pub const SCHEMA_VERSION: u32 = 1;
/// Bump when the landed probe's semantics change; part of every landed key.
pub const PROBE_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CacheFile {
    pub schema_version: u32,
    pub name_with_owner: String,
    pub repo_id: String,
    /// Max updatedAt observed by the completing fetch. Empty = refresh everything.
    #[serde(default)]
    pub watermark: String,
    #[serde(default)]
    pub pulls: std::collections::BTreeMap<u64, PullSummary>,
    #[serde(default)]
    pub landed: std::collections::BTreeMap<String, LandedVerdict>,
}

/// `XDG_CACHE_HOME`, else `$HOME/.cache`, then `/knives`. `None` only when `HOME` is unset.
pub fn cache_root() -> Option<std::path::PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".cache"))
        })
        .map(|base| base.join("knives"))
}

/// <root>/forge/<owner>/<repo>.json — owner and repo are separate path segments.
pub fn cache_path(root: &std::path::Path, identity: &RepoIdentity) -> Option<std::path::PathBuf> {
    let (owner, repo) = identity.split().ok()?;
    Some(root.join("forge").join(owner).join(format!("{repo}.json")))
}

/// `None` on: missing file, unreadable, unparseable, `schema_version` mismatch,
/// `nameWithOwner` mismatch, `repo_id` mismatch. Never an error (spec: cache loss
/// is not a problem).
pub fn load(path: &std::path::Path, identity: &RepoIdentity) -> Option<CacheFile> {
    let bytes = std::fs::read(path).ok()?;
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    (file.schema_version == SCHEMA_VERSION
        && file.name_with_owner == identity.name_with_owner
        && file.repo_id == identity.id)
        .then_some(file)
}

pub fn landed_key(tip: &crate::ids::CommitId, trunk: &crate::ids::CommitId) -> String {
    format!(
        "{}:{}:{}:{}",
        tip.as_str(),
        trunk.as_str(),
        env!("CARGO_PKG_VERSION"),
        PROBE_SCHEMA
    )
}

/// Warm merge against the state read at open (never a re-read): per number the
/// newer updatedAt wins, equal updatedAt keeps this run's fetched row; the
/// watermark is max(read.watermark, `sweep_max`).
pub fn merge_warm(
    read: CacheFile,
    fetched: &std::collections::BTreeMap<u64, PullFacts>,
    sweep_max: &str,
) -> CacheFile {
    let CacheFile {
        schema_version,
        name_with_owner,
        repo_id,
        watermark,
        mut pulls,
        landed,
    } = read;
    for (number, facts) in fetched {
        let fresh = PullSummary::of(&facts.pull);
        let has_strictly_newer_read = pulls
            .get(number)
            .is_some_and(|cached| cached.updated_at > fresh.updated_at);
        if !has_strictly_newer_read {
            pulls.insert(*number, fresh);
        }
    }
    let watermark = if watermark.as_str() >= sweep_max {
        watermark
    } else {
        sweep_max.to_owned()
    };

    CacheFile {
        schema_version,
        name_with_owner,
        repo_id,
        watermark,
        pulls,
        landed,
    }
}

/// Cold replacement: the fetched rows ARE the map (overflow cannot strand
/// stale rows); watermark = max updatedAt over rows; landed carried over from
/// the read (or empty when there was none).
pub fn replace_cold(
    identity: &RepoIdentity,
    rows: &[PullSummary],
    landed: std::collections::BTreeMap<String, LandedVerdict>,
) -> CacheFile {
    let watermark = rows
        .iter()
        .map(|row| row.updated_at.as_str())
        .max()
        .map_or_else(String::new, str::to_owned);
    let pulls = rows.iter().cloned().map(|row| (row.number, row)).collect();

    CacheFile {
        schema_version: SCHEMA_VERSION,
        name_with_owner: identity.name_with_owner.clone(),
        repo_id: identity.id.clone(),
        watermark,
        pulls,
        landed,
    }
}

/// `create_dir_all` + tempfile in the same directory + persist (rename).
pub fn write(path: &std::path::Path, file: &CacheFile) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, file).map_err(std::io::Error::other)?;
    temporary
        .persist(path)
        .map_err(|error| std::io::Error::other(error.error))?;
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixture setup and envelope inspection failures are test failures"
)]
mod tests {
    use std::collections::BTreeMap;

    use super::{
        CacheFile, PROBE_SCHEMA, SCHEMA_VERSION, cache_path, landed_key, load, merge_warm,
        replace_cold, write,
    };
    use crate::{
        detect::LandedVerdict,
        forge::{PullDetails, PullFacts, PullRequest, PullSummary, RepoIdentity},
        ids::CommitId,
    };

    fn identity() -> RepoIdentity {
        RepoIdentity {
            name_with_owner: "owner/repo".to_owned(),
            id: "repo-node-id".to_owned(),
        }
    }

    fn summary(number: u64, updated_at: &str) -> PullSummary {
        PullSummary {
            number,
            state: "OPEN".to_owned(),
            review_decision: String::new(),
            head_ref_name: format!("branch-at-{updated_at}"),
            head_ref_oid: format!("oid-at-{updated_at}"),
            updated_at: updated_at.to_owned(),
            is_draft: false,
            url: String::new(),
            head_repository_owner: None,
            base_ref_name: "main".to_owned(),
            merge_commit: None,
        }
    }

    fn facts(number: u64, updated_at: &str) -> PullFacts {
        let summary = summary(number, updated_at);
        PullFacts {
            pull: PullRequest {
                number: summary.number,
                state: summary.state,
                review_decision: summary.review_decision,
                head_ref_name: summary.head_ref_name,
                head_ref_oid: summary.head_ref_oid,
                updated_at: summary.updated_at,
                is_draft: summary.is_draft,
                url: summary.url,
                head_repository_owner: summary.head_repository_owner,
                mergeable: "MERGEABLE".to_owned(),
                merge_state_status: "CLEAN".to_owned(),
                base_ref_name: summary.base_ref_name,
                merge_commit: summary.merge_commit,
            },
            details: PullDetails::default(),
            newest_comment: None,
        }
    }

    fn cache(identity: &RepoIdentity, pulls: BTreeMap<u64, PullSummary>) -> CacheFile {
        CacheFile {
            schema_version: SCHEMA_VERSION,
            name_with_owner: identity.name_with_owner.clone(),
            repo_id: identity.id.clone(),
            watermark: String::new(),
            pulls,
            landed: BTreeMap::new(),
        }
    }

    #[test]
    fn a_missing_or_corrupt_or_foreign_file_loads_as_none() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let path = directory.path().join("cache.json");
        let repo = identity();

        let dotted_name = RepoIdentity {
            name_with_owner: "owner/repo.backup".to_owned(),
            id: "dotted-repo-node-id".to_owned(),
        };
        assert_eq!(
            cache_path(directory.path(), &dotted_name),
            Some(
                directory
                    .path()
                    .join("forge")
                    .join("owner")
                    .join("repo.backup.json")
            ),
            "the whole repository segment remains part of the cache file name"
        );

        assert!(load(&path, &repo).is_none(), "missing cache is a cold read");

        std::fs::write(&path, b"not json").expect("write corrupt cache");
        assert!(load(&path, &repo).is_none(), "corrupt cache is a cold read");

        let cases = [
            (
                "wrong schema",
                CacheFile {
                    schema_version: SCHEMA_VERSION + 1,
                    ..cache(&repo, BTreeMap::new())
                },
            ),
            (
                "different forge name",
                CacheFile {
                    name_with_owner: "other/repo".to_owned(),
                    ..cache(&repo, BTreeMap::new())
                },
            ),
            (
                "different forge id",
                CacheFile {
                    repo_id: "other-node-id".to_owned(),
                    ..cache(&repo, BTreeMap::new())
                },
            ),
        ];
        for (description, foreign) in cases {
            let bytes = serde_json::to_vec(&foreign).expect("serialize foreign cache");
            std::fs::write(&path, bytes).expect("write foreign cache");
            assert!(load(&path, &repo).is_none(), "{description}");
        }
    }

    #[test]
    fn a_newer_file_row_survives_the_merge_and_an_equal_one_loses_to_the_fetch() {
        let repo = identity();
        let read = cache(
            &repo,
            BTreeMap::from([
                (1, summary(1, "2026-01-03T00:00:00Z")),
                (2, summary(2, "2026-01-02T00:00:00Z")),
            ]),
        );
        let mut equal_timestamp_fetch = facts(2, "2026-01-02T00:00:00Z");
        equal_timestamp_fetch.pull.head_ref_oid = "fresh-oid-at-same-timestamp".to_owned();
        let fetched = BTreeMap::from([
            (1, facts(1, "2026-01-01T00:00:00Z")),
            (2, equal_timestamp_fetch),
        ]);

        let merged = merge_warm(read, &fetched, "2026-01-04T00:00:00Z");

        assert_eq!(
            merged.pulls.get(&1).map(|pull| pull.head_ref_name.as_str()),
            Some("branch-at-2026-01-03T00:00:00Z"),
            "a newer row in the read file wins"
        );
        assert_eq!(
            merged.pulls.get(&2).map(|pull| pull.head_ref_oid.as_str()),
            Some("fresh-oid-at-same-timestamp"),
            "an equal timestamp belongs to this run's fresh fetch"
        );
    }

    #[test]
    fn the_watermark_pairs_with_the_rows_from_the_same_read() {
        let repo = identity();
        let mut read = cache(
            &repo,
            BTreeMap::from([(1, summary(1, "2026-01-01T00:00:00Z"))]),
        );
        read.watermark = "2026-01-03T00:00:00Z".to_owned();
        let fetched = BTreeMap::from([
            (2, facts(2, "2026-01-02T00:00:00Z")),
            (3, facts(3, "2026-01-04T00:00:00Z")),
        ]);

        let merged = merge_warm(read, &fetched, "2026-01-05T00:00:00Z");

        assert_eq!(merged.watermark, "2026-01-05T00:00:00Z");
        assert!(merged.pulls.contains_key(&1), "unfetched read row remains");
        assert!(merged.pulls.contains_key(&2), "fetched row is added");
        assert!(merged.pulls.contains_key(&3), "every fetched row is added");

        let mut later_read = cache(&repo, BTreeMap::new());
        later_read.watermark = "2026-01-06T00:00:00Z".to_owned();
        let later_watermark = merge_warm(later_read, &BTreeMap::new(), "2026-01-05T00:00:00Z");
        assert_eq!(
            later_watermark.watermark, "2026-01-06T00:00:00Z",
            "a watermark from the same read remains when the sweep is older"
        );
    }

    #[test]
    fn a_cold_replacement_strands_no_stale_row() {
        let repo = identity();
        let landed = BTreeMap::from([("tip:trunk:v:1".to_owned(), LandedVerdict::InTrunk)]);
        let replacement = replace_cold(
            &repo,
            &[
                summary(17, "2026-01-02T00:00:00Z"),
                summary(19, "2026-01-04T00:00:00Z"),
            ],
            landed.clone(),
        );

        assert_eq!(replacement.pulls.len(), 2);
        assert!(
            !replacement.pulls.contains_key(&4000),
            "old rows cannot survive a reseed"
        );
        assert_eq!(replacement.watermark, "2026-01-04T00:00:00Z");
        assert_eq!(
            replacement.landed, landed,
            "the landed section is preserved"
        );
    }

    #[test]
    fn a_write_lands_whole_or_not_at_all() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let repo = identity();
        let input = cache(
            &repo,
            BTreeMap::from([(1, summary(1, "2026-01-02T00:00:00Z"))]),
        );
        let path = directory.path().join("nested").join("cache.json");

        write(&path, &input).expect("write cache atomically");
        let round_trip: CacheFile =
            serde_json::from_slice(&std::fs::read(&path).expect("read atomically written cache"))
                .expect("parse atomically written cache");
        assert_eq!(round_trip, input);

        let blocking_parent = directory.path().join("cannot-be-a-directory");
        std::fs::write(&blocking_parent, b"original bytes").expect("prewrite parent file");
        let blocked = blocking_parent.join("cache.json");
        assert!(
            write(&blocked, &input).is_err(),
            "a file cannot become a directory"
        );
        assert_eq!(
            std::fs::read(&blocking_parent).expect("read prewritten parent file"),
            b"original bytes",
            "a failed atomic write leaves existing bytes alone"
        );
    }

    #[test]
    fn a_landed_key_changes_with_the_knives_version_and_probe_schema() {
        let key = landed_key(&CommitId::new("tip"), &CommitId::new("trunk"));

        assert!(key.contains(env!("CARGO_PKG_VERSION")));
        assert!(key.contains(&PROBE_SCHEMA.to_string()));
    }
}
