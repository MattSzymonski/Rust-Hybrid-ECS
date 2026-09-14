// Collectible managed gameplay loader and ECS system discovery.
//
// Responsibilities:
// - Loads project assemblies without locking their build output.
// - Discovers and deterministically orders methods marked with EcsSystem.
// - Derives scheduler access from every query parameter a method declares.
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
/// <remarks>
/// `Accesses` is the union of every query's component terms: one entry per
/// component, positioned by first occurrence, with a write in any query
/// upgrading a shared declaration. The host consumes exactly this list for
/// scheduling, access checks, and reload verification, so query grouping
/// never crosses the native ABI. `Queries` preserves the grouping for
/// iteration and for the reload signature.
/// </remarks>
internal sealed record ManagedSystem(
    string Name, ManagedAccess[] Accesses, QueryDescriptor?[] Queries,
    bool UsesCommands, Action Run)
{
    internal string Signature
    {
        get
        {
            var builder = new System.Text.StringBuilder(Name);
            builder.Append(":commands=").Append(UsesCommands);
            // One separator per query keeps a grouping change visible even
            // when the merged access list happens to be identical.
            foreach (QueryDescriptor? query in Queries)
            {
                builder.Append('|');
                if (query is null)
                    continue;
                for (int index = 0; index < query.Terms.Count; index++)
                {
                    if (index > 0)
                        builder.Append(',');
                    QueryTermDescriptor term = query.Terms[index];
                    if (term.IsEntity)
                    {
                        builder.Append("entity");
                        continue;
                    }
                    builder.Append((byte)term.Access)
                        .Append(':')
                        .Append(term.ComponentKeyHigh.ToString("X16"))
                        .Append(term.ComponentKey.ToString("X16"));
                }
            }
            return builder.ToString();
        }
    }
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
/// <see cref="EcsSystemAttribute"/>. Every query parameter is both an
/// executable iterator and part of the authoritative scheduler access list;
/// the merged list is what the host registers.
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
            .Select(registration =>
            {
                QueryDescriptor?[] queries = registration.Queries ?? [];
                ValidateQueries(registration.Name, queries, registration.QueryNames);
                return new ManagedSystem(
                    registration.Name,
                    MergeAccesses(queries),
                    queries,
                    registration.UsesCommands,
                    registration.Run);
            })
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

    /// <summary>Upper bound on the total parameters one managed system may declare.</summary>
    /// <remarks>
    /// Matches the native `SystemParam` tuple arity, so a system ported
    /// between the two languages can keep the same signature.
    /// </remarks>
    internal const int MaxSystemParameters = 6;

    /// <summary>
    /// Validate one managed method, derive component access from every query
    /// parameter it declares, and compile a parameterless runner for the Rust
    /// scheduler.
    /// </summary>
    /// <remarks>
    /// A system may declare any number of query parameters plus at most one
    /// `Commands`, within the parameter budget. Each query keeps its own
    /// iteration, exactly like a native system that takes several query
    /// parameters.
    /// </remarks>
    internal static ManagedSystem CreateSystem(MethodInfo method)
    {
        if (method.ReturnType != typeof(void))
            throw new InvalidOperationException($"{method} must return void.");

        var parameters = method.GetParameters();
        if (parameters.Length == 0)
            throw new InvalidOperationException(
                $"{method} must declare at least one parameter: queries, Commands, or both.");
        if (parameters.Length > MaxSystemParameters)
            throw new InvalidOperationException(
                $"{method} declares {parameters.Length} parameters; managed systems support at " +
                $"most {MaxSystemParameters}, matching the native system parameter arity.");

        var queries = new List<QueryDescriptor?>(parameters.Length);
        var queryNames = new List<string?>(parameters.Length);
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
            if (!typeof(IQueryDescriptor).IsAssignableFrom(parameterType))
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
            queries.Add(((IQueryDescriptor)query).Descriptor);
            queryNames.Add(parameter.Name);
            arguments.Add(Expression.Constant(query, parameterType));
        }

        QueryDescriptor?[] queryArray = queries.ToArray();
        ValidateQueries(method.ToString() ?? method.Name, queryArray, queryNames);
        var call = Expression.Call(method, arguments);
        Action runner = Expression.Lambda<Action>(call).Compile();
        string name = $"{method.DeclaringType?.FullName}.{method.Name}";
        return new ManagedSystem(name, MergeAccesses(queryArray), queryArray, usesCommands, runner);
    }

    /// <summary>
    /// Reject a component that two query parameters both touch when either
    /// access is a write.
    /// </summary>
    /// <remarks>
    /// The native access check treats a write declaration as permitting
    /// reads, so without this validation both queries would be authorized to
    /// reach the same component - one through writable pointers, the other
    /// through read-only ones - which aliases storage the native scheduler
    /// cannot see. Sharing a component is allowed when every access is a
    /// read.
    /// </remarks>
    private static void ValidateQueries(
        string systemName, QueryDescriptor?[] queries, IReadOnlyList<string?>? queryNames)
    {
        var firstUse = new Dictionary<(ulong High, ulong Low), (QueryAccess Access, int Query)>();
        for (int queryIndex = 0; queryIndex < queries.Length; queryIndex++)
        {
            QueryDescriptor? query = queries[queryIndex];
            if (query is null)
                continue;
            foreach (QueryTermDescriptor term in query.Terms)
            {
                if (term.IsEntity)
                    continue;
                var key = (term.ComponentKeyHigh, term.ComponentKey);
                if (firstUse.TryGetValue(key, out var existing))
                {
                    if (existing.Access == QueryAccess.Write || term.Access == QueryAccess.Write)
                        throw new InvalidOperationException(
                            $"{systemName}: component {term.ComponentType!.FullName} is accessed by " +
                            $"{DescribeQuery(queryNames, existing.Query)} and " +
                            $"{DescribeQuery(queryNames, queryIndex)}; declare it in one query " +
                            "parameter or use read access only.");
                    continue;
                }
                firstUse[key] = (term.Access, queryIndex);
            }
        }
    }

    /// <summary>Name one query parameter for a validation message.</summary>
    private static string DescribeQuery(IReadOnlyList<string?>? names, int queryIndex) =>
        names is not null && queryIndex < names.Count && !string.IsNullOrWhiteSpace(names[queryIndex])
            ? $"query parameter '{names[queryIndex]}'"
            : $"query parameter #{queryIndex}";

    /// <summary>
    /// Merge every query's component terms into the flat scheduler access
    /// list: one entry per component, first-seen order, and a write in any
    /// query upgrades a shared declaration.
    /// </summary>
    internal static ManagedAccess[] MergeAccesses(QueryDescriptor?[] queries)
    {
        var merged = new List<ManagedAccess>();
        var indexByKey = new Dictionary<(ulong High, ulong Low), int>();
        foreach (QueryDescriptor? query in queries)
        {
            if (query is null)
                continue;
            foreach (QueryTermDescriptor term in query.Terms)
            {
                if (term.IsEntity)
                    continue;
                var key = (term.ComponentKeyHigh, term.ComponentKey);
                if (indexByKey.TryGetValue(key, out int existing))
                {
                    if (term.Access == QueryAccess.Write)
                        merged[existing] = merged[existing] with { Mode = 1 };
                    continue;
                }
                indexByKey[key] = merged.Count;
                merged.Add(new ManagedAccess(
                    term.ComponentKey, term.ComponentKeyHigh, (byte)term.Access));
            }
        }
        return merged.ToArray();
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
