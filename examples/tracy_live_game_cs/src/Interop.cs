using System.Runtime.InteropServices;

namespace TracyLive;

/// <summary>
/// The native boundary <c>TracyLive.Loader.GameHost</c> resolves by name
/// (via reflection, since this assembly is reloaded through a collectible
/// <see cref="System.Runtime.Loader.AssemblyLoadContext"/> rather than
/// hostfxr directly).
///
/// Signatures use <see cref="IntPtr"/>/<c>float</c> only — never a raw
/// pointer type — so this file compiles with this project's
/// <c>AllowUnsafeBlocks</c> left off. The one place a pointer needs
/// constructing (<c>Engine.Bind</c>) does that internally, in the
/// unsafe-allowed loader project.
///
/// Bodies are wrapped in try/catch because a managed exception must never
/// unwind across the native call boundary.
/// </summary>
public static class Interop
{
    [UnmanagedCallersOnly]
    public static void Init(IntPtr api)
    {
        try
        {
            Engine.Bind(api);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs] Init failed: {e}");
        }
    }

    [UnmanagedCallersOnly]
    public static void Update(float dt)
    {
        try
        {
            MovementSystem.Run();
            HealthDecaySystem.Run();
            GravitySystem.Run();
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs] Update failed: {e}");
        }
    }
}
