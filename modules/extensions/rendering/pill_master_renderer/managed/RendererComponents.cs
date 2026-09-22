// Managed mirrors of the renderer's components.
//
// These match the `#[repr(C)]` Rust definitions in `../src/component.rs`
// field for field: same names, same order, same types. The host binds them by
// canonical name and validates the layouts against the schema strings in
// `pill_host::csharp::components`, so a divergence is rejected at startup
// rather than corrupting memory.
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
[StructLayout(LayoutKind.Sequential)]
public struct PbrRenderableComponent {
    public ulong Mesh, Material;
    public float R, G, B, A;
    public float Metallic, Roughness;
    public byte Visible;
    public static PbrRenderableComponent FromColor(Color color) => new() { R=color.R,G=color.G,B=color.B,A=color.A,Roughness=0.5f,Visible=1 };
}
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct TransformComponent {
    public float X,Y,Z,RotationX,RotationY,RotationZ,RotationW,ScaleX,ScaleY,ScaleZ;
    public static TransformComponent At(float x,float y,float z,float scale) => new() { X=x,Y=y,Z=z,RotationW=1,ScaleX=scale,ScaleY=scale,ScaleZ=scale };
}
[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct CameraComponent {
    public byte Enabled; public int Priority; public float VerticalFov, Near, Far;
}

[EcsSharedComponent]
[StructLayout(LayoutKind.Sequential)]
public struct DirectionalLightComponent {public float R,G,B,Intensity;}
