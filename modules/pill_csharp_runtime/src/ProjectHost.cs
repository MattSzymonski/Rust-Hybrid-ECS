// Collectible managed gameplay loader and ECS system discovery.
//
// Responsibilities:
// - Loads project assemblies without locking their build output.
// - Discovers and deterministically orders methods marked with EcsSystem.
// - Derives scheduler access from each method's single query parameter.
// - Reloads behavior while rejecting scheduler-signature changes.
//
// Design:
// - Every project version lives in a collectible AssemblyLoadContext and is read
//   from bytes so the compiler can replace the source DLL on Windows.
// - Rust builds its execution graph once. A reload may replace method bodies,
//   but names and access signatures must remain stable until host restart.

using System.Linq.Expressions;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.Loader;
using System.Threading;

namespace TracyLive.Loader;

// =============================================================================
// Discovered System Metadata
// =============================================================================

/// <summary>One 128-bit component ID and its native access mode.</summary>
internal readonly record struct ManagedAccess(ulong ComponentKey, ulong ComponentKeyHigh, byte Mode);

/// <summary>Compiled managed system plus its scheduler declaration.</summary>
internal sealed record ManagedSystem(
    string Name, ManagedAccess[] Accesses, QueryDescriptor? QueryDescriptor,
    bool UsesCommands, Action Run)
{
    internal string Signature =>
        $"{Name}:commands={UsesCommands}:{string.Join(',', Accesses.Select(a => $"{a.Mode}:{a.ComponentKeyHigh:X16}{a.ComponentKey:X16}"))}";
}

/// <summary>Compiled one-shot startup method.</summary>
internal sealed record ManagedStartup(string Name, Action Run);

/// <summary>
/// Outcome of one reload poll, reported back to the Rust host.
/// </summary>
internal enum PollStatus : byte
{
    /// <summary>No reload was due or the assembly has not changed.</summary>
    NoChange = 0,

    /// <summary>A behavior-compatible assembly was swapped in.</summary>
    Reloaded = 1,

    /// <summary>The new assembly was rejected; the old one stays loaded.</summary>
    Rejected = 2,
}

// =============================================================================
// ProjectHost
// =============================================================================

/// <summary>
/// Loads the collectible gameplay assembly and discovers methods marked with
/// <see cref="EcsSystemAttribute"/>. Each method's single query parameter is
/// both its executable iterator and the authoritative scheduler access list.
/// </summary>
internal sealed class ProjectHost
{
    /// <summary>
    /// Collectible context for one project version. Requests for csharp_runtime types
    /// resolve to the already-loaded stable runtime assembly.
    /// </summary>
    private sealed class ProjectContext : AssemblyLoadContext
    {
        public ProjectContext() : base(isCollectible: true) { }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            if (assemblyName.Name == "csharp_runtime")
                return typeof(ProjectHost).Assembly;
            return null;
        }
    }

    private static readonly TimeSpan PollInterval = TimeSpan.FromMilliseconds(500);
    private readonly string _assemblyPath;
    private ProjectContext? _context;
    private ManagedSystem[] _systems = [];
    private ManagedStartup[] _startups = [];
    private byte[] _componentManifest = [];
    private string?[] _lastSystemErrors = [];
    /// <summary>
    /// Write time of the assembly this loader last EXAMINED, accepted or not.
    /// Not "the write time of the loaded assembly": after a rejection the loaded
    /// assembly is the previous one, and the point of the field is to stop the
    /// poll re-examining bytes it has already judged.
    /// </summary>
    private DateTime _lastWriteUtc;
    private DateTime _lastPollUtc;

    /// <summary>Create a loader for the configured gameplay assembly.</summary>
    public ProjectHost(string assemblyPath) => _assemblyPath = assemblyPath;

    /// <summary>Number of systems exposed by the active project version.</summary>
    public int SystemCount => _systems.Length;
    public int StartupCount => _startups.Length;
    public ReadOnlySpan<byte> ComponentManifest => _componentManifest;

    /// <summary>Return one reflected scheduler access.</summary>
    public ManagedAccess GetAccess(int systemIndex, int accessIndex) =>
        _systems[systemIndex].Accesses[accessIndex];

    /// <summary>Return the reflected name of one system.</summary>
    public string GetSystemName(int systemIndex) => _systems[systemIndex].Name;

    /// <summary>Return the UTF-8 byte count of one system's name.</summary>
    public int GetSystemNameLength(int systemIndex) =>
        System.Text.Encoding.UTF8.GetByteCount(_systems[systemIndex].Name);

    /// <summary>Return the number of accesses declared by one system.</summary>
    public int GetAccessCount(int systemIndex) => _systems[systemIndex].Accesses.Length;
    public bool UsesCommands(int systemIndex) => _systems[systemIndex].UsesCommands;

    /// <summary>Load the initial project version (reflection or AOT registry).</summary>
    public void Init()
    {
        // NativeAOT cannot load a project assembly or reflect over it at
        // runtime, so the generated registry (installed at module init) is the
        // only source of systems. The JIT path keeps loading + reflecting.
        if (!RuntimeFeature.IsDynamicCodeSupported)
        {
            LoadFromAotRegistry();
            return;
        }
        Load(isReload: false);
    }

    /// <summary>
    /// Load systems from the compile-time generated registry (NativeAOT).
    /// </summary>
    private void LoadFromAotRegistry()
    {
        ManagedSystem[] systems = AotRegistry.Systems
            .Select(registration => new ManagedSystem(
                registration.Name,
                (registration.Query?.Terms ?? [])
                    .Where(term => !term.IsEntity)
                    .Select(term => new ManagedAccess(
                        term.ComponentKey, term.ComponentKeyHigh, (byte)term.Access))
                    .ToArray(),
                registration.Query,
                registration.UsesCommands,
                registration.Run))
            .ToArray();
        ManagedStartup[] startups = AotRegistry.Startups
            .Select(registration => new ManagedStartup(registration.Name, registration.Run))
            .ToArray();
        if (systems.Length == 0)
            throw new InvalidOperationException(
                "No [EcsSystem] methods were generated for the AOT build.");
        _systems = systems;
        _startups = startups;
        _componentManifest = ComponentManifestBuilder.Build(
            systems, AotRegistry.ProjectAssembly);
        _lastSystemErrors = new string?[systems.Length];
    }

    /// <summary>Invoke a discovered system by its stable index.</summary>
    public void RunSystem(int index) => _systems[index].Run();
    public void RunStartup(int index) => _startups[index].Run();

    /// <summary>Return the last failure message recorded for one system, if any.</summary>
    public string GetSystemError(int systemIndex) =>
        (uint)systemIndex < (uint)_lastSystemErrors.Length
            ? _lastSystemErrors[systemIndex] ?? ""
            : "";

    /// <summary>Record the failure message reported by one managed system.</summary>
    /// <remarks>Out-of-range indices are ignored so a hostile or stale system
    /// index can never corrupt the error store.</remarks>
    public void SetSystemError(int systemIndex, string message)
    {
        if ((uint)systemIndex < (uint)_lastSystemErrors.Length)
            _lastSystemErrors[systemIndex] = message;
    }

    /// <summary>
    /// Poll the project DLL timestamp and reload a newer build.
    /// Returns the swap outcome so the Rust host can distinguish a clean
    /// behavior reload from a rejection that requires a host restart.
    /// </summary>
    public byte PollReload()
    {
        // NativeAOT builds are static shipping artifacts: no project assembly
        // exists to watch, so a reload poll is always a no-op.
        if (!RuntimeFeature.IsDynamicCodeSupported)
            return (byte)PollStatus.NoChange;
        var now = DateTime.UtcNow;
        if (now - _lastPollUtc < PollInterval)
            return (byte)PollStatus.NoChange;
        _lastPollUtc = now;

        DateTime written;
        try
        {
            written = File.GetLastWriteTimeUtc(_assemblyPath);
        }
        catch (IOException)
        {
            return (byte)PollStatus.NoChange;
        }

        if (written <= _lastWriteUtc)
            return (byte)PollStatus.NoChange;

        try
        {
            Load(isReload: true);
            Console.WriteLine($"[csharp_runtime] reloaded {Path.GetFileName(_assemblyPath)}");
            return (byte)PollStatus.Reloaded;
        }
        catch (Exception e)
        {
            Console.Error.WriteLine($"[csharp_runtime] reload failed: {e}");
            // Remember the rejected build too. Without this the same bytes are
            // re-examined every poll interval for as long as the source stays
            // broken - reading the whole assembly, creating a collectible
            // context, reflecting over it and unloading it again, twice a
            // second, forever. The Rust host logs the rejection only once, so
            // the churn is invisible. A genuinely new build has a later write
            // time and is examined normally.
            _lastWriteUtc = written;
            return (byte)PollStatus.Rejected;
        }
    }

    /// <summary>
    /// Load one assembly version, validate its scheduler signature, then swap
    /// it atomically with the active collectible context.
    /// </summary>
    private void Load(bool isReload)
    {
        var bytes = ReadAllBytesWithRetry(_assemblyPath);
        var context = new ProjectContext();
        try
        {
            Assembly assembly;
            using (var stream = new MemoryStream(bytes))
                assembly = context.LoadFromStream(stream);

            var systems = DiscoverSystems(assembly);
            var startups = DiscoverStartups(assembly);
            if (systems.Length == 0)
                throw new InvalidOperationException(
                    "No [EcsSystem] methods with a supported query parameter were found.");

            byte[] manifest = ComponentManifestBuilder.Build(systems, assembly);

            // Rust's execution graph and component registry are built at
            // startup. Behavior-only reloads are safe; changing either
            // contract needs a restart so native metadata cannot go stale.
            if (isReload && !_systems.Select(s => s.Signature).SequenceEqual(
                    systems.Select(s => s.Signature)))
                throw new InvalidOperationException(
                    "C# system names or query signatures changed; restart the host to rebuild the Rust scheduler.");
            if (isReload && !_componentManifest.AsSpan().SequenceEqual(manifest))
                throw new InvalidOperationException(
                    "C# component identities or layouts changed; restart the host to rebuild the native component registry.");
            if (isReload && !_startups.Select(s => s.Name).SequenceEqual(startups.Select(s => s.Name)))
                throw new InvalidOperationException(
                    "C# startup methods changed; restart the host. Startup methods are not rerun during hot reload.");

            var oldContext = _context;
            // The new assembly carries its own mirror delegate types (each
            // collectible context defines them), and an optional module reload
            // may have moved every mirrored method to fresh trampoline
            // addresses, so re-read the host's table and drop the old
            // context's resolved delegates. Done before the swap so the old
            // context holds nothing back and can actually unload.
            Engine.ReloadMirrorMethods();
            _context = context;
            _systems = systems;
            _startups = startups;
            _componentManifest = manifest;
            _lastSystemErrors = new string?[systems.Length];
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
        if (parameters.Length == 0 || parameters.Length > 2)
            throw new InvalidOperationException(
                $"{method} must have a query, Commands, or one of each.");

        QueryDescriptor? descriptor = null;
        bool usesCommands = false;
        var arguments = new List<Expression>(parameters.Length);
        foreach (ParameterInfo parameter in parameters)
        {
            Type parameterType = parameter.ParameterType;
            if (parameterType == typeof(Commands))
            {
                if (usesCommands)
                    throw new InvalidOperationException($"{method} declares Commands more than once.");
                usesCommands = true;
                arguments.Add(Expression.Default(typeof(Commands)));
                continue;
            }
            if (!typeof(IQueryDescriptor).IsAssignableFrom(parameterType) || descriptor is not null)
                throw UnsupportedQuery(method);
            object query;
            try
            {
                query = Activator.CreateInstance(parameterType, nonPublic: true)
                    ?? throw new InvalidOperationException($"Could not create query parameter {parameterType}.");
            }
            catch (TargetInvocationException exception) when (exception.InnerException is not null)
            {
                throw new InvalidOperationException(
                    $"Invalid query parameter on {method}: {exception.InnerException.Message}",
                    exception.InnerException);
            }
            descriptor = ((IQueryDescriptor)query).Descriptor;
            arguments.Add(Expression.Constant(query, parameterType));
        }

        ManagedAccess[] accesses = (descriptor?.Terms ?? [])
            .Where(term => !term.IsEntity)
            .Select(term => new ManagedAccess(
                term.ComponentKey, term.ComponentKeyHigh, (byte)term.Access))
            .ToArray();
        var call = Expression.Call(method, arguments);
        Action runner = Expression.Lambda<Action>(call).Compile();
        string name = $"{method.DeclaringType?.FullName}.{method.Name}";
        return new ManagedSystem(name, accesses, descriptor, usesCommands, runner);
    }

    /// <summary>Discover and compile deterministic one-shot startup methods.</summary>
    internal static ManagedStartup[] DiscoverStartups(Assembly assembly) => assembly.GetTypes()
        .SelectMany(type => type.GetMethods(
            BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Static))
        .Where(method => method.GetCustomAttribute<EcsStartupAttribute>() is not null)
        .OrderBy(method => method.DeclaringType?.FullName, StringComparer.Ordinal)
        .ThenBy(method => method.Name, StringComparer.Ordinal)
        .Select(CreateStartup)
        .ToArray();

    internal static ManagedStartup CreateStartup(MethodInfo method)
    {
        if (method.ReturnType != typeof(void) ||
            method.GetParameters() is not [var parameter] ||
            parameter.ParameterType != typeof(Commands))
            throw new InvalidOperationException(
                $"{method} must be static void and have exactly one Commands parameter.");
        Action runner = Expression.Lambda<Action>(
            Expression.Call(method, Expression.Default(typeof(Commands)))).Compile();
        return new ManagedStartup($"{method.DeclaringType?.FullName}.{method.Name}", runner);
    }

    private static InvalidOperationException UnsupportedQuery(MethodInfo method) => new(
        $"{method} has an unsupported parameter. Use Query<...> composed from " +
        "Read<T>, Write<T>, OptionalRead<T>, OptionalWrite<T>, and EntityTerm.");

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
