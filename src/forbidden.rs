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
/// The diff is read as `git diff` writes it: `diff --git` opens a file and
/// closes any hunk, `+++ b/<path>` names the file the following hunks add to,
/// `@@ -a,b +c,d @@` opens a hunk and restarts the new-side line count at `c`,
/// and inside a hunk a `+` line is an addition, a ` ` line is context (both
/// advance the new-side count), and a `-` line is a removal (which does not).
/// A `+++ /dev/null` header is a deletion: nothing after it is an addition.
///
/// A hunk header whose new-side start cannot be read is an error naming the
/// header: a hit at a guessed line would be a fact knives cannot stand behind.
pub fn scan(diff: &str, terms: &[String]) -> Result<Vec<Hit>, String> {
    if terms.is_empty() {
        return Ok(Vec::new());
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
        if let Some(rest) = raw.strip_prefix("@@ ") {
            next_line = new_start(rest).ok_or_else(|| format!("unreadable hunk header {raw:?}"))?;
            in_hunk = true;
            continue;
        }
        if raw.starts_with("diff --git ") {
            in_hunk = false;
            continue;
        }
        if !in_hunk {
            // File headers: `index`, `new file mode`, `---`, `+++`.
            if let Some(header) = raw.strip_prefix("+++ ") {
                file = new_side_path(header);
            }
            continue;
        }
        if let Some(text) = raw.strip_prefix('+') {
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
    Ok(hits)
}

/// The path a `+++` header names, or `None` for a deletion (`/dev/null`).
fn new_side_path(header: &str) -> Option<String> {
    let header = header.split('\t').next().unwrap_or(header);
    if header == "/dev/null" {
        return None;
    }
    Some(header.strip_prefix("b/").unwrap_or(header).to_owned())
}

/// The new-side start line of a hunk header's remainder (`-a,b +c,d @@ …`),
/// or `None` when the remainder carries no readable `+c` range.
fn new_start(rest: &str) -> Option<usize> {
    rest.split_whitespace()
        .find_map(|token| token.strip_prefix('+'))
        .and_then(|range| range.split(',').next())
        .and_then(|start| start.parse().ok())
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        reason = "indexing a hit or unwrapping a scan in a test is the assertion; a panic is the failure"
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

    /// A term that appears only in file headers: the `diff --git` line and the
    /// `+++ b/<path>` line. Nothing added carries it.
    const HEADER_ONLY: &str = "\
diff --git a/acme-corp/new.py b/acme-corp/new.py
new file mode 100644
index 0000000..1111111
--- /dev/null
+++ b/acme-corp/new.py
@@ -0,0 +1,1 @@
+import os
";

    #[test]
    fn only_added_lines_are_hits_with_new_file_line_numbers() {
        let hits = scan(DIFF, &["acme-corp".to_owned()]).unwrap();

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
        // The header `+++ b/acme-corp/new.py` and the `diff --git` line are the
        // only places the term appears: zero hits, not one at line 0.
        assert_eq!(
            scan(HEADER_ONLY, &["acme-corp".to_owned()]).unwrap(),
            Vec::<Hit>::new()
        );

        // The removed `name = "acme-corp"`, the context `# shared by acme-corp`
        // and the deleted file's `-acme-corp was here` are upstream's text.
        let hits = scan(DIFF, &["acme-corp".to_owned()]).unwrap();
        assert!(
            !hits.iter().any(|hit| hit.file == "old.txt"),
            "a deleted file adds nothing: {hits:?}"
        );
        assert!(
            !hits.iter().any(|hit| hit.text.starts_with("name = ")),
            "the removed line was not counted, nor its replacement: {hits:?}"
        );
        assert!(
            !hits.iter().any(|hit| hit.text.contains("shared by")),
            "a context line is not an addition: {hits:?}"
        );
    }

    #[test]
    fn matching_is_case_insensitive_and_reports_the_configured_spelling() {
        let hits = scan(DIFF, &["ACME-CORP".to_owned()]).unwrap();

        assert_eq!(hits.len(), 2, "was: {hits:?}");
        assert!(hits.iter().all(|hit| hit.term == "ACME-CORP"));
    }

    #[test]
    fn no_terms_means_no_hits() {
        assert_eq!(scan(DIFF, &[]).unwrap(), Vec::<Hit>::new());
    }

    #[test]
    fn every_matching_term_is_its_own_hit() {
        let hits = scan(DIFF, &["acme-corp".to_owned(), "deploy".to_owned()]).unwrap();

        let on_line_9: Vec<&str> = hits
            .iter()
            .filter(|hit| hit.file == "infra/app.py" && hit.line == 9)
            .map(|hit| hit.term.as_str())
            .collect();
        assert_eq!(on_line_9, ["acme-corp", "deploy"]);
    }

    #[test]
    fn an_unreadable_hunk_header_is_an_error_naming_the_header() {
        let diff = "\
diff --git a/x.py b/x.py
--- a/x.py
+++ b/x.py
@@ garbage @@
+acme-corp
";
        let error = scan(diff, &["acme-corp".to_owned()]).unwrap_err();
        assert!(error.contains("@@ garbage @@"), "was: {error}");
    }

    #[test]
    fn bytes_that_are_not_utf8_scan_as_replacement_characters_not_terms() {
        // A Latin-1 file adds `caf\xe9`: decoded lossily, the byte becomes U+FFFD
        // and the line is still scanned for the term beside it.
        let bytes: &[u8] = b"diff --git a/l.txt b/l.txt\n--- /dev/null\n+++ b/l.txt\n\
            @@ -0,0 +1,2 @@\n+caf\xe9 acme-corp\n+\xff\xfe\n";
        let diff = String::from_utf8_lossy(bytes);

        let hits = scan(&diff, &["acme-corp".to_owned()]).unwrap();

        assert_eq!(hits.len(), 1, "was: {hits:?}");
        assert_eq!((hits[0].file.as_str(), hits[0].line), ("l.txt", 1));
        assert!(hits[0].text.contains('\u{FFFD}'), "was: {:?}", hits[0].text);
    }
}
