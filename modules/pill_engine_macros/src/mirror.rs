//! The mirror attributes: Rust functions and types made callable from C#.
//!
//! # Responsibilities
//!
//! - Lower one Rust signature to the single trampoline shape
//!   `pill_engine::mirror` defines - `(args, ret) -> status` - reading each
//!   argument from its 16-byte slot and writing the result to `ret`.
//! - Tag every argument and the result for the C# codegen, from the closed
//!   vocabulary documented on [`lower_parameter`] and [`lower_return`].
//! - Emit what a mirrored type needs: the drop trampoline and asset store
//!   operations of an object, the encodings of an enum or a value type, and
//!   the shared name of a resource.
//!
//! # Design
//!
//! The lowering is syntactic, as every proc macro's must be: a parameter is
//! classified by how it is written. Shapes with one meaning - `&str`,
//! `impl Into<String>`, `Vec<T>`, `Handle<T>`, a tuple of primitives - are
//! read here directly. A named type taken by value is read through
//! `MirrorValue`, which the attribute declaring that type implemented, so a
//! type nobody mirrored is a compile error at the trampoline rather than a
//! wrong read at run time. A named type taken by reference crosses as the
//! address of the value, whichever kind of type it is - an object's box, a
//! component row, a resource - which is why the codegen, not this macro,
//! decides what the C# parameter looks like.

// External crates
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, ToTokens};
use syn::spanned::Spanned;

// =============================================================================
// Vocabulary
// =============================================================================

/// The primitive type names the vocabulary passes by value, with their width.
const PRIMITIVES: &[(&str, usize)] = &[
    ("u8", 1),
    ("u16", 2),
    ("u32", 4),
    ("u64", 8),
    ("i8", 1),
    ("i16", 2),
    ("i32", 4),
    ("i64", 8),
    ("f32", 4),
    ("f64", 8),
    ("bool", 1),
    ("usize", 8),
    ("isize", 8),
];

/// The width of a primitive type name, or `None` when it is not one.
fn primitive_width(name: &str) -> Option<usize> {
    PRIMITIVES
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, width)| *width)
}

/// The type every mirrored function and type is owned by, as the trampoline
/// sees it.
#[derive(Clone)]
pub(crate) struct Owner {
    /// The type's identifier; `Self` in a signature resolves to it.
    pub(crate) ident: syn::Ident,
}

/// The last segment of a path type, when the type is one.
fn last_segment(ty: &syn::Type) -> Option<&syn::PathSegment> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => path.path.segments.last(),
        syn::Type::Group(group) => last_segment(&group.elem),
        syn::Type::Paren(paren) => last_segment(&paren.elem),
        _ => None,
    }
}

/// The `index`-th generic type argument of a path segment.
fn generic_argument(segment: &syn::PathSegment, index: usize) -> Option<&syn::Type> {
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .nth(index)
}

/// The name the codegen resolves a named type by: its last path segment, with
/// `Self` replaced by the owner.
fn type_name(ty: &syn::Type, owner: Option<&Owner>) -> Option<String> {
    let segment = last_segment(ty)?;
    let name = segment.ident.to_string();
    if name == "Self" {
        return owner.map(|owner| owner.ident.to_string());
    }
    Some(name)
}

/// `ty` with `Self` replaced by the owner, so it can be named outside the
/// `impl` block the trampoline is emitted beside.
fn concrete_type(ty: &syn::Type, owner: Option<&Owner>) -> syn::Result<TokenStream2> {
    match last_segment(ty) {
        Some(segment) if segment.ident == "Self" => match owner {
            Some(owner) => Ok(owner.ident.to_token_stream()),
            None => Err(syn::Error::new_spanned(
                ty,
                "`Self` has no meaning in a free function",
            )),
        },
        _ => Ok(ty.to_token_stream()),
    }
}

/// The primitive name of a type, when it is one.
fn primitive_name(ty: &syn::Type) -> Option<String> {
    let segment = last_segment(ty)?;
    if !segment.arguments.is_empty() {
        return None;
    }
    let name = segment.ident.to_string();
    primitive_width(&name).map(|_| name)
}

/// Whether a type is written as `Name<...>` with the given last segment.
fn is_named(ty: &syn::Type, name: &str) -> bool {
    last_segment(ty).is_some_and(|segment| segment.ident == name)
}

/// Whether a type is `str`.
fn is_str(ty: &syn::Type) -> bool {
    last_segment(ty).is_some_and(|segment| segment.ident == "str" && segment.arguments.is_empty())
}

// =============================================================================
// Parameters
// =============================================================================

/// One parameter, lowered.
pub(crate) struct LoweredParameter {
    /// The codegen's tag for it.
    pub(crate) tag: String,
    /// The statement that reads it into its binding.
    pub(crate) binding: TokenStream2,
    /// The expression the call receives.
    pub(crate) pass: TokenStream2,
}

/// The tag and reader of a named type taken by value.
///
/// `Handle<T>` and `AssetLoader` are named by the vocabulary itself; any
/// other type is read through `MirrorValue`, which the attribute that
/// mirrored it implemented.
fn value_tag(ty: &syn::Type, owner: Option<&Owner>) -> syn::Result<String> {
    if let Some(primitive) = primitive_name(ty) {
        return Ok(primitive);
    }
    let segment = last_segment(ty).ok_or_else(|| unsupported(ty))?;
    match segment.ident.to_string().as_str() {
        "Handle" => {
            let asset = generic_argument(segment, 0).ok_or_else(|| {
                syn::Error::new_spanned(ty, "a `Handle` must name its asset type")
            })?;
            let asset = type_name(asset, owner).ok_or_else(|| unsupported(asset))?;
            Ok(format!("handle:{asset}"))
        }
        "AssetLoader" => Ok("loader".to_string()),
        "String" | "Vec" | "Option" | "Result" | "Box" | "Arc" | "Rc" | "HashMap" | "BTreeMap" => {
            Err(unsupported(ty))
        }
        _ => {
            let name = type_name(ty, owner).ok_or_else(|| unsupported(ty))?;
            if !segment.arguments.is_empty() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "a generic type cannot be mirrored; mirror a concrete type",
                ));
            }
            Ok(format!("val:{name}"))
        }
    }
}

/// The error for a type outside the vocabulary.
fn unsupported(ty: &impl ToTokens) -> syn::Error {
    syn::Error::new_spanned(
        ty,
        format!(
            "`{}` cannot be mirrored to C#; use a primitive, `&str`/`String`/`impl Into<String>`, \
             a slice/`Vec`/`impl IntoIterator` of those, `Handle<T>`, `AssetLoader`, `Option<T>`, \
             a tuple or array of primitives, or a type declared with `#[pill_mirror_object]`, \
             `#[pill_mirror_resource]` or `#[derive(PillMirror)]`",
            ty.to_token_stream()
        ),
    )
}

/// Lower one slice element type: its tag, and the expression reading the
/// whole slot as a `Vec` of it.
fn lower_element(
    element: &syn::Type,
    slot: &TokenStream2,
    owner: Option<&Owner>,
) -> syn::Result<(String, TokenStream2)> {
    if let Some(primitive) = primitive_name(element) {
        let element_type = element.to_token_stream();
        // A `bool` is read through its checked decoder: reading arbitrary
        // bytes as `bool` would be undefined behaviour.
        let read = if primitive == "bool" {
            quote! { ::pill_engine::mirror::read_value_vec::<bool>(#slot)? }
        } else {
            quote! { ::pill_engine::mirror::read_plain_vec::<#element_type>(#slot)? }
        };
        return Ok((primitive, read));
    }
    if is_named(element, "String") {
        return Ok((
            "str".to_string(),
            quote! { ::pill_engine::mirror::read_string_vec(#slot)? },
        ));
    }
    if matches!(element, syn::Type::Reference(_)) {
        return Err(syn::Error::new_spanned(
            element,
            "a slice of references cannot be mirrored; take owned elements (`String`, values)",
        ));
    }
    let tag = value_tag(element, owner)?;
    let element_type = concrete_type(element, owner)?;
    Ok((
        tag,
        quote! { ::pill_engine::mirror::read_value_vec::<#element_type>(#slot)? },
    ))
}

/// The element of an `impl IntoIterator<Item = T>` bound.
fn into_iterator_item(bound: &syn::TraitBound) -> Option<&syn::Type> {
    let segment = bound.path.segments.last()?;
    if segment.ident != "IntoIterator" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return None;
    };
    arguments.args.iter().find_map(|argument| match argument {
        syn::GenericArgument::AssocType(assoc) if assoc.ident == "Item" => Some(&assoc.ty),
        _ => None,
    })
}

/// The field offsets of a tuple of primitives packed with natural alignment,
/// and its total width.
fn tuple_layout(elements: &[&syn::Type]) -> syn::Result<(Vec<usize>, usize)> {
    let mut offsets = Vec::new();
    let mut cursor = 0usize;
    for element in elements {
        let name = primitive_name(element).ok_or_else(|| {
            syn::Error::new_spanned(element, "a mirrored tuple holds primitives only")
        })?;
        let width = primitive_width(&name).expect("a primitive name has a width");
        cursor = cursor.div_ceil(width) * width;
        offsets.push(cursor);
        cursor += width;
    }
    Ok((offsets, cursor))
}

/// The element type and length of a `[T; N]` of primitives.
fn array_shape(array: &syn::TypeArray) -> syn::Result<(String, usize)> {
    let element = primitive_name(&array.elem)
        .ok_or_else(|| syn::Error::new_spanned(array, "a mirrored array holds primitives only"))?;
    let syn::Expr::Lit(syn::ExprLit {
        lit: syn::Lit::Int(length),
        ..
    }) = &array.len
    else {
        return Err(syn::Error::new_spanned(
            &array.len,
            "a mirrored array needs a literal length",
        ));
    };
    let length: usize = length.base10_parse()?;
    if length == 0 || length * primitive_width(&element).expect("a primitive") > 16 {
        return Err(syn::Error::new_spanned(
            array,
            "a mirrored array must fit in 16 bytes",
        ));
    }
    Ok((element, length))
}

/// Lower one parameter read from `slot` into the binding `binding`.
///
/// The vocabulary, as tags:
///
/// | Rust | Tag |
/// | --- | --- |
/// | primitive | its name (`u32`, `bool`, ...) |
/// | `&str`, `String`, `&String`, `&Path`, `PathBuf`, `impl Into<String>`, `impl AsRef<str>` | `str` |
/// | `&[T]`, `Vec<T>`, `impl IntoIterator<Item = T>` | `slice:<element>` |
/// | `Handle<T>`, `&Handle<T>` | `handle:<T>` |
/// | `AssetLoader`, `&AssetLoader` | `loader` |
/// | `Option<T>` | `option:<tag of T>` |
/// | `(A, B, ...)` of primitives | `tuple:A,B,...` |
/// | `[T; N]` of primitives | `array:T:N` |
/// | `&Name`, `&mut Name` | `ref:Name`, `mut:Name` |
/// | `Name` by value | `val:Name` |
pub(crate) fn lower_parameter(
    ty: &syn::Type,
    slot: TokenStream2,
    binding: &syn::Ident,
    display_name: &str,
    owner: Option<&Owner>,
) -> syn::Result<LoweredParameter> {
    let what = format!("the `{display_name}` argument");
    match ty {
        syn::Type::Reference(reference) => {
            let mutable = reference.mutability.is_some();
            let element = &*reference.elem;
            if let syn::Type::Slice(slice) = element {
                if mutable {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "a `&mut [T]` cannot be mirrored; return the result instead",
                    ));
                }
                let (tag, read) = lower_element(&slice.elem, &slot, owner)?;
                return Ok(LoweredParameter {
                    tag: format!("slice:{tag}"),
                    binding: quote! { let #binding = #read; },
                    pass: quote! { &#binding[..] },
                });
            }
            if is_str(element) && !mutable {
                return Ok(LoweredParameter {
                    tag: "str".to_string(),
                    binding: quote! { let #binding = ::pill_engine::mirror::read_str(#slot)?; },
                    pass: quote! { #binding },
                });
            }
            if is_named(element, "String") && !mutable {
                return Ok(LoweredParameter {
                    tag: "str".to_string(),
                    binding: quote! {
                        let #binding = ::std::string::String::from(::pill_engine::mirror::read_str(#slot)?);
                    },
                    pass: quote! { &#binding },
                });
            }
            if is_named(element, "Path") && !mutable {
                return Ok(LoweredParameter {
                    tag: "str".to_string(),
                    binding: quote! {
                        let #binding = ::std::path::Path::new(::pill_engine::mirror::read_str(#slot)?);
                    },
                    pass: quote! { #binding },
                });
            }
            if (is_named(element, "Handle") || is_named(element, "AssetLoader")) && !mutable {
                let tag = value_tag(element, owner)?;
                return Ok(LoweredParameter {
                    tag,
                    binding: quote! {
                        let #binding = <#element as ::pill_engine::mirror::MirrorValue>::read_slot(#slot)?;
                    },
                    pass: quote! { &#binding },
                });
            }
            if primitive_name(element).is_some() {
                return Err(syn::Error::new_spanned(
                    ty,
                    "take a primitive by value to mirror it",
                ));
            }
            let name = type_name(element, owner).ok_or_else(|| unsupported(ty))?;
            let target = concrete_type(element, owner)?;
            let (tag, read) = if mutable {
                (
                    format!("mut:{name}"),
                    quote! { ::pill_engine::mirror::read_mut::<#target>(#slot, #what)? },
                )
            } else {
                (
                    format!("ref:{name}"),
                    quote! { ::pill_engine::mirror::read_ref::<#target>(#slot, #what)? },
                )
            };
            Ok(LoweredParameter {
                tag,
                binding: quote! { let #binding = #read; },
                pass: quote! { #binding },
            })
        }
        syn::Type::ImplTrait(implementation) => {
            for bound in &implementation.bounds {
                let syn::TypeParamBound::Trait(bound) = bound else {
                    continue;
                };
                if let Some(item) = into_iterator_item(bound) {
                    let (tag, read) = lower_element(item, &slot, owner)?;
                    return Ok(LoweredParameter {
                        tag: format!("slice:{tag}"),
                        binding: quote! { let #binding = #read; },
                        pass: quote! { #binding },
                    });
                }
                let Some(segment) = bound.path.segments.last() else {
                    continue;
                };
                let target = generic_argument(segment, 0);
                let textual = match segment.ident.to_string().as_str() {
                    "Into" => target.is_some_and(|target| {
                        is_named(target, "String") || is_named(target, "PathBuf")
                    }),
                    "AsRef" => {
                        target.is_some_and(|target| is_str(target) || is_named(target, "Path"))
                    }
                    _ => false,
                };
                if textual {
                    return Ok(LoweredParameter {
                        tag: "str".to_string(),
                        binding: quote! { let #binding = ::pill_engine::mirror::read_str(#slot)?; },
                        pass: quote! { #binding },
                    });
                }
            }
            Err(unsupported(ty))
        }
        syn::Type::Tuple(tuple) if !tuple.elems.is_empty() => {
            let elements: Vec<&syn::Type> = tuple.elems.iter().collect();
            let (offsets, width) = tuple_layout(&elements)?;
            if width > 16 {
                return Err(syn::Error::new_spanned(
                    ty,
                    "a mirrored tuple must fit in 16 bytes",
                ));
            }
            let reads = elements.iter().zip(&offsets).map(|(element, offset)| {
                if primitive_name(element).as_deref() == Some("bool") {
                    quote! { ::pill_engine::mirror::read_bool((#slot).add(#offset)) }
                } else {
                    quote! { ::pill_engine::mirror::read::<#element>((#slot).add(#offset)) }
                }
            });
            let tag = elements
                .iter()
                .map(|element| primitive_name(element).expect("checked by tuple_layout"))
                .collect::<Vec<_>>()
                .join(",");
            Ok(LoweredParameter {
                tag: format!("tuple:{tag}"),
                binding: quote! { let #binding = (#(#reads,)*); },
                pass: quote! { #binding },
            })
        }
        syn::Type::Array(array) => {
            let (element, length) = array_shape(array)?;
            if element == "bool" {
                return Err(syn::Error::new_spanned(
                    ty,
                    "an array of `bool` cannot be mirrored",
                ));
            }
            Ok(LoweredParameter {
                tag: format!("array:{element}:{length}"),
                binding: quote! { let #binding = ::pill_engine::mirror::read::<#array>(#slot); },
                pass: quote! { #binding },
            })
        }
        syn::Type::Path(_) | syn::Type::Group(_) | syn::Type::Paren(_) => {
            if let Some(primitive) = primitive_name(ty) {
                let read = if primitive == "bool" {
                    quote! { ::pill_engine::mirror::read_bool(#slot) }
                } else {
                    quote! { ::pill_engine::mirror::read::<#ty>(#slot) }
                };
                return Ok(LoweredParameter {
                    tag: primitive,
                    binding: quote! { let #binding = #read; },
                    pass: quote! { #binding },
                });
            }
            let segment = last_segment(ty).ok_or_else(|| unsupported(ty))?;
            match segment.ident.to_string().as_str() {
                "PathBuf" => Ok(LoweredParameter {
                    tag: "str".to_string(),
                    binding: quote! {
                        let #binding = ::std::path::PathBuf::from(::pill_engine::mirror::read_str(#slot)?);
                    },
                    pass: quote! { #binding },
                }),
                "String" => Ok(LoweredParameter {
                    tag: "str".to_string(),
                    binding: quote! {
                        let #binding = ::std::string::String::from(::pill_engine::mirror::read_str(#slot)?);
                    },
                    pass: quote! { #binding },
                }),
                "Vec" => {
                    let element = generic_argument(segment, 0).ok_or_else(|| unsupported(ty))?;
                    let (tag, read) = lower_element(element, &slot, owner)?;
                    Ok(LoweredParameter {
                        tag: format!("slice:{tag}"),
                        binding: quote! { let #binding = #read; },
                        pass: quote! { #binding },
                    })
                }
                "Option" => {
                    let inner = generic_argument(segment, 0).ok_or_else(|| unsupported(ty))?;
                    let tag = value_tag(inner, owner)?;
                    let inner_type = concrete_type(inner, owner)?;
                    Ok(LoweredParameter {
                        tag: format!("option:{tag}"),
                        binding: quote! {
                            let #binding = ::pill_engine::mirror::read_option(#slot, |at| {
                                <#inner_type as ::pill_engine::mirror::MirrorValue>::read_slot(at)
                            })?;
                        },
                        pass: quote! { #binding },
                    })
                }
                _ => {
                    let tag = value_tag(ty, owner)?;
                    let target = concrete_type(ty, owner)?;
                    Ok(LoweredParameter {
                        tag,
                        binding: quote! {
                            let #binding = <#target as ::pill_engine::mirror::MirrorValue>::read_slot(#slot)?;
                        },
                        pass: quote! { #binding },
                    })
                }
            }
        }
        _ => Err(unsupported(ty)),
    }
}

// =============================================================================
// Results
// =============================================================================

/// A lowered result: its tag, and the statements that write `__value`.
pub(crate) struct LoweredReturn {
    /// The codegen's tag for the result; empty for `()`.
    pub(crate) tag: String,
    /// Whether the function returns a `Result` whose `Err` fails the call.
    pub(crate) fallible: bool,
    /// Whether there is a value to write at all.
    pub(crate) has_value: bool,
    /// Statements writing `__value` (the `Ok` value, for a `Result`).
    pub(crate) write: TokenStream2,
}

/// Lower the plain (not `Result`) part of a result type.
///
/// The vocabulary is the parameter one, minus borrows other than `&str`: a
/// result outlives the call, so it has to be owned by the time it crosses.
fn lower_plain_return(
    ty: &syn::Type,
    owner: Option<&Owner>,
) -> syn::Result<(String, TokenStream2)> {
    let ret = quote! { __ret };
    if let syn::Type::Tuple(tuple) = ty {
        if tuple.elems.is_empty() {
            return Ok((String::new(), TokenStream2::new()));
        }
        let elements: Vec<&syn::Type> = tuple.elems.iter().collect();
        let (offsets, width) = tuple_layout(&elements)?;
        if width > 16 {
            return Err(syn::Error::new_spanned(
                ty,
                "a mirrored tuple must fit in 16 bytes",
            ));
        }
        let names: Vec<syn::Ident> = (0..elements.len())
            .map(|index| format_ident!("__element{index}"))
            .collect();
        let writes = names.iter().zip(&offsets).map(|(name, offset)| {
            quote! {
                ::pill_engine::mirror::MirrorReturn::write_return(#name, #ret.add(#offset));
            }
        });
        let tag = elements
            .iter()
            .map(|element| primitive_name(element).expect("checked by tuple_layout"))
            .collect::<Vec<_>>()
            .join(",");
        return Ok((
            format!("tuple:{tag}"),
            quote! { let (#(#names,)*) = __value; #(#writes)* },
        ));
    }
    if let syn::Type::Array(array) = ty {
        let (element, length) = array_shape(array)?;
        return Ok((
            format!("array:{element}:{length}"),
            quote! { ::pill_engine::mirror::write(#ret, __value); },
        ));
    }
    if let syn::Type::Reference(reference) = ty {
        if is_str(&reference.elem) {
            return Ok((
                "str".to_string(),
                quote! {
                    ::pill_engine::mirror::set_return_string(::std::string::String::from(__value));
                },
            ));
        }
        return Err(syn::Error::new_spanned(
            ty,
            "a mirrored function cannot return a reference; return an owned value",
        ));
    }
    if is_named(ty, "String") {
        return Ok((
            "str".to_string(),
            quote! { ::pill_engine::mirror::set_return_string(__value); },
        ));
    }
    if is_named(ty, "Option") {
        let segment = last_segment(ty).expect("checked by is_named");
        let inner = generic_argument(segment, 0).ok_or_else(|| unsupported(ty))?;
        let tag = value_tag(inner, owner)?;
        return Ok((
            format!("option:{tag}"),
            quote! {
                ::pill_engine::mirror::write_option(#ret, __value, |at, value| {
                    ::pill_engine::mirror::MirrorReturn::write_return(value, at)
                });
            },
        ));
    }
    if is_named(ty, "AssetLoader") {
        return Err(syn::Error::new_spanned(
            ty,
            "an `AssetLoader` cannot be returned to C#",
        ));
    }
    let tag = value_tag(ty, owner)?;
    Ok((
        tag,
        quote! { ::pill_engine::mirror::MirrorReturn::write_return(__value, #ret); },
    ))
}

/// Lower a function's result.
///
/// `Result<T, E>` (or an alias ending in `Result`) with `E: Display` fails the
/// call with `E`'s message and is
/// tagged `result:<tag of T>`; anything else is tagged as
/// [`lower_plain_return`] describes.
pub(crate) fn lower_return(
    output: &syn::ReturnType,
    owner: Option<&Owner>,
) -> syn::Result<LoweredReturn> {
    let syn::ReturnType::Type(_, ty) = output else {
        return Ok(LoweredReturn {
            tag: String::new(),
            fallible: false,
            has_value: false,
            write: TokenStream2::new(),
        });
    };
    // A `Result` alias (`AssetLoadResult<T>`, `io::Result<T>`) names its value
    // first just as `Result` does; the macro sees only the spelling, so any
    // `...Result<T>` counts.
    if let Some(segment) = last_segment(ty)
        .filter(|segment| segment.ident.to_string().ends_with("Result"))
        .filter(|segment| generic_argument(segment, 0).is_some())
    {
        let inner = generic_argument(segment, 0).ok_or_else(|| {
            syn::Error::new_spanned(ty, "name the `Result`'s value type to mirror it")
        })?;
        let (tag, write) = lower_plain_return(inner, owner)?;
        return Ok(LoweredReturn {
            has_value: !tag.is_empty(),
            tag: format!("result:{tag}"),
            fallible: true,
            write,
        });
    }
    let (tag, write) = lower_plain_return(ty, owner)?;
    Ok(LoweredReturn {
        has_value: !tag.is_empty(),
        tag,
        fallible: false,
        write,
    })
}

// =============================================================================
// Trampolines
// =============================================================================

/// How a mirrored function takes its owner.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Receiver {
    /// No receiver: a free or associated function.
    None,
    /// `&self`.
    Ref,
    /// `&mut self`.
    Mut,
    /// `self`.
    Value,
}

impl Receiver {
    /// The descriptor's spelling.
    fn tag(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Ref => "ref",
            Self::Mut => "mut",
            Self::Value => "value",
        }
    }
}

/// Everything one trampoline and its descriptor need.
pub(crate) struct MirroredFunction<'a> {
    /// The Rust function's name.
    pub(crate) ident: &'a syn::Ident,
    /// Its signature.
    pub(crate) signature: &'a syn::Signature,
    /// The owning type, for a method or associated function.
    pub(crate) owner: Option<Owner>,
    /// The descriptor's `type_name`: the owner's qualified name, or the
    /// declaring module's path for a free function.
    pub(crate) type_name: TokenStream2,
    /// The descriptor's `owner_kind`; empty when the attribute cannot know it.
    pub(crate) owner_kind: &'static str,
}

/// Build the trampoline and descriptor submission for one function.
pub(crate) fn emit_function(function: MirroredFunction<'_>) -> syn::Result<TokenStream2> {
    let MirroredFunction {
        ident,
        signature,
        owner,
        type_name,
        owner_kind,
    } = function;
    if !signature.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &signature.generics,
            format!("`{ident}` is generic; a mirrored function needs one concrete signature"),
        ));
    }
    if signature.asyncness.is_some() {
        return Err(syn::Error::new_spanned(
            signature,
            "an `async fn` cannot be mirrored",
        ));
    }

    let mut receiver = Receiver::None;
    let mut parameters = Vec::new();
    for input in &signature.inputs {
        match input {
            syn::FnArg::Receiver(self_receiver) => {
                if owner.is_none() {
                    return Err(syn::Error::new_spanned(
                        self_receiver,
                        "a free function has no receiver",
                    ));
                }
                receiver = match (&self_receiver.reference, &self_receiver.mutability) {
                    (Some(_), Some(_)) => Receiver::Mut,
                    (Some(_), None) => Receiver::Ref,
                    (None, _) => Receiver::Value,
                };
            }
            syn::FnArg::Typed(typed) => parameters.push(typed),
        }
    }

    let first_slot = usize::from(receiver != Receiver::None);
    let mut tags = Vec::new();
    let mut names = Vec::new();
    let mut bindings = Vec::new();
    let mut passes = Vec::new();
    for (index, parameter) in parameters.iter().enumerate() {
        let name = match &*parameter.pat {
            syn::Pat::Ident(pattern) => pattern.ident.to_string(),
            _ => format!("arg{index}"),
        };
        let binding = format_ident!("__argument{index}");
        let slot_index = first_slot + index;
        let lowered = lower_parameter(
            &parameter.ty,
            quote! { ::pill_engine::mirror::slot(__args, #slot_index) },
            &binding,
            &name,
            owner.as_ref(),
        )?;
        tags.push(lowered.tag);
        names.push(name);
        bindings.push(lowered.binding);
        passes.push(lowered.pass);
    }
    let returned = lower_return(&signature.output, owner.as_ref())?;

    let (receiver_binding, call) = match (&owner, receiver) {
        (Some(owner), Receiver::None) => {
            let owner = &owner.ident;
            (TokenStream2::new(), quote! { #owner::#ident(#(#passes),*) })
        }
        (Some(owner), Receiver::Ref) => {
            let owner = &owner.ident;
            (
                quote! {
                    let __receiver = ::pill_engine::mirror::read_ref::<#owner>(
                        ::pill_engine::mirror::slot(__args, 0), "the receiver")?;
                },
                quote! { #owner::#ident(__receiver #(, #passes)*) },
            )
        }
        (Some(owner), Receiver::Mut) => {
            let owner = &owner.ident;
            (
                quote! {
                    let __receiver = ::pill_engine::mirror::read_mut::<#owner>(
                        ::pill_engine::mirror::slot(__args, 0), "the receiver")?;
                },
                quote! { #owner::#ident(__receiver #(, #passes)*) },
            )
        }
        (Some(owner), Receiver::Value) => {
            let owner = &owner.ident;
            (
                quote! {
                    let __receiver = <#owner as ::pill_engine::mirror::MirrorValue>::read_slot(
                        ::pill_engine::mirror::slot(__args, 0))?;
                },
                quote! { #owner::#ident(__receiver #(, #passes)*) },
            )
        }
        (None, _) => (TokenStream2::new(), quote! { #ident(#(#passes),*) }),
    };

    let write = &returned.write;
    let body = match (returned.fallible, returned.has_value) {
        (false, false) => quote! { #call; },
        (false, true) => quote! { let __value = #call; #write },
        (true, false) => quote! { #call.map_err(::pill_engine::mirror::error_message)?; },
        (true, true) => quote! {
            let __value = #call.map_err(::pill_engine::mirror::error_message)?;
            #write
        },
    };

    let symbol_name = match &owner {
        Some(owner) => format!("pill_mirror_{}_{ident}", owner.ident),
        None => format!("pill_mirror_fn_{ident}"),
    };
    let symbol = syn::Ident::new(&symbol_name, ident.span());
    let symbol_literal = syn::LitStr::new(&symbol_name, ident.span());
    let name_literal = syn::LitStr::new(&ident.to_string(), ident.span());
    let return_tag = syn::LitStr::new(&returned.tag, ident.span());
    let tag_literals = tags
        .iter()
        .map(|tag| syn::LitStr::new(tag, Span::call_site()));
    let name_literals = names
        .iter()
        .map(|name| syn::LitStr::new(name, Span::call_site()));
    let receiver_tag = receiver.tag();
    let is_free_function = owner.is_none();

    // SAFETY: in the emitted body, the managed caller packs one slot per
    // argument exactly as the descriptor's tags describe and reserves the
    // result buffer the result's tag needs; every read and write below goes
    // through `pill_engine::mirror`, which states that contract per encoding.
    Ok(quote! {
        #[doc(hidden)]
        #[allow(unused_unsafe, unused_variables, non_snake_case, clippy::needless_borrow)]
        unsafe extern "C" fn #symbol(__args: *const u8, __ret: *mut u8) -> u8 {
            ::pill_engine::mirror::invoke(|| {
                unsafe {
                    #receiver_binding
                    #(#bindings)*
                    #body
                }
                ::core::result::Result::Ok(())
            })
        }

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillMethodDescriptor {
                type_name: #type_name,
                is_free_function: #is_free_function,
                crate_name: env!("CARGO_PKG_NAME"),
                name: #name_literal,
                symbol: #symbol_literal,
                return_tag: #return_tag,
                arg_tags: &[#(#tag_literals),*],
                arg_names: &[#(#name_literals),*],
                receiver: #receiver_tag,
                owner_kind: #owner_kind,
                address: ::pill_engine::component_registry::ExportAddress(#symbol as *const ()),
            }
        }
    })
}

/// The qualified name a type declared at the expansion site is known by.
pub(crate) fn qualified_name(ident: &syn::Ident) -> TokenStream2 {
    quote! { ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident)) }
}

// =============================================================================
// #[pill_mirror_impl]
// =============================================================================

/// Expand `#[pill_mirror_impl]` on an inherent `impl` block.
pub(crate) fn mirror_impl(impl_block: syn::ItemImpl) -> syn::Result<TokenStream2> {
    if impl_block.trait_.is_some() {
        return Err(syn::Error::new_spanned(
            &impl_block,
            "`#[pill_mirror_impl]` requires an inherent impl block",
        ));
    }
    if !impl_block.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &impl_block.generics,
            "`#[pill_mirror_impl]` does not support generic impl blocks",
        ));
    }
    let owner_ident = match &*impl_block.self_ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.clone()),
        _ => None,
    }
    .ok_or_else(|| {
        syn::Error::new_spanned(
            &impl_block.self_ty,
            "`#[pill_mirror_impl]` requires a named receiver type (e.g. `impl OmoMO`)",
        )
    })?;
    let owner = Owner {
        ident: owner_ident.clone(),
    };
    let type_name = qualified_name(&owner_ident);

    let mut emitted = Vec::new();
    for item in &impl_block.items {
        let syn::ImplItem::Fn(method) = item else {
            continue;
        };
        // The marker may be written bare or fully qualified; only its last
        // segment is matched, so a qualified spelling cannot skip a method.
        let mirrored = method.attrs.iter().any(|attribute| {
            attribute
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "pill_mirror_method")
        });
        if !mirrored {
            continue;
        }
        emitted.push(emit_function(MirroredFunction {
            ident: &method.sig.ident,
            signature: &method.sig,
            owner: Some(owner.clone()),
            type_name: type_name.clone(),
            owner_kind: "",
        })?);
    }
    Ok(quote! {
        #impl_block
        #(#emitted)*
    })
}

// =============================================================================
// #[pill_mirror_fn]
// =============================================================================

/// Expand `#[pill_mirror_fn]` on a free function.
pub(crate) fn mirror_fn(item: syn::ItemFn) -> syn::Result<TokenStream2> {
    let trampoline = emit_function(MirroredFunction {
        ident: &item.sig.ident,
        signature: &item.sig,
        owner: None,
        type_name: quote! { ::core::module_path!() },
        owner_kind: "module",
    })?;
    Ok(quote! {
        #item
        #trampoline
    })
}

// =============================================================================
// Type Rows
// =============================================================================

/// One `__type` (or other `__`-named) row describing a type rather than a
/// function.
struct TypeRow<'a> {
    /// The type's identifier.
    ident: &'a syn::Ident,
    /// The row's name.
    name: &'a str,
    /// The owner kind it declares.
    kind: &'a str,
    /// `return_tag`.
    return_tag: String,
    /// `arg_tags`.
    arg_tags: Vec<String>,
    /// `arg_names`.
    arg_names: Vec<String>,
    /// The trampoline, when the row has one.
    address: Option<syn::Ident>,
}

/// Submit one type row.
fn type_row(row: TypeRow<'_>) -> TokenStream2 {
    let type_name = qualified_name(row.ident);
    let name = row.name;
    let kind = row.kind;
    let return_tag = row.return_tag;
    let arg_tags = row.arg_tags;
    let arg_names = row.arg_names;
    let (symbol, address) = match &row.address {
        Some(trampoline) => (
            trampoline.to_string(),
            quote! { ::pill_engine::component_registry::ExportAddress(#trampoline as *const ()) },
        ),
        None => (
            String::new(),
            quote! { ::pill_engine::component_registry::ExportAddress(::core::ptr::null()) },
        ),
    };
    quote! {
        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillMethodDescriptor {
                type_name: #type_name,
                is_free_function: false,
                crate_name: env!("CARGO_PKG_NAME"),
                name: #name,
                symbol: #symbol,
                return_tag: #return_tag,
                arg_tags: &[#(#arg_tags),*],
                arg_names: &[#(#arg_names),*],
                receiver: "",
                owner_kind: #kind,
                address: #address,
            }
        }
    }
}

// =============================================================================
// #[pill_mirror_object]
// =============================================================================

/// The flags `#[pill_mirror_object(...)]` accepts.
#[derive(Default)]
struct ObjectFlags {
    /// The type is an `Asset`: emit the asset store operations.
    asset: bool,
    /// The type is an `ImportedAsset`: emit the import operation.
    import: bool,
    /// The type is a `StandaloneAsset`: emit the standalone import.
    standalone: bool,
}

/// Parse the attribute's comma-separated flags.
fn object_flags(attribute: TokenStream2) -> syn::Result<ObjectFlags> {
    let mut flags = ObjectFlags::default();
    let parser = syn::punctuated::Punctuated::<syn::Ident, syn::Token![,]>::parse_terminated;
    let idents = syn::parse::Parser::parse2(parser, attribute)?;
    for ident in idents {
        match ident.to_string().as_str() {
            "asset" => flags.asset = true,
            "import" => flags.import = true,
            "standalone" => flags.standalone = true,
            other => {
                return Err(syn::Error::new_spanned(
                    &ident,
                    format!(
                    "unknown `#[pill_mirror_object]` flag `{other}`; expected `asset`, `import` \
                         or `standalone`"
                ),
                ))
            }
        }
    }
    if (flags.import || flags.standalone) && !flags.asset {
        flags.asset = true;
    }
    Ok(flags)
}

/// Expand `#[pill_mirror_object]` on a struct or enum.
///
/// The type crosses to C# as a box the managed side owns: a class whose
/// methods are the `#[pill_mirror_impl]` ones, created by the functions that
/// return the type, moved into the functions that take it by value, and
/// dropped through the drop trampoline emitted here. With `asset` the type
/// also gets the asset store operations the runtime's `AssetManager`
/// extension methods reach (`__asset_add`, ...); `import` and `standalone`
/// add the two import paths for an `ImportedAsset` or `StandaloneAsset`.
pub(crate) fn mirror_object(attribute: TokenStream2, item: syn::Item) -> syn::Result<TokenStream2> {
    let flags = object_flags(attribute)?;
    let (ident, generics) = match &item {
        syn::Item::Struct(item) => (&item.ident, &item.generics),
        syn::Item::Enum(item) => (&item.ident, &item.generics),
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "`#[pill_mirror_object]` goes on a struct or an enum",
            ))
        }
    };
    if !generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            generics,
            "a generic type cannot be mirrored; mirror a concrete type",
        ));
    }

    let drop_trampoline = format_ident!("pill_mirror_{}___drop", ident);
    let mut flag_names = Vec::new();
    if flags.asset {
        flag_names.push("asset".to_string());
    }
    if flags.import {
        flag_names.push("import".to_string());
    }
    if flags.standalone {
        flag_names.push("standalone".to_string());
    }
    let mut rows = vec![type_row(TypeRow {
        ident,
        name: "__type",
        kind: "object",
        return_tag: String::new(),
        arg_tags: Vec::new(),
        arg_names: flag_names,
        address: Some(drop_trampoline.clone()),
    })];
    let mut trampolines = Vec::new();

    let mut operations: Vec<(&str, TokenStream2)> = Vec::new();
    if flags.asset {
        operations.extend([
            ("add", quote! { add }),
            ("add_named", quote! { add_named }),
            ("add_named_with_guid", quote! { add_named_with_guid }),
            ("remove", quote! { remove }),
            ("contains", quote! { contains }),
            ("handle_by_name", quote! { handle_by_name }),
            ("handle_by_guid", quote! { handle_by_guid }),
            ("guid_of", quote! { guid_of }),
        ]);
    }
    if flags.import {
        operations.push(("import", quote! { import }));
    }
    if flags.standalone {
        operations.push(("import_standalone", quote! { import_standalone }));
    }
    for (operation, function) in operations {
        let trampoline = format_ident!("pill_mirror_{}___asset_{}", ident, operation);
        let row_name = format!("__asset_{operation}");
        // SAFETY: in the emitted body, forwarded unchanged - the trampoline's
        // contract is the asset operation's, which `pill_engine::mirror::assets`
        // documents per operation.
        trampolines.push(quote! {
            #[doc(hidden)]
            #[allow(non_snake_case)]
            unsafe extern "C" fn #trampoline(__args: *const u8, __ret: *mut u8) -> u8 {
                unsafe { ::pill_engine::mirror::assets::#function::<#ident>(__args, __ret) }
            }
        });
        rows.push(type_row(TypeRow {
            ident,
            name: &row_name,
            kind: "object",
            return_tag: String::new(),
            arg_tags: Vec::new(),
            arg_names: Vec::new(),
            address: Some(trampoline),
        }));
    }

    Ok(quote! {
        #item

        impl ::pill_engine::mirror::MirrorObject for #ident {}

        // SAFETY: the slot and every slice element hold the box's address,
        // which is what the managed `RustObject` passes for this type.
        unsafe impl ::pill_engine::mirror::MirrorValue for #ident {
            const ELEMENT_SIZE: usize = 8;

            unsafe fn read_slot(slot: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                unsafe { ::pill_engine::mirror::object_from_raw::<#ident>(slot) }
            }

            unsafe fn read_element(element: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                unsafe { ::pill_engine::mirror::object_from_raw::<#ident>(element) }
            }
        }

        // SAFETY: the result is the new box's address, which the managed side
        // wraps in a `RustObject` for this type.
        unsafe impl ::pill_engine::mirror::MirrorReturn for #ident {
            unsafe fn write_return(self, ret: *mut u8) {
                unsafe {
                    ::pill_engine::mirror::write(
                        ret,
                        ::pill_engine::mirror::object_into_raw(self) as usize,
                    )
                }
            }
        }

        #[doc(hidden)]
        #[allow(non_snake_case)]
        unsafe extern "C" fn #drop_trampoline(__args: *const u8, __ret: *mut u8) -> u8 {
            unsafe { ::pill_engine::mirror::drop_object::<#ident>(__args) }
        }

        #(#trampolines)*
        #(#rows)*
    })
}

// =============================================================================
// #[pill_mirror_resource]
// =============================================================================

/// Expand `#[pill_mirror_resource("shared::Name")]` on a struct.
///
/// Implements `Resource` with that shared name - the identity C# computes for
/// the generated marker type - and declares the type, so its
/// `#[pill_mirror_impl]` methods become extension methods on
/// `Res<T>`/`ResMut<T>` and a mirrored function can take it as `&T`/`&mut T`.
pub(crate) fn mirror_resource(
    attribute: TokenStream2,
    item: syn::Item,
) -> syn::Result<TokenStream2> {
    let name: syn::LitStr = syn::parse2(attribute).map_err(|error| {
        syn::Error::new(
            error.span(),
            "`#[pill_mirror_resource]` takes the resource's shared name: \
             `#[pill_mirror_resource(\"my_crate::MyResource\")]`",
        )
    })?;
    let ident = match &item {
        syn::Item::Struct(item) if item.generics.params.is_empty() => &item.ident,
        other => {
            return Err(syn::Error::new_spanned(
                other,
                "`#[pill_mirror_resource]` goes on a non-generic struct",
            ))
        }
    };
    let row = type_row(TypeRow {
        ident,
        name: "__type",
        kind: "resource",
        return_tag: String::new(),
        arg_tags: Vec::new(),
        arg_names: vec![name.value()],
        address: None,
    });
    Ok(quote! {
        #item

        impl ::pill_engine::Resource for #ident {
            fn shared_name() -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#name)
            }
        }

        #row
    })
}

// =============================================================================
// #[derive(PillMirror)]
// =============================================================================

/// The `MirrorValue` and `MirrorReturn` implementations of a value type.
///
/// A slot carries the address of the value (it may be larger than a slot); a
/// slice element and a result carry the value's own bytes.
pub(crate) fn value_type_encodings(ident: &syn::Ident) -> TokenStream2 {
    quote! {
        // SAFETY: a value type is plain data - `PillMirror` refuses heap fields
        // - so a bitwise copy of the managed bytes is a valid value.
        unsafe impl ::pill_engine::mirror::MirrorValue for #ident {
            const ELEMENT_SIZE: usize = ::core::mem::size_of::<#ident>();

            unsafe fn read_slot(slot: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                let pointer = unsafe { ::pill_engine::mirror::read_pointer(slot) };
                if pointer.is_null() {
                    return ::core::result::Result::Err(::std::string::String::from(
                        "a value argument was null",
                    ));
                }
                ::core::result::Result::Ok(unsafe {
                    ::core::ptr::read_unaligned(pointer as *const #ident)
                })
            }

            unsafe fn read_element(element: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                ::core::result::Result::Ok(unsafe {
                    ::core::ptr::read_unaligned(element as *const #ident)
                })
            }
        }

        // SAFETY: the managed side reserves `size_of::<Self>()` bytes for a
        // value-type result.
        unsafe impl ::pill_engine::mirror::MirrorReturn for #ident {
            unsafe fn write_return(self, ret: *mut u8) {
                unsafe { ::core::ptr::write_unaligned(ret as *mut #ident, self) }
            }
        }
    }
}

/// Expand `#[derive(PillMirror)]` on a fieldless enum with an integer `repr`.
///
/// The enum crosses as its discriminant; reading an unknown one is an error,
/// never a transmute. The `__type` row carries the representation and the
/// variants, which the codegen turns into a C# `enum`.
pub(crate) fn mirror_enum(
    input: &syn::DeriveInput,
    data: &syn::DataEnum,
) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "`PillMirror` cannot be derived for a generic type",
        ));
    }
    let mut representation = None;
    for attribute in &input.attrs {
        if !attribute.path().is_ident("repr") {
            continue;
        }
        attribute.parse_nested_meta(|meta| {
            if let Some(name) = meta.path.get_ident().map(ToString::to_string) {
                if matches!(
                    name.as_str(),
                    "u8" | "u16" | "u32" | "i8" | "i16" | "i32" | "u64" | "i64"
                ) {
                    representation = Some(name);
                }
            }
            Ok(())
        })?;
    }
    let representation = representation.ok_or_else(|| {
        syn::Error::new_spanned(
            ident,
            "a mirrored enum needs an integer representation: add `#[repr(u8)]` (or another \
             integer)",
        )
    })?;
    let repr_type = syn::Ident::new(&representation, Span::call_site());

    let mut names = Vec::new();
    let mut values: Vec<i128> = Vec::new();
    let mut next = 0i128;
    for variant in &data.variants {
        if !matches!(variant.fields, syn::Fields::Unit) {
            return Err(syn::Error::new_spanned(
                variant,
                "a mirrored enum has fieldless variants only",
            ));
        }
        let value = match &variant.discriminant {
            None => next,
            Some((_, expression)) => discriminant_value(expression)?,
        };
        names.push(variant.ident.clone());
        values.push(value);
        next = value + 1;
    }

    let arms = names.iter().zip(&values).map(|(name, value)| {
        let literal = proc_macro2::Literal::i128_unsuffixed(*value);
        quote! { #literal => ::core::result::Result::Ok(#ident::#name), }
    });
    // SAFETY: in the emitted read, `at` is a slot or element the managed side
    // wrote for this enum, holding its representation's bytes.
    let read = quote! {
        let value = unsafe { ::pill_engine::mirror::read::<#repr_type>(at) };
        match value {
            #(#arms)*
            other => ::core::result::Result::Err(::std::format!(
                "{} is not a {} value", other, ::core::stringify!(#ident)
            )),
        }
    };
    let row = type_row(TypeRow {
        ident,
        name: "__type",
        kind: "enum",
        return_tag: representation.clone(),
        arg_tags: values.iter().map(ToString::to_string).collect(),
        arg_names: names.iter().map(ToString::to_string).collect(),
        address: None,
    });
    Ok(quote! {
        // SAFETY: the discriminant is read as the declared representation and
        // matched against the declared variants; anything else is refused.
        unsafe impl ::pill_engine::mirror::MirrorValue for #ident {
            const ELEMENT_SIZE: usize = ::core::mem::size_of::<#repr_type>();

            unsafe fn read_slot(slot: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                let at = slot;
                #read
            }

            unsafe fn read_element(element: *const u8) -> ::pill_engine::mirror::MirrorResult<Self> {
                let at = element;
                #read
            }
        }

        // SAFETY: the discriminant, as the declared representation.
        unsafe impl ::pill_engine::mirror::MirrorReturn for #ident {
            unsafe fn write_return(self, ret: *mut u8) {
                unsafe { ::pill_engine::mirror::write(ret, self as #repr_type) }
            }
        }

        #row
    })
}

/// The value of a literal (possibly negated) enum discriminant.
fn discriminant_value(expression: &syn::Expr) -> syn::Result<i128> {
    match expression {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(value),
            ..
        }) => value.base10_parse(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => discriminant_value(expr).map(|value| -value),
        syn::Expr::Group(group) => discriminant_value(&group.expr),
        other => Err(syn::Error::new(
            other.span(),
            "a mirrored enum's discriminants must be integer literals",
        )),
    }
}
