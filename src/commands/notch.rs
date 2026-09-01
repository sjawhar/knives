//! `knives notch`: read what happened here, or add to it.
//!
//! Two moods on one command, split by `-m`, because reading and writing the same
//! record are the same act from opposite ends. Reading is intentional: nothing
//! injects notches into a session, so the bare form has to answer the question an
//! agent actually has — what happened in this fork lately — rather than making
//! them name a subject they do not know yet.

use crate::cli::Exit;
use crate::config::{default_config_path, load};
use crate::ids::{BranchName, BranchTarget, RepoName};
use crate::ledger::{
    Draft, Entry, EntryClass, Filter, Kind, Ledger, LedgerError, Scribe, VerifyFlag,
    body_human_text, inline_human_text, select, verify_entries,
};
use crate::store::{Store, default_state_path};

/// How many entries a bare read shows.
///
/// A cap on the unfiltered view only: a reader who named a subject or a pull
/// request asked for that chronology and gets all of it.
const RECENT: usize = 20;

/// Machine-event summary appended to a bare human discovery read.
#[derive(Debug, serde::Serialize)]
pub struct EventsFold {
    pub count: usize,
    pub newest_ts: String,
    pub newest_text: String,
}

#[derive(Debug, serde::Serialize)]
#[serde(untagged)]
pub enum Report {
    Read {
        repo: String,
        entries: Vec<Entry>,
        matched: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        events: Option<EventsFold>,
    },
    Written {
        #[serde(skip)]
        repo: String,
        wrote: Entry,
    },
    Verified {
        repo: String,
        checked: usize,
        flags: Vec<VerifyFlag>,
    },
}

/// What one invocation asks for.
#[derive(Debug)]
pub struct Request<'a> {
    pub repo: &'a RepoName,
    pub subject: Option<&'a str>,
    /// Present for a write, absent for a read.
    pub message: Option<&'a str>,
    pub evidence: &'a [String],
    pub pr: Option<u64>,
    pub disposition: Option<&'a str>,
    pub dispositions: bool,
    pub events: bool,
    pub verify: bool,
}

/// Entries for one repository, filtered.
pub fn read(ledger: &Ledger, repo: &RepoName, filter: &Filter<'_>) -> Result<Report, LedgerError> {
    let entries = ledger.entries()?;
    let (selected, matched) = select(&entries, filter);
    Ok(Report::Read {
        repo: repo.to_string(),
        entries: selected.into_iter().cloned().collect(),
        matched,
        events: None,
    })
}

pub fn render(report: &Report) -> String {
    match report {
        Report::Written { repo, wrote } => wrote_line(repo, wrote),
        Report::Verified {
            repo,
            checked: _,
            flags,
        } if flags.is_empty() => format!("{repo}  no evidence flags"),
        Report::Verified { repo, flags, .. } => flags
            .iter()
            .map(|flag| {
                let subject = flag
                    .subject
                    .as_deref()
                    .map_or_else(|| "(this repo)".to_owned(), inline_human_text);
                format!(
                    "{repo}  {}  {subject}  {}",
                    flag.ts,
                    inline_human_text(&flag.what)
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Report::Read {
            repo,
            entries,
            matched,
            events,
        } => {
            if entries.is_empty() && events.is_none() {
                return format!("{repo}  no notches yet");
            }
            let mut lines = vec![format!("{repo}  {matched} notch(es)")];
            if *matched > entries.len() {
                lines.push(format!(
                    "  showing the newest {} of {}",
                    entries.len(),
                    matched
                ));
            }
            for entry in entries {
                lines.push(format!(
                    "  {}  {:<5}  {}",
                    entry.ts,
                    entry.kind,
                    heading(entry)
                ));
                lines.push(format!(
                    "    {}",
                    body_human_text(&entry.text).replace('\n', "\n    ")
                ));
                if !entry.evidence.is_empty() {
                    let evidence: Vec<String> = entry
                        .evidence
                        .iter()
                        .map(|item| inline_human_text(item))
                        .collect();
                    lines.push(format!("    evidence  {}", evidence.join(", ")));
                }
            }
            if let Some(events) = events {
                lines.push(format!(
                    "  + {} machine event(s), newest: {} {}",
                    events.count,
                    events.newest_ts,
                    inline_human_text(&events.newest_text)
                ));
            }
            lines.join("\n")
        }
    }
}

/// Subject, anchor and stated pull request on one line, each omitted when absent.
fn heading(entry: &Entry) -> String {
    let subject = entry
        .subject
        .as_deref()
        .map_or_else(|| "(this repo)".to_owned(), inline_human_text);
    let mut parts = vec![subject];
    if let Some(anchor) = &entry.anchor {
        parts.push(format!("@{}", short(anchor)));
    }
    if let Some(number) = entry.pr {
        parts.push(format!("#{number}"));
    }
    if let Some(disposition) = &entry.disposition {
        parts.push(format!("[{}]", inline_human_text(disposition)));
    }
    parts.join("  ")
}

fn wrote_line(repo: &str, entry: &Entry) -> String {
    let subject = entry
        .subject
        .as_deref()
        .map_or_else(|| format!("{repo} itself"), inline_human_text);
    entry.anchor.as_deref().map_or_else(
        || format!("notched {subject}"),
        |anchor| format!("notched {subject} at {}", short(anchor)),
    )
}

/// Short form for display. Full ids are correct and unreadable.
fn short(id: &str) -> String {
    inline_human_text(id).chars().take(12).collect()
}

fn pr_subject(subject: Option<&str>) -> Option<u64> {
    subject?.strip_prefix('#')?.parse().ok()
}

fn events_fold(entries: &[&Entry]) -> Option<EventsFold> {
    let newest = entries.last()?;
    Some(EventsFold {
        count: entries.len(),
        newest_ts: newest.ts.clone(),
        newest_text: newest.text.clone(),
    })
}

fn read_filtered(
    ledger: &Ledger,
    repo: &RepoName,
    request: &Request<'_>,
) -> Result<Report, LedgerError> {
    let entries = ledger.entries()?;
    let bare = request.subject.is_none()
        && request.pr.is_none()
        && !request.events
        && !request.dispositions;
    if bare {
        let (notes, matched) = select(
            &entries,
            &Filter {
                only: Some(EntryClass::Note),
                limit: Some(RECENT),
                ..Filter::default()
            },
        );
        let (events, _) = select(
            &entries,
            &Filter {
                only: Some(EntryClass::Event),
                ..Filter::default()
            },
        );
        return Ok(Report::Read {
            repo: repo.to_string(),
            entries: notes.into_iter().cloned().collect(),
            matched,
            events: events_fold(&events),
        });
    }

    let entry_class = if request.events {
        Some(EntryClass::Event)
    } else if request.dispositions {
        Some(EntryClass::Disposition)
    } else {
        None
    };
    let (selected, matched) = select(
        &entries,
        &Filter {
            subject: request.subject,
            pr: request.pr,
            only: entry_class,
            limit: (!request.events
                && !request.dispositions
                && request.subject.is_none()
                && request.pr.is_none())
            .then_some(RECENT),
        },
    );
    Ok(Report::Read {
        repo: repo.to_string(),
        entries: selected.into_iter().cloned().collect(),
        matched,
        events: None,
    })
}

fn verify(
    ledger: &Ledger,
    path: &std::path::Path,
    repo: &RepoName,
    request: &Request<'_>,
) -> anyhow::Result<Report> {
    let entries = ledger.entries()?;
    let (selected, _) = select(
        &entries,
        &Filter {
            subject: request.subject,
            pr: request.pr,
            only: request.dispositions.then_some(EntryClass::Disposition),
            limit: None,
        },
    );
    let selected: Vec<Entry> = selected.into_iter().cloned().collect();
    let visible = crate::jj::commits_matching(path, "all()")?
        .into_iter()
        .map(|commit| commit.as_str().to_owned())
        .collect();
    let tips = crate::jj::Repo::open(path)?
        .bookmark_tips()?
        .into_iter()
        .filter_map(|(reference, tip)| {
            reference
                .is_local()
                .then(|| (reference.branch().to_string(), tip.as_str().to_owned()))
        })
        .collect();
    let flags = verify_entries(&selected, &visible, &tips);
    Ok(Report::Verified {
        repo: repo.to_string(),
        checked: selected.len(),
        flags,
    })
}

pub fn run(request: &Request<'_>, output: crate::cli::Output) -> anyhow::Result<Exit> {
    let registry = load(&default_config_path())?;
    let Some(entry) = registry.get(request.repo) else {
        let known: Vec<String> = registry.names().map(|name| name.to_string()).collect();
        eprintln!("unknown repo {}; known: {}", request.repo, known.join(", "));
        return Ok(Exit::Usage);
    };
    let ledger = Ledger::for_repo(request.repo);
    let report = match request.message {
        Some(text) => {
            // The store is read, never written: a notch changes no intent, and a
            // ledger append needs no store lock.
            let store = Store::open(default_state_path())?;
            let pr = request
                .pr
                .or_else(|| pr_subject(request.subject))
                .or_else(|| {
                    request
                        .subject
                        .filter(|subject| !subject.starts_with('#'))
                        .and_then(|subject| {
                            store.tracked_pull(&BranchTarget::new(
                                request.repo.clone(),
                                BranchName::new(subject),
                            ))
                        })
                });
            let identity = crate::commands::claim::current_identity(&std::env::current_dir()?)?;
            let scribe =
                Scribe::new(ledger, request.repo.clone(), entry.path.clone(), identity.owner);
            let written = scribe.record(&Draft {
                subject: request.subject,
                kind: Kind::Note,
                disposition: request.disposition.map(str::to_owned),
                text: text.to_owned(),
                evidence: request.evidence.to_vec(),
                pr,
            })?;
            Report::Written {
                repo: request.repo.to_string(),
                wrote: written,
            }
        }
        None if request.verify => verify(&ledger, &entry.path, request.repo, request)?,
        None => read_filtered(&ledger, request.repo, request)?,
    };
    if let Some(payload) = crate::cli::machine_payload(output, &report)? {
        println!("{payload}");
    } else {
        println!("{}", render(&report));
    }
    Ok(
        matches!(&report, Report::Verified { flags, .. } if !flags.is_empty())
            .then_some(Exit::Findings)
            .unwrap_or(Exit::Ok),
    )
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a result in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    fn entry(subject: Option<&str>, kind: Kind, text: &str) -> Entry {
        Entry {
            ts: "2026-08-15T22:14:03Z".to_owned(),
            owner: "ses_fff688".to_owned(),
            subject: subject.map(str::to_owned),
            kind,
            disposition: None,
            text: text.to_owned(),
            evidence: Vec::new(),
            anchor: Some("6c42fe71aaaaaaaa".to_owned()),
            pr: Some(1157),
        }
    }

    #[test]
    fn a_read_names_the_subject_the_anchor_and_the_stated_pull_request() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(
                Some("feat/log-queue"),
                Kind::Note,
                "superseded by #1157",
            )],
            matched: 1,
            events: None,
        };
        let text = render(&report);
        assert!(text.contains("feat/log-queue"), "was: {text}");
        assert!(text.contains("@6c42fe71aaaa"), "was: {text}");
        assert!(text.contains("#1157"), "was: {text}");
        assert!(text.contains("note"), "was: {text}");
        assert!(text.contains("superseded by #1157"), "was: {text}");
    }

    #[test]
    fn human_rendering_escapes_control_characters_from_ledger_fields() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![Entry {
                subject: Some("feat/\u{1b}queue".to_owned()),
                evidence: vec!["review\u{1b}link\rnext".to_owned()],
                anchor: Some("6c42\u{1b}anchor\r".to_owned()),
                ..entry(Some("feat/ignored"), Kind::Note, "parked\u{1b}now\ragain")
            }],
            matched: 1,
            events: None,
        };

        let text = render(&report);
        assert!(!text.contains('\u{1b}'), "was: {text:?}");
        assert!(!text.contains('\r'), "was: {text:?}");
        assert!(text.contains('\u{fffd}'), "was: {text:?}");
    }

    #[test]
    fn body_rendering_preserves_line_breaks_and_escapes_other_controls() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(
                Some("feat/alpha"),
                Kind::Note,
                "one\ntwo\u{1b}three\rfour",
            )],
            matched: 1,
            events: None,
        };

        let text = render(&report);
        assert!(text.contains("    one\n    two"), "was: {text:?}");
        assert!(!text.contains('\u{1b}'), "was: {text:?}");
        assert!(!text.contains('\r'), "was: {text:?}");
        assert!(text.contains('\u{fffd}'), "was: {text:?}");
    }

    #[test]
    fn a_truncated_read_says_how_many_it_did_not_show() {
        // A window that does not announce itself is how a reader concludes a
        // branch has no older history.
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: vec![entry(Some("feat/alpha"), Kind::Event, "claimed")],
            matched: 57,
            events: None,
        };
        assert!(
            render(&report).contains("showing the newest 1 of 57"),
            "was: {}",
            render(&report)
        );
    }

    #[test]
    fn an_empty_read_says_so_rather_than_printing_a_bare_repo_name() {
        let report = Report::Read {
            repo: "a-repo".to_owned(),
            entries: Vec::new(),
            matched: 0,
            events: None,
        };
        assert_eq!(render(&report), "a-repo  no notches yet");
    }

    #[test]
    fn an_events_read_returns_every_matching_event_without_a_recent_cap() {
        let directory = tempfile::tempdir().expect("ledger directory");
        let ledger = Ledger::at(directory.path().to_owned());
        for second in 0..21 {
            ledger
                .append(&Entry {
                    ts: format!("2026-08-15T22:14:{second:02}Z"),
                    owner: "test".to_owned(),
                    subject: Some("feat/alpha".to_owned()),
                    kind: Kind::Event,
                    disposition: None,
                    text: format!("event {second}"),
                    evidence: Vec::new(),
                    anchor: None,
                    pr: Some(7),
                })
                .expect("record event");
        }
        ledger
            .append(&Entry {
                ts: "2026-08-15T22:15:00Z".to_owned(),
                owner: "test".to_owned(),
                subject: Some("feat/alpha".to_owned()),
                kind: Kind::Note,
                disposition: None,
                text: "human note".to_owned(),
                evidence: Vec::new(),
                anchor: None,
                pr: Some(7),
            })
            .expect("record note");
        let repo = RepoName::new("demo");
        let request = Request {
            repo: &repo,
            subject: Some("feat/alpha"),
            message: None,
            evidence: &[],
            pr: Some(7),
            disposition: None,
            dispositions: false,
            events: true,
            verify: false,
        };

        let report = read_filtered(&ledger, &repo, &request).expect("read events");
        let Report::Read {
            entries, matched, ..
        } = report
        else {
            panic!("expected a read report");
        };
        assert_eq!(matched, 21);
        assert_eq!(entries.len(), 21);
        assert!(entries.iter().all(|entry| entry.kind == Kind::Event));
    }
    #[test]
    fn a_repo_level_entry_is_headed_by_the_repo_rather_than_an_empty_subject() {
        let report = Report::Written {
            repo: "a-repo".to_owned(),
            wrote: Entry {
                anchor: None,
                pr: None,
                ..entry(None, Kind::Note, "the fork needs a cut")
            },
        };
        assert_eq!(render(&report), "notched a-repo itself");
    }
}
