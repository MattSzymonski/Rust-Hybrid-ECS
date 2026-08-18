// Canonical managed mirrors of components owned by the native engine.
//
// Project assemblies reference these definitions through csharp_runtime so their
// names and layouts cannot silently diverge between independently reloaded
// gameplay projects.

using System.Runtime.InteropServices;

namespace TracyLive;

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
