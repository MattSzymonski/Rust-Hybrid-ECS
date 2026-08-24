//! Minimal source scanning for patch classification and generation.
//!
//! # Responsibilities
//!
//! - Locate one function's full text (attributes, signature and body) by name.
//! - Collect a file's top-level `use` statements.
//! - Strip named function bodies, so a body-only edit can be told from any other.
//!
//! # Design
//!
//! Deliberately a byte scanner rather than a syntax tree. The only questions
//! asked are "where does this function start and end" and "what changed outside
//! the bodies", and both are answered by brace matching over a mask that marks
//! which bytes are real code. Pulling `syn` into the host to answer them would
//! add a parse of the whole file to every keystroke-triggered classification,
//! and would still need the original byte spans to copy text verbatim.
//!
//! The scanner is conservative by construction: anything it cannot confidently
//! identify makes classification report "not body-only", which costs a full
//! module reload rather than risking a wrong patch.

// Standard library
use std::collections::HashSet;

// =============================================================================
// Code mask
// =============================================================================

/// Marks which bytes of `source` are real code.
///
/// `false` inside string literals, char literals and comments, so a brace in a
/// string or a `//` in a URL can never confuse brace matching.
pub fn code_mask(source: &str) -> Vec<bool> {
    let bytes = source.as_bytes();
    let mut mask = vec![true; bytes.len()];
    let mut index = 0usize;

    while index < bytes.len() {
        match bytes[index] {
            // Line comment: masked to end of line.
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    mask[index] = false;
                    index += 1;
                }
            }
            // Block comment: masked through the closing delimiter. Rust allows
            // nesting, so depth is tracked rather than scanning for the first
            // `*/`.
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                let mut depth = 1usize;
                mask[index] = false;
                mask[index + 1] = false;
                index += 2;
                while index < bytes.len() && depth > 0 {
                    if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'*' {
                        depth += 1;
                        mask[index] = false;
                        mask[index + 1] = false;
                        index += 2;
                        continue;
                    }
                    if index + 1 < bytes.len() && bytes[index] == b'*' && bytes[index + 1] == b'/' {
                        depth -= 1;
                        mask[index] = false;
                        mask[index + 1] = false;
                        index += 2;
                        continue;
                    }
                    mask[index] = false;
                    index += 1;
                }
            }
            // String literal, including the raw forms `r"..."` and `r#"..."#`.
            b'r' if index + 1 < bytes.len()
                && (bytes[index + 1] == b'"' || bytes[index + 1] == b'#') =>
            {
                let hash_start = index + 1;
                let mut hashes = 0usize;
                while hash_start + hashes < bytes.len() && bytes[hash_start + hashes] == b'#' {
                    hashes += 1;
                }
                if hash_start + hashes >= bytes.len() || bytes[hash_start + hashes] != b'"' {
                    // Just an identifier beginning with `r`.
                    index += 1;
                    continue;
                }
                mask[index] = false;
                index = hash_start + hashes + 1;
                // Scan for the matching `"` followed by the same hash count.
                while index < bytes.len() {
                    if bytes[index] == b'"' {
                        let mut closing = 0usize;
                        while index + 1 + closing < bytes.len()
                            && bytes[index + 1 + closing] == b'#'
                            && closing < hashes
                        {
                            closing += 1;
                        }
                        if closing == hashes {
                            for offset in 0..=hashes {
                                if index + offset < bytes.len() {
                                    mask[index + offset] = false;
                                }
                            }
                            index += hashes + 1;
                            break;
                        }
                    }
                    mask[index] = false;
                    index += 1;
                }
            }
            b'"' => {
                mask[index] = false;
                index += 1;
                while index < bytes.len() {
                    if bytes[index] == b'\\' {
                        mask[index] = false;
                        if index + 1 < bytes.len() {
                            mask[index + 1] = false;
                        }
                        index += 2;
                        continue;
                    }
                    let closing = bytes[index] == b'"';
                    mask[index] = false;
                    index += 1;
                    if closing {
                        break;
                    }
                }
            }
            _ => index += 1,
        }
    }
    mask
}

/// The matching `}` for the `{` at `open`, searching only real code.
fn matching_brace(bytes: &[u8], mask: &[bool], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut index = open + 1;
    while index < bytes.len() {
        if mask[index] {
            match bytes[index] {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(index);
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    None
}

/// The first real-code `{` at or after `from`.
fn next_open_brace(bytes: &[u8], mask: &[bool], from: usize) -> Option<usize> {
    (from..bytes.len()).find(|&index| mask[index] && bytes[index] == b'{')
}

// =============================================================================
// Function lookup
// =============================================================================

/// One function located in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionText {
    /// Byte offset of `fn`, after any attributes.
    pub start: usize,
    /// Byte offset just past the closing brace.
    pub end: usize,
    /// The full declaration, from `fn` through the closing brace.
    pub text: String,
    /// Just the body, braces excluded.
    pub body: String,
}

/// Find `fn <name>` at the top level and return its full text.
///
/// Returns `None` when the name does not appear as a function declaration, or
/// when its braces are unbalanced - both of which make the caller fall back to
/// a full reload rather than guess.
pub fn find_function(source: &str, name: &str) -> Option<FunctionText> {
    let bytes = source.as_bytes();
    let mask = code_mask(source);
    let needle = format!("fn {name}");
    let needle_bytes = needle.as_bytes();

    let mut search = 0usize;
    while search + needle_bytes.len() <= bytes.len() {
        let candidate = &bytes[search..search + needle_bytes.len()];
        let is_code = mask[search..search + needle_bytes.len()]
            .iter()
            .all(|byte_is_code| *byte_is_code);

        if candidate == needle_bytes && is_code {
            // The character before must be a boundary, so `fn movement` does not
            // match inside `fn movement_extra`.
            let preceded_ok = search == 0
                || matches!(bytes[search - 1], b'\n' | b'\r' | b'\t' | b' ' | b'}' | b';');
            let after = search + needle_bytes.len();
            let followed_ok = after >= bytes.len()
                || matches!(bytes[after], b'(' | b' ' | b'<' | b'\n' | b'\r' | b'\t');

            if preceded_ok && followed_ok {
                let open = next_open_brace(bytes, &mask, after)?;
                let close = matching_brace(bytes, &mask, open)?;
                return Some(FunctionText {
                    start: search,
                    end: close + 1,
                    text: source[search..=close].to_string(),
                    body: source[open + 1..close].to_string(),
                });
            }
        }
        search += 1;
    }
    None
}

// =============================================================================
// Imports
// =============================================================================

/// Collect the file's top-level `use` statements, verbatim.
///
/// A generated patch replays these so the copied function body resolves the
/// same names the original did. Only depth-zero statements are taken;
/// function-local `use` travels with the body it lives in.
pub fn top_level_use_statements(source: &str) -> Vec<String> {
    let bytes = source.as_bytes();
    let mask = code_mask(source);
    let mut statements = Vec::new();
    let mut depth = 0i32;
    let mut index = 0usize;

    while index < bytes.len() {
        if mask[index] {
            match bytes[index] {
                b'{' => depth += 1,
                b'}' => depth -= 1,
                b'u' if depth == 0 => {
                    let line_start = index;
                    let preceded_ok = index == 0
                        || matches!(bytes[index - 1], b'\n' | b'\r' | b'\t' | b' ');
                    if preceded_ok && source[index..].starts_with("use ") {
                        // A `use` statement ends at its terminating semicolon.
                        if let Some(offset) = source[index..].find(';') {
                            let end = index + offset + 1;
                            statements.push(source[line_start..end].trim().to_string());
                            index = end;
                            continue;
                        }
                    }
                }
                _ => {}
            }
        }
        index += 1;
    }
    statements
}

// =============================================================================
// Hot function discovery
// =============================================================================

/// Which hot-patching attribute a function carries.
///
/// The two are redirected through different machinery, so a patch has to know
/// which one it is replacing before it can be generated or installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotFunctionKind {
    /// `#[pill_hot]` - an ECS system, redirected through the engine's registry
    /// in this process.
    System,
    /// `#[pill_hot_fn]` - a plain function or inherent method, redirected
    /// through a slot that exists once per artifact linking the crate.
    PlainFunction,
}

/// One function a source file marks as hot-patchable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotFunction {
    /// The function's own name, which is also the last segment of the name the
    /// running artifact registered it under.
    pub name: String,
    /// Which attribute marked it.
    pub kind: HotFunctionKind,
    /// The type whose inherent `impl` block encloses it, for a method.
    ///
    /// `None` for a free function, and also for a method the scanner could not
    /// attribute to a simple named type - a generic or trait `impl`. A patch
    /// for a method must name the receiver type, so classification refuses the
    /// second case rather than generating something that cannot compile.
    pub self_type: Option<String>,
    /// Whether the declaration takes a receiver.
    pub takes_receiver: bool,
}

/// Names of the functions this file marks as hot-patchable.
///
/// The attribute is the developer's opt-in, so it also defines the set of
/// bodies a classification is allowed to treat as replaceable. Everything else
/// in the file - including other functions' bodies - counts as a structural
/// change and forces a full reload.
pub fn hot_function_names(source: &str) -> Vec<String> {
    hot_functions(source)
        .into_iter()
        .map(|function| function.name)
        .collect()
}

/// Every hot function in this file, with the attribute that marked it and the
/// type whose `impl` block encloses it.
pub fn hot_functions(source: &str) -> Vec<HotFunction> {
    let mask = code_mask(source);
    let bytes = source.as_bytes();
    let blocks = inherent_impl_blocks(source, &mask);
    let mut found: Vec<HotFunction> = Vec::new();
    let mut index = 0usize;

    while let Some(offset) = source[index..].find("#[pill_hot") {
        let attribute_start = index + offset;
        if !mask[attribute_start] {
            index = attribute_start + 1;
            continue;
        }
        // `#[pill_hot_fn]` shares its prefix with `#[pill_hot]`, so the longer
        // name is tested first; anything else starting with the prefix (a
        // `#[pill_hot(name = "...")]` override, for instance) is a system.
        let kind = if source[attribute_start..].starts_with("#[pill_hot_fn") {
            HotFunctionKind::PlainFunction
        } else {
            HotFunctionKind::System
        };

        // The declaration follows the attribute (and any further attributes).
        // Scan forward for the next real-code `fn ` and take its name.
        let mut cursor = attribute_start;
        let mut name = None;
        let mut declaration_start = attribute_start;
        while cursor + 3 <= bytes.len() {
            if mask[cursor] && source[cursor..].starts_with("fn ") {
                declaration_start = cursor;
                let name_start = cursor + 3;
                let name_end = source[name_start..]
                    .find(|character: char| !character.is_alphanumeric() && character != '_')
                    .map(|end| name_start + end)
                    .unwrap_or(source.len());
                if name_end > name_start {
                    name = Some(source[name_start..name_end].to_string());
                }
                break;
            }
            // An opening brace before any `fn` means the attribute belonged to
            // something else; stop rather than guess.
            if mask[cursor] && bytes[cursor] == b'{' {
                break;
            }
            cursor += 1;
        }

        if let Some(name) = name {
            if !found.iter().any(|existing| existing.name == name) {
                // The innermost `impl` body containing this declaration, which
                // is the type a generated patch must name.
                let self_type = blocks
                    .iter()
                    .filter(|block| block.body.0 < declaration_start && declaration_start < block.body.1)
                    .min_by_key(|block| block.body.1 - block.body.0)
                    .map(|block| block.type_name.clone());
                found.push(HotFunction {
                    name,
                    kind,
                    self_type,
                    takes_receiver: declaration_takes_receiver(source, &mask, declaration_start),
                });
            }
        }
        index = attribute_start + 1;
    }
    found
}

/// One `impl` block whose methods can be attributed to a named type.
struct ImplBlock {
    /// Byte range of the block's body, braces included.
    body: (usize, usize),
    /// The type the block implements methods on.
    type_name: String,
}

/// Every inherent `impl` block in the file whose type is a simple name.
///
/// Generic blocks (`impl<T> Foo<T>`) and trait blocks are deliberately skipped:
/// a patch replaces one concrete implementation, and neither has a single
/// receiver type a generated patch could name. A method inside one is reported
/// with no `self_type`, which classification then refuses with a clear reason
/// instead of emitting a patch that cannot compile.
fn inherent_impl_blocks(source: &str, mask: &[bool]) -> Vec<ImplBlock> {
    let bytes = source.as_bytes();
    let mut blocks = Vec::new();
    let mut index = 0usize;

    while let Some(offset) = source[index..].find("impl") {
        let start = index + offset;
        index = start + 4;
        if !mask[start] {
            continue;
        }
        // Must be the whole word, not the tail of an identifier.
        if start > 0 && is_identifier_byte(bytes[start - 1]) {
            continue;
        }
        if start + 4 < bytes.len() && is_identifier_byte(bytes[start + 4]) {
            continue;
        }
        let Some(open) = next_open_brace(bytes, mask, start + 4) else {
            continue;
        };
        let Some(close) = matching_brace(bytes, mask, open) else {
            continue;
        };
        // The header between `impl` and the opening brace names the type.
        let header = &source[start + 4..open];
        if let Some(type_name) = inherent_type_name(header) {
            blocks.push(ImplBlock {
                body: (open, close),
                type_name,
            });
        }
    }
    blocks
}

/// The type a simple inherent `impl` header names, if it is one.
fn inherent_type_name(header: &str) -> Option<String> {
    let header = header.trim();
    // A trait implementation, a generic block, or a `where` clause all mean
    // there is no single concrete receiver type to name.
    if header.is_empty()
        || header.starts_with('<')
        || header.contains(" for ")
        || header.contains("where")
        || header.contains('<')
    {
        return None;
    }
    let candidate = header.trim_end_matches(|character: char| character.is_whitespace());
    if candidate.is_empty()
        || !candidate
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_' || character == ':')
    {
        return None;
    }
    Some(candidate.to_string())
}

/// Whether the declaration beginning at `fn` takes a receiver.
fn declaration_takes_receiver(source: &str, mask: &[bool], declaration_start: usize) -> bool {
    let bytes = source.as_bytes();
    let mut cursor = declaration_start;
    // Find the opening parenthesis of the parameter list.
    while cursor < bytes.len() {
        if mask[cursor] && bytes[cursor] == b'(' {
            break;
        }
        if mask[cursor] && bytes[cursor] == b'{' {
            return false;
        }
        cursor += 1;
    }
    let rest = &source[cursor.saturating_add(1)..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('&').unwrap_or(rest).trim_start();
    // A lifetime on the receiver, as in `&'a self`.
    let rest = if let Some(after) = rest.strip_prefix('\'') {
        after
            .trim_start_matches(|character: char| character.is_alphanumeric() || character == '_')
            .trim_start()
    } else {
        rest
    };
    let rest = rest.strip_prefix("mut ").unwrap_or(rest).trim_start();
    rest.starts_with("self")
}

/// Whether a byte can appear inside a Rust identifier.
fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

// =============================================================================
// Body stripping
// =============================================================================

/// Replace the bodies of the named functions with a placeholder.
///
/// Comparing two stripped revisions answers the only question classification
/// needs: did anything OUTSIDE these bodies change? Signatures, types,
/// constants, imports, attributes and every other function all survive the
/// strip, so any edit to them shows up as a difference.
pub fn strip_function_bodies(source: &str, names: &HashSet<String>) -> String {
    let mut stripped = String::with_capacity(source.len());
    let mut cursor = 0usize;

    // Repeatedly find the earliest remaining named function and blank its body.
    loop {
        let next = names
            .iter()
            .filter_map(|name| find_function(&source[cursor..], name))
            .min_by_key(|found| found.start);

        let Some(found) = next else {
            break;
        };

        // Keep everything up to and including the opening brace, drop the body.
        let absolute_start = cursor + found.start;
        let body_open = absolute_start + (found.text.len() - found.body.len() - 2);
        stripped.push_str(&source[cursor..=body_open]);
        stripped.push_str("/*body*/");
        stripped.push('}');
        cursor = cursor + found.end;
    }

    stripped.push_str(&source[cursor..]);
    stripped
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
use pill_engine::*;
use project::PhysicsState;

const GRAVITY: f32 = 800.0;

fn helper(value: f32) -> f32 {
    value * 2.0
}

#[pill_hot]
fn movement_system(mut query: Query<&mut PhysicsState>) -> Result<(), SystemError> {
    // a brace in a comment: {
    let label = "and one in a string: }";
    for mut physics in query.iter_mut() {
        physics.position_x += 1.0;
    }
    Ok(())
}

fn movement_system_extra() {}
"#;

    #[test]
    fn finds_a_function_and_its_body() {
        let found = find_function(SAMPLE, "movement_system").expect("found");
        assert!(found.text.starts_with("fn movement_system("));
        assert!(found.text.ends_with('}'));
        assert!(found.body.contains("physics.position_x += 1.0;"));
        // The body must end at the real closing brace, not one inside a string
        // or comment.
        assert!(found.body.contains("Ok(())"));
        assert!(!found.body.contains("fn movement_system_extra"));
    }

    /// A prefix match must not be mistaken for the function being patched.
    #[test]
    fn does_not_match_a_longer_name() {
        let found = find_function(SAMPLE, "movement_system").expect("found");
        let extra = find_function(SAMPLE, "movement_system_extra").expect("found");
        assert_ne!(found.start, extra.start);
    }

    #[test]
    fn reports_absence_for_an_unknown_function() {
        assert!(find_function(SAMPLE, "no_such_function").is_none());
    }

    #[test]
    fn collects_top_level_imports() {
        let imports = top_level_use_statements(SAMPLE);
        assert_eq!(
            imports,
            vec![
                "use pill_engine::*;".to_string(),
                "use project::PhysicsState;".to_string()
            ]
        );
    }

    /// The strip must hide body edits and expose everything else.
    #[test]
    fn stripping_hides_body_edits_only() {
        let names: HashSet<String> = ["movement_system".to_string()].into_iter().collect();

        let edited_body = SAMPLE.replace("physics.position_x += 1.0;", "physics.position_x += 9.0;");
        assert_eq!(
            strip_function_bodies(SAMPLE, &names),
            strip_function_bodies(&edited_body, &names),
            "a body-only edit must vanish from the stripped form"
        );

        let edited_constant = SAMPLE.replace("800.0", "900.0");
        assert_ne!(
            strip_function_bodies(SAMPLE, &names),
            strip_function_bodies(&edited_constant, &names),
            "a constant change must survive the strip"
        );

        let edited_signature =
            SAMPLE.replace("fn movement_system(mut query", "fn movement_system(mut q2");
        assert_ne!(
            strip_function_bodies(SAMPLE, &names),
            strip_function_bodies(&edited_signature, &names),
            "a signature change must survive the strip"
        );

        // An edit to a DIFFERENT function's body must also survive, because that
        // function is not in the hot set.
        let edited_helper = SAMPLE.replace("value * 2.0", "value * 3.0");
        assert_ne!(
            strip_function_bodies(SAMPLE, &names),
            strip_function_bodies(&edited_helper, &names),
            "an edit outside the hot set must not be classified as body-only"
        );
    }

    #[test]
    fn finds_hot_functions_by_attribute() {
        assert_eq!(hot_function_names(SAMPLE), vec!["movement_system".to_string()]);
    }

    /// Attributes between `#[pill_hot]` and the declaration must not confuse the
    /// scan, and an unannotated function must not be picked up.
    #[test]
    fn hot_discovery_handles_stacked_attributes() {
        let source = r#"
#[pill_hot]
#[allow(clippy::needless_range_loop)]
#[inline(never)]
fn annotated(value: i32) -> i32 { value }

fn not_annotated() {}

// #[pill_hot] in a comment must be ignored
fn also_not_annotated() {}
"#;
        assert_eq!(hot_function_names(source), vec!["annotated".to_string()]);
    }

    #[test]
    fn code_mask_marks_strings_and_comments() {
        let source = r#"let a = "{"; // }
"#;
        let mask = code_mask(source);
        let bytes = source.as_bytes();
        // The brace inside the string literal must not count as code.
        let string_brace = source.find("\"{\"").unwrap() + 1;
        assert!(!mask[string_brace], "brace in a string must be masked");
        // The brace in the trailing comment likewise.
        let comment_brace = source.rfind('}').unwrap();
        assert!(!mask[comment_brace], "brace in a comment must be masked");
        assert!(mask[0] && bytes[0] == b'l', "plain code stays unmasked");
    }
}
