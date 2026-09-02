//! Procedural macros that remove the error-prone registration and FFI
//! boilerplate from optional-module and project crates.
//!
//! # Responsibilities
//!
//! - [`derive(PillComponent)`] turns one component type into everything the
//!   engine needs to know about it: the [`Component`] impl, the
//!   [`TraitAccessible`] impl, and a descriptor submitted into this artifact's
//!   compile-time registry. Persistable components (those marked
//!   `#[pill(persistable)]`) are additionally registered for schema migration,
//!   and they drive the aggregate project schema fingerprint — no hand-written
//!   registration list or fingerprint hash to keep in sync.
//! - [`attribute(PillModule)`] wraps an optional module's `register` function
//!   and generates the `pill_module_*` C-ABI exports (version, name, init) with
//!   the panic guard and engine-pointer reconstruction that every module
//!   otherwise hand-writes.
//! - [`attribute(PillProject)`] does the same for the project ABI
//!   (`project_init`, `project_update`, `project_schema_fingerprint`).
//!
//! # Design
//!
//! The generated code references the engine through fully-qualified paths
//! (`::pill_engine::...`, `::trait_type_map::...`) so the macros never need to
//! know which crate a consumer is. Component collection uses the [`inventory`]
//! crate, which builds a registry per linked artifact: each hot-reload
//! generation DLL ends up with its own registry containing exactly the
//! components its own sources declared, so re-registering a stale generation's
//! types is impossible by construction.
//!
//! [`Component`]: ::pill_engine::Component
//! [`TraitAccessible`]: ::trait_type_map::TraitAccessible
//! [`inventory`]: https://docs.rs/inventory

extern crate proc_macro;

// External crates
use proc_macro::TokenStream;
use quote::{format_ident, quote, ToTokens};
use syn::{parse_macro_input, spanned::Spanned, DeriveInput, ItemFn};

// =============================================================================
// #[derive(PillComponent)]
// =============================================================================

/// Turns a component type into its engine registration.
///
/// Generates:
/// - `impl Component for T`
/// - the `TraitAccessible<dyn Component>` impl
/// - a descriptor submitted into this artifact's compile-time registry
///
/// Supported helper attribute:
/// - `#[pill(persistable)]` — the component is schema-migrated across reloads
///   (requires `Clone + Serialize + DeserializeOwned + Default`, matching
///   [`World::register_persistable_component`]).
///
/// [`World::register_persistable_component`]: ::pill_engine::World::register_persistable_component
#[proc_macro_derive(PillComponent, attributes(pill))]
pub fn derive_pill_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;

    // Parse the `#[pill(...)]` helper attribute.
    let mut persistable = false;
    for attribute in &input.attrs {
        if attribute.path().is_ident("pill") {
            if let Err(error) = attribute.parse_nested_meta(|meta| {
                if meta.path.is_ident("persistable") {
                    persistable = true;
                    Ok(())
                } else {
                    Err(meta.error("unknown `pill` attribute; expected `persistable`"))
                }
            }) {
                return error.to_compile_error().into();
            }
        }
    }

    // Generic components are not supported: the engine registers concrete
    // types keyed by `TypeId`, so a generic would be meaningless.
    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "`PillComponent` cannot be derived for a generic type",
        )
        .to_compile_error()
        .into();
    }

    // Capture the compile-time field layout for the C# mirror codegen. An
    // unsupported field type is a compile error here, before the host would
    // have to guess at a mirror.
    let (declared_layout, layout_reference) = match component_field_descriptors(&input) {
        Ok(pair) => pair,
        Err(error) => return error.to_compile_error().into(),
    };

    let register_fn_name = format_ident!("__pill_register_{}", ident);
    let registration_call = if persistable {
        quote! {
            world.register_persistable_component_with_layout::<#ident>(#layout_reference);
        }
    } else {
        quote! {
            world.register_component_with_layout::<#ident>(#layout_reference);
        }
    };
    let type_name = quote! {
        ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident))
    };

    let expanded = quote! {
        impl ::pill_engine::Component for #ident {}
        ::trait_type_map::impl_trait_accessible!(dyn ::pill_engine::Component; #ident);

        #declared_layout

        /// Registers this component into the world; used by the artifact-wide
        /// registration loop generated for the module/project entry point.
        #[allow(non_snake_case)]
        fn #register_fn_name(world: &mut ::pill_engine::World) {
            #registration_call
        }

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillComponentDescriptor {
                type_name: #type_name,
                persistable: #persistable,
                fields: #layout_reference,
                register: #register_fn_name,
            }
        }
    };

    expanded.into()
}

/// Emit a `static` carrying one [`ComponentFieldDescriptor`] per named field
/// and return it together with the expression that names it.
///
/// Enums, unions, unit structs and tuple structs have no named fields to
/// mirror and yield an empty layout.
fn component_field_descriptors(
    input: &DeriveInput,
) -> syn::Result<(proc_macro2::TokenStream, proc_macro2::TokenStream)> {
    let ident = &input.ident;
    let named = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            syn::Fields::Named(named) => &named.named,
            _ => return Ok((quote! {}, quote! { &[] })),
        },
        _ => return Ok((quote! {}, quote! { &[] })),
    };
    if named.is_empty() {
        return Ok((quote! {}, quote! { &[] }));
    }

    let mut entries = Vec::with_capacity(named.len());
    for field in named {
        let field_ident = field.ident.as_ref().expect("named field has an identifier");
        let field_ty = &field.ty;
        let field_name = field_ident.to_string();
        let type_tag = field_type_tag(field_ty)?;
        // Array lengths may be a literal or a `const` path; both resolve in a
        // const context, so the count is computed here rather than parsed.
        let element_count = match field_ty {
            syn::Type::Array(array) => {
                let array_len = &array.len;
                quote! { (#array_len as usize) }
            }
            _ => quote! { 0usize },
        };
        entries.push(quote! {
            ::pill_engine::component_registry::ComponentFieldDescriptor {
                name: #field_name,
                type_tag: #type_tag,
                offset: ::core::mem::offset_of!(#ident, #field_ident),
                size: ::core::mem::size_of::<#field_ty>(),
                align: ::core::mem::align_of::<#field_ty>(),
                element_count: #element_count,
            }
        });
    }

    let static_name = format_ident!("__PILL_FIELD_LAYOUT_{}", ident);
    let declared = quote! {
        /// Compile-time field layout of `#ident`, consumed by the C# mirror
        /// codegen. Do not reference; generated by `#[derive(...)]`.
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #static_name: &[::pill_engine::component_registry::ComponentFieldDescriptor] = &[ #(#entries),* ];
    };
    Ok((declared, quote! { #static_name }))
}

/// Map a Rust field type to the closed C#-mirror type-tag vocabulary.
///
/// Primitives and fixed-size arrays are expressible; any other path type is
/// tagged as a nested struct by its fully-qualified name (the codegen resolves
/// it against the artifact's `PillMirror` inventory, falling back to an opaque
/// blob of the field's size when it is not declared). Heap-owning types and
/// `char` are rejected outright because no mirror can represent them.
fn field_type_tag(ty: &syn::Type) -> syn::Result<String> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => {
            let last = path
                .path
                .segments
                .last()
                .map(|segment| segment.ident.to_string())
                .unwrap_or_default();
            match last.as_str() {
                "f32" | "f64" | "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64"
                | "u64" | "bool" | "usize" | "isize" => Ok(last),
                "char" => Err(syn::Error::new_spanned(
                    ty,
                    "`char` fields cannot be mirrored: Rust `char` is 4 bytes while C# `char` is 2",
                )),
                // Heap-owning or otherwise un-mirrorable standard types.
                "String" | "Vec" | "Box" | "Arc" | "Rc" | "Option" | "Result" | "Cow"
                | "HashMap" | "HashSet" | "BTreeMap" | "BTreeSet" | "VecDeque"
                | "LinkedList" => Err(syn::Error::new_spanned(
                    ty,
                    format!(
                        "field type `{last}` owns heap memory and cannot be mirrored to C#; use a blittable value type"
                    ),
                )),
                _ => {
                    // A nested struct (or an enum, which the codegen treats as
                    // an un-resolvable struct and renders opaque). Tagged by
                    // its fully-qualified path so `PillMirror` descriptors
                    // resolve by name.
                    let qualified = path
                        .path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::");
                    Ok(format!("struct:{qualified}"))
                }
            }
        }
        syn::Type::Array(array) => {
            // The element count lives in `element_count` (computed from the
            // length expression at compile time), so the tag only carries the
            // element type.
            let inner = field_type_tag(&array.elem)?;
            Ok(format!("array:{inner}"))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "unsupported field type for the C# mirror; use a primitive, a fixed-size array, or a `#[derive(PillMirror)]` struct",
        )),
    }
}

// =============================================================================
// #[derive(PillMirror)]
// =============================================================================

/// Turns a plain (non-component) value type into a typed C# mirror.
///
/// Generates a [`PillValueTypeDescriptor`] submitted into this artifact's
/// compile-time inventory, so the host's C# codegen can resolve
/// `struct:<path>` field tags to typed nested structs. Only named-field
/// structs are supported; generic types are rejected.
///
/// [`PillValueTypeDescriptor`]: ::pill_engine::component_registry::PillValueTypeDescriptor
#[proc_macro_derive(PillMirror)]
pub fn derive_pill_mirror(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;

    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "`PillMirror` cannot be derived for a generic type",
        )
        .to_compile_error()
        .into();
    }

    let (declared_layout, layout_reference) = match component_field_descriptors(&input) {
        Ok(pair) => pair,
        Err(error) => return error.to_compile_error().into(),
    };

    let type_name = quote! {
        ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident))
    };

    let expanded = quote! {
        #declared_layout

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillValueTypeDescriptor {
                type_name: #type_name,
                size: ::core::mem::size_of::<#ident>(),
                align: ::core::mem::align_of::<#ident>(),
                fields: #layout_reference,
            }
        }
    };

    expanded.into()
}

// =============================================================================
// #[pill_mirror_impl] / #[pill_mirror_method]
// =============================================================================

/// Mirrors selected `&self` methods of a `#[derive(PillMirror)]` type to C#.
///
/// Applied to an inherent `impl` block; methods inside it marked with
/// `#[pill_mirror_method]` become typed C# instance methods on the generated
/// mirror struct, implemented by calling the real Rust method through a
/// generated `extern "C"` trampoline. The derive and this attribute are
/// separate because the derive only sees the struct definition, while the
/// method signatures live in a later `impl` block.
///
/// ```ignore
/// #[derive(PillMirror)]
/// pub struct OmoMO { pub x: u64, pub y: u64 }
///
/// #[pill_mirror_impl]
/// impl OmoMO {
///     #[pill_mirror_method]
///     pub fn get_sum(&self) -> u64 { self.x + self.y }
/// }
/// ```
///
/// For every marked method the macro emits, at module level next to the impl:
/// - a `#[no_mangle] extern "C"` trampoline named
///   `pill_mirror_{TypeName}_{method}` whose ABI is fixed regardless of the
///   Rust method's calling convention, and
/// - a [`PillMethodDescriptor`] submitted into this artifact's registry, so
///   the host can resolve the trampoline's symbol and hand the address to the
///   C# runtime.
///
/// v1 supports a deliberately narrow contract, matching what a pinned C# box
/// can express in safe code: a `&self` receiver (read-only — writes cannot
/// propagate back through the box), primitive arguments and return values
/// (`u8..u64`, `i8..i64`, `f32`, `f64`, `bool`, `usize`, `isize`), and a `()`
/// return. Anything else is rejected here at compile time with a clear error.
///
/// [`PillMethodDescriptor`]: ::pill_engine::component_registry::PillMethodDescriptor
#[proc_macro_attribute]
pub fn pill_mirror_impl(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let impl_block = parse_macro_input!(item as syn::ItemImpl);

    if impl_block.trait_.is_some() {
        return syn::Error::new_spanned(
            &impl_block,
            "`#[pill_mirror_impl]` requires an inherent impl block",
        )
        .to_compile_error()
        .into();
    }
    if !impl_block.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &impl_block.generics,
            "`#[pill_mirror_impl]` does not support generic impl blocks",
        )
        .to_compile_error()
        .into();
    }
    let type_ident = match &*impl_block.self_ty {
        syn::Type::Path(type_path) => type_path
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
    });
    let type_ident = match type_ident {
        Ok(ident) => ident,
        Err(error) => return error.to_compile_error().into(),
    };
    let type_name = quote! {
        ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#type_ident))
    };

    let mut mirror_items: Vec<proc_macro2::TokenStream> = Vec::new();
    for item in &impl_block.items {
        let syn::ImplItem::Fn(method) = item else {
            continue;
        };
        let is_mirrored = method
            .attrs
            .iter()
            .any(|attribute| attribute.path().is_ident("pill_mirror_method"));
        if !is_mirrored {
            continue;
        }
        match emit_mirrored_method_trampoline(method, &type_ident, &type_name) {
            Ok(stream) => mirror_items.push(stream),
            Err(error) => return error.to_compile_error().into(),
        }
    }

    let mut expanded = quote! { #impl_block };
    for mirror_item in mirror_items {
        expanded.extend(mirror_item);
    }
    expanded.into()
}

/// Marker attribute for methods inside a `#[pill_mirror_impl]` block.
///
/// The impl-level macro reads it from the token stream; it is otherwise a
/// no-op that leaves the method untouched, so a method compiles the same with
/// or without the mirror machinery.
#[proc_macro_attribute]
pub fn pill_mirror_method(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Build the `#[no_mangle] extern "C"` trampoline and descriptor submission
/// for one `#[pill_mirror_method]` method.
fn emit_mirrored_method_trampoline(
    method: &syn::ImplItemFn,
    type_ident: &syn::Ident,
    type_name: &proc_macro2::TokenStream,
) -> Result<proc_macro2::TokenStream, syn::Error> {
    let method_ident = &method.sig.ident;
    if !method.sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &method.sig.generics,
            format!(
                "`{}` is generic; mirrored methods must have concrete signatures",
                method_ident
            ),
        ));
    }

    // Receiver must be `&self` (read-only). `&mut self` cannot propagate writes
    // through the C# pinned-box call, and `self` by value would copy the type
    // across an ABI the mirror does not define.
    match &method.sig.inputs.first() {
        Some(syn::FnArg::Receiver(receiver))
            if receiver.reference.is_some() && receiver.mutability.is_none() => {}
        Some(other) => {
            return Err(syn::Error::new_spanned(
                other,
                format!(
                    "`{method_ident}` must take `&self` to be mirrored; `&mut self` and `self` are not supported"
                ),
            ));
        }
        None => {
            return Err(syn::Error::new_spanned(
                &method.sig,
                format!("`{method_ident}` must take `&self` to be mirrored"),
            ));
        }
    }

    // Every argument must be a supported primitive, tagged for the C# codegen.
    let mut arg_types: Vec<&syn::Type> = Vec::new();
    let mut arg_idents: Vec<syn::Ident> = Vec::new();
    let mut arg_tag_literals: Vec<syn::LitStr> = Vec::new();
    for (index, argument) in method.sig.inputs.iter().enumerate().skip(1) {
        let syn::FnArg::Typed(pat_type) = argument else {
            unreachable!("receiver handled above; remaining inputs are typed")
        };
        let tag = mirror_method_type_tag(&pat_type.ty).map_err(|error| {
            syn::Error::new_spanned(
                &pat_type.ty,
                format!("{}: {}", error, pat_type.ty.to_token_stream()),
            )
        })?;
        arg_types.push(&pat_type.ty);
        arg_idents.push(format_ident!("arg{index}"));
        arg_tag_literals.push(syn::LitStr::new(&tag, pat_type.ty.span()));
    }

    // Return must be a supported primitive or `()`.
    let return_tag_literal: syn::LitStr;
    let return_type: proc_macro2::TokenStream;
    match &method.sig.output {
        syn::ReturnType::Default => {
            return_tag_literal = syn::LitStr::new("", method.sig.ident.span());
            return_type = quote! { () };
        }
        syn::ReturnType::Type(_, return_ty) => {
            let tag = mirror_method_type_tag(return_ty).map_err(|error| {
                syn::Error::new_spanned(
                    return_ty,
                    format!("{}: {}", error, return_ty.to_token_stream()),
                )
            })?;
            return_tag_literal = syn::LitStr::new(&tag, return_ty.span());
            return_type = quote! { #return_ty };
        }
    }

    // Deterministic exported symbol + descriptor string, both derived from the
    // same names so they can never drift.
    let method_name_literal = syn::LitStr::new(&method_ident.to_string(), method_ident.span());
    let symbol_name = format!("pill_mirror_{type_ident}_{method_ident}");
    let symbol_ident = syn::Ident::new(&symbol_name, method_ident.span());
    let symbol_literal = syn::LitStr::new(&symbol_name, method_ident.span());
    let self_pointer = format_ident!("self_pointer");

    Ok(quote! {
        /// C-ABI trampoline for the mirrored method
        /// [`#type_ident::#method_ident`], generated by `#[pill_mirror_impl]`.
        ///
        /// # Safety
        ///
        /// `#self_pointer` must point at a live `#type_ident` value for the
        /// duration of the call; the C# runtime passes the pinned box of the
        /// struct it was invoked on.
        #[doc(hidden)]
        #[no_mangle]
        pub unsafe extern "C" fn #symbol_ident(
            #self_pointer: *const #type_ident
            #(, #arg_idents: #arg_types)*
        ) -> #return_type {
            // SAFETY: the host-side runtime guarantees the pointer points at a
            // live value of this type for the whole call.
            unsafe { (*#self_pointer).#method_ident(#(#arg_idents),*) }
        }

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillMethodDescriptor {
                type_name: #type_name,
                name: #method_name_literal,
                symbol: #symbol_literal,
                return_tag: #return_tag_literal,
                arg_tags: &[#(#arg_tag_literals),*],
            }
        }
    })
}

/// Tag a mirrored-method argument/return type from the closed vocabulary the
/// C# codegen understands; everything else is rejected.
fn mirror_method_type_tag(ty: &syn::Type) -> Result<String, String> {
    let syn::Type::Path(type_path) = ty else {
        return Err("unsupported mirrored-method type".to_string());
    };
    let Some(segment) = type_path.path.segments.last() else {
        return Err("unsupported mirrored-method type".to_string());
    };
    let name = segment.ident.to_string();
    match name.as_str() {
        "u8" | "u16" | "u32" | "u64" | "i8" | "i16" | "i32" | "i64" | "f32" | "f64"
        | "bool" | "usize" | "isize" => Ok(name),
        _ => Err(format!(
            "unsupported mirrored-method type `{name}`; use a primitive (u8..u64, i8..i64, f32, f64, bool, usize, isize)"
        )),
    }
}

// =============================================================================
// #[pill_hot]
// =============================================================================

/// Marks a system function as hot-patchable, so the host can replace its
/// implementation without re-registering it.
///
/// The function itself is emitted unchanged; the macro only adds a descriptor
/// carrying its fully-qualified path, its dispatch address and its signature
/// identity, submitted into this artifact's compile-time registry. Nothing has
/// to be listed by hand, exactly as with `#[derive(PillComponent)]`.
///
/// ```ignore
/// #[pill_hot]
/// fn movement_system(mut query: Query<(&mut Position, &Velocity)>) {
///     for (mut position, velocity) in query.iter_mut() {
///         position.x += velocity.x;
///     }
/// }
/// ```
///
/// The name the host patches by is `module_path!() + "::" + fn name`, matching
/// how `PillComponent` derives its type name.
///
/// The address and signature are computed through the function VALUE rather
/// than from its syntax: a concrete function-item type satisfies exactly one
/// arity of `SystemParamFunction`, so the parameter tuple is inferred. Rebuilding
/// that tuple from tokens would mean stripping patterns like `mut query` and
/// guessing elided lifetimes.
/// Supported argument:
/// - `#[pill_hot(name = "project::movement_system")]` — override the derived
///   qualified name. A generated patch library is its own crate, so
///   `module_path!()` there would report the patch's name rather than the
///   original's; the host passes the name it is patching so the two agree.
#[proc_macro_attribute]
pub fn pill_hot(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = &item_fn.sig.ident;

    // Parse the optional `name = "..."` override.
    let mut name_override: Option<String> = None;
    if !attribute.is_empty() {
        let parsed = syn::parse::<syn::MetaNameValue>(attribute);
        match parsed {
            Ok(meta) if meta.path.is_ident("name") => match &meta.value {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(literal),
                    ..
                }) => name_override = Some(literal.value()),
                other => {
                    return syn::Error::new_spanned(
                        other,
                        "`#[pill_hot(name = ...)]` expects a string literal",
                    )
                    .to_compile_error()
                    .into();
                }
            },
            Ok(meta) => {
                return syn::Error::new_spanned(
                    meta.path,
                    "unknown `pill_hot` argument; expected `name = \"...\"`",
                )
                .to_compile_error()
                .into();
            }
            Err(error) => return error.to_compile_error().into(),
        }
    }

    // Generic systems are not supported: the engine patches one concrete
    // monomorphization, so a generic function has no single address to swap.
    if !item_fn.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &item_fn.sig.generics,
            "`#[pill_hot]` cannot be applied to a generic function: hot patching \
             replaces one concrete implementation, and a generic has one per \
             instantiation",
        )
        .to_compile_error()
        .into();
    }

    let descriptor_fn = format_ident!("__pill_hot_descriptor_{}", fn_ident);
    let qualified_name = match &name_override {
        Some(name) => quote! { #name },
        None => quote! {
            ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#fn_ident))
        },
    };

    let expanded = quote! {
        #item_fn

        /// Resolves this function's dispatch address and signature identity for
        /// the artifact-wide hot-patch registry.
        #[allow(non_snake_case)]
        fn #descriptor_fn() -> (usize, u64) {
            (
                ::pill_engine::hot_patch::local_implementation_address(&#fn_ident),
                ::pill_engine::hot_patch::signature_hash_of(&#fn_ident),
            )
        }

        ::pill_engine::submit! {
            ::pill_engine::hot_patch::PillHotFunctionDescriptor {
                qualified_name: #qualified_name,
                resolve: #descriptor_fn,
            }
        }
    };

    expanded.into()
}
/// Arguments `#[pill_hot_fn]` accepts when it is generating a patch body.
///
/// A patch for an inherent method cannot copy the method into a free function,
/// because the body names `self`. It is instead placed in a LOCAL trait
/// implemented for the receiver type: a trait method has the same call shape as
/// the inherent one it replaces, so its address drops straight into the slot.
///
/// The host supplies both values; a developer never writes them.
#[derive(Default)]
struct PatchArguments {
    /// Registry name the generated descriptor is filed under.
    name: Option<String>,
    /// Concrete receiver type the local trait is implemented for.
    self_type: Option<syn::Type>,
}

/// Parse `name = "...", self_type = Path` from an attribute argument list.
///
/// An empty list is the ordinary case: the attribute is being applied by a
/// developer to their own function or method.
fn parse_patch_arguments(attribute: TokenStream) -> Result<PatchArguments, syn::Error> {
    let mut parsed = PatchArguments::default();
    if attribute.is_empty() {
        return Ok(parsed);
    }
    let attribute = proc_macro2::TokenStream::from(attribute);
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("name") {
            let value: syn::LitStr = meta.value()?.parse()?;
            parsed.name = Some(value.value());
            return Ok(());
        }
        if meta.path.is_ident("self_type") {
            parsed.self_type = Some(meta.value()?.parse()?);
            return Ok(());
        }
        Err(meta.error("expected `name` or `self_type`"))
    });
    syn::parse::Parser::parse2(parser, attribute)?;
    Ok(parsed)
}

/// Makes an ordinary function hot-patchable.
///
/// Use this for a plain `pub fn`; use `#[pill_hot]` for an ECS system. The two
/// differ because a system already has an indirection - the engine holds its
/// boxed closure and can swap what it calls - while an ordinary function is
/// called directly by its callers, so the indirection has to live inside the
/// function itself.
///
/// ```ignore
/// #[pill_hot_fn]
/// pub fn get_color_a() -> f32 {
///     133.0
/// }
/// ```
///
/// The real body is renamed and the public name becomes a dispatcher that reads
/// a slot, so every caller - including ones in other crates that linked this one
/// statically - goes through the redirect.
///
/// # A crate linked into several artifacts
///
/// The slot is a `static` in whichever artifact compiled the function. A crate
/// linked into both a module DLL and the project has an independent copy of its
/// code in each, so each copy has its own slot and must be patched separately.
/// The host installs into every loaded artifact that declares the name.
#[proc_macro_attribute]
pub fn pill_hot_fn(attribute: TokenStream, item: TokenStream) -> TokenStream {
    let patch = match parse_patch_arguments(attribute) {
        Ok(parsed) => parsed,
        Err(error) => return error.to_compile_error().into(),
    };
    let item_fn = parse_macro_input!(item as ItemFn);
    let signature = &item_fn.sig;
    let fn_ident = &signature.ident;
    let visibility = &item_fn.vis;
    let attributes = &item_fn.attrs;
    let body = &item_fn.block;

    // A generic function has one instantiation per set of type arguments, so
    // there is no single address a slot could hold.
    if !signature.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &signature.generics,
            concat!(
                "`#[pill_hot_fn]` cannot be applied to a generic function: ",
                "patching replaces one concrete implementation, and a generic ",
                "has one per instantiation"
            ),
        )
        .to_compile_error()
        .into();
    }

    // An inherent method is supported, and takes a different shape: its body
    // stays inline in the dispatcher rather than being hoisted into a function
    // of its own. Hoisting is impossible for a method, because every item
    // inside a method body is barred from naming `Self` (error E0401), and a
    // hoisted body would have to name the receiver type.
    let receiver = signature.receiver().cloned();

    // Rebuild the argument list: the dispatcher needs plain names to forward,
    // and a pattern like `mut value` or `(a, b)` cannot be forwarded as-is.
    let mut parameter_names = Vec::new();
    let mut parameter_declarations = Vec::new();
    let mut parameter_types = Vec::new();
    for (index, argument) in signature.inputs.iter().enumerate() {
        let syn::FnArg::Typed(typed) = argument else {
            continue;
        };
        let name = format_ident!("argument_{index}");
        let argument_type = &*typed.ty;
        parameter_names.push(quote! { #name });
        parameter_declarations.push(quote! { #name: #argument_type });
        parameter_types.push(quote! { #argument_type });
    }

    let return_type = &signature.output;
    let slot_ident = format_ident!("__PILL_HOT_SLOT_{}", fn_ident.to_string().to_uppercase());

    // The name a host addresses this function by. The receiver type is
    // deliberately absent: a method-level attribute cannot learn it, because
    // the descriptor is an item and items may not name `Self`. Two hot
    // functions sharing a name in one module therefore collide, which the
    // host source scanner detects and refuses with a clear message.
    let qualified_name = match &patch.name {
        // A generated patch is filed under the name the host asks for, which is
        // the running function's own path behind a prefix that cannot collide
        // with the copy the linked rlib also contributes.
        Some(name) => quote! { #name },
        None => quote! {
            ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#fn_ident))
        },
    };

    // The gate: the signature exactly as written, receiver included. A patch
    // derives the same text from the same source through this same shape, so a
    // reshaped function is refused rather than installed behind call sites
    // compiled for the old shape.
    let receiver_type = receiver.as_ref().map(|receiver| receiver.ty.clone());
    let mut signature_parts: Vec<proc_macro2::TokenStream> = Vec::new();
    if let Some(receiver_type) = &receiver_type {
        signature_parts.push(quote! { ::core::stringify!(#receiver_type), "," });
    }
    for parameter_type in &parameter_types {
        signature_parts.push(quote! { ::core::stringify!(#parameter_type), "," });
    }
    let signature_text = quote! {
        ::core::concat!("(", #(#signature_parts,)* ")", ::core::stringify!(#return_type))
    };

    // The pointer type an installed replacement is called through. A method
    // receiver is simply its first argument.
    let dispatch_type = match &receiver_type {
        Some(receiver_type) => {
            quote! { fn(#receiver_type #(, #parameter_types)*) #return_type }
        }
        None => quote! { fn(#(#parameter_types),*) #return_type },
    };
    let dispatch_arguments = match &receiver {
        Some(_) => quote! { self #(, #parameter_names)* },
        None => quote! { #(#parameter_names),* },
    };
    let declarations = match &receiver {
        Some(receiver) => quote! { #receiver #(, #parameter_declarations)* },
        None => quote! { #(#parameter_declarations),* },
    };

    // A generated patch for an inherent method. The body names `self`, so it
    // cannot be copied into a free function - but a LOCAL trait may be
    // implemented for a foreign type, and a trait method has the same call
    // shape as the inherent one it replaces: the receiver is simply its first
    // argument. So the body is carried verbatim into a trait implementation for
    // the concrete receiver type, and that method's address drops straight into
    // the running artifact's slot.
    //
    // No dispatcher is generated: nothing ever calls a patch through a slot of
    // its own. The signature text comes from the same computation the running
    // artifact used, which is what keeps the two comparable.
    if let Some(self_type) = &patch.self_type {
        if receiver.is_none() {
            return syn::Error::new_spanned(
                signature,
                "a `self_type` patch requires a method, but this function takes no receiver",
            )
            .to_compile_error()
            .into();
        }
        let slot_ident = format_ident!("__PILL_PATCH_SLOT_{}", fn_ident.to_string().to_uppercase());
        let address_ident = format_ident!("__pill_patch_address_{}", fn_ident);
        let expanded = quote! {
            /// The replacement body, in a local trait so it keeps using `self`.
            trait PillHotMethodPatch {
                fn #fn_ident(#declarations) #return_type;
            }

            impl PillHotMethodPatch for #self_type {
                #[inline(never)]
                fn #fn_ident(#declarations) #return_type #body
            }

            /// Address of the replacement, reported through the descriptor.
            #[doc(hidden)]
            fn #address_ident() -> usize {
                <#self_type as PillHotMethodPatch>::#fn_ident as *const () as usize
            }

            /// Unused here; a patch is never itself patched.
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static #slot_ident: ::pill_engine::hot_patch::PlainSlot =
                ::pill_engine::hot_patch::PlainSlot::new();

            ::pill_engine::submit! {
                ::pill_engine::hot_patch::PillHotSlotDescriptor {
                    qualified_name: #qualified_name,
                    slot: &#slot_ident,
                    signature: #signature_text,
                    implementation_address:
                        ::core::option::Option::Some(#address_ident as fn() -> usize),
                }
            }
        };
        return expanded.into();
    }

    // Only the slot machinery is conditional. The body is emitted exactly once,
    // unconditionally, so an optimized build compiles the function as written
    // and an editor never greys the source out as inactive code.
    let (implementation_item, implementation_address, fallback) = match &receiver {
        // A method keeps its body inline; there is nothing to address, and only
        // a patch - which names the receiver type concretely - ever needs one.
        Some(_) => (
            quote! {},
            quote! { ::core::option::Option::None },
            quote! { #body },
        ),
        // A free function hoists its body, so a patch built from this same
        // attribute has a symbol to report.
        None => {
            let implementation_ident = format_ident!("__pill_hot_impl_{}", fn_ident);
            let address_ident = format_ident!("__pill_hot_address_{}", fn_ident);
            let mut renamed = item_fn.clone();
            renamed.sig.ident = implementation_ident.clone();
            renamed.vis = syn::Visibility::Inherited;
            renamed.attrs.clear();
            (
                quote! {
                    /// The original body, renamed so the public name can dispatch.
                    ///
                    /// `inline(never)` only where its address is taken; an
                    /// optimized build folds it back into the caller.
                    #[doc(hidden)]
                    #[cfg_attr(debug_assertions, inline(never))]
                    #renamed

                    /// Address of the body above.
                    ///
                    /// A function rather than a constant because casting a fn
                    /// item to `usize` is not allowed while building a `static`.
                    #[doc(hidden)]
                    #[cfg(debug_assertions)]
                    fn #address_ident() -> usize {
                        #implementation_ident as *const () as usize
                    }
                },
                quote! { ::core::option::Option::Some(#address_ident as fn() -> usize) },
                quote! { #implementation_ident(#(#parameter_names),*) },
            )
        }
    };

    let expanded = quote! {
        #implementation_item

        #(#attributes)*
        #visibility fn #fn_ident(#declarations) #return_type {
            #[cfg(debug_assertions)]
            {
                /// Redirect slot for this function, private to this artifact.
                #[doc(hidden)]
                #[allow(non_upper_case_globals)]
                static #slot_ident: ::pill_engine::hot_patch::PlainSlot =
                    ::pill_engine::hot_patch::PlainSlot::new();

                ::pill_engine::submit! {
                    ::pill_engine::hot_patch::PillHotSlotDescriptor {
                        qualified_name: #qualified_name,
                        slot: &#slot_ident,
                        signature: #signature_text,
                        implementation_address: #implementation_address,
                    }
                }

                // One acquire load from a hot cache line, then a call. Measured
                // at under 0.2 ns against a direct call - below the noise floor.
                let installed = #slot_ident.installed();
                if installed != 0 {
                    // SAFETY: the slot holds an address accepted by
                    // `install_plain_function`, which refuses any whose
                    // signature text differs from the one recorded above. A
                    // replacement lives in a patch library the host never
                    // unloads, so it stays executable for the process lifetime.
                    let implementation: #dispatch_type =
                        unsafe { ::core::mem::transmute(installed) };
                    return implementation(#dispatch_arguments);
                }
            }
            #fallback
        }
    };

    expanded.into()
}

/// Emits the hot-patch resolver export on its own.
///
/// `#[pill_project]` and `#[pill_module]` already include it, so this exists for
/// artifacts that carry hot functions but neither entry point — in particular
/// the small patch libraries the host generates, which contain one edited
/// function and nothing else.
///
/// Takes an optional export name, defaulting to `pill_hot_resolve`:
///
/// ```ignore
/// pill_engine::pill_hot_resolver!();                    // pill_hot_resolve
/// pill_engine::pill_hot_resolver!(pill_patch_resolve);  // custom
/// ```
///
/// A generated patch **must** pass a different name. It links the project's
/// rlib to reach that crate's types and helpers, and that rlib already exports
/// `pill_hot_resolve`; two `#[no_mangle]` definitions of one symbol in a single
/// artifact is a linker error.
#[proc_macro]
pub fn pill_hot_resolver(item: TokenStream) -> TokenStream {
    let export_name = if item.is_empty() {
        format_ident!("pill_hot_resolve")
    } else {
        match syn::parse::<syn::Ident>(item) {
            Ok(identifier) => identifier,
            Err(error) => return error.to_compile_error().into(),
        }
    };
    hot_patch_resolver_export(&export_name, &quote! { #[cfg(debug_assertions)] }).into()
}

/// The exported resolver every loadable artifact provides.
///
/// One export rather than one per hot function, for the same reason the module
/// ABI keeps its surface small: a Windows DLL cannot exceed 65535 exports, and
/// a name-keyed lookup costs nothing at reload time.
fn hot_patch_resolver_export(
    export_name: &proc_macro2::Ident,
    gate: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    let install_name = format_ident!("{}_install", export_name);
    let plain_name = format_ident!("{}_plain", export_name);
    let reset_name = format_ident!("{}_reset", export_name);
    let address_name = format_ident!("{}_address", export_name);
    let coverage_name = format_ident!("{}_extent_coverage", export_name);
    quote! {
        /// SPIKE: report the address of ANY function in this artifact, by
        /// qualified path, from the build-script-generated inventory.
        ///
        /// Unlike the slot exports this needs no annotation on the function -
        /// discovery is mechanical - which is what makes prologue patching
        /// macro-free. Returns zero when this artifact has no such entry.
        ///
        /// # Safety
        ///
        /// `qualified_name` must be a valid NUL-terminated C string that stays
        /// readable for the call.
        #gate
        #[no_mangle]
        pub unsafe extern "C" fn #address_name(
            qualified_name: *const ::core::ffi::c_char,
            out_signature: *mut *const u8,
            out_signature_length: *mut usize,
        ) -> usize {
            if qualified_name.is_null() {
                return 0;
            }
            // SAFETY: the caller guarantees a NUL-terminated string readable
            // for the duration of this call.
            let name = unsafe { ::std::ffi::CStr::from_ptr(qualified_name) };
            let Ok(name) = name.to_str() else {
                return 0;
            };
            let Some(address) = ::pill_engine::hot_patch::function_address(name) else {
                return 0;
            };
            // The declaration this artifact was built with, so the caller can
            // refuse a replacement whose shape no longer matches.
            if let Some(signature) = ::pill_engine::hot_patch::function_signature(name) {
                if !out_signature.is_null() && !out_signature_length.is_null() {
                    // SAFETY: both pointers were checked non-null and the caller
                    // guarantees they address writable slots.
                    unsafe {
                        *out_signature = signature.as_ptr();
                        *out_signature_length = signature.len();
                    }
                }
            }
            address
        }

        /// How many of this artifact's functions have a length the exception
        /// directory records, packed as `(known << 32) | total`.
        ///
        /// A prologue patch can only overwrite a function whose extent is
        /// known, so this is what a host reports when one is refused: it
        /// separates "this function happens to be a leaf" from "nothing in this
        /// artifact is reachable by that route".
        #gate
        #[no_mangle]
        pub extern "C" fn #coverage_name() -> u64 {
            let (known, total) = ::pill_engine::hot_patch::functions_with_known_extent();
            ((known as u64) << 32) | (total as u64 & 0xFFFF_FFFF)
        }

        /// Return a `#[pill_hot_fn]` declared in THIS artifact to its own body.
        ///
        /// The counterpart of the install export, used to roll a patch back to
        /// generation zero. A plain function has no single baseline address a
        /// host could reinstall - every artifact linking the crate holds its own
        /// copy - so each artifact is asked to empty its own slot instead.
        ///
        /// Returns 0 on success and 1 when this artifact declares no such
        /// function.
        ///
        /// # Safety
        ///
        /// `qualified_name` must be a valid NUL-terminated C string that stays
        /// readable for the call.
        #gate
        #[no_mangle]
        pub unsafe extern "C" fn #reset_name(
            qualified_name: *const ::core::ffi::c_char,
        ) -> u32 {
            if qualified_name.is_null() {
                return 1;
            }
            // SAFETY: the caller guarantees a NUL-terminated string readable
            // for the duration of this call.
            let name = unsafe { ::std::ffi::CStr::from_ptr(qualified_name) };
            let Ok(name) = name.to_str() else {
                return 1;
            };
            match ::pill_engine::hot_patch::reset_plain_function(name) {
                Ok(()) => 0,
                Err(_) => 1,
            }
        }

        /// Report where a `#[pill_hot_fn]` declared in THIS artifact lives.
        ///
        /// Returns the implementation's address, or zero when this artifact
        /// declares no such function. On success the signature text is written
        /// through `out_signature` and `out_signature_length` as a pointer and
        /// byte count, because the text comes from `concat!` and is therefore
        /// not NUL-terminated.
        ///
        /// # Safety
        ///
        /// `qualified_name` must be a valid NUL-terminated C string readable
        /// for the call. `out_signature` and `out_signature_length` must be
        /// null or point at writable slots. The reported signature borrows
        /// static storage inside this artifact and stays valid while it is
        /// loaded.
        #gate
        #[no_mangle]
        pub unsafe extern "C" fn #plain_name(
            qualified_name: *const ::core::ffi::c_char,
            out_signature: *mut *const u8,
            out_signature_length: *mut usize,
        ) -> usize {
            if qualified_name.is_null() {
                return 0;
            }
            // SAFETY: the caller guarantees a NUL-terminated string readable
            // for the duration of this call.
            let name = unsafe { ::std::ffi::CStr::from_ptr(qualified_name) };
            let Ok(name) = name.to_str() else {
                return 0;
            };
            match ::pill_engine::hot_patch::plain_function_entry(name) {
                Some((address, signature)) => {
                    if !out_signature.is_null() && !out_signature_length.is_null() {
                        // SAFETY: both pointers were checked non-null and the
                        // caller guarantees they address writable slots.
                        unsafe {
                            *out_signature = signature.as_ptr();
                            *out_signature_length = signature.len();
                        }
                    }
                    address
                }
                None => 0,
            }
        }

        /// Redirect a `#[pill_hot_fn]` declared in THIS artifact.
        ///
        /// A crate linked into several artifacts has an independent copy of its
        /// code in each, so the host calls this on every loaded artifact rather
        /// than assuming one of them owns the function.
        ///
        /// Returns 0 on success, 1 when this artifact declares no such function,
        /// and 2 when the signature no longer matches. A non-zero result always
        /// means the running implementation was left untouched.
        ///
        /// # Safety
        ///
        /// `qualified_name` and `signature` must be valid NUL-terminated C
        /// strings that stay readable for the call, and `address` must point at
        /// a function with the signature `signature` describes, in a library
        /// that outlives the process's use of it.
        #gate
        #[no_mangle]
        pub unsafe extern "C" fn #install_name(
            qualified_name: *const ::core::ffi::c_char,
            address: usize,
            signature: *const ::core::ffi::c_char,
        ) -> u32 {
            if qualified_name.is_null() || signature.is_null() {
                return 1;
            }
            // SAFETY: the caller guarantees NUL-terminated strings readable for
            // the duration of this call.
            let (name, signature) = unsafe {
                (
                    ::std::ffi::CStr::from_ptr(qualified_name),
                    ::std::ffi::CStr::from_ptr(signature),
                )
            };
            let (Ok(name), Ok(signature)) = (name.to_str(), signature.to_str()) else {
                return 1;
            };
            match ::pill_engine::hot_patch::install_plain_function(name, address, signature) {
                Ok(()) => 0,
                Err(::pill_engine::hot_patch::HotPatchError::UnknownSystem { .. }) => 1,
                Err(_) => 2,
            }
        }

        /// Resolves a `#[pill_hot]` function's dispatch address by qualified
        /// name, writing its signature hash through `out_signature_hash`.
        ///
        /// Returns zero when this artifact declares no such function, in which
        /// case `out_signature_hash` is left untouched.
        ///
        /// # Safety
        ///
        /// `qualified_name` must be a valid NUL-terminated C string that stays
        /// readable for the call. `out_signature_hash` must be null or point at
        /// a writable `u64`.
        #gate
        #[no_mangle]
        pub unsafe extern "C" fn #export_name(
            qualified_name: *const ::core::ffi::c_char,
            out_signature_hash: *mut u64,
        ) -> usize {
            if qualified_name.is_null() {
                return 0;
            }
            // SAFETY: the caller guarantees a NUL-terminated string readable
            // for the duration of this call.
            let name = unsafe { ::std::ffi::CStr::from_ptr(qualified_name) };
            let Ok(name) = name.to_str() else {
                return 0;
            };
            match ::pill_engine::hot_patch::resolve_hot_function(name) {
                Some((address, signature_hash)) => {
                    if !out_signature_hash.is_null() {
                        // SAFETY: the caller guarantees a writable `u64` when
                        // the pointer is non-null.
                        unsafe { *out_signature_hash = signature_hash };
                    }
                    address
                }
                None => 0,
            }
        }
    }
}

// =============================================================================
// #[pill_module]
// =============================================================================

/// Wraps an optional module's `register` function and generates the
/// `pill_module_*` C-ABI exports.
///
/// The annotated function must have the signature
/// `fn(engine: &mut Engine) -> u32`. The macro:
///
/// - auto-registers every component declared with `#[derive(PillComponent)]`
///   in this artifact before calling the wrapped function;
/// - derives the module name from `CARGO_PKG_NAME` (null-terminated) instead
///   of a hand-written constant;
/// - reads the ABI version from `::pill_engine::module_abi::MODULE_ABI_VERSION`
///   so the module and host can never drift;
/// - wraps the whole init in `catch_unwind` so a panic becomes a non-zero
///   status and the host rolls back instead of unwinding across the C ABI.
///
/// Everything generated is gated behind `#[cfg(feature = "module-abi")]`, and
/// the same gate is applied to the wrapped function, so a crate linked into
/// the project build (where the feature is off) exports no `#[no_mangle]`
/// symbols and leaves no dead code behind.
#[proc_macro_attribute]
pub fn pill_module(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = &item_fn.sig.ident;

    // Gated twice over. `module-abi`, because a crate linked directly into
    // another binary must not export these `#[no_mangle]` symbols twice.
    // `debug_assertions`, because hot patching is a development facility and a
    // shipped artifact should carry no trace of it.
    let hot_patch_resolver = hot_patch_resolver_export(
        &format_ident!("pill_hot_resolve"),
        &quote! { #[cfg(all(feature = "module-abi", debug_assertions))] },
    );

    let expanded = quote! {
        // Emitted unconditionally. What `module-abi` gates is the `#[no_mangle]`
        // exports below: those must stay off when this crate is linked as an
        // ordinary dependency, or one symbol ends up with two definitions. The
        // user's own function has no such problem, and a statically linked
        // build calls it directly - without this it would not be compiled at
        // all, and a shipping binary would have no way to initialize the module
        // it just linked in.
        #item_fn

        #hot_patch_resolver

        /// Optional-module ABI revision this crate was built against.
        #[cfg(feature = "module-abi")]
        const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

        /// Name reported to the host for diagnostics; null-terminated for the
        /// C ABI. Derived from the crate name so it can never drift from the
        /// package the host builds.
        #[cfg(feature = "module-abi")]
        const PILL_MODULE_NAME: &[u8] =
            ::core::concat!(::core::env!("CARGO_PKG_NAME"), "\0").as_bytes();

        /// Module ABI revision, checked by the host before anything else is
        /// called.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub extern "C" fn pill_module_abi_version() -> u32 {
            PILL_MODULE_ABI_VERSION
        }

        /// Human-readable module name used in host log messages.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
            PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
        }

        /// Number of `#[derive(PillMirror)]` value-type descriptors this
        /// artifact declares, letting the host size its copy buffer.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub extern "C" fn pill_value_type_descriptor_count() -> u32 {
            ::pill_engine::component_registry::value_type_descriptors().len() as u32
        }

        /// Copy up to `max` value-type descriptors into `out`; returns the
        /// count actually copied. The descriptors (and their inner field
        /// slices) point into this artifact's static data, which stays mapped
        /// for the artifact's lifetime.
        ///
        /// # Safety
        ///
        /// `out` must point at `max` writable
        /// [`PillValueTypeDescriptor`](::pill_engine::component_registry::PillValueTypeDescriptor)
        /// slots owned by the host for the duration of this call.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub unsafe extern "C" fn pill_copy_value_type_descriptors(
            out: *mut ::pill_engine::component_registry::PillValueTypeDescriptor,
            max: u32,
        ) -> u32 {
            let descriptors = ::pill_engine::component_registry::value_type_descriptors();
            let count = (descriptors.len() as u32).min(max);
            for (index, descriptor) in descriptors.iter().take(count as usize).enumerate() {
                // SAFETY: `index < count <= max`, so `out.add(index)` stays
                // inside the buffer the host promised, and
                // `PillValueTypeDescriptor` is `Copy`.
                unsafe { out.add(index).write(**descriptor); }
            }
            count
        }

        /// Number of `#[pill_mirror_method]` descriptors this artifact
        /// declares, letting the host size its copy buffer.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub extern "C" fn pill_mirror_method_descriptor_count() -> u32 {
            ::pill_engine::component_registry::mirror_method_descriptors().len() as u32
        }

        /// Copy up to `max` mirrored-method descriptors into `out`; returns
        /// the count actually copied. Each descriptor names a `#[no_mangle]`
        /// trampoline this artifact exports, which the host resolves by symbol
        /// to obtain the callable address.
        ///
        /// # Safety
        ///
        /// `out` must point at `max` writable
        /// [`PillMethodDescriptor`](::pill_engine::component_registry::PillMethodDescriptor)
        /// slots owned by the host for the duration of this call.
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub unsafe extern "C" fn pill_copy_mirror_method_descriptors(
            out: *mut ::pill_engine::component_registry::PillMethodDescriptor,
            max: u32,
        ) -> u32 {
            let descriptors = ::pill_engine::component_registry::mirror_method_descriptors();
            let count = (descriptors.len() as u32).min(max);
            for (index, descriptor) in descriptors.iter().take(count as usize).enumerate() {
                // SAFETY: `index < count <= max`, so `out.add(index)` stays
                // inside the buffer the host promised, and
                // `PillMethodDescriptor` is `Copy`.
                unsafe { out.add(index).write(**descriptor); }
            }
            count
        }

        /// Registers the module against the host engine; returns zero on
        /// success.
        ///
        /// # Safety
        ///
        /// `api` must be a valid [`EngineApi`] pointer owned by the host and
        /// kept alive for the whole duration of this call.
        ///
        /// [`EngineApi`]: ::pill_engine::EngineApi
        #[cfg(feature = "module-abi")]
        #[no_mangle]
        pub unsafe extern "C" fn pill_module_init(api: *const ::pill_engine::EngineApi) -> u32 {
            // A panic must never unwind across the C ABI boundary, so it is
            // converted into a non-zero status and the host keeps the previous
            // generation. `catch_unwind` lives in `std::panic` (not `core`),
            // because unwinding is a std-level feature.
            let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                // SAFETY: The host guarantees `api` points at a live `EngineApi`
                // whose `engine_handle` addresses the single engine instance,
                // and that both outlive this call. The engine is not otherwise
                // borrowed while a module initializes, so the reconstructed
                // `&mut Engine` is unique.
                let api = unsafe { &*api };
                let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
                if let Err(_error) =
                    ::pill_engine::component_registry::register_all_components(engine.world_mut())
                {
                    // The engine logged the first-class diagnostic; fail the
                    // init so the host rolls the reload back instead of running
                    // with a half-registered component set.
                    return u32::MAX;
                }
                #fn_ident(engine)
            }));
            result.unwrap_or(u32::MAX)
        }
    };

    expanded.into()
}

// =============================================================================
// #[pill_project]
// =============================================================================

/// Wraps a project's `init` function and generates the project-module ABI
/// exports (`project_init`, `project_update`, `project_schema_fingerprint`).
///
/// The annotated function must have the signature
/// `fn(engine: &mut Engine) -> u32`. The macro auto-registers every component
/// declared with `#[derive(PillComponent)]` in this artifact before calling
/// the wrapped function, and generates the schema fingerprint from the same
/// registry, so adding a component can never leave the fingerprint stale.
#[proc_macro_attribute]
pub fn pill_project(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = &item_fn.sig.ident;

    // Debug-only: a released project keeps no hot-patching surface.
    let hot_patch_resolver = hot_patch_resolver_export(
        &format_ident!("pill_hot_resolve"),
        &quote! { #[cfg(debug_assertions)] },
    );

    let expanded = quote! {
        #item_fn

        #hot_patch_resolver

        /// Registers the project's components, resources, and systems; returns
        /// zero on success.
        ///
        /// # Safety
        ///
        /// `api` must be a valid [`EngineApi`] pointer owned by the host for
        /// the complete duration of this call.
        ///
        /// [`EngineApi`]: ::pill_engine::EngineApi
        #[no_mangle]
        pub unsafe extern "C" fn project_init(api: *const ::pill_engine::EngineApi) -> u32 {
            // A panic must never unwind across the C ABI boundary, so it is
            // converted into a non-zero status and the host keeps the previous
            // generation. `catch_unwind` lives in `std::panic` (not `core`),
            // because unwinding is a std-level feature.
            let result = ::std::panic::catch_unwind(::std::panic::AssertUnwindSafe(|| {
                // SAFETY: The host guarantees `api` points at a live `EngineApi`
                // whose `engine_handle` addresses the single engine instance,
                // and that both outlive this call. The engine is not otherwise
                // borrowed while a project initializes, so the reconstructed
                // `&mut Engine` is unique.
                let api = unsafe { &*api };
                let engine = unsafe { &mut *(api.engine_handle as *mut ::pill_engine::Engine) };
                if let Err(_error) =
                    ::pill_engine::component_registry::register_all_components(engine.world_mut())
                {
                    // The engine logged the first-class diagnostic; fail the
                    // init so the host rolls the reload back instead of running
                    // with a half-registered component set.
                    return u32::MAX;
                }
                #fn_ident(engine)
            }));
            result.unwrap_or(u32::MAX)
        }

        /// Optional per-frame hook; gameplay is executed entirely by
        /// scheduler-managed ECS systems.
        #[no_mangle]
        pub extern "C" fn project_update(_api: *const ::pill_engine::EngineApi) {}

        /// Aggregate schema fingerprint of every persistable component,
        /// computed from the compile-time registry.
        #[no_mangle]
        pub extern "C" fn project_schema_fingerprint() -> u64 {
            ::pill_engine::component_registry::persistable_schema_fingerprint()
        }
    };

    expanded.into()
}
