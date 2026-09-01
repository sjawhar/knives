//! Facts supplied directly, for tests: the [`Forge`](crate::forge::Forge)
//! implementation scenario tests configure with rows and failure switches instead
//! of a network.
//!
//! One failure switch per failure-table row.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{
    ChecksSummary, ConsumerHead, Forge, ForgeError, PullDetails, PullFacts, PullRequest,
    PullSummary, RepoIdentity, SweepEntry, SweepPage, TimelineEvent,
};
use crate::ids::BranchName;
/// Facts supplied directly, for tests.
///
// Test-fake failure injection has one switch per failure-table row.
#[allow(
    clippy::struct_excessive_bools,
    reason = "test-fake failure injection needs one switch per failure-table row"
)]
#[derive(Debug, Default, Clone)]
pub struct FakeForge {
    pub pull_requests: BTreeMap<BranchName, PullRequest>,
    pub stale_reviews: Vec<u64>,
    pub checks: BTreeMap<u64, ChecksSummary>,
    /// States for numbers outside the listed universe (deleted-from-window
    /// history a batch can still answer about).
    pub vanished_states: BTreeMap<u64, String>,
    pub newest_comments: BTreeMap<u64, String>,
    pub timeline: BTreeMap<u64, Vec<TimelineEvent>>,
    pub fail_identity: bool,
    pub fail_list: bool,
    pub fail_sweep: bool,
    pub fail_facts: bool,
    pub fail_timeline: bool,
    /// Sweep reports a continuation past page 1 (overflow → cold reseed).
    pub sweep_overflows: bool,
    pub heads: BTreeMap<String, ConsumerHead>,
    pub files: BTreeMap<(String, String, String), String>,
    pub fail_consumer_head: bool,
    pub fail_file_at: bool,
    pub consumer_head_calls: Arc<AtomicUsize>,
    pub file_calls: Arc<AtomicUsize>,
}

fn fake_failure(operation: &str) -> ForgeError {
    ForgeError::Command {
        command: "fake".to_owned(),
        dir: "/fake".to_owned(),
        code: 1,
        stderr: format!("fake {operation} failed"),
    }
}

const fn vanished_pull(number: u64, state: String) -> PullRequest {
    PullRequest {
        number,
        state,
        review_decision: String::new(),
        head_ref_name: String::new(),
        head_ref_oid: String::new(),
        updated_at: String::new(),
        is_draft: false,
        url: String::new(),
        head_repository_owner: None,
        mergeable: String::new(),
        merge_state_status: String::new(),
        base_ref_name: String::new(),
        merge_commit: None,
    }
}

impl Forge for FakeForge {
    fn repo_identity(&self, _repo: &Path) -> Result<RepoIdentity, ForgeError> {
        if self.fail_identity {
            return Err(fake_failure("identity"));
        }
        Ok(RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        })
    }

    fn list_pull_requests(
        &self,
        _repo: &Path,
        _authors: &[String],
    ) -> Result<Vec<PullSummary>, ForgeError> {
        if self.fail_list {
            return Err(fake_failure("list"));
        }
        Ok(self.pull_requests.values().map(PullSummary::of).collect())
    }

    fn sweep(&self, _repo: &Path, _target: &RepoIdentity) -> Result<SweepPage, ForgeError> {
        if self.fail_sweep {
            return Err(fake_failure("sweep"));
        }
        let mut entries = self
            .pull_requests
            .values()
            .map(|pull| SweepEntry {
                number: pull.number,
                updated_at: pull.updated_at.clone(),
                state: pull.state.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_unstable_by(|left, right| {
            right
                .updated_at
                .cmp(&left.updated_at)
                .then_with(|| left.number.cmp(&right.number))
        });
        Ok(SweepPage {
            entries,
            has_next_page: self.sweep_overflows,
        })
    }

    fn pull_facts(
        &self,
        _repo: &Path,
        _target: &RepoIdentity,
        numbers: &[u64],
    ) -> Result<BTreeMap<u64, PullFacts>, ForgeError> {
        if self.fail_facts {
            return Err(fake_failure("facts"));
        }
        Ok(numbers
            .iter()
            .filter_map(|number| {
                let facts = self
                    .pull_requests
                    .values()
                    .find(|pull| pull.number == *number)
                    .map_or_else(
                        || {
                            self.vanished_states.get(number).map(|state| PullFacts {
                                pull: vanished_pull(*number, state.clone()),
                                details: PullDetails::default(),
                                newest_comment: self.newest_comments.get(number).cloned(),
                            })
                        },
                        |pull| {
                            Some(PullFacts {
                                pull: pull.clone(),
                                details: PullDetails {
                                    review_predates_head: Some(self.stale_reviews.contains(number)),
                                    checks: self.checks.get(number).cloned(),
                                    diff: None,
                                    head_ref_deleted: None,
                                    tip_commit_empty: None,
                                },
                                newest_comment: self.newest_comments.get(number).cloned(),
                            })
                        },
                    );
                facts.map(|facts| (*number, facts))
            })
            .collect())
    }

    fn pull_timeline(
        &self,
        _repo: &Path,
        _target: &RepoIdentity,
        number: u64,
    ) -> Result<Vec<TimelineEvent>, ForgeError> {
        if self.fail_timeline {
            return Err(fake_failure("timeline"));
        }
        Ok(self.timeline.get(&number).cloned().unwrap_or_default())
    }

    fn consumer_head(&self, _repo: &Path, slug: &str) -> Result<ConsumerHead, ForgeError> {
        self.consumer_head_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_consumer_head {
            return Err(fake_failure("consumer head"));
        }
        self.heads
            .get(slug)
            .cloned()
            .ok_or_else(|| ForgeError::Query {
                detail: format!("fake consumer head not configured for {slug}"),
            })
    }

    fn file_at(
        &self,
        _repo: &Path,
        slug: &str,
        commit: &str,
        path: &str,
    ) -> Result<Option<String>, ForgeError> {
        self.file_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_file_at {
            return Err(fake_failure("consumer file"));
        }
        Ok(self
            .files
            .get(&(slug.to_owned(), commit.to_owned(), path.to_owned()))
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]

    use super::*;
    use crate::forge::{Forge, PullRequest, RepoIdentity};
    #[test]
    fn the_fake_sweep_is_newest_first_and_reports_overflow() {
        let pull_requests = BTreeMap::from([
            (
                BranchName::new("feat/older"),
                PullRequest {
                    number: 7,
                    updated_at: "2026-08-01T00:00:00Z".to_owned(),
                    ..PullRequest::default()
                },
            ),
            (
                BranchName::new("feat/newer"),
                PullRequest {
                    number: 9,
                    updated_at: "2026-08-02T00:00:00Z".to_owned(),
                    ..PullRequest::default()
                },
            ),
        ]);
        let fake = FakeForge {
            pull_requests,
            sweep_overflows: true,
            ..FakeForge::default()
        };
        let target = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };

        let sweep = fake.sweep(Path::new("/tmp"), &target).expect("sweep");

        assert!(sweep.has_next_page);
        assert_eq!(
            sweep
                .entries
                .iter()
                .map(|entry| entry.number)
                .collect::<Vec<_>>(),
            vec![9, 7]
        );
    }

    #[test]
    fn fake_facts_answer_the_universe_the_vanished_and_nothing_else() {
        let pull = PullRequest {
            number: 7,
            state: "OPEN".to_owned(),
            head_ref_name: "feat/known".to_owned(),
            ..PullRequest::default()
        };
        let fake = FakeForge {
            pull_requests: BTreeMap::from([(BranchName::new("feat/known"), pull)]),
            vanished_states: BTreeMap::from([(8, "CLOSED".to_owned())]),
            newest_comments: BTreeMap::from([(7, "2026-08-03T00:00:00Z".to_owned())]),
            ..FakeForge::default()
        };
        let target = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };

        let facts = fake
            .pull_facts(Path::new("/tmp"), &target, &[7, 8, 9])
            .expect("facts");

        assert_eq!(facts[&7].pull.head_ref_name, "feat/known");
        assert_eq!(
            facts[&7].newest_comment.as_deref(),
            Some("2026-08-03T00:00:00Z")
        );
        assert_eq!(facts[&8].pull.state, "CLOSED");
        assert!(!facts.contains_key(&9));
    }

    #[test]
    fn fake_timeline_answers_configured_events_and_leaves_other_numbers_empty() {
        let event = crate::forge::TimelineEvent {
            at: "2026-08-30T22:43:13Z".to_owned(),
            kind: crate::forge::TimelineEventKind::HeadDeleted,
        };
        let fake = FakeForge {
            timeline: BTreeMap::from([(7, vec![event.clone()])]),
            ..FakeForge::default()
        };
        let target = RepoIdentity {
            name_with_owner: "fake-owner/fake-repo".to_owned(),
            id: "FAKEID".to_owned(),
        };

        assert_eq!(
            fake.pull_timeline(Path::new("/tmp"), &target, 7)
                .expect("configured timeline"),
            vec![event]
        );
        assert!(
            fake.pull_timeline(Path::new("/tmp"), &target, 8)
                .expect("unconfigured timeline")
                .is_empty()
        );
    }
}
