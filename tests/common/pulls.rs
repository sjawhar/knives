#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot proceed IS the test failure"
)]
#![allow(
    unreachable_pub,
    dead_code,
    reason = "a test fixture included by path; not every test target uses every helper"
)]

use knives::forge::PullRequest;

/// A pull request with every field the binary requires, so integration tests
/// (which cannot see the crate's `#[cfg(test)]` `Default`) state deltas only.
/// Override `updated_at`/`head_ref_oid`/owner per scenario via struct update.
pub fn pull_request(number: u64, state: &str, branch: &str) -> PullRequest {
    PullRequest {
        number,
        state: state.to_owned(),
        review_decision: String::new(),
        head_ref_name: branch.to_owned(),
        head_ref_oid: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        updated_at: "2026-01-01T00:00:00Z".to_owned(),
        is_draft: false,
        url: String::new(),
        head_repository_owner: None,
        mergeable: Some(String::new()),
        merge_state_status: Some(String::new()),
        base_ref_name: Some("main".to_owned()),
        merge_commit: None,
    }
}

/// [`pull_request`] at a stated head, updated later than the base fixture
/// (2026-08-15 against 2026-01-01), so it wins a newest-wins comparison.
pub fn pull_request_with_head(number: u64, state: &str, branch: &str, head: &str) -> PullRequest {
    PullRequest {
        head_ref_oid: head.to_owned(),
        updated_at: "2026-08-15T00:00:00Z".to_owned(),
        ..pull_request(number, state, branch)
    }
}
