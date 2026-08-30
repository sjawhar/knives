#![allow(
    clippy::expect_used,
    clippy::panic,
    dead_code,
    reason = "the shared lab fixture exposes helpers each isolated test target does not use"
)]

#[path = "common/lab.rs"]
mod lab;

use knives::carriage::{CarryVerdict, CheckInput, Target, TargetRole, check};
use knives::ids::CommitId;
use knives::jj::Repo;
use lab::Lab;

const fn upstream_trunk_target(commit: CommitId) -> Target {
    Target {
        refs: Vec::new(),
        commit,
        role: TargetRole::UpstreamTrunk,
    }
}

fn check_against(repo: &Repo, lab: &Lab, revision: &str, target: &str) -> CarryVerdict {
    let tip = repo.resolve_commit(revision).expect("resolve revision");
    let target = upstream_trunk_target(repo.resolve_commit(target).expect("resolve target"));
    check(
        &CheckInput {
            repo_path: lab.work_path(),
            repo,
            revision,
            tip: &tip,
        },
        &target,
    )
    .expect("check carriage")
    .verdict
}

#[test]
fn a_net_zero_branch_is_carried_rewritten_against_its_base() {
    let lab = Lab::new();
    lab.branch("feat/net-zero", "feature.txt", "added\n");
    lab.jj_work(["new", "-m", "revert feature"]);
    std::fs::remove_file(lab.work_path().join("feature.txt")).expect("remove feature");
    lab.jj_work(["bookmark", "set", "feat/net-zero", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(lab.work_path()).expect("open repository");

    let verdict = check_against(&repo, &lab, "feat/net-zero", "main@upstream");

    assert_eq!(verdict, CarryVerdict::CarriedRewritten);
}

#[test]
fn a_squashed_multicommit_branch_is_carried_despite_an_intermediate_conflict() {
    let lab = Lab::new();
    lab.branch("feat/multi", "feature.txt", "first\n");
    std::fs::write(lab.work_path().join("feature.txt"), "final\n").expect("finish branch");
    lab.jj_work(["bookmark", "set", "feat/multi", "-r", "@"]);
    lab.jj_work(["new"]);
    lab.jj_work(["new", "-r", "main@upstream", "-m", "squash merge"]);
    std::fs::write(lab.work_path().join("feature.txt"), "final\n").expect("write squashed tree");
    lab.jj_work(["bookmark", "create", "target", "-r", "@"]);
    lab.jj_work(["new"]);
    let repo = Repo::open(lab.work_path()).expect("open repository");

    let verdict = check_against(&repo, &lab, "feat/multi", "target");

    assert_eq!(verdict, CarryVerdict::CarriedRewritten);
}
