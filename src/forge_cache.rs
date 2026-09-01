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

/// `None` when cache loss or a validation failure makes a warm read unsafe.
///
/// Validation covers file readability and syntax, schema and identity fields,
/// plus every discovery timestamp. Cache loss is never an error.
pub fn load(path: &std::path::Path, identity: &RepoIdentity) -> Option<CacheFile> {
    let bytes = std::fs::read(path).ok()?;
    let file: CacheFile = serde_json::from_slice(&bytes).ok()?;
    (file.schema_version == SCHEMA_VERSION
        && file.name_with_owner == identity.name_with_owner
        && file.repo_id == identity.id
        && (file.watermark.is_empty() || is_rfc3339_utc(&file.watermark))
        && file
            .pulls
            .values()
            .all(|pull| is_rfc3339_utc(&pull.updated_at)))
    .then_some(file)
}

fn is_rfc3339_utc(value: &str) -> bool {
    let Some(body) = value.strip_suffix('Z') else {
        return false;
    };
    let Some((date, time)) = body.split_once('T') else {
        return false;
    };
    let date = date.as_bytes();
    let valid_date = date.len() == 10
        && date.get(4) == Some(&b'-')
        && date.get(7) == Some(&b'-')
        && date
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit());
    let (seconds, fraction) = time
        .split_once('.')
        .map_or((time, None), |(seconds, fraction)| {
            (seconds, Some(fraction))
        });
    let seconds = seconds.as_bytes();
    let valid_time = seconds.len() == 8
        && seconds.get(2) == Some(&b':')
        && seconds.get(5) == Some(&b':')
        && seconds
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 2 | 5) || byte.is_ascii_digit());
    let valid_fraction = fraction.is_none_or(|fraction| {
        !fraction.is_empty() && fraction.bytes().all(|byte| byte.is_ascii_digit())
    });

    valid_date && valid_time && valid_fraction && value.parse::<jiff::Timestamp>().is_ok()
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

pub const CONSUMER_SCHEMA_VERSION: u32 = 1;

/// Cached pin-file contents for one consumer at one exact default-branch head.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConsumerCache {
    pub schema: u32,
    pub slug: String,
    pub branch: String,
    pub commit: String,
    pub fetched_at: String,
    pub files: std::collections::BTreeMap<String, String>,
}

/// `<root>/consumers/<owner>/<repo>.json`.
pub fn consumer_cache_path(root: &std::path::Path, slug: &str) -> Option<std::path::PathBuf> {
    let (owner, repository) = slug.split_once('/')?;
    let path_syntax = |segment: &str| {
        segment.is_empty() || segment.starts_with(['/', '~', '.']) || segment.contains('\\')
    };
    (!path_syntax(owner) && !path_syntax(repository) && !repository.contains('/')).then(|| {
        root.join("consumers")
            .join(owner)
            .join(format!("{repository}.json"))
    })
}

/// `None` when cache loss or validation failure makes a cached consumer scan unsafe.
pub fn load_consumer_cache(path: &std::path::Path, slug: &str) -> Option<ConsumerCache> {
    let bytes = std::fs::read(path).ok()?;
    let cache: ConsumerCache = serde_json::from_slice(&bytes).ok()?;
    (cache.schema == CONSUMER_SCHEMA_VERSION
        && cache.slug == slug
        && !cache.branch.is_empty()
        && !cache.commit.is_empty()
        && is_rfc3339_utc(&cache.fetched_at)
        && cache
            .files
            .keys()
            .all(|path| crate::pins::PIN_FILES.contains(&path.as_str())))
    .then_some(cache)
}

/// `create_dir_all` + tempfile in the same directory + persist (rename).
pub fn write_consumer_cache(path: &std::path::Path, cache: &ConsumerCache) -> std::io::Result<()> {
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => std::path::Path::new("."),
    };
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, cache).map_err(std::io::Error::other)?;
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
        CONSUMER_SCHEMA_VERSION, CacheFile, ConsumerCache, PROBE_SCHEMA, SCHEMA_VERSION,
        cache_path, consumer_cache_path, landed_key, load, load_consumer_cache, merge_warm,
        replace_cold, write, write_consumer_cache,
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
            head_ref_name: format!("branch-at-{updated_at}"),
            updated_at: updated_at.to_owned(),
            ..PullSummary::default()
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
                mergeable: Some("MERGEABLE".to_owned()),
                merge_state_status: Some("CLEAN".to_owned()),
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

    fn consumer_cache(slug: &str) -> ConsumerCache {
        ConsumerCache {
            schema: CONSUMER_SCHEMA_VERSION,
            slug: slug.to_owned(),
            branch: "main".to_owned(),
            commit: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            fetched_at: "2026-08-31T12:34:56Z".to_owned(),
            files: BTreeMap::from([("uv.lock".to_owned(), "pin = \"release\"".to_owned())]),
        }
    }

    #[test]
    fn a_consumer_cache_round_trips_only_for_the_matching_slug_and_schema() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let slug = "owner/repo";
        let path = consumer_cache_path(directory.path(), slug).expect("valid slug");
        assert_eq!(
            path,
            directory
                .path()
                .join("consumers")
                .join("owner")
                .join("repo.json")
        );
        assert!(
            consumer_cache_path(directory.path(), "../repo").is_none()
                && consumer_cache_path(directory.path(), "owner/.repo").is_none()
                && consumer_cache_path(directory.path(), "owner/repo/extra").is_none()
        );
        let cache = consumer_cache(slug);

        write_consumer_cache(&path, &cache).expect("write consumer cache");

        assert_eq!(load_consumer_cache(&path, slug), Some(cache.clone()));
        assert!(
            load_consumer_cache(&path, "other/repo").is_none(),
            "a cache cannot answer for another consumer"
        );
        let stale_schema = ConsumerCache {
            schema: CONSUMER_SCHEMA_VERSION + 1,
            ..cache
        };
        write_consumer_cache(&path, &stale_schema).expect("write stale schema");
        assert!(
            load_consumer_cache(&path, slug).is_none(),
            "a changed schema requires a refetch"
        );
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
    fn a_cache_with_a_non_timestamp_watermark_loads_as_none() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let path = directory.path().join("cache.json");
        let repo = identity();
        let file = CacheFile {
            watermark: "~".to_owned(),
            ..cache(
                &repo,
                BTreeMap::from([(1, summary(1, "2026-01-02T00:00:00Z"))]),
            )
        };

        std::fs::write(
            &path,
            serde_json::to_vec(&file).expect("serialize cache fixture"),
        )
        .expect("write cache fixture");

        assert!(
            load(&path, &repo).is_none(),
            "a malformed watermark must force a cold reseed"
        );
    }

    #[test]
    fn a_cache_with_a_non_timestamp_row_loads_as_none() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let path = directory.path().join("cache.json");
        let repo = identity();
        let file = cache(&repo, BTreeMap::from([(1, summary(1, "~"))]));

        std::fs::write(
            &path,
            serde_json::to_vec(&file).expect("serialize cache fixture"),
        )
        .expect("write cache fixture");

        assert!(
            load(&path, &repo).is_none(),
            "a malformed row timestamp must force a cold reseed"
        );
    }

    #[test]
    fn a_cache_with_rfc3339_utc_timestamps_loads() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let path = directory.path().join("cache.json");
        let repo = identity();
        let file = CacheFile {
            watermark: "2026-01-03T00:00:00Z".to_owned(),
            ..cache(
                &repo,
                BTreeMap::from([(1, summary(1, "2026-01-02T00:00:00Z"))]),
            )
        };

        std::fs::write(
            &path,
            serde_json::to_vec(&file).expect("serialize cache fixture"),
        )
        .expect("write cache fixture");

        assert_eq!(load(&path, &repo), Some(file));
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
