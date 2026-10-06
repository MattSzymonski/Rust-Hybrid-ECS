//! Procedural macros that remove the error-prone registration and FFI
//! boilerplate from extension and project crates.
//!
//! # Responsibilities
//!
//! - [`derive(PillComponent)`] turns one component type into everything the
//!   engine needs to know about it: the [`Component`] impl and a descriptor
//!   submitted into this artifact's compile-time registry. Persistable components (those marked
//!   `#[pill(persistable)]`) are additionally registered for schema migration,
//!   and they drive the aggregate project schema fingerprint — no hand-written
//!   registration list or fingerprint hash to keep in sync.
//! - [`derive(PillLayout)`] emits just the compile-time field layout for a
//!   type that registers itself explicitly and must not auto-register (engine
//!   types, host-linked extensions): `offset_of!`/`size_of!`/`align_of!`
//!   descriptors with no `Component` impl, no accessors and no inventory
//!   submission.
//! - [`pill_value_type!`] declares a foreign value type - one whose definition
//!   lives in another crate, so no derive can reach it - for the managed
//!   mirror: the same descriptors `#[derive(PillMirror)]` would submit, with
//!   every offset taken from the compiler.
//! - [`attribute(PillModule)`] wraps an extension's `register` function
//!   and generates the `pill_module_*` C-ABI exports (version, name, init) with
//!   the panic guard and engine-pointer reconstruction that every module
//!   otherwise hand-writes.
//! - [`attribute(PillProject)`] does the same for the project ABI
//!   (`pill_module_init`, `pill_module_abi_version`,
//!   `project_schema_fingerprint`).
//! - [`attribute(PillMirrorFn)`] mirrors a free function to C# as a static
//!   method on a class named after its declaring module, the free-function
//!   counterpart of `#[pill_mirror_impl]`'s instance methods.
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
/// - a descriptor submitted into this artifact's compile-time registry
///
/// Supported helper attributes:
/// - `#[pill(persistable)]` - the component is schema-migrated across reloads
///   (requires `Serialize + DeserializeOwned + Default`, matching
///   [`World::register_persistable_component`]).
/// - `#[pill(shared)]` - the component keeps one identity across every binary
///   that links it, instead of a separate one per binary. Use it for a type
///   more than one artifact names directly: without it each binary gets its
///   own `TypeId`, hence its own column, and neither can see the other's
///   entities. The identity is derived from `module_path!()::TypeName`;
///   `#[pill(shared = "some::other::Name")]` overrides that when a type has
///   moved between modules and the old identity must be kept. The name must be
///   unique process-wide, so it should stay namespaced.
///
/// Supported field types:
/// - blittable values — primitives, fixed-size arrays, `#[derive(PillMirror)]`
///   structs — become typed C# fields on the generated mirror;
/// - `String` and `Vec<E>` (with `E` a primitive or a `#[derive(PillMirror)]`
///   struct) are Rust-owned container fields. They mirror as accessor members
///   (a span over the live buffer, a count, a resize; get/set for text) that
///   call derive-generated C-ABI trampolines, so managed code reads and writes
///   the real container in place. Resizing a `Vec` field needs `E: Default +
///   Clone`, which the supported element types all satisfy;
/// - `Vec<String>` is supported too, through per-element accessors: its
///   elements are separately allocated, so managed code gets `Count`, `GetX`,
///   `SetX`, `PushX` and `ResizeX` (one boundary call per element) instead of
///   a span;
/// - `DynamicBuffer<E>` is the engine-owned container: its elements live in
///   native memory with a stable address, the mirror reads the `(ptr, len,
///   cap)` handle straight out of the row (iterating costs zero calls), and
///   only resizing - which retires the block - calls native code. `E` must be
///   a primitive or a `#[derive(PillMirror)]` struct, and `Copy`, because the
///   buffer never runs element drop glue.
///
/// [`World::register_persistable_component`]: ::pill_engine::World::register_persistable_component
#[proc_macro_derive(PillComponent, attributes(pill))]
pub fn derive_pill_component(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;

    // Parse the `#[pill(...)]` helper attribute.
    let mut persistable = false;
    let mut shared = false;
    let mut shared_name_override: Option<String> = None;
    for attribute in &input.attrs {
        if attribute.path().is_ident("pill") {
            if let Err(error) = attribute.parse_nested_meta(|meta| {
                if meta.path.is_ident("persistable") {
                    persistable = true;
                    Ok(())
                } else if meta.path.is_ident("shared") {
                    shared = true;
                    // `shared` on its own derives the name from the module
                    // path; `shared = "..."` pins it explicitly.
                    if meta.input.peek(syn::Token![=]) {
                        let literal: syn::LitStr = meta.value()?.parse()?;
                        let name = literal.value();
                        if name.is_empty() {
                            return Err(syn::Error::new_spanned(
                                &literal,
                                "a shared component name cannot be empty",
                            ));
                        }
                        // A shared name is a process-wide identity: two types
                        // holding it become one component, one column, and one
                        // set of rows. When their layouts also happen to agree
                        // nothing downstream can notice, so the guard has to be
                        // here, before the collision is expressible. Requiring
                        // a path separator makes an accidental `"Transform"`
                        // a compile error while leaving a deliberate, qualified
                        // name - the only way two artifacts legitimately share
                        // one - untouched.
                        if !name.contains("::") {
                            return Err(syn::Error::new_spanned(
                                &literal,
                                format!(
                                    "shared component name `{name}` is not namespaced. A shared \
                                     name is a process-wide identity, so an unrelated crate \
                                     declaring `{name}` too would bind to this component's \
                                     column and read its rows. Qualify it (`my_crate::{name}`), \
                                     or drop the argument to derive it from `module_path!()`."
                                ),
                            ));
                        }
                        shared_name_override = Some(name);
                    }
                    Ok(())
                } else {
                    Err(meta.error("unknown `pill` attribute; expected `persistable` or `shared`"))
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

    // Capture the compile-time field layout for the C# mirror codegen, plus
    // the accessor trampolines that let managed code reach a `Vec` or `String`
    // field's live buffer. An unsupported field type is a compile error here,
    // before the host would have to guess at a mirror.
    let (declared_layout, layout_reference, field_accessors) =
        match component_field_descriptors(&input, ident, true, true) {
            Ok(triple) => triple,
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

    // A shared component reports a stable name that every binary linking it
    // computes identically; an ordinary one reports nothing and keeps the
    // default per-binary `TypeId` identity.
    let shared_name_impl = if shared {
        let shared_name = match &shared_name_override {
            Some(name) => quote! { #name },
            None => type_name.clone(),
        };
        quote! {
            fn shared_name() -> ::core::option::Option<&'static str> {
                ::core::option::Option::Some(#shared_name)
            }

            fn shared_identity() -> ::core::option::Option<u128> {
                // A `const` item, not the trait's default: that would hash the
                // name on every `ComponentId::of` call, which is measurable on
                // the per-call random-access path. Bound here, it is folded at
                // compile time.
                const IDENTITY: u128 =
                    ::pill_engine::component::shared_component_identity(#shared_name);
                ::core::option::Option::Some(IDENTITY)
            }
        }
    } else {
        quote! {}
    };

    let expanded = quote! {
        impl ::pill_engine::Component for #ident {
            #shared_name_impl

            fn declared_schema_hash() -> ::core::option::Option<u64> {
                // The same descriptors registration hashes, so a type agrees
                // with its own registration by construction. An empty layout
                // declares nothing, matching `ComponentLayout::of`.
                let fields: &[::pill_engine::component_registry::ComponentFieldDescriptor] =
                    #layout_reference;
                (!fields.is_empty())
                    .then(|| ::pill_engine::component::component_schema_hash(fields))
            }
        }
        #declared_layout

        #field_accessors

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

// =============================================================================
// #[derive(PillLayout)]
// =============================================================================

/// Emits a type's compile-time field layout without any of the component
/// registration machinery.
///
/// Generates the same descriptor list as [`derive(PillComponent)`] - built
/// with `offset_of!`/`size_of!`/`align_of!` and the same tag vocabulary - as
/// an inherent `FIELD_LAYOUT` const plus a [`ComponentLayout`] impl. Use it
/// for a type that registers itself explicitly: an engine type, a host-linked
/// extension, anything that must not submit an inventory registration into
/// every artifact that links it.
///
/// Nested struct fields are tagged `struct:<path>` and flattened into dotted
/// leaf rows by the world at registration, provided the nested type's own
/// layout is registered first.
///
/// [`ComponentLayout`]: ::pill_engine::component_registry::ComponentLayout
#[proc_macro_derive(PillLayout)]
pub fn derive_pill_layout(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);

    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &input.generics,
            "`PillLayout` cannot be derived for a generic type",
        )
        .to_compile_error()
        .into();
    }

    // The accessor half is dropped: a layout-only type has no registration
    // that could carry heap-field trampolines, and a container field stays
    // read-only through the generic field path either way.
    match component_field_descriptors(&input, &input.ident, true, true) {
        Ok((declared_layout, _layout_reference, _accessors)) => declared_layout.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// The inherent `FIELD_LAYOUT` const and `ComponentLayout` impl every
/// layout-carrying derive emits, both naming the same descriptor slice.
fn layout_emission(
    ident: &syn::Ident,
    layout_reference: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        impl #ident {
            /// Compile-time field layout of this type, generated by the derive
            /// macro; pass it to `World::register_component_with_layout`.
            pub const FIELD_LAYOUT: &'static [::pill_engine::component_registry::ComponentFieldDescriptor] = #layout_reference;
        }

        impl ::pill_engine::component_registry::ComponentLayout for #ident {
            const FIELDS: &'static [::pill_engine::component_registry::ComponentFieldDescriptor] = #layout_reference;
        }
    }
}

/// The layout emission for a type with no named fields (unit struct, tuple
/// struct, enum, union), so the const and trait impl exist on every derived
/// type.
fn empty_field_layout(ident: &syn::Ident) -> proc_macro2::TokenStream {
    layout_emission(ident, &quote! { &[] })
}

/// Emit a `static` carrying one [`ComponentFieldDescriptor`] per named field,
/// the expression that names it, and the heap-field accessor machinery when
/// the struct declares container fields.
///
/// `self_type` is the path every offset is taken through: the derived type's
/// own name for the derives, or the declared path when `pill_value_type!`
/// describes a foreign type. `allow_containers` is `true` for
/// `#[derive(PillComponent)]`, whose values live in native columns that
/// managed code may reach through generated accessors, and `false` for
/// `#[derive(PillMirror)]` value types, which cross the boundary as plain data
/// and therefore cannot own a heap buffer. `emit_layout_consts` is `false` for
/// a foreign type, which can carry neither an inherent const nor a trait impl.
///
/// Enums, unions, unit structs and tuple structs have no named fields to
/// mirror and yield an empty layout.
fn component_field_descriptors(
    input: &DeriveInput,
    self_type: &dyn ToTokens,
    allow_containers: bool,
    emit_layout_consts: bool,
) -> syn::Result<(
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
    proc_macro2::TokenStream,
)> {
    let ident = &input.ident;
    let named = match &input.data {
        syn::Data::Struct(data) => match &data.fields {
            syn::Fields::Named(named) => &named.named,
            _ => return Ok((empty_field_layout(ident), quote! { &[] }, quote! {})),
        },
        _ => return Ok((empty_field_layout(ident), quote! { &[] }, quote! {})),
    };
    if named.is_empty() {
        return Ok((empty_field_layout(ident), quote! { &[] }, quote! {}));
    }

    let type_name = quote! {
        ::core::concat!(::core::module_path!(), "::", ::core::stringify!(#ident))
    };

    let mut entries = Vec::with_capacity(named.len());
    let mut accessors: Vec<proc_macro2::TokenStream> = Vec::new();
    for field in named {
        let field_ident = field.ident.as_ref().expect("named field has an identifier");
        let field_ty = &field.ty;
        let field_name = field_ident.to_string();
        let type_tag = field_type_tag(field_ty, allow_containers)?;
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
                offset: ::core::mem::offset_of!(#self_type, #field_ident),
                size: ::core::mem::size_of::<#field_ty>(),
                align: ::core::mem::align_of::<#field_ty>(),
                element_count: #element_count,
            }
        });
        // A container field additionally gets one accessor per supported
        // operation, so a managed mirror iterates the live buffer instead of
        // copying the row's 24-byte header somewhere else.
        if let Some(kind) = heap_field_accessor(&type_tag) {
            accessors.push(emit_heap_field_accessor(
                ident,
                field_ident,
                &field_name,
                &type_name,
                kind,
            ));
        }
    }

    let static_name = format_ident!("__PILL_FIELD_LAYOUT_{}", ident);
    // A foreign type can carry neither an inherent const nor the
    // `ComponentLayout` impl (both are orphan-rule violations), so its
    // declaration emits the descriptor list alone.
    let layout_consts = if emit_layout_consts {
        layout_emission(ident, &quote! { #static_name })
    } else {
        quote! {}
    };
    let declared = quote! {
        /// Compile-time field layout of `#ident`, consumed by the C# mirror
        /// codegen. Do not reference; generated by `#[derive(...)]`.
        #[doc(hidden)]
        #[allow(non_upper_case_globals)]
        static #static_name: &[::pill_engine::component_registry::ComponentFieldDescriptor] = &[ #(#entries),* ];

        #layout_consts
    };
    Ok((declared, quote! { #static_name }, quote! { #(#accessors)* }))
}

/// What the managed side needs to know about one heap-owning field.
enum HeapFieldKind {
    /// A `Vec<E>` field; the element tag drives the generated span type.
    Vec {
        /// Element type tag (`f32`, `struct:<path>`, ...).
        element_tag: String,
    },
    /// A `DynamicBuffer<E>` field: elements in engine-owned native memory,
    /// whose `(ptr, len, cap)` handle the mirror reads straight out of the
    /// row.
    DynamicBuffer {
        /// Element type tag (`f32`, `struct:<path>`, ...).
        element_tag: String,
    },
    /// A `Vec<String>` field: elements are individually allocated, so managed
    /// code reaches each one through its own accessor instead of a span.
    VecString,
    /// A `String` field, exposed to C# as UTF-8 text.
    String,
}

/// Recognise a container tag the derive emitted, or `None` for every other
/// tag in the vocabulary.
fn heap_field_accessor(type_tag: &str) -> Option<HeapFieldKind> {
    // `vec:string` is checked before the `vec:` prefix below, because its
    // accessor set is per element rather than a span over the run.
    if type_tag == "vec:string" {
        return Some(HeapFieldKind::VecString);
    }
    if let Some(element_tag) = type_tag.strip_prefix("vec:") {
        return Some(HeapFieldKind::Vec {
            element_tag: element_tag.to_string(),
        });
    }
    if let Some(element_tag) = type_tag.strip_prefix("dynbuf:") {
        return Some(HeapFieldKind::DynamicBuffer {
            element_tag: element_tag.to_string(),
        });
    }
    (type_tag == "string").then_some(HeapFieldKind::String)
}

/// Emit the accessor trampolines and the descriptor submission for one
/// heap-owning field.
///
/// Each trampoline receives the address of the live component value (the row a
/// managed query is iterating): the view answers `(data, length)` for every
/// container, resize grows or shrinks a `Vec` or a `DynamicBuffer`, and set
/// replaces a `String`'s contents from UTF-8 bytes. Nothing here serializes or
/// copies a buffer; the managed side reads and writes elements through the
/// pointer it is handed.
fn emit_heap_field_accessor(
    type_ident: &syn::Ident,
    field_ident: &syn::Ident,
    field_name: &str,
    type_name: &proc_macro2::TokenStream,
    kind: HeapFieldKind,
) -> proc_macro2::TokenStream {
    let view_symbol = format!("pill_accessor_{type_ident}_{field_ident}_view");
    let view_fn = syn::Ident::new(&view_symbol, field_ident.span());

    // A `Vec<String>` has no contiguous element run to point at: each element
    // is its own allocation, so its view reports the count and a null data
    // pointer, and the elements travel through their own trampolines below.
    let view_data = match &kind {
        HeapFieldKind::VecString => quote! {
            *out_data = ::core::ptr::null();
        },
        _ => quote! {
            *out_data = values.as_ptr().cast::<u8>();
        },
    };

    let view = quote! {
        /// View trampoline for the `#field_ident` field, generated by
        /// `#[derive(PillComponent)]`.
        ///
        /// Writes what one borrowed view of the field can offer: the element
        /// address and count for a contiguous run, or a null address and the
        /// count for a `Vec<String>`, whose elements are separately allocated
        /// and have no span. Returns 0, or returns 1 when a pointer is null.
        /// The buffer stays owned by the component: the caller may read (and,
        /// for a `Vec`, write) elements until the container is structurally
        /// modified.
        ///
        /// # Safety
        ///
        /// `row` must point at a live `#type_ident` value, and both out
        /// pointers must be writable.
        #[doc(hidden)]
        #[no_mangle]
        pub unsafe extern "C" fn #view_fn(
            row: *const u8,
            out_data: *mut *const u8,
            out_length: *mut usize,
        ) -> u8 {
            if row.is_null() || out_data.is_null() || out_length.is_null() {
                return 1;
            }
            // SAFETY: the caller guarantees `row` addresses a live component
            // value, so the shared reference is valid for the whole call.
            let component = unsafe { &*(row as *const #type_ident) };
            let values = &component.#field_ident;
            // SAFETY: both out pointers were checked non-null above.
            unsafe {
                #view_data
                *out_length = values.len();
            }
            0
        }
    };

    let mut resize = quote! {};
    let mut set = quote! {};
    let mut item = quote! {};
    let mut set_item = quote! {};
    let mut push = quote! {};
    let mut resize_symbol = String::new();
    let mut set_symbol = String::new();
    let mut item_symbol = String::new();
    let mut set_item_symbol = String::new();
    let mut push_symbol = String::new();
    let (kind_tag, element_tag) = match &kind {
        HeapFieldKind::Vec { element_tag } => {
            resize_symbol = format!("pill_accessor_{type_ident}_{field_ident}_resize");
            let resize_fn = syn::Ident::new(&resize_symbol, field_ident.span());
            resize = quote! {
                /// Resize trampoline for the `#field_ident` field, generated by
                /// `#[derive(PillComponent)]`.
                ///
                /// Rewrites the vector to `new_length` elements and returns 0,
                /// or returns 1 when `row` is null. Growing fills the new
                /// elements with the element type's `Default`. Existing
                /// elements keep their values; the buffer may move, so a view
                /// taken before the call must not be used afterwards.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to.
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #resize_fn(row: *mut u8, new_length: usize) -> u8 {
                    if row.is_null() {
                        return 1;
                    }
                    // SAFETY: the caller guarantees `row` addresses a live
                    // component value and holds the write declaration the
                    // mutation needs, so the exclusive reference is valid for
                    // the whole call.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    // `resize` needs `E: Default + Clone`. The derive only
                    // accepts a primitive or a `#[derive(PillMirror)]` struct
                    // as `E`, and both are, so the bound is satisfied by the
                    // field's element type rather than by the component's.
                    component.#field_ident.resize(
                        new_length,
                        ::core::default::Default::default(),
                    );
                    0
                }
            };
            ("vec", element_tag.as_str())
        }
        HeapFieldKind::DynamicBuffer { element_tag } => {
            resize_symbol = format!("pill_accessor_{type_ident}_{field_ident}_resize");
            let resize_fn = syn::Ident::new(&resize_symbol, field_ident.span());
            resize = quote! {
                /// Resize trampoline for the `#field_ident` buffer, generated
                /// by `#[derive(PillComponent)]`.
                ///
                /// Rewrites the buffer to `new_length` elements and returns 0,
                /// or returns 1 when `row` is null. Growing fills the new
                /// elements with the element type's `Default`. This is the one
                /// operation that can retire the block, so it ends every
                /// outstanding view of it; the handle is re-read from the row
                /// afterwards.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to.
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #resize_fn(row: *mut u8, new_length: usize) -> u8 {
                    if row.is_null() {
                        return 1;
                    }
                    // SAFETY: the caller guarantees `row` addresses a live
                    // component value and holds the write declaration the
                    // mutation needs, so the exclusive reference is valid for
                    // the whole call.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    // The block is engine-owned, so this reallocate-and-release
                    // pair runs the shared native-buffer service rather than
                    // whatever code currently owns the element heap.
                    component.#field_ident.resize(
                        new_length,
                        ::core::default::Default::default(),
                    );
                    0
                }
            };
            ("dynbuf", element_tag.as_str())
        }
        HeapFieldKind::VecString => {
            resize_symbol = format!("pill_accessor_{type_ident}_{field_ident}_resize");
            item_symbol = format!("pill_accessor_{type_ident}_{field_ident}_item");
            set_item_symbol = format!("pill_accessor_{type_ident}_{field_ident}_set_item");
            push_symbol = format!("pill_accessor_{type_ident}_{field_ident}_push");
            let resize_fn = syn::Ident::new(&resize_symbol, field_ident.span());
            let item_fn = syn::Ident::new(&item_symbol, field_ident.span());
            let set_item_fn = syn::Ident::new(&set_item_symbol, field_ident.span());
            let push_fn = syn::Ident::new(&push_symbol, field_ident.span());
            resize = quote! {
                /// Resize trampoline for the `#field_ident` list, generated by
                /// `#[derive(PillComponent)]`.
                ///
                /// Rewrites the list to `new_length` elements and returns 0,
                /// or returns 1 when `row` is null. Growing fills the new
                /// elements with empty strings. This is the one operation that
                /// can move the outer allocation, so it ends every outstanding
                /// element pointer.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to.
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #resize_fn(row: *mut u8, new_length: usize) -> u8 {
                    if row.is_null() {
                        return 1;
                    }
                    // SAFETY: the caller guarantees `row` addresses a live
                    // component value and holds the write declaration the
                    // mutation needs, so the exclusive reference is valid for
                    // the whole call.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    component.#field_ident.resize(
                        new_length,
                        ::core::default::Default::default(),
                    );
                    0
                }
            };
            item = quote! {
                /// Element-view trampoline for the `#field_ident` list,
                /// generated by `#[derive(PillComponent)]`.
                ///
                /// Writes element `index`'s UTF-8 bytes through
                /// `out_data`/`out_length` and returns 0, returns 1 when a
                /// pointer is null, or returns 2 when `index` is out of
                /// range. The bytes borrow the element only for the call.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value, and both
                /// out pointers must be writable.
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #item_fn(
                    row: *const u8,
                    index: usize,
                    out_data: *mut *const u8,
                    out_length: *mut usize,
                ) -> u8 {
                    if row.is_null() || out_data.is_null() || out_length.is_null() {
                        return 1;
                    }
                    // SAFETY: the caller guarantees `row` addresses a live
                    // component value, so the shared reference is valid for
                    // the whole call.
                    let component = unsafe { &*(row as *const #type_ident) };
                    let Some(text) = component.#field_ident.get(index) else {
                        return 2;
                    };
                    // SAFETY: both out pointers were checked non-null above,
                    // and `text` borrows the live element for this call.
                    unsafe {
                        *out_data = text.as_ptr();
                        *out_length = text.len();
                    }
                    0
                }
            };
            set_item = quote! {
                /// Element-replace trampoline for the `#field_ident` list,
                /// generated by `#[derive(PillComponent)]`.
                ///
                /// Replaces element `index` with the UTF-8 bytes
                /// `utf8..utf8+length` and returns 0, returns 1 for a null
                /// pointer, returns 2 when `index` is out of range, or returns
                /// 3 when the bytes are not valid UTF-8.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to, and `utf8` must be readable for
                /// `length` bytes (it may be null only when `length` is zero).
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #set_item_fn(
                    row: *mut u8,
                    index: usize,
                    utf8: *const u8,
                    length: usize,
                ) -> u8 {
                    if row.is_null() || (utf8.is_null() && length != 0) {
                        return 1;
                    }
                    // SAFETY: the caller promises `utf8` is readable for
                    // `length` bytes; the slice borrows only for this call.
                    let bytes: &[u8] = if length == 0 {
                        &[]
                    } else {
                        unsafe { ::core::slice::from_raw_parts(utf8, length) }
                    };
                    let Ok(text) = ::core::str::from_utf8(bytes) else {
                        return 3;
                    };
                    // SAFETY: the caller guarantees `row` addresses a live
                    // component value the caller holds write access to, so the
                    // exclusive reference is valid for the whole call.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    let Some(slot) = component.#field_ident.get_mut(index) else {
                        return 2;
                    };
                    // Reuse the element's allocation rather than replacing it.
                    slot.clear();
                    slot.push_str(text);
                    0
                }
            };
            push = quote! {
                /// Append trampoline for the `#field_ident` list, generated by
                /// `#[derive(PillComponent)]`.
                ///
                /// Appends the UTF-8 bytes `utf8..utf8+length` as a new last
                /// element and returns 0, returns 1 for a null pointer, or
                /// returns 2 when the bytes are not valid UTF-8.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to, and `utf8` must be readable for
                /// `length` bytes (it may be null only when `length` is zero).
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #push_fn(
                    row: *mut u8,
                    utf8: *const u8,
                    length: usize,
                ) -> u8 {
                    if row.is_null() || (utf8.is_null() && length != 0) {
                        return 1;
                    }
                    // SAFETY: the caller promises `utf8` is readable for
                    // `length` bytes; the slice borrows only for this call.
                    let bytes: &[u8] = if length == 0 {
                        &[]
                    } else {
                        unsafe { ::core::slice::from_raw_parts(utf8, length) }
                    };
                    let Ok(text) = ::core::str::from_utf8(bytes) else {
                        return 2;
                    };
                    // SAFETY: as above: `row` addresses a live component value
                    // the caller holds write access to.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    component.#field_ident.push(text.to_string());
                    0
                }
            };
            ("vecstring", "string")
        }
        HeapFieldKind::String => {
            set_symbol = format!("pill_accessor_{type_ident}_{field_ident}_set");
            let set_fn = syn::Ident::new(&set_symbol, field_ident.span());
            set = quote! {
                /// Replace-in-place trampoline for the `#field_ident` field,
                /// generated by `#[derive(PillComponent)]`.
                ///
                /// Replaces the string with the UTF-8 bytes `utf8..utf8+length`
                /// and returns 0, returns 1 for a null pointer, or returns 2
                /// when the bytes are not valid UTF-8. Invalid input is
                /// refused rather than lossily replaced, so an edit can never
                /// silently corrupt the stored text.
                ///
                /// # Safety
                ///
                /// `row` must point at a live `#type_ident` value the caller
                /// holds write access to, and `utf8` must be readable for
                /// `length` bytes (it may be null only when `length` is zero).
                #[doc(hidden)]
                #[no_mangle]
                pub unsafe extern "C" fn #set_fn(
                    row: *mut u8,
                    utf8: *const u8,
                    length: usize,
                ) -> u8 {
                    if row.is_null() || (utf8.is_null() && length != 0) {
                        return 1;
                    }
                    // SAFETY: the caller promises `utf8` is readable for
                    // `length` bytes; the slice borrows only for this call.
                    let bytes: &[u8] = if length == 0 {
                        &[]
                    } else {
                        unsafe { ::core::slice::from_raw_parts(utf8, length) }
                    };
                    let Ok(text) = ::core::str::from_utf8(bytes) else {
                        return 2;
                    };
                    // SAFETY: as in the view trampoline: `row` addresses a live
                    // component value the caller holds write access to.
                    let component = unsafe { &mut *(row as *mut #type_ident) };
                    component.#field_ident.clear();
                    component.#field_ident.push_str(text);
                    0
                }
            };
            ("string", "")
        }
    };

    quote! {
        #view
        #resize
        #set
        #item
        #set_item
        #push

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillFieldAccessorDescriptor {
                type_name: #type_name,
                field_name: #field_name,
                kind: #kind_tag,
                element_tag: #element_tag,
                view_symbol: #view_symbol,
                resize_symbol: #resize_symbol,
                set_symbol: #set_symbol,
                item_symbol: #item_symbol,
                set_item_symbol: #set_item_symbol,
                push_symbol: #push_symbol,
            }
        }
    }
}

/// Map a Rust field type to the closed C#-mirror type-tag vocabulary.
///
/// Primitives and fixed-size arrays are expressible; any other path type is
/// tagged as a nested struct by its fully-qualified name (the codegen resolves
/// it against the artifact's `PillMirror` inventory, falling back to an opaque
/// blob of the field's size when it is not declared). `String`, `Vec<E>` and
/// `DynamicBuffer<E>` become the heap-owning container tags `string`,
/// `vec:<element>` and `dynbuf:<element>` when the caller allows them
/// (component rows, which managed code reaches through generated accessor
/// members) and are rejected otherwise (mirrored value types, which cross the
/// boundary as plain data). `char` is rejected outright because Rust's and
/// C#'s widths disagree.
fn field_type_tag(ty: &syn::Type, allow_containers: bool) -> syn::Result<String> {
    match ty {
        syn::Type::Path(path) if path.qself.is_none() => {
            let segment = path.path.segments.last();
            let last = segment
                .map(|segment| segment.ident.to_string())
                .unwrap_or_default();
            match last.as_str() {
                "f32" | "f64" | "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64"
                | "u64" | "bool" | "usize" | "isize" => Ok(last),
                "char" => Err(syn::Error::new_spanned(
                    ty,
                    "`char` fields cannot be mirrored: Rust `char` is 4 bytes while C# `char` is 2",
                )),
                "String" => {
                    if allow_containers {
                        Ok("string".to_string())
                    } else {
                        Err(heap_owned_rejection(ty, "String"))
                    }
                }
                "Vec" | "DynamicBuffer" => {
                    if !allow_containers {
                        return Err(heap_owned_rejection(ty, &last));
                    }
                    // The element type decides the span C# iterates, so it
                    // must be inside the closed vocabulary too.
                    let element = container_element_type(segment, &last)
                        .map_err(|message| syn::Error::new_spanned(ty, message))?;
                    let element_tag = field_type_tag(element, allow_containers)?;
                    if is_primitive_tag(&element_tag) || element_tag.starts_with("struct:") {
                        let prefix = if last == "Vec" { "vec" } else { "dynbuf" };
                        Ok(format!("{prefix}:{element_tag}"))
                    } else if element_tag == "string" {
                        if last == "Vec" {
                            // Elements are individually allocated strings, so
                            // there is no span over them; C# reaches each one
                            // through its own accessor instead.
                            Ok("vec:string".to_string())
                        } else {
                            Err(syn::Error::new_spanned(
                                ty,
                                "`DynamicBuffer<String>` fields are not supported; a \
                                 `DynamicBuffer` holds plain `Copy` elements - use a `Vec<String>` \
                                 for a text list",
                            ))
                        }
                    } else if element_tag.starts_with("vec:")
                        || element_tag.starts_with("dynbuf:")
                    {
                        Err(syn::Error::new_spanned(
                            ty,
                            format!(
                                "nested containers are not supported; a `{last}` field's element \
                                 must be a primitive or a `#[derive(PillMirror)]` struct"
                            ),
                        ))
                    } else if element_tag.starts_with("array:") {
                        Err(syn::Error::new_spanned(
                            ty,
                            format!(
                                "a `{last}` of fixed-size arrays is not supported; use a `{last}` \
                                 of primitives"
                            ),
                        ))
                    } else {
                        Err(syn::Error::new_spanned(
                            ty,
                            format!("`{last}<{element_tag}>` is not a supported container field"),
                        ))
                    }
                }
                // Heap-owning standard types that have no accessor machinery.
                "Box" | "Arc" | "Rc" | "Option" | "Result" | "Cow" | "HashMap" | "HashSet"
                | "BTreeMap" | "BTreeSet" | "VecDeque" | "LinkedList" => {
                    Err(heap_owned_rejection(ty, &last))
                }
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
            let inner = field_type_tag(&array.elem, allow_containers)?;
            if inner.starts_with("vec:") || inner.starts_with("dynbuf:") || inner == "string" {
                return Err(syn::Error::new_spanned(
                    ty,
                    "arrays of heap-owning fields are not supported; use a `Vec` or \
                     `DynamicBuffer` field instead",
                ));
            }
            Ok(format!("array:{inner}"))
        }
        other => Err(syn::Error::new_spanned(
            other,
            "unsupported field type for the C# mirror; use a primitive, a fixed-size array, or a `#[derive(PillMirror)]` struct",
        )),
    }
}

/// The rejection for a heap-owning type outside the container vocabulary.
///
/// `String`, `Vec` and `DynamicBuffer` reach this only from
/// `#[derive(PillMirror)]`, whose values are copied across the boundary and
/// have no live row to reach a buffer through.
fn heap_owned_rejection(ty: &syn::Type, name: &str) -> syn::Error {
    syn::Error::new_spanned(
        ty,
        format!(
            "field type `{name}` owns heap memory and cannot be mirrored to C#; use a blittable \
             value type, a `String`, a `Vec`, or a `DynamicBuffer`"
        ),
    )
}

/// Extract the element type of a `Vec<E>` or `DynamicBuffer<E>` field.
fn container_element_type<'a>(
    segment: Option<&'a syn::PathSegment>,
    container: &str,
) -> Result<&'a syn::Type, String> {
    let missing = || {
        format!(
            "a `{container}` field must name its element type (`{container}<f32>`, not bare \
             `{container}`)"
        )
    };
    let Some(segment) = segment else {
        return Err(missing());
    };
    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
        return Err(missing());
    };
    arguments
        .args
        .iter()
        .find_map(|argument| match argument {
            syn::GenericArgument::Type(element) => Some(element),
            _ => None,
        })
        .ok_or_else(missing)
}

/// Whether a tag names one of the scalar primitives.
fn is_primitive_tag(tag: &str) -> bool {
    matches!(
        tag,
        "f32"
            | "f64"
            | "i8"
            | "u8"
            | "i16"
            | "u16"
            | "i32"
            | "u32"
            | "i64"
            | "u64"
            | "bool"
            | "usize"
            | "isize"
    )
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

    // Value types cross the boundary as plain data, so a heap-owning field has
    // no meaning here: there is no live row for managed code to reach a buffer
    // through, and the mirror is copied by value.
    let (declared_layout, layout_reference, _accessors) =
        match component_field_descriptors(&input, ident, false, true) {
            Ok(triple) => triple,
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
                crate_name: env!("CARGO_PKG_NAME"),
                size: ::core::mem::size_of::<#ident>(),
                align: ::core::mem::align_of::<#ident>(),
                fields: #layout_reference,
            }
        }
    };

    expanded.into()
}

// =============================================================================
// pill_value_type!
// =============================================================================

/// Declares the layout of a foreign value type for the managed mirror.
///
/// `#[derive(PillMirror)]` reads the type definition, so a type this crate
/// does not define (`glam::Vec3`, say) cannot carry it. This macro takes the
/// same declaration a derive would read - the type path and its `field: type`
/// list - and emits the same
/// [`PillValueTypeDescriptor`](::pill_engine::component_registry::PillValueTypeDescriptor)
/// into the calling artifact's registry, so managed code gets typed members
/// for the fields instead of an opaque byte blob.
///
/// Write the path exactly as the component field is written: the field tag is
/// the path as spelled at the field site (`Vector3f` for a `[Vector3f; 16]`
/// field), and the host resolves descriptors by exact name. The declared
/// fields must cover the type byte-for-byte - a generated assertion fails the
/// build if the type gains, loses, or repacks a field.
///
/// The declaration belongs to the artifact whose mirror needs it: a type
/// declared in one module is not visible to another module's registry.
#[proc_macro]
pub fn pill_value_type(input: TokenStream) -> TokenStream {
    let declaration = parse_macro_input!(input as ValueTypeDeclaration);

    if declaration.fields.is_empty() {
        return syn::Error::new_spanned(
            &declaration.type_path,
            "declare at least one field; a type with none has nothing to mirror",
        )
        .to_compile_error()
        .into();
    }

    // The field walk runs over a synthetic struct, so tags and offsets come
    // from the same code path as the derives. Its name only has to keep the
    // generated `static` unique - two declarations whose paths end in the
    // same segment would collide under the raw name - so the full path is
    // flattened into the identifier.
    let flat_name = declaration
        .type_path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("_");
    let synthetic_ident = syn::Ident::new(&flat_name, declaration.type_path.span());
    let field_names = declaration.fields.iter().map(|field| &field.name);
    let field_types = declaration.fields.iter().map(|field| &field.ty);
    let synthetic: DeriveInput = syn::parse_quote! {
        struct #synthetic_ident {
            #(#field_names: #field_types,)*
        }
    };

    // A foreign type can carry neither the inherent `FIELD_LAYOUT` const nor
    // the trait impl, so the emitter skips them and the submit below is the
    // only registration output.
    let (declared_layout, layout_reference, _accessors) =
        match component_field_descriptors(&synthetic, &declaration.type_path, false, false) {
            Ok(triple) => triple,
            Err(error) => return error.to_compile_error().into(),
        };

    // Joined the same way the field tag is, so the descriptor name matches
    // what a component field spelled with this path produces.
    let type_name = declaration
        .type_path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::");
    let type_path = &declaration.type_path;
    let last_field = declaration
        .fields
        .last()
        .expect("the empty declaration was rejected above");
    let last_name = &last_field.name;
    let last_type = &last_field.ty;

    let expanded = quote! {
        #declared_layout

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillValueTypeDescriptor {
                type_name: #type_name,
                crate_name: env!("CARGO_PKG_NAME"),
                size: ::core::mem::size_of::<#type_path>(),
                align: ::core::mem::align_of::<#type_path>(),
                fields: #layout_reference,
            }
        }

        // The declared fields have to account for the whole type: if the
        // foreign type gained a field behind the last declared one, managed
        // code would see fewer members than the bytes contain.
        const _: () = assert!(
            ::core::mem::offset_of!(#type_path, #last_name)
                + ::core::mem::size_of::<#last_type>()
                == ::core::mem::size_of::<#type_path>(),
            "the declared fields must cover the foreign type exactly; declare any missing field"
        );
    };

    expanded.into()
}

/// One `field: type` entry of a [`pill_value_type!`] declaration.
struct ValueTypeField {
    /// The field's name, as it appears on the foreign type.
    name: syn::Ident,
    /// The field's type, used for its size, alignment and tag.
    ty: syn::Type,
}

impl syn::parse::Parse for ValueTypeField {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let name = input.parse()?;
        input.parse::<syn::Token![:]>()?;
        let ty = input.parse()?;
        Ok(Self { name, ty })
    }
}

/// The `Type { field: type, ... }` form [`pill_value_type!`] accepts.
struct ValueTypeDeclaration {
    /// The foreign type's path, spelled as the component field is.
    type_path: syn::Path,
    /// The declared fields, in `repr(C)` order.
    fields: Vec<ValueTypeField>,
}

impl syn::parse::Parse for ValueTypeDeclaration {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        let type_path: syn::Path = input.parse()?;
        let content;
        syn::braced!(content in input);
        let fields = content.parse_terminated(ValueTypeField::parse, syn::Token![,])?;
        Ok(Self {
            type_path,
            fields: fields.into_iter().collect(),
        })
    }
}

// =============================================================================
// #[pill_mirror_impl] / #[pill_mirror_method]
// =============================================================================

/// Mirrors selected `&self` or `&mut self` methods of a mirrored type to C#.
///
/// The type may be a `#[derive(PillMirror)]` value type or an exposed
/// component row; both reach the generated C# struct the same way.
///
/// Applied to an inherent `impl` block; methods inside it marked with
/// `#[pill_mirror_method]` become typed C# instance methods on the generated
/// mirror struct, implemented by calling the real Rust method through a
/// generated `extern "C"` trampoline. The marker's path may be written fully
/// qualified (`#[pill_engine::pill_mirror_method]`); only its last segment is
/// matched. The derive and this attribute are
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
/// v1 supports a deliberately narrow contract: a `&self` or `&mut self`
/// receiver (the generated call hands the trampoline the receiver's live
/// address, so a mirrored call allocates nothing and a `&mut self` method
/// writes through to that address), primitive arguments and return values
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
        // The marker may be written bare (`#[pill_mirror_method]`, the house
        // style) or fully qualified (`#[pill_engine::pill_mirror_method]`):
        // accept both by matching the attribute path's last segment, so a
        // qualified spelling cannot silently skip a method.
        let is_mirrored = method.attrs.iter().any(|attribute| {
            attribute
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "pill_mirror_method")
        });
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

    // Receiver must be `&self` or `&mut self`: the trampoline receives the
    // address of the value the method was invoked on, so a `&mut self` method
    // writes through to the caller's storage - a component row, or the local
    // copy the call was made on. `self` by value would copy the type across
    // an ABI the mirror does not define.
    let receiver_is_mutable = match &method.sig.inputs.first() {
        Some(syn::FnArg::Receiver(receiver)) if receiver.reference.is_some() => {
            receiver.mutability.is_some()
        }
        Some(other) => {
            return Err(syn::Error::new_spanned(
                other,
                format!(
                    "`{method_ident}` must take `&self` or `&mut self` to be mirrored; `self` by value is not supported"
                ),
            ));
        }
        None => {
            return Err(syn::Error::new_spanned(
                &method.sig,
                format!("`{method_ident}` must take `&self` or `&mut self` to be mirrored"),
            ));
        }
    };

    // Every argument must be a supported primitive, tagged for the C# codegen.
    // The trampoline's own parameters are positional (`arg1`, `arg2`, ...); the
    // C#-facing fallback name is zero-based, matching the codegen's convention
    // for a pattern that is not a plain identifier.
    let mut arguments: Vec<MirroredArgument> = Vec::new();
    for (index, argument) in method.sig.inputs.iter().enumerate().skip(1) {
        let syn::FnArg::Typed(pat_type) = argument else {
            unreachable!("receiver handled above; remaining inputs are typed")
        };
        arguments.push(capture_mirrored_argument(pat_type, index, index - 1)?);
    }
    let arg_idents: Vec<&syn::Ident> = arguments.iter().map(|argument| &argument.ident).collect();
    let arg_types: Vec<&syn::Type> = arguments.iter().map(|argument| &argument.ty).collect();
    let arg_tag_literals: Vec<&syn::LitStr> =
        arguments.iter().map(|argument| &argument.tag).collect();
    let arg_name_literals: Vec<&syn::LitStr> =
        arguments.iter().map(|argument| &argument.name).collect();

    // Return must be a supported primitive or `()`.
    let (return_tag_literal, return_type) =
        capture_mirrored_return(&method.sig.output, method.sig.ident.span())?;

    // Deterministic exported symbol + descriptor string, both derived from the
    // same names so they can never drift.
    let method_name_literal = syn::LitStr::new(&method_ident.to_string(), method_ident.span());
    let symbol_name = format!("pill_mirror_{type_ident}_{method_ident}");
    let symbol_ident = syn::Ident::new(&symbol_name, method_ident.span());
    let symbol_literal = syn::LitStr::new(&symbol_name, method_ident.span());
    let self_pointer = format_ident!("self_pointer");
    // A `&mut self` method receives a writable pointer so it can edit the
    // value the managed call was made on; `&self` methods keep the read-only
    // `*const` contract.
    let self_pointer_type = if receiver_is_mutable {
        quote! { *mut #type_ident }
    } else {
        quote! { *const #type_ident }
    };

    Ok(quote! {
        /// C-ABI trampoline for the mirrored method
        /// [`#type_ident::#method_ident`], generated by `#[pill_mirror_impl]`.
        ///
        /// # Safety
        ///
        /// `#self_pointer` must point at a live `#type_ident` value for the
        /// duration of the call; the C# runtime passes the address of the
        /// value the mirror method was invoked on, and a `&mut self` method
        /// writes through it.
        #[doc(hidden)]
        #[no_mangle]
        pub unsafe extern "C" fn #symbol_ident(
            #self_pointer: #self_pointer_type
            #(, #arg_idents: #arg_types)*
        ) -> #return_type {
            // SAFETY: the host-side runtime guarantees the pointer points at a
            // live value of this type for the whole call.
            unsafe { (*#self_pointer).#method_ident(#(#arg_idents),*) }
        }

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillMethodDescriptor {
                type_name: #type_name,
                is_free_function: false,
                crate_name: env!("CARGO_PKG_NAME"),
                name: #method_name_literal,
                symbol: #symbol_literal,
                return_tag: #return_tag_literal,
                arg_tags: &[#(#arg_tag_literals),*],
                arg_names: &[#(#arg_name_literals),*],
            }
        }
    })
}

/// One mirrored argument captured for a generated trampoline and descriptor.
struct MirroredArgument {
    /// Identifier the trampoline receives it under (`arg1`, `arg2`, ...).
    ident: syn::Ident,
    /// The declared type, copied into the trampoline's signature.
    ty: syn::Type,
    /// Type tag for the C# codegen; primitives only.
    tag: syn::LitStr,
    /// C#-facing parameter name: the Rust source name when the pattern is a
    /// plain identifier, a positional `argN` otherwise.
    name: syn::LitStr,
}

/// Validate one argument of a mirrored function against the primitive
/// vocabulary and capture everything the trampoline and descriptor need.
///
/// `ident_index` numbers the trampoline's parameters (`arg1` for the first
/// argument of a method, `arg0` for the first argument of a free function);
/// `fallback_index` numbers the C#-facing fallback name, zero-based.
fn capture_mirrored_argument(
    pat_type: &syn::PatType,
    ident_index: usize,
    fallback_index: usize,
) -> Result<MirroredArgument, syn::Error> {
    let tag = mirror_method_type_tag(&pat_type.ty).map_err(|error| {
        syn::Error::new_spanned(
            &pat_type.ty,
            format!("{}: {}", error, pat_type.ty.to_token_stream()),
        )
    })?;
    let argument_name = match &*pat_type.pat {
        syn::Pat::Ident(pat_ident) => pat_ident.ident.to_string(),
        _ => format!("arg{fallback_index}"),
    };
    Ok(MirroredArgument {
        ident: format_ident!("arg{ident_index}"),
        ty: (*pat_type.ty).clone(),
        tag: syn::LitStr::new(&tag, pat_type.ty.span()),
        name: syn::LitStr::new(&argument_name, pat_type.ty.span()),
    })
}

/// Validate a mirrored function's return against the primitive vocabulary;
/// `()` maps to an empty tag and a `()` trampoline return.
fn capture_mirrored_return(
    output: &syn::ReturnType,
    span: proc_macro2::Span,
) -> Result<(syn::LitStr, proc_macro2::TokenStream), syn::Error> {
    match output {
        syn::ReturnType::Default => Ok((syn::LitStr::new("", span), quote! { () })),
        syn::ReturnType::Type(_, return_ty) => {
            let tag = mirror_method_type_tag(return_ty).map_err(|error| {
                syn::Error::new_spanned(
                    return_ty,
                    format!("{}: {}", error, return_ty.to_token_stream()),
                )
            })?;
            Ok((
                syn::LitStr::new(&tag, return_ty.span()),
                quote! { #return_ty },
            ))
        }
    }
}

/// Tag a mirrored function's argument/return type from the closed vocabulary
/// the C# codegen understands; everything else is rejected.
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
// #[pill_mirror_fn]
// =============================================================================

/// Mirrors a free function to C#, as a static method on a static class named
/// after the module declaring it.
///
/// The free-function counterpart of [`pill_mirror_impl`]: while that attribute
/// mirrors `&self` methods onto the C# struct emitted for their type, this one
/// exposes a plain function on its own. `pill_dummy_color::get_color_a`
/// becomes `pill_dummy_color.PillDummyColor.GetColorA()` - the class is the
/// last path segment PascalCased, declared in the namespace of the preceding
/// segments.
///
/// For the decorated function the macro emits, at module level next to it:
/// - a `#[no_mangle] extern "C"` trampoline named `pill_mirror_fn_<name>`
///   whose ABI is fixed regardless of the Rust function's calling convention,
///   and
/// - a [`PillMethodDescriptor`] submitted into this artifact's registry with
///   `is_free_function: true` and the declaring module's path as `type_name`,
///   so the host resolves the trampoline and the codegen emits the static
///   class rather than a struct member.
///
/// v1 keeps the same deliberately narrow contract as mirrored methods:
/// primitive arguments and return values (`u8..u64`, `i8..i64`, `f32`, `f64`,
/// `bool`, `usize`, `isize`) and a `()` return. Everything else is rejected at
/// compile time. The trampoline symbol derives from the function name alone,
/// so one mirrored free function per name per artifact.
///
/// Combine it with [`pill_hot_fn`] by listing this attribute first: it then
/// captures the original signature (the contract C# compiled against) while
/// the body stays hot-patchable, so a patch replaces the code a managed call
/// executes.
///
/// ```ignore
/// #[pill_mirror_fn]
/// #[pill_hot_fn]
/// pub fn get_color_a() -> f32 {
///     1.0
/// }
/// ```
///
/// [`PillMethodDescriptor`]: ::pill_engine::component_registry::PillMethodDescriptor
#[proc_macro_attribute]
pub fn pill_mirror_fn(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = item_fn.sig.ident.clone();

    if !item_fn.sig.generics.params.is_empty() {
        return syn::Error::new_spanned(
            &item_fn.sig.generics,
            "`#[pill_mirror_fn]` cannot mirror a generic function: the trampoline needs one concrete signature",
        )
        .to_compile_error()
        .into();
    }

    // Every argument must be a supported primitive, tagged for the C# codegen.
    // A free function has no receiver, so numbering starts at the first
    // argument for both the trampoline's parameters and the fallback names.
    let mut arguments: Vec<MirroredArgument> = Vec::new();
    for (index, argument) in item_fn.sig.inputs.iter().enumerate() {
        let syn::FnArg::Typed(pat_type) = argument else {
            return syn::Error::new_spanned(
                argument,
                "`#[pill_mirror_fn]` mirrors free functions; a `self` receiver belongs to `#[pill_mirror_impl]`",
            )
            .to_compile_error()
            .into();
        };
        match capture_mirrored_argument(pat_type, index, index) {
            Ok(captured) => arguments.push(captured),
            Err(error) => return error.to_compile_error().into(),
        }
    }
    let arg_idents: Vec<&syn::Ident> = arguments.iter().map(|argument| &argument.ident).collect();
    let arg_types: Vec<&syn::Type> = arguments.iter().map(|argument| &argument.ty).collect();
    let arg_tag_literals: Vec<&syn::LitStr> =
        arguments.iter().map(|argument| &argument.tag).collect();
    let arg_name_literals: Vec<&syn::LitStr> =
        arguments.iter().map(|argument| &argument.name).collect();

    // Return must be a supported primitive or `()`.
    let (return_tag_literal, return_type) =
        match capture_mirrored_return(&item_fn.sig.output, item_fn.sig.ident.span()) {
            Ok(captured) => captured,
            Err(error) => return error.to_compile_error().into(),
        };

    let function_name_literal = syn::LitStr::new(&fn_ident.to_string(), fn_ident.span());
    let symbol_name = format!("pill_mirror_fn_{fn_ident}");
    let symbol_ident = syn::Ident::new(&symbol_name, fn_ident.span());
    let symbol_literal = syn::LitStr::new(&symbol_name, fn_ident.span());

    let expanded = quote! {
        #item_fn

        /// C-ABI trampoline for this module's mirrored free function,
        /// generated by `#[pill_mirror_fn]`.
        #[doc(hidden)]
        #[no_mangle]
        pub extern "C" fn #symbol_ident(#(#arg_idents: #arg_types),*) -> #return_type {
            #fn_ident(#(#arg_idents),*)
        }

        ::pill_engine::submit! {
            ::pill_engine::component_registry::PillMethodDescriptor {
                type_name: ::core::module_path!(),
                is_free_function: true,
                crate_name: env!("CARGO_PKG_NAME"),
                name: #function_name_literal,
                symbol: #symbol_literal,
                return_tag: #return_tag_literal,
                arg_tags: &[#(#arg_tag_literals),*],
                arg_names: &[#(#arg_name_literals),*],
            }
        }
    };

    expanded.into()
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
/// An inherent method is supported as well, and takes a different shape: its
/// body cannot be renamed because it uses `self`, so it is emitted in place and
/// the dispatcher sits in front of it. A method parameter must therefore be a
/// plain binding, because that is the name the dispatcher forwards by -
/// destructure the value inside the body instead.
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

    // Build the argument list. A free function's body is hoisted under its
    // original signature, so its dispatcher may rename every parameter and
    // forward the renamed values. A method keeps its body inline and forwards
    // through the names that body uses, so its declaration keeps each pattern
    // exactly as written and the forwarded name is the one the pattern binds.
    let mut parameter_names = Vec::new();
    let mut parameter_declarations = Vec::new();
    let mut parameter_declaration_shapes = Vec::new();
    let mut parameter_types = Vec::new();
    for (index, argument) in signature.inputs.iter().enumerate() {
        let syn::FnArg::Typed(typed) = argument else {
            continue;
        };
        let argument_type = &*typed.ty;
        parameter_types.push(quote! { #argument_type });
        if receiver.is_some() {
            // A destructuring pattern binds no single name to forward, and the
            // inline body is emitted verbatim below, so it is refused here
            // with the reason rather than surfacing as an unresolved name
            // inside the body.
            let syn::Pat::Ident(identifier) = &*typed.pat else {
                return syn::Error::new_spanned(
                    &typed.pat,
                    concat!(
                        "`#[pill_hot_fn]` on a method requires a plain parameter ",
                        "binding: the body is emitted in place, and the dispatcher ",
                        "forwards through the names it uses. Bind the value to a ",
                        "name here and destructure it inside the body."
                    ),
                )
                .to_compile_error()
                .into();
            };
            let identifier = &identifier.ident;
            parameter_names.push(quote! { #identifier });
            parameter_declarations.push(quote! { #typed });
            parameter_declaration_shapes.push(quote! { #identifier: #argument_type });
        } else {
            let name = format_ident!("argument_{index}");
            parameter_names.push(quote! { #name });
            parameter_declarations.push(quote! { #name: #argument_type });
        }
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

    // The declaration used by the body-less trait a method patch is carried in:
    // binding modifiers are stripped, because `mut value` in a signature without
    // a body trips the `patterns_in_fns_without_body` lint. The implementation
    // that carries the body keeps every pattern exactly as written, which is
    // what the body itself needs.
    let trait_declarations = match &receiver {
        Some(receiver) => quote! { #receiver #(, #parameter_declaration_shapes)* },
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
                fn #fn_ident(#trait_declarations) #return_type;
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

/// Emit the `pill_module_init` entry point every loadable artifact exports.
///
/// One body for both attributes. A project and an extension are the same
/// DLL contract: they export the same symbols, are loaded the same way, reload
/// through the same transaction and retire into the same graveyard. What used
/// to distinguish them was a symbol prefix, which is a difference in a string
/// rather than in a contract, so the entry point is written once here and each
/// attribute supplies only the `#[cfg]` gate its artifact needs.
///
/// `gate` is empty in both emissions: a project is always built as its own
/// artifact, and a module's entry points are emitted by the wrapper macro
/// (where `init_fn` is the path `$crate::register`), so the one body serves
/// both.
fn module_init_export(
    gate: &proc_macro2::TokenStream,
    init_fn: &proc_macro2::TokenStream,
) -> proc_macro2::TokenStream {
    quote! {
        /// Registers this artifact against the host engine; returns zero on
        /// success.
        ///
        /// # Safety
        ///
        /// `api` must be a valid [`EngineApi`] pointer owned by the host and
        /// kept alive for the whole duration of this call.
        ///
        /// [`EngineApi`]: ::pill_engine::EngineApi
        #gate
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
                // borrowed while an artifact initializes, so the reconstructed
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
                let status = #init_fn(engine);
                // That drain runs before the user's own registration code, so a
                // guard raised from there - a shared resource name claimed by
                // two types, or one registered with two layouts - is recorded
                // after it has been read. Read the slot once more, so such a
                // conflict fails the init instead of being recorded and
                // forgotten.
                if engine.world_mut().take_registration_error().is_some() {
                    return u32::MAX;
                }
                status
            }));
            result.unwrap_or(u32::MAX)
        }
    }
}

/// Emit the ABI revision export the host reads before it calls anything else.
///
/// Shared for the same reason [`module_init_export`] is: the revision belongs
/// to the one loadable-artifact contract, not to one kind of artifact.
fn module_abi_version_export(gate: &proc_macro2::TokenStream) -> proc_macro2::TokenStream {
    quote! {
        /// Loadable-artifact ABI revision this crate was built against.
        #gate
        const PILL_MODULE_ABI_VERSION: u32 = ::pill_engine::module_abi::MODULE_ABI_VERSION;

        /// ABI revision, checked by the host before anything else is called.
        #gate
        #[no_mangle]
        pub extern "C" fn pill_module_abi_version() -> u32 {
            PILL_MODULE_ABI_VERSION
        }
    }
}

// =============================================================================
// #[pill_module]
// =============================================================================

/// Wraps an extension's `register` function and generates the
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
/// The `#[no_mangle]` exports come from `__pill_module_entry_points!`, the
/// macro this attribute emits: the loadable artifact is the generated
/// `host_module_<name>` wrapper crate, which compiles this one as a plain
/// library and expands the macro, so a crate linked into several artifacts
/// never defines a symbol more than once. The wrapped function itself is
/// always compiled, since a statically linked build calls it directly.
#[proc_macro_attribute]
pub fn pill_module(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = &item_fn.sig.ident;

    // The entry points are written once, as the body of the macro below: the
    // loadable artifact is always a generated wrapper crate, which compiles
    // this one as a plain library, so the symbols cannot be emitted by this
    // crate itself - a crate linked into several artifacts would define each
    // one more than once.
    fn entry_points(
        init_fn: &proc_macro2::TokenStream,
        name_bytes: &proc_macro2::TokenStream,
    ) -> proc_macro2::TokenStream {
        // The resolver stays behind `debug_assertions`: hot patching is a
        // development facility and a shipped artifact should carry no trace of
        // it.
        let hot_patch_resolver = hot_patch_resolver_export(
            &format_ident!("pill_hot_resolve"),
            &quote! { #[cfg(debug_assertions)] },
        );
        let abi_version_export = module_abi_version_export(&quote! {});
        let init_export = module_init_export(&quote! {}, init_fn);

        quote! {
            #hot_patch_resolver

        #abi_version_export

        /// Name reported to the host for diagnostics; null-terminated for the
        /// C ABI.
        const PILL_MODULE_NAME: &[u8] = #name_bytes;

        /// Human-readable module name used in host log messages.
        #[no_mangle]
        pub extern "C" fn pill_module_name() -> *const ::core::ffi::c_char {
            PILL_MODULE_NAME.as_ptr() as *const ::core::ffi::c_char
        }

        /// Number of `#[derive(PillMirror)]` value-type descriptors this
        /// artifact declares, letting the host size its copy buffer.
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

        /// Number of heap-field accessor descriptors this artifact declares,
        /// letting the host size its copy buffer.
        #[no_mangle]
        pub extern "C" fn pill_field_accessor_descriptor_count() -> u32 {
            ::pill_engine::component_registry::field_accessor_descriptors().len() as u32
        }

        /// Copy up to `max` heap-field accessor descriptors into `out`;
        /// returns the count actually copied. Each descriptor names the
        /// `#[no_mangle]` trampolines this artifact exports for one `Vec` or
        /// `String` component field, which the host resolves by symbol to
        /// obtain the callable addresses.
        ///
        /// # Safety
        ///
        /// `out` must point at `max` writable
        /// [`PillFieldAccessorDescriptor`](::pill_engine::component_registry::PillFieldAccessorDescriptor)
        /// slots owned by the host for the duration of this call.
        #[no_mangle]
        pub unsafe extern "C" fn pill_copy_field_accessor_descriptors(
            out: *mut ::pill_engine::component_registry::PillFieldAccessorDescriptor,
            max: u32,
        ) -> u32 {
            let descriptors = ::pill_engine::component_registry::field_accessor_descriptors();
            let count = (descriptors.len() as u32).min(max);
            for (index, descriptor) in descriptors.iter().take(count as usize).enumerate() {
                // SAFETY: `index < count <= max`, so `out.add(index)` stays
                // inside the buffer the host promised, and
                // `PillFieldAccessorDescriptor` is `Copy`.
                unsafe { out.add(index).write(**descriptor); }
            }
            count
        }

            #init_export
        }
    }

    let entry_point_items = entry_points(
        &quote! { $crate::#fn_ident },
        &quote! { $crate::PILL_MODULE_NAME_BYTES },
    );

    let expanded = quote! {
        // Emitted unconditionally: a statically linked build calls this
        // directly, and without it a shipping binary would have no way to
        // initialize the module it just linked in.
        #item_fn

        /// Null-terminated crate name the entry-point macro reports, so the
        /// artifact the host loads carries the extension's name rather than a
        /// wrapper's package name.
        #[doc(hidden)]
        pub const PILL_MODULE_NAME_BYTES: &[u8] =
            ::core::concat!(::core::env!("CARGO_PKG_NAME"), "\0").as_bytes();

        /// Expands to this crate's loadable-artifact entry points.
        ///
        /// The loadable artifact is the generated `host_module_<name>` wrapper
        /// crate, which compiles the extension as a plain library and expands
        /// this macro: the entry points cannot come from this crate directly,
        /// because a crate linked into several artifacts would define each
        /// `#[no_mangle]` symbol more than once. `$crate` roots everything the
        /// expansion generates back in this crate, its name included.
        #[doc(hidden)]
        #[macro_export]
        macro_rules! __pill_module_entry_points {
            () => {
                #entry_point_items
            };
        }
    };

    expanded.into()
}

// =============================================================================
// #[pill_project]
// =============================================================================

/// Wraps a project's `init` function and generates the loadable-artifact
/// exports (`pill_module_init`, `pill_module_abi_version`) plus the
/// project-only `project_schema_fingerprint`.
///
/// The annotated function must have the signature
/// `fn(engine: &mut Engine) -> u32`. The macro auto-registers every component
/// declared with `#[derive(PillComponent)]` in this artifact before calling
/// the wrapped function, and generates the schema fingerprint from the same
/// registry, so adding a component can never leave the fingerprint stale.
///
/// The entry points are the same ones [`macro@pill_module`] emits, because a
/// project and an extension are one DLL contract: the host loads both the
/// same way, reloads both through the same transaction, and used to tell them
/// apart only by a symbol prefix. What remains project-specific is the schema
/// fingerprint, which a module has no equivalent of, and the ungated exports -
/// a project is always built as its own artifact.
#[proc_macro_attribute]
pub fn pill_project(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let item_fn = parse_macro_input!(item as ItemFn);
    let fn_ident = &item_fn.sig.ident;

    // Debug-only: a released project keeps no hot-patching surface.
    let hot_patch_resolver = hot_patch_resolver_export(
        &format_ident!("pill_hot_resolve"),
        &quote! { #[cfg(debug_assertions)] },
    );
    // The same loadable-artifact contract a module emits, ungated: a project is
    // always built as its own artifact, so there is no second definition to
    // collide with.
    let ungated = quote! {};
    let abi_version_export = module_abi_version_export(&ungated);
    let init_export = module_init_export(&ungated, &quote! { #fn_ident });

    let expanded = quote! {
        #item_fn

        #hot_patch_resolver

        #abi_version_export

        #init_export

        /// Aggregate schema fingerprint of every persistable component,
        /// computed from the compile-time registry.
        #[no_mangle]
        pub extern "C" fn project_schema_fingerprint() -> u64 {
            ::pill_engine::component_registry::persistable_schema_fingerprint()
        }
    };

    expanded.into()
}
