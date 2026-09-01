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

    /// <summary>Validated query metadata; null for Commands-only systems.</summary>
    public QueryDescriptor? Query { get; }

    /// <summary>Whether the system declared a Commands parameter.</summary>
    public bool UsesCommands { get; }

    /// <summary>Direct, reflection-free runner for the Rust scheduler.</summary>
    public Action Run { get; }

    /// <summary>Describe one generated system registration.</summary>
    public AotSystemRegistration(string name, QueryDescriptor? query, bool usesCommands, Action run)
    {
        Name = name;
        Query = query;
        UsesCommands = usesCommands;
        Run = run;
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
