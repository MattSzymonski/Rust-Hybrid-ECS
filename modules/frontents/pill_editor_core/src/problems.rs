//! Build diagnostics: the problems the Problems panel shows.
//!
//! # Responsibilities
//!
//! - Parse a build's output text into structured problems: severity, message
//!   and the first `file:line:column` a compiler printed for it.
//! - Keep the parser dependency-free and total: malformed or unfamiliar output
//!   yields fewer problems, never an error.
//!
//! # Contract
//!
//! The input is rustc's human-readable output as cargo prints it (the `-v`
//! form and ANSI colour are stripped by the caller). Both one-line
//! (`error: message`) and code-carrying (`error[E0425]: message`) headers are
//! recognised, at any leading indentation; the location is the first `-->`
//! span line within the following [`SPAN_LOOKAHEAD`] lines. Lines like
//! `error: could not compile \`crate\`` are summaries of problems reported
//! above them, so they are dropped.

/// How serious one problem is, in the order the panel groups them.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// A compiler error: the build stopped.
    Error,
    /// A compiler warning: the build finished anyway.
    Warning,
    /// A note attached to a problem (rare in the top-level form).
    Note,
}

/// One problem parsed from a build's output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    /// Error, warning or note.
    pub severity: Severity,
    /// The compiler's message, code included when it printed one.
    pub message: String,
    /// The file the first span names, when the problem carries one.
    pub file: Option<String>,
    /// The line of that span.
    pub line: Option<u32>,
    /// The column of that span.
    pub column: Option<u32>,
}

/// How many lines after a header the span line may sit.
const SPAN_LOOKAHEAD: usize = 6;

/// Parse `output` into problems, in the order the compiler reported them.
///
/// A header whose first span line cannot be found (or is malformed) still
/// yields a problem, with no location: the panel can show the message even
/// when the link has nowhere to go.
pub fn parse_build_output(output: &str) -> Vec<Problem> {
    let lines: Vec<&str> = output.lines().collect();
    let mut problems = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let Some((severity, message)) = parse_header(lines[index]) else {
            index += 1;
            continue;
        };
        // A `could not compile` summary repeats problems already listed.
        if message.starts_with("could not compile") {
            index += 1;
            continue;
        }
        let mut problem = Problem {
            severity,
            message,
            file: None,
            line: None,
            column: None,
        };
        // Step 1: the first span within the lookahead window names the
        // location; anything past a blank line belongs to the next entry.
        for span in lines.iter().skip(index + 1).take(SPAN_LOOKAHEAD) {
            if span.trim().is_empty() {
                break;
            }
            if let Some((file, line, column)) = parse_span(span) {
                problem.file = Some(file);
                problem.line = Some(line);
                problem.column = Some(column);
                break;
            }
        }
        problems.push(problem);
        index += 1;
    }
    problems
}

/// The severity and message of a rustc header line, when it is one.
///
/// `strip_diagnostics_prefix` in the caller removes cargo's `Compiling ...`
/// status lines; this only has to judge the compiler's own shapes.
fn parse_header(line: &str) -> Option<(Severity, String)> {
    let text = line.trim_start();
    let (severity, rest) = if let Some(rest) = text.strip_prefix("error") {
        (Severity::Error, rest)
    } else if let Some(rest) = text.strip_prefix("warning") {
        (Severity::Warning, rest)
    } else if let Some(rest) = text.strip_prefix("note") {
        (Severity::Note, rest)
    } else {
        return None;
    };
    // `error[E0425]: msg` keeps the code in the message; `error: msg` is the
    // bare form. Anything else (`error_span`, `errors`) is not a header.
    let rest = if let Some(rest) = rest.strip_prefix('[') {
        let code_end = rest.find(']')?;
        let code = &rest[..code_end];
        if code.is_empty() || !code.chars().next()?.is_ascii_uppercase() {
            return None;
        }
        if !rest[code_end + 1..].starts_with(':') {
            return None;
        }
        &rest[code_end + 1..]
    } else {
        rest
    };
    let message = rest.strip_prefix(':')?.trim();
    if message.is_empty() {
        return None;
    }
    Some((severity, message.to_string()))
}

/// The `file`, `line` and `column` of a rustc span line (`  --> src/x.rs:12:5`).
fn parse_span(line: &str) -> Option<(String, u32, u32)> {
    let text = line.trim_start();
    let rest = text.strip_prefix("-->")?.trim();
    // The path may contain colons (a Windows drive), so split from the right:
    // the location is the last two `:`-separated numeric fields.
    let (path, rest) = rest.rsplit_once(':')?;
    let (path, line) = path.rsplit_once(':')?;
    let line: u32 = line.trim().parse().ok()?;
    // A span often trails `column` with decoration (`:5` or `:5:6`); the
    // column is the leading number.
    let column: u32 = rest
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), line, column))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_coded_error_carries_its_first_span() {
        let output = "\
   Compiling pill_spline v0.1.0
error[E0425]: cannot find value `speed` in this scope
  --> extensions/pill_spline/pill_spline/src/pill_spline.rs:41:9
   |
41 |     let v = speed * dt;
   |             ^^^^^ not found in this scope

error: could not compile `pill_spline` (lib) due to 1 previous error
";
        let problems = parse_build_output(output);
        assert_eq!(problems.len(), 1, "{problems:?}");
        let problem = &problems[0];
        assert_eq!(problem.severity, Severity::Error);
        assert_eq!(problem.message, "cannot find value `speed` in this scope");
        assert_eq!(
            problem.file.as_deref(),
            Some("extensions/pill_spline/pill_spline/src/pill_spline.rs")
        );
        assert_eq!((problem.line, problem.column), (Some(41), Some(9)));
    }

    #[test]
    fn a_warning_and_an_error_both_parse_in_order() {
        let output = "\
warning: unused variable: `x`
  --> src/lib.rs:3:9
error: expected `;`, found `}`
  --> src/lib.rs:7:1
";
        let problems = parse_build_output(output);
        assert_eq!(problems.len(), 2);
        assert_eq!(problems[0].severity, Severity::Warning);
        assert_eq!(problems[0].message, "unused variable: `x`");
        assert_eq!(problems[1].severity, Severity::Error);
        assert_eq!(problems[1].line, Some(7));
    }

    #[test]
    fn a_windows_drive_path_survives_the_colon_split() {
        let output = "error[E0308]: mismatched types\n  --> D:\\work\\crate\\src\\lib.rs:12:34\n";
        let problems = parse_build_output(output);
        assert_eq!(problems.len(), 1);
        assert_eq!(
            problems[0].file.as_deref(),
            Some("D:\\work\\crate\\src\\lib.rs")
        );
        assert_eq!((problems[0].line, problems[0].column), (Some(12), Some(34)));
    }

    #[test]
    fn a_header_without_a_span_still_reports_its_message() {
        let problems =
            parse_build_output("error: linking with `link.exe` failed: exit code: 1120\n");
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].file, None);
        assert_eq!(problems[0].severity, Severity::Error);
    }

    #[test]
    fn unrelated_lines_are_ignored() {
        let output = "\
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1.2s
error_span: not a header
errors happened
";
        assert!(parse_build_output(output).is_empty());
    }

    #[test]
    fn a_summary_line_is_dropped_as_a_duplicate() {
        let output = "\
error: expected one of `.`, `;`, `?`, `}`, or an operator, found `)`
 --> src/main.rs:2:5
error: could not compile `demo` (bin \"demo\") due to 1 previous error
";
        let problems = parse_build_output(output);
        assert_eq!(problems.len(), 1);
        assert!(problems[0].message.starts_with("expected one of"));
    }
}
