//! Selectors of the CSS subset: parsing, matching and specificity.
//!
//! # Responsibilities
//!
//! - [`Selector`]: type, `.class`, `#id`, `:pseudo`, descendant and child
//!   combinators, and comma lists of those.
//! - [`matches`]: whether a selector applies to a node, given the node's
//!   facts and its ancestors' - the chain is what makes `.toolbar button`
//!   work - and with what [`Specificity`].
//!
//! # Design
//!
//! The subset refuses what it cannot honour: attribute selectors and
//! functional pseudo-classes (`:not(...)`) are parse errors the caller
//! reports with position, never silently ignored. `:root` parses (it is the
//! variable scope) but matches no widget. Matching runs right to left, the
//! way real engines do, so ancestor checks only run for selectors whose
//! subject already matched.

// Standard library
use std::fmt;

// =============================================================================
// Types
// =============================================================================

/// A pseudo-class the subset understands.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PseudoClass {
    /// The pointer is over the node.
    Hover,
    /// The pointer is holding the node down.
    Active,
    /// The node holds the keyboard focus.
    FocusVisible,
    /// The node's widget is disabled.
    Disabled,
    /// The variable scope; matches no widget.
    Root,
}

/// How a compound relates to the one before the subject.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Combinator {
    /// `a b`: `b` anywhere under `a`.
    Descendant,
    /// `a > b`: `b` a direct child of `a`.
    Child,
}

/// One compound selector: `button.primary:hover`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Compound {
    /// The type name, or `None` for `*` and unspecified.
    pub type_name: Option<String>,
    /// Whether the compound wrote `*`.
    pub universal: bool,
    /// The classes required, in written order.
    pub classes: Vec<String>,
    /// The id required, if any.
    pub id: Option<String>,
    /// The pseudo-class required, if any.
    pub pseudo: Option<PseudoClass>,
}

/// A full selector: one or more compounds joined by combinators.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Selector {
    /// The compounds, ancestors first; the last is the subject.
    pub compounds: Vec<Compound>,
    /// How each compound relates to the next; `relations.len()` is
    /// `compounds.len() - 1`.
    pub relations: Vec<Combinator>,
}

/// CSS specificity, ordered by field precedence.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Specificity {
    /// The number of ids.
    pub ids: u32,
    /// The number of classes and pseudo-classes.
    pub classes: u32,
    /// The number of type names.
    pub types: u32,
}

impl Specificity {
    /// Add one compound's contribution.
    fn add(&mut self, compound: &Compound) {
        if compound.id.is_some() {
            self.ids += 1;
        }
        self.classes += compound.classes.len() as u32 + compound.pseudo.map_or(0, |_| 1);
        if compound.type_name.is_some() {
            self.types += 1;
        }
    }
}

/// What a node is, for matching: its type, classes, id name and state.
#[derive(Clone, Debug)]
pub struct NodeFacts<'a> {
    /// The node's type name (`button`, `row`, ...).
    pub type_name: &'a str,
    /// The node's classes.
    pub classes: &'a [String],
    /// The node's explicit name, matched by `#id`.
    pub id_name: Option<&'a str>,
    /// The pointer is over the node.
    pub hovered: bool,
    /// The pointer is holding the node down.
    pub active: bool,
    /// The node holds the focus.
    pub focused: bool,
    /// The node's widget is disabled.
    pub disabled: bool,
}

// =============================================================================
// Parsing
// =============================================================================

/// Parse a selector list; the error names what the subset refuses.
pub fn parse_list(text: &str) -> Result<Vec<Selector>, String> {
    let mut selectors = Vec::new();
    for part in text.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        selectors.push(parse_selector(part)?);
    }
    if selectors.is_empty() {
        return Err(String::from("empty selector"));
    }
    Ok(selectors)
}

fn parse_selector(text: &str) -> Result<Selector, String> {
    if text.contains('[') {
        return Err(String::from(
            "attribute selectors are not part of the CSS subset",
        ));
    }
    let characters: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    let mut compounds = Vec::new();
    let mut relations = Vec::new();

    loop {
        let compound = parse_compound(&characters, &mut index)?;
        compounds.push(compound);
        let spaced = skip_whitespace(&characters, &mut index);
        if index >= characters.len() {
            break;
        }
        if characters[index] == '>' {
            index += 1;
            skip_whitespace(&characters, &mut index);
            relations.push(Combinator::Child);
        } else if spaced {
            relations.push(Combinator::Descendant);
        } else {
            return Err(format!(
                "unexpected `{}` in selector `{text}`",
                characters[index]
            ));
        }
    }
    Ok(Selector {
        compounds,
        relations,
    })
}

/// Parse one compound; errors when nothing was read.
fn parse_compound(characters: &[char], index: &mut usize) -> Result<Compound, String> {
    let mut compound = Compound::default();
    if *index < characters.len() && characters[*index] == '*' {
        compound.universal = true;
        *index += 1;
    } else if let Some(name) = read_ident(characters, index) {
        compound.type_name = Some(name);
    }

    loop {
        if *index >= characters.len() {
            break;
        }
        match characters[*index] {
            '.' => {
                *index += 1;
                let name = read_ident(characters, index)
                    .ok_or_else(|| String::from("class selector with no name"))?;
                compound.classes.push(name);
            }
            '#' => {
                *index += 1;
                let name = read_ident(characters, index)
                    .ok_or_else(|| String::from("id selector with no name"))?;
                compound.id = Some(name);
            }
            ':' => {
                *index += 1;
                if *index < characters.len() && characters[*index] == ':' {
                    return Err(String::from(
                        "pseudo-elements are not part of the CSS subset",
                    ));
                }
                let name = read_ident(characters, index)
                    .ok_or_else(|| String::from("pseudo-class with no name"))?;
                let pseudo = match name.as_str() {
                    "hover" => PseudoClass::Hover,
                    "active" => PseudoClass::Active,
                    "focus-visible" => PseudoClass::FocusVisible,
                    "disabled" => PseudoClass::Disabled,
                    "root" => PseudoClass::Root,
                    other => {
                        return Err(format!(
                            "pseudo-class :{other} is not part of the CSS subset"
                        ))
                    }
                };
                if *index < characters.len() && characters[*index] == '(' {
                    return Err(String::from(
                        "functional pseudo-classes are not part of the CSS subset",
                    ));
                }
                compound.pseudo = Some(pseudo);
            }
            _ => break,
        }
    }

    let empty = compound.type_name.is_none()
        && !compound.universal
        && compound.classes.is_empty()
        && compound.id.is_none()
        && compound.pseudo.is_none();
    if empty {
        return Err(String::from("empty selector compound"));
    }
    Ok(compound)
}

fn skip_whitespace(characters: &[char], index: &mut usize) -> bool {
    let start = *index;
    while *index < characters.len() && characters[*index].is_whitespace() {
        *index += 1;
    }
    *index > start
}

fn read_ident(characters: &[char], index: &mut usize) -> Option<String> {
    let start = *index;
    while *index < characters.len() {
        let character = characters[*index];
        if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
            *index += 1;
        } else {
            break;
        }
    }
    (*index > start).then(|| characters[start..*index].iter().collect::<String>())
}

// =============================================================================
// Matching
// =============================================================================

/// Whether `selector` applies to the last node of `chain` (ancestors first),
/// and with what specificity.
pub fn matches(selector: &Selector, chain: &[NodeFacts<'_>]) -> Option<Specificity> {
    let subject = selector.compounds.last()?;
    let facts = chain.last()?;
    if !compound_matches(subject, facts) {
        return None;
    }
    let mut specificity = Specificity::default();
    specificity.add(subject);

    let mut ancestor = chain.len().saturating_sub(1);
    for position in (0..selector.compounds.len() - 1).rev() {
        let compound = &selector.compounds[position];
        let relation = selector.relations[position];
        match relation {
            Combinator::Child => {
                if ancestor == 0 {
                    return None;
                }
                ancestor -= 1;
                if !compound_matches(compound, &chain[ancestor]) {
                    return None;
                }
            }
            Combinator::Descendant => loop {
                if ancestor == 0 {
                    return None;
                }
                ancestor -= 1;
                if compound_matches(compound, &chain[ancestor]) {
                    break;
                }
            },
        }
        specificity.add(compound);
    }
    Some(specificity)
}

fn compound_matches(compound: &Compound, facts: &NodeFacts<'_>) -> bool {
    if let Some(type_name) = &compound.type_name {
        if !facts.type_name.eq_ignore_ascii_case(type_name) {
            return false;
        }
    }
    if let Some(id) = &compound.id {
        if facts.id_name != Some(id.as_str()) {
            return false;
        }
    }
    if !compound
        .classes
        .iter()
        .all(|class| facts.classes.iter().any(|held| held == class))
    {
        return false;
    }
    match compound.pseudo {
        None => true,
        // `:root` is the variable scope, not a widget.
        Some(PseudoClass::Root) => false,
        Some(PseudoClass::Hover) => facts.hovered,
        Some(PseudoClass::Active) => facts.active,
        Some(PseudoClass::FocusVisible) => facts.focused,
        Some(PseudoClass::Disabled) => facts.disabled,
    }
}

impl fmt::Display for Selector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, compound) in self.compounds.iter().enumerate() {
            if index > 0 {
                match self.relations[index - 1] {
                    Combinator::Descendant => write!(formatter, " ")?,
                    Combinator::Child => write!(formatter, " > ")?,
                }
            }
            if compound.universal {
                write!(formatter, "*")?;
            }
            if let Some(type_name) = &compound.type_name {
                write!(formatter, "{type_name}")?;
            }
            for class in &compound.classes {
                write!(formatter, ".{class}")?;
            }
            if let Some(id) = &compound.id {
                write!(formatter, "#{id}")?;
            }
            if let Some(pseudo) = compound.pseudo {
                let name = match pseudo {
                    PseudoClass::Hover => "hover",
                    PseudoClass::Active => "active",
                    PseudoClass::FocusVisible => "focus-visible",
                    PseudoClass::Disabled => "disabled",
                    PseudoClass::Root => "root",
                };
                write!(formatter, ":{name}")?;
            }
        }
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(type_name: &'a str, classes: &'a [String]) -> NodeFacts<'a> {
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

    fn classes(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn selectors_parse_with_combinators_and_pseudos() {
        let selector = &parse_list(".dock-tab:hover").unwrap()[0];
        assert_eq!(selector.compounds.len(), 1);
        assert_eq!(selector.compounds[0].classes, vec!["dock-tab"]);
        assert_eq!(selector.compounds[0].pseudo, Some(PseudoClass::Hover));

        let nested = &parse_list(".toolbar > button.primary, .x y").unwrap()[1];
        assert_eq!(nested.compounds.len(), 2);
        assert_eq!(nested.relations, vec![Combinator::Descendant]);
        let child = &parse_list(".toolbar > button.primary").unwrap()[0];
        assert_eq!(child.relations, vec![Combinator::Child]);
        assert_eq!(child.compounds[1].type_name.as_deref(), Some("button"));
        assert_eq!(child.compounds[1].classes, vec!["primary"]);
    }

    #[test]
    fn unsupported_selectors_are_named() {
        assert!(parse_list("[aria-selected=\"true\"]").is_err());
        assert!(parse_list(".x:not(.y)").is_err());
        assert!(parse_list(".x::before").is_err());
        assert!(parse_list("").is_err());
    }

    #[test]
    fn specificity_ranks_ids_over_classes_over_types() {
        let id = parse_list("#hero").unwrap().remove(0);
        let class = parse_list(".hero").unwrap().remove(0);
        let two_types = parse_list("column button").unwrap().remove(0);
        let no_classes = classes(&[]);
        let hero_classes = classes(&["hero"]);
        let hero = NodeFacts {
            id_name: Some("hero"),
            ..facts("button", &hero_classes)
        };
        let chain = [facts("column", &no_classes), hero];
        assert!(matches(&id, &chain).unwrap() > matches(&class, &chain).unwrap());
        assert!(matches(&class, &chain).unwrap() > matches(&two_types, &chain).unwrap());
    }

    #[test]
    fn descendant_and_child_differ() {
        let child_selectors = parse_list(".toolbar > button").unwrap();
        let descendant_selectors = parse_list(".toolbar button").unwrap();
        let child = &child_selectors[0];
        let descendant = &descendant_selectors[0];
        let toolbar = classes(&["toolbar"]);
        let none = classes(&[]);
        let deep_chain = [
            facts("column", &toolbar),
            facts("row", &none),
            facts("button", &none),
        ];
        let direct_chain = [facts("column", &toolbar), facts("button", &none)];
        assert!(matches(descendant, &deep_chain).is_some());
        assert!(matches(child, &deep_chain).is_none());
        assert!(matches(child, &direct_chain).is_some());
    }

    #[test]
    fn pseudos_match_the_node_state() {
        let hover_selectors = parse_list(".tab:hover").unwrap();
        let hover = &hover_selectors[0];
        let tab = classes(&["tab"]);
        let idle = [facts("row", &tab)];
        let hovered = [NodeFacts {
            hovered: true,
            ..facts("row", &tab)
        }];
        assert!(matches(hover, &idle).is_none());
        assert!(matches(hover, &hovered).is_some());
    }
}
