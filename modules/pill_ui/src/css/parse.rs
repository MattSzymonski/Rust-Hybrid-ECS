//! Loading a stylesheet: rules, variables, typed declarations and reports.
//!
//! # Responsibilities
//!
//! - [`load`]: parses CSS source into rules of typed declarations, the
//!   `:root` custom properties, and a list of [`Report`]s naming everything
//!   the subset refused - with the position in the file.
//! - [`Declaration`]: one typed declaration; the property set is exactly
//!   the subset's, everything else is reported by name.
//!
//! # Design
//!
//! `cssparser` does the hard part of CSS *syntax* - comments, blocks,
//! strings, escapes, `!important` - and hands over raw rule preludes and
//! declaration values as slices; selectors and values are then parsed here
//! and in [`super::selector`] / [`super::value`]. A declaration is validated
//! once at load time, so matching a frame only walks typed data.
//!
//! `var(--name)` is substituted textually at load time: `:root` variables
//! are collected first, whatever order the file writes them in, then
//! substituted (depth-limited) into every declaration value. The subset has
//! no cascade between variables and no dynamic theming, so load-time
//! substitution gives the same answers more cheaply.

// Standard library
use std::collections::HashMap;
use std::fmt;

// External crates
use cssparser::{Delimiter, ParseError, Parser, ParserInput, ToCss, Token};

// Current crate
use crate::draw::Color;

use super::selector::{self, Selector};
use super::value::{
    self, parse_color, parse_length, parse_number, parse_size, sides_from, Length, Sides, Size,
};

// =============================================================================
// Reports
// =============================================================================

/// Why the subset refused something.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ReportKind {
    /// The selector uses syntax the subset does not implement.
    UnsupportedSelector,
    /// The property is not in the subset.
    UnsupportedProperty,
    /// The property is in the subset, but this value is not.
    UnsupportedValue,
    /// The syntax is broken.
    Malformed,
}

impl ReportKind {
    /// The word reports use for this kind.
    pub fn label(self) -> &'static str {
        match self {
            ReportKind::UnsupportedSelector => "unsupported selector",
            ReportKind::UnsupportedProperty => "unsupported property",
            ReportKind::UnsupportedValue => "unsupported value",
            ReportKind::Malformed => "malformed",
        }
    }
}

/// One thing the stylesheet said that the subset did not take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// What kind of refusal this is.
    pub kind: ReportKind,
    /// What was refused: a property name, a selector, a value.
    pub subject: String,
    /// Why, in a sentence.
    pub detail: String,
    /// The 1-based line in the source.
    pub line: u32,
    /// The 1-based column in the source.
    pub column: u32,
}

impl fmt::Display for Report {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}: {} `{}`: {}",
            self.line,
            self.column,
            self.kind.label(),
            self.subject,
            self.detail
        )
    }
}

// =============================================================================
// Typed declarations
// =============================================================================

/// Keywords of `text-align`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextAlign {
    /// Text starts at the left edge.
    Left,
    /// Text is centred.
    Center,
    /// Text ends at the right edge.
    Right,
}

/// Keywords of `white-space` the subset takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WhiteSpace {
    /// Text wraps at the available width.
    Normal,
    /// Text stays on one line.
    Nowrap,
}

/// Keywords of `text-overflow`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TextOverflow {
    /// Overflowing text is cut at the edge.
    Clip,
    /// Overflowing text is cut with an ellipsis (rendered as a cut).
    Ellipsis,
}

/// One side's border.
#[derive(Copy, Clone, Debug, PartialEq, Default)]
pub struct BorderSide {
    /// The stroke width in pixels.
    pub width: Option<f32>,
    /// The stroke colour.
    pub color: Option<Color>,
}

/// Keywords of `display` the subset takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DisplayKind {
    /// The flex layout of Stage 8.
    Flex,
    /// The block layout `taffy` also offers.
    Block,
    /// The node is not laid out or painted.
    None,
}

/// Keywords of `flex-direction`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlexDirectionKind {
    /// Left to right.
    Row,
    /// Right to left.
    RowReverse,
    /// Top to bottom.
    Column,
    /// Bottom to top.
    ColumnReverse,
}

/// Keywords of `align-items`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AlignKind {
    /// Fill the cross axis.
    Stretch,
    /// Centre on the cross axis.
    Center,
    /// Start of the cross axis.
    FlexStart,
    /// End of the cross axis.
    FlexEnd,
}

/// Keywords of `justify-content`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum JustifyKind {
    /// Pack at the start.
    Start,
    /// Pack at the end.
    End,
    /// Centre the items.
    Center,
    /// Spread the items, outer edges flush.
    SpaceBetween,
}

/// Keywords of `position` the subset takes.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PositionKind {
    /// Offset from where layout placed it.
    Relative,
    /// Relative to the containing block's edges.
    Absolute,
    /// Like absolute; the window is the containing block (the shell's root).
    Fixed,
}

/// Keywords of `overflow`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OverflowKind {
    /// Content may overflow.
    Visible,
    /// Content is clipped.
    Hidden,
    /// Clipped; the widget path scrolls (the shell's panels).
    Auto,
    /// Clipped and scrollable.
    Scroll,
    /// Clipped, never scrollable.
    Clip,
}

/// Keywords of `box-sizing`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BoxSizingKind {
    /// Widths include padding and border.
    BorderBox,
    /// Widths are the content box.
    ContentBox,
}

/// A `line-height` value: a multiple of the font size, or pixels.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum LineHeight {
    /// A unitless multiple of the font size.
    Factor(f32),
    /// A fixed pixel height.
    Px(f32),
}

/// The `font` shorthand, resolved as far as the subset goes.
#[derive(Clone, Debug, PartialEq)]
pub struct FontSpec {
    /// The font size in pixels.
    pub size: Option<f32>,
    /// The line height when the shorthand wrote `size/height`.
    pub line_height: Option<LineHeight>,
    /// The weight (parsed; the bundled font has one instance).
    pub weight: Option<u16>,
    /// The first family named.
    pub family: Option<String>,
}

/// One typed declaration of the subset.
#[derive(Clone, Debug, PartialEq)]
pub enum Declaration {
    /// `background`, `background-color`: the node's fill.
    Background(Color),
    /// `color`: text colour.
    Color(Color),
    /// `opacity`: multiplied into the node's colours.
    Opacity(f32),
    /// `font-size`.
    FontSize(f32),
    /// `line-height`.
    LineHeight(LineHeight),
    /// `font-family`: the first family named.
    FontFamily(String),
    /// `font-weight` (parsed; the bundled font has one instance).
    FontWeight(u16),
    /// `font` shorthand.
    Font(FontSpec),
    /// `text-align`.
    TextAlign(TextAlign),
    /// `white-space`.
    WhiteSpace(WhiteSpace),
    /// `text-overflow`.
    TextOverflow(TextOverflow),
    /// `padding` and its side forms; unset sides keep their previous value.
    Padding(Sides<Option<f32>>),
    /// `margin` and its side forms; unset sides keep their previous value.
    Margin(Sides<Option<Length>>),
    /// `border`, `border-width`, `border-top`, ...: widths and colours.
    Border(Sides<BorderSide>),
    /// `border-radius`, kept for the rounded shader (not rendered yet).
    BorderRadius(f32),
    /// `box-shadow`, kept as written (not rendered yet).
    BoxShadow(String),
    /// `outline`: the focus ring, width and colour.
    Outline {
        /// Stroke width in pixels; zero draws nothing.
        width: f32,
        /// Stroke colour.
        color: Color,
    },
    /// `outline-offset`: positive grows outward, negative pulls inside.
    OutlineOffset(f32),
    /// `box-sizing`.
    BoxSizing(BoxSizingKind),
    /// `display`.
    Display(DisplayKind),
    /// `flex-direction`.
    FlexDirection(FlexDirectionKind),
    /// `flex-grow`.
    FlexGrow(f32),
    /// `flex-shrink`.
    FlexShrink(f32),
    /// `flex-basis`.
    FlexBasis(Size),
    /// `align-items`.
    AlignItems(AlignKind),
    /// `justify-content`.
    JustifyContent(JustifyKind),
    /// `gap`, `row-gap`, `column-gap`; unset axes keep their value.
    Gap {
        /// The row gap.
        row: Option<f32>,
        /// The column gap.
        column: Option<f32>,
    },
    /// `width`.
    Width(Size),
    /// `height`.
    Height(Size),
    /// `min-width`.
    MinWidth(Size),
    /// `min-height`.
    MinHeight(Size),
    /// `max-width`.
    MaxWidth(Size),
    /// `max-height`.
    MaxHeight(Size),
    /// `position`.
    Position(PositionKind),
    /// `inset`, `top`, `right`, `bottom`, `left`.
    Inset(Sides<Option<Length>>),
    /// `overflow`, `overflow-x`, `overflow-y`; unset axes keep their value.
    Overflow {
        /// The horizontal behaviour.
        x: Option<OverflowKind>,
        /// The vertical behaviour.
        y: Option<OverflowKind>,
    },
    /// `z-index`, kept (ordering arrives with the overlay layer).
    ZIndex(i32),
    /// `cursor`, kept (applied by the platform layer).
    Cursor(String),
    /// `pointer-events`: whether the node takes the pointer at all.
    PointerEvents(bool),
    /// `user-select`, kept (text fields are the only selectable text).
    UserSelect(bool),
}

/// One declaration entry: the typed declaration, or an explicit keyword.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The typed declaration; `None` for the keyword forms below.
    pub declaration: Option<Declaration>,
    /// The property name as written, for `inherit` and diagnostics.
    pub declaration_name: String,
    /// The value was `inherit`.
    pub inherit: bool,
    /// The value was `unset` or `revert`.
    pub unset: bool,
    /// The declaration carried `!important`.
    pub important: bool,
}

/// One parsed style rule.
#[derive(Clone, Debug)]
pub struct RawRule {
    /// The selector list, all selectors sharing the declarations.
    pub selectors: Vec<Selector>,
    /// The declarations, in written order.
    pub declarations: Vec<Entry>,
}

/// A declaration as read, before variables resolve and values are typed.
#[derive(Clone, Debug)]
struct RawDecl {
    /// The property name as written.
    name: String,
    /// The value's raw text.
    value: String,
    /// Whether the declaration carried `!important`.
    important: bool,
    /// Whether the value was one of the shared keywords.
    keyword: KeywordForm,
    /// Where the declaration sits in the file, for reports.
    location: cssparser::SourceLocation,
}

/// A rule as read.
#[derive(Clone, Debug)]
struct UnparsedRule {
    /// The selector list.
    selectors: Vec<Selector>,
    /// The declarations.
    declarations: Vec<RawDecl>,
}

// =============================================================================
// Loading
// =============================================================================

/// Parse `source` into rules, `:root` variables and reports.
pub fn load(source: &str) -> (Vec<RawRule>, HashMap<String, String>, Vec<Report>) {
    let mut reports = Vec::new();
    let mut unparsed: Vec<UnparsedRule> = Vec::new();
    let mut variables: HashMap<String, String> = HashMap::new();
    let mut variable_sources: Vec<(String, String, cssparser::SourceLocation)> = Vec::new();

    let mut input = ParserInput::new(source);
    let mut parser = Parser::new(&mut input);
    while !parser.is_exhausted() {
        let rule_location = parser.current_source_location();
        // The prelude reader rewinds if it runs into the block's `{`:
        // consuming it here would make `parse_until_before` swallow the
        // whole block, and the block is entered explicitly below.
        let prelude: Result<String, ParseError<'_, ()>> =
            parser.parse_until_before(Delimiter::CurlyBracketBlock, |inner| {
                let start = inner.position();
                loop {
                    let before = inner.state();
                    match inner.next() {
                        Err(_) => {
                            let end = inner.position();
                            return Ok(strip_comments(inner.slice(start..end)));
                        }
                        Ok(Token::CurlyBracketBlock) => {
                            inner.reset(&before);
                            let end = inner.position();
                            return Ok(strip_comments(inner.slice(start..end)));
                        }
                        Ok(_) => {}
                    }
                }
            });
        let prelude = match prelude {
            Ok(prelude) => prelude,
            Err(_) => {
                report(
                    &mut reports,
                    ReportKind::Malformed,
                    "<rule>",
                    "the rule's prelude could not be read",
                    rule_location,
                );
                break;
            }
        };

        if prelude.starts_with('@') {
            let at_name = prelude.split_whitespace().next().unwrap_or("@").to_string();
            report(
                &mut reports,
                ReportKind::UnsupportedSelector,
                &at_name,
                "at-rules are not part of the CSS subset",
                rule_location,
            );
            // No at-rules are in the subset; skip the block, keep parsing.
            if parser.expect_curly_bracket_block().is_ok() {
                let _: Result<(), ParseError<'_, ()>> = parser.parse_nested_block(|block| {
                    while block.next().is_ok() {}
                    Ok(())
                });
            }
            continue;
        }

        let is_root = prelude.trim() == ":root";
        let selectors = if is_root {
            Vec::new()
        } else {
            match selector::parse_list(&prelude) {
                Ok(selectors) => selectors,
                Err(error) => {
                    report(
                        &mut reports,
                        ReportKind::UnsupportedSelector,
                        &prelude,
                        &error,
                        rule_location,
                    );
                    if parser.expect_curly_bracket_block().is_ok() {
                        let _: Result<(), ParseError<'_, ()>> =
                            parser.parse_nested_block(|block| {
                                while block.next().is_ok() {}
                                Ok(())
                            });
                    }
                    continue;
                }
            }
        };

        if parser.expect_curly_bracket_block().is_err() {
            report(
                &mut reports,
                ReportKind::Malformed,
                &prelude,
                "the rule has no `{` block",
                rule_location,
            );
            continue;
        }
        let declarations: Result<Vec<RawDecl>, ParseError<'_, ()>> =
            parser.parse_nested_block(|block| Ok(parse_declarations(block, &mut reports)));
        let declarations = match declarations {
            Ok(declarations) => declarations,
            Err(_) => {
                report(
                    &mut reports,
                    ReportKind::Malformed,
                    &prelude,
                    "the rule's block could not be read",
                    rule_location,
                );
                continue;
            }
        };

        if is_root {
            for declaration in &declarations {
                if declaration.name.starts_with("--") {
                    variable_sources.push((
                        declaration.name.clone(),
                        declaration.value.clone(),
                        declaration.location,
                    ));
                }
            }
        } else {
            unparsed.push(UnparsedRule {
                selectors,
                declarations,
            });
        }
    }

    // Resolve the variables in dependency order, however the file wrote them.
    resolve_variables(&mut variable_sources, &mut variables, &mut reports);

    // Substitute and type every declaration now that variables exist.
    let mut rules = Vec::new();
    for rule in unparsed {
        let mut entries = Vec::new();
        for declaration in rule.declarations {
            let location = declaration.location;
            let shared = Entry {
                declaration: None,
                declaration_name: declaration.name.clone(),
                inherit: declaration.keyword == KeywordForm::Inherit,
                unset: declaration.keyword == KeywordForm::Unset,
                important: declaration.important,
            };
            if declaration.keyword != KeywordForm::Value {
                entries.push(shared);
                continue;
            }
            let substituted = match substitute_vars(&declaration.value, &variables, 0) {
                Ok(value) => value,
                Err(problem) => {
                    report(
                        &mut reports,
                        ReportKind::UnsupportedValue,
                        &declaration.name,
                        &problem,
                        location,
                    );
                    continue;
                }
            };
            match type_declarations(&declaration.name, &substituted) {
                Ok(typed) => {
                    for declaration_type in typed {
                        entries.push(Entry {
                            declaration: Some(declaration_type),
                            declaration_name: shared.declaration_name.clone(),
                            ..shared.clone()
                        });
                    }
                }
                Err(problem) => {
                    let kind = if SUPPORTED_PROPERTIES.contains(&declaration.name.as_str()) {
                        ReportKind::UnsupportedValue
                    } else {
                        ReportKind::UnsupportedProperty
                    };
                    report(&mut reports, kind, &declaration.name, &problem, location);
                }
            }
        }
        rules.push(RawRule {
            selectors: rule.selectors,
            declarations: entries,
        });
    }

    (rules, variables, reports)
}

/// Resolve pending variable values against each other until none change.
fn resolve_variables(
    pending: &mut Vec<(String, String, cssparser::SourceLocation)>,
    variables: &mut HashMap<String, String>,
    reports: &mut Vec<Report>,
) {
    for _ in 0..16 {
        if pending.is_empty() {
            return;
        }
        let mut progressed = false;
        let mut remaining = Vec::new();
        for (name, raw, location) in pending.drain(..) {
            match substitute_vars(&raw, variables, 0) {
                Ok(resolved) => {
                    variables.insert(name, resolved);
                    progressed = true;
                }
                Err(_) => remaining.push((name, raw, location)),
            }
        }
        *pending = remaining;
        if !progressed {
            break;
        }
    }
    for (name, _, location) in pending.iter() {
        report(
            reports,
            ReportKind::UnsupportedValue,
            name,
            "the variable's value could not be resolved",
            *location,
        );
    }
    pending.clear();
}

// =============================================================================
// Declarations
// =============================================================================

/// Read every declaration of a block, as raw name and value text with
/// positions.
fn parse_declarations(block: &mut Parser<'_, '_>, reports: &mut Vec<Report>) -> Vec<RawDecl> {
    let mut declarations = Vec::new();
    loop {
        let location = block.current_source_location();
        match block.next() {
            Ok(Token::WhiteSpace(_)) | Ok(Token::Semicolon) => continue,
            Ok(Token::Ident(name)) => {
                let name = name.to_string();
                if !matches!(block.next(), Ok(Token::Colon)) {
                    report(
                        reports,
                        ReportKind::Malformed,
                        &name,
                        "expected `:` after the property",
                        location,
                    );
                    skip_to_semicolon(block);
                    continue;
                }
                let value: Result<String, ParseError<'_, ()>> =
                    block.parse_until_before(Delimiter::Semicolon, |value_parser| {
                        let mut text = String::new();
                        serialize_value(value_parser, &mut text)?;
                        Ok(text.trim().to_string())
                    });
                match value {
                    Ok(raw) => {
                        let (raw, important) = split_important(&raw);
                        declarations.push(RawDecl {
                            keyword: keyword_form(&raw),
                            name,
                            value: raw,
                            important,
                            location,
                        });
                    }
                    Err(_) => {
                        report(
                            reports,
                            ReportKind::Malformed,
                            &name,
                            "the value could not be read",
                            location,
                        );
                        skip_to_semicolon(block);
                    }
                }
            }
            Ok(_) | Err(_) => break,
        }
    }
    declarations
}

fn skip_to_semicolon(block: &mut Parser<'_, '_>) {
    loop {
        match block.next() {
            Ok(Token::Semicolon) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

/// Remove `/* ... */` comments from a rule prelude (the tokenizer skips
/// them, but the *slice* still holds them) and trim the rest.
fn strip_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("/*") {
        out.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Write a declaration value's canonical text, entering every nested block:
/// `var(--x)`, `rgb(1, 2, 3)` and plain words all survive, where stepping
/// with `next()` alone would swallow the contents of any block opener.
fn serialize_value<'i>(
    parser: &mut Parser<'i, '_>,
    out: &mut String,
) -> Result<(), ParseError<'i, ()>> {
    loop {
        match parser.next_including_whitespace_and_comments() {
            Ok(Token::Function(name)) => {
                out.push_str(name);
                out.push('(');
                parser.parse_nested_block(|inner| serialize_value(inner, out))?;
                out.push(')');
            }
            Ok(Token::ParenthesisBlock) => {
                out.push('(');
                parser.parse_nested_block(|inner| serialize_value(inner, out))?;
                out.push(')');
            }
            Ok(Token::SquareBracketBlock) => {
                out.push('[');
                parser.parse_nested_block(|inner| serialize_value(inner, out))?;
                out.push(']');
            }
            Ok(token) => {
                // Every other token serializes to its own text; a failure
                // is a writing failure, which a `String` cannot have.
                let _ = token.to_css(out);
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Strip a trailing `!important`, however it is spaced.
fn split_important(raw: &str) -> (String, bool) {
    let trimmed = raw.trim_end();
    let Some(bang) = trimmed.rfind('!') else {
        return (raw.to_string(), false);
    };
    let marker = trimmed[bang + 1..].trim();
    if marker.eq_ignore_ascii_case("important") {
        (trimmed[..bang].trim_end().to_string(), true)
    } else {
        (raw.to_string(), false)
    }
}

/// The keyword forms every property shares.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum KeywordForm {
    /// `inherit`.
    Inherit,
    /// `unset` or `revert`.
    Unset,
    /// An ordinary value.
    Value,
}

fn keyword_form(raw: &str) -> KeywordForm {
    match raw.trim().to_ascii_lowercase().as_str() {
        "inherit" => KeywordForm::Inherit,
        "unset" | "revert" => KeywordForm::Unset,
        _ => KeywordForm::Value,
    }
}

/// Replace every `var(--name[, fallback])` with its value.
fn substitute_vars(
    raw: &str,
    variables: &HashMap<String, String>,
    depth: u32,
) -> Result<String, String> {
    if depth > 8 {
        return Err(String::from(
            "the variable references are nested too deeply",
        ));
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.to_ascii_lowercase().find("var(") {
        out.push_str(&rest[..start]);
        let inner_start = start + 4;
        let Some(end) = find_matching_paren(&rest[inner_start..]) else {
            return Err(String::from("a `var(` has no closing `)`"));
        };
        let inner = &rest[inner_start..inner_start + end];
        rest = &rest[inner_start + end + 1..];
        let (name, fallback) = match inner.split_once(',') {
            Some((name, fallback)) => (name.trim(), Some(fallback.trim())),
            None => (inner.trim(), None),
        };
        if !name.starts_with("--") {
            return Err(format!("`{name}` is not a custom property name"));
        }
        match variables.get(name) {
            Some(resolved) => {
                let resolved = substitute_vars(resolved, variables, depth + 1)?;
                out.push_str(&resolved);
            }
            None => match fallback {
                Some(fallback) => {
                    out.push_str(&substitute_vars(fallback, variables, depth + 1)?);
                }
                None => return Err(format!("`{name}` is not defined in `:root`")),
            },
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// The offset just past the `)` matching the `(` the slice starts after.
fn find_matching_paren(text: &str) -> Option<usize> {
    let mut depth = 1usize;
    for (index, character) in text.char_indices() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

// =============================================================================
// Typing
// =============================================================================

/// Every property name the subset knows.
pub const SUPPORTED_PROPERTIES: &[&str] = &[
    "background",
    "background-color",
    "border",
    "border-bottom",
    "border-bottom-color",
    "border-bottom-style",
    "border-bottom-width",
    "border-color",
    "border-left",
    "border-left-color",
    "border-left-style",
    "border-left-width",
    "border-radius",
    "border-right",
    "border-right-color",
    "border-right-style",
    "border-right-width",
    "border-style",
    "border-top",
    "border-top-color",
    "border-top-style",
    "border-top-width",
    "border-width",
    "bottom",
    "box-shadow",
    "box-sizing",
    "color",
    "column-gap",
    "cursor",
    "display",
    "flex",
    "flex-basis",
    "flex-direction",
    "flex-grow",
    "flex-shrink",
    "font",
    "font-family",
    "font-size",
    "font-weight",
    "gap",
    "height",
    "inset",
    "justify-content",
    "left",
    "line-height",
    "margin",
    "margin-bottom",
    "margin-left",
    "margin-right",
    "margin-top",
    "max-height",
    "max-width",
    "min-height",
    "min-width",
    "opacity",
    "outline",
    "outline-offset",
    "overflow",
    "overflow-x",
    "overflow-y",
    "padding",
    "padding-bottom",
    "padding-left",
    "padding-right",
    "padding-top",
    "pointer-events",
    "position",
    "right",
    "row-gap",
    "text-align",
    "text-overflow",
    "top",
    "user-select",
    "white-space",
    "width",
    "z-index",
];

/// Type one declaration's value into one or more typed declarations; the
/// error names what was refused.
fn type_declarations(name: &str, raw: &str) -> Result<Vec<Declaration>, String> {
    if name == "flex" {
        return parse_flex_shorthand(raw);
    }
    let words = value::words(raw);
    fn single(words: &[value::Component]) -> Result<&str, String> {
        match words {
            [only] => Ok(only.text.as_str()),
            _ => Err(String::from("expected a single value")),
        }
    }
    let length = |text: &str| -> Result<f32, String> {
        match parse_length(text) {
            Some(Length::Px(value)) => Ok(value),
            Some(Length::Percent(_)) => {
                Err(format!("percentages are not supported here (`{text}`)"))
            }
            Some(Length::Auto) => Err(String::from("`auto` is not supported here")),
            None => Err(format!("`{text}` is not a length")),
        }
    };

    let declaration = match name {
        "background" | "background-color" => {
            Declaration::Background(parse_color(single(&words)?).ok_or_else(|| {
                format!("`{raw}` is not a colour or plain background (no gradients)")
            })?)
        }
        "color" => Declaration::Color(
            parse_color(single(&words)?).ok_or_else(|| format!("`{raw}` is not a colour"))?,
        ),
        "opacity" => Declaration::Opacity(
            parse_number(single(&words)?, 0.0..=1.0)
                .ok_or_else(|| format!("`{raw}` is not a number in 0..=1"))?,
        ),
        "font-size" => Declaration::FontSize(length(single(&words)?)?),
        "font-weight" => Declaration::FontWeight(parse_font_weight(single(&words)?)?),
        "font-family" => Declaration::FontFamily(first_family(raw)?),
        "font" => Declaration::Font(parse_font_shorthand(raw)?),
        "line-height" => {
            let text = single(&words)?;
            let value = if let Some(px) = text.strip_suffix("px") {
                LineHeight::Px(
                    px.trim()
                        .parse()
                        .map_err(|_| format!("`{text}` is not a line height"))?,
                )
            } else {
                LineHeight::Factor(
                    text.parse()
                        .map_err(|_| format!("`{text}` is not a line height"))?,
                )
            };
            Declaration::LineHeight(value)
        }
        "text-align" => Declaration::TextAlign(match single(&words)? {
            "left" | "start" => TextAlign::Left,
            "center" => TextAlign::Center,
            "right" | "end" => TextAlign::Right,
            other => return Err(format!("`{other}` is not a text alignment")),
        }),
        "white-space" => Declaration::WhiteSpace(match single(&words)? {
            "normal" => WhiteSpace::Normal,
            "nowrap" => WhiteSpace::Nowrap,
            other => return Err(format!("`{other}` is not supported for white-space")),
        }),
        "text-overflow" => Declaration::TextOverflow(match single(&words)? {
            "clip" => TextOverflow::Clip,
            "ellipsis" => TextOverflow::Ellipsis,
            other => return Err(format!("`{other}` is not supported for text-overflow")),
        }),
        "padding" | "margin" => {
            let lengths: Result<Vec<Length>, String> = words
                .iter()
                .map(|word| {
                    parse_length(&word.text)
                        .ok_or_else(|| format!("`{}` is not a length", word.text))
                })
                .collect();
            let lengths = lengths?;
            if name == "padding" {
                let pixels: Result<Vec<Option<f32>>, String> = lengths
                    .iter()
                    .map(|length| match length {
                        Length::Px(value) => Ok(Some(*value)),
                        _ => Err(format!("padding takes pixels only (`{raw}`)")),
                    })
                    .collect();
                Declaration::Padding(
                    sides_from(&pixels?).ok_or_else(|| format!("`{raw}` has too many sides"))?,
                )
            } else {
                let sides = sides_from(&lengths)
                    .ok_or_else(|| format!("`{raw}` has too many sides"))?
                    .map(Some);
                Declaration::Margin(sides)
            }
        }
        "padding-top" | "padding-right" | "padding-bottom" | "padding-left" => {
            let value = length(single(&words)?)?;
            let mut sides = Sides::all(None);
            match side_of(name) {
                Side::Top => sides.top = Some(value),
                Side::Right => sides.right = Some(value),
                Side::Bottom => sides.bottom = Some(value),
                Side::Left => sides.left = Some(value),
            }
            Declaration::Padding(sides)
        }
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            let value =
                parse_length(single(&words)?).ok_or_else(|| format!("`{raw}` is not a length"))?;
            let mut sides = Sides::all(None);
            match side_of(name) {
                Side::Top => sides.top = Some(value),
                Side::Right => sides.right = Some(value),
                Side::Bottom => sides.bottom = Some(value),
                Side::Left => sides.left = Some(value),
            }
            Declaration::Margin(sides)
        }
        "border" => {
            let side = parse_border_shorthand(&words)?;
            Declaration::Border(Sides::all(side))
        }
        "border-top" | "border-right" | "border-bottom" | "border-left" => {
            let side = parse_border_shorthand(&words)?;
            let mut sides = Sides::all(BorderSide::default());
            match side_of(name) {
                Side::Top => sides.top = side,
                Side::Right => sides.right = side,
                Side::Bottom => sides.bottom = side,
                Side::Left => sides.left = side,
            }
            Declaration::Border(sides)
        }
        "border-width" => {
            let pixels: Result<Vec<f32>, String> =
                words.iter().map(|word| length(&word.text)).collect();
            let sides =
                sides_from(&pixels?).ok_or_else(|| format!("`{raw}` has too many sides"))?;
            Declaration::Border(Sides {
                top: BorderSide {
                    width: Some(sides.top),
                    ..BorderSide::default()
                },
                right: BorderSide {
                    width: Some(sides.right),
                    ..BorderSide::default()
                },
                bottom: BorderSide {
                    width: Some(sides.bottom),
                    ..BorderSide::default()
                },
                left: BorderSide {
                    width: Some(sides.left),
                    ..BorderSide::default()
                },
            })
        }
        "border-style" => {
            let mut widths = Vec::new();
            for word in &words {
                widths.push(parse_border_style(&word.text)?);
            }
            let sides = sides_from(&widths).ok_or_else(|| format!("`{raw}` has too many sides"))?;
            Declaration::Border(Sides {
                top: BorderSide {
                    width: Some(sides.top),
                    ..BorderSide::default()
                },
                right: BorderSide {
                    width: Some(sides.right),
                    ..BorderSide::default()
                },
                bottom: BorderSide {
                    width: Some(sides.bottom),
                    ..BorderSide::default()
                },
                left: BorderSide {
                    width: Some(sides.left),
                    ..BorderSide::default()
                },
            })
        }
        "border-color" => {
            let colors: Result<Vec<Color>, String> = words
                .iter()
                .map(|word| {
                    parse_color(&word.text)
                        .ok_or_else(|| format!("`{}` is not a colour", word.text))
                })
                .collect();
            let sides =
                sides_from(&colors?).ok_or_else(|| format!("`{raw}` has too many sides"))?;
            Declaration::Border(Sides {
                top: BorderSide {
                    color: Some(sides.top),
                    ..BorderSide::default()
                },
                right: BorderSide {
                    color: Some(sides.right),
                    ..BorderSide::default()
                },
                bottom: BorderSide {
                    color: Some(sides.bottom),
                    ..BorderSide::default()
                },
                left: BorderSide {
                    color: Some(sides.left),
                    ..BorderSide::default()
                },
            })
        }
        "border-top-width" | "border-right-width" | "border-bottom-width" | "border-left-width" => {
            let value = length(single(&words)?)?;
            let mut sides = Sides::all(BorderSide::default());
            let side = side_of(name);
            match side {
                Side::Top => sides.top.width = Some(value),
                Side::Right => sides.right.width = Some(value),
                Side::Bottom => sides.bottom.width = Some(value),
                Side::Left => sides.left.width = Some(value),
            }
            Declaration::Border(sides)
        }
        "border-top-color" | "border-right-color" | "border-bottom-color" | "border-left-color" => {
            let color =
                parse_color(single(&words)?).ok_or_else(|| format!("`{raw}` is not a colour"))?;
            let mut sides = Sides::all(BorderSide::default());
            match side_of(name) {
                Side::Top => sides.top.color = Some(color),
                Side::Right => sides.right.color = Some(color),
                Side::Bottom => sides.bottom.color = Some(color),
                Side::Left => sides.left.color = Some(color),
            }
            Declaration::Border(sides)
        }
        "border-top-style" | "border-right-style" | "border-bottom-style" | "border-left-style" => {
            let width = parse_border_style(single(&words)?)?;
            let mut sides = Sides::all(BorderSide::default());
            match side_of(name) {
                Side::Top => sides.top.width = Some(width),
                Side::Right => sides.right.width = Some(width),
                Side::Bottom => sides.bottom.width = Some(width),
                Side::Left => sides.left.width = Some(width),
            }
            Declaration::Border(sides)
        }
        "border-radius" => Declaration::BorderRadius(length(single(&words)?)?),
        "box-shadow" => Declaration::BoxShadow(raw.to_string()),
        "outline" => parse_outline(&words)?,
        "outline-offset" => Declaration::OutlineOffset(length(single(&words)?)?),
        "box-sizing" => Declaration::BoxSizing(match single(&words)? {
            "border-box" => BoxSizingKind::BorderBox,
            "content-box" => BoxSizingKind::ContentBox,
            other => return Err(format!("`{other}` is not a box-sizing")),
        }),
        "display" => Declaration::Display(match single(&words)? {
            "flex" => DisplayKind::Flex,
            "block" => DisplayKind::Block,
            "none" => DisplayKind::None,
            other => return Err(format!("display: {other} is not supported")),
        }),
        "flex-direction" => Declaration::FlexDirection(match single(&words)? {
            "row" => FlexDirectionKind::Row,
            "row-reverse" => FlexDirectionKind::RowReverse,
            "column" => FlexDirectionKind::Column,
            "column-reverse" => FlexDirectionKind::ColumnReverse,
            other => return Err(format!("flex-direction: {other} is not supported")),
        }),
        "flex-grow" => Declaration::FlexGrow(number(single(&words)?, raw)?),
        "flex-shrink" => Declaration::FlexShrink(number(single(&words)?, raw)?),
        "flex-basis" => Declaration::FlexBasis(
            parse_size(single(&words)?).ok_or_else(|| format!("`{raw}` is not a flex basis"))?,
        ),
        "align-items" => Declaration::AlignItems(match single(&words)? {
            "stretch" => AlignKind::Stretch,
            "center" => AlignKind::Center,
            "flex-start" | "start" => AlignKind::FlexStart,
            "flex-end" | "end" => AlignKind::FlexEnd,
            other => return Err(format!("align-items: {other} is not supported")),
        }),
        "justify-content" => Declaration::JustifyContent(match single(&words)? {
            "flex-start" | "start" | "left" => JustifyKind::Start,
            "flex-end" | "end" | "right" => JustifyKind::End,
            "center" => JustifyKind::Center,
            "space-between" => JustifyKind::SpaceBetween,
            other => return Err(format!("justify-content: {other} is not supported")),
        }),
        "gap" => {
            let values: Result<Vec<f32>, String> =
                words.iter().map(|word| length(&word.text)).collect();
            let values = values?;
            let (row, column) = match values.as_slice() {
                [both] => (Some(*both), Some(*both)),
                [row, column] => (Some(*row), Some(*column)),
                _ => return Err(format!("`{raw}` is not a gap")),
            };
            Declaration::Gap { row, column }
        }
        "row-gap" => Declaration::Gap {
            row: Some(length(single(&words)?)?),
            column: None,
        },
        "column-gap" => Declaration::Gap {
            row: None,
            column: Some(length(single(&words)?)?),
        },
        "width" | "height" | "min-width" | "min-height" | "max-width" | "max-height" => {
            let size =
                parse_size(single(&words)?).ok_or_else(|| format!("`{raw}` is not a size"))?;
            match name {
                "width" => Declaration::Width(size),
                "height" => Declaration::Height(size),
                "min-width" => Declaration::MinWidth(size),
                "min-height" => Declaration::MinHeight(size),
                "max-width" => Declaration::MaxWidth(size),
                _ => Declaration::MaxHeight(size),
            }
        }
        "position" => Declaration::Position(match single(&words)? {
            "relative" => PositionKind::Relative,
            "absolute" => PositionKind::Absolute,
            "fixed" => PositionKind::Fixed,
            "static" => PositionKind::Relative,
            other => return Err(format!("position: {other} is not supported")),
        }),
        "inset" => {
            let values: Result<Vec<Option<Length>>, String> = words
                .iter()
                .map(|word| {
                    parse_length(&word.text)
                        .map(Some)
                        .ok_or_else(|| format!("`{}` is not a length", word.text))
                })
                .collect();
            Declaration::Inset(
                sides_from(&values?).ok_or_else(|| format!("`{raw}` has too many sides"))?,
            )
        }
        "top" | "right" | "bottom" | "left" => {
            let value =
                parse_length(single(&words)?).ok_or_else(|| format!("`{raw}` is not a length"))?;
            let mut sides = Sides::all(None);
            match side_of(name) {
                Side::Top => sides.top = Some(value),
                Side::Right => sides.right = Some(value),
                Side::Bottom => sides.bottom = Some(value),
                Side::Left => sides.left = Some(value),
            }
            Declaration::Inset(sides)
        }
        "overflow" | "overflow-x" | "overflow-y" => {
            let value = parse_overflow(single(&words)?)?;
            let (x, y) = match name {
                "overflow-x" => (Some(value), None),
                "overflow-y" => (None, Some(value)),
                _ => (Some(value), Some(value)),
            };
            Declaration::Overflow { x, y }
        }
        "z-index" => Declaration::ZIndex(
            single(&words)?
                .parse()
                .map_err(|_| format!("`{raw}` is not a z-index"))?,
        ),
        "cursor" => Declaration::Cursor(single(&words)?.to_string()),
        "pointer-events" => Declaration::PointerEvents(match single(&words)? {
            "none" => false,
            "auto" => true,
            other => return Err(format!("pointer-events: {other} is not supported")),
        }),
        "user-select" => Declaration::UserSelect(match single(&words)? {
            "none" => false,
            "text" => true,
            other => return Err(format!("user-select: {other} is not supported")),
        }),
        other => return Err(format!("`{other}` is not in the CSS subset")),
    };
    Ok(vec![declaration])
}

fn number(text: &str, raw: &str) -> Result<f32, String> {
    text.parse().map_err(|_| format!("`{raw}` is not a number"))
}

fn side_of(name: &str) -> Side {
    if name.ends_with("top") {
        Side::Top
    } else if name.ends_with("right") {
        Side::Right
    } else if name.ends_with("bottom") {
        Side::Bottom
    } else {
        Side::Left
    }
}

enum Side {
    Top,
    Right,
    Bottom,
    Left,
}

fn parse_border_shorthand(words: &[value::Component]) -> Result<BorderSide, String> {
    let mut side = BorderSide::default();
    for word in words {
        if let Some(px) = parse_length(&word.text).and_then(Length::px) {
            side.width = Some(px);
        } else if let Some(color) = parse_color(&word.text) {
            side.color = Some(color);
        } else {
            side.width = Some(parse_border_style(&word.text)?);
        }
    }
    Ok(side)
}

/// The stroke widths `solid` and `none` mean; other styles are refused by
/// name before reaching here.
fn parse_border_style(text: &str) -> Result<f32, String> {
    match text {
        "solid" => Ok(1.0),
        "none" | "hidden" => Ok(0.0),
        other => Err(format!(
            "border-style: {other} is not supported (solid only)"
        )),
    }
}

/// `flex: none` and friends expand into their longhands.
fn parse_flex_shorthand(raw: &str) -> Result<Vec<Declaration>, String> {
    let mut declarations = Vec::new();
    let mut components = value::words(raw);
    if components.len() == 1 && components[0].text == "none" {
        return Ok(vec![
            Declaration::FlexGrow(0.0),
            Declaration::FlexShrink(0.0),
            Declaration::FlexBasis(Size::Length(Length::Auto)),
        ]);
    }
    // `flex: <grow> <shrink>? <basis>?`, in any of the three orders the
    // app cares about.
    let mut numbers = Vec::new();
    while let Some(component) = components.first() {
        if let Ok(number) = component.text.parse::<f32>() {
            numbers.push(number);
            components.remove(0);
        } else {
            break;
        }
    }
    let basis = match components.first() {
        Some(component) => Some(
            parse_size(&component.text)
                .ok_or_else(|| format!("`{}` is not a flex basis", component.text))?,
        ),
        None => None,
    };
    let grow = numbers.first().copied().unwrap_or(1.0);
    let shrink = numbers.get(1).copied().unwrap_or(1.0);
    declarations.push(Declaration::FlexGrow(grow));
    declarations.push(Declaration::FlexShrink(shrink));
    declarations.push(Declaration::FlexBasis(match basis {
        Some(basis) => basis,
        // The CSS initial value: the basis follows width/height.
        None => Size::Length(Length::Auto),
    }));
    Ok(declarations)
}

fn parse_outline(words: &[value::Component]) -> Result<Declaration, String> {
    let mut width = 1.0f32;
    let mut color = None;
    for word in words {
        if word.text == "none" {
            width = 0.0;
        } else if let Some(px) = parse_length(&word.text).and_then(Length::px) {
            width = px;
        } else if let Some(parsed) = parse_color(&word.text) {
            color = Some(parsed);
        } else {
            parse_border_style(&word.text)?;
        }
    }
    Ok(Declaration::Outline {
        width,
        // `invert` and `currentColor` are not in the subset; white is the
        // fallback the design uses explicitly everywhere anyway.
        color: color.unwrap_or(Color::WHITE),
    })
}

fn parse_font_weight(text: &str) -> Result<u16, String> {
    match text {
        "normal" => Ok(400),
        "bold" => Ok(700),
        other => other
            .parse()
            .map_err(|_| format!("`{other}` is not a font weight")),
    }
}

fn first_family(raw: &str) -> Result<String, String> {
    raw.split(',')
        .next()
        .map(|family| family.trim().trim_matches(['"', '\'']).to_string())
        .filter(|family| !family.is_empty())
        .ok_or_else(|| String::from("no font family named"))
}

fn parse_font_shorthand(raw: &str) -> Result<FontSpec, String> {
    let mut spec = FontSpec {
        size: None,
        line_height: None,
        weight: None,
        family: None,
    };
    let components = value::components(raw);
    for (index, word) in components.iter().enumerate() {
        match word.text.as_str() {
            "," => continue,
            "normal" | "italic" | "oblique" => continue,
            "bold" => spec.weight = Some(700),
            "bolder" => spec.weight = Some(700),
            "lighter" => spec.weight = Some(300),
            other => {
                // `12px`, `12px/1.4` or a weight number before the size.
                let (size_part, height_part) = match other.split_once('/') {
                    Some((size, height)) => (size, Some(height)),
                    None => (other, None),
                };
                if let Some(size) = size_part
                    .strip_suffix("px")
                    .and_then(|px| px.parse::<f32>().ok())
                {
                    spec.size = Some(size);
                    spec.line_height = match height_part {
                        Some("") => None,
                        Some(height) => Some(if let Some(px) = height.strip_suffix("px") {
                            LineHeight::Px(
                                px.parse()
                                    .map_err(|_| format!("`{height}` is not a line height"))?,
                            )
                        } else {
                            LineHeight::Factor(
                                height
                                    .parse()
                                    .map_err(|_| format!("`{height}` is not a line height"))?,
                            )
                        }),
                        None => None,
                    };
                    // The first family is everything after the size up to
                    // the next comma, as written (spaces and all).
                    let tail: Vec<String> = components[index + 1..]
                        .iter()
                        .take_while(|component| component.text != ",")
                        .map(|component| component.source.clone())
                        .collect();
                    if !tail.is_empty() {
                        spec.family = Some(tail.join(" "));
                    }
                } else if let Ok(weight) = other.parse::<u16>() {
                    spec.weight = Some(weight);
                }
            }
        }
    }
    if spec.size.is_none() {
        return Err(format!("the `font` shorthand lacks a size (`{raw}`)"));
    }
    if spec.family.is_none() {
        spec.family = Some(first_family(raw).unwrap_or_else(|_| String::from("sans-serif")));
    }
    Ok(spec)
}

fn parse_overflow(text: &str) -> Result<OverflowKind, String> {
    match text {
        "visible" => Ok(OverflowKind::Visible),
        "hidden" => Ok(OverflowKind::Hidden),
        "auto" => Ok(OverflowKind::Auto),
        "scroll" => Ok(OverflowKind::Scroll),
        "clip" => Ok(OverflowKind::Clip),
        other => Err(format!("overflow: {other} is not supported")),
    }
}

// =============================================================================
// Errors
// =============================================================================

fn report(
    reports: &mut Vec<Report>,
    kind: ReportKind,
    subject: &str,
    detail: &str,
    location: cssparser::SourceLocation,
) {
    reports.push(Report {
        kind,
        subject: subject.to_string(),
        detail: detail.to_string(),
        // `cssparser` counts lines from 0 and columns from 1; reports count
        // lines from 1, like an editor does.
        line: location.line + 1,
        column: location.column,
    });
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variables_substitute_anywhere_in_a_value() {
        let mut variables = HashMap::new();
        variables.insert(String::from("--text"), String::from("rgb(10, 20, 30)"));
        variables.insert(String::from("--pad"), String::from("6px"));
        assert_eq!(
            substitute_vars("var(--text)", &variables, 0).unwrap(),
            "rgb(10, 20, 30)"
        );
        assert_eq!(
            substitute_vars("var(--pad, 4px)", &variables, 0).unwrap(),
            "6px"
        );
        assert_eq!(
            substitute_vars("var(--missing, 4px)", &variables, 0).unwrap(),
            "4px"
        );
        assert!(substitute_vars("var(--missing)", &variables, 0).is_err());
    }

    #[test]
    fn a_full_load_reads_rules_and_reports_nothing_for_the_subset() {
        let (rules, variables, reports) = load(
            "
            :root { --text: rgb(10, 20, 30); }
            .x { color: var(--text); padding: 1px 2px; }
            ",
        );
        assert!(reports.is_empty(), "{reports:?}");
        assert_eq!(
            variables.get("--text").map(String::as_str),
            Some("rgb(10, 20, 30)")
        );
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].declarations.len(), 2);
        assert!(rules[0].declarations[0].declaration.is_some());
    }
}
