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
            var dir = Environment.GetEnvironmentVariable("ECS_CS_GAME_DIR")
                ?? AppContext.BaseDirectory;
            var assembly = Environment.GetEnvironmentVariable("ECS_CS_GAME_ASSEMBLY")
                ?? "game_cs.dll";
            _host = new GameHost(Path.Combine(dir, assembly));
            _host.Init();
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[cs_runtime] Init failed: {e}");
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
            Console.Error.WriteLine($"[cs_runtime] SystemAccessCount failed: {e}");
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
            Console.Error.WriteLine($"[cs_runtime] GetSystemAccess failed: {e}");
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
            Console.Error.WriteLine($"[cs_runtime] system {systemIndex} failed: {e}");
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
            Console.Error.WriteLine($"[cs_runtime] PollReload failed: {e}");
        }
    }
}
