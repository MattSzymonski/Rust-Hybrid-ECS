// The declarations a gameplay assembly marks its ECS members with.
//
// Responsibilities:
// - Declares the attributes system discovery reads: [EcsSystem], [EcsStartup],
//   [EcsSharedComponent], [EcsComponentAlias] and [EcsFieldDefault].
//
// Design:
// - Attributes only carry what the manifest needs to say; every rule about
//   what a declaration means lives in ProjectHost and the native host.

using System.Globalization;

namespace TracyLive;

// =============================================================================
// System Declaration
// =============================================================================

/// <summary>Marks a static method for automatic ECS system discovery.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class EcsSystemAttribute : Attribute;

/// <summary>Marks a one-shot method run before the first ECS frame.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class EcsStartupAttribute : Attribute;

/// <summary>
/// Marks a component whose layout the native host already knows and binds
/// natively, rather than registering as a descriptor byte-level component.
/// </summary>
/// <remarks>
/// This is the managed half of a shared ABI: the host holds a canonical schema
/// for the type and validates this declaration against it, so the two cannot
/// drift. The renderer's components carry it.
///
/// It used to be inferred from assembly identity - "declared in csharp_runtime"
/// stood in for "the host binds it natively". That proxy broke once shared
/// components moved out to the crate that owns them, so the property is now
/// declared rather than guessed.
/// </remarks>
[AttributeUsage(AttributeTargets.Struct)]
public sealed class EcsSharedComponentAttribute : Attribute;

/// <summary>
/// Declares a name this struct used to be known by, so a rename migrates the
/// rows instead of reading as a disappearance.
/// </summary>
/// <remarks>
/// The manifest carries these names, and the host resolves each one to the
/// registration that answered to it: that registration's rows move onto this
/// type's new identity, in place, with fields matched by name exactly as a
/// layout change matches them. Without the alias a rename is not migratable -
/// the old declaration's storage cannot be safely retired - so the host
/// refuses the manifest and asks for a restart.
///
/// One hop only: an alias must name a declaration that was live when this
/// type was last registered, not another alias.
/// </remarks>
[AttributeUsage(AttributeTargets.Struct, AllowMultiple = true)]
public sealed class EcsComponentAliasAttribute : Attribute
{
    /// <summary>Declare one previous name of this component.</summary>
    /// <param name="oldName">
    /// The full name the type was declared under, as it appeared in the
    /// manifest - for example <c>"project_cs.SplineSample"</c>.
    /// </param>
    public EcsComponentAliasAttribute(string oldName) => OldName = oldName;

    /// <summary>The previous name this declaration claims.</summary>
    public string OldName { get; }
}

/// <summary>
/// Declares the value a newly added or reset field starts from, instead of
/// zero. Applies to component and resource fields alike.
/// </summary>
/// <remarks>
/// Read when the host migrates stored bytes: a field the migration carries
/// over keeps its value, and only a field the new layout leaves empty takes
/// the default. Literals only - the value travels as text in the manifest, and
/// the host refuses one that does not parse as the field's own type, so the
/// choice of constructor here is what makes a default on a field it cannot
/// describe a compile error rather than a runtime surprise.
/// </remarks>
[AttributeUsage(AttributeTargets.Field)]
public sealed class EcsFieldDefaultAttribute : Attribute
{
    /// <summary>Declare a floating-point default.</summary>
    public EcsFieldDefaultAttribute(double value) =>
        Literal = value.ToString("R", CultureInfo.InvariantCulture);

    /// <summary>Declare a single-precision default.</summary>
    public EcsFieldDefaultAttribute(float value) =>
        Literal = value.ToString("R", CultureInfo.InvariantCulture);

    /// <summary>Declare a signed integer default.</summary>
    public EcsFieldDefaultAttribute(long value) =>
        Literal = value.ToString(CultureInfo.InvariantCulture);

    /// <summary>Declare a signed integer default from an <see cref="int"/>.</summary>
    public EcsFieldDefaultAttribute(int value) =>
        Literal = value.ToString(CultureInfo.InvariantCulture);

    /// <summary>Declare an unsigned integer default.</summary>
    public EcsFieldDefaultAttribute(ulong value) =>
        Literal = value.ToString(CultureInfo.InvariantCulture);

    /// <summary>Declare a boolean default.</summary>
    public EcsFieldDefaultAttribute(bool value) => Literal = value ? "true" : "false";

    /// <summary>The literal the host parses against the field's declared type.</summary>
    public string Literal { get; }
}
