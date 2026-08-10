using System.Runtime.InteropServices;

namespace TracyLive.Loader;

[StructLayout(LayoutKind.Sequential)]
public struct NativeSystemAccess
{
    public ulong ComponentKey;
    public byte Mode; // 0 = read, 1 = write
}

/// <summary>Stable native entry points used by the Rust scheduler bridge.</summary>
public static unsafe class LoaderInterop
{
    private static GameHost? _host;

    [UnmanagedCallersOnly]
    public static byte Init(IntPtr api)
    {
        try
        {
            Engine.Bind(api);
            var dir = Environment.GetEnvironmentVariable("TRACY_LIVE_MANAGED_DIR")
                ?? AppContext.BaseDirectory;
            _host = new GameHost(Path.Combine(dir, "tracy_live_game_cs.dll"));
            _host.Init();
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] Init failed: {e}");
            return 0;
        }
    }

    [UnmanagedCallersOnly]
    public static uint SystemCount() => (uint)(_host?.SystemCount ?? 0);

    [UnmanagedCallersOnly]
    public static uint SystemAccessCount(uint systemIndex)
    {
        try
        {
            return (uint)(_host?.GetAccessCount(checked((int)systemIndex)) ?? 0);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] SystemAccessCount failed: {e}");
            return 0;
        }
    }

    [UnmanagedCallersOnly]
    public static byte GetSystemAccess(uint systemIndex, uint accessIndex, NativeSystemAccess* output)
    {
        try
        {
            if (_host is null || output is null)
                return 0;
            var access = _host.GetAccess(checked((int)systemIndex), checked((int)accessIndex));
            output->ComponentKey = access.ComponentKey;
            output->Mode = access.Mode;
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] GetSystemAccess failed: {e}");
            return 0;
        }
    }

    [UnmanagedCallersOnly]
    public static void RunSystem(uint systemIndex)
    {
        try
        {
            _host?.RunSystem(checked((int)systemIndex));
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] system {systemIndex} failed: {e}");
        }
    }

    [UnmanagedCallersOnly]
    public static void PollReload()
    {
        try
        {
            _host?.PollReload();
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] PollReload failed: {e}");
        }
    }
}
