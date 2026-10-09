//! The CSS subset: a stylesheet, its rules, and what they compute to.
//!
//! # Responsibilities
//!
//! - [`Stylesheet`]: one parsed stylesheet - rules, `:root` variables and
//!   the [`Report`]s naming everything the subset refused - plus the style
//!   cache keyed by a node's full matching context.
//! - [`Stylesheet::resolve`]: the cascade for one node: match every rule
//!   against the node's facts and its ancestors', order by importance,
//!   specificity and source position, and apply everything that matched on
//!   top of the inheritable values.
//! - Modules: [`parse`] (cssparser in, typed declarations out), [`selector`]
//!   (selectors and specificity), [`style`] (the computed style),
//!   [`value`] (typed values).
//!
//! # Design
//!
//! The subset is deliberate: whatever it does not implement is reported by
//! name, file and line
//! ([`docs/css_subset.md`](../../docs/css_subset.md) documents it), never
//! guessed at. A `pill_ui` frame carries a `&mut Stylesheet`; nodes carry
//! their type, classes and name, so matching is the same walk layout does.
//! The cache key is a hash of every fact a selector may look at - the whole
//! ancestor chain's types, classes, names and states - so a hover change is
//! a new key, never a stale hit.

// Standard library
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

// crate modules
/// cssparser in, typed declarations and reports out.
pub mod parse;
/// Selectors: parsing, matching and specificity.
pub mod selector;
/// The computed style and the cascade that fills it.
pub mod style;
/// Typed values: components, lengths, colours, sides.
pub mod value;

pub use parse::{
    AlignKind, BorderSide, BoxSizingKind, Declaration, DisplayKind, Entry, FlexDirectionKind,
    FontSpec, JustifyKind, LineHeight, OverflowKind, PositionKind, RawRule, Report, ReportKind,
    Shadow, TextAlign, TextOverflow, WhiteSpace,
};
pub use selector::{NodeFacts, PseudoClass, Selector, Specificity};
pub use style::ComputedStyle;
pub use value::{Length, Sides, Size};

/// One parsed stylesheet.
#[derive(Debug)]
pub struct Stylesheet {
    /// The rules, in source order.
    rules: Vec<RawRule>,
    /// The `:root` custom properties, resolved.
    variables: HashMap<String, String>,
    /// Everything the subset refused, in source order.
    reports: Vec<Report>,
    /// Computed styles by matching context.
    cache: HashMap<u64, ComputedStyle>,
}

impl Default for Stylesheet {
    fn default() -> Stylesheet {
        Stylesheet::empty()
    }
}

impl Stylesheet {
    /// A stylesheet with no rules: every node falls back to the theme.
    pub fn empty() -> Stylesheet {
        Stylesheet {
            rules: Vec::new(),
            variables: HashMap::new(),
            reports: Vec::new(),
            cache: HashMap::new(),
        }
    }

    /// Parse `source`; whatever the subset refused lands in
    /// [`Self::reports`].
    pub fn parse(source: &str) -> Stylesheet {
        let (rules, variables, reports) = parse::load(source);
        Stylesheet {
            rules,
            variables,
            reports,
            cache: HashMap::new(),
        }
    }

    /// Everything the subset refused, in source order.
    pub fn reports(&self) -> &[Report] {
        &self.reports
    }

    /// The resolved `:root` variables.
    pub fn variables(&self) -> &HashMap<String, String> {
        &self.variables
    }

    /// How many rules parsed.
    pub fn rule_count(&self) -> usize {
        self.rules.len()
    }

    /// Forget every computed style (a reloaded file never reuses one).
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }

    /// The computed style for the node whose ancestor chain (root first,
    /// subject last) is `chain`, with `parent` the parent's computed style
    /// for inheritance.
    ///
    /// `key` is [`key_for`] of the same chain; equal keys always resolve
    /// identically, which is what the cache relies on.
    pub fn resolve(
        &mut self,
        chain: &[NodeFacts<'_>],
        key: u64,
        parent: &ComputedStyle,
    ) -> ComputedStyle {
        if let Some(cached) = self.cache.get(&key) {
            return cached.clone();
        }

        let mut matched: Vec<Match<'_>> = Vec::new();
        for (order, rule) in self.rules.iter().enumerate() {
            let mut best: Option<Specificity> = None;
            for selector in &rule.selectors {
                if let Some(specificity) = selector::matches(selector, chain) {
                    best = Some(match best {
                        Some(previous) if previous > specificity => previous,
                        _ => specificity,
                    });
                }
            }
            let Some(specificity) = best else {
                continue;
            };
            for entry in &rule.declarations {
                matched.push(Match {
                    entry,
                    important: entry.important,
                    specificity,
                    order,
                });
            }
        }
        matched.sort_by_key(|entry| (entry.important, entry.specificity, entry.order));

        let mut style = ComputedStyle::default();
        style::inherit(parent, &mut style);
        for matched in &matched {
            match &matched.entry.declaration {
                Some(declaration) => style::apply(&mut style, declaration),
                None => {
                    // `inherit` (and, for the inherited set, `unset`) copies
                    // the parent's value.
                    style::apply_inherit(&mut style, &matched.entry.declaration_name, parent);
                }
            }
        }
        self.cache.insert(key, style.clone());
        style
    }
}

/// One matched declaration, with everything the cascade sorts by.
struct Match<'a> {
    /// The declaration.
    entry: &'a Entry,
    /// Whether it carried `!important`.
    important: bool,
    /// The matching selector's specificity.
    specificity: Specificity,
    /// The rule's position in the file; later rules win ties.
    order: usize,
}

/// Hash the facts a selector can see: every chain level's type, classes,
/// name and state.
pub fn key_for(chain: &[NodeFacts<'_>]) -> u64 {
    let mut hasher = DefaultHasher::new();
    for facts in chain {
        facts.type_name.hash(&mut hasher);
        facts.classes.hash(&mut hasher);
        facts.id_name.hash(&mut hasher);
        facts.hovered.hash(&mut hasher);
        facts.active.hash(&mut hasher);
        facts.focused.hash(&mut hasher);
        facts.disabled.hash(&mut hasher);
    }
    hasher.finish()
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::Color;

    fn classes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    fn node<'a>(type_name: &'a str, classes: &'a [String]) -> NodeFacts<'a> {
        NodeFacts {
            type_name,
            classes,
            id_name: None,
            hovered: false,
            active: false,
            focused: false,
            disabled: false,
        }
    }

    /// Resolve one node with no ancestors.
    fn resolve<'a>(sheet: &mut Stylesheet, facts: NodeFacts<'a>) -> ComputedStyle {
        let chain = [facts];
        let key = key_for(&chain);
        sheet.resolve(&chain, key, &ComputedStyle::default())
    }

    #[test]
    fn an_empty_stylesheet_changes_nothing() {
        let mut sheet = Stylesheet::empty();
        let class_list = classes(&[]);
        let style = resolve(&mut sheet, node("button", &class_list));
        assert_eq!(style, ComputedStyle::default());
        assert!(sheet.reports().is_empty());
    }

    #[test]
    fn specificity_and_order_decide_the_cascade() {
        let mut sheet = Stylesheet::parse(
            "
            button { color: #ffffff; }
            .primary { color: #ff0000; }
            button.primary { color: #00ff00; }
            ",
        );
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());
        let primary = classes(&["primary"]);
        let styled = resolve(&mut sheet, node("button", &primary));
        assert_eq!(styled.color, Some(Color::rgba(0.0, 1.0, 0.0, 1.0)));

        // Same specificity: the later rule wins.
        let mut sheet = Stylesheet::parse(".a { color: white; } .b { color: black; }");
        let both = classes(&["a", "b"]);
        let styled = resolve(&mut sheet, node("label", &both));
        assert_eq!(styled.color, Some(Color::BLACK));
    }

    #[test]
    fn important_beats_specificity() {
        let mut sheet =
            Stylesheet::parse(".a.b.c { color: black; } #hero { color: red !important; }");
        let many = classes(&["a", "b", "c"]);
        let mut facts = node("label", &many);
        facts.id_name = Some("hero");
        let styled = resolve(&mut sheet, facts);
        assert_eq!(styled.color, Some(Color::rgba(1.0, 0.0, 0.0, 1.0)));
    }

    #[test]
    fn descendant_selectors_see_ancestors() {
        let mut sheet = Stylesheet::parse(".toolbar > button { background: #123456; }");
        let toolbar = classes(&["toolbar"]);
        let button = classes(&[]);
        let chain = [node("column", &toolbar), node("button", &button)];
        let key = key_for(&chain);
        let style = sheet.resolve(&chain, key, &ComputedStyle::default());
        assert_eq!(
            style.background,
            Some(Color::rgba(
                0x12 as f32 / 255.0,
                0x34 as f32 / 255.0,
                0x56 as f32 / 255.0,
                1.0
            ))
        );

        // A deeper chain does not match `>`; the child combinator must not
        // skip a level.
        let none = classes(&[]);
        let deeper = [
            node("column", &toolbar),
            node("row", &none),
            node("button", &button),
        ];
        let key = key_for(&deeper);
        let style = sheet.resolve(&deeper, key, &ComputedStyle::default());
        assert_eq!(style.background, None);
    }

    #[test]
    fn pseudo_classes_follow_the_state() {
        let mut sheet = Stylesheet::parse(".tab:hover { background: white; }");
        let tab = classes(&["tab"]);
        let idle = resolve(&mut sheet, node("row", &tab));
        assert_eq!(idle.background, None);

        let mut hovered = node("row", &tab);
        hovered.hovered = true;
        let styled = resolve(&mut sheet, hovered);
        assert_eq!(styled.background, Some(Color::WHITE));
    }

    #[test]
    fn variables_resolve_in_any_order() {
        let mut sheet = Stylesheet::parse(
            "
            .x { color: var(--text); padding: var(--pad, 4px); }
            :root { --pad: 6px; --text: rgb(10, 20, 30); }
            ",
        );
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());
        let x = classes(&["x"]);
        let styled = resolve(&mut sheet, node("label", &x));
        assert_eq!(
            styled.color,
            Some(Color::rgba(10.0 / 255.0, 20.0 / 255.0, 30.0 / 255.0, 1.0))
        );
        assert_eq!(styled.padding.map(|sides| sides.top), Some(6.0f32));
    }

    #[test]
    fn inheritance_reaches_children_and_named_inherit_overrides() {
        let mut sheet = Stylesheet::parse(
            "
            .panel { color: white; font-size: 12px; }
            .panel .quiet { color: inherit; font-size: 10px; }
            ",
        );
        let panel = classes(&["panel"]);
        let quiet = classes(&["quiet"]);
        let key = key_for(&[node("column", &panel)]);
        let parent = sheet.resolve(&[node("column", &panel)], key, &ComputedStyle::default());

        let chain = [node("column", &panel), node("label", &quiet)];
        let key = key_for(&chain);
        let child = sheet.resolve(&chain, key, &parent);
        assert_eq!(child.color, Some(Color::WHITE), "inherit copies the parent");
        assert_eq!(child.font_size, Some(10.0), "the child's own size wins");
    }

    #[test]
    fn unsupported_things_are_reported_with_positions() {
        let sheet = Stylesheet::parse(
            "
            .x { float: left; color: linear-gradient(red, blue); background: #fff; }
            [aria-selected=\"true\"] { color: white; }
            @media screen { .y { color: white; } }
            ",
        );
        let reports = sheet.reports();
        assert!(
            reports
                .iter()
                .any(|report| report.kind == ReportKind::UnsupportedProperty
                    && report.subject == "float"),
            "{reports:?}"
        );
        assert!(
            reports
                .iter()
                .any(|report| report.kind == ReportKind::UnsupportedValue
                    && report.subject == "color")
        );
        assert!(reports
            .iter()
            .any(|report| report.kind == ReportKind::UnsupportedSelector
                && report.subject.contains("aria")));
        assert!(reports
            .iter()
            .any(|report| report.subject == "@media" && report.line > 0));
        // The supported declaration in the same block still parsed.
        let x = classes(&["x"]);
        let mut sheet = sheet;
        let styled = resolve(&mut sheet, node("label", &x));
        assert_eq!(styled.background, Some(Color::WHITE));
    }

    #[test]
    fn the_cache_returns_the_same_style_for_the_same_context() {
        let mut sheet = Stylesheet::parse(".a { color: white; }");
        let a = classes(&["a"]);
        let chain = [node("label", &a)];
        let key = key_for(&chain);
        let first = sheet.resolve(&chain, key, &ComputedStyle::default());
        let second = sheet.resolve(&chain, key, &ComputedStyle::default());
        assert_eq!(first, second);
    }

    #[test]
    fn font_shorthand_and_outline_parse() {
        let mut sheet = Stylesheet::parse(
            ".tab { font: 600 12px Inter, sans-serif; outline: 2px solid rgba(74, 144, 226, 1); outline-offset: -2px; }",
        );
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());
        let tab = classes(&["tab"]);
        let styled = resolve(&mut sheet, node("row", &tab));
        assert_eq!(styled.font_size, Some(12.0));
        assert_eq!(styled.font_weight, Some(600));
        assert_eq!(styled.font_family.as_deref(), Some("Inter"));
        assert_eq!(
            styled.outline,
            Some((
                2.0,
                Color::rgba(74.0 / 255.0, 144.0 / 255.0, 226.0 / 255.0, 1.0)
            ))
        );
        assert_eq!(styled.outline_offset, Some(-2.0));
    }

    #[test]
    fn the_default_sheet_restates_the_theme_without_reports() {
        let mut sheet = Stylesheet::parse(include_str!("../../assets/default.css"));
        assert!(sheet.reports().is_empty(), "{:?}", sheet.reports());
        assert!(sheet.rule_count() >= 6, "the sheet's rules parsed");

        let no_classes = classes(&[]);
        let styled = resolve(&mut sheet, node("label", &no_classes));
        let theme = crate::theme::Theme::default();
        assert_eq!(
            styled.color,
            Some(theme.text),
            "the sheet's label colour is the theme's exact value"
        );
        assert_eq!(styled.font_size, Some(theme.font_size));

        let disabled = resolve(
            &mut sheet,
            NodeFacts {
                disabled: true,
                ..node("button", &no_classes)
            },
        );
        assert_eq!(disabled.color, Some(theme.text_disabled));
        assert_eq!(disabled.background, Some(theme.inset));
    }

    #[test]
    fn the_editor_dock_port_loads_without_reports() {
        let sheet = Stylesheet::parse(include_str!("../../assets/editor_dock.css"));
        assert!(sheet.reports().is_empty(), "{:#?}", sheet.reports());
        assert!(
            sheet.rule_count() >= 20,
            "the port kept the design's rules, got {}",
            sheet.rule_count()
        );
    }
}
