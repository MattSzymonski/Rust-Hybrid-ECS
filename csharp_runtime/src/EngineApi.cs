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
    /// <summary>Return the entity count from the currently scheduled world.</summary>
    public delegate* unmanaged[Cdecl]<uint> EntityCount;

    /// <summary>Request one native archetype column by component ID and mode.</summary>
    public delegate* unmanaged[Cdecl]<ulong, ulong, byte, uint, NativeComponentChunk*, byte> GetComponentChunk;

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
    internal IntPtr Data;

    /// <summary>Number of component values in the column.</summary>
    internal uint Length;

    /// <summary>Native size of one component, used for ABI validation.</summary>
    internal uint ElementSize;

    /// <summary>Parallel per-row change metadata for component columns.</summary>
    internal IntPtr Ticks;

    /// <summary>World change tick current at managed system execution.</summary>
    internal uint ChangeTick;
}

/// <summary>ABI mirror of Rust's per-component change-detection metadata.</summary>
[StructLayout(LayoutKind.Sequential)]
internal struct NativeComponentTicks
{
    internal uint Added;
    internal uint Changed;
}
