//! `#[engine_error]` — one semantic error definition generating plain and
//! styled messages.
//!
//! # Responsibilities
//!
//! - Parse the `#[engine_error]` attribute and its `#[message(...)]` DSL.
//! - Generate `thiserror::Error` plain `Display` messages from the semantic
//!   message AST — never the other way around.
//! - Generate `miette::Diagnostic` implementations with stable diagnostic
//!   codes derived from the namespace and variant name.
//! - Generate [`EngineMessage`] implementations that render the semantic
//!   message into a [`MessageRenderer`].
//!
//! # Design
//!
//! The DSL supports the following message nodes:
//!
//! - `"plain text"` — ordinary text.
//! - `general_style("Renderer")` / `specific_style(...)` /
//!   `module_style(...)` / `name_style(...)` — static semantic tokens.
//! - `general_style(field)` etc. — dynamic values rendered with a role.
//! - `value(field)` — plain dynamic value.
//! - `debug_value(field)` — debug representation of a dynamic value.
//!
//! `#[transparent]` variants delegate both plain and semantic rendering to
//! the wrapped error and are the only supported composition mechanism.
//! Diagnostic codes default to `namespace::snake_case_variant` and can be
//! overridden with `#[code(other_name)]`.

extern crate proc_macro;

// Standard library
use std::collections::HashSet;

// External crates
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, ToTokens};
use syn::parse::ParseStream;
use syn::punctuated::Punctuated;
use syn::{
    parenthesized, parse_macro_input, Attribute, Fields, Ident, ItemEnum, LitStr, Meta, Path,
    Token, Variant,
};

// =============================================================================
// Macro Arguments
// =============================================================================

/// Arguments accepted by `#[engine_error(...)]`.
struct MacroArguments {
    /// Diagnostic-code namespace, e.g. `host::build`.
    namespace: Path,
    /// Path of the diagnostics runtime module used by generated code.
    runtime: Path,
}

impl syn::parse::Parse for MacroArguments {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut namespace: Option<Path> = None;
        let mut runtime: Option<Path> = None;

        // Step 1: Consume `key = value` pairs until the argument list ends.
        while !input.is_empty() {
            let key: Ident = input.parse()?;
            input.parse::<Token![=]>()?;
            match key.to_string().as_str() {
                "namespace" => namespace = Some(Path::parse_mod_style(input)?),
                "runtime" => runtime = Some(Path::parse_mod_style(input)?),
                other => {
                    return Err(syn::Error::new(
                        key.span(),
                        format!("unknown `engine_error` argument `{other}`"),
                    ));
                }
            }
            if input.peek(Token![,]) {
                input.parse::<Token![,]>()?;
            }
        }

        // Step 2: `namespace` is mandatory; `runtime` falls back to the
        // crate-local diagnostics module.
        let namespace = namespace
            .ok_or_else(|| input.error("missing `namespace = ...` argument for `engine_error`"))?;
        let runtime = runtime.unwrap_or_else(|| syn::parse_quote!(crate::error));
        Ok(Self { namespace, runtime })
    }
}

// =============================================================================
// Message AST
// =============================================================================

/// Semantic role of one message token.
///
/// Maps one-to-one onto the `SemanticRole` enum of the diagnostics runtime so
/// generated code can address the same variants the runtime renders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SemanticRole {
    /// Token introduced by `general_style`.
    General,
    /// Token introduced by `specific_style`.
    Specific,
    /// Token introduced by `module_style`.
    Module,
    /// Token introduced by `name_style`.
    Name,
}

impl SemanticRole {
    /// Role path segment used by the diagnostics runtime.
    fn variant_ident(self) -> &'static str {
        match self {
            SemanticRole::General => "General",
            SemanticRole::Specific => "Specific",
            SemanticRole::Module => "Module",
            SemanticRole::Name => "Name",
        }
    }
}

/// Display format of a dynamic message value.
///
/// Picks the formatting trait used both by the generated plain-text `Display`
/// string and by the semantic renderer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueFormat {
    /// Render the value with `Display`.
    Display,
    /// Render the value with `Debug` (`{:?}`).
    Debug,
}

/// One parsed node of a `#[message(...)]` definition.
enum MessageNode {
    /// `"plain text"`
    Text(LitStr),
    /// `general_style("Renderer")` and friends with a static token.
    StaticStyled { role: SemanticRole, text: LitStr },
    /// `name_style(field)`, `value(field)`, or `debug_value(field)`.
    FieldStyled {
        role: Option<SemanticRole>,
        field: Ident,
        format: ValueFormat,
    },
}

/// Argument of a style DSL function: either a static string or a field name.
///
/// Static arguments produce constant semantic tokens; field arguments produce
/// dynamic values rendered with the surrounding role.
enum StyleArgument {
    /// A string literal, e.g. `general_style("Renderer")`.
    Static(LitStr),
    /// A field name, e.g. `name_style(variant_name)`.
    Field(Ident),
}

impl syn::parse::Parse for MessageNode {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        // Step 1: A bare string literal is a plain-text node.
        if input.peek(LitStr) {
            return Ok(MessageNode::Text(input.parse()?));
        }

        // Step 2: Otherwise this is a DSL call `function(argument)` whose
        // single argument is either a static string or a field name.
        let function: Ident = input.parse()?;
        let content;
        parenthesized!(content in input);
        let argument = if content.peek(LitStr) {
            StyleArgument::Static(content.parse()?)
        } else if content.peek(Ident) {
            StyleArgument::Field(content.parse()?)
        } else {
            return Err(content.error("expected a string literal or a field name"));
        };

        // Step 3: Dispatch on the function name, suggesting typos.
        match function.to_string().as_str() {
            "general_style" => Ok(styled_node(SemanticRole::General, argument)),
            "specific_style" => Ok(styled_node(SemanticRole::Specific, argument)),
            "module_style" => Ok(styled_node(SemanticRole::Module, argument)),
            "name_style" => Ok(styled_node(SemanticRole::Name, argument)),
            "value" => Ok(plain_node(argument, ValueFormat::Display)),
            "debug_value" => Ok(plain_node(argument, ValueFormat::Debug)),
            other => {
                let hint = did_you_mean(other).map_or_else(String::new, |candidate| {
                    format!("; help: did you mean `{candidate}`?")
                });
                Err(syn::Error::new(
                    function.span(),
                    format!("unknown message function `{other}`{hint}"),
                ))
            }
        }
    }
}

/// Build one static- or field-styled semantic node.
fn styled_node(role: SemanticRole, argument: StyleArgument) -> MessageNode {
    match argument {
        StyleArgument::Static(text) => MessageNode::StaticStyled { role, text },
        StyleArgument::Field(field) => MessageNode::FieldStyled {
            role: Some(role),
            field,
            format: ValueFormat::Display,
        },
    }
}

/// Build one value node carrying no semantic role.
fn plain_node(argument: StyleArgument, format: ValueFormat) -> MessageNode {
    match argument {
        StyleArgument::Static(text) => MessageNode::Text(text),
        StyleArgument::Field(field) => MessageNode::FieldStyled {
            role: None,
            field,
            format,
        },
    }
}

/// Closest DSL function name to `given`, when it is plausibly a typo.
fn did_you_mean(given: &str) -> Option<&'static str> {
    const CANDIDATES: [&str; 6] = [
        "general_style",
        "specific_style",
        "module_style",
        "name_style",
        "value",
        "debug_value",
    ];
    CANDIDATES
        .into_iter()
        .filter(|candidate| edit_distance(given, candidate) <= 2)
        .min_by_key(|candidate| edit_distance(given, candidate))
}

/// Classic Levenshtein distance over ASCII bytes.
fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0_usize; right.len() + 1];
    for (left_index, left_char) in left.chars().enumerate() {
        current[0] = left_index + 1;
        for (right_index, right_char) in right.iter().enumerate() {
            current[right_index + 1] = (previous[right_index + 1] + 1)
                .min(current[right_index] + 1)
                .min(previous[right_index] + usize::from(left_char != *right_char));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[right.len()]
}

// =============================================================================
// Expansion
// =============================================================================

/// Everything the macro extracted from one variant.
struct VariantInfo {
    /// Attributes passed through to the generated enum.
    attributes: Vec<Attribute>,
    /// Parsed `#[message(...)]` nodes, when present.
    message: Option<Vec<MessageNode>>,
    /// Whether `#[transparent]` was present.
    transparent: bool,
    /// `#[code(...)]` override for the generated diagnostic code.
    code_override: Option<Ident>,
    /// User-supplied `#[diagnostic(...)]` attribute, kept to merge the code.
    user_diagnostic: Option<Attribute>,
    /// Whether the user's diagnostic attribute already declares a code.
    user_declares_code: bool,
}

/// Split one variant's attributes into consumed helpers and pass-throughs.
fn classify_variant(variant: &Variant) -> syn::Result<VariantInfo> {
    let mut attributes = Vec::new();
    let mut message: Option<Vec<MessageNode>> = None;
    let mut transparent = false;
    let mut code_override = None;
    let mut user_diagnostic = None;
    let mut user_declares_code = false;

    for attribute in &variant.attrs {
        if attribute.path().is_ident("message") {
            if message.is_some() {
                return Err(syn::Error::new_spanned(
                    attribute,
                    "duplicate `#[message(...)]` attribute",
                ));
            }
            message = Some(
                attribute
                    .parse_args_with(Punctuated::<MessageNode, Token![,]>::parse_terminated)?
                    .into_iter()
                    .collect(),
            );
        } else if attribute.path().is_ident("transparent") {
            transparent = true;
        } else if attribute.path().is_ident("code") {
            code_override = Some(attribute.parse_args()?);
        } else if attribute.path().is_ident("diagnostic") {
            // Parse the user's diagnostic arguments to detect an explicit
            // `code(...)`; the generated code argument is merged into the
            // same attribute so the derive sees exactly one `#[diagnostic]`.
            if let Ok(arguments) =
                attribute.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            {
                user_declares_code = arguments
                    .iter()
                    .any(|argument| argument.path().is_ident("code"));
            }
            user_diagnostic = Some(attribute.clone());
        } else {
            attributes.push(attribute.clone());
        }
    }

    Ok(VariantInfo {
        attributes,
        message,
        transparent,
        code_override,
        user_diagnostic,
        user_declares_code,
    })
}

/// Plain `#[error(...)]` format string generated from the semantic AST.
fn build_format_string(nodes: &[MessageNode]) -> String {
    let mut format = String::new();
    for node in nodes {
        match node {
            MessageNode::Text(text) | MessageNode::StaticStyled { text, .. } => {
                format.push_str(&text.value());
            }
            MessageNode::FieldStyled {
                field,
                format: ValueFormat::Display,
                ..
            } => {
                format.push('{');
                format.push_str(&field.to_string());
                format.push('}');
            }
            MessageNode::FieldStyled {
                field,
                format: ValueFormat::Debug,
                ..
            } => {
                format.push('{');
                format.push_str(&field.to_string());
                format.push_str(":?}");
            }
        }
    }
    format
}

/// Convert a variant name to snake case for the diagnostic code suffix.
fn to_snake_case(ident: &Ident) -> String {
    let name = ident.to_string();
    let mut output = String::with_capacity(name.len() + 4);
    let mut previous_uppercase = false;
    for character in name.chars() {
        if character.is_ascii_uppercase() {
            if !output.is_empty() && !previous_uppercase {
                output.push('_');
            }
            output.push(character.to_ascii_lowercase());
            previous_uppercase = true;
        } else {
            output.push(character);
            previous_uppercase = false;
        }
    }
    output
}

/// The dot-free diagnostic-code namespace of an `engine_error` enum.
fn namespace_string(path: &Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Validate field references and return the referenced names in first-use order.
fn referenced_fields(variant: &Variant, nodes: &[MessageNode]) -> syn::Result<Vec<Ident>> {
    let named_fields: HashSet<String> = match &variant.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .filter_map(|field| field.ident.as_ref())
            .map(|ident| ident.to_string())
            .collect(),
        _ => HashSet::new(),
    };

    let mut referenced: Vec<Ident> = Vec::new();
    for node in nodes {
        let MessageNode::FieldStyled { field, .. } = node else {
            continue;
        };
        if !named_fields.contains(&field.to_string()) {
            if matches!(variant.fields, Fields::Unnamed(_)) {
                return Err(syn::Error::new(
                    field.span(),
                    "styled field references require named fields; give the variant named fields",
                ));
            }
            return Err(syn::Error::new(
                field.span(),
                format!(
                    "`{field}` references an unknown field on variant `{}`",
                    variant.ident
                ),
            ));
        }
        if !referenced.iter().any(|existing| existing == field) {
            referenced.push(field.clone());
        }
    }
    Ok(referenced)
}

/// Generate one `EngineMessage` match arm for a leaf message variant.
fn build_message_arm(
    runtime: &Path,
    variant: &Variant,
    nodes: &[MessageNode],
    referenced: &[Ident],
) -> TokenStream2 {
    let ident = &variant.ident;
    let pattern = match &variant.fields {
        Fields::Unit => quote!(),
        Fields::Named(_) => {
            if referenced.is_empty() {
                quote!({ .. })
            } else {
                quote!({ #(#referenced,)* .. })
            }
        }
        Fields::Unnamed(_) => quote!((..)),
    };

    let steps = message_steps(runtime, nodes);
    let cfg_attributes = cfg_attributes(variant);

    quote! {
        #(#cfg_attributes)* Self::#ident #pattern => { #(#steps;)* Ok(()) },
    }
}

/// The renderer calls for one leaf variant's semantic message.
fn message_steps(runtime: &Path, nodes: &[MessageNode]) -> Vec<TokenStream2> {
    nodes
        .iter()
        .map(|node| {
            let step = match node {
                MessageNode::Text(text) => quote!(renderer.text(#text)),
                MessageNode::StaticStyled { role, text } => {
                    let role_ident =
                        Ident::new(role.variant_ident(), proc_macro2::Span::call_site());
                    quote!(renderer.styled(#runtime::SemanticRole::#role_ident, &#text))
                }
                MessageNode::FieldStyled {
                    role: Some(role),
                    field,
                    ..
                } => {
                    let role_ident =
                        Ident::new(role.variant_ident(), proc_macro2::Span::call_site());
                    quote!(renderer.styled(#runtime::SemanticRole::#role_ident, #field))
                }
                MessageNode::FieldStyled {
                    role: None,
                    field,
                    format: ValueFormat::Display,
                } => quote!(renderer.value(#field)),
                MessageNode::FieldStyled {
                    role: None,
                    field,
                    format: ValueFormat::Debug,
                } => quote!(renderer.debug_value(#field)),
            };
            quote!(#step?)
        })
        .collect()
}

/// `#[cfg(...)]` attributes carried by one variant, re-emitted on the match
/// arm so feature-gated variants vanish together with their arms.
fn cfg_attributes(variant: &Variant) -> Vec<&Attribute> {
    variant
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg"))
        .collect()
}

/// Generate one `EngineMessage` match arm for a transparent variant.
fn build_transparent_arm(runtime: &Path, variant: &Variant) -> syn::Result<TokenStream2> {
    let ident = &variant.ident;
    let cfg_attributes = cfg_attributes(variant);
    match &variant.fields {
        Fields::Unnamed(unnamed) if unnamed.unnamed.len() == 1 => Ok(
            quote!(#(#cfg_attributes)* Self::#ident(source) => #runtime::EngineMessage::render_message(source, renderer),),
        ),
        Fields::Named(named) if named.named.len() == 1 => {
            let field = named
                .named
                .first()
                .and_then(|field| field.ident.as_ref())
                .ok_or_else(|| {
                    syn::Error::new_spanned(named, "transparent variant field needs a name")
                })?;
            Ok(
                quote!(#(#cfg_attributes)* Self::#ident { #field, .. } => #runtime::EngineMessage::render_message(#field, renderer),),
            )
        }
        _ => Err(syn::Error::new_spanned(
            variant,
            "`#[transparent]` requires exactly one wrapped error field",
        )),
    }
}

/// Derive traits already present on the enum.
fn existing_derives(item: &ItemEnum) -> HashSet<String> {
    let mut found = HashSet::new();
    for attribute in item
        .attrs
        .iter()
        .filter(|attribute| attribute.path().is_ident("derive"))
    {
        let Ok(paths) = attribute.parse_args_with(Punctuated::<Path, Token![,]>::parse_terminated)
        else {
            continue;
        };
        for path in paths {
            if let Some(segment) = path.segments.last() {
                found.insert(segment.ident.to_string());
            }
        }
    }
    found
}

// =============================================================================
// Entry Point
// =============================================================================

/// Generates `thiserror::Error` display output, `miette::Diagnostic` codes,
/// and an `EngineMessage` implementation from one `#[message(...)]`
/// definition per variant.
///
/// # Errors
///
/// Emits a compile error when:
///
/// - `namespace` is missing or an argument name is unknown.
/// - A variant has neither `#[message(...)]` nor `#[transparent]`.
/// - A variant mixes `#[message(...)]` with `#[transparent]`.
/// - A `#[message(...)]` is empty or references an unknown field.
/// - A `#[transparent]` variant does not wrap exactly one error field.
///
/// # Examples
///
/// ```ignore
/// use pill_core::error::EngineMessage;
///
/// #[engine_error(namespace = "host::build", runtime = "pill_core::error")]
/// pub enum BuildError {
///     #[message("failed to link {general_style(crate_name)}")]
///     LinkFailed { crate_name: String },
///
///     #[transparent]
///     Io(std::io::Error),
/// }
/// ```
///
/// The example is marked `ignore` because proc-macro expansion cannot run
/// inside a doc-test of the macro crate itself.
#[proc_macro_attribute]
pub fn engine_error(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let arguments = parse_macro_input!(attribute as MacroArguments);
    let mut item = parse_macro_input!(item as ItemEnum);
    expand(arguments, &mut item)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Core expansion behind the `engine_error` attribute entry point.
///
/// Rewrites each variant (stripping consumed helper attributes and appending
/// the generated `#[error(...)]` / `#[diagnostic(...)]` attributes), validates
/// the message DSL, and emits the enum plus its `EngineMessage` implementation.
fn expand(arguments: MacroArguments, item: &mut ItemEnum) -> syn::Result<TokenStream2> {
    let namespace = namespace_string(&arguments.namespace);

    // Step 1: Transform every variant and collect semantic match arms.
    let mut variants = Vec::new();
    let mut arms: Vec<TokenStream2> = Vec::new();
    for variant in &item.variants {
        let info = classify_variant(variant)?;
        if info.message.is_none() && !info.transparent {
            return Err(syn::Error::new_spanned(
                variant,
                "error variant must have either `#[message(...)]` or `#[transparent]`",
            ));
        }
        if info.message.is_some() && info.transparent {
            return Err(syn::Error::new_spanned(
                variant,
                "error variant cannot be both `#[message(...)]` and `#[transparent]`",
            ));
        }

        let mut generated = variant.clone();
        generated.attrs = info.attributes;

        if info.transparent {
            generated
                .attrs
                .push(syn::parse_quote!(#[error(transparent)]));
            generated
                .attrs
                .push(syn::parse_quote!(#[diagnostic(transparent)]));
            arms.push(build_transparent_arm(&arguments.runtime, variant)?);
        } else {
            let nodes = info.message.expect("validated above");
            if nodes.is_empty() {
                return Err(syn::Error::new_spanned(
                    variant,
                    "`#[message(...)]` must not be empty",
                ));
            }
            let referenced = referenced_fields(variant, &nodes)?;
            let format_string = build_format_string(&nodes);
            generated
                .attrs
                .push(syn::parse_quote!(#[error(#format_string)]));

            // Diagnostic code: namespace + snake_case variant, overridable
            // through #[code(...)] or an explicit user code argument.
            if !info.user_declares_code {
                let suffix = info
                    .code_override
                    .as_ref()
                    .map_or_else(|| to_snake_case(&variant.ident), |ident| ident.to_string());
                let code = format!("{namespace}::{suffix}");
                match &info.user_diagnostic {
                    Some(attribute) => {
                        // Merge the generated code into the user's diagnostic
                        // arguments so the derive sees a single attribute.
                        if let Meta::List(list) = &attribute.meta {
                            let inner = &list.tokens;
                            generated.attrs.push(syn::parse_quote!(
                                #[diagnostic(code(#code), #inner)]
                            ));
                        } else {
                            generated
                                .attrs
                                .push(syn::parse_quote!(#[diagnostic(code(#code))]));
                            generated.attrs.push(attribute.clone());
                        }
                    }
                    None => {
                        generated
                            .attrs
                            .push(syn::parse_quote!(#[diagnostic(code(#code))]));
                    }
                }
            } else if let Some(attribute) = &info.user_diagnostic {
                generated.attrs.push(attribute.clone());
            }

            arms.push(build_message_arm(
                &arguments.runtime,
                variant,
                &nodes,
                &referenced,
            ));
        }

        variants.push(generated);
    }
    item.variants = Punctuated::from_iter(variants);

    // Step 2: Ensure the required derives are present without duplicating
    // derives the author already declared.
    let derives = existing_derives(item);
    if !derives.contains("Error") {
        item.attrs
            .push(syn::parse_quote!(#[derive(::thiserror::Error)]));
    }
    if !derives.contains("Diagnostic") {
        item.attrs
            .push(syn::parse_quote!(#[derive(::miette::Diagnostic)]));
    }
    if !derives.contains("Debug") {
        item.attrs
            .push(syn::parse_quote!(#[derive(::core::fmt::Debug)]));
    }

    // Step 3: Emit the enum plus its semantic rendering implementation.
    let enum_ident = &item.ident;
    let (impl_generics, type_generics, where_clause) = item.generics.split_for_impl();
    let runtime = &arguments.runtime;

    let output = quote! {
        #item

        impl #impl_generics #runtime::EngineMessage for #enum_ident #type_generics #where_clause {
            fn render_message(
                &self,
                renderer: &mut dyn #runtime::MessageRenderer,
            ) -> ::std::fmt::Result {
                match self {
                    #(#arms)*
                }
            }
        }
    };

    // Debug aid: dump the expansion when the env var is set.
    if std::env::var_os("ENGINE_ERROR_MACRO_DEBUG").is_some() {
        if let Ok(mut dump) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open("engine_error_macro_debug.rs")
        {
            use std::io::Write as _;
            let _ = writeln!(
                dump,
                "\n// ==== {} ====\n{}",
                enum_ident,
                output.to_token_stream()
            );
        }
    }

    Ok(output.into_token_stream())
}
