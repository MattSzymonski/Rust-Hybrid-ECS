using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>
/// Mirror of the Rust <c>EngineApi</c> struct: a table of native function
/// pointers into the engine. Field order and signatures MUST stay in
/// lockstep with <c>examples/tracy_live/hot_cs.rs</c>. All pointers use the
/// C (cdecl) convention.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public unsafe struct EngineApi
{
    public delegate* unmanaged[Cdecl]<uint> EntityCount;
    public delegate* unmanaged[Cdecl]<ulong, byte, uint, NativeComponentChunk*, byte> GetComponentChunk;
}

[StructLayout(LayoutKind.Sequential)]
public struct NativeComponentChunk
{
    internal ulong ArchetypeLow;
    internal ulong ArchetypeHigh;
    internal IntPtr Data;
    internal uint Length;
    internal uint ElementSize;
}
