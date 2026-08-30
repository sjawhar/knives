//! A fake `gh` speaking the snapshot protocol: identity, cold list, warm sweep, by-number facts — plus an always-failing variant. Shared by every integration test that drives the real binary against a scripted forge.

#![allow(
    clippy::expect_used,
    reason = "a fixture that cannot proceed IS the test failure"
)]
#![allow(
    unreachable_pub,
    dead_code,
    reason = "a test fixture included by path; not every test target uses every helper"
)]

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// Install a fake `gh` that answers the full snapshot protocol: repository
/// identity, cold list, warm sweep, and by-number facts.
pub fn install_snapshot_gh(shim: &Path, pulls: &str, withheld_facts: &[u64], log: Option<&Path>) {
    install_snapshot_gh_configured(
        shim,
        pulls,
        SnapshotGhOptions {
            withheld_facts,
            log,
            timeline_nodes: None,
        },
    );
}

/// Install the snapshot shim with an optional canned pull-request timeline.
pub fn install_snapshot_gh_with_timeline(shim: &Path, pulls: &str, timeline_nodes: Option<&str>) {
    install_snapshot_gh_configured(
        shim,
        pulls,
        SnapshotGhOptions {
            withheld_facts: &[],
            log: None,
            timeline_nodes,
        },
    );
}

#[derive(Clone, Copy)]
struct SnapshotGhOptions<'a> {
    withheld_facts: &'a [u64],
    log: Option<&'a Path>,
    timeline_nodes: Option<&'a str>,
}

fn install_snapshot_gh_configured(shim: &Path, pulls: &str, options: SnapshotGhOptions<'_>) {
    let SnapshotGhOptions {
        withheld_facts,
        log,
        timeline_nodes,
    } = options;
    let pulls: Vec<serde_json::Value> =
        serde_json::from_str(pulls).expect("parse fake pull request payload");
    let sweep_entries = pulls
        .iter()
        .map(|pull| {
            serde_json::json!({
                "number": pull.get("number"),
                "updatedAt": pull.get("updatedAt"),
                "state": pull.get("state"),
            })
        })
        .collect::<Vec<_>>();
    let mut fact_rows = serde_json::Map::new();
    for pull in &pulls {
        let number = pull["number"].as_u64().expect("pull request number");
        if !withheld_facts.contains(&number) {
            let _ = fact_rows.insert(format!("p{number}"), pull.clone());
        }
    }
    let identity = shim.join("identity.json");
    let list = shim.join("pulls.json");
    let sweep = shim.join("sweep.json");
    let facts_payload = shim.join("facts.json");
    let timeline_payload = shim.join("timeline.json");
    std::fs::write(
        &identity,
        serde_json::to_vec(&serde_json::json!({
            "nameWithOwner": "fake-owner/fake-repo",
            "id": "FAKEID",
        }))
        .expect("serialize identity payload"),
    )
    .expect("write identity payload");
    std::fs::write(
        &list,
        serde_json::to_vec(&pulls).expect("serialize list payload"),
    )
    .expect("write pull request payload");
    std::fs::write(
        &sweep,
        serde_json::to_vec(&serde_json::json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "pageInfo": {"hasNextPage": false},
                        "nodes": sweep_entries,
                    }
                }
            }
        }))
        .expect("serialize sweep payload"),
    )
    .expect("write sweep payload");
    std::fs::write(
        &facts_payload,
        serde_json::to_vec(&serde_json::json!({
            "data": {"repository": fact_rows},
        }))
        .expect("serialize facts payload"),
    )
    .expect("write facts payload");
    write_timeline_payload(&timeline_payload, timeline_nodes);
    let log_line = log.map_or_else(String::new, |log| {
        format!("printf '%s\\n' \"$*\" >> \"{}\"\n", log.display())
    });
    let gh = shim.join("gh");
    install_executable(
        &gh,
        &format!(
            "#!/bin/sh\n{log_line}case \" $* \" in\n\
             *\" repo view \"*) cat \"{}\" ;;\n\
             *\" pr list \"*) cat \"{}\" ;;\n\
             *\" api graphql \"*)\n\
               case \"$*\" in\n\
                 *\"timelineItems\"*) cat \"{}\" ;;\n\
                 *\"pullRequest(number:\"*) cat \"{}\" ;;\n\
                 *\"orderBy: {{field: UPDATED_AT\"*) cat \"{}\" ;;\n\
                 *) echo \"unexpected GraphQL invocation: $*\" >&2; exit 1 ;;\n\
               esac ;;\n\
             *) echo \"unexpected gh invocation: $*\" >&2; exit 1 ;;\n\
             esac\n",
            identity.display(),
            list.display(),
            timeline_payload.display(),
            facts_payload.display(),
            sweep.display(),
        ),
    );
}
fn write_timeline_payload(path: &Path, timeline_nodes: Option<&str>) {
    let timeline_nodes: Vec<serde_json::Value> = timeline_nodes.map_or_else(Vec::new, |nodes| {
        serde_json::from_str(nodes).expect("parse fake timeline payload")
    });
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "timelineItems": {
                            "pageInfo": {"hasPreviousPage": false},
                            "nodes": timeline_nodes,
                        }
                    }
                }
            }
        }))
        .expect("serialize timeline payload"),
    )
    .expect("write timeline payload");
}

pub fn install_failing_gh(shim: &Path, log: &Path) {
    let gh = shim.join("gh");
    install_executable(
        &gh,
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"{}\"\nexit 1\n",
            log.display()
        ),
    );
}

pub fn path_with_gh_shim(shim: &Path) -> std::ffi::OsString {
    std::env::join_paths(
        std::iter::once(shim.to_owned()).chain(std::env::split_paths(
            &std::env::var_os("PATH").expect("PATH is set"),
        )),
    )
    .expect("construct shim PATH")
}

/// One forge pull request record with the fields the binary requires.
pub fn pull_record(number: u64, state: &str, branch: &str, merge_oid: Option<&str>) -> String {
    let merge = merge_oid.map_or_else(String::new, |oid| {
        format!(",\"mergeCommit\":{{\"oid\":\"{oid}\"}}")
    });
    pull_record_with_fields(number, state, branch, &merge)
}

/// One forge pull request record with additional raw JSON fields.
///
/// `extra_fields` begins with a comma when non-empty, so test scenarios can
/// add facts to a single row without changing existing callers.
pub fn pull_record_with_fields(
    number: u64,
    state: &str,
    branch: &str,
    extra_fields: &str,
) -> String {
    format!(
        "{{\"number\":{number},\"state\":\"{state}\",\"headRefName\":\"{branch}\",\
         \"headRefOid\":\"0123456789abcdef0123456789abcdef01234567\",\
         \"updatedAt\":\"2026-08-07T00:00:00Z\",\"baseRefName\":\"main\"{extra_fields}}}"
    )
}

fn install_executable(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write gh shim");
    let mut permissions = std::fs::metadata(path)
        .expect("read gh shim permissions")
        .permissions();
    PermissionsExt::set_mode(&mut permissions, 0o755);
    std::fs::set_permissions(path, permissions).expect("make gh shim executable");
}
