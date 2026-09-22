// Native ABI declarations shared by the Rust host and managed runtime.
//
// Responsibilities:
// - Mirrors the Rust function table and component-chunk result layout.
// - Documents the exact calling convention used across the native boundary.
//
// Design:
// - These structs contain no behavior. Their sequential field order and sizes
//   are part of the ABI and must change in lockstep with host/src/csharp/abi.rs.

using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>
/// Mirror of the Rust <c>EngineApi</c> struct: a table of native function
/// pointers into the engine. All pointers use the C calling convention.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct EngineApi
{
    /// <summary>
    /// Write the entity count from the currently scheduled world.
    /// Status <c>0</c> wrote the count, <c>3</c> means no system is scheduled,
    /// and <c>5</c> means the caller passed no output buffer.
    /// </summary>
    public delegate* unmanaged[Cdecl]<uint*, byte> EntityCount;

    /// <summary>Request one native archetype column by component ID and mode.</summary>
    public delegate* unmanaged[Cdecl]<ulong, ulong, byte, uint, NativeComponentChunk*, byte> GetComponentChunk;

    /// <summary>Request one component's chunk within an already-known archetype.</summary>
    /// <remarks>Mode <c>2</c> requests the archetype's entity column.</remarks>
    public delegate* unmanaged[Cdecl]<ulong, ulong, ulong, ulong, byte, NativeComponentChunk*, byte> GetArchetypeChunk;

    /// <summary>Request one native archetype's entity-handle column.</summary>
    public delegate* unmanaged[Cdecl]<uint, NativeComponentChunk*, byte> GetEntityChunk;

    /// <summary>Reserve a generation-checked handle for deferred creation.</summary>
    public delegate* unmanaged[Cdecl]<Entity*, byte> ReserveEntity;

    /// <summary>Queue creation with an array of pinned component blobs.</summary>
    public delegate* unmanaged[Cdecl]<Entity*, NativeComponentBlob*, uint, byte> QueueCreate;

    /// <summary>Queue destruction of a currently live entity.</summary>
    public delegate* unmanaged[Cdecl]<Entity*, byte> QueueDestroy;

    /// <summary>Queue adding one native or runtime-defined component.</summary>
    public delegate* unmanaged[Cdecl]<Entity*, ulong, ulong, byte*, uint, byte> QueueAddComponent;

    /// <summary>Queue removing one component selected by stable identity.</summary>
    public delegate* unmanaged[Cdecl]<Entity*, ulong, ulong, byte> QueueRemoveComponent;

    /// <summary>Return how many mirrored Rust methods are registered.</summary>
    public delegate* unmanaged[Cdecl]<uint> MirrorMethodCount;

    /// <summary>Copy the mirrored-method rows into a caller-owned buffer.</summary>
    public delegate* unmanaged[Cdecl]<MirrorMethodEntry*, uint, uint> CopyMirrorMethods;

    /// <summary>
    /// Token of the managed invocation active on the calling thread, or zero
    /// when no scheduled system is running on it.
    /// </summary>
    public delegate* unmanaged[Cdecl]<uint> CurrentScopeToken;

    /// <summary>
    /// Epoch of the mirrored-method table: bumped every time the host
    /// republishes it, so managed code can notice a rebind without waiting for
    /// the assembly swap that would normally carry one.
    /// </summary>
    public delegate* unmanaged[Cdecl]<uint> MirrorEpoch;

    /// <summary>
    /// Fill a <see cref="NativeResourceView"/> for one resource the active
    /// system declared.
    /// </summary>
    /// <remarks>
    /// Arguments are the stable identity's low and high halves, the requested
    /// mode (<c>0</c> read, <c>1</c> write) and the output view. Status
    /// <c>0</c> filled it, <c>1</c> the identity names no registered resource,
    /// <c>2</c> the system did not declare this access, <c>3</c> no system is
    /// running on this thread, <c>4</c> the world holds no value yet, and
    /// <c>5</c> the caller passed no output buffer.
    /// </remarks>
    public delegate* unmanaged[Cdecl]<ulong, ulong, byte, NativeResourceView*, byte> GetResourceView;

    /// <summary>
    /// Decode a Wavefront OBJ buffer into a mesh, inserted into the active
    /// invocation's <c>AssetManager</c>. Appended after <see cref="GetResourceView"/>
    /// for the same reason every earlier addition was: a new slot goes at the
    /// end, never between existing ones.
    /// </summary>
    public delegate* unmanaged[Cdecl]<byte*, uint, byte*, uint, uint*, uint*, byte> AssetLoadMeshObj;

    /// <summary>Decode a PNG buffer into a color texture, inserted the same way.</summary>
    public delegate* unmanaged[Cdecl]<byte*, uint, byte*, uint, uint*, uint*, byte> AssetLoadTexturePng;

    /// <summary>Build a shader from managed WGSL sources and slot declarations.</summary>
    public delegate* unmanaged[Cdecl]<
        byte*, uint,
        byte*, uint,
        byte*, uint,
        NativeShaderParameterSlot*, uint,
        NativeShaderTextureSlot*, uint,
        byte, byte,
        uint*, uint*, byte> AssetLoadShader;

    /// <summary>Build a material from already-loaded handles and per-slot parameters.</summary>
    public delegate* unmanaged[Cdecl]<
        byte*, uint,
        uint, uint,
        NativeMaterialTexture*, uint,
        NativeMaterialScalar*, uint,
        NativeMaterialColor*, uint,
        byte,
        uint*, uint*, byte> AssetCreateMaterial;
}

/// <summary>One parameter slot a managed shader declaration supplies.</summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct NativeShaderParameterSlot
{
    public byte* Name;
    public uint NameLen;
    /// <summary><c>0</c> scalar, <c>1</c> bool, <c>2</c> color.</summary>
    public byte Kind;
}

/// <summary>
/// One texture slot a managed shader declaration supplies. The bound texture
/// is always color-typed.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct NativeShaderTextureSlot
{
    public byte* Name;
    public uint NameLen;
    public uint TextureBinding;
    public uint SamplerBinding;
}

/// <summary>One texture a managed material declaration binds to a shader slot.</summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct NativeMaterialTexture
{
    public byte* Slot;
    public uint SlotLen;
    public uint TextureIndex;
    public uint TextureGeneration;
}

/// <summary>One scalar parameter a managed material declaration sets.</summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct NativeMaterialScalar
{
    public byte* Name;
    public uint NameLen;
    public float Value;
}

/// <summary>One color parameter a managed material declaration sets.</summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct NativeMaterialColor
{
    public byte* Name;
    public uint NameLen;
    public float R;
    public float G;
    public float B;
}

/// <summary>
/// Borrowed view of one resource's bytes. Valid only for the managed system
/// invocation that asked for it.
/// </summary>
/// <remarks>
/// A resource is one value, so this carries far less than
/// <see cref="NativeComponentChunk"/>: no archetype identity, no row count and
/// no per-row tick column, only the bytes, their width, and the invocation the
/// view was issued to.
/// </remarks>
[StructLayout(LayoutKind.Sequential)]
public struct NativeResourceView
{
    /// <summary>Pointer to the resource's first byte in engine storage.</summary>
    internal IntPtr Data;

    /// <summary>Width of the stored value, checked against the managed struct.</summary>
    internal uint Length;

    /// <summary>Token of the managed invocation this view was issued to.</summary>
    internal uint ScopeToken;
}

/// <summary>
/// One mirrored Rust method the managed runtime can call through its exported
/// C-ABI trampoline. Field order and widths match the Rust host's
/// <c>MirrorMethodEntry</c>.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct MirrorMethodEntry
{
    /// <summary>Fully-qualified Rust type name, e.g. <c>pill_spline::OmoMO</c>.</summary>
    internal IntPtr TypeName;

    /// <summary>Rust method name, e.g. <c>get_sum</c>.</summary>
    internal IntPtr Method;

    /// <summary>Address of the exported <c>#[no_mangle]</c> C-ABI trampoline.</summary>
    internal IntPtr Address;
}

/// <summary>Pinned component value passed synchronously to native commands.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct NativeComponentBlob
{
    internal ulong ComponentKey;
    internal ulong ComponentKeyHigh;
    internal IntPtr Data;
    internal uint Size;
}

/// <summary>
/// Borrowed view of one native component column and its archetype identity.
/// The pointer is valid only for the active managed system invocation.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct NativeComponentChunk
{
    /// <summary>Low 64 bits of the native archetype identifier.</summary>
    internal ulong ArchetypeLow;

    /// <summary>High 64 bits of the native archetype identifier.</summary>
    internal ulong ArchetypeHigh;

    /// <summary>Pointer to the first component in the contiguous native column.</summary>
    /// <remarks>Null for entity columns, whose rows arrive in <see cref="Entities"/>.</remarks>
    internal IntPtr Data;

    /// <summary>Pointer to the first entity of an archetype's entity column.</summary>
    /// <remarks>
    /// Entity rows are const on the native side and arrive only through this
    /// slot, separate from the writable <see cref="Data"/>; the query join
    /// reads this one for entity terms.
    /// </remarks>
    internal IntPtr Entities;

    /// <summary>Number of component values in the column.</summary>
    internal uint Length;

    /// <summary>Native size of one component, used for ABI validation.</summary>
    internal uint ElementSize;

    /// <summary>Parallel per-row change metadata for component columns.</summary>
    internal IntPtr Ticks;

    /// <summary>World change tick current at managed system execution.</summary>
    internal uint ChangeTick;

    /// <summary>
    /// Token of the managed invocation this chunk was issued to.
    /// </summary>
    /// <remarks>
    /// Compared against the live token before the chunk is dereferenced in a
    /// debug build, so a chunk kept past the call that produced it is a named
    /// error instead of a read of storage that has since moved. Declared last
    /// to match the Rust struct, where it lands in tail padding the layout
    /// already carried and therefore costs no bytes.
    /// </remarks>
    internal uint ScopeToken;
}

/// <summary>ABI mirror of Rust's per-component change-detection metadata.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct NativeComponentTicks
{
    internal uint Added;
    internal uint Changed;
}
