//! Consumer pin retrieval from forge, cache, and local checkout sources.
//!
//! Release-model code classifies supplied texts; this module owns every source
//! that opens a checkout, calls a forge, or reads and writes a cache.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use crate::forge::{ConsumerHead, ForgeError};
use crate::forge_cache::{
    CONSUMER_SCHEMA_VERSION, ConsumerCache, consumer_cache_path, load_consumer_cache,
    write_consumer_cache,
};
use crate::ids::ReleaseScheme;
use crate::jj::{self, OriginTrunk};
use crate::pins::PIN_FILES;
use crate::release_model::{ConsumerScan, scan_consumer_texts};

/// The narrow forge surface required to retrieve a consumer's release pins.
pub trait ConsumerPinSource: Send + Sync {
    /// The consumer's default branch and its head commit in one forge call.
    fn consumer_head(&self, repo: &Path, slug: &str) -> Result<ConsumerHead, ForgeError>;

    /// One file's raw text at a commit. A missing file is not an error.
    #[allow(
        clippy::too_many_arguments,
        reason = "the checkout, consumer slug, commit, and file path are distinct forge-address components"
    )]
    fn file_at(
        &self,
        repo: &Path,
        slug: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<String>, ForgeError>;
}

/// One process's memo of consumer default-branch heads.
#[derive(Debug, Default)]
pub struct ConsumerHeadMemo {
    heads: Mutex<BTreeMap<String, ConsumerHead>>,
}

/// Scan a forge-addressed consumer with a process-wide default-branch-head memo.
#[allow(
    clippy::too_many_arguments,
    reason = "the forge, cache, checkout, consumer identity, filter, release scheme, and shared memo are independent scan inputs"
)]
pub fn scan_consumer_slug_with_heads(
    forge: &dyn ConsumerPinSource,
    cache_root: Option<&Path>,
    repo_path: &Path,
    slug: &str,
    repo_slug_filter: Option<&str>,
    scheme: &ReleaseScheme,
    heads: &ConsumerHeadMemo,
) -> ConsumerScan {
    let mut result = ConsumerScan::default();
    let cache_path = cache_root.and_then(|root| consumer_cache_path(root, slug));
    let cache = cache_path
        .as_deref()
        .and_then(|path| load_consumer_cache(path, slug));
    match (consumer_head(forge, repo_path, slug, heads), cache.as_ref()) {
        (Ok(head), Some(cache)) if cache.commit == head.commit => {
            result.extend(scan_consumer_texts(
                cache
                    .files
                    .iter()
                    .map(|(file, text)| (file.as_str(), text.as_str())),
                repo_slug_filter,
                scheme,
            ));
        }
        (Ok(head), _) => {
            let mut files = BTreeMap::new();
            for file in PIN_FILES {
                match forge.file_at(repo_path, slug, &head.commit, file) {
                    Ok(Some(text)) => {
                        files.insert((*file).to_owned(), text);
                    }
                    Ok(None) => {}
                    Err(error) => result.problems.push(format!(
                        "could not read {file} at {slug}@{}: {error}",
                        head.commit
                    )),
                }
            }
            if result.problems.is_empty() {
                let cache = ConsumerCache {
                    schema: CONSUMER_SCHEMA_VERSION,
                    slug: slug.to_owned(),
                    branch: head.branch,
                    commit: head.commit,
                    fetched_at: jiff::Timestamp::now().to_string(),
                    files: files.clone(),
                };
                if let Some(path) = cache_path.as_deref()
                    && let Err(error) = write_consumer_cache(path, &cache)
                {
                    result
                        .notes
                        .push(format!("{slug}: could not write consumer cache: {error}"));
                }
            }
            result.extend(scan_consumer_texts(
                files
                    .iter()
                    .map(|(file, text)| (file.as_str(), text.as_str())),
                repo_slug_filter,
                scheme,
            ));
        }
        (Err(error), cache) => {
            if let Some(cache) = cache {
                result.extend(scan_consumer_texts(
                    cache
                        .files
                        .iter()
                        .map(|(file, text)| (file.as_str(), text.as_str())),
                    repo_slug_filter,
                    scheme,
                ));
                result.notes.push(format!(
                    "{slug}: forge unreachable; pins answered from cache at {}",
                    short_text(&cache.commit)
                ));
            }
            result
                .problems
                .push(format!("{slug}: forge unreachable: {error}"));
        }
    }
    result
}

fn consumer_head(
    forge: &dyn ConsumerPinSource,
    repo_path: &Path,
    slug: &str,
    heads: &ConsumerHeadMemo,
) -> Result<ConsumerHead, ForgeError> {
    let mut cached = heads
        .heads
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(head) = cached.get(slug) {
        return Ok(head.clone());
    }
    let head = forge.consumer_head(repo_path, slug)?;
    cached.insert(slug.to_owned(), head.clone());
    drop(cached);
    Ok(head)
}

/// Scan a local checkout for pins of one repo's releases.
pub fn scan_consumer_for(
    consumer: &Path,
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) -> ConsumerScan {
    let mut result = ConsumerScan::default();
    match jj::origin_trunk(consumer) {
        Ok(OriginTrunk::Reference(branch)) => {
            let mut checkout_lag = None;
            for name in PIN_FILES {
                match jj::file_at_ref(consumer, &branch, name) {
                    Ok(Some((text, behind))) => {
                        result.extend(scan_consumer_texts(
                            std::iter::once((*name, text.as_str())),
                            slug,
                            scheme,
                        ));
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
            extend_working_copy_pins(&mut result, consumer, slug, scheme);
            result.notes.push(format!(
                "{}: no origin trunk resolved; pins read from the working copy",
                consumer.display()
            ));
        }
        Ok(OriginTrunk::NotRepository) => {
            extend_working_copy_pins(&mut result, consumer, slug, scheme);
            result.notes.push(format!(
                "{}: not a repository; pins read from the working copy",
                consumer.display()
            ));
        }
        Err(error) => {
            extend_working_copy_pins(&mut result, consumer, slug, scheme);
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
    slug: Option<&str>,
    scheme: &ReleaseScheme,
) {
    for name in PIN_FILES {
        match std::fs::read_to_string(consumer.join(name)) {
            Ok(text) => result.extend(scan_consumer_texts(
                std::iter::once((*name, text.as_str())),
                slug,
                scheme,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => result
                .problems
                .push(format!("could not read {name}: {error}")),
        }
    }
}

fn short_text(value: &str) -> String {
    value.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::atomic::Ordering;

    use super::{ConsumerHeadMemo, scan_consumer_slug_with_heads};
    use crate::forge::{ConsumerHead, fake::FakeForge};
    use crate::ids::ReleaseScheme;
    use crate::pins::PIN_FILES;

    const CONSUMER: &str = "acme/consumer";
    const REPOSITORY: &str = "tool";
    const HOST: &str = concat!("github", ".com");

    fn lockfile(reference: &str) -> String {
        format!(
            "tool = {{ git = \"https://{HOST}/acme/{REPOSITORY}.git?rev={}#0123456789abcdef\" }}\n",
            reference.replace('/', "%2F")
        )
    }

    fn forge(commit: &str, lock: &str) -> FakeForge {
        FakeForge {
            heads: BTreeMap::from([(
                CONSUMER.to_owned(),
                ConsumerHead {
                    branch: "main".to_owned(),
                    commit: commit.to_owned(),
                },
            )]),
            files: BTreeMap::from([(
                (CONSUMER.to_owned(), commit.to_owned(), "uv.lock".to_owned()),
                lock.to_owned(),
            )]),
            ..FakeForge::default()
        }
    }

    #[test]
    fn a_same_commit_consumer_scan_serves_lock_files_from_the_cache_with_zero_content_calls() {
        let root = tempfile::tempdir().expect("create cache root");
        let initial = forge("aaaaaaaaaaaaaaaa", &lockfile("release/2026-08-01"));
        let heads = ConsumerHeadMemo::default();

        let first = scan_consumer_slug_with_heads(
            &initial,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &heads,
        );
        assert_eq!(first.pins.len(), 1);
        assert_eq!(initial.file_calls.load(Ordering::SeqCst), PIN_FILES.len());

        let cached = forge("aaaaaaaaaaaaaaaa", &lockfile("release/2026-08-02"));
        let second = scan_consumer_slug_with_heads(
            &cached,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &ConsumerHeadMemo::default(),
        );

        assert_eq!(cached.file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            second.pins.first().map(|pin| pin.reference.as_str()),
            Some("release/2026-08-01")
        );
    }

    #[test]
    fn a_new_commit_refetches_pin_files_and_rewrites_the_cache() {
        let root = tempfile::tempdir().expect("create cache root");
        let initial = forge("aaaaaaaaaaaaaaaa", &lockfile("release/2026-08-01"));
        let heads = ConsumerHeadMemo::default();
        let _ = scan_consumer_slug_with_heads(
            &initial,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &heads,
        );

        let advanced = forge("bbbbbbbbbbbbbbbb", &lockfile("release/2026-08-02"));
        let scan = scan_consumer_slug_with_heads(
            &advanced,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &ConsumerHeadMemo::default(),
        );

        assert_eq!(advanced.file_calls.load(Ordering::SeqCst), PIN_FILES.len());
        assert_eq!(
            scan.pins.first().map(|pin| pin.reference.as_str()),
            Some("release/2026-08-02")
        );

        let cached = forge("bbbbbbbbbbbbbbbb", &lockfile("release/2026-08-03"));
        let scan = scan_consumer_slug_with_heads(
            &cached,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &ConsumerHeadMemo::default(),
        );
        assert_eq!(cached.file_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            scan.pins.first().map(|pin| pin.reference.as_str()),
            Some("release/2026-08-02")
        );
    }

    #[test]
    fn a_forge_failure_with_no_cache_is_a_problem_not_an_empty_success() {
        let root = tempfile::tempdir().expect("create cache root");
        let forge = FakeForge {
            fail_consumer_head: true,
            ..FakeForge::default()
        };

        let scan = scan_consumer_slug_with_heads(
            &forge,
            Some(root.path()),
            Path::new("/fork"),
            CONSUMER,
            Some(REPOSITORY),
            &ReleaseScheme::Dated,
            &ConsumerHeadMemo::default(),
        );

        assert!(scan.pins.is_empty());
        assert_eq!(scan.notes, Vec::<String>::new());
        assert_eq!(scan.problems.len(), 1);
    }
}
