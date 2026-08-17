using System.Runtime.InteropServices;

namespace TracyLive;

[StructLayout(LayoutKind.Sequential)]
public struct PhysicsState
{
    public float DeltaTime;
    public float PositionX;
    public float PositionY;
    public float VelocityX;
    public float VelocityY;
    public float Radius;
    public byte Active;
}

// Project-owned component: the native host discovers and registers this layout
// from the managed component manifest without a Rust mirror or match arm.
[StructLayout(LayoutKind.Sequential)]
public struct BallTag
{
    public uint Kind;
}
