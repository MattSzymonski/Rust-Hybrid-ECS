// Safe C# ECS facade and composable scheduler-aware queries.
//
// Responsibilities:
// - Binds the native engine function table supplied by the Rust host.
// - Describes queries as independent Read/Write/Optional/Entity terms.
// - Joins native archetype columns without reflection or allocation per frame.
// - Keeps every native pointer inside stack-only enumerator and row values.
//
// Design:
// - Closed Query<T...> types build and validate their descriptor once and
//   return a typed enumerator through a hidden `new` GetEnumerator; the base
//   type keeps the shape-erased enumerator for consumers that need it.
// - ProjectHost consumes IQueryDescriptor without knowing query arity or shape.
// - QueryRow<T...> is a ref struct holding references into the enumerator's
//   joined columns. Typed accessors resolve against the query's own term list
//   at JIT time and return writable or read-only references into the active
//   native chunk, with no per-row column lookup and no per-row column copy.

using System.Collections.Concurrent;
using System.Diagnostics;
using System.Diagnostics.CodeAnalysis;
using System.Globalization;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Text;

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

// =============================================================================
// Query Terms and Descriptors
// =============================================================================

/// <summary>Compile-time metadata every query term carries.</summary>
/// <remarks>
/// The static abstract members let a typed row resolve an accessor against the
/// query's own ordered term list instead of searching the joined columns: for
/// a closed value-type query both sides of every comparison are compile-time
/// constants, so the JIT folds every non-matching branch.
/// </remarks>
public interface IQueryTerm
{
    /// <summary>Declared component type; null for padding slots.</summary>
    static abstract Type? DataType { get; }

    /// <summary>Scheduler access the term declares.</summary>
    static abstract QueryAccess Access { get; }

    /// <summary>Whether the term tolerates archetypes without the component.</summary>
    static abstract bool Optional { get; }

    /// <summary>Whether the term exposes the entity column.</summary>
    static abstract bool IsEntity { get; }
}

/// <summary>Required read-only component term.</summary>
public readonly struct Read<T> : IQueryTerm where T : unmanaged
{
    static Type? IQueryTerm.DataType => typeof(T);
    static QueryAccess IQueryTerm.Access => QueryAccess.Read;
    static bool IQueryTerm.Optional => false;
    static bool IQueryTerm.IsEntity => false;
}

/// <summary>Required writable component term.</summary>
public readonly struct Write<T> : IQueryTerm where T : unmanaged
{
    static Type? IQueryTerm.DataType => typeof(T);
    static QueryAccess IQueryTerm.Access => QueryAccess.Write;
    static bool IQueryTerm.Optional => false;
    static bool IQueryTerm.IsEntity => false;
}

/// <summary>Optional read-only component term.</summary>
public readonly struct OptionalRead<T> : IQueryTerm where T : unmanaged
{
    static Type? IQueryTerm.DataType => typeof(T);
    static QueryAccess IQueryTerm.Access => QueryAccess.Read;
    static bool IQueryTerm.Optional => true;
    static bool IQueryTerm.IsEntity => false;
}

/// <summary>Optional writable component term.</summary>
public readonly struct OptionalWrite<T> : IQueryTerm where T : unmanaged
{
    static Type? IQueryTerm.DataType => typeof(T);
    static QueryAccess IQueryTerm.Access => QueryAccess.Write;
    static bool IQueryTerm.Optional => true;
    static bool IQueryTerm.IsEntity => false;
}

/// <summary>Term exposing the current entity without scheduler component access.</summary>
public readonly struct EntityTerm : IQueryTerm
{
    static Type? IQueryTerm.DataType => typeof(Entity);
    static QueryAccess IQueryTerm.Access => QueryAccess.Read;
    static bool IQueryTerm.Optional => false;
    static bool IQueryTerm.IsEntity => true;
}

/// <summary>Padding term filling the unused slots of a closed query's row.</summary>
/// <remarks>Never matches an accessor: it declares no component type.</remarks>
public readonly struct None : IQueryTerm
{
    static Type? IQueryTerm.DataType => null;
    static QueryAccess IQueryTerm.Access => QueryAccess.Read;
    static bool IQueryTerm.Optional => false;
    static bool IQueryTerm.IsEntity => false;
}

/// <summary>Native access mode declared to the Rust scheduler.</summary>
public enum QueryAccess : byte
{
    Read = 0,
    Write = 1,
}

/// <summary>Immutable metadata for one ordered query term.</summary>
public readonly record struct QueryTermDescriptor(
    Type? ComponentType,
    ulong ComponentKey,
    ulong ComponentKeyHigh,
    int ComponentSize,
    QueryAccess Access,
    bool Optional,
    bool IsEntity);

/// <summary>Validated metadata shared by discovery and iteration.</summary>
public sealed class QueryDescriptor
{
    private readonly QueryTermDescriptor[] _terms;

    internal QueryDescriptor(params Type[] termTypes)
    {
        if (termTypes.Length == 0)
            throw new InvalidOperationException("An ECS query must contain at least one term.");

        _terms = termTypes.Select(CreateTerm).ToArray();
        var components = new HashSet<UInt128>();
        var hasEntity = false;
        foreach (var term in _terms)
        {
            if (term.IsEntity)
            {
                if (hasEntity)
                    throw new InvalidOperationException("An ECS query cannot contain EntityTerm more than once.");
                hasEntity = true;
            }
            else if (!components.Add(((UInt128)term.ComponentKeyHigh << 64) | term.ComponentKey))
            {
                throw new InvalidOperationException(
                    $"An ECS query cannot contain component {term.ComponentType!.FullName} more than once.");
            }
        }
    }

    /// <summary>Ordered terms exactly as written in the system signature.</summary>
    public IReadOnlyList<QueryTermDescriptor> Terms => _terms;

    internal QueryTermDescriptor[] TermArray => _terms;

    private static QueryTermDescriptor CreateTerm(Type termType)
    {
        if (termType == typeof(EntityTerm))
            return new QueryTermDescriptor(null, 0, 0, Marshal.SizeOf<Entity>(), QueryAccess.Read, false, true);
        if (!termType.IsGenericType)
            throw UnsupportedTerm(termType);

        Type definition = termType.GetGenericTypeDefinition();
        Type component = termType.GetGenericArguments()[0];
        QueryAccess access;
        bool optional;
        if (definition == typeof(Read<>))
            (access, optional) = (QueryAccess.Read, false);
        else if (definition == typeof(Write<>))
            (access, optional) = (QueryAccess.Write, false);
        else if (definition == typeof(OptionalRead<>))
            (access, optional) = (QueryAccess.Read, true);
        else if (definition == typeof(OptionalWrite<>))
            (access, optional) = (QueryAccess.Write, true);
        else
            throw UnsupportedTerm(termType);

        var stableId = Engine.ComponentStableId(component);
        return new QueryTermDescriptor(
            component,
            stableId.Low,
            stableId.High,
            TracyLive.Loader.NativeLayout.SizeOf(component),
            access,
            optional,
            false);
    }

    private static InvalidOperationException UnsupportedTerm(Type termType) => new(
        $"{termType} is not a query term. Use Read<T>, Write<T>, OptionalRead<T>, " +
        "OptionalWrite<T>, or EntityTerm.");
}

/// <summary>Implemented by every composable closed query type.</summary>
public interface IQueryDescriptor
{
    /// <summary>Return cached ordered query metadata.</summary>
    QueryDescriptor Descriptor { get; }
}

// =============================================================================
// Native Engine Facade
// =============================================================================

/// <summary>Safe facade over the type-erased native ECS query API.</summary>
public static unsafe class Engine
{
    private static EngineApi _api;
    private static uint _mirrorEpoch;

    /// <summary>Bind the native function table for all subsequent queries.</summary>
    internal static void Bind(EngineApi* api)
    {
        _api = *api;
        ReloadMirrorMethods();
    }

    /// <summary>Bind an unmanaged pointer received by the exported loader API.</summary>
    /// <remarks>
    /// This is the sharpest edge on the safe surface: anything holding a
    /// plausible <see cref="IntPtr"/> can bind an arbitrary function table.
    /// That is deliberate - hot-reloadable project code is trusted, and the
    /// loader handshake has no safer channel - and <c>AllowUnsafeBlocks</c>
    /// does not stand in the way of it either way; see the trust note in the
    /// project's `.csproj`.
    /// </remarks>
    internal static void Bind(IntPtr api) => Bind((EngineApi*)api);

    /// <summary>
    /// Token of the managed invocation running on this thread, or zero outside
    /// one.
    /// </summary>
    internal static uint CurrentScopeToken =>
        _api.CurrentScopeToken == null ? 0u : _api.CurrentScopeToken();

    /// <summary>
    /// Reject a chunk that was issued to an earlier managed invocation.
    /// </summary>
    /// <remarks>
    /// Debug builds only, which is the same posture Unity takes with its job
    /// safety system: the checks run while you are developing and compile out
    /// of the build you ship, because validating on every row access would
    /// cost the data plane the property that makes it worth having.
    ///
    /// A mismatch means a chunk outlived the call that produced it. The
    /// storage it points at may have been moved by an archetype migration,
    /// freed, or unloaded with its module.
    /// </remarks>
    [Conditional("DEBUG")]
    internal static void ValidateChunkScope(uint issuedToScope, string component)
    {
        uint current = CurrentScopeToken;
        if (issuedToScope == current)
            return;
        throw new InvalidOperationException(
            $"The component chunk for {component} was issued to " +
            (issuedToScope == 0 ? "no managed invocation" : $"invocation {issuedToScope}") +
            (current == 0
                ? ", and no ECS system is running on this thread now."
                : $", but invocation {current} is running now.") +
            " Chunks are valid only inside the [EcsSystem] call that produced them; " +
            "copy the value out, or re-run the query.");
    }

    /// <summary>
    /// Ask the host for one resource's bytes under the requested access.
    /// </summary>
    /// <remarks>
    /// Thin on purpose: the status byte is turned into a message by
    /// <c>ResourceAccess</c>, which knows the resource type and can name it.
    /// </remarks>
    internal static byte GetResourceView(
        StableComponentId id, byte mode, NativeResourceView* output)
    {
        if (_api.GetResourceView == null)
            return 1;
        return _api.GetResourceView(id.Low, id.High, mode, output);
    }

    /// <summary>
    /// Reject a resource view that was issued to an earlier managed invocation.
    /// </summary>
    /// <remarks>
    /// Debug builds only, like <see cref="ValidateChunkScope"/>. A resource
    /// reference cannot normally go stale - every access re-asks the host - but
    /// a system that stashes one in a local and reads it from a lambda that
    /// runs later would, and this is what names that rather than reading
    /// storage the reload has since moved.
    /// </remarks>
    [Conditional("DEBUG")]
    internal static void ValidateResourceScope(uint issuedToScope, string resource)
    {
        uint current = CurrentScopeToken;
        if (issuedToScope == current)
            return;
        throw new InvalidOperationException(
            $"The view of resource {resource} was issued to " +
            (issuedToScope == 0 ? "no managed invocation" : $"invocation {issuedToScope}") +
            (current == 0
                ? ", and no ECS system is running on this thread now."
                : $", but invocation {current} is running now.") +
            " A resource reference is valid only inside the [EcsSystem] call that " +
            "produced it; read it again, or copy the value out.");
    }

    /// <summary>
    /// Native size of one component row, after the manifest and the runtime
    /// have been proved to agree.
    /// </summary>
    /// <remarks>
    /// Exists so the agreement check is reachable from a test: resolving
    /// <c>ComponentTypeMetadata&lt;T&gt;</c> is what runs it, and a query or a
    /// deferred command is otherwise the only way to get there.
    /// </remarks>
    internal static int ComponentSizeOf<T>() where T : unmanaged =>
        ComponentTypeMetadata<T>.Size;

    /// <summary>Return the active native world's entity count.</summary>
    /// <exception cref="InvalidOperationException">
    /// No managed system is scheduled on the native side.
    /// </exception>
    public static uint EntityCount()
    {
        uint count;
        byte status = _api.EntityCount(&count);
        if (status == 0)
            return count;
        if (status == 3)
            throw new InvalidOperationException(
                "EntityCount is only available while a system is scheduled.");
        throw new InvalidOperationException($"EntityCount failed with native status {status}.");
    }

    /// <summary>
    /// Re-copy the host's mirrored-method table when the host has republished
    /// it since the last copy.
    ///
    /// The assembly swap that normally carries a rebind waits on a queued
    /// project build - seconds at best, and never if that build fails - while
    /// the trampoline addresses from the previous bind already point into the
    /// retiring module generation. Checking the host's epoch before resolving
    /// a mirrored method closes that window on the calling thread.
    /// </summary>
    internal static void RefreshMirrorMethodsIfStale()
    {
        // A host that never bound this runtime has no table to refresh, and no
        // call into the native table is possible either.
        if (_api.MirrorEpoch == null)
            return;
        if (_api.MirrorEpoch() == _mirrorEpoch)
            return;
        ReloadMirrorMethods();
    }

    /// <summary>
    /// Re-copy the host's mirrored-method table into <see cref="MirrorMethods"/>
    /// and drop the resolved delegate cache.
    ///
    /// Called at bind time and again after each managed assembly swap: an
    /// extension reload maps the module at a fresh base and may add or
    /// remove mirrored methods, so the addresses the generated structs call
    /// must be re-read from the host, and each collectible context defines its
    /// own delegate types.
    /// </summary>
    internal static void ReloadMirrorMethods()
    {
        // The epoch is read *before* the rows: a publish that lands while they
        // are being copied then leaves a newer epoch behind, so the next
        // refresh copies again instead of trusting a half-updated view.
        if (_api.MirrorEpoch != null)
            _mirrorEpoch = _api.MirrorEpoch();
        MirrorMethods.Reset();
        uint count = _api.MirrorMethodCount();
        if (count == 0)
            return;
        MirrorMethodEntry[] buffer = new MirrorMethodEntry[count];
        uint written;
        fixed (MirrorMethodEntry* pointer = buffer)
            written = _api.CopyMirrorMethods(pointer, count);
        for (int index = 0; index < written; index++)
        {
            string typeName = Marshal.PtrToStringAnsi(buffer[index].TypeName) ?? string.Empty;
            string method = Marshal.PtrToStringAnsi(buffer[index].Method) ?? string.Empty;
            MirrorMethods.Register(typeName, method, buffer[index].Address);
        }
    }

    /// <summary>Build the stable component ID shared with the Rust adapter.</summary>
    internal static ulong ComponentKey(Type type) => ComponentStableId(type).Low;
    internal static ulong ComponentKeyHigh(Type type) => ComponentStableId(type).High;
    internal static StableComponentId ComponentStableId(Type type) =>
        StableIdOf(type.FullName ?? type.Name);

    /// <summary>
    /// The stable 128-bit identity of one declared name.
    /// </summary>
    /// <remarks>
    /// A component's name is always its full type name; a resource may declare
    /// its own, so the hash is reachable from a string as well as a type. Both
    /// go through here, so the two can never drift apart.
    /// </remarks>
    internal static StableComponentId StableIdOf(string name) =>
        new StableComponentId(HashName(name, 0xcbf29ce484222325),
            HashName(name, 0x84222325cbf29ce4));

    internal static bool TryGetChunk(
        QueryTermDescriptor term,
        uint index,
        out NativeComponentChunk chunk)
    {
        NativeComponentChunk result;
        byte status = _api.GetComponentChunk(
            term.ComponentKey, term.ComponentKeyHigh, (byte)term.Access, index, &result);
        chunk = result;
        if (status == 0)
            return false;
        ValidateStatus(status, term.ComponentType?.FullName ?? "<unknown>", term.Access);
        if (chunk.ElementSize != term.ComponentSize)
            throw new InvalidOperationException(
                $"Component {term.ComponentType!.FullName} has size {term.ComponentSize} in C# but " +
                $"{chunk.ElementSize} in Rust. The component layouts must match exactly.");
        if (term.Access == QueryAccess.Write && chunk.Ticks == IntPtr.Zero)
            throw new InvalidOperationException(
                $"Writable component {term.ComponentType!.FullName} has no native change-tick column.");
        return true;
    }

    /// <summary>
    /// Resolve one term's column inside an archetype a driver chunk already
    /// identified.
    /// </summary>
    /// <remarks>
    /// The enumerator calls this once per term per archetype instead of
    /// scanning chunk indices until the archetypes match, so the number of
    /// native calls no longer grows with the archetype count. Entity terms
    /// request the archetype's entity column through the same slot.
    /// </remarks>
    internal static bool TryGetArchetypeChunk(
        QueryTermDescriptor term,
        ulong archetypeLow,
        ulong archetypeHigh,
        out NativeComponentChunk chunk)
    {
        string name = term.ComponentType?.FullName ?? "entity";
        NativeComponentChunk result;
        byte mode = term.IsEntity ? (byte)2 : (byte)term.Access;
        byte status = _api.GetArchetypeChunk(
            archetypeLow,
            archetypeHigh,
            term.ComponentKey,
            term.ComponentKeyHigh,
            mode,
            &result);
        chunk = result;
        if (status == 0)
            return false;
        ValidateStatus(status, name, term.Access);
        if (chunk.ElementSize != term.ComponentSize)
            throw new InvalidOperationException(
                $"Component {name} has size {term.ComponentSize} in C# but " +
                $"{chunk.ElementSize} in Rust. The component layouts must match exactly.");
        if (term.Access == QueryAccess.Write && chunk.Ticks == IntPtr.Zero)
            throw new InvalidOperationException(
                $"Writable component {name} has no native change-tick column.");
        return true;
    }

    internal static bool TryGetEntityChunk(uint index, out NativeComponentChunk chunk)
    {
        NativeComponentChunk result;
        byte status = _api.GetEntityChunk(index, &result);
        chunk = result;
        if (status == 0)
            return false;
        ValidateStatus(status, nameof(Entity), QueryAccess.Read);
        if (chunk.ElementSize != Marshal.SizeOf<Entity>())
            throw new InvalidOperationException(
                $"Entity has size {Marshal.SizeOf<Entity>()} in C# but {chunk.ElementSize} in Rust.");
        return true;
    }

    private static void ValidateStatus(byte status, string name, QueryAccess access)
    {
        // Status 5 reports a caller bug on this side: every managed call site
        // passes a real result buffer, so a null one reached the native table
        // by some other route.
        if (status == 5)
            throw new ArgumentException(
                $"The native query for {name} was given no result buffer.", nameof(status));
        if (status == 2)
            throw new InvalidOperationException($"Component {name} is not registered by the Rust host.");
        if (status == 3)
            throw new InvalidOperationException("An ECS query was used outside its scheduled system call.");
        if (status != 1)
            throw new InvalidOperationException(
                $"The current system did not declare this {access.ToString().ToLowerInvariant()} access to {name}.");
    }

    internal static Entity ReserveEntity()
    {
        Entity entity;
        ValidateCommandStatus(_api.ReserveEntity(&entity), "reserve an entity");
        return entity;
    }

    internal static void QueueCreate(Entity entity, Span<NativeComponentBlob> blobs)
    {
        fixed (NativeComponentBlob* pointer = blobs)
            ValidateCommandStatus(
                _api.QueueCreate(&entity, pointer, checked((uint)blobs.Length)),
                "create an entity");
    }

    internal static void QueueDestroy(Entity entity) =>
        ValidateCommandStatus(_api.QueueDestroy(&entity), "destroy an entity");

    internal static void QueueAdd<T>(Entity entity, T value) where T : unmanaged
    {
        StableComponentId id = ComponentTypeMetadata<T>.StableId;
        ValidateCommandStatus(
            _api.QueueAddComponent(
                &entity, id.Low, id.High, (byte*)&value,
                checked((uint)ComponentTypeMetadata<T>.Size)),
            $"add component {typeof(T).FullName}");
    }

    internal static void QueueRemove<T>(Entity entity) where T : unmanaged
    {
        StableComponentId id = ComponentTypeMetadata<T>.StableId;
        ValidateCommandStatus(
            _api.QueueRemoveComponent(&entity, id.Low, id.High),
            $"remove component {typeof(T).FullName}");
    }

    private static void ValidateCommandStatus(byte status, string operation)
    {
        if (status == 1)
            return;
        string reason = status switch
        {
            2 => "the component is not registered",
            3 => "Commands was used outside an active startup or scheduled system",
            4 => "the current system did not declare a Commands parameter",
            5 => "the entity handle is stale, invalid, or was not reserved by this invocation",
            6 => "the component list or ABI layout is invalid",
            _ => $"native status {status}",
        };
        throw new InvalidOperationException($"Could not {operation}: {reason}.");
    }

    private static ulong HashName(string name, ulong offset)
    {
        const ulong prime = 0x100000001b3;
        ulong hash = offset;
        foreach (byte value in Encoding.UTF8.GetBytes(name))
        {
            hash ^= value;
            hash = unchecked(hash * prime);
        }
        return hash;
    }

    // =========================================================================
    // Asset loading
    // =========================================================================

    /// <summary>
    /// Decodes a Wavefront OBJ buffer into a mesh and inserts it into the
    /// active invocation's <c>AssetManager</c>.
    /// </summary>
    /// <remarks>
    /// Only valid from inside an active invocation - ordinarily an
    /// <see cref="EcsStartupAttribute"/> method, the managed equivalent of the
    /// Rust project's own one-time asset loading.
    /// </remarks>
    public static AssetHandle LoadMeshObj(string name, ReadOnlySpan<byte> objBytes)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        uint index, generation;
        byte status;
        fixed (byte* namePointer = nameBytes)
        fixed (byte* bytesPointer = objBytes)
        {
            status = _api.AssetLoadMeshObj(
                namePointer, (uint)nameBytes.Length,
                bytesPointer, (uint)objBytes.Length,
                &index, &generation);
        }
        ValidateAssetStatus(status, $"load mesh \"{name}\"");
        return new AssetHandle(index, generation);
    }

    /// <summary>
    /// Decodes a PNG buffer into a color texture and inserts it into the
    /// active invocation's <c>AssetManager</c>. Same invocation contract as
    /// <see cref="LoadMeshObj"/>.
    /// </summary>
    public static AssetHandle LoadTexturePng(string name, ReadOnlySpan<byte> pngBytes)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        uint index, generation;
        byte status;
        fixed (byte* namePointer = nameBytes)
        fixed (byte* bytesPointer = pngBytes)
        {
            status = _api.AssetLoadTexturePng(
                namePointer, (uint)nameBytes.Length,
                bytesPointer, (uint)pngBytes.Length,
                &index, &generation);
        }
        ValidateAssetStatus(status, $"load texture \"{name}\"");
        return new AssetHandle(index, generation);
    }

    /// <summary>
    /// Builds a shader from managed WGSL sources and slot declarations, and
    /// inserts it into the active invocation's <c>AssetManager</c>. Same
    /// invocation contract as <see cref="LoadMeshObj"/>.
    /// </summary>
    public static AssetHandle LoadShader(
        string name,
        string vertexWgsl,
        string fragmentWgsl,
        ReadOnlySpan<ShaderParameter> parameters,
        ReadOnlySpan<ShaderTextureBinding> textures,
        bool passEngineParameters,
        bool passCameraParameters)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);
        byte[] vertexBytes = Encoding.UTF8.GetBytes(vertexWgsl);
        byte[] fragmentBytes = Encoding.UTF8.GetBytes(fragmentWgsl);

        // Every nested name needs its own pinned buffer alive for the whole
        // call, so they are collected up front rather than pinned one at a
        // time inside the loop that fills the native slot arrays.
        byte[][] parameterNameBytes = new byte[parameters.Length][];
        for (int i = 0; i < parameters.Length; i++)
            parameterNameBytes[i] = Encoding.UTF8.GetBytes(parameters[i].Name);
        byte[][] textureNameBytes = new byte[textures.Length][];
        for (int i = 0; i < textures.Length; i++)
            textureNameBytes[i] = Encoding.UTF8.GetBytes(textures[i].Name);

        Span<GCHandle> parameterNamePins = new GCHandle[parameters.Length];
        Span<GCHandle> textureNamePins = new GCHandle[textures.Length];
        uint index, generation;
        byte status;
        try
        {
            var nativeParameters = new NativeShaderParameterSlot[parameters.Length];
            for (int i = 0; i < parameters.Length; i++)
            {
                parameterNamePins[i] = GCHandle.Alloc(parameterNameBytes[i], GCHandleType.Pinned);
                nativeParameters[i] = new NativeShaderParameterSlot
                {
                    Name = (byte*)parameterNamePins[i].AddrOfPinnedObject(),
                    NameLen = (uint)parameterNameBytes[i].Length,
                    Kind = (byte)parameters[i].Kind,
                };
            }

            var nativeTextures = new NativeShaderTextureSlot[textures.Length];
            for (int i = 0; i < textures.Length; i++)
            {
                textureNamePins[i] = GCHandle.Alloc(textureNameBytes[i], GCHandleType.Pinned);
                nativeTextures[i] = new NativeShaderTextureSlot
                {
                    Name = (byte*)textureNamePins[i].AddrOfPinnedObject(),
                    NameLen = (uint)textureNameBytes[i].Length,
                    TextureBinding = textures[i].TextureBinding,
                    SamplerBinding = textures[i].SamplerBinding,
                };
            }

            fixed (byte* namePointer = nameBytes)
            fixed (byte* vertexPointer = vertexBytes)
            fixed (byte* fragmentPointer = fragmentBytes)
            fixed (NativeShaderParameterSlot* parametersPointer = nativeParameters)
            fixed (NativeShaderTextureSlot* texturesPointer = nativeTextures)
            {
                status = _api.AssetLoadShader(
                    namePointer, (uint)nameBytes.Length,
                    vertexPointer, (uint)vertexBytes.Length,
                    fragmentPointer, (uint)fragmentBytes.Length,
                    parametersPointer, (uint)nativeParameters.Length,
                    texturesPointer, (uint)nativeTextures.Length,
                    passEngineParameters ? (byte)1 : (byte)0,
                    passCameraParameters ? (byte)1 : (byte)0,
                    &index, &generation);
            }
        }
        finally
        {
            foreach (GCHandle pin in parameterNamePins)
                if (pin.IsAllocated) pin.Free();
            foreach (GCHandle pin in textureNamePins)
                if (pin.IsAllocated) pin.Free();
        }
        ValidateAssetStatus(status, $"load shader \"{name}\"");
        return new AssetHandle(index, generation);
    }

    /// <summary>
    /// Builds a material from already-loaded handles and per-slot parameters,
    /// and inserts it into the active invocation's <c>AssetManager</c>. Same
    /// invocation contract as <see cref="LoadMeshObj"/>.
    /// </summary>
    /// <param name="shader">
    /// The shader to render with, or <see cref="AssetHandle.None"/> to leave
    /// the renderer's default shader in place.
    /// </param>
    public static AssetHandle CreateMaterial(
        string name,
        AssetHandle shader,
        ReadOnlySpan<MaterialTextureBinding> textures,
        ReadOnlySpan<MaterialScalarParameter> scalars,
        ReadOnlySpan<MaterialColorParameter> colors,
        byte renderingOrder = byte.MaxValue)
    {
        byte[] nameBytes = Encoding.UTF8.GetBytes(name);

        byte[][] textureSlotBytes = new byte[textures.Length][];
        for (int i = 0; i < textures.Length; i++)
            textureSlotBytes[i] = Encoding.UTF8.GetBytes(textures[i].Slot);
        byte[][] scalarNameBytes = new byte[scalars.Length][];
        for (int i = 0; i < scalars.Length; i++)
            scalarNameBytes[i] = Encoding.UTF8.GetBytes(scalars[i].Name);
        byte[][] colorNameBytes = new byte[colors.Length][];
        for (int i = 0; i < colors.Length; i++)
            colorNameBytes[i] = Encoding.UTF8.GetBytes(colors[i].Name);

        Span<GCHandle> texturePins = new GCHandle[textures.Length];
        Span<GCHandle> scalarPins = new GCHandle[scalars.Length];
        Span<GCHandle> colorPins = new GCHandle[colors.Length];
        uint index, generation;
        byte status;
        try
        {
            var nativeTextures = new NativeMaterialTexture[textures.Length];
            for (int i = 0; i < textures.Length; i++)
            {
                texturePins[i] = GCHandle.Alloc(textureSlotBytes[i], GCHandleType.Pinned);
                nativeTextures[i] = new NativeMaterialTexture
                {
                    Slot = (byte*)texturePins[i].AddrOfPinnedObject(),
                    SlotLen = (uint)textureSlotBytes[i].Length,
                    TextureIndex = textures[i].Texture.Index,
                    TextureGeneration = textures[i].Texture.Generation,
                };
            }

            var nativeScalars = new NativeMaterialScalar[scalars.Length];
            for (int i = 0; i < scalars.Length; i++)
            {
                scalarPins[i] = GCHandle.Alloc(scalarNameBytes[i], GCHandleType.Pinned);
                nativeScalars[i] = new NativeMaterialScalar
                {
                    Name = (byte*)scalarPins[i].AddrOfPinnedObject(),
                    NameLen = (uint)scalarNameBytes[i].Length,
                    Value = scalars[i].Value,
                };
            }

            var nativeColors = new NativeMaterialColor[colors.Length];
            for (int i = 0; i < colors.Length; i++)
            {
                colorPins[i] = GCHandle.Alloc(colorNameBytes[i], GCHandleType.Pinned);
                nativeColors[i] = new NativeMaterialColor
                {
                    Name = (byte*)colorPins[i].AddrOfPinnedObject(),
                    NameLen = (uint)colorNameBytes[i].Length,
                    R = colors[i].R,
                    G = colors[i].G,
                    B = colors[i].B,
                };
            }

            fixed (byte* namePointer = nameBytes)
            fixed (NativeMaterialTexture* texturesPointer = nativeTextures)
            fixed (NativeMaterialScalar* scalarsPointer = nativeScalars)
            fixed (NativeMaterialColor* colorsPointer = nativeColors)
            {
                status = _api.AssetCreateMaterial(
                    namePointer, (uint)nameBytes.Length,
                    shader.Index, shader.Generation,
                    texturesPointer, (uint)nativeTextures.Length,
                    scalarsPointer, (uint)nativeScalars.Length,
                    colorsPointer, (uint)nativeColors.Length,
                    renderingOrder,
                    &index, &generation);
            }
        }
        finally
        {
            foreach (GCHandle pin in texturePins)
                if (pin.IsAllocated) pin.Free();
            foreach (GCHandle pin in scalarPins)
                if (pin.IsAllocated) pin.Free();
            foreach (GCHandle pin in colorPins)
                if (pin.IsAllocated) pin.Free();
        }
        ValidateAssetStatus(status, $"create material \"{name}\"");
        return new AssetHandle(index, generation);
    }

    private static void ValidateAssetStatus(byte status, string operation)
    {
        if (status == 0)
            return;
        string reason = status switch
        {
            1 => "no managed invocation is active (asset loading needs an [EcsStartup] method)",
            2 => "the engine's AssetManager resource is missing",
            3 => "a supplied string was not valid UTF-8",
            4 => "the source data failed to decode",
            5 => "a required buffer was null",
            6 => "this host build has no renderer, so no asset types exist to load into",
            7 => "an asset with that name is already loaded",
            _ => $"native status {status}",
        };
        throw new InvalidOperationException($"Could not {operation}: {reason}.");
    }
}

internal readonly record struct StableComponentId(ulong Low, ulong High);

// =============================================================================
// Mirrored Rust Methods
// =============================================================================

/// <summary>
/// Registry of the Rust methods mirrored into generated C# structs.
///
/// Populated once at startup from the host's native table (see
/// <see cref="Engine.Bind"/>). Generated mirror methods resolve a typed
/// delegate over each method's exported C-ABI trampoline and invoke it with
/// the receiver's live address, so a call boxes and pins nothing. Generated
/// heap-field accessors skip the delegate layer
/// entirely: they resolve the trampoline's raw address once per host bind
/// (guarded by <see cref="Generation"/>) and call it through the
/// <c>Invoke*</c> helpers below, which use C-ABI function pointers with no
/// marshalling between managed and native code. Both paths stay callable from
/// code compiled without the `unsafe` keyword, so the reloadable project
/// assembly needs no <c>AllowUnsafeBlocks</c> - which guards that keyword,
/// not memory safety: the addresses these methods exchange are raw, and the
/// project's code is trusted the same way any other code the user builds is.
/// </summary>
/// <summary>
/// The address of a live value the runtime handed out.
/// </summary>
/// <remarks>
/// A script cannot construct one: the field and the constructor are both
/// internal, so the only way to hold a row address is to have been given it by
/// <see cref="MirrorMethods.AddressOf{T}(ref T)"/>. That does not make the
/// address safe to *keep* - it points into a native archetype column that a
/// structural change or a module reload can move - it makes it impossible to
/// invent, which removes the accidental path without pretending to remove the
/// deliberate one.
/// </remarks>
public readonly struct RowPointer
{
    internal readonly IntPtr Address;
    internal RowPointer(IntPtr address) => Address = address;
}

/// <summary>
/// A mirrored-method trampoline address the host published for the module
/// generation currently loaded.
/// </summary>
/// <remarks>
/// Opaque for the same reason as <see cref="RowPointer"/>, and with a shorter
/// life: a module reload republishes the table at fresh addresses, so a
/// trampoline resolved before a reload is stale after it.
/// </remarks>
public readonly struct MirrorTrampoline
{
    internal readonly IntPtr Address;
    internal MirrorTrampoline(IntPtr address) => Address = address;
}

/// <summary>
/// A borrowed window over a native buffer, exactly as a Rust trampoline
/// reported it.
/// </summary>
/// <remarks>
/// The pointer and the length always travel together and can only be produced
/// by a trampoline call, so managed code cannot pair an address with a length
/// of its own choosing. The lease ends when the call that produced it returns:
/// anything that resizes or replaces the container on the Rust side
/// invalidates it.
/// </remarks>
public readonly struct BufferView
{
    internal readonly IntPtr Data;

    /// <summary>Number of elements the trampoline reported.</summary>
    public readonly int Length;

    internal BufferView(IntPtr data, IntPtr length)
    {
        Data = data;
        Length = checked((int)length);
    }
}

public static class MirrorMethods
{
    // Concurrent collections because `Resolve` is reachable from scheduled
    // systems, and the scheduler runs batches of disjoint systems on parallel
    // threads: a plain `Dictionary` mutated from two of them at once is a lost
    // update at best and a corrupted bucket chain at worst. Every operation
    // here is a single `TryGetValue` or indexer write, so the concurrent forms
    // cover it without a lock.
    private static readonly ConcurrentDictionary<(string TypeName, string Method), IntPtr> Addresses = new();
    private static readonly ConcurrentDictionary<(string TypeName, string Method), (int Generation, Delegate Delegate)> Cache = new();
    private static int _generation;

    /// <summary>
    /// Counts host binds. Generated accessors cache trampoline addresses and
    /// re-resolve whenever this value changes: reloading an extension
    /// maps it at a fresh base address, so the addresses from the previous
    /// bind dangle from then on.
    /// </summary>
    public static int Generation => global::System.Threading.Volatile.Read(ref _generation);

    /// <summary>Drop all registered methods; called when the host rebinds.</summary>
    internal static void Reset()
    {
        Addresses.Clear();
        Cache.Clear();
        global::System.Threading.Volatile.Write(ref _generation, _generation + 1);
    }

    /// <summary>Register one mirrored method's trampoline address.</summary>
    internal static void Register(string typeName, string method, IntPtr address)
        => Addresses[(typeName, method)] = address;

    /// <summary>
    /// Address of a mirrored Rust method's C-ABI trampoline, for generated
    /// accessors that call it through the <c>Invoke*</c> helpers.
    /// </summary>
    /// <exception cref="InvalidOperationException">
    /// No trampoline is registered for the method - most often because the
    /// module was statically linked rather than loaded as a dynamic library.
    /// </exception>
    public static MirrorTrampoline Address(string typeName, string method)
    {
        if (!Addresses.TryGetValue((typeName, method), out IntPtr address))
        {
            throw new InvalidOperationException(
                $"No mirrored Rust method {typeName}::{method} is registered. " +
                "Mirrored methods are only available when the declaring module is " +
                "loaded as a dynamic library by the developer host.");
        }
        return new MirrorTrampoline(address);
    }

    /// <summary>
    /// Resolve (and cache) the typed delegate that calls a mirrored Rust
    /// method's C-ABI trampoline.
    /// </summary>
    /// <typeparam name="T">The generated delegate type for the method.</typeparam>
    public static T Resolve<T>(string typeName, string method) where T : Delegate
    {
        // A host republish since the last check invalidates every address this
        // cache holds; refreshing first means the generation read below is the
        // one the entries are compared against.
        Engine.RefreshMirrorMethodsIfStale();
        (string, string) key = (typeName, method);
        int generation = Generation;
        if (Cache.TryGetValue(key, out (int Generation, Delegate Delegate) cached)
            && cached.Generation == generation)
        {
            return (T)cached.Delegate;
        }
        T created = (T)Marshal.GetDelegateForFunctionPointer(
            Address(typeName, method).Address, typeof(T));
        Cache[key] = (generation, created);
        return created;
    }

    // One helper per heap-field trampoline shape. The address comes from the
    // host's registration table and the caller passes the component row's
    // live address, so the call reads and writes the real container: one
    // boundary call per member use, no delegate stub, no marshalling.

    /// <summary>
    /// Call a `(row, out data, out length)` trampoline - a `Vec`/`String` view
    /// or the count view of a `Vec&lt;String&gt;`.
    /// </summary>
    public static unsafe byte InvokeView(
        MirrorTrampoline address, RowPointer row, out BufferView view)
    {
        byte status = ((delegate* unmanaged[Cdecl]<IntPtr, out IntPtr, out IntPtr, byte>)
            address.Address)(row.Address, out IntPtr data, out IntPtr length);
        view = new BufferView(data, length);
        return status;
    }

    /// <summary>Call a `(row, count)` trampoline that resizes a container field.</summary>
    public static unsafe byte InvokeResize(MirrorTrampoline address, RowPointer row, int count)
        => ((delegate* unmanaged[Cdecl]<IntPtr, IntPtr, byte>)address.Address)(
            row.Address, (IntPtr)count);

    /// <summary>
    /// Call a `(row, index, out data, out length)` trampoline - one element of
    /// a `Vec&lt;String&gt;` (status: 0 reachable, 1 dead row, 2 out of range).
    /// </summary>
    public static unsafe byte InvokeItem(
        MirrorTrampoline address, RowPointer row, int index, out BufferView view)
    {
        byte status = ((delegate* unmanaged[Cdecl]<IntPtr, IntPtr, out IntPtr, out IntPtr, byte>)
            address.Address)(row.Address, (IntPtr)index, out IntPtr data, out IntPtr length);
        view = new BufferView(data, length);
        return status;
    }

    /// <summary>
    /// Call a `(row, index, utf8, length)` trampoline that replaces one
    /// element of a `Vec&lt;String&gt;` (status: 0 ok, 1 dead row, 2 out of
    /// range, 3 invalid UTF-8).
    /// </summary>
    /// <remarks>
    /// The payload crosses as a span rather than an address and a length, so
    /// the caller cannot pair a pointer with a length of its own choosing, and
    /// generated accessors no longer pin a byte array by hand.
    /// </remarks>
    public static unsafe byte InvokeSetItem(
        MirrorTrampoline address, RowPointer row, int index, ReadOnlySpan<byte> utf8)
    {
        fixed (byte* payload = utf8)
            return ((delegate* unmanaged[Cdecl]<IntPtr, IntPtr, IntPtr, IntPtr, byte>)
                address.Address)(
                    row.Address, (IntPtr)index, (IntPtr)payload, (IntPtr)utf8.Length);
    }

    /// <summary>
    /// Call a `(row, utf8, length)` trampoline - writing a `String` field or
    /// appending to a `Vec&lt;String&gt;`.
    /// </summary>
    public static unsafe byte InvokeUtf8Write(
        MirrorTrampoline address, RowPointer row, ReadOnlySpan<byte> utf8)
    {
        fixed (byte* payload = utf8)
            return ((delegate* unmanaged[Cdecl]<IntPtr, IntPtr, IntPtr, byte>)address.Address)(
                row.Address, (IntPtr)payload, (IntPtr)utf8.Length);
    }

    /// <summary>
    /// Address of a live value passed by reference, for generated heap-field
    /// accessors and mirrored method receivers.
    ///
    /// The address points at the storage the reference names - for a component
    /// row, the native column slot - so a trampoline reached through it reads
    /// and writes the real container rather than a copy of its header.
    /// </summary>
    public static unsafe RowPointer AddressOf<T>(ref T value) where T : unmanaged
        => new RowPointer(
            (IntPtr)global::System.Runtime.CompilerServices.Unsafe.AsPointer(ref value));
}

/// <summary>
/// Span helpers for generated heap-field accessors.
///
/// A generated accessor hands these methods the address and element count a
/// Rust trampoline (or a mirrored `DynamicBuffer` handle) reported, so managed
/// code iterates a component's live buffer with no per-element boundary call.
/// An empty buffer carries a null element pointer, which a zero count turns
/// into the empty span rather than an invalid one. A span is a lease, not
/// ownership: resizing or replacing the container on the Rust side invalidates
/// it.
/// </summary>
public static class ComponentViews
{
    /// <summary>Writable span over the buffer a trampoline reported.</summary>
    public static unsafe Span<T> AsSpan<T>(BufferView view) where T : unmanaged
        => view.Length == 0 ? Span<T>.Empty : new Span<T>((void*)view.Data, view.Length);

    /// <summary>Read-only span over the buffer a trampoline reported.</summary>
    public static unsafe ReadOnlySpan<T> AsReadOnlySpan<T>(BufferView view) where T : unmanaged
        => view.Length == 0 ? ReadOnlySpan<T>.Empty : new ReadOnlySpan<T>((void*)view.Data, view.Length);

    /// <summary>Decode a UTF-8 buffer a trampoline reported into a string.</summary>
    /// <remarks>
    /// The copy is the point: the view is a lease that ends when the call that
    /// produced it returns, and a string is the only form of the data that can
    /// outlive it.
    /// </remarks>
    public static string ToUtf8String(BufferView view)
        => view.Length == 0
            ? string.Empty
            : Marshal.PtrToStringUTF8(view.Data, view.Length) ?? string.Empty;

    /// <summary>
    /// Span over an engine-owned dynamic buffer whose `(ptr, len, cap)` handle
    /// is mirrored into a component row.
    /// </summary>
    /// <remarks>
    /// Separate from <see cref="AsSpan{T}(BufferView)"/> because this handle is
    /// mirrored *data* rather than the result of a trampoline call: the pointer
    /// is read out of the row itself, so there is no call to hand back a
    /// <see cref="BufferView"/>. Generated accessors are the only intended
    /// caller, and they read the pointer from private fields of the generated
    /// mirror struct.
    ///
    /// This is the one raw entry point left in the public surface. It survives
    /// because the alternative would be to expose the mirror's handle fields,
    /// which is strictly worse. The address-stability contract that makes a
    /// retained handle legal at all is the buffer allocator's, not this
    /// method's: resizing is a native call that ends every outstanding lease.
    /// </remarks>
    public static unsafe Span<T> OverDynamicBuffer<T>(UIntPtr data, int count) where T : unmanaged
        => count == 0 ? Span<T>.Empty : new Span<T>((void*)data, count);

    /// <summary>Read-only twin of <see cref="OverDynamicBuffer{T}"/>.</summary>
    public static unsafe ReadOnlySpan<T> OverDynamicBufferReadOnly<T>(UIntPtr data, int count)
        where T : unmanaged
        => count == 0 ? ReadOnlySpan<T>.Empty : new ReadOnlySpan<T>((void*)data, count);
}

/// <summary>
/// Per-closed-component metadata initialized once outside row iteration.
/// Generic static initialization avoids repeated Type.FullName lookup,
/// UTF-8 allocation, hashing, and Marshal.SizeOf calls in managed hot paths.
/// </summary>
internal static class ComponentTypeMetadata<T> where T : unmanaged
{
    internal static readonly StableComponentId StableId =
        Engine.ComponentStableId(typeof(T));

    /// <summary>
    /// Native size of one component row, taken from the runtime itself.
    /// </summary>
    /// <remarks>
    /// <c>Unsafe.SizeOf&lt;T&gt;()</c> is the number a row write actually
    /// touches: <c>((T*)column.Data)[row] = value</c> moves exactly this many
    /// bytes. <c>NativeLayout</c> predicts the same number by walking fields,
    /// because NativeAOT denies it <c>Marshal.SizeOf</c>, and the manifest the
    /// host registers a column stride from is built from that prediction.
    ///
    /// If the two ever disagree the column stride is wrong and every row write
    /// runs off the end of its slot, so the prediction is checked against the
    /// authority here - the one place a real <c>T</c> is in scope, which keeps
    /// the check working under NativeAOT where <c>MakeGenericMethod</c> over a
    /// value type is not available. Every component reachable from a query row
    /// or a deferred command resolves this class, so the check covers the
    /// whole reachable set without a registry to keep in sync.
    /// </remarks>
    internal static readonly int Size = SizeCheckedAgainstManifest();

    /// <summary>Return the runtime size after proving the manifest agrees.</summary>
    private static int SizeCheckedAgainstManifest()
    {
        int actual = Unsafe.SizeOf<T>();
        int describedByManifest = TracyLive.Loader.NativeLayout.SizeOf(typeof(T));
        if (describedByManifest != actual)
            throw new InvalidOperationException(
                $"Component {typeof(T).FullName} is {actual} bytes to the runtime but the " +
                $"component manifest describes {describedByManifest}. The native column would " +
                "be strided by the manifest size, so every row write would address the wrong " +
                "bytes. Remove StructLayout Pack, or declare an explicit Size that matches.");
        return actual;
    }
}

// =============================================================================
// Deferred Entity Commands
// =============================================================================

/// <summary>
/// Stateless system parameter for deferred structural mutations. Native code
/// accepts calls only during the startup/system invocation that supplied it.
/// </summary>
public readonly struct Commands
{
    /// <summary>Begin describing a new entity.</summary>
    public DeferredEntityBuilder CreateEntity() => new();

    /// <summary>Queue adding one component after the current system phase.</summary>
    public void AddComponent<T>(Entity entity, T component) where T : unmanaged =>
        Engine.QueueAdd(entity, component);

    /// <summary>Queue removing one component after the current system phase.</summary>
    public void RemoveComponent<T>(Entity entity) where T : unmanaged =>
        Engine.QueueRemove<T>(entity);

    /// <summary>Queue entity destruction after the current system phase.</summary>
    public void DestroyEntity(Entity entity) => Engine.QueueDestroy(entity);
}

/// <summary>Managed component value retained until a creation command is queued.</summary>
internal readonly record struct DeferredComponentValue(
    StableComponentId Id, Type Type, byte[] Bytes);

/// <summary>Fluent, allocation-backed builder for one deferred entity.</summary>
public sealed class DeferredEntityBuilder
{
    private readonly List<DeferredComponentValue> _components = [];
    private readonly HashSet<UInt128> _ids = [];
    private bool _built;

    /// <summary>Add an unmanaged component value to this new entity.</summary>
    public DeferredEntityBuilder With<T>(T component) where T : unmanaged
    {
        if (_built)
            throw new InvalidOperationException("A deferred entity builder can only be built once.");
        StableComponentId id = ComponentTypeMetadata<T>.StableId;
        UInt128 key = ((UInt128)id.High << 64) | id.Low;
        if (!_ids.Add(key))
            throw new InvalidOperationException(
                $"Entity creation contains component {typeof(T).FullName} more than once.");
        byte[] bytes = new byte[ComponentTypeMetadata<T>.Size];
        MemoryMarshal.Write(bytes, in component);
        _components.Add(new DeferredComponentValue(id, typeof(T), bytes));
        return this;
    }

    /// <summary>Reserve the entity handle and queue its atomic creation.</summary>
    public Entity Build()
    {
        if (_built)
            throw new InvalidOperationException("A deferred entity builder can only be built once.");
        _built = true;
        Entity entity = Engine.ReserveEntity();
        var handles = new GCHandle[_components.Count];
        Span<NativeComponentBlob> blobs = _components.Count <= 64
            ? stackalloc NativeComponentBlob[_components.Count]
            : new NativeComponentBlob[_components.Count];
        try
        {
            for (int index = 0; index < _components.Count; index++)
            {
                DeferredComponentValue component = _components[index];
                handles[index] = GCHandle.Alloc(component.Bytes, GCHandleType.Pinned);
                blobs[index] = new NativeComponentBlob
                {
                    ComponentKey = component.Id.Low,
                    ComponentKeyHigh = component.Id.High,
                    Data = handles[index].AddrOfPinnedObject(),
                    Size = checked((uint)component.Bytes.Length),
                };
            }
            Engine.QueueCreate(entity, blobs);
            return entity;
        }
        finally
        {
            foreach (GCHandle handle in handles)
                if (handle.IsAllocated)
                    handle.Free();
        }
    }
}

// =============================================================================
// Stack-only Row Views
// =============================================================================

/// <summary>ABI-compatible managed entity handle.</summary>
[StructLayout(LayoutKind.Sequential)]
public readonly struct Entity
{
    public readonly ulong Id;
    public readonly uint Generation;

    public Entity(ulong id, uint generation) => (Id, Generation) = (id, generation);
}

internal readonly record struct ArchetypeKey(ulong Low, ulong High);

internal struct QueryColumn
{
    internal QueryTermDescriptor Term;
    /// <summary>Invocation the chunk behind this column was issued to.</summary>
    internal uint ScopeToken;
    internal IntPtr Data;
    internal int Length;
    internal bool Present;
    internal IntPtr Ticks;
    internal uint ChangeTick;
}

/// <summary>Optional writable reference valid only for the current row.</summary>
public readonly unsafe ref struct OptionalWriteRef<T> where T : unmanaged
{
    private readonly T* _value;
    private readonly NativeComponentTicks* _ticks;
    private readonly uint _changeTick;
    internal OptionalWriteRef(T* value, NativeComponentTicks* ticks, uint changeTick)
    {
        _value = value;
        _ticks = ticks;
        _changeTick = changeTick;
    }
    public bool HasValue => _value != null;
    public ref T Value
    {
        get
        {
            if (_value == null)
                throw new InvalidOperationException($"Optional component {typeof(T).FullName} is absent.");
            if (_ticks == null)
                throw new InvalidOperationException(
                    $"Writable component {typeof(T).FullName} has no native change-tick column.");
            _ticks->Changed = _changeTick;
            return ref *_value;
        }
    }
}

/// <summary>Optional read-only reference valid only for the current row.</summary>
public readonly unsafe ref struct OptionalReadRef<T> where T : unmanaged
{
    private readonly T* _value;
    internal OptionalReadRef(T* value) => _value = value;
    public bool HasValue => _value != null;
    public ref readonly T Value
    {
        get
        {
            if (_value == null)
                throw new InvalidOperationException($"Optional component {typeof(T).FullName} is absent.");
            return ref *_value;
        }
    }
}

/// <summary>Stack-only view over one joined ECS row.</summary>
public readonly unsafe ref struct QueryRow
{
    private readonly QueryColumn _c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7;
    private readonly int _count;
    private readonly int _row;

    internal QueryRow(
        QueryColumn c0, QueryColumn c1, QueryColumn c2, QueryColumn c3,
        QueryColumn c4, QueryColumn c5, QueryColumn c6, QueryColumn c7,
        int count, int row) =>
        (_c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7, _count, _row) =
        (c0, c1, c2, c3, c4, c5, c6, c7, count, row);

    /// <summary>Borrow a required writable component declared by Write&lt;T&gt;.</summary>
    public ref T Write<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Write, optional: false);
        MarkChanged(column);
        return ref ((T*)column.Data)[_row];
    }

    /// <summary>Borrow a required read-only component declared by Read&lt;T&gt;.</summary>
    public ref readonly T Read<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Read, optional: false);
        return ref ((T*)column.Data)[_row];
    }

    /// <summary>Borrow an optional writable component when it is present.</summary>
    public OptionalWriteRef<T> OptionalWrite<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Write, optional: true);
        return new OptionalWriteRef<T>(
            column.Present ? &((T*)column.Data)[_row] : null,
            column.Present ? &((NativeComponentTicks*)column.Ticks)[_row] : null,
            column.ChangeTick);
    }

    /// <summary>Borrow an optional read-only component when it is present.</summary>
    public OptionalReadRef<T> OptionalRead<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Read, optional: true);
        return new OptionalReadRef<T>(column.Present ? &((T*)column.Data)[_row] : null);
    }

    /// <summary>Return the current entity declared by EntityTerm.</summary>
    public Entity Entity
    {
        get
        {
            for (var i = 0; i < _count; i++)
            {
                QueryColumn column = Column(i);
                if (column.Term.IsEntity)
                    return ((Entity*)column.Data)[_row];
            }
            throw new InvalidOperationException("This query does not declare EntityTerm.");
        }
    }

    private QueryColumn Find<T>(QueryAccess access, bool optional) where T : unmanaged
    {
        StableComponentId key = ComponentTypeMetadata<T>.StableId;
        for (var i = 0; i < _count; i++)
        {
            QueryColumn column = Column(i);
            if (!column.Term.IsEntity && column.Term.ComponentKey == key.Low &&
                column.Term.ComponentKeyHigh == key.High &&
                column.Term.Access == access && column.Term.Optional == optional)
            {
                // Every typed accessor resolves its column here, so one check
                // covers Read, Write, OptionalRead and OptionalWrite. Compiled
                // out of release builds.
                if (column.Present)
                    Engine.ValidateChunkScope(column.ScopeToken, typeof(T).FullName ?? typeof(T).Name);
                return column;
            }
        }
        throw new InvalidOperationException(
            $"This query does not declare {(optional ? "optional " : "")}{access.ToString().ToLowerInvariant()} " +
            $"access to {typeof(T).FullName}.");
    }

    private void MarkChanged(QueryColumn column)
    {
        if (column.Ticks == IntPtr.Zero)
            throw new InvalidOperationException(
                $"Writable component {column.Term.ComponentType!.FullName} has no native change-tick column.");
        ((NativeComponentTicks*)column.Ticks)[_row].Changed = column.ChangeTick;
    }

    private QueryColumn Column(int index)
    {
        switch (index)
        {
            case 0: return _c0;
            case 1: return _c1;
            case 2: return _c2;
            case 3: return _c3;
            case 4: return _c4;
            case 5: return _c5;
            case 6: return _c6;
            case 7: return _c7;
            default: throw new ArgumentOutOfRangeException(nameof(index));
        }
    }
}

// =============================================================================
// Shared Query Iterator
// =============================================================================

/// <summary>Stack-only iterator shared by every composable query shape.</summary>
public ref struct QueryEnumerator
{
    private readonly QueryTermDescriptor[] _terms;
    // The joined columns live in one array so a typed row can address every
    // slot through a single reference to element zero.
    private readonly QueryColumn[] _columns;
    private readonly int _driver;
    private uint _nextDriverChunk;
    private int _row;
    private int _length;

    internal QueryEnumerator(QueryDescriptor descriptor)
    {
        _terms = descriptor.TermArray;
        _columns = new QueryColumn[8];
        _driver = FindDriver(_terms);
        _nextDriverChunk = 0;
        _row = -1;
        _length = 0;
    }

    /// <summary>Return the current stack-only joined row.</summary>
    public QueryRow Current => new(
        _columns[0],
        _columns[1],
        _columns[2],
        _columns[3],
        _columns[4],
        _columns[5],
        _columns[6],
        _columns[7],
        _terms.Length,
        _row);

    /// <summary>Advance one row inside the joined chunk.</summary>
    /// <remarks>
    /// This is the per-row hot path, so it is deliberately one branch: the
    /// typed enumerator's MoveNext inlines it, and everything that happens
    /// once per chunk lives in <see cref="MoveToNextChunk"/>. Merging the two
    /// makes MoveNext big enough that the JIT stops inlining it, which
    /// measurably moves the per-row cost from ~0.4 ns to ~3 ns.
    /// </remarks>
    internal bool TryAdvanceRow() => ++_row < _length;

    /// <summary>Join the next archetype chunk; runs once per chunk, not per row.</summary>
    internal bool MoveToNextChunk()
    {
        while (TryLoadDriver(_nextDriverChunk++, out var archetype, out var driverChunk))
        {
            _length = checked((int)driverChunk.Length);
            if (_length == 0)
                continue;

            var matched = true;
            for (var i = 0; i < _terms.Length; i++)
            {
                QueryTermDescriptor term = _terms[i];
                NativeComponentChunk chunk;
                bool present;
                if (i == _driver)
                {
                    chunk = driverChunk;
                    present = true;
                }
                else
                {
                    present = Engine.TryGetArchetypeChunk(
                        term, archetype.Low, archetype.High, out chunk);
                    if (!present && !term.Optional)
                    {
                        matched = false;
                        break;
                    }
                }

                if (present && chunk.Length != driverChunk.Length)
                    throw new InvalidOperationException("ECS component chunk lengths are inconsistent.");
                // Entity rows arrive in the const `Entities` slot, which the
                // native ABI leaves the writable `Data` slot null for; the
                // joined column keeps one row-base field either way.
                IntPtr rows = term.IsEntity ? chunk.Entities : chunk.Data;
                SetColumn(i, new QueryColumn
                {
                    Term = term,
                    Data = rows,
                    Length = present ? checked((int)chunk.Length) : 0,
                    Present = present,
                    Ticks = present ? chunk.Ticks : IntPtr.Zero,
                    ChangeTick = present ? chunk.ChangeTick : 0,
                    ScopeToken = present ? chunk.ScopeToken : 0,
                });
            }

            if (!matched)
                continue;
            _row = 0;
            return true;
        }
        return false;
    }

    /// <summary>Advance within the current archetype or join the next one.</summary>
    public bool MoveNext()
    {
        if (TryAdvanceRow())
            return true;
        return MoveToNextChunk();
    }

    private static int FindDriver(QueryTermDescriptor[] terms)
    {
        for (var i = 0; i < terms.Length; i++)
            if (!terms[i].IsEntity && !terms[i].Optional)
                return i;
        for (var i = 0; i < terms.Length; i++)
            if (terms[i].IsEntity)
                return i;
        return -1; // An implicit entity driver makes optional-only queries useful.
    }

    private bool TryLoadDriver(
        uint index, out ArchetypeKey archetype, out NativeComponentChunk chunk)
    {
        bool found = _driver >= 0 && !_terms[_driver].IsEntity
            ? Engine.TryGetChunk(_terms[_driver], index, out chunk)
            : Engine.TryGetEntityChunk(index, out chunk);
        archetype = found
            ? new ArchetypeKey(chunk.ArchetypeLow, chunk.ArchetypeHigh)
            : default;
        return found;
    }

    private void SetColumn(int index, QueryColumn value) => _columns[index] = value;

    /// <summary>Reference to one joined column, for the typed row wrappers.</summary>
    /// <remarks>
    /// Ref-returning an array element needs the explicit [UnscopedRef] opt-in.
    /// The contract is narrow: only the typed enumerator calls this, and the
    /// row it feeds never outlives the enumerator that owns the columns.
    /// </remarks>
    [UnscopedRef]
    internal ref QueryColumn ColumnRef(int index) => ref _columns[index];

    /// <summary>Index of the current row inside the joined chunk.</summary>
    internal int RowIndex => _row;
}

// =============================================================================
// Composable Query Arities
// =============================================================================

/// <summary>Shared behavior for all composable query arities.</summary>
public abstract class QueryBase : IQueryDescriptor
{
    protected QueryBase(QueryDescriptor descriptor) => Descriptor = descriptor;
    public QueryDescriptor Descriptor { get; }
    public QueryEnumerator GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1> : QueryBase where T1 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, None, None, None, None, None, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, None, None, None, None, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, None, None, None, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3, T4> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm where T4 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, T4, None, None, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3, T4, T5> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm where T4 : IQueryTerm where T5 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, T4, T5, None, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3, T4, T5, T6> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm where T4 : IQueryTerm where T5 : IQueryTerm where T6 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, T4, T5, T6, None, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3, T4, T5, T6, T7> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm where T4 : IQueryTerm where T5 : IQueryTerm where T6 : IQueryTerm where T7 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6), typeof(T7));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, T4, T5, T6, T7, None> GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1, T2, T3, T4, T5, T6, T7, T8> : QueryBase where T1 : IQueryTerm where T2 : IQueryTerm where T3 : IQueryTerm where T4 : IQueryTerm where T5 : IQueryTerm where T6 : IQueryTerm where T7 : IQueryTerm where T8 : IQueryTerm
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6), typeof(T7), typeof(T8));
    public Query() : base(Cached) { }
    public new QueryEnumerator<T1, T2, T3, T4, T5, T6, T7, T8> GetEnumerator() => new(Descriptor);
}
