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
internal readonly record struct ManagedAccess(
    ulong ComponentKey, ulong ComponentKeyHigh, byte Mode, byte Kind)
{
    /// <summary>The key names a component column.</summary>
    internal const byte ComponentKind = 0;

    /// <summary>The key names a world resource.</summary>
    internal const byte ResourceKind = 1;
}

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

    /// <summary>
    /// A behaviour-compatible assembly is loaded and waiting, but its component
    /// manifest differs from the one in force.
    /// </summary>
    /// <remarks>
    /// The managed side cannot decide this one. Whether a manifest change can
    /// be applied depends on what each component is bound to natively - a
    /// descriptor column can be relaid out and its rows migrated, a mirror of a
    /// Rust type cannot - and only the host holds those bindings.
    ///
    /// So the swap stops here and waits. Applying the manifest *after* swapping
    /// would leave a running assembly against a world that never took its
    /// layout; parking first means a refusal costs an unload and nothing else.
    /// </remarks>
    ManifestPending = 3,
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
        public ProjectContext() : base(isCollectible: true)
        {
            // Dropping the resolved delegate cache here rather than only
            // before a successful swap covers every unload path, the
            // rejection one included, and keeps the runtime from being the
            // thing that holds a retiring context alive.
            Unloading += _ => Engine.ReloadMirrorMethods();
        }

        protected override Assembly? Load(AssemblyName assemblyName)
        {
            if (assemblyName.Name == "csharp_runtime")
                return typeof(ProjectHost).Assembly;
            return null;
        }
    }

    /// <summary>
    /// One loaded, validated project version waiting on the host's verdict
    /// about its component manifest.
    /// </summary>
    /// <remarks>
    /// Nothing about the running generation changes while a version sits here:
    /// its context is loaded but not installed, so an abort costs an unload and
    /// leaves the world exactly as it was.
    /// </remarks>
    private sealed record PendingReload(
        ProjectContext Context,
        ManagedSystem[] Systems,
        ManagedStartup[] Startups,
        byte[] Manifest,
        DateTime WriteUtc);

    /// <summary>The version awaiting a verdict, if any.</summary>
    private PendingReload? _pending;

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
    /// <summary>
    /// Every context this loader has retired, weakly referenced so the check
    /// itself never keeps one alive.
    /// </summary>
    private readonly List<WeakReference<ProjectContext>> _retiredContexts = new();
    /// <summary>Versions retired since startup, and versions confirmed dead.</summary>
    private int _retiredCount;
    private int _collectedCount;

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
                // The two postures must declare identical access, or a system
                // that works in development would be refused its resource in
                // the shipping build. Same list, same order, different source.
                ManagedAccess[] accesses = MergeAccesses(queries)
                    .Concat((registration.ResourceAccesses ?? [])
                        .Select(resource => new ManagedAccess(
                            resource.Low, resource.High, resource.Mode,
                            ManagedAccess.ResourceKind)))
                    .ToArray();
                return new ManagedSystem(
                    registration.Name,
                    accesses,
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
        _componentManifest = ProjectManifestBuilder.Build(
            systems, AotRegistry.ProjectAssembly);
        _lastSystemErrors = new string?[systems.Length];
        _lastAllocatedBytes = new long[systems.Length];
    }

    /// <summary>Invoke a discovered system by its stable index.</summary>
    public void RunSystem(int index) => _systems[index].Run();
    public void RunStartup(int index) => _startups[index].Run();

    /// <summary>Name one system for a diagnostic, without throwing on a bad index.</summary>
    public string DescribeSystem(int systemIndex) =>
        (uint)systemIndex < (uint)_systems.Length ? _systems[systemIndex].Name : $"system {systemIndex}";

    /// <summary>Bytes the last run of each system allocated on the managed heap.</summary>
    private long[] _lastAllocatedBytes = [];

    /// <summary>Record what one system's last run allocated.</summary>
    /// <remarks>
    /// Attribution, not prevention: the query path itself allocates nothing, so
    /// a non-zero figure here is always the script's own. Surfacing it per
    /// system is what turns a frame spike into a name.
    /// </remarks>
    public void RecordAllocation(int systemIndex, long bytes)
    {
        if ((uint)systemIndex < (uint)_lastAllocatedBytes.Length)
            _lastAllocatedBytes[systemIndex] = bytes;
    }

    /// <summary>Bytes the last run of one system allocated, or zero.</summary>
    public long GetAllocatedBytes(int systemIndex) =>
        (uint)systemIndex < (uint)_lastAllocatedBytes.Length ? _lastAllocatedBytes[systemIndex] : 0;

    /// <summary>When the allocation summary was last written.</summary>
    private DateTime _lastAllocationReportUtc = DateTime.UtcNow;

    /// <summary>Interval between allocation summaries.</summary>
    private static readonly TimeSpan AllocationReportInterval = TimeSpan.FromSeconds(10);

    /// <summary>
    /// Periodically name the systems that allocate on the managed heap.
    /// </summary>
    /// <remarks>
    /// The query path allocates nothing - rows and enumerators are ref structs
    /// and the generated wrappers do not box - so any figure here is the
    /// script's own. It matters more than the raw number suggests: the worker
    /// threads that run systems become attached managed threads, so a
    /// collection triggered by one system suspends the whole pool.
    ///
    /// Reported on an interval rather than per frame, because the point is to
    /// notice a steady allocator, not to narrate every frame.
    /// </remarks>
    private void ReportAllocationsIfDue()
    {
        DateTime now = DateTime.UtcNow;
        if (now - _lastAllocationReportUtc < AllocationReportInterval)
            return;
        _lastAllocationReportUtc = now;
        var offenders = new List<string>();
        for (int index = 0; index < _lastAllocatedBytes.Length; index++)
        {
            if (_lastAllocatedBytes[index] > 0)
                offenders.Add($"{_systems[index].Name} ({_lastAllocatedBytes[index]} B/frame)");
        }
        if (offenders.Count > 0)
            Console.WriteLine(
                "[csharp_runtime] managed systems allocating per frame: " +
                string.Join(", ", offenders));
    }

    /// <summary>Record a failure against the system with this reflected name.</summary>
    /// <remarks>
    /// The frame-scope context knows which system it was installed for by name
    /// rather than by index, because that is what makes its message readable.
    /// </remarks>
    public void SetSystemErrorByName(string systemName, string message)
    {
        for (int index = 0; index < _systems.Length; index++)
        {
            if (_systems[index].Name == systemName)
            {
                SetSystemError(index, message);
                return;
            }
        }
    }

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
        ReportAllocationsIfDue();
        // NativeAOT builds are static shipping artifacts: no project assembly
        // exists to watch, so a reload poll is always a no-op.
        if (!RuntimeFeature.IsDynamicCodeSupported)
            return (byte)PollStatus.NoChange;
        // A version already waiting on the host's verdict owns the slot;
        // polling again would load a second one on top of it. The host answers
        // within the same frame, so this is a guard rather than a state the
        // loader sits in.
        if (_pending is not null)
            return (byte)PollStatus.ManifestPending;
        var now = DateTime.UtcNow;
        if (now - _lastPollUtc < PollInterval)
            return (byte)PollStatus.NoChange;
        _lastPollUtc = now;

        // Before deciding anything: see whether the previous versions actually
        // unloaded. The API only requests an unload, so this is the only place
        // a leaked context becomes visible.
        SweepRetiredContexts();

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
            if (Load(isReload: true) == PollStatus.ManifestPending)
            {
                Console.WriteLine(
                    "[csharp_runtime] " +
                    $"{Path.GetFileName(_assemblyPath)} changed its component manifest; " +
                    "awaiting the host's verdict");
                return (byte)PollStatus.ManifestPending;
            }
            Console.WriteLine(
                $"[csharp_runtime] reloaded {Path.GetFileName(_assemblyPath)} " +
                $"(retired {_retiredCount}, collected {_collectedCount})");
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
    /// Count the retired assembly versions the runtime has actually released.
    /// </summary>
    /// <remarks>
    /// A collectible context unloads only when nothing roots it, and
    /// <see cref="AssemblyLoadContext.Unload"/> returns before that happens -
    /// it cannot await a collection. Every blocker Microsoft lists for the
    /// feature (a static holding a delegate, an event handler, a cached
    /// <see cref="Type"/>, a thread-pool callback, a thread-static) is
    /// something a project writes by accident, and each one leaves the whole
    /// previous assembly - and its statics - alive with no symptom but memory
    /// growth. Holding a weak reference and looking after a collection turns
    /// that into an observable fact. Survivors are reported on stderr, the
    /// same channel as the reload-failed line, and the counts ride on the
    /// reloaded line.
    /// </remarks>
    private void SweepRetiredContexts()
    {
        if (_retiredContexts.Count == 0)
            return;
        // A collection is what turns "unload requested" into an answer. One
        // pass is often enough, but a context whose assembly is referenced
        // until the finalizer pass can need another; the loop is the pattern
        // Microsoft's own sample uses, and it keeps a merely-slow collection
        // from being reported as a leak.
        for (int attempt = 0; attempt < 10 && AnyRetiredContextAlive(); attempt++)
        {
            GC.Collect();
            GC.WaitForPendingFinalizers();
        }
        int survivors = 0;
        for (int index = _retiredContexts.Count - 1; index >= 0; index--)
        {
            if (_retiredContexts[index].TryGetTarget(out _))
            {
                survivors++;
                continue;
            }
            _retiredContexts.RemoveAt(index);
            _collectedCount++;
        }
        if (survivors > 0)
        {
            Console.Error.WriteLine(
                $"[csharp_runtime] {survivors} retired version(s) of " +
                $"{Path.GetFileName(_assemblyPath)} are still loaded after " +
                $"{_collectedCount} of {_retiredCount} reloads; something in the " +
                "assembly roots its load context (a static event, a cached Type, " +
                "a thread-pool callback)");
        }
    }

    /// <summary>Whether any retired context the loader still tracks is alive.</summary>
    private bool AnyRetiredContextAlive()
    {
        foreach (WeakReference<ProjectContext> reference in _retiredContexts)
        {
            if (reference.TryGetTarget(out _))
                return true;
        }
        return false;
    }

    /// <summary>
    /// Queue one retiring context for the unload check, then request the unload.
    /// </summary>
    private void RetireContext(ProjectContext context)
    {
        _retiredContexts.Add(new WeakReference<ProjectContext>(context));
        _retiredCount++;
        context.Unload();
    }

    /// <summary>
    /// Load one assembly version, validate its scheduler signature, then swap
    /// it atomically with the active collectible context.
    /// </summary>
    private PollStatus Load(bool isReload)
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

            byte[] manifest = ProjectManifestBuilder.Build(systems, assembly);

            // Rust's execution graph and component registry are built at
            // startup. Behavior-only reloads are safe; changing either
            // contract needs a restart so native metadata cannot go stale.
            if (isReload && !_systems.Select(s => s.Signature).SequenceEqual(
                    systems.Select(s => s.Signature)))
                throw new InvalidOperationException(
                    "C# system names or query signatures changed; restart the host to rebuild the Rust scheduler.");
            if (isReload && !_startups.Select(s => s.Name).SequenceEqual(startups.Select(s => s.Name)))
                throw new InvalidOperationException(
                    "C# startup methods changed; restart the host. Startup methods are not rerun during hot reload.");

            // A changed manifest parks rather than refusing: whether it can be
            // applied depends on the native bindings, which only the host
            // holds. It answers with commit or abort.
            if (isReload && !_componentManifest.AsSpan().SequenceEqual(manifest))
            {
                _pending = new PendingReload(
                    context, systems, startups, manifest,
                    File.GetLastWriteTimeUtc(_assemblyPath));
                return PollStatus.ManifestPending;
            }

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
            _lastAllocatedBytes = new long[systems.Length];
            _lastWriteUtc = File.GetLastWriteTimeUtc(_assemblyPath);
            if (oldContext is not null)
                RetireContext(oldContext);
            return PollStatus.Reloaded;
        }
        catch
        {
            RetireContext(context);
            throw;
        }
    }

    /// <summary>UTF-8 manifest of the version awaiting a verdict.</summary>
    public ReadOnlySpan<byte> PendingComponentManifest =>
        _pending is null ? ReadOnlySpan<byte>.Empty : _pending.Manifest;

    /// <summary>Install the parked version; the host accepted its manifest.</summary>
    /// <remarks>
    /// The host has already relaid out the columns the new manifest asks for,
    /// so the world and this assembly agree the moment the swap lands.
    /// </remarks>
    public void CommitPendingReload()
    {
        if (_pending is null)
            return;
        PendingReload pending = _pending;
        _pending = null;

        ProjectContext? oldContext = _context;
        // Same ordering as the ordinary swap: drop the old context's resolved
        // delegates before it retires, so nothing holds it back.
        Engine.ReloadMirrorMethods();
        _context = pending.Context;
        _systems = pending.Systems;
        _startups = pending.Startups;
        _componentManifest = pending.Manifest;
        _lastSystemErrors = new string?[pending.Systems.Length];
        _lastAllocatedBytes = new long[pending.Systems.Length];
        _lastWriteUtc = pending.WriteUtc;
        if (oldContext is not null)
            RetireContext(oldContext);

        // A commit is a completed reload, so it reports like one. The two paths
        // reaching this point - an ordinary swap and a manifest the host
        // applied first - are one event to anything watching the log, and the
        // suites wait on this line.
        Console.WriteLine(
            $"[csharp_runtime] reloaded {Path.GetFileName(_assemblyPath)} " +
            $"(retired {_retiredCount}, collected {_collectedCount})");
    }

    /// <summary>Discard the parked version; the host refused its manifest.</summary>
    /// <remarks>
    /// The running generation never moved, so this unloads what was loaded
    /// speculatively and nothing else. The write time is remembered either way,
    /// so the same refused bytes are not re-examined twice a second for as long
    /// as the source stays that way.
    /// </remarks>
    public void AbortPendingReload()
    {
        if (_pending is null)
            return;
        PendingReload pending = _pending;
        _pending = null;
        _lastWriteUtc = pending.WriteUtc;
        RetireContext(pending.Context);
    }

    // =========================================================================
    // System Discovery and Compilation
    // =========================================================================

    /// <summary>Discover attributed static methods in deterministic order.</summary>
    internal static ManagedSystem[] DiscoverSystems(Assembly assembly)
    {
        ReportAttributedInstanceMethods(assembly);
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
    /// Name attributed instance methods, which discovery cannot register.
    /// </summary>
    /// <remarks>
    /// Discovery looks for static methods only, so an <c>[EcsSystem]</c> on an
    /// instance method is not merely rejected - it is invisible. The system
    /// never runs and nothing says why, which is the one declaration mistake
    /// here that produces no symptom at all. One extra reflection pass over an
    /// assembly that is already being reflected over buys a name.
    /// </remarks>
    private static void ReportAttributedInstanceMethods(Assembly assembly)
    {
        foreach (Type type in assembly.GetTypes())
        {
            foreach (MethodInfo method in type.GetMethods(
                         BindingFlags.Public | BindingFlags.NonPublic | BindingFlags.Instance))
            {
                bool attributed =
                    method.GetCustomAttribute<EcsSystemAttribute>() is not null ||
                    method.GetCustomAttribute<EcsStartupAttribute>() is not null;
                if (attributed)
                    Console.Error.WriteLine(
                        $"[csharp_runtime] {type.FullName}.{method.Name} carries an ECS attribute " +
                        "but is an instance method, so it was not registered and will never run. " +
                        "Make it static.");
            }
        }
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
        // Keyed by resource identity rather than by whole access, so a second
        // declaration is caught whatever its mode. Two entries for one resource
        // would tell the scheduler nothing new, and a Res<T> beside a ResMut<T>
        // would hand the same bytes out as readable and writable at once - the
        // aliasing ValidateQueries refuses between two query parameters.
        var resourceAccesses = new Dictionary<(ulong Low, ulong High), ManagedAccess>();
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
            if (TryDescribeResourceParameter(parameterType, out ManagedAccess resourceAccess))
            {
                var key = (resourceAccess.ComponentKey, resourceAccess.ComponentKeyHigh);
                if (!resourceAccesses.TryAdd(key, resourceAccess))
                    throw new InvalidOperationException(
                        $"{method} declares resource {parameterType.GetGenericArguments()[0].FullName} " +
                        "more than once; one Res<T> or ResMut<T> parameter per resource.");
                arguments.Add(Expression.Default(parameterType));
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
        // Component and resource accesses go to the scheduler in one list,
        // distinguished by kind: the native side derives one SystemAccess from
        // it, and a system's whole declaration has to arrive together for the
        // scheduler to order it against its peers.
        ManagedAccess[] accesses = MergeAccesses(queryArray)
            .Concat(resourceAccesses.Values
                .OrderBy(access => access.ComponentKeyHigh)
                .ThenBy(access => access.ComponentKey))
            .ToArray();
        return new ManagedSystem(name, accesses, queryArray, usesCommands, runner);
    }

    /// <summary>
    /// Describe a <c>Res&lt;T&gt;</c> or <c>ResMut&lt;T&gt;</c> parameter as one
    /// reflected access, or report that the parameter is something else.
    /// </summary>
    /// <remarks>
    /// The declaration is read off the closed generic type rather than an
    /// instance: <see cref="IResourceParameter"/>'s members are static
    /// abstract, so there is nothing to construct, and reflecting over the
    /// generic definition keeps this AOT-safe - no MakeGenericMethod is
    /// involved, which NativeAOT could not service over a value type anyway.
    /// </remarks>
    private static bool TryDescribeResourceParameter(Type parameterType, out ManagedAccess access)
    {
        access = default;
        if (!parameterType.IsGenericType ||
            !typeof(IResourceParameter).IsAssignableFrom(parameterType))
            return false;
        Type definition = parameterType.GetGenericTypeDefinition();
        QueryAccess mode;
        if (definition == typeof(Res<>))
            mode = QueryAccess.Read;
        else if (definition == typeof(ResMut<>))
            mode = QueryAccess.Write;
        else
            return false;
        Type resource = parameterType.GetGenericArguments()[0];
        // The resource's declared identity, not its C# type name: a resource
        // that meets a Rust module halfway is registered under the name both
        // sides write down, and the access has to name the same thing.
        StableComponentId id = Engine.StableIdOf(ResourceNames.Of(resource));
        access = new ManagedAccess(id.Low, id.High, (byte)mode, ManagedAccess.ResourceKind);
        return true;
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
                    term.ComponentKey, term.ComponentKeyHigh, (byte)term.Access,
                    ManagedAccess.ComponentKind));
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
