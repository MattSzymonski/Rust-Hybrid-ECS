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

[StructLayout(LayoutKind.Sequential)]
public struct Position
{
    public float X;
    public float Y;
}

[StructLayout(LayoutKind.Sequential)]
public struct Color
{
    public float R;
    public float G;
    public float B;
    public float A;
}

[StructLayout(LayoutKind.Sequential)]
public struct Sprite
{
    public float Width;
    public float Height;
    public Color Color;
}
