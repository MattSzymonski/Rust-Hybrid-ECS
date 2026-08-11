// Stable unmanaged exports consumed by the Rust C# backend.
//
// Responsibilities:
// - Initializes the managed Engine facade and collectible game loader.
// - Exposes discovered system counts and access declarations to Rust.
// - Dispatches scheduled system calls and polls assembly hot reload.
// - Converts managed exceptions into diagnostics and failure status codes.
//
// Design:
// - Methods are marked UnmanagedCallersOnly and resolved through hostfxr.
// - No exception may cross the native ABI boundary; every exported operation
//   catches failures locally and returns a neutral status where applicable.

using System.Runtime.InteropServices;

namespace TracyLive.Loader;

// =============================================================================
// Native Access Layout
// =============================================================================

/// <summary>ABI mirror of the Rust scheduler-access record.</summary>
[StructLayout(LayoutKind.Sequential)]
public struct NativeSystemAccess
{
    /// <summary>Low half of the stable 128-bit component ID.</summary>
    public ulong ComponentKey;

    /// <summary>High half of the stable 128-bit component identifier.</summary>
    public ulong ComponentKeyHigh;

    /// <summary>Access mode: zero for read, one for write.</summary>
    public byte Mode;
}

// =============================================================================
// Unmanaged Entry Points
// =============================================================================

/// <summary>Stable native entry points used by the Rust scheduler bridge.</summary>
public static unsafe class LoaderInterop
{
    // The stable runtime owns exactly one active collectible game loader.
    private static GameHost? _host;

    /// <summary>Bind the native API and load the initial gameplay assembly.</summary>
    /// <returns>One on success; zero after reporting an initialization error.</returns>
    [UnmanagedCallersOnly]
    public static byte Init(IntPtr api)
    {
        try
        {
            Engine.Bind(api);
            var dir = Environment.GetEnvironmentVariable("ECS_CSHARP_GAME_DIR")
                ?? AppContext.BaseDirectory;
            var assembly = Environment.GetEnvironmentVariable("ECS_CSHARP_GAME_ASSEMBLY")
                ?? "game_cs.dll";
            _host = new GameHost(Path.Combine(dir, assembly));
            _host.Init();
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] Init failed: {e}");
            return 0;
        }
    }

    /// <summary>Return the number of systems in the active game version.</summary>
    [UnmanagedCallersOnly]
    public static uint SystemCount() => (uint)(_host?.SystemCount ?? 0);

    /// <summary>Return the number of one-shot startup methods.</summary>
    [UnmanagedCallersOnly]
    public static uint StartupCount() => (uint)(_host?.StartupCount ?? 0);

    /// <summary>Return whether a system declared the Commands parameter.</summary>
    [UnmanagedCallersOnly]
    public static byte SystemUsesCommands(uint systemIndex)
    {
        try
        {
            return _host?.UsesCommands(checked((int)systemIndex)) == true ? (byte)1 : (byte)0;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] SystemUsesCommands failed: {e}");
            return 0;
        }
    }

    /// <summary>Run one startup method before the first frame.</summary>
    [UnmanagedCallersOnly]
    public static byte RunStartup(uint startupIndex)
    {
        try
        {
            if (_host is null)
                return 0;
            _host.RunStartup(checked((int)startupIndex));
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] startup {startupIndex} failed: {e}");
            return 0;
        }
    }

    /// <summary>Return the UTF-8 JSON component manifest byte count.</summary>
    [UnmanagedCallersOnly]
    public static uint ComponentManifestLength() =>
        checked((uint)(_host?.ComponentManifest.Length ?? 0));

    /// <summary>Copy the complete UTF-8 JSON component manifest.</summary>
    [UnmanagedCallersOnly]
    public static byte CopyComponentManifest(byte* output, uint capacity)
    {
        try
        {
            if (_host is null || output is null || capacity < _host.ComponentManifest.Length)
                return 0;
            _host.ComponentManifest.CopyTo(new Span<byte>(output, checked((int)capacity)));
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] CopyComponentManifest failed: {e}");
            return 0;
        }
    }

    /// <summary>Return the number of scheduler accesses for one system.</summary>
    [UnmanagedCallersOnly]
    public static uint SystemAccessCount(uint systemIndex)
    {
        try
        {
            return (uint)(_host?.GetAccessCount(checked((int)systemIndex)) ?? 0);
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] SystemAccessCount failed: {e}");
            return 0;
        }
    }

    /// <summary>Copy one scheduler access into native-owned output storage.</summary>
    /// <returns>One on success or zero for invalid input/failure.</returns>
    [UnmanagedCallersOnly]
    public static byte GetSystemAccess(uint systemIndex, uint accessIndex, NativeSystemAccess* output)
    {
        try
        {
            if (_host is null || output is null)
                return 0;
            var access = _host.GetAccess(checked((int)systemIndex), checked((int)accessIndex));
            output->ComponentKey = access.ComponentKey;
            output->ComponentKeyHigh = access.ComponentKeyHigh;
            output->Mode = access.Mode;
            return 1;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] GetSystemAccess failed: {e}");
            return 0;
        }
    }

    /// <summary>Run one managed system selected by its stable discovery index.</summary>
    [UnmanagedCallersOnly]
    public static void RunSystem(uint systemIndex)
    {
        try
        {
            _host?.RunSystem(checked((int)systemIndex));
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] system {systemIndex} failed: {e}");
        }
    }

    /// <summary>Poll and apply a behavior-compatible gameplay assembly reload.</summary>
    [UnmanagedCallersOnly]
    public static void PollReload()
    {
        try
        {
            _host?.PollReload();
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] PollReload failed: {e}");
        }
    }
}
