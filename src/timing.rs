//! Where a run spends its time, when asked (`KNIVES_TIMING`).

/// Whether timing output was requested.
pub fn enabled() -> bool {
    std::env::var_os("KNIVES_TIMING").is_some()
}

/// One stderr line per forge call: duration, repo dir tail, argv summary.
///
/// Args longer than 48 chars (GraphQL query bodies) render as their first 45
/// chars plus "…", so the line stays a line.
pub fn call_line(duration: std::time::Duration, dir: &std::path::Path, args: &[&str]) -> String {
    let dir_tail = dir.file_name().map_or_else(
        || dir.display().to_string(),
        |tail| tail.to_string_lossy().into_owned(),
    );
    let mut line = format!("timing gh {dir_tail} {}ms: ", duration.as_millis());

    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        if arg.chars().nth(48).is_some() {
            line.extend(arg.chars().take(45));
            line.push('…');
        } else {
            line.push_str(arg);
        }
    }

    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, time::Duration};

    #[test]
    fn a_call_line_truncates_query_bodies_but_names_the_verb() {
        let query = format!("query={}", "q".repeat(200));
        let line = call_line(
            Duration::from_millis(1234),
            Path::new("/x/some-fork"),
            &["api", "graphql", "-f", &query],
        );

        assert!(line.starts_with("timing gh some-fork 1234ms: api graphql -f query="));
        assert!(line.len() < 120, "was {} chars: {line}", line.len());
    }

    #[test]
    fn short_args_render_verbatim() {
        assert_eq!(
            call_line(
                Duration::from_millis(42),
                Path::new("/x/some-fork"),
                &["pr", "list", "--state", "all"],
            ),
            "timing gh some-fork 42ms: pr list --state all"
        );
    }
}
