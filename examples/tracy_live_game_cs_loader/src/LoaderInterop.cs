using System.Runtime.InteropServices;

namespace TracyLive.Loader;

/// <summary>
/// The stable native boundary. The Rust host resolves these
/// <see cref="UnmanagedCallersOnlyAttribute"/> entry points by name
/// (<c>TracyLive.Loader.LoaderInterop, tracy_live_game_cs_loader</c>)
/// instead of going straight to <c>TracyLive.Interop</c> in
/// <c>tracy_live_game_cs.dll</c>.
///
/// Unlike <c>tracy_live_game_cs.dll</c>, this assembly is loaded once via
/// hostfxr into .NET's default (non-collectible) load context and never
/// reloaded — so the addresses Rust caches for <see cref="Init"/>/
/// <see cref="Update"/> stay valid forever. The actual game gets reloaded
/// underneath, inside <see cref="GameHost"/>, through its own collectible
/// <see cref="System.Runtime.Loader.AssemblyLoadContext"/>.
/// </summary>
public static unsafe class LoaderInterop
{
    private static GameHost? _host;

    [UnmanagedCallersOnly]
    public static void Init(IntPtr api)
    {
        try
        {
            var dir = Environment.GetEnvironmentVariable("TRACY_LIVE_MANAGED_DIR")
                ?? AppContext.BaseDirectory;
            var assemblyPath = Path.Combine(dir, "tracy_live_game_cs.dll");
            _host = new GameHost(assemblyPath);
            _host.Init(api);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] Init failed: {e}");
        }
    }

    [UnmanagedCallersOnly]
    public static void Update(float dt)
    {
        try
        {
            _host?.Update(dt);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] Update failed: {e}");
        }
    }
}
