//! The run-scoped forge snapshot: the only door to forge facts.

//!
//! Constructed only by a successful sweep+batch or reseed+batch. Downstream
//! consumers take the snapshot, never the raw trait, so a failed fetch is a
//! `None` every renderer must acknowledge — not a stale map that reads as truth.
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::detect::LandedVerdict;
use crate::forge::{Forge, ForgeError, PullFacts, PullSummary, RepoIdentity};
use crate::forge_cache::{self, CacheFile};

pub struct SnapshotConfig<'a> {
    pub forge: &'a dyn Forge,
    pub path: &'a Path,
    /// Origin and release remotes: authors derive from them (search_authors),
    /// and ours() filters by them (ours_only).
    pub remotes: [&'a str; 2],
    /// Resolved cache root (…/knives). None = no persistence: cold fetch, no read, no write.
    pub cache_root: Option<&'a Path>,
}

impl fmt::Debug for SnapshotConfig<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapshotConfig")
            .field("forge", &true)
            .field("path", &self.path)
            .field("remotes", &self.remotes)
            .field("cache_root", &self.cache_root)
            .finish()
    }
}

/// Identity resolved, cache file read. The single read (spec §5) happens here,
/// before any phase forks; the landed section is served from it.
pub struct Opened<'a> {
    config: SnapshotConfig<'a>,
    identity: RepoIdentity,
    file: Option<PathBuf>,
    read: Option<CacheFile>,
}

impl fmt::Debug for Opened<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Opened")
            .field("config", &self.config)
            .field("identity", &self.identity)
            .field("file", &self.file)
            .field("has_read", &self.read.is_some())
            .finish()
    }
}

pub fn open<'a>(config: SnapshotConfig<'a>) -> Result<Opened<'a>, ForgeError> {
    let identity = config.forge.repo_identity(config.path)?;
    let file = config
        .cache_root
        .and_then(|root| forge_cache::cache_path(root, &identity));
    let read = file
        .as_deref()
        .and_then(|path| forge_cache::load(path, &identity));
    Ok(Opened {
        config,
        identity,
        file,
        read,
    })
}

impl<'a> Opened<'a> {
    pub fn identity(&self) -> &RepoIdentity {
        &self.identity
    }

    /// Landed verdict cached under this key, from the same single read.
    pub fn landed_cached(&self, key: &str) -> Option<LandedVerdict> {
        self.read
            .as_ref()
            .and_then(|file| file.landed.get(key).copied())
    }

    /// Warm: sweep; valid delta → cached rows. Overflow/invalid/failed sweep or
    /// no cache → cold reseed (wide cheap lists). Err = neither path succeeded.
    pub fn discover(&self) -> Result<Discovery<'_>, ForgeError> {
        if let Some(read) = &self.read {
            match self.config.forge.sweep(self.config.path, &self.identity) {
                Ok(page) => {
                    let spans = page
                        .entries
                        .last()
                        .is_some_and(|oldest| oldest.updated_at.as_str() < read.watermark.as_str())
                        || !page.has_next_page;
                    if spans {
                        let refresh = page
                            .entries
                            .iter()
                            .filter(|entry| {
                                entry.updated_at.as_str() >= read.watermark.as_str()
                            })
                            .map(|entry| entry.number)
                            .collect();
                        let sweep_max = page
                            .entries
                            .first()
                            .map_or_else(String::new, |newest| newest.updated_at.clone());
                        let mut rows: Vec<PullSummary> = read.pulls.values().cloned().collect();
                        rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
                        return Ok(Discovery {
                            opened: self,
                            rows,
                            refresh,
                            sweep_max,
                            cold: false,
                        });
                    }
                }
                Err(_) => {}
            }
        }

        let authors = crate::forge::search_authors(&self.config.remotes);
        let mut rows = self
            .config
            .forge
            .list_pull_requests(self.config.path, &authors)?;
        rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        let sweep_max = rows
            .iter()
            .map(|row| row.updated_at.as_str())
            .max()
            .map_or_else(String::new, str::to_owned);
        Ok(Discovery {
            opened: self,
            rows,
            refresh: Vec::new(),
            sweep_max,
            cold: true,
        })
    }
}


pub struct Discovery<'o> {
    opened: &'o Opened<'o>,
    rows: Vec<PullSummary>,
    refresh: Vec<u64>,
    sweep_max: String,
    cold: bool,
}

impl fmt::Debug for Discovery<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Discovery")
            .field("opened", &self.opened)
            .field("rows", &self.rows)
            .field("refresh", &self.refresh)
            .field("sweep_max", &self.sweep_max)
            .field("cold", &self.cold)
            .finish()
    }
}

impl<'o> Discovery<'o> {
    /// Every known row, all owners, newest-updated first. Discovery only:
    /// branch attachment, shadowed history, tracked/stated/dependency numbers.
    pub fn rows(&self) -> &[PullSummary] {
        &self.rows
    }

    /// rows() filtered to our own copies (ours_only over config.remotes).
    pub fn ours(&self) -> Vec<PullSummary> {
        ours_only(&self.rows, &self.opened.config.remotes)
    }

    /// The one live batch: refresh set ∪ surfaced (deduped). Any failure → Err
    /// and no snapshot exists (I3). Success merges fetched rows over the
    /// discovery rows so rows() reflects what the batch just proved.
    pub fn complete(self, surfaced: &[u64]) -> Result<ForgeSnapshot<'o>, ForgeError> {
        let mut numbers = self.refresh;
        numbers.extend_from_slice(surfaced);
        numbers.sort_unstable();
        numbers.dedup();
        let facts = if numbers.is_empty() {
            BTreeMap::new()
        } else {
            self.opened
                .config
                .forge
                .pull_facts(self.opened.config.path, &self.opened.identity, &numbers)?
        };
        let mut rows = self.rows;
        for fact in facts.values() {
            let refreshed = PullSummary::of(&fact.pull);
            if let Some(row) = rows.iter_mut().find(|row| row.number == refreshed.number) {
                *row = refreshed;
            } else {
                rows.push(refreshed);
            }
        }
        rows.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
        Ok(ForgeSnapshot {
            opened: self.opened,
            rows,
            facts,
            sweep_max: self.sweep_max,
            cold: self.cold,
        })
    }
}

pub struct ForgeSnapshot<'o> {
    opened: &'o Opened<'o>,
    rows: Vec<PullSummary>,
    facts: BTreeMap<u64, PullFacts>,
    sweep_max: String,
    cold: bool,
}

impl fmt::Debug for ForgeSnapshot<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForgeSnapshot")
            .field("opened", &self.opened)
            .field("rows", &self.rows)
            .field("facts", &self.facts)
            .field("sweep_max", &self.sweep_max)
            .field("cold", &self.cold)
            .finish()
    }
}

impl ForgeSnapshot<'_> {
    pub fn rows(&self) -> &[PullSummary] {
        &self.rows
    }

    pub fn ours(&self) -> Vec<PullSummary> {
        ours_only(&self.rows, &self.opened.config.remotes)
    }

    /// The live fact row. Present for every number the batch answered; a
    /// surfaced number that is absent was NOT_FOUND — render it as unanswered,
    /// never from cache.
    pub fn fact(&self, number: u64) -> Option<&PullFacts> {
        self.facts.get(&number)
    }

    /// The single write: merge (warm) or replace (cold) against the state read
    /// at open, landed section replaced when `landed` is Some, temp+rename.
    /// Err = cache write failed AFTER live success: consulted stays true,
    /// caller adds a problem note (failure-table row 7). No-op Ok when no
    /// cache path exists.
    pub fn persist(
        &self,
        landed: Option<BTreeMap<String, LandedVerdict>>,
    ) -> std::io::Result<()> {
        let Some(path) = &self.opened.file else {
            return Ok(());
        };
        let landed_out = landed.unwrap_or_else(|| {
            self.opened
                .read
                .as_ref()
                .map(|file| file.landed.clone())
                .unwrap_or_default()
        });
        let file = match (self.cold, self.opened.read.as_ref()) {
            (true, _) => forge_cache::replace_cold(&self.opened.identity, &self.rows, landed_out),
            (false, Some(read)) => {
                let mut merged =
                    forge_cache::merge_warm(read.clone(), &self.facts, &self.sweep_max);
                merged.landed = landed_out;
                merged
            }
            (false, None) => {
                return Err(std::io::Error::other(
                    "a warm snapshot must retain the cache read from open",
                ));
            }
        };
        forge_cache::write(path, &file)
    }
}

/// PullSummary successor to [`crate::forge::ours_only`], the PullRequest helper
/// that Wave 5 deletes after every consumer has moved to snapshots.
fn ours_only(rows: &[PullSummary], remotes: &[&str]) -> Vec<PullSummary> {
    let owners: Vec<&str> = remotes
        .iter()
        .filter_map(|remote| crate::forge::remote_owner(remote))
        .collect();
    if owners.is_empty() {
        return rows.to_vec();
    }
    rows.iter()
        .filter(|row| owners.iter().any(|owner| row.is_from(owner)))
        .cloned()
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::expect_used,
    clippy::indexing_slicing,
    reason = "fixture setup and envelope inspection failures are test failures"
)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;

    use super::{SnapshotConfig, open};
    use crate::detect::LandedVerdict;
    use crate::forge::{Account, FakeForge, PullRequest, PullSummary, RepoIdentity};
    use crate::forge_cache::{CacheFile, SCHEMA_VERSION, cache_path, load, write};
    use crate::ids::BranchName;

    const EARLIER: &str = "2026-08-01T00:00:00Z";
    const LATER: &str = "2026-08-02T00:00:00Z";

    fn config<'a>(fake: &'a FakeForge, root: Option<&'a Path>) -> SnapshotConfig<'a> {
        SnapshotConfig {
            forge: fake,
            path: Path::new("/fake"),
            remotes: [
                "https://github.com/fake-owner/fake-repo.git",
                "git@github.com:fake-owner/fake-repo.git",
            ],
            cache_root: root,
        }
    }

    fn identity() -> RepoIdentity {
        RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        }
    }

    fn pull(number: u64, updated_at: &str, oid: &str) -> PullRequest {
        PullRequest {
            number,
            head_ref_name: format!("branch-{number}"),
            head_ref_oid: oid.to_owned(),
            updated_at: updated_at.to_owned(),
            head_repository_owner: Some(Account {
                login: "fake-owner".to_owned(),
            }),
            ..PullRequest::default()
        }
    }

    fn fake(pulls: impl IntoIterator<Item = PullRequest>) -> FakeForge {
        FakeForge {
            pull_requests: pulls
                .into_iter()
                .map(|pull| (BranchName::new(pull.head_ref_name.clone()), pull))
                .collect(),
            ..FakeForge::default()
        }
    }

    fn cache(watermark: &str, pulls: impl IntoIterator<Item = PullRequest>) -> CacheFile {
        CacheFile {
            schema_version: SCHEMA_VERSION,
            name_with_owner: identity().name_with_owner,
            repo_id: identity().id,
            watermark: watermark.to_owned(),
            pulls: pulls
                .into_iter()
                .map(|pull| (pull.number, PullSummary::of(&pull)))
                .collect(),
            landed: BTreeMap::new(),
        }
    }

    fn write_cache(root: &Path, file: &CacheFile) -> std::path::PathBuf {
        let path = cache_path(root, &identity()).expect("the fake identity has owner/repo form");
        write(&path, file).expect("write cache fixture");
        path
    }

    fn seed_cache(root: &Path, initial: PullRequest) {
        let forge = fake([initial]);
        let opened = open(config(&forge, Some(root))).expect("resolve fake identity");
        let snapshot = opened
            .discover()
            .expect("cold reseed")
            .complete(&[])
            .expect("empty live batch succeeds");
        snapshot.persist(None).expect("persist seed cache");
    }

    #[test]
    fn a_snapshot_only_exists_after_sweep_and_batch_both_succeed() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let mut read = cache(EARLIER, [pull(7, EARLIER, "old-oid")]);
        let _ = read.landed.insert("tip:trunk".to_owned(), LandedVerdict::InTrunk);
        let path = write_cache(directory.path(), &read);
        let forge = fake([pull(7, LATER, "fresh-oid")]);

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        assert_eq!(opened.identity(), &identity());
        assert_eq!(
            opened.landed_cached("tip:trunk"),
            Some(LandedVerdict::InTrunk),
            "the landed section comes from the single read at open"
        );
        let snapshot = opened
            .discover()
            .expect("sweep spans the watermark")
            .complete(&[])
            .expect("live batch succeeds");

        assert_eq!(snapshot.fact(7).map(|fact| fact.pull.head_ref_oid.as_str()), Some("fresh-oid"));
        assert_eq!(snapshot.rows()[0].head_ref_oid, "fresh-oid");
        assert_eq!(snapshot.ours().len(), 1, "ours uses the configured remotes");
        snapshot.persist(None).expect("persist warm snapshot");
        assert_eq!(
            load(&path, &identity())
                .and_then(|file| file.landed.get("tip:trunk").copied()),
            Some(LandedVerdict::InTrunk),
            "a non-probe caller keeps the landed cache section"
        );

        let quiet_forge = FakeForge {
            fail_facts: true,
            ..fake([])
        };
        let quiet_opened = open(config(&quiet_forge, None)).expect("open quiet repository");
        let quiet_snapshot = quiet_opened
            .discover()
            .expect("empty cold list succeeds")
            .complete(&[])
            .expect("empty batch does not call pull_facts");
        assert!(
            quiet_snapshot.fact(7).is_none(),
            "a quiet snapshot exists even when the forge facts route would fail"
        );
    }

    #[test]
    fn a_failed_batch_leaves_no_snapshot_no_watermark_advance_and_no_write() {
        let directory = tempfile::tempdir().expect("create cache directory");
        seed_cache(directory.path(), pull(7, EARLIER, "seed-oid"));
        let path = cache_path(directory.path(), &identity()).expect("cache path");
        let before = std::fs::read(&path).expect("read seeded cache");
        let forge = FakeForge {
            fail_facts: true,
            ..fake([pull(7, LATER, "updated-oid")])
        };

        let opened = open(config(&forge, Some(directory.path()))).expect("open seeded cache");
        let result = opened
            .discover()
            .expect("sweep succeeds")
            .complete(&[]);

        assert!(result.is_err(), "a failed live batch constructs no snapshot");
        assert_eq!(
            std::fs::read(&path).expect("read cache after failed batch"),
            before,
            "failed live data cannot advance or rewrite the cache"
        );
    }

    #[test]
    fn sweep_failure_falls_back_to_reseed_and_still_consults() {
        let directory = tempfile::tempdir().expect("create cache directory");
        write_cache(directory.path(), &cache(EARLIER, [pull(99, EARLIER, "stale-oid")]));
        let forge = FakeForge {
            fail_sweep: true,
            ..fake([pull(7, LATER, "live-oid"), pull(100, EARLIER, "older-oid")])
        };

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        let discovery = opened
            .discover()
            .expect("failed sweep falls back to list");
        assert_eq!(
            discovery.rows().iter().map(|row| row.number).collect::<Vec<_>>(),
            vec![7, 100],
            "cold discovery normalizes the fake's branch-ordered list to newest-updated first"
        );
        let snapshot = discovery
            .complete(&[7])
            .expect("live batch after reseed");

        assert_eq!(
            snapshot.rows().iter().map(|row| row.number).collect::<Vec<_>>(),
            vec![7, 100]
        );
        assert!(snapshot.fact(7).is_some(), "the fallback snapshot has live facts");
    }

    #[test]
    fn sweep_and_reseed_both_failing_is_todays_forge_down() {
        let directory = tempfile::tempdir().expect("create cache directory");
        write_cache(directory.path(), &cache(EARLIER, [pull(7, EARLIER, "cached-oid")]));
        let forge = FakeForge {
            fail_sweep: true,
            fail_list: true,
            ..fake([pull(7, LATER, "live-oid")])
        };

        assert!(
            open(config(&forge, Some(directory.path())))
                .expect("open cache")
                .discover()
                .is_err(),
            "no cached discovery survives when both live paths fail"
        );
    }

    #[test]
    fn overflow_replaces_the_map_instead_of_merging() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let path = write_cache(
            directory.path(),
            &cache(EARLIER, [pull(7, EARLIER, "old-oid"), pull(99, EARLIER, "stale-oid")]),
        );
        let forge = FakeForge {
            sweep_overflows: true,
            ..fake([pull(7, LATER, "fresh-oid")])
        };

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        let snapshot = opened
            .discover()
            .expect("overflow falls back to cold list")
            .complete(&[7])
            .expect("live batch succeeds");
        snapshot.persist(None).expect("persist cold replacement");

        let written = load(&path, &identity()).expect("read persisted cache");
        assert_eq!(written.pulls.len(), 1);
        assert!(written.pulls.contains_key(&7));
        assert!(
            !written.pulls.contains_key(&99),
            "a cold replacement cannot strand a stale cached row"
        );
    }

    #[test]
    fn a_short_all_fresh_page_is_not_an_overflow() {
        let directory = tempfile::tempdir().expect("create cache directory");
        write_cache(directory.path(), &cache(EARLIER, [pull(7, EARLIER, "old-oid")]));
        let forge = FakeForge {
            fail_list: true,
            ..fake([pull(7, LATER, "fresh-oid")])
        };

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        let snapshot = opened
            .discover()
            .expect("a complete first page is a valid delta")
            .complete(&[])
            .expect("the refreshed row is fetched live");

        assert_eq!(snapshot.fact(7).map(|fact| fact.pull.head_ref_oid.as_str()), Some("fresh-oid"));
    }

    #[test]
    fn a_same_second_mutation_is_refreshed_anyway() {
        let directory = tempfile::tempdir().expect("create cache directory");
        write_cache(directory.path(), &cache(EARLIER, [pull(7, EARLIER, "old-oid")]));
        let forge = fake([pull(7, EARLIER, "fresh-oid")]);

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        let snapshot = opened
            .discover()
            .expect("sweep spans its equal watermark")
            .complete(&[])
            .expect("same-second entry is in the live batch");

        assert_eq!(snapshot.fact(7).map(|fact| fact.pull.head_ref_oid.as_str()), Some("fresh-oid"));
        assert_eq!(snapshot.rows()[0].head_ref_oid, "fresh-oid");
    }

    #[test]
    fn the_cache_write_failing_after_live_success_is_a_note_not_a_failure() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let root_file = directory.path().join("cache-root-file");
        std::fs::write(&root_file, b"not a directory").expect("make invalid cache root");
        let forge = fake([pull(7, LATER, "live-oid")]);

        let opened =
            open(config(&forge, Some(&root_file))).expect("identity resolution does not depend on the cache");
        let snapshot = opened
            .discover()
            .expect("cold list succeeds")
            .complete(&[7])
            .expect("live data succeeds before the write");

        assert!(snapshot.fact(7).is_some(), "live fact is usable before persistence");
        assert!(snapshot.persist(None).is_err(), "an unwritable cache is reported to the caller");
        assert!(snapshot.fact(7).is_some(), "a write failure cannot invalidate live facts");
    }

    #[test]
    fn surfaced_numbers_join_the_batch_deduped_with_the_refresh_set() {
        let directory = tempfile::tempdir().expect("create cache directory");
        write_cache(directory.path(), &cache(EARLIER, [pull(7, EARLIER, "old-oid")]));
        let forge = fake([pull(7, LATER, "fresh-oid"), pull(9, EARLIER, "surfaced-oid")]);

        let opened = open(config(&forge, Some(directory.path()))).expect("open cache");
        let snapshot = opened
            .discover()
            .expect("valid warm delta")
            .complete(&[7, 9, 9])
            .expect("one batch answers refresh and surfaced numbers");

        assert!(snapshot.fact(7).is_some(), "the refresh number is answered");
        assert!(snapshot.fact(9).is_some(), "a surfaced number joins the batch once");
    }

    #[test]
    fn identity_mismatch_ignores_the_file_and_reseeds() {
        let directory = tempfile::tempdir().expect("create cache directory");
        let mut foreign = cache(EARLIER, [pull(99, EARLIER, "foreign-oid")]);
        foreign.repo_id = "OTHERID".to_owned();
        write_cache(directory.path(), &foreign);
        let forge = fake([pull(7, LATER, "live-oid")]);

        let opened =
            open(config(&forge, Some(directory.path()))).expect("open ignores a foreign cache file");
        let discovery = opened
            .discover()
            .expect("identity mismatch cold-reseeds");

        assert_eq!(
            discovery.rows().iter().map(|row| row.number).collect::<Vec<_>>(),
            vec![7],
            "the foreign file contributes no discovery rows"
        );
    }
}
