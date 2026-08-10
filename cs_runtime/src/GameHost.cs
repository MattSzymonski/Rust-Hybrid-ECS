// Collectible managed gameplay loader and ECS system discovery.
//
// Responsibilities:
// - Loads game assemblies without locking their build output.
// - Discovers and deterministically orders methods marked with EcsSystem.
// - Derives scheduler access from each method's single query parameter.
// - Reloads behavior while rejecting scheduler-signature changes.
//
// Design:
// - Every game version lives in a collectible AssemblyLoadContext and is read
//   from bytes so the compiler can replace the source DLL on Windows.
// - Rust builds its execution graph once. A reload may replace method bodies,
//   but names and access signatures must remain stable until host restart.

using System.Linq.Expressions;
using System.Reflection;
using System.Runtime.Loader;
using System.Threading;

namespace TracyLive.Loader;

// =============================================================================
// Discovered System Metadata
// =============================================================================

/// <summary>One component key and its native access mode.</summary>
internal readonly record struct ManagedAccess(ulong ComponentKey, byte Mode);

/// <summary>Compiled managed system plus its scheduler declaration.</summary>
internal sealed record ManagedSystem(string Name, ManagedAccess[] Accesses, Action Run)
{
    internal string Signature =>
        $"{Name}:{string.Join(',', Accesses.Select(a => $"{a.Mode}:{a.ComponentKey}"))}";
}

// =============================================================================
// GameHost
// =============================================================================

/// <summary>
/// Loads the collectible gameplay assembly and discovers methods marked with
/// <see cref="EcsSystemAttribute"/>. Each method's single query parameter is
/// both its executable iterator and the authoritative scheduler access list.
/// </summary>
internal sealed class GameHost
{
    /// <summary>
    /// Collectible context for one game version. Requests for cs_runtime types
    /// resolve to the already-loaded stable runtime assembly.
    /// </summary>
    private sealed class GameContext : AssemblyLoadContext
    {
        public GameContext() : base(isCollectible: true) { }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            if (assemblyName.Name == "cs_runtime")
                return typeof(GameHost).Assembly;
            return null;
        }
    }

    private static readonly TimeSpan PollInterval = TimeSpan.FromMilliseconds(500);
    private readonly string _assemblyPath;
    private GameContext? _context;
    private ManagedSystem[] _systems = [];
    private DateTime _lastWriteUtc;
    private DateTime _lastPollUtc;

    /// <summary>Create a loader for the configured gameplay assembly.</summary>
    public GameHost(string assemblyPath) => _assemblyPath = assemblyPath;

    /// <summary>Number of systems exposed by the active game version.</summary>
    public int SystemCount => _systems.Length;

    /// <summary>Return one reflected scheduler access.</summary>
    public ManagedAccess GetAccess(int systemIndex, int accessIndex) =>
        _systems[systemIndex].Accesses[accessIndex];

    /// <summary>Return the number of accesses declared by one system.</summary>
    public int GetAccessCount(int systemIndex) => _systems[systemIndex].Accesses.Length;

    /// <summary>Load the initial game assembly and compile its runners.</summary>
    public void Init() => Load(isReload: false);

    /// <summary>Invoke a discovered system by its stable index.</summary>
    public void RunSystem(int index) => _systems[index].Run();

    /// <summary>Poll the game DLL timestamp and reload a newer build.</summary>
    public void PollReload()
    {
        var now = DateTime.UtcNow;
        if (now - _lastPollUtc < PollInterval)
            return;
        _lastPollUtc = now;

        DateTime written;
        try
        {
            written = File.GetLastWriteTimeUtc(_assemblyPath);
        }
        catch (IOException)
        {
            return;
        }

        if (written <= _lastWriteUtc)
            return;

        try
        {
            Load(isReload: true);
            Console.WriteLine($"[cs_runtime] reloaded {Path.GetFileName(_assemblyPath)}");
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[cs_runtime] reload failed: {e}");
        }
    }

    /// <summary>
    /// Load one assembly version, validate its scheduler signature, then swap
    /// it atomically with the active collectible context.
    /// </summary>
    private void Load(bool isReload)
    {
        var bytes = ReadAllBytesWithRetry(_assemblyPath);
        var context = new GameContext();
        try
        {
            Assembly assembly;
            using (var stream = new MemoryStream(bytes))
                assembly = context.LoadFromStream(stream);

            var systems = DiscoverSystems(assembly);
            if (systems.Length == 0)
                throw new InvalidOperationException(
                    "No [EcsSystem] methods with a supported query parameter were found.");

            // Rust's execution graph is built at startup. Behavior-only reloads
            // are safe; changing a query signature needs a restart so Rust can
            // rebuild the scheduler metadata.
            if (isReload && !_systems.Select(s => s.Signature).SequenceEqual(
                    systems.Select(s => s.Signature)))
                throw new InvalidOperationException(
                    "C# system names or query signatures changed; restart the host to rebuild the Rust scheduler.");

            var oldContext = _context;
            _context = context;
            _systems = systems;
            _lastWriteUtc = File.GetLastWriteTimeUtc(_assemblyPath);
            oldContext?.Unload();
        }
        catch
        {
            context.Unload();
            throw;
        }
    }

    // =========================================================================
    // System Discovery and Compilation
    // =========================================================================

    /// <summary>Discover attributed static methods in deterministic order.</summary>
    internal static ManagedSystem[] DiscoverSystems(Assembly assembly)
    {
        return assembly.GetTypes()
            .SelectMany(type => type.GetMethods(
                BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static))
            .Where(method => method.GetCustomAttribute<EcsSystemAttribute>() is not null)
            .OrderBy(method => method.DeclaringType?.FullName, StringComparer.Ordinal)
            .ThenBy(method => method.Name, StringComparer.Ordinal)
            .Select(CreateSystem)
            .ToArray();
    }

    /// <summary>
    /// Validate one managed method, derive component access from its query
    /// type, and compile a parameterless runner for the Rust scheduler.
    /// </summary>
    internal static ManagedSystem CreateSystem(MethodInfo method)
    {
        if (method.ReturnType != typeof(void))
            throw new InvalidOperationException($"{method} must return void.");

        var parameters = method.GetParameters();
        if (parameters.Length != 1)
            throw new InvalidOperationException(
                $"{method} must have exactly one supported ECS query parameter.");

        Type queryType = parameters[0].ParameterType;
        if (!queryType.IsGenericType)
            throw UnsupportedQuery(method);

        Type generic = queryType.GetGenericTypeDefinition();
        Type[] components = queryType.GetGenericArguments();
        ManagedAccess[] accesses;
        if (generic == typeof(WriteQuery<>))
        {
            accesses = [new ManagedAccess(Engine.ComponentKey(components[0]), 1)];
        }
        else if (generic == typeof(WriteReadQuery<,>))
        {
            ulong write = Engine.ComponentKey(components[0]);
            ulong read = Engine.ComponentKey(components[1]);
            if (write == read)
                throw new InvalidOperationException($"{method} reads and writes the same component type.");
            accesses = [new ManagedAccess(write, 1), new ManagedAccess(read, 0)];
        }
        else if (generic == typeof(Write3Query<,,>))
        {
            ulong first = Engine.ComponentKey(components[0]);
            ulong second = Engine.ComponentKey(components[1]);
            ulong third = Engine.ComponentKey(components[2]);
            if (first == second || first == third || second == third)
                throw new InvalidOperationException($"{method} writes the same component type more than once.");
            accesses =
            [
                new ManagedAccess(first, 1),
                new ManagedAccess(second, 1),
                new ManagedAccess(third, 1),
            ];
        }
        else
        {
            throw UnsupportedQuery(method);
        }

        object query = Activator.CreateInstance(queryType, nonPublic: true)
            ?? throw new InvalidOperationException($"Could not create query parameter {queryType}.");
        var call = Expression.Call(method, Expression.Constant(query, queryType));
        Action runner = Expression.Lambda<Action>(call).Compile();
        string name = $"{method.DeclaringType?.FullName}.{method.Name}";
        return new ManagedSystem(name, accesses, runner);
    }

    private static InvalidOperationException UnsupportedQuery(MethodInfo method) => new(
        $"{method} has an unsupported parameter. Use WriteQuery<T>, WriteReadQuery<TWrite, TRead>, or Write3Query<T1, T2, T3>.");

    // =========================================================================
    // File Loading
    // =========================================================================

    /// <summary>Read a just-built DLL, retrying transient compiler file locks.</summary>
    private static byte[] ReadAllBytesWithRetry(string path)
    {
        const int maxAttempts = 10;
        const int delayMs = 50;
        for (var attempt = 1; ; attempt++)
        {
            try
            {
                return File.ReadAllBytes(path);
            }
            catch (IOException) when (attempt < maxAttempts)
            {
                Thread.Sleep(delayMs);
            }
        }
    }
}
