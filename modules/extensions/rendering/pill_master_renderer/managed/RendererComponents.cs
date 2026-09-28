// Managed mirrors of the renderer's components.
//
// These mirror the `#[repr(C)]` Rust definitions in `../src/components/` in
// layout: field order, sizes and alignment, which is what shared memory needs.
// Field names need not match - the host binds fields by the schema strings in
// `pill_host::csharp::components` and validates each struct's size against the
// registered layout at startup, so a divergence is rejected there rather than
// corrupting memory.
//
// They live beside the renderer, not in csharp_runtime, for the same reason
// the Rust definitions do: a mesh is a renderer concept. A managed project
// that draws includes this file; one that does not, does not.

using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>World-space position of an entity's top-left draw origin, in pixels.</summary>
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct Position
{
    public float X;
    public float Y;
}

/// <summary>Plain RGBA color, 0.0-1.0 per channel.</summary>
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct Color
{
    public float R;
    public float G;
    public float B;
    public float A;
}

[EcsSharedComponent]
// Four uint fields, not two ulong: the native side is two `Handle<T>` (u32
// index + u32 generation, align 4 each). A `ulong` field would force the
// whole struct to the CLR's natural 8-byte alignment, and StructLayout.Pack
// cannot fix that - the analyzer rejects it (PILL0402) because the manifest
// generator does not model Pack-adjusted sizes.
[StructLayout(LayoutKind.Sequential)]
public struct MeshRendererComponent
{
    public uint MeshIndex, MeshGeneration, MaterialIndex, MaterialGeneration;

    /// <summary>
    /// "No mesh, no material". <c>default(MeshRendererComponent)</c> is four
    /// zeroes, and (index 0, generation 0) is the first asset ever added to
    /// its column - a live handle, not an absent one. A component that has
    /// not been given real handles yet should start from this.
    /// </summary>
    public static MeshRendererComponent None => new()
    {
        MeshIndex = uint.MaxValue,
        MeshGeneration = uint.MaxValue,
        MaterialIndex = uint.MaxValue,
        MaterialGeneration = uint.MaxValue,
    };
}
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct TransformComponent
{
    public float X, Y, Z, RotationX, RotationY, RotationZ, RotationW, ScaleX, ScaleY, ScaleZ;
    public static TransformComponent At(float x, float y, float z, float scale) => new() { X = x, Y = y, Z = z, RotationW = 1, ScaleX = scale, ScaleY = scale, ScaleZ = scale };
}
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct CameraComponent
{
    public byte Enabled; public int Priority; public float VerticalFov, Near, Far;
}

[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct DirectionalLightComponent { public float R, G, B, Intensity; }
