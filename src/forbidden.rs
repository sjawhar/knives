//! Configured identifiers found in the lines a branch adds over the upstream trunk.
//!
//! A fact for `knives audit`: an upstream-bound diff that names our fork, our
//! product or our infrastructure is something a maintainer will ask about.
//! Case-insensitive substrings over added lines only; context and removed
//! lines are upstream's own text.

/// One added line that names a configured term.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Hit {
    /// The new-side path, as the `+++ b/` header names it.
    pub file: String,
    /// Line number in the new file, from the hunk header onward.
    pub line: usize,
    /// The configured term, as configured (not as it appears in the line).
    pub term: String,
    /// The added line without its leading `+`.
    pub text: String,
}

/// Every added line of a `--git` diff that contains one of `terms`.
///
/// The diff is read as `git diff` writes it: `+++ b/<path>` names the file the
/// following hunks add to, `@@ -a,b +c,d @@` restarts the new-side line count at
/// `c`, and inside a hunk a `+` line is an addition, a ` ` line is context (both
/// advance the new-side count), and a `-` line is a removal (which does not).
/// A `+++ /dev/null` header is a deletion: nothing after it is an addition.
pub fn scan(diff: &str, terms: &[String]) -> Vec<Hit> {
    if terms.is_empty() {
        return Vec::new();
    }
    let lowered: Vec<(String, &str)> = terms
        .iter()
        .map(|term| (term.to_lowercase(), term.as_str()))
        .collect();
    let mut hits = Vec::new();
    let mut file: Option<String> = None;
    let mut in_hunk = false;
    let mut next_line = 0usize;
    for raw in diff.lines() {
        if !in_hunk {
            // File headers: `diff --git`, `index`, `new file mode`, `---`, `+++`.
            if let Some(header) = raw.strip_prefix("+++ ") {
                file = new_side_path(header);
            } else if let Some(rest) = raw.strip_prefix("@@ ") {
                next_line = new_start(rest);
                in_hunk = true;
            }
            continue;
        }
        if let Some(rest) = raw.strip_prefix("@@ ") {
            next_line = new_start(rest);
        } else if raw.starts_with("diff --git ") {
            in_hunk = false;
        } else if let Some(text) = raw.strip_prefix('+') {
            let line = next_line;
            next_line += 1;
            if let Some(path) = &file {
                let lowered_text = text.to_lowercase();
                for (needle, term) in &lowered {
                    if lowered_text.contains(needle.as_str()) {
                        hits.push(Hit {
                            file: path.clone(),
                            line,
                            term: (*term).to_owned(),
                            text: text.to_owned(),
                        });
                    }
                }
            }
        } else if raw.starts_with(' ') || raw.is_empty() {
            // Context, which an empty source line may render as "".
            next_line += 1;
        }
        // `-` removals and `\ No newline at end of file` do not advance.
    }
    hits
}

/// The path a `+++` header names, or `None` for a deletion (`/dev/null`).
fn new_side_path(header: &str) -> Option<String> {
    let header = header.split('\t').next().unwrap_or(header);
    if header == "/dev/null" {
        return None;
    }
    Some(header.strip_prefix("b/").unwrap_or(header).to_owned())
}

/// The new-side start line of a hunk header's remainder (`-a,b +c,d @@ …`).
///
/// A header knives cannot read counts from line 0 rather than failing the scan:
/// the term is still reported, only its position is unknown.
fn new_start(rest: &str) -> usize {
    rest.split_whitespace()
        .find_map(|token| token.strip_prefix('+'))
        .and_then(|range| range.split(',').next())
        .and_then(|start| start.parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        reason = "indexing a hit in a test is the assertion; a panic is the failure"
    )]
    use super::*;

    /// Three files: a new one, a modified one with two hunks, and a deletion.
    /// `acme-corp` appears on an added line (twice), on a removed line, in a
    /// context line, and in a `+++ b/acme-corp/…` header.
    const DIFF: &str = "\
diff --git a/acme-corp/new.py b/acme-corp/new.py
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/acme-corp/new.py
@@ -0,0 +1,2 @@
+import os
+HOST = \"https://Acme-Corp.example\"
diff --git a/infra/app.py b/infra/app.py
index 2222222..3333333 100644
--- a/infra/app.py
+++ b/infra/app.py
@@ -1,2 +1,2 @@
 # shared by acme-corp and others
-name = \"acme-corp\"
+name = \"upstream\"
@@ -8,1 +8,2 @@
 context()
+deploy(\"acme-corp\")
diff --git a/old.txt b/old.txt
deleted file mode 100644
index 4444444..0000000
--- a/old.txt
+++ /dev/null
@@ -1,1 +0,0 @@
-acme-corp was here
";

    #[test]
    fn only_added_lines_are_hits_with_new_file_line_numbers() {
        let hits = scan(DIFF, &["acme-corp".to_owned()]);

        let positions: Vec<(&str, usize, &str)> = hits
            .iter()
            .map(|hit| (hit.file.as_str(), hit.line, hit.term.as_str()))
            .collect();
        assert_eq!(
            positions,
            [
                ("acme-corp/new.py", 2, "acme-corp"),
                ("infra/app.py", 9, "acme-corp"),
            ],
            "was: {hits:?}"
        );
        assert_eq!(hits[0].text, "HOST = \"https://Acme-Corp.example\"");
        assert_eq!(hits[1].text, "deploy(\"acme-corp\")");
    }

    #[test]
    fn a_term_in_a_header_a_removed_line_or_context_is_not_a_hit() {
        // `import os` (line 1 of the new file) names nothing; the header
        // `+++ b/acme-corp/new.py`, the removed `name = "acme-corp"`, the context
        // `# shared by acme-corp` and the deleted file's `-acme-corp was here` are all
        // upstream's text or metadata, not additions.
        let hits = scan(DIFF, &["acme-corp".to_owned()]);

        assert!(
            hits.iter()
                .all(|hit| hit.text.to_lowercase().contains("acme-corp")),
            "every hit carries the term in its own text: {hits:?}"
        );
        assert!(
            !hits.iter().any(|hit| hit.file == "old.txt"),
            "a deleted file adds nothing: {hits:?}"
        );
        assert!(
            !hits.iter().any(|hit| hit.text == "import os"),
            "a line without the term is not a hit by virtue of its path: {hits:?}"
        );
        assert!(
            !hits.iter().any(|hit| hit.text.starts_with("name = ")),
            "the removed line was not counted, nor its replacement: {hits:?}"
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_reports_the_configured_spelling() {
        let hits = scan(DIFF, &["ACME-CORP".to_owned()]);

        assert_eq!(hits.len(), 2, "was: {hits:?}");
        assert!(hits.iter().all(|hit| hit.term == "ACME-CORP"));
    }

    #[test]
    fn no_terms_means_no_hits() {
        assert_eq!(scan(DIFF, &[]), Vec::<Hit>::new());
    }

    #[test]
    fn every_matching_term_is_its_own_hit() {
        let hits = scan(DIFF, &["acme-corp".to_owned(), "deploy".to_owned()]);

        let on_line_9: Vec<&str> = hits
            .iter()
            .filter(|hit| hit.file == "infra/app.py" && hit.line == 9)
            .map(|hit| hit.term.as_str())
            .collect();
        assert_eq!(on_line_9, ["acme-corp", "deploy"]);
    }
}
