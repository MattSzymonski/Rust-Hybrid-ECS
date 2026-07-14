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
    public delegate* unmanaged[Cdecl]<Position**, uint*, void> GetPositions;
    public delegate* unmanaged[Cdecl]<Velocity**, uint*, void> GetVelocities;
    public delegate* unmanaged[Cdecl]<Health**, uint*, void> GetHealths;
    public delegate* unmanaged[Cdecl]<Mass**, uint*, void> GetMasses;
    public delegate* unmanaged[Cdecl]<GravityForce**, uint*, void> GetGravityForces;
}
