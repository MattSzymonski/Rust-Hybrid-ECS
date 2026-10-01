// NativeAOT shipping contract: direct, compile-time system registrations.
//
// The reflection-based discovery in `ProjectHost` cannot run under NativeAOT:
// there is no dynamic code (`Expression.Compile`), and reflection invokers for
// `Activator.CreateInstance` / `MethodInfo.Invoke` are not generated. When a
// project is published with `PublishAot=true`, the
// `PillCSharpRuntimeMacros.EcsAotRegistryGenerator` source generator emits one
// static runner per `[EcsSystem]` / `[EcsStartup]` method into the project
// assembly and installs them here through a module initializer. `ProjectHost`'s
// AOT branch reads these arrays instead of reflecting over a loaded assembly.
// In ordinary JIT builds the arrays stay empty and this type is unused.

using System;
using System.Collections.Generic;

namespace TracyLive.Loader;

/// <summary>One compile-time registered system for the AOT shipping posture.</summary>
public readonly struct AotSystemRegistration
{
    /// <summary>Stable display name, matching the reflection path's format.</summary>
    public string Name { get; }

    /// <summary>Validated query metadata in parameter order; empty for Commands-only systems.</summary>
    public QueryDescriptor?[] Queries { get; }

    /// <summary>Declared query parameter names, aligned with <see cref="Queries"/>.</summary>
    public string[] QueryNames { get; }

    /// <summary>Whether the system declared a Commands parameter.</summary>
    public bool UsesCommands { get; }

    /// <summary>
    /// Resources the system declared, as stable identity plus access mode, in
    /// parameter order.
    /// </summary>
    public ResourceAccessRegistration[] ResourceAccesses { get; }

    /// <summary>Direct, reflection-free runner for the Rust scheduler.</summary>
    public Action Run { get; }

    /// <summary>Describe one generated system registration.</summary>
    public AotSystemRegistration(
        string name, QueryDescriptor?[] queries, string[] queryNames, bool usesCommands, Action run)
        : this(name, queries, queryNames, usesCommands, [], run)
    {
    }

    /// <summary>Describe one generated system registration, resources included.</summary>
    public AotSystemRegistration(
        string name, QueryDescriptor?[] queries, string[] queryNames, bool usesCommands,
        ResourceAccessRegistration[] resourceAccesses, Action run)
    {
        Name = name;
        Queries = queries;
        QueryNames = queryNames;
        UsesCommands = usesCommands;
        ResourceAccesses = resourceAccesses;
        Run = run;
    }
}

/// <summary>One resource a generated system declared, and how it reaches it.</summary>
public readonly struct ResourceAccessRegistration
{
    /// <summary>Low half of the resource's stable 128-bit identity.</summary>
    public ulong Low { get; }

    /// <summary>High half of the resource's stable 128-bit identity.</summary>
    public ulong High { get; }

    /// <summary>Access mode: <c>0</c> read, <c>1</c> read-write.</summary>
    public byte Mode { get; }

    /// <summary>Describe one declared resource access.</summary>
    public ResourceAccessRegistration(ulong low, ulong high, byte mode)
    {
        Low = low;
        High = high;
        Mode = mode;
    }

    /// <summary>
    /// Build the declaration for one resource type and access mode.
    /// </summary>
    /// <remarks>
    /// The generated registry calls this rather than writing a precomputed
    /// hash, so the identity has exactly one definition in the process. A
    /// duplicate would be a second place for the managed and native sides to
    /// disagree about what a resource is called, and it would disagree first
    /// for a nested struct - whose name Roslyn and reflection spell differently
    /// - and again for any resource that declares its own name, which a
    /// generator reading only the type would never see.
    ///
    /// Generic over an unmanaged <typeparamref name="T"/> named directly in
    /// generated source, so NativeAOT sees a closed instantiation at compile
    /// time - no <c>MakeGenericMethod</c>, which it cannot service over a value
    /// type.
    /// </remarks>
    public static ResourceAccessRegistration Of<T>(byte mode) where T : unmanaged
    {
        StableComponentId id = ResourceTypeMetadata<T>.StableId;
        return new ResourceAccessRegistration(id.Low, id.High, mode);
    }
}

/// <summary>One compile-time registered startup method for the AOT posture.</summary>
public readonly struct AotStartupRegistration
{
    /// <summary>Stable display name, matching the reflection path's format.</summary>
    public string Name { get; }

    /// <summary>Direct, reflection-free runner for the Rust scheduler.</summary>
    public Action Run { get; }

    /// <summary>Describe one generated startup registration.</summary>
    public AotStartupRegistration(string name, Action run)
    {
        Name = name;
        Run = run;
    }
}

/// <summary>
/// Registry filled by generated module-initializer code in the project
/// assembly during a NativeAOT publish. Empty in ordinary JIT builds.
/// </summary>
public static class AotRegistry
{
    private static AotSystemRegistration[] _systems = [];
    private static AotStartupRegistration[] _startups = [];
    private static System.Reflection.Assembly? _projectAssembly;

    /// <summary>Generated system registrations (AOT publish only).</summary>
    public static IReadOnlyList<AotSystemRegistration> Systems => _systems;

    /// <summary>Generated startup registrations (AOT publish only).</summary>
    public static IReadOnlyList<AotStartupRegistration> Startups => _startups;

    /// <summary>
    /// The project assembly (the AOT root) whose types back the component
    /// manifest. In the merged AOT image this is distinct from the runtime
    /// assembly, so the manifest never sees csharp_runtime's internal types.
    /// </summary>
    public static System.Reflection.Assembly? ProjectAssembly => _projectAssembly;

    /// <summary>Install the generated registrations, called once at module init.</summary>
    public static void Install(
        AotSystemRegistration[] systems,
        AotStartupRegistration[] startups,
        System.Reflection.Assembly? projectAssembly = null)
    {
        _systems = systems;
        _startups = startups;
        _projectAssembly = projectAssembly;
    }
}
