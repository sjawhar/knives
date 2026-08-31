//! `knives consumers`: compare consumer pins with live published releases.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::cli::Exit;
use crate::config::{RepoEntry, Role};
use crate::ids::{CommitId, ReleaseScheme, RepoName, strict_dated_release};
use crate::jj::{self, Repo};
use crate::pins::{Pin, PinVerdict};
use crate::release_model::{newest_release, repo_slug, scan_consumer_for};

/// Inputs for one fork's consumer-pin census.
#[derive(Debug)]
pub struct Request<'a> {
    pub fork: &'a RepoName,
    pub entry: &'a RepoEntry,
    pub consumers: &'a [PathBuf],
}

/// The release against which the census classified pins.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Release {
    pub reference: String,
    pub commit: String,
    pub source: String,
}

/// One pin observed in a consumer checkout.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PinRow {
    pub file: String,
    pub line: usize,
    pub reference: String,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<PinVerdict>,
}

/// One checkout that may consume the fork's releases.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConsumerRow {
    pub path: String,
    pub pins: Vec<PinRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

/// Consumer pins and the live release refs that answered for them.
#[derive(Debug, serde::Serialize)]
pub struct Report {
    pub fork: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub newest: Option<Release>,
    pub consumers: Vec<ConsumerRow>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
}

struct Positions {
    refs: BTreeMap<String, CommitId>,
    newest: Option<Release>,
    notes: Vec<String>,
    problems: Vec<String>,
}

struct ConsumerContext<'a> {
    fork: &'a str,
    slug: Option<&'a str>,
    scheme: &'a ReleaseScheme,
    live: &'a BTreeMap<String, CommitId>,
    newest: Option<&'a Release>,
}

/// Classify every registered and explicitly named consumer against live release refs.
pub fn gather(request: &Request<'_>) -> Report {
    let scheme = request.entry.release_scheme();
    let positions = positions(request.entry, &scheme);
    let slug = repo_slug(request.entry);
    let context = ConsumerContext {
        fork: request.fork.as_str(),
        slug: slug.as_deref(),
        scheme: &scheme,
        live: &positions.refs,
        newest: positions.newest.as_ref(),
    };
    let mut consumers = request
        .consumers
        .iter()
        .map(|consumer| consumer_row(consumer, &context))
        .collect::<Vec<_>>();
    consumers.sort_by(|left, right| left.path.cmp(&right.path));
    let mut notes = positions.notes;
    add_skew_note(&consumers, &mut notes);
    Report {
        fork: request.fork.to_string(),
        newest: positions.newest,
        consumers,
        notes,
        problems: positions.problems,
    }
}

fn positions(entry: &RepoEntry, scheme: &ReleaseScheme) -> Positions {
    let mut notes = Vec::new();
    let mut problems = Vec::new();
    let local = local_positions(entry, scheme, &mut problems);
    let pattern = release_pattern(scheme);
    let remote = entry.remote(Role::Release);
    match jj::remote_refs(remote, &[&pattern]) {
        Ok(refs) => {
            let newest = newest_live(&refs, scheme).map(|(reference, commit)| Release {
                reference,
                commit: commit.to_string(),
                source: "live".to_owned(),
            });
            if newest.is_none() {
                problems.push(format!(
                    "release remote {remote} has no configured release ref"
                ));
            }
            if let (Some(local), Some(remote)) = (local.newest.as_ref(), newest.as_ref())
                && let Some(note) = local_remote_skew_note(local, remote)
            {
                notes.push(note);
            }
            Positions {
                refs,
                newest,
                notes,
                problems,
            }
        }
        Err(error) => {
            problems.push(format!(
                "could not read live release refs from {remote}: {error}"
            ));
            if let Some(newest) = local.newest.as_ref() {
                notes.push(format!(
                    "live release refs unavailable; local view has {} @ {}, but pins are unclassified",
                    newest.reference, newest.commit
                ));
            }
            Positions {
                refs: BTreeMap::new(),
                newest: None,
                notes,
                problems,
            }
        }
    }
}

fn local_remote_skew_note(local: &Release, remote: &Release) -> Option<String> {
    (local.reference != remote.reference || local.commit != remote.commit).then(|| {
        format!(
            "local view has {} @ {}, remote has {} @ {} — the checkout is behind or ahead of the remote",
            local.reference, local.commit, remote.reference, remote.commit
        )
    })
}

struct LocalPositions {
    newest: Option<Release>,
}

fn local_positions(
    entry: &RepoEntry,
    scheme: &ReleaseScheme,
    problems: &mut Vec<String>,
) -> LocalPositions {
    let Ok(repo) = Repo::open(&entry.path) else {
        problems.push(format!(
            "could not open local checkout {}",
            entry.path.display()
        ));
        return LocalPositions { newest: None };
    };
    let Ok(tips) = repo.bookmark_tips() else {
        problems.push(format!(
            "could not read local release refs from {}",
            entry.path.display()
        ));
        return LocalPositions { newest: None };
    };
    let newest =
        newest_release(&tips, scheme, entry.publish_remote()).map(|(reference, commit)| Release {
            reference: reference.branch().to_string(),
            commit: commit.to_string(),
            source: "local".to_owned(),
        });
    LocalPositions { newest }
}

fn release_pattern(scheme: &ReleaseScheme) -> String {
    match scheme {
        ReleaseScheme::Dated => "refs/heads/release/*".to_owned(),
        ReleaseScheme::Fixed(name) => format!("refs/heads/{name}"),
    }
}

fn newest_live(
    refs: &BTreeMap<String, CommitId>,
    scheme: &ReleaseScheme,
) -> Option<(String, CommitId)> {
    match scheme {
        ReleaseScheme::Dated => refs
            .iter()
            .filter_map(|(full, commit)| {
                let name = full.strip_prefix("refs/heads/")?;
                strict_dated_release(name).map(|order| (order, name, commit))
            })
            .max_by_key(|(order, _, _)| order.clone())
            .map(|(_, name, commit)| (name.to_owned(), commit.clone())),
        ReleaseScheme::Fixed(name) => refs
            .get(&format!("refs/heads/{name}"))
            .cloned()
            .map(|commit| (name.to_string(), commit)),
    }
}

fn consumer_row(consumer: &Path, context: &ConsumerContext<'_>) -> ConsumerRow {
    let path = consumer.display().to_string();
    if !consumer.exists() {
        return ConsumerRow {
            path,
            pins: Vec::new(),
            notes: Vec::new(),
            problem: Some("not found".to_owned()),
        };
    }
    if !consumer.is_dir() {
        return ConsumerRow {
            path,
            pins: Vec::new(),
            notes: Vec::new(),
            problem: Some("not a directory".to_owned()),
        };
    }
    let scan = scan_consumer_for(consumer, context.slug, context.scheme);
    let mut notes = scan.notes;
    let problem = (!scan.problems.is_empty()).then(|| scan.problems.join("; "));
    if problem.is_none() && scan.pins.is_empty() {
        notes.push(format!("does not pin {}", context.fork));
    } else if !scan.pins.is_empty() && context.newest.is_none() {
        notes.push("cannot classify pins: no newest release is available".to_owned());
    }
    ConsumerRow {
        path,
        pins: scan
            .pins
            .iter()
            .map(|pin| pin_row(pin, context.live, context.newest))
            .collect(),
        notes,
        problem,
    }
}

fn pin_row(pin: &Pin, live: &BTreeMap<String, CommitId>, newest: Option<&Release>) -> PinRow {
    PinRow {
        file: pin.file.clone(),
        line: pin.line,
        reference: pin.reference.clone(),
        kind: pin.kind.to_string(),
        locked: pin.locked.clone(),
        verdict: newest.map(|newest| verdict(pin, live, &newest.reference)),
    }
}

/// Compare one pin with the known live release refs and their newest member.
pub fn verdict(pin: &Pin, live: &BTreeMap<String, CommitId>, newest: &str) -> PinVerdict {
    let reference = format!("refs/heads/{}", pin.reference);
    let Some(commit) = live.get(&reference) else {
        return PinVerdict::UnknownName;
    };
    if pin.reference != newest {
        return PinVerdict::BehindName {
            newest: newest.to_owned(),
        };
    }
    match &pin.locked {
        Some(locked) if !commit.as_str().starts_with(locked) => PinVerdict::StaleLock {
            expected: commit.to_string(),
        },
        Some(_) | None => PinVerdict::Current,
    }
}

fn add_skew_note(consumers: &[ConsumerRow], notes: &mut Vec<String>) {
    let mut pins = BTreeMap::new();
    for consumer in consumers {
        for pin in &consumer.pins {
            pins.entry(pin.reference.clone())
                .or_insert_with(|| consumer_label(Path::new(&consumer.path)));
        }
    }
    if pins.len() > 1 {
        let detail = pins
            .into_iter()
            .map(|(reference, consumer)| format!("{consumer} pins {reference}"))
            .collect::<Vec<_>>()
            .join(", ");
        notes.push(format!("consumers disagree: {detail}"));
    }
}

fn consumer_label(path: &Path) -> String {
    path.parent()
        .and_then(Path::file_name)
        .or_else(|| path.file_name())
        .map_or_else(
            || path.display().to_string(),
            |name| name.to_string_lossy().into_owned(),
        )
}

/// The command outcome, with unanswered checks outranking actionable pin findings.
pub fn exit_for(report: &Report) -> Exit {
    if !report.problems.is_empty()
        || report
            .consumers
            .iter()
            .any(|consumer| consumer.problem.is_some())
    {
        return Exit::Incomplete;
    }
    if report
        .consumers
        .iter()
        .flat_map(|consumer| &consumer.pins)
        .any(|pin| {
            matches!(
                pin.verdict,
                Some(
                    PinVerdict::StaleLock { .. }
                        | PinVerdict::BehindName { .. }
                        | PinVerdict::UnknownName
                )
            )
        })
    {
        Exit::Findings
    } else {
        Exit::Ok
    }
}

/// Render a compact census for an interactive terminal.
pub fn render(report: &Report) -> String {
    let mut lines = report.newest.as_ref().map_or_else(
        || format!("{}: newest release unavailable", report.fork),
        |newest| {
            format!(
                "{}: newest {} @ {} (release remote, {})",
                report.fork,
                newest.reference,
                short(&newest.commit),
                newest.source
            )
        },
    );
    for problem in &report.problems {
        let _ = write!(lines, "\n  PROBLEM: {problem}");
    }
    for consumer in &report.consumers {
        let _ = write!(lines, "\n  {}", consumer.path);
        if let Some(problem) = &consumer.problem {
            let _ = write!(lines, ": PROBLEM: {problem}");
            continue;
        }
        lines.push(':');
        for pin in &consumer.pins {
            let _ = write!(
                lines,
                "\n    {}:{}  {}  {}{}  {}",
                pin.file,
                pin.line,
                pin.reference,
                pin.kind,
                pin.locked
                    .as_deref()
                    .map_or_else(String::new, |locked| format!("  @{}", short(locked))),
                pin.verdict
                    .as_ref()
                    .map_or_else(|| "unclassified".to_owned(), render_verdict)
            );
        }
        for note in &consumer.notes {
            let _ = write!(lines, "\n    {note}");
        }
    }
    for note in &report.notes {
        let _ = write!(lines, "\n  note: {note}");
    }
    lines
}

fn render_verdict(verdict: &PinVerdict) -> String {
    match verdict {
        PinVerdict::Current => "current".to_owned(),
        PinVerdict::StaleLock { expected } => format!("stale lock: expected @{}", short(expected)),
        PinVerdict::BehindName { newest } => format!("behind: newest is {newest}"),
        PinVerdict::UnknownName => "unknown release".to_owned(),
    }
}

fn short(value: &str) -> String {
    value.chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{ConsumerContext, Release, consumer_row, local_remote_skew_note, verdict};
    use crate::ids::{CommitId, ReleaseScheme};
    use crate::pins::{Pin, PinKind, PinVerdict};

    fn pin(reference: &str, locked: Option<&str>) -> Pin {
        Pin {
            file: "uv.lock".to_owned(),
            line: 1,
            reference: reference.to_owned(),
            kind: PinKind::Frozen,
            locked: locked.map(str::to_owned),
            source: String::new(),
        }
    }

    fn release(reference: &str, commit: &str) -> Release {
        Release {
            reference: reference.to_owned(),
            commit: commit.to_owned(),
            source: "test".to_owned(),
        }
    }

    fn live() -> BTreeMap<String, CommitId> {
        BTreeMap::from([
            (
                "refs/heads/release/2026-08-04".to_owned(),
                CommitId::new("aabbccddeeff00112233445566778899"),
            ),
            (
                "refs/heads/release/2026-08-05".to_owned(),
                CommitId::new("11223344556677889900aabbccddeeff"),
            ),
        ])
    }

    #[test]
    fn a_malformed_lock_fragment_makes_the_consumer_incomplete_instead_of_current() {
        let consumer = tempfile::tempdir().expect("create consumer");
        std::fs::write(
            consumer.path().join("uv.lock"),
            "tool = { git = \"https://forge.invalid/acme/tool.git?rev=release%2F2026-08-05#12345\" }\n",
        )
        .expect("write malformed pin");
        let newest = release("release/2026-08-05", "11223344556677889900aabbccddeeff");
        let scheme = ReleaseScheme::Dated;
        let live = live();
        let context = ConsumerContext {
            fork: "demo",
            slug: Some("tool"),
            scheme: &scheme,
            live: &live,
            newest: Some(&newest),
        };

        let row = consumer_row(consumer.path(), &context);

        assert!(row.pins.is_empty());
        assert_eq!(
            row.problem.as_deref(),
            Some("uv.lock:1: malformed locked commit fragment `12345`")
        );
        assert!(!row.notes.iter().any(|note| note == "does not pin demo"));
    }

    #[test]
    fn verdict_marks_a_missing_release_name_unknown() {
        let found = verdict(
            &pin("release/2026-08-03", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::UnknownName);
    }

    #[test]
    fn verdict_marks_a_known_older_release_behind() {
        let found = verdict(
            &pin("release/2026-08-04", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(
            found,
            PinVerdict::BehindName {
                newest: "release/2026-08-05".to_owned()
            }
        );
    }

    #[test]
    fn verdict_marks_an_unlocked_newest_release_current() {
        let found = verdict(
            &pin("release/2026-08-05", None),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::Current);
    }

    #[test]
    fn verdict_accepts_a_short_lock_prefix_for_the_newest_release() {
        let found = verdict(
            &pin("release/2026-08-05", Some("11223344")),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(found, PinVerdict::Current);
    }

    #[test]
    fn verdict_marks_a_mismatched_newest_lock_stale() {
        let found = verdict(
            &pin("release/2026-08-05", Some("deadbeef")),
            &live(),
            "release/2026-08-05",
        );

        assert_eq!(
            found,
            PinVerdict::StaleLock {
                expected: "11223344556677889900aabbccddeeff".to_owned()
            }
        );
    }
    #[test]
    fn an_advanced_live_ref_with_the_same_name_records_view_disagreement() {
        let local = release("release/2026-08-05", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let remote = release("release/2026-08-05", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");

        assert_eq!(
            local_remote_skew_note(&local, &remote),
            Some(
                "local view has release/2026-08-05 @ aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa, remote has release/2026-08-05 @ bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb — the checkout is behind or ahead of the remote"
                    .to_owned()
            )
        );
    }
}
