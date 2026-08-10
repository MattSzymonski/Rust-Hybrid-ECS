using System.Runtime.InteropServices;

namespace TracyLive;

// Game components live in the reloadable script assembly. Any unmanaged
// struct whose name is registered by the Rust host can be queried; the
// loader itself contains no per-component knowledge.
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
