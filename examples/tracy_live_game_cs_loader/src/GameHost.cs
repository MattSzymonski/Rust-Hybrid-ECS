using System.Reflection;
using System.Runtime.Loader;

namespace TracyLive.Loader;

/// <summary>
/// Owns a collectible <see cref="AssemblyLoadContext"/> for
/// <c>tracy_live_game_cs.dll</c> and forwards Init/Update into whichever
/// version is currently loaded. Polls the assembly's last-write time
/// periodically and reloads when it changes — the caller
/// (<see cref="LoaderInterop"/>) doesn't need to know anything happened.
///
/// Unlike Flappy's <c>game_cs_loader</c>, the entity population here lives
/// in Rust-owned native memory (the ECS archetype storage), not on the
/// managed heap — so unloading the old context on reload does *not* reset
/// gameplay state the way it does for Flappy. Only the code changes; the
/// data survives.
///
/// Everything here runs on the dedicated <c>cs-script-worker</c> thread
/// `hot_cs.rs` spawns for exactly this purpose (see that file's watchdog
/// docs), never on the engine's main thread.
/// </summary>
internal sealed unsafe class GameHost
{
    private sealed class GameContext : AssemblyLoadContext
    {
        public GameContext() : base(isCollectible: true) { }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            // tracy_live_game_cs.dll references tracy_live_game_cs_loader
            // (for Engine.cs's Span<T> facade and the component structs) —
            // it must resolve to the *exact same* already-loaded instance
            // this code is itself running as, not a fresh copy loaded from
            // disk. A second, separately-loaded copy would mean
            // `TracyLive.Engine`'s static `_api` field (bound once by
            // `LoaderInterop.Init`) is a different static than the one
            // `Systems.cs` reads from, breaking the whole Init/Update
            // hand-off silently.
            //
            // Note: hostfxr's component-hosting mode does NOT load this
            // assembly into `AssemblyLoadContext.Default` (verified — it's
            // absent from `Default.Assemblies`), so that's not a usable
            // lookup path. `typeof(GameHost).Assembly` sidesteps the
            // question of which context hostfxr actually used: it's always
            // exactly the assembly this code is currently executing as.
            if (assemblyName.Name == "tracy_live_game_cs_loader")
            {
                return typeof(GameHost).Assembly;
            }
            return null; // Let the default context resolve shared BCL assemblies.
        }
    }

    /// Check the file's timestamp roughly every half second rather than every
    /// frame — cheap, but avoids a syscall on every single frame.
    private const int PollEveryNFrames = 30;

    private readonly string _assemblyPath;

    private GameContext? _context;
    private delegate* unmanaged[Cdecl]<IntPtr, void> _init;
    private delegate* unmanaged[Cdecl]<float, void> _update;

    private IntPtr _api;
    private DateTime _lastWriteUtc;
    private int _frameCounter;

    public GameHost(string assemblyPath)
    {
        _assemblyPath = assemblyPath;
    }

    public void Init(IntPtr api)
    {
        _api = api;
        Load();
    }

    public void Update(float dt)
    {
        if (++_frameCounter >= PollEveryNFrames)
        {
            _frameCounter = 0;
            MaybeReload();
        }

        if (_update != null)
        {
            _update(dt);
        }
    }

    private void MaybeReload()
    {
        DateTime written;
        try
        {
            written = File.GetLastWriteTimeUtc(_assemblyPath);
        }
        catch (IOException)
        {
            return; // File briefly missing/locked mid-build — try again later.
        }

        if (written <= _lastWriteUtc)
        {
            return;
        }

        try
        {
            Load();
            Console.WriteLine("[tracy_live_game_cs_loader] reloaded tracy_live_game_cs.dll");
        }
        catch (Exception e)
        {
            // Leave the previous (working) version in place.
            Console.Error.WriteLine($"[tracy_live_game_cs_loader] reload failed: {e}");
        }
    }

    private void Load()
    {
        _lastWriteUtc = File.GetLastWriteTimeUtc(_assemblyPath);

        var bytes = File.ReadAllBytes(_assemblyPath);
        var context = new GameContext();
        Assembly assembly;
        using (var stream = new MemoryStream(bytes))
        {
            assembly = context.LoadFromStream(stream);
        }

        var interopType = assembly.GetType("TracyLive.Interop")
            ?? throw new InvalidOperationException($"TracyLive.Interop not found in {_assemblyPath}");

        var init = (delegate* unmanaged[Cdecl]<IntPtr, void>)GetExport(interopType, "Init");
        var update = (delegate* unmanaged[Cdecl]<float, void>)GetExport(interopType, "Update");

        // Only swap over — and only unload the old context — once the new
        // one has fully resolved. If anything above throws, the previous
        // (working) version and its function pointers are left untouched.
        _context?.Unload();
        _context = context;
        _init = init;
        _update = update;

        _init(_api);
    }

    private static nint GetExport(Type type, string methodName)
    {
        var method = type.GetMethod(methodName, BindingFlags.Public | BindingFlags.Static)
            ?? throw new MissingMethodException(type.FullName, methodName);
        return method.MethodHandle.GetFunctionPointer();
    }
}
