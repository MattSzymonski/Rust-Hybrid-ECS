//! The C# members, classes, enums and resource markers generated from a
//! module's mirror descriptors.
//!
//! # Responsibilities
//!
//! - Resolve the type names a mirror tag carries (`val:Mesh`, `mut:AssetManager`)
//!   to C# types: an object class, an enum, a resource marker, a value-type
//!   struct, or one of the runtime's own types.
//! - Emit one C# member per mirrored function, whatever owns it: a value type,
//!   an object class, a resource (as extension methods on `Res<T>` /
//!   `ResMut<T>`), or a module (as a static class).
//! - Emit the object classes, enums and resource markers the type rows
//!   declare.
//!
//! # Design
//!
//! Every generated member has the same body: begin a `TracyLive.MirrorCall`
//! for the function's trampoline, push the receiver and each argument the way
//! its tag says, invoke, and read the result the way the result's tag says.
//! The runtime owns every pointer, so the generated code is safe C# and the
//! reloadable project assembly still needs no `AllowUnsafeBlocks`; one
//! function-pointer type covers every trampoline, so NativeAOT needs nothing
//! generated per signature either.
//!
//! The encodings each `Push*`/`Result*` call writes are the contract with
//! `pill_engine::mirror` on the Rust side.

// Standard library
use std::collections::BTreeMap;

// External crates
use pill_csharp_bridge::{snake_to_pascal, ResolvedMirrorMethod};
use pill_engine::component_registry::{PillValueTypeDescriptor, MIRROR_TYPE_ROW};

// =============================================================================
// Types
// =============================================================================

/// What a mirrored type name is on the C# side.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum MirrorKind {
    /// A `#[pill_mirror_object]` class.
    Object,
    /// A `#[derive(PillMirror)]` enum.
    Enum,
    /// A `#[pill_mirror_resource]` marker (or the runtime's `AssetManager`).
    Resource,
    /// A `#[derive(PillMirror)]` value type or a component row.
    Value,
}

/// One type a signature can name, resolved.
#[derive(Clone, Debug)]
pub(super) struct MirrorType {
    /// What it is.
    pub(super) kind: MirrorKind,
    /// The fully qualified C# name (`global::ns.Name`).
    pub(super) csharp: String,
}

/// One object type, enum or resource a module declares through a type row.
#[derive(Clone, Debug)]
pub(super) struct DeclaredType {
    /// The Rust qualified name the type's functions are filed under.
    pub(super) type_name: String,
    /// The C# simple name.
    pub(super) name: String,
    /// The C# namespace.
    pub(super) namespace: String,
    /// What it is.
    pub(super) kind: MirrorKind,
    /// The type row, for its flags, variants or shared name.
    pub(super) row: ResolvedMirrorMethod,
}

/// Every type a module's signatures can name.
pub(super) struct MirrorScope {
    /// By the simple Rust name a tag carries.
    by_name: BTreeMap<String, MirrorType>,
    /// The object types, enums and resources the module declares, in a
    /// deterministic order.
    pub(super) declared: Vec<DeclaredType>,
}

/// The runtime's own resource marker for the engine's asset store.
const ASSET_MANAGER: &str = "global::TracyLive.AssetManager";

/// The namespace a declared type is emitted in: its crate's root, so one
/// `using` reaches every object, enum and resource a module mirrors.
fn crate_namespace(type_name: &str) -> String {
    type_name
        .split("::")
        .next()
        .unwrap_or(type_name)
        .to_string()
}

/// The last `::` segment of a Rust path.
fn last_segment(type_name: &str) -> &str {
    type_name.rsplit("::").next().unwrap_or(type_name)
}

impl MirrorScope {
    /// Build the scope from a module's descriptors and value types.
    ///
    /// `value_namespace` gives the namespace a value type is emitted in.
    pub(super) fn new(
        methods: &[ResolvedMirrorMethod],
        value_types: &[PillValueTypeDescriptor],
        value_namespace: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let mut by_name = BTreeMap::new();
        by_name.insert(
            "AssetManager".to_string(),
            MirrorType {
                kind: MirrorKind::Resource,
                csharp: ASSET_MANAGER.to_string(),
            },
        );
        let mut declared = Vec::new();
        for row in methods
            .iter()
            .filter(|method| method.method_name == MIRROR_TYPE_ROW)
        {
            let kind = match row.owner_kind.as_str() {
                "object" => MirrorKind::Object,
                "enum" => MirrorKind::Enum,
                "resource" => MirrorKind::Resource,
                other => {
                    return Err(format!(
                        "type row of `{}` has unknown kind `{other}`",
                        row.type_name
                    ))
                }
            };
            let name = last_segment(&row.type_name).to_string();
            let namespace = crate_namespace(&row.type_name);
            if let Some(existing) = declared
                .iter()
                .find(|existing: &&DeclaredType| existing.name == name)
            {
                return Err(format!(
                    "mirrored types `{}` and `{}` both map to C# name `{name}`; rename one",
                    existing.type_name, row.type_name
                ));
            }
            by_name.insert(
                name.clone(),
                MirrorType {
                    kind: kind.clone(),
                    csharp: format!("global::{namespace}.{name}"),
                },
            );
            declared.push(DeclaredType {
                type_name: row.type_name.clone(),
                name,
                namespace,
                kind,
                row: row.clone(),
            });
        }
        declared.sort_by(|left, right| left.type_name.cmp(&right.type_name));
        for value_type in value_types {
            let name = last_segment(value_type.type_name).to_string();
            if by_name.contains_key(&name) {
                continue;
            }
            let csharp = match value_namespace(value_type.type_name) {
                Some(namespace) => format!("global::{namespace}.{name}"),
                None => name.clone(),
            };
            by_name.insert(
                name,
                MirrorType {
                    kind: MirrorKind::Value,
                    csharp,
                },
            );
        }
        Ok(Self { by_name, declared })
    }

    /// The type a tag's name resolves to.
    pub(super) fn resolve(&self, name: &str) -> Result<&MirrorType, String> {
        self.by_name.get(name).ok_or_else(|| {
            format!(
                "`{name}` is not a type this module mirrors; declare it with \
                 `#[pill_mirror_object]`, `#[pill_mirror_resource]` or `#[derive(PillMirror)]`"
            )
        })
    }

    /// The kind of a declared type, by its Rust qualified name.
    pub(super) fn declared_kind(&self, type_name: &str) -> Option<&MirrorKind> {
        self.declared
            .iter()
            .find(|declared| declared.type_name == type_name)
            .map(|declared| &declared.kind)
    }

    /// The C# type a component field tagged `struct:Handle<Name>` is mirrored
    /// as, when `Name` is an object this module mirrors.
    pub(super) fn typed_handle(&self, asset: &str) -> Option<String> {
        self.by_name
            .get(asset)
            .filter(|resolved| resolved.kind == MirrorKind::Object)
            .map(|resolved| format!("global::TracyLive.Handle<{}>", resolved.csharp))
    }
}

// =============================================================================
// Primitive Vocabulary
// =============================================================================

/// The C# type and width of a primitive tag.
fn primitive(tag: &str) -> Option<(&'static str, usize)> {
    Some(match tag {
        "u8" => ("byte", 1),
        "u16" => ("ushort", 2),
        "u32" => ("uint", 4),
        "u64" => ("ulong", 8),
        "i8" => ("sbyte", 1),
        "i16" => ("short", 2),
        "i32" => ("int", 4),
        "i64" => ("long", 8),
        "f32" => ("float", 4),
        "f64" => ("double", 8),
        "bool" => ("bool", 1),
        "usize" => ("nuint", 8),
        "isize" => ("nint", 8),
        _ => return None,
    })
}

/// The offsets of a tuple of primitives packed with natural alignment - the
/// layout `pill_engine_macros`'s `tuple_layout` computes.
fn tuple_layout(elements: &[&str]) -> Result<Vec<(usize, &'static str)>, String> {
    let mut cursor = 0usize;
    let mut layout = Vec::new();
    for element in elements {
        let (csharp, width) = primitive(element)
            .ok_or_else(|| format!("tuple element `{element}` is not a primitive"))?;
        cursor = cursor.div_ceil(width) * width;
        layout.push((cursor, csharp));
        cursor += width;
    }
    Ok(layout)
}

/// The C# type of an `array:<element>:<length>` tag.
fn array_type(tag: &str) -> Result<String, String> {
    let mut parts = tag.splitn(2, ':');
    let element = parts.next().unwrap_or_default();
    let length = parts.next().unwrap_or_default();
    match (element, length) {
        ("f32", "2") => Ok("global::System.Numerics.Vector2".to_string()),
        ("f32", "3") => Ok("global::System.Numerics.Vector3".to_string()),
        ("f32", "4") => Ok("global::System.Numerics.Vector4".to_string()),
        _ => Err(format!(
            "an `[{element}; {length}]` cannot be mirrored to C#; use `[f32; 2..=4]` or a tuple"
        )),
    }
}

// =============================================================================
// Arguments
// =============================================================================

/// One argument, lowered to C#.
struct CsArgument {
    /// The parameter declaration (`ReadOnlySpan<byte> bytes`).
    declaration: String,
    /// The statements pushing it onto the call.
    push: String,
    /// How a call forwards the parameter to another member (`in value`).
    forward: String,
}

/// Lower one argument tag to its C# parameter and push.
fn lower_argument(tag: &str, name: &str, scope: &MirrorScope) -> Result<CsArgument, String> {
    let simple = |declaration: String, push: String| {
        let forward = if declaration.starts_with("in ") {
            format!("in {name}")
        } else if declaration.starts_with("ref ") {
            format!("ref {name}")
        } else {
            name.to_string()
        };
        Ok(CsArgument {
            declaration,
            push,
            forward,
        })
    };
    if let Some((csharp, _)) = primitive(tag) {
        return simple(format!("{csharp} {name}"), format!("__call.Push({name});"));
    }
    if tag == "str" {
        return simple(
            format!("string {name}"),
            format!("__call.PushString({name});"),
        );
    }
    if tag == "loader" {
        return simple(
            format!("global::TracyLive.AssetLoader {name}"),
            format!("__call.PushLoader({name});"),
        );
    }
    if let Some(asset) = tag.strip_prefix("handle:") {
        let asset = scope.resolve(asset)?;
        return simple(
            format!("global::TracyLive.Handle<{}> {name}", asset.csharp),
            format!("__call.Push({name});"),
        );
    }
    if let Some(element) = tag.strip_prefix("slice:") {
        if let Some((csharp, _)) = primitive(element) {
            return simple(
                format!("global::System.ReadOnlySpan<{csharp}> {name}"),
                format!("__call.PushSpan({name});"),
            );
        }
        if element == "str" {
            return simple(
                format!("global::System.ReadOnlySpan<string> {name}"),
                format!("__call.PushStrings({name});"),
            );
        }
        if let Some(asset) = element.strip_prefix("handle:") {
            let asset = scope.resolve(asset)?;
            return simple(
                format!(
                    "global::System.ReadOnlySpan<global::TracyLive.Handle<{}>> {name}",
                    asset.csharp
                ),
                format!("__call.PushSpan({name});"),
            );
        }
        if let Some(named) = element.strip_prefix("val:") {
            let resolved = scope.resolve(named)?;
            let push = match resolved.kind {
                MirrorKind::Object => format!("__call.PushObjects({name});"),
                MirrorKind::Value | MirrorKind::Enum => format!("__call.PushSpan({name});"),
                MirrorKind::Resource => {
                    return Err(format!("a slice of resource `{named}` cannot be mirrored"))
                }
            };
            return simple(
                format!("global::System.ReadOnlySpan<{}> {name}", resolved.csharp),
                push,
            );
        }
        return Err(format!("unsupported slice element tag `{element}`"));
    }
    if let Some(inner) = tag.strip_prefix("option:") {
        if let Some((csharp, _)) = primitive(inner) {
            return simple(
                format!("{csharp}? {name}"),
                format!("__call.PushOptional({name});"),
            );
        }
        if let Some(asset) = inner.strip_prefix("handle:") {
            let asset = scope.resolve(asset)?;
            return simple(
                format!("global::TracyLive.Handle<{}>? {name}", asset.csharp),
                format!("__call.PushOptional({name});"),
            );
        }
        if let Some(named) = inner.strip_prefix("val:") {
            let resolved = scope.resolve(named)?;
            return match resolved.kind {
                MirrorKind::Object => simple(
                    format!("{}? {name}", resolved.csharp),
                    format!("__call.PushOptionalObject({name});"),
                ),
                MirrorKind::Enum => simple(
                    format!("{}? {name}", resolved.csharp),
                    format!("__call.PushOptional({name});"),
                ),
                _ => Err(format!("an optional `{named}` cannot be mirrored")),
            };
        }
        return Err(format!("unsupported optional tag `{inner}`"));
    }
    if let Some(elements) = tag.strip_prefix("tuple:") {
        let elements: Vec<&str> = elements.split(',').collect();
        let layout = tuple_layout(&elements)?;
        let types: Vec<&str> = layout.iter().map(|(_, csharp)| *csharp).collect();
        let pushes: Vec<String> = layout
            .iter()
            .enumerate()
            .map(|(index, (offset, _))| {
                format!("__call.PushField({offset}, {name}.Item{});", index + 1)
            })
            .collect();
        return simple(
            format!("({}) {name}", types.join(", ")),
            format!("{} __call.EndSlot();", pushes.join(" ")),
        );
    }
    if let Some(array) = tag.strip_prefix("array:") {
        return simple(
            format!("{} {name}", array_type(array)?),
            format!("__call.Push({name});"),
        );
    }
    if let Some((mutable, named)) = tag
        .strip_prefix("ref:")
        .map(|named| (false, named))
        .or_else(|| tag.strip_prefix("mut:").map(|named| (true, named)))
    {
        let resolved = scope.resolve(named)?;
        return match resolved.kind {
            MirrorKind::Object => simple(
                format!("{} {name}", resolved.csharp),
                format!("__call.PushBorrowed({name});"),
            ),
            MirrorKind::Resource => {
                let (parameter, access) = if mutable {
                    ("ResMut", "Write")
                } else {
                    ("Res", "Read")
                };
                simple(
                    format!("global::TracyLive.{parameter}<{}> {name}", resolved.csharp),
                    format!(
                        "__call.PushResource<{}>(global::TracyLive.QueryAccess.{access});",
                        resolved.csharp
                    ),
                )
            }
            MirrorKind::Value => {
                if mutable {
                    simple(
                        format!("ref {} {name}", resolved.csharp),
                        format!("__call.PushAddress(ref {name});"),
                    )
                } else {
                    simple(
                        format!("in {} {name}", resolved.csharp),
                        format!(
                            "__call.PushAddress(ref global::System.Runtime.CompilerServices.Unsafe.AsRef(in {name}));"
                        ),
                    )
                }
            }
            MirrorKind::Enum => simple(
                format!("{} {name}", resolved.csharp),
                format!("__call.PushCopy({name});"),
            ),
        };
    }
    if let Some(named) = tag.strip_prefix("val:") {
        let resolved = scope.resolve(named)?;
        return match resolved.kind {
            MirrorKind::Object => simple(
                format!("{} {name}", resolved.csharp),
                format!("__call.PushMoved({name});"),
            ),
            MirrorKind::Value => simple(
                format!("in {} {name}", resolved.csharp),
                format!("__call.PushCopy({name});"),
            ),
            MirrorKind::Enum => simple(
                format!("{} {name}", resolved.csharp),
                format!("__call.Push({name});"),
            ),
            MirrorKind::Resource => Err(format!(
                "resource `{named}` cannot be taken by value; take `&{named}` or `&mut {named}`"
            )),
        };
    }
    Err(format!("unsupported argument tag `{tag}`"))
}

// =============================================================================
// Results
// =============================================================================

/// One result, lowered to C#.
struct CsResult {
    /// The C# return type.
    csharp: String,
    /// The bytes the return buffer must hold, as a C# expression; empty for
    /// the default buffer.
    size: String,
    /// The statement returning it, after the call.
    read: String,
}

/// Lower one result tag to its C# type and read.
fn lower_result(tag: &str, scope: &MirrorScope) -> Result<CsResult, String> {
    let tag = tag.strip_prefix("result:").unwrap_or(tag);
    let plain = |csharp: String, read: String| {
        Ok(CsResult {
            csharp,
            size: String::new(),
            read,
        })
    };
    if tag.is_empty() {
        return plain("void".to_string(), String::new());
    }
    if let Some((csharp, _)) = primitive(tag) {
        return plain(
            csharp.to_string(),
            format!("return __call.Result<{csharp}>();"),
        );
    }
    if tag == "str" {
        return plain(
            "string".to_string(),
            "return __call.ResultString();".to_string(),
        );
    }
    if let Some(asset) = tag.strip_prefix("handle:") {
        let csharp = format!("global::TracyLive.Handle<{}>", scope.resolve(asset)?.csharp);
        return plain(csharp.clone(), format!("return __call.Result<{csharp}>();"));
    }
    if let Some(elements) = tag.strip_prefix("tuple:") {
        let elements: Vec<&str> = elements.split(',').collect();
        let layout = tuple_layout(&elements)?;
        let types: Vec<&str> = layout.iter().map(|(_, csharp)| *csharp).collect();
        let reads: Vec<String> = layout
            .iter()
            .map(|(offset, csharp)| format!("__call.ResultAt<{csharp}>({offset})"))
            .collect();
        return plain(
            format!("({})", types.join(", ")),
            format!("return ({});", reads.join(", ")),
        );
    }
    if let Some(array) = tag.strip_prefix("array:") {
        let csharp = array_type(array)?;
        return plain(csharp.clone(), format!("return __call.Result<{csharp}>();"));
    }
    if let Some(named) = tag.strip_prefix("val:") {
        let resolved = scope.resolve(named)?;
        let csharp = resolved.csharp.clone();
        return match resolved.kind {
            MirrorKind::Object => plain(
                csharp.clone(),
                format!("return new {csharp}(__call.ResultObject(0));"),
            ),
            MirrorKind::Enum => plain(csharp.clone(), format!("return __call.Result<{csharp}>();")),
            MirrorKind::Value => Ok(CsResult {
                size: format!("global::System.Runtime.CompilerServices.Unsafe.SizeOf<{csharp}>()"),
                read: format!("return __call.Result<{csharp}>();"),
                csharp,
            }),
            MirrorKind::Resource => Err(format!("resource `{named}` cannot be returned")),
        };
    }
    if let Some(inner) = tag.strip_prefix("option:") {
        if let Some((csharp, _)) = primitive(inner) {
            return plain(
                format!("{csharp}?"),
                format!("return __call.ResultPresent() ? __call.ResultAt<{csharp}>(16) : null;"),
            );
        }
        if let Some(asset) = inner.strip_prefix("handle:") {
            let csharp = format!("global::TracyLive.Handle<{}>", scope.resolve(asset)?.csharp);
            return plain(
                format!("{csharp}?"),
                format!("return __call.ResultPresent() ? __call.ResultAt<{csharp}>(16) : null;"),
            );
        }
        if let Some(named) = inner.strip_prefix("val:") {
            let resolved = scope.resolve(named)?;
            let csharp = resolved.csharp.clone();
            return match resolved.kind {
                MirrorKind::Object => plain(
                    format!("{csharp}?"),
                    format!("return __call.ResultPresent() ? new {csharp}(__call.ResultObject(16)) : null;"),
                ),
                MirrorKind::Enum => plain(
                    format!("{csharp}?"),
                    format!("return __call.ResultPresent() ? __call.ResultAt<{csharp}>(16) : null;"),
                ),
                MirrorKind::Value => Ok(CsResult {
                    size: format!(
                        "16 + global::System.Runtime.CompilerServices.Unsafe.SizeOf<{csharp}>()"
                    ),
                    read: format!(
                        "return __call.ResultPresent() ? __call.ResultAt<{csharp}>(16) : null;"
                    ),
                    csharp: format!("{csharp}?"),
                }),
                MirrorKind::Resource => Err(format!("resource `{named}` cannot be returned")),
            };
        }
        return Err(format!("unsupported optional result tag `{inner}`"));
    }
    Err(format!("unsupported result tag `{tag}`"))
}

// =============================================================================
// Members
// =============================================================================

/// What a member is emitted on.
pub(super) enum MemberHost<'a> {
    /// An instance or static member of a value-type struct.
    ValueStruct,
    /// An instance or static member of an object class.
    Object {
        /// The class's simple name: an associated `new` returning it also
        /// becomes a constructor.
        name: &'a str,
    },
    /// An extension method on `Res<T>` / `ResMut<T>` of a resource marker.
    Resource {
        /// The marker's fully qualified C# name.
        marker: &'a str,
    },
    /// A static member of a module's static class.
    Module,
}

/// The C# parameter name for one argument: the Rust name, unless it is blank
/// or a C# keyword.
fn parameter_name(index: usize, rust_name: &str) -> String {
    super::codegen::csharp_parameter_name(index, rust_name)
}

/// Emit the C# member (or members) for one mirrored function.
pub(super) fn emit_member(
    method: &ResolvedMirrorMethod,
    host: &MemberHost<'_>,
    scope: &MirrorScope,
) -> Result<String, String> {
    let context = || {
        format!(
            "mirrored function `{}::{}`",
            method.type_name, method.method_name
        )
    };
    let pascal = snake_to_pascal(&method.method_name);
    let mut declarations = Vec::new();
    let mut pushes = Vec::new();
    let mut forwards = Vec::new();
    for (index, tag) in method.arg_tags.iter().enumerate() {
        let rust_name = method.arg_names.get(index).map_or("", String::as_str);
        let argument = lower_argument(tag, &parameter_name(index, rust_name), scope)
            .map_err(|error| format!("{}: {error}", context()))?;
        declarations.push(argument.declaration);
        pushes.push(argument.push);
        forwards.push(argument.forward);
    }
    let result = lower_result(&method.return_tag, scope)
        .map_err(|error| format!("{}: {error}", context()))?;
    let begin = if result.size.is_empty() {
        format!(
            "var __call = global::TracyLive.MirrorCall.Begin(\"{}\", \"{}\");",
            method.type_name, method.method_name
        )
    } else {
        format!(
            "var __call = global::TracyLive.MirrorCall.Begin(\"{}\", \"{}\", {});",
            method.type_name, method.method_name, result.size
        )
    };
    let doc = format!(
        "    /// <summary>Calls the Rust function <c>{}::{}</c>.</summary>\n",
        method.type_name, method.method_name
    );

    // Each member variant: (signature prefix, receiver parameter, receiver push).
    let variants: Vec<(String, Option<String>, Option<String>)> = match (host, method.receiver.as_str()) {
        (MemberHost::Module, "") => vec![("public static".to_string(), None, None)],
        (MemberHost::Module, other) => {
            return Err(format!("{}: a free function cannot have a `{other}` receiver", context()))
        }
        (MemberHost::ValueStruct | MemberHost::Object { .. }, "") => {
            vec![("public static".to_string(), None, None)]
        }
        (MemberHost::ValueStruct, "ref" | "mut") => vec![(
            "public".to_string(),
            None,
            Some(
                "__call.PushAddress(ref global::System.Runtime.CompilerServices.Unsafe.AsRef(in this));"
                    .to_string(),
            ),
        )],
        (MemberHost::ValueStruct, "value") => vec![(
            "public readonly".to_string(),
            None,
            Some("__call.PushCopy(this);".to_string()),
        )],
        (MemberHost::Object { .. }, "ref" | "mut") => vec![(
            "public".to_string(),
            None,
            Some("__call.PushBorrowed(this);".to_string()),
        )],
        (MemberHost::Object { .. }, "value") => vec![(
            "public".to_string(),
            None,
            Some("__call.PushMoved(this);".to_string()),
        )],
        (MemberHost::Resource { .. }, "") => vec![("public static".to_string(), None, None)],
        (MemberHost::Resource { marker }, "ref") => vec![
            (
                "public static".to_string(),
                Some(format!("this global::TracyLive.Res<{marker}> self")),
                Some(format!(
                    "__call.PushResource<{marker}>(global::TracyLive.QueryAccess.Read);"
                )),
            ),
            (
                "public static".to_string(),
                Some(format!("this global::TracyLive.ResMut<{marker}> self")),
                Some(format!(
                    "__call.PushResource<{marker}>(global::TracyLive.QueryAccess.Read);"
                )),
            ),
        ],
        (MemberHost::Resource { marker }, "mut") => vec![(
            "public static".to_string(),
            Some(format!("this global::TracyLive.ResMut<{marker}> self")),
            Some(format!(
                "__call.PushResource<{marker}>(global::TracyLive.QueryAccess.Write);"
            )),
        )],
        (_, other) => {
            return Err(format!("{}: unsupported receiver `{other}`", context()));
        }
    };

    let mut output = String::new();
    for (prefix, receiver_parameter, receiver_push) in variants {
        let mut parameters: Vec<String> = Vec::new();
        if let Some(receiver_parameter) = receiver_parameter {
            parameters.push(receiver_parameter);
        }
        parameters.extend(declarations.iter().cloned());
        let mut body = String::new();
        body.push_str(&format!("        {begin}\n"));
        if let Some(receiver_push) = receiver_push {
            body.push_str(&format!("        {receiver_push}\n"));
        }
        for push in &pushes {
            body.push_str(&format!("        {push}\n"));
        }
        body.push_str("        __call.Invoke();\n");
        if !result.read.is_empty() {
            body.push_str(&format!("        {}\n", result.read));
        }
        output.push_str(&format!(
            "\n{doc}    {prefix} {} {pascal}({})\n    {{\n{body}    }}\n",
            result.csharp,
            parameters.join(", ")
        ));
    }

    // An associated `new` that returns the object itself is also its
    // constructor, so C# reads `new ShaderParameterSlot("tint", kind)` the way
    // Rust reads `ShaderParameterSlot::new("tint", kind)`. The static `New`
    // stays beside it for code that prefers the Rust spelling.
    if let MemberHost::Object { name } = host {
        let returns_self = method
            .return_tag
            .strip_prefix("result:")
            .unwrap_or(&method.return_tag)
            == format!("val:{name}");
        if method.method_name == "new" && method.receiver.is_empty() && returns_self {
            output.push_str(&format!(
                "\n{doc}    public {name}({})\n        : this(New({}).TakeHandle())\n    {{\n    }}\n",
                declarations.join(", "),
                forwards.join(", ")
            ));
        }
    }
    Ok(output)
}

/// The functions mirrored onto one owner, skipping type and runtime-only rows.
pub(super) fn members_of<'a>(
    methods: &'a [ResolvedMirrorMethod],
    type_name: &'a str,
) -> impl Iterator<Item = &'a ResolvedMirrorMethod> {
    methods.iter().filter(move |method| {
        method.type_name == type_name && !method.method_name.starts_with("__")
    })
}

// =============================================================================
// Declared Types
// =============================================================================

/// Emit one declared type - an object class, an enum, or a resource marker
/// and its extension class - wrapped in its namespace.
pub(super) fn emit_declared_type(
    declared: &DeclaredType,
    methods: &[ResolvedMirrorMethod],
    scope: &MirrorScope,
) -> Result<String, String> {
    let name = &declared.name;
    let body = match declared.kind {
        MirrorKind::Object => {
            let flags = &declared.row.arg_names;
            let mut interfaces = vec![format!("global::TracyLive.IRustObject<{name}>")];
            if flags.iter().any(|flag| flag == "asset") {
                interfaces.push(format!("global::TracyLive.IRustAsset<{name}>"));
            }
            if flags.iter().any(|flag| flag == "import") {
                interfaces.push(format!("global::TracyLive.IRustImportedAsset<{name}>"));
            }
            if flags.iter().any(|flag| flag == "standalone") {
                interfaces.push(format!("global::TracyLive.IRustStandaloneAsset<{name}>"));
            }
            let mut members = String::new();
            for method in members_of(methods, &declared.type_name) {
                members.push_str(&emit_member(method, &MemberHost::Object { name }, scope)?);
            }
            format!(
                "/// <summary>The Rust value <c>{type_name}</c>, owned by this object until it is moved\n\
                 /// into a Rust call or disposed.</summary>\n\
                 public sealed partial class {name} : global::TracyLive.RustObject, {interfaces}\n\
                 {{\n\
                 \x20\x20\x20\x20/// <summary>The Rust type this class wraps.</summary>\n\
                 \x20\x20\x20\x20public const string RustType = \"{type_name}\";\n\
                 \n\
                 \x20\x20\x20\x20static string global::TracyLive.IRustObject<{name}>.RustTypeName => RustType;\n\
                 \n\
                 \x20\x20\x20\x20static {name} global::TracyLive.IRustObject<{name}>.Wrap(global::TracyLive.RustObjectHandle handle) => new(handle);\n\
                 \n\
                 \x20\x20\x20\x20internal {name}(global::TracyLive.RustObjectHandle handle) : base(handle, RustType) {{ }}\n\
                 {members}}}\n",
                type_name = declared.type_name,
                interfaces = interfaces.join(", "),
            )
        }
        MirrorKind::Enum => {
            let representation = primitive(&declared.row.return_tag)
                .map(|(csharp, _)| csharp)
                .ok_or_else(|| {
                    format!(
                        "enum `{}` has unsupported representation `{}`",
                        declared.type_name, declared.row.return_tag
                    )
                })?;
            let variants: Vec<String> = declared
                .row
                .arg_names
                .iter()
                .zip(&declared.row.arg_tags)
                .map(|(variant, value)| format!("    {variant} = {value},"))
                .collect();
            if members_of(methods, &declared.type_name).next().is_some() {
                return Err(format!(
                    "enum `{}` has mirrored methods; a C# enum cannot carry them",
                    declared.type_name
                ));
            }
            format!(
                "/// <summary>The Rust enum <c>{}</c>.</summary>\n\
                 public enum {name} : {representation}\n{{\n{}\n}}\n",
                declared.type_name,
                variants.join("\n")
            )
        }
        MirrorKind::Resource => {
            let shared_name = declared.row.arg_names.first().ok_or_else(|| {
                format!("resource `{}` declares no shared name", declared.type_name)
            })?;
            let marker = format!("global::{}.{name}", declared.namespace);
            let mut members = String::new();
            for method in members_of(methods, &declared.type_name) {
                members.push_str(&emit_member(
                    method,
                    &MemberHost::Resource { marker: &marker },
                    scope,
                )?);
            }
            format!(
                "/// <summary>The Rust resource <c>{type_name}</c>. Declare <c>Res&lt;{name}&gt;</c> or\n\
                 /// <c>ResMut&lt;{name}&gt;</c> to reach it; its value stays in Rust.</summary>\n\
                 [global::TracyLive.NativeResource(\"{shared_name}\")]\n\
                 public readonly struct {name}\n{{\n}}\n\
                 \n\
                 /// <summary>The mirrored functions of <see cref=\"{name}\"/>.</summary>\n\
                 public static class {name}Methods\n{{{members}}}\n",
                type_name = declared.type_name,
            )
        }
        MirrorKind::Value => {
            return Err(format!(
                "`{}` is a value type, which has no type row",
                declared.type_name
            ))
        }
    };
    Ok(format!(
        "namespace {} {{\n\n{body}\n}}\n\n",
        declared.namespace
    ))
}

/// Emit the static class mirroring a module's free functions.
///
/// `module_path` is the Rust module declaring them: the class is named after
/// its last segment, PascalCased, and declared in the namespace of the
/// preceding segments, so `pill_dummy_color::get_color_a` reaches C# as
/// `pill_dummy_color.PillDummyColor.GetColorA()`.
pub(super) fn emit_free_function_class(
    module_path: &str,
    functions: &[&ResolvedMirrorMethod],
    scope: &MirrorScope,
) -> Result<String, String> {
    let class_name = snake_to_pascal(last_segment(module_path));
    let namespace = module_path
        .rfind("::")
        .map(|separator| module_path[..separator].replace("::", "."))
        .unwrap_or_else(|| module_path.to_string());
    let mut body = String::new();
    for function in functions {
        body.push_str(&emit_member(function, &MemberHost::Module, scope)?);
    }
    Ok(format!(
        "\nnamespace {namespace} {{\n\n\
         /// Static mirror of the free functions the Rust module `{module_path}` declares;\n\
         /// each member calls its trampoline through `TracyLive.MirrorCall`.\n\
         public static class {class_name}\n{{{body}}}\n\n}}\n\n"
    ))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A resolved row, as the loader produces it.
    fn row(
        type_name: &str,
        name: &str,
        receiver: &str,
        owner_kind: &str,
        return_tag: &str,
        arguments: &[(&str, &str)],
    ) -> ResolvedMirrorMethod {
        ResolvedMirrorMethod {
            type_name: type_name.to_string(),
            method_name: name.to_string(),
            return_tag: return_tag.to_string(),
            arg_tags: arguments.iter().map(|(tag, _)| tag.to_string()).collect(),
            arg_names: arguments.iter().map(|(_, name)| name.to_string()).collect(),
            address: 1,
            is_free_function: owner_kind == "module",
            crate_name: "fixture".to_string(),
            receiver: receiver.to_string(),
            owner_kind: owner_kind.to_string(),
        }
    }

    /// The type rows of a small renderer-like module.
    fn fixture() -> Vec<ResolvedMirrorMethod> {
        let mut texture_type = row("fixture::TextureType", "__type", "", "enum", "u8", &[]);
        texture_type.arg_names = vec!["Color".to_string(), "Normal".to_string()];
        texture_type.arg_tags = vec!["0".to_string(), "1".to_string()];
        let mut manager = row(
            "fixture::RenderingManager",
            "__type",
            "",
            "resource",
            "",
            &[],
        );
        manager.arg_names = vec!["fixture::RenderingManager".to_string()];
        let mut mesh = row("fixture::mesh::Mesh", "__type", "", "object", "", &[]);
        mesh.arg_names = vec!["asset".to_string(), "import".to_string()];
        vec![
            mesh,
            row(
                "fixture::mesh::Mesh",
                "from_obj_bytes",
                "",
                "",
                "result:val:Mesh",
                &[("str", "name"), ("slice:u8", "bytes")],
            ),
            row(
                "fixture::mesh::Mesh",
                "vertex_count",
                "ref",
                "",
                "usize",
                &[],
            ),
            row(
                "fixture::mesh::Mesh",
                "new",
                "",
                "",
                "result:val:Mesh",
                &[("str", "name")],
            ),
            texture_type,
            manager,
            row("fixture::RenderingManager", "clear", "mut", "", "", &[]),
            row(
                "fixture::RenderingManager",
                "pipeline",
                "ref",
                "",
                "option:handle:Mesh",
                &[],
            ),
            row(
                "fixture",
                "set_sky",
                "",
                "module",
                "result:",
                &[
                    ("mut:AssetManager", "assets"),
                    ("option:handle:Mesh", "mesh"),
                    ("tuple:u32,u32", "bindings"),
                    ("array:f32:3", "color"),
                ],
            ),
        ]
    }

    fn scope(methods: &[ResolvedMirrorMethod]) -> MirrorScope {
        MirrorScope::new(methods, &[], &|_| None).expect("the fixture resolves")
    }

    #[test]
    fn an_object_becomes_a_class_with_static_and_instance_members() {
        let methods = fixture();
        let scope = scope(&methods);
        let mesh = scope
            .declared
            .iter()
            .find(|declared| declared.name == "Mesh")
            .unwrap();
        let text = emit_declared_type(mesh, &methods, &scope).unwrap();
        assert!(text.contains("namespace fixture {"), "{text}");
        assert!(
            text.contains("public sealed partial class Mesh : global::TracyLive.RustObject, global::TracyLive.IRustObject<Mesh>, global::TracyLive.IRustAsset<Mesh>, global::TracyLive.IRustImportedAsset<Mesh>"),
            "{text}"
        );
        assert!(
            text.contains("public static global::fixture.Mesh FromObjBytes(string name, global::System.ReadOnlySpan<byte> bytes)"),
            "{text}"
        );
        assert!(text.contains("__call.PushString(name);"), "{text}");
        assert!(
            text.contains("return new global::fixture.Mesh(__call.ResultObject(0));"),
            "{text}"
        );
        assert!(text.contains("public nuint VertexCount()"), "{text}");
        assert!(text.contains("__call.PushBorrowed(this);"), "{text}");
        // An associated `new` returning the object is also its constructor.
        assert!(
            text.contains("public static global::fixture.Mesh New(string name)"),
            "{text}"
        );
        assert!(
            text.contains("public Mesh(string name)\n        : this(New(name).TakeHandle())"),
            "{text}"
        );
    }

    #[test]
    fn an_enum_keeps_its_representation_and_values() {
        let methods = fixture();
        let scope = scope(&methods);
        let declared = scope
            .declared
            .iter()
            .find(|declared| declared.name == "TextureType")
            .unwrap();
        let text = emit_declared_type(declared, &methods, &scope).unwrap();
        assert!(text.contains("public enum TextureType : byte"), "{text}");
        assert!(text.contains("    Normal = 1,"), "{text}");
    }

    #[test]
    fn a_resource_becomes_a_marker_with_extension_methods() {
        let methods = fixture();
        let scope = scope(&methods);
        let declared = scope
            .declared
            .iter()
            .find(|declared| declared.name == "RenderingManager")
            .unwrap();
        let text = emit_declared_type(declared, &methods, &scope).unwrap();
        assert!(
            text.contains("[global::TracyLive.NativeResource(\"fixture::RenderingManager\")]"),
            "{text}"
        );
        assert!(
            text.contains("public static void Clear(this global::TracyLive.ResMut<global::fixture.RenderingManager> self)"),
            "{text}"
        );
        assert!(
            !text.contains("Clear(this global::TracyLive.Res<"),
            "a write needs ResMut: {text}"
        );
        assert!(
            text.contains(
                "Pipeline(this global::TracyLive.Res<global::fixture.RenderingManager> self)"
            ),
            "a read is offered on Res: {text}"
        );
        assert!(
            text.contains(
                "Pipeline(this global::TracyLive.ResMut<global::fixture.RenderingManager> self)"
            ),
            "and on ResMut: {text}"
        );
        assert!(
            text.contains("return __call.ResultPresent() ? __call.ResultAt<global::TracyLive.Handle<global::fixture.Mesh>>(16) : null;"),
            "{text}"
        );
    }

    #[test]
    fn a_free_function_takes_resources_options_tuples_and_vectors() {
        let methods = fixture();
        let scope = scope(&methods);
        let functions: Vec<&ResolvedMirrorMethod> = methods
            .iter()
            .filter(|method| method.is_free_function)
            .collect();
        let text = emit_free_function_class("fixture", &functions, &scope).unwrap();
        assert!(
            text.contains("public static void SetSky(global::TracyLive.ResMut<global::TracyLive.AssetManager> assets, global::TracyLive.Handle<global::fixture.Mesh>? mesh, (uint, uint) bindings, global::System.Numerics.Vector3 color)"),
            "{text}"
        );
        assert!(
            text.contains("__call.PushResource<global::TracyLive.AssetManager>(global::TracyLive.QueryAccess.Write);"),
            "{text}"
        );
        assert!(
            text.contains("__call.PushField(0, bindings.Item1); __call.PushField(4, bindings.Item2); __call.EndSlot();"),
            "{text}"
        );
    }

    #[test]
    fn an_unknown_type_is_refused_with_its_name() {
        let methods = vec![row(
            "fixture",
            "broken",
            "",
            "module",
            "",
            &[("val:Missing", "value")],
        )];
        let scope = scope(&methods);
        let functions: Vec<&ResolvedMirrorMethod> = methods.iter().collect();
        let error = emit_free_function_class("fixture", &functions, &scope).unwrap_err();
        assert!(error.contains("Missing"), "{error}");
    }

    #[test]
    fn a_handle_field_is_typed_when_its_asset_is_mirrored() {
        let methods = fixture();
        let scope = scope(&methods);
        assert_eq!(
            scope.typed_handle("Mesh").as_deref(),
            Some("global::TracyLive.Handle<global::fixture.Mesh>")
        );
        assert_eq!(scope.typed_handle("Unknown"), None);
    }
}
