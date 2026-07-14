using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>
/// Blittable mirrors of the Rust <c>#[repr(C)]</c> components defined in
/// <c>examples/tracy_live/cs_components.rs</c>. Layout must stay in
/// lockstep with that file, field for field.
///
/// These live in the loader project (not the reloadable
/// <c>tracy_live_game_cs</c> project) purely because <see cref="EngineApi"/>
/// needs them and the reference direction goes the other way
/// (<c>tracy_live_game_cs</c> references the loader). Plain data structs
/// carry no sandboxing risk regardless of which project they're compiled
/// in — only pointer-constructing *code* does.
/// </summary>
[StructLayout(LayoutKind.Sequential)]
public struct Position { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Velocity { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Health { public float Value; }

[StructLayout(LayoutKind.Sequential)]
public struct Mass { public float Value; }

[StructLayout(LayoutKind.Sequential)]
public struct GravityForce { public float X, Y; }
