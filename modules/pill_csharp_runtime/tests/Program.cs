// Executable regression tests for the C# ECS runtime and current C# project.
//
// Responsibilities:
// - Verifies scheduler access derived from every supported query shape.
// - Validates the actual bouncing-ball system discovered from project_cs.dll.
// - Guards native/managed component sizes and field offsets.
// - Checks invalid managed system signatures are rejected before registration.

using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using TracyLive;
using TracyLive.Loader;

namespace TracyLive.Tests;

// =============================================================================
// Test-only component and system declarations
// =============================================================================

[StructLayout(LayoutKind.Sequential)]
internal struct TestPosition { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestVelocity { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestHealth { public float Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestComponent4 { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestComponent5 { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestComponent6 { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestComponent7 { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct TestComponent8 { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct CommandOnlyComponent { public uint Value; }

[StructLayout(LayoutKind.Sequential)]
internal struct InvalidBoolComponent { public bool Value; }

internal static class TestSystems
{
    internal static bool WasRun;

    public static void Runner(Query<Write<TestPosition>, Read<TestVelocity>> query) =>
        WasRun = query is not null;

    public static void SingleWriter(Query<Write<TestPosition>> query) { }

    public static void MixedAccess(Query<Write<TestPosition>, Read<TestVelocity>> query) { }

    public static void TripleWriter(
        Query<Write<TestPosition>, Write<TestVelocity>, Write<TestHealth>> query)
    { }

    public static void OptionalAndEntity(
        Query<EntityTerm, Read<TestPosition>, OptionalWrite<TestHealth>> query)
    { }

    public static void EightTerms(
        Query<
            Read<TestPosition>, Write<TestVelocity>, OptionalRead<TestHealth>,
            Write<TestComponent4>, Read<TestComponent5>, Write<TestComponent6>,
            Read<TestComponent7>, OptionalWrite<TestComponent8>> query)
    { }

    public static void DuplicateReadWrite(
        Query<Write<TestPosition>, Read<TestPosition>> query)
    { }

    public static void DuplicateTriple(
        Query<Write<TestPosition>, Write<TestPosition>, Write<TestHealth>> query)
    { }

    public static void DuplicateEntity(Query<EntityTerm, EntityTerm> query) { }

    public static void NoParameters() { }

    public static int NonVoid(Query<Write<TestPosition>> query) => 0;

    public static void Unsupported(string value) { }

    public static void InvalidLayout(Query<Read<InvalidBoolComponent>> query) { }

    public static void CommandsOnly(Commands commands) { }

    public static void QueryAndCommands(
        Query<Read<TestPosition>> query, Commands commands)
    { }

    public static void DespawnSystem(
        Query<EntityTerm, Read<TestVelocity>> query, Commands commands)
    {
        foreach (var row in query)
        {
            commands.DestroyEntity(row.Entity);
            break;
        }
    }
}

internal static unsafe class MockNativeWorld
{
    internal static TestPosition* Positions;
    internal static TestVelocity* Velocities;
    internal static TestHealth* Healths;
    internal static Entity* Entities;
    internal static NativeComponentTicks* PositionTicks;
    internal static NativeComponentTicks* VelocityTicks;
    internal static NativeComponentTicks* HealthTicks;
    internal static uint Length;
    internal static uint ChangeTick;
    internal static ulong NextEntityId;
    internal static int QueuedCreates;
    internal static int LastCreateComponentCount;
    internal static int QueuedDestroys;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static uint EntityCount() => Length;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetComponentChunk(
        ulong key, ulong keyHigh, byte mode, uint index, NativeComponentChunk* output)
    {
        if (index != 0 || !TryResolveChunk(key, keyHigh, mode, out var chunk))
            return 0;
        *output = chunk;
        return 1;
    }

    /// <summary>Mirror of the host's archetype-scoped chunk entry point.</summary>
    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetArchetypeChunk(
        ulong archetypeLow, ulong archetypeHigh, ulong key, ulong keyHigh, byte mode,
        NativeComponentChunk* output)
    {
        if (archetypeLow != 7 || archetypeHigh != 11)
            return 0;
        if (mode == 2)
        {
            *output = Chunk(Entities, null, sizeof(Entity));
            return 1;
        }
        if (!TryResolveChunk(key, keyHigh, mode, out var chunk))
            return 0;
        *output = chunk;
        return 1;
    }

    /// <summary>Shared component-key matching for both chunk entry points.</summary>
    private static bool TryResolveChunk(
        ulong key, ulong keyHigh, byte mode, out NativeComponentChunk chunk)
    {
        chunk = default;
        if (key == Engine.ComponentKey(typeof(TestPosition)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestPosition)) &&
            (mode == 0 || mode == 1))
        {
            chunk = Chunk(Positions, PositionTicks, sizeof(TestPosition));
            return true;
        }
        if (key == Engine.ComponentKey(typeof(TestVelocity)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestVelocity)) && mode == 0)
        {
            chunk = Chunk(Velocities, VelocityTicks, sizeof(TestVelocity));
            return true;
        }
        if (key == Engine.ComponentKey(typeof(TestHealth)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestHealth)) &&
            (mode == 0 || mode == 1) && Healths != null)
        {
            chunk = Chunk(Healths, HealthTicks, sizeof(TestHealth));
            return true;
        }
        return false;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetEntityChunk(uint index, NativeComponentChunk* output)
    {
        if (index != 0)
            return 0;
        *output = Chunk(Entities, null, sizeof(Entity));
        return 1;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte ReserveEntity(Entity* output)
    {
        *output = new Entity(++NextEntityId, 0);
        return 1;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte QueueCreate(Entity* entity, NativeComponentBlob* blobs, uint count)
    {
        if (entity is null || (count != 0 && blobs is null))
            return 6;
        QueuedCreates++;
        LastCreateComponentCount = checked((int)count);
        return 1;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte QueueDestroy(Entity* entity)
    {
        if (entity->Generation == 99)
            return 5;
        QueuedDestroys++;
        return 1;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte QueueAddComponent(
        Entity* entity, ulong low, ulong high, byte* data, uint size) => 1;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte QueueRemoveComponent(Entity* entity, ulong low, ulong high) => 1;

    internal static EngineApi Api() => new()
    {
        EntityCount = &EntityCount,
        GetComponentChunk = &GetComponentChunk,
        GetArchetypeChunk = &GetArchetypeChunk,
        GetEntityChunk = &GetEntityChunk,
        ReserveEntity = &ReserveEntity,
        QueueCreate = &QueueCreate,
        QueueDestroy = &QueueDestroy,
        QueueAddComponent = &QueueAddComponent,
        QueueRemoveComponent = &QueueRemoveComponent,
        MirrorMethodCount = &MirrorMethodCount,
        CopyMirrorMethods = &CopyMirrorMethods,
    };

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static uint MirrorMethodCount() => 0;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static uint CopyMirrorMethods(MirrorMethodEntry* entries, uint max) => 0;

    internal static void ResetCommands()
    {
        NextEntityId = 0;
        QueuedCreates = 0;
        LastCreateComponentCount = 0;
        QueuedDestroys = 0;
    }

    // Trampolines shaped like the heap-field accessor ABI, for the
    // MirrorMethods raw-call tests. Each echoes fixed values, so a signature
    // mismatch between helper and trampoline shows up as wrong numbers rather
    // than being masked.

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorView(IntPtr row, IntPtr* data, IntPtr* length)
    {
        *data = row + 32;
        *length = 5;
        return 0;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorViewSecond(IntPtr row, IntPtr* data, IntPtr* length)
    {
        *data = row + 16;
        *length = 9;
        return 0;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorResize(IntPtr row, IntPtr count) => count == 7 ? (byte)0 : (byte)1;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorItem(IntPtr row, IntPtr index, IntPtr* data, IntPtr* length)
    {
        *data = row + 8;
        *length = index + 1;
        return 2;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorSetItem(
        IntPtr row, IntPtr index, IntPtr utf8, IntPtr length) =>
        length == 3 && index == (IntPtr)4 ? (byte)0 : (byte)1;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorUtf8Write(IntPtr row, IntPtr utf8, IntPtr length) =>
        utf8 != IntPtr.Zero && length == 3 ? (byte)0 : (byte)1;

    /// <summary>Element column the accessor benchmark's view trampoline reports.</summary>
    internal static float* AccessorElements;
    internal static int AccessorElementCount;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte AccessorElementsView(IntPtr row, IntPtr* data, IntPtr* length)
    {
        *data = (IntPtr)AccessorElements;
        *length = AccessorElementCount;
        return 0;
    }

    private static NativeComponentChunk Chunk(
        void* data, NativeComponentTicks* ticks, int elementSize) => new()
        {
            ArchetypeLow = 7,
            ArchetypeHigh = 11,
            Data = (IntPtr)data,
            Length = Length,
            ElementSize = checked((uint)elementSize),
            Ticks = (IntPtr)ticks,
            ChangeTick = ChangeTick,
        };
}

// =============================================================================
// Test runner
// =============================================================================

internal static class Program
{
    private static int _passed;

    private static MethodInfo Method(string name) =>
        typeof(TestSystems).GetMethod(name)
        ?? throw new InvalidOperationException($"Missing test method {name}");

    private static void Assert(bool condition, string message)
    {
        if (!condition)
            throw new InvalidOperationException(message);
    }

    private static void Equal<T>(T actual, T expected, string message)
        where T : IEquatable<T>
    {
        if (!actual.Equals(expected))
            throw new InvalidOperationException(
                $"{message}: expected {expected}, got {actual}");
    }

    private static void Throws<T>(Action action, string message) where T : Exception
    {
        try
        {
            action();
        }
        catch (T)
        {
            return;
        }

        throw new InvalidOperationException(message);
    }

    private static void Test(string name, Action test)
    {
        test();
        _passed++;
        Console.WriteLine($"pass: {name}");
    }

    public static unsafe int Main(string[] args)
    {
        // Benchmark mode times the query path instead of asserting on it, so
        // the regression suite stays deterministic. Run it explicitly with
        // `dotnet run -c Release --project ... -- bench-query <iterations>`.
        if (args.Length > 0 && args[0] == "bench-query")
        {
            int iterations = args.Length > 1 ? int.Parse(args[1]) : 10_000;
            RunQueryBenchmark([10_000u, 100_000u]);
            RunAccessorBenchmark(iterations);
            return 0;
        }

        try
        {
            Test("current project discovers native and dynamic component systems", () =>
            {
                var systems = ProjectHost.DiscoverSystems(typeof(BallPhysicsSystem).Assembly);
                // BallPhysicsSystem + BallTagSystem + the module-spline bridge demo.
                Equal(systems.Length, 3, "unexpected project system count");
                Assert(systems.Any(system => system.Name == "TracyLive.BallPhysicsSystem.Run"),
                    "ball physics system was not discovered");
                Assert(systems.Any(system => system.Name == "TracyLive.BallTagSystem.Observe"),
                    "dynamic component system was not discovered");
                Assert(systems.Any(system => system.Name == "TracyLive.ModuleSplineBridgeDemo.Run"),
                    "module-spline bridge demo system was not discovered");
            });

            Test("component manifest separates runtime mirrors from project components", () =>
            {
                var systems = ProjectHost.DiscoverSystems(typeof(BallPhysicsSystem).Assembly);
                using var json = System.Text.Json.JsonDocument.Parse(
                    ComponentManifestBuilder.Build(systems));
                var components = json.RootElement.EnumerateArray().ToArray();
                // Position, Sprite, PhysicsState, BallTag + the module Spline mirror.
                Equal(components.Length, 5, "unexpected manifest component count");
                var position = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.Position");
                var sprite = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.Sprite");
                var physics = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.PhysicsState");
                var tag = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.BallTag");
                var spline = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "pill_spline.Spline");
                Assert(position.GetProperty("shared").GetBoolean(),
                    "runtime Position mirror must be shared");
                Assert(sprite.GetProperty("shared").GetBoolean(),
                    "runtime Sprite mirror must be shared");
                Assert(!physics.GetProperty("shared").GetBoolean(),
                    "project-owned PhysicsState must be dynamic");
                Assert(!tag.GetProperty("shared").GetBoolean(),
                    "project-owned BallTag must be dynamic");
                Equal(tag.GetProperty("size").GetInt32(), 4, "BallTag size mismatch");
                Equal(tag.GetProperty("alignment").GetInt32(), 4,
                    "BallTag alignment mismatch");
                Equal(tag.GetProperty("fields").GetArrayLength(), 1,
                    "BallTag field schema mismatch");
                // The module mirror is declared in the project (not shared on
                // the managed side); the Rust host resolves it to the module's
                // native binding by stable identity instead.
                Assert(!spline.GetProperty("shared").GetBoolean(),
                    "module Spline mirror must be project-owned on the managed side");
                Equal(spline.GetProperty("size").GetInt32(), 200,
                    "Spline mirror size mismatch (16 * Vector3f + uint + float)");
            });

            Test("manifest includes project structs used only by commands", () =>
            {
                using var json = System.Text.Json.JsonDocument.Parse(
                    ComponentManifestBuilder.Build([], typeof(CommandOnlyComponent).Assembly));
                Assert(json.RootElement.EnumerateArray().Any(component =>
                        component.GetProperty("full_name").GetString() ==
                        typeof(CommandOnlyComponent).FullName),
                    "command-only project component was omitted from the manifest");
            });

            Test("ball physics declares PhysicsState, Position, and Sprite writes", () =>
            {
                var method = typeof(BallPhysicsSystem).GetMethod(nameof(BallPhysicsSystem.Run))
                    ?? throw new InvalidOperationException("BallPhysicsSystem.Run is missing");
                var system = ProjectHost.CreateSystem(method);

                Equal(system.Accesses.Length, 3, "unexpected ball access count");
                Assert(system.Accesses.All(access => access.Mode == 1),
                    "ball physics accesses must all be writable");
                Equal(system.Accesses[0].ComponentKey, Engine.ComponentKey(typeof(PhysicsState)),
                    "wrong PhysicsState key");
                Equal(system.Accesses[0].ComponentKeyHigh,
                    Engine.ComponentKeyHigh(typeof(PhysicsState)), "wrong PhysicsState high key");
                Equal(system.Accesses[1].ComponentKey, Engine.ComponentKey(typeof(Position)),
                    "wrong Position key");
                Equal(system.Accesses[1].ComponentKeyHigh,
                    Engine.ComponentKeyHigh(typeof(Position)), "wrong Position high key");
                Equal(system.Accesses[2].ComponentKey, Engine.ComponentKey(typeof(Sprite)),
                    "wrong Sprite key");
                Equal(system.Accesses[2].ComponentKeyHigh,
                    Engine.ComponentKeyHigh(typeof(Sprite)), "wrong Sprite high key");
            });

            Test("managed Commands parameter is reflected into system metadata", () =>
            {
                var commandsOnly = ProjectHost.CreateSystem(Method(nameof(TestSystems.CommandsOnly)));
                Assert(commandsOnly.UsesCommands, "Commands-only system did not declare commands");
                Equal(commandsOnly.Accesses.Length, 0, "Commands-only system has component access");
                var mixed = ProjectHost.CreateSystem(Method(nameof(TestSystems.QueryAndCommands)));
                Assert(mixed.UsesCommands, "query plus Commands did not declare commands");
                Equal(mixed.Accesses.Length, 1, "query plus Commands lost query access");
            });

            Test("project startup queues exactly 100 fully described balls", () =>
            {
                MockNativeWorld.ResetCommands();
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);
                var startups = ProjectHost.DiscoverStartups(typeof(ProjectStartup).Assembly);
                Equal(startups.Length, 1, "unexpected startup count");
                startups[0].Run();
                Equal(MockNativeWorld.QueuedCreates, 100, "startup create count mismatch");
                Equal(MockNativeWorld.LastCreateComponentCount, 4,
                    "each ball must contain PhysicsState, Position, Sprite, and BallTag");
                Equal(MockNativeWorld.NextEntityId, 100UL, "startup did not reserve unique entities");
            });

            Test("managed Commands reports stale entity generations", () =>
            {
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);
                Throws<InvalidOperationException>(
                    () => new Commands().DestroyEntity(new Entity(7, 99)),
                    "stale entity command should be rejected");
            });

            Test("a query system can queue despawn for its current entity", () =>
            {
                TestVelocity* velocities = stackalloc TestVelocity[1];
                Entity* entities = stackalloc Entity[1];
                velocities[0].X = 1;
                entities[0] = new Entity(55, 2);
                MockNativeWorld.Velocities = velocities;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 1;
                MockNativeWorld.ResetCommands();
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);
                ProjectHost.CreateSystem(Method(nameof(TestSystems.DespawnSystem))).Run();
                Equal(MockNativeWorld.QueuedDestroys, 1,
                    "despawn system did not enqueue destruction");
            });

            Test("project and shared component layouts match their manifests", () =>
            {
                Equal(Marshal.SizeOf<PhysicsState>(), 28, "PhysicsState size mismatch");
                Equal(Marshal.OffsetOf<PhysicsState>(nameof(PhysicsState.DeltaTime)).ToInt32(), 0,
                    "PhysicsState.DeltaTime offset mismatch");
                Equal(Marshal.OffsetOf<PhysicsState>(nameof(PhysicsState.Radius)).ToInt32(), 20,
                    "PhysicsState.Radius offset mismatch");
                Equal(Marshal.OffsetOf<PhysicsState>(nameof(PhysicsState.Active)).ToInt32(), 24,
                    "PhysicsState.Active offset mismatch");

                Equal(Marshal.SizeOf<Position>(), 8, "Position size mismatch");
                Equal(Marshal.SizeOf<Color>(), 16, "Color size mismatch");
                Equal(Marshal.SizeOf<Sprite>(), 24, "Sprite size mismatch");
                Equal(Marshal.OffsetOf<Sprite>(nameof(Sprite.Color)).ToInt32(), 8,
                    "Sprite.Color offset mismatch");
            });

            Test("native change tracking ABI layout is stable", () =>
            {
                Equal(Marshal.SizeOf<NativeComponentTicks>(), 8,
                    "NativeComponentTicks size mismatch");
                Equal(Marshal.OffsetOf<NativeComponentTicks>(nameof(NativeComponentTicks.Changed))
                    .ToInt32(), 4, "NativeComponentTicks.Changed offset mismatch");
                Equal(Marshal.SizeOf<NativeComponentChunk>(), 48,
                    "NativeComponentChunk size mismatch");
                Equal(Marshal.OffsetOf<NativeComponentChunk>(nameof(NativeComponentChunk.Ticks))
                    .ToInt32(), 32, "NativeComponentChunk.Ticks offset mismatch");
                Equal(Marshal.OffsetOf<NativeComponentChunk>(nameof(NativeComponentChunk.ChangeTick))
                    .ToInt32(), 40, "NativeComponentChunk.ChangeTick offset mismatch");
            });

            Test("single-term query reports one write", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.SingleWriter)));
                Equal(system.Accesses.Length, 1, "unexpected access count");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), Engine.ComponentKeyHigh(typeof(TestPosition)), 1),
                    "wrong write access");
            });

            Test("composed query reports write then read", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.MixedAccess)));
                Equal(system.Accesses.Length, 2, "unexpected access count");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), Engine.ComponentKeyHigh(typeof(TestPosition)), 1),
                    "wrong writable access");
                Equal(system.Accesses[1],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestVelocity)), Engine.ComponentKeyHigh(typeof(TestVelocity)), 0),
                    "wrong read-only access");
            });

            Test("three composed terms report writes in declaration order", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.TripleWriter)));
                Equal(system.Accesses.Length, 3, "unexpected access count");
                Equal(system.Accesses[0].ComponentKey, Engine.ComponentKey(typeof(TestPosition)),
                    "wrong first key");
                Equal(system.Accesses[1].ComponentKey, Engine.ComponentKey(typeof(TestVelocity)),
                    "wrong second key");
                Equal(system.Accesses[2].ComponentKey, Engine.ComponentKey(typeof(TestHealth)),
                    "wrong third key");
                Assert(system.Accesses.All(access => access.Mode == 1),
                    "composed write terms must report only writes");
            });

            Test("entity and optional terms produce exact scheduler access", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.OptionalAndEntity)));
                Equal(system.Accesses.Length, 2, "EntityTerm must not create scheduler access");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), Engine.ComponentKeyHigh(typeof(TestPosition)), 0),
                    "wrong required read access");
                Equal(system.Accesses[1],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestHealth)), Engine.ComponentKeyHigh(typeof(TestHealth)), 1),
                    "wrong optional write access");
            });

            Test("query arities one through eight build closed descriptors", () =>
            {
                Type[] definitions =
                [
                    typeof(Query<>),
                    typeof(Query<,>),
                    typeof(Query<,,>),
                    typeof(Query<,,,>),
                    typeof(Query<,,,,>),
                    typeof(Query<,,,,,>),
                    typeof(Query<,,,,,,>),
                    typeof(Query<,,,,,,,>),
                ];
                Type[] terms =
                [
                    typeof(Read<TestPosition>),
                    typeof(Write<TestVelocity>),
                    typeof(OptionalRead<TestHealth>),
                    typeof(Write<TestComponent4>),
                    typeof(Read<TestComponent5>),
                    typeof(Write<TestComponent6>),
                    typeof(Read<TestComponent7>),
                    typeof(OptionalWrite<TestComponent8>),
                ];
                for (int arity = 1; arity <= definitions.Length; arity++)
                {
                    Type closed = definitions[arity - 1]
                        .MakeGenericType(terms.Take(arity).ToArray());
                    var query = (IQueryDescriptor)(Activator.CreateInstance(
                        closed, nonPublic: true)
                        ?? throw new InvalidOperationException(
                            $"Could not instantiate query arity {arity}."));
                    Equal(query.Descriptor.Terms.Count, arity,
                        $"query arity {arity} descriptor term count mismatch");
                }
            });

            Test("eight-term query exports every scheduler access in order", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.EightTerms)));
                Type[] componentTypes =
                [
                    typeof(TestPosition), typeof(TestVelocity), typeof(TestHealth),
                    typeof(TestComponent4), typeof(TestComponent5),
                    typeof(TestComponent6), typeof(TestComponent7),
                    typeof(TestComponent8),
                ];
                byte[] modes = [0, 1, 0, 1, 0, 1, 0, 1];
                Equal(system.Accesses.Length, componentTypes.Length,
                    "eight-term scheduler access count mismatch");
                for (int index = 0; index < componentTypes.Length; index++)
                {
                    Equal(system.Accesses[index], new ManagedAccess(
                            Engine.ComponentKey(componentTypes[index]),
                            Engine.ComponentKeyHigh(componentTypes[index]),
                            modes[index]),
                        $"wrong scheduler access at term {index}");
                }
            });

            Test("query rows and enumerators are stack-only", () =>
            {
                Assert(typeof(QueryRow).IsByRefLike, "QueryRow must remain a ref struct");
                Assert(typeof(QueryEnumerator).IsByRefLike,
                    "QueryEnumerator must remain a ref struct");
                Assert(typeof(QueryRow<Read<TestPosition>, None, None, None, None, None, None, None>).IsByRefLike,
                    "typed QueryRow must remain a ref struct");
                Assert(typeof(QueryEnumerator<Read<TestPosition>, None, None, None, None, None, None, None>).IsByRefLike,
                    "typed QueryEnumerator must remain a ref struct");
                Assert(typeof(OptionalReadRef<TestHealth>).IsByRefLike,
                    "OptionalReadRef must remain a ref struct");
                Assert(typeof(OptionalWriteRef<TestHealth>).IsByRefLike,
                    "OptionalWriteRef must remain a ref struct");
            });

            Test("composed query iterates required optional and entity terms", () =>
            {
                TestPosition* positions = stackalloc TestPosition[2];
                TestVelocity* velocities = stackalloc TestVelocity[2];
                Entity* entities = stackalloc Entity[2];
                NativeComponentTicks* positionTicks = stackalloc NativeComponentTicks[2];
                NativeComponentTicks* velocityTicks = stackalloc NativeComponentTicks[2];
                positions[0].X = 1;
                positions[1].X = 2;
                velocities[0].X = 10;
                velocities[1].X = 20;
                entities[0] = new Entity(100, 3);
                entities[1] = new Entity(200, 4);
                positionTicks[0] = positionTicks[1] = new NativeComponentTicks
                { Added = 1, Changed = 2 };
                velocityTicks[0] = velocityTicks[1] = new NativeComponentTicks
                { Added = 1, Changed = 2 };

                MockNativeWorld.Positions = positions;
                MockNativeWorld.Velocities = velocities;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.PositionTicks = positionTicks;
                MockNativeWorld.VelocityTicks = velocityTicks;
                MockNativeWorld.Length = 2;
                MockNativeWorld.ChangeTick = 9;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var query = new Query<
                    Write<TestPosition>, Read<TestVelocity>,
                    OptionalWrite<TestHealth>, EntityTerm>();
                var seen = 0;
                foreach (var row in query)
                {
                    if (row.Entity.Id == 100)
                    {
                        ref TestPosition position = ref row.Write<TestPosition>();
                        ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
                        position.X += velocity.X;
                    }
                    Assert(!row.OptionalWrite<TestHealth>().HasValue,
                        "missing optional component unexpectedly has a value");
                    Equal(row.Entity.Id, entities[seen].Id, "wrong entity joined to row");
                    seen++;
                }

                Equal(seen, 2, "wrong composed query row count");
                Equal(positions[0].X, 11.0f, "first writable row was not updated");
                Equal(positions[1].X, 2.0f, "unrequested writable row was modified");
                Equal(positionTicks[0].Changed, 9u, "written row was not marked changed");
                Equal(positionTicks[1].Changed, 2u, "unrequested row was marked changed");
                Equal(velocityTicks[0].Changed, 2u, "read-only row was marked changed");
                Equal(velocityTicks[1].Changed, 2u, "read-only row was marked changed");
            });

            Test("typed rows alias native columns instead of copying them", () =>
            {
                TestPosition* positions = stackalloc TestPosition[1];
                TestVelocity* velocities = stackalloc TestVelocity[1];
                positions[0].X = 5;
                velocities[0].X = 7;
                MockNativeWorld.Positions = positions;
                MockNativeWorld.Velocities = velocities;
                MockNativeWorld.Length = 1;
                MockNativeWorld.ChangeTick = 3;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var query = new Query<Write<TestPosition>, Read<TestVelocity>>();
                foreach (var row in query)
                {
                    ref TestPosition position = ref row.Write<TestPosition>();
                    ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
                    Assert(Unsafe.AreSame(ref position, ref positions[0]),
                        "writable accessor did not alias the native row");
                    Assert(Unsafe.AreSame(ref Unsafe.AsRef(in velocity), ref velocities[0]),
                        "read-only accessor did not alias the native row");
                    position.X += velocity.X;
                }

                Equal(positions[0].X, 12.0f, "aliased write did not reach native storage");
                MockNativeWorld.Positions = null;
                MockNativeWorld.Velocities = null;
            });

            Test("typed rows reject access the query did not declare", () =>
            {
                TestPosition* positions = stackalloc TestPosition[1];
                TestHealth* health = stackalloc TestHealth[1];
                positions[0].X = 1;
                health[0].Value = 2;
                MockNativeWorld.Positions = positions;
                MockNativeWorld.Healths = health;
                MockNativeWorld.Length = 1;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var query = new Query<Write<TestPosition>, OptionalRead<TestHealth>>();
                foreach (var row in query)
                {
                    // Positive control: the declared access resolves and writes.
                    ref TestPosition position = ref row.Write<TestPosition>();
                    position.X += 1;

                    bool rejected = false;
                    try { row.Read<TestPosition>(); }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "read access to a write-only term must be rejected");

                    rejected = false;
                    try { row.OptionalRead<TestPosition>(); }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "optional read access to a required term must be rejected");

                    rejected = false;
                    try { row.Read<TestHealth>(); }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "required read access to an optional term must be rejected");

                    rejected = false;
                    try { row.OptionalWrite<TestHealth>(); }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "optional write access to an optional read term must be rejected");

                    rejected = false;
                    try { row.Read<TestComponent4>(); }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "access to an undeclared component must be rejected");

                    rejected = false;
                    try { _ = row.Entity; }
                    catch (InvalidOperationException) { rejected = true; }
                    Assert(rejected, "EntityTerm access without the term must be rejected");
                }

                Equal(positions[0].X, 2.0f, "positive control did not write");
                MockNativeWorld.Positions = null;
                MockNativeWorld.Healths = null;
            });

            Test("optional writes mark only rows whose value is requested", () =>
            {
                TestHealth* health = stackalloc TestHealth[2];
                Entity* entities = stackalloc Entity[2];
                NativeComponentTicks* ticks = stackalloc NativeComponentTicks[2];
                health[0].Value = 10;
                health[1].Value = 20;
                entities[0] = new Entity(100, 3);
                entities[1] = new Entity(200, 4);
                ticks[0] = ticks[1] = new NativeComponentTicks { Added = 1, Changed = 2 };

                MockNativeWorld.Healths = health;
                MockNativeWorld.HealthTicks = ticks;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 2;
                MockNativeWorld.ChangeTick = 12;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var query = new Query<OptionalWrite<TestHealth>, EntityTerm>();
                foreach (var row in query)
                {
                    var optional = row.OptionalWrite<TestHealth>();
                    Assert(optional.HasValue, "present optional component was not found");
                    if (row.Entity.Id == 100)
                        optional.Value.Value += 5;
                }

                Equal(health[0].Value, 15.0f, "optional writable value was not updated");
                Equal(health[1].Value, 20.0f, "unrequested optional value was modified");
                Equal(ticks[0].Changed, 12u, "optional written row was not marked changed");
                Equal(ticks[1].Changed, 2u, "HasValue incorrectly marked the optional row");
                MockNativeWorld.Healths = null;
                MockNativeWorld.HealthTicks = null;
            });

            Test("optional-read-only query yields present and missing components", () =>
            {
                TestHealth* health = stackalloc TestHealth[1];
                Entity* entities = stackalloc Entity[1];
                health[0].Value = 42;
                entities[0] = new Entity(300, 5);
                MockNativeWorld.Healths = health;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 1;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var presentRows = 0;
                foreach (var row in new Query<OptionalRead<TestHealth>>())
                {
                    var optional = row.OptionalRead<TestHealth>();
                    Assert(optional.HasValue, "present optional read was not found");
                    Equal(optional.Value.Value, 42.0f, "optional read returned the wrong value");
                    presentRows++;
                }
                Equal(presentRows, 1, "optional-only query did not use the entity driver");

                MockNativeWorld.Healths = null;
                var missingRows = 0;
                foreach (var row in new Query<OptionalRead<TestHealth>>())
                {
                    Assert(!row.OptionalRead<TestHealth>().HasValue,
                        "missing optional read unexpectedly had a value");
                    missingRows++;
                }
                Equal(missingRows, 1,
                    "optional-only query skipped entities without the component");
            });

            Test("entity-only query iterates native entity chunks", () =>
            {
                Entity* entities = stackalloc Entity[2];
                entities[0] = new Entity(400, 6);
                entities[1] = new Entity(500, 7);
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 2;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                ulong idSum = 0;
                var rows = 0;
                foreach (var row in new Query<EntityTerm>())
                {
                    idSum += row.Entity.Id;
                    rows++;
                }
                Equal(rows, 2, "entity-only query row count mismatch");
                Equal(idSum, 900UL, "entity-only query returned the wrong entities");
            });

            Test("warmed typed row access performs no managed allocations", () =>
            {
                TestVelocity* velocities = stackalloc TestVelocity[1];
                Entity* entities = stackalloc Entity[1];
                velocities[0].X = 3;
                entities[0] = new Entity(600, 8);
                MockNativeWorld.Velocities = velocities;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 1;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                long allocated = -1;
                float sum = 0;
                foreach (var row in new Query<Read<TestVelocity>>())
                {
                    _ = row.Read<TestVelocity>().X; // Initialize generic metadata and JIT paths.
                    long before = GC.GetAllocatedBytesForCurrentThread();
                    for (int iteration = 0; iteration < 1_000; iteration++)
                        sum += row.Read<TestVelocity>().X;
                    allocated = GC.GetAllocatedBytesForCurrentThread() - before;
                }
                Equal(allocated, 0L, "typed row access allocated after warm-up");
                Equal(sum, 3_000.0f, "typed row access loop was not executed");
            });

            Test("compiled runner supplies its query parameter", () =>
            {
                TestSystems.WasRun = false;
                ProjectHost.CreateSystem(Method(nameof(TestSystems.Runner))).Run();
                Assert(TestSystems.WasRun, "compiled runner did not invoke the method");
            });

            Test("component keys are stable and type-specific", () =>
            {
                Equal(Engine.ComponentKey(typeof(Position)),
                    Engine.ComponentKey(typeof(Position)), "Position key is unstable");
                Assert(Engine.ComponentKey(typeof(PhysicsState)) !=
                       Engine.ComponentKey(typeof(Position)),
                    "different current project components produced the same key");
                Assert(Engine.ComponentKey(typeof(Position)) !=
                       Engine.ComponentKey(typeof(Sprite)),
                    "different current project components produced the same key");
            });

            Test("duplicate write/read component is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.DuplicateReadWrite))),
                    "duplicate write/read component should be rejected"));

            Test("duplicate component in a three-term query is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.DuplicateTriple))),
                    "duplicate component in a three-term query should be rejected"));

            Test("duplicate entity term is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.DuplicateEntity))),
                    "duplicate EntityTerm should be rejected"));

            Test("zero-parameter system is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.NoParameters))),
                    "zero-parameter system should be rejected"));

            Test("non-void system is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.NonVoid))),
                    "non-void system should be rejected"));

            Test("unsupported parameter is rejected", () =>
                Throws<InvalidOperationException>(
                    () => ProjectHost.CreateSystem(Method(nameof(TestSystems.Unsupported))),
                    "unsupported parameter should be rejected"));

            Test("invalid managed component layout is rejected before registration", () =>
            {
                var system = ProjectHost.CreateSystem(Method(nameof(TestSystems.InvalidLayout)));
                Throws<InvalidOperationException>(
                    () => ComponentManifestBuilder.Build([system]),
                    "bool fields must be rejected from native component manifests");
            });

            Test("loader interop runs a discovered system and clears its error slot", () =>
            {
                var projectAssembly = typeof(BallPhysicsSystem).Assembly;
                Environment.SetEnvironmentVariable(
                    "ECS_CSHARP_PROJECT_DIR", Path.GetDirectoryName(projectAssembly.Location));
                Environment.SetEnvironmentVariable(
                    "ECS_CSHARP_PROJECT_ASSEMBLY", Path.GetFileName(projectAssembly.Location));
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                // The Rust host consumes these exports through function
                // pointers, so the regression tests do exactly the same.
                var init = (delegate* unmanaged<nint, byte>)&LoaderInterop.Init;
                var systemCount = (delegate* unmanaged<uint>)&LoaderInterop.SystemCount;
                var runSystem = (delegate* unmanaged<uint, byte>)&LoaderInterop.RunSystem;
                var errorLength =
                    (delegate* unmanaged<uint, uint>)&LoaderInterop.SystemErrorMessageLength;

                Equal(init((IntPtr)(&api)), 1, "loader init failed");
                // BallPhysicsSystem + BallTagSystem + the module-spline bridge demo.
                Equal(systemCount(), 3u, "unexpected managed system count");
                Equal(runSystem(1), 1, "healthy system reported failure");
                Equal(errorLength(1), 0u,
                    "healthy system carries a stale error message");
            });

            Test("loader interop reports failure for an invalid system index", () =>
            {
                var runSystem = (delegate* unmanaged<uint, byte>)&LoaderInterop.RunSystem;
                Equal(runSystem(99), 0,
                    "invalid managed system index reported success");
            });

            Test("accessor addresses resolve and re-resolve across a host rebind", () =>
            {
                // Step 1: a fresh bind resolves the address a generated
                // accessor would cache for its trampoline.
                MirrorMethods.Reset();
                IntPtr first = (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr*, IntPtr*, byte>)
                    &MockNativeWorld.AccessorView;
                MirrorMethods.Register("TracyLive.Probe", "entries_view", first);
                int generation = MirrorMethods.Generation;
                Equal(MirrorMethods.Address("TracyLive.Probe", "entries_view"), first,
                    "a registered address did not resolve");

                // Step 2: an op the module never published is refused with the
                // registration message instead of handing out a null pointer.
                Throws<InvalidOperationException>(
                    () => MirrorMethods.Address("TracyLive.Probe", "missing_view"),
                    "an unregistered accessor must be refused");

                // Step 3: rebinding bumps the generation, which is what makes
                // generated caches re-resolve; the new bind's address wins.
                MirrorMethods.Reset();
                IntPtr second = (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr*, IntPtr*, byte>)
                    &MockNativeWorld.AccessorViewSecond;
                MirrorMethods.Register("TracyLive.Probe", "entries_view", second);
                Equal(MirrorMethods.Generation, generation + 1,
                    "a rebind must bump the generation");
                Equal(MirrorMethods.Address("TracyLive.Probe", "entries_view"), second,
                    "the rebind's address did not replace the previous one");
                Assert(second != first, "the two test trampolines must differ");
            });

            Test("raw accessor invokes call the registered trampolines", () =>
            {
                // The five helpers cover every heap-field trampoline shape;
                // the argument order of each is part of the ABI, so the
                // trampolines echo the arguments back.
                MirrorMethods.Reset();
                MirrorMethods.Register("TracyLive.Probe", "entries_view",
                    (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr*, IntPtr*, byte>)
                        &MockNativeWorld.AccessorView);
                MirrorMethods.Register("TracyLive.Probe", "entries_resize",
                    (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr, byte>)
                        &MockNativeWorld.AccessorResize);
                MirrorMethods.Register("TracyLive.Probe", "entries_item",
                    (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr, IntPtr*, IntPtr*, byte>)
                        &MockNativeWorld.AccessorItem);
                MirrorMethods.Register("TracyLive.Probe", "entries_set_item",
                    (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr, IntPtr, IntPtr, byte>)
                        &MockNativeWorld.AccessorSetItem);
                MirrorMethods.Register("TracyLive.Probe", "entries_push",
                    (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr, IntPtr, byte>)
                        &MockNativeWorld.AccessorUtf8Write);

                byte status = MirrorMethods.InvokeView(
                    MirrorMethods.Address("TracyLive.Probe", "entries_view"),
                    (IntPtr)32, out IntPtr data, out IntPtr length);
                Equal(status, (byte)0, "view status");
                Equal(data, (IntPtr)64, "view data");
                Equal(length, (IntPtr)5, "view length");

                byte resized = MirrorMethods.InvokeResize(
                    MirrorMethods.Address("TracyLive.Probe", "entries_resize"),
                    (IntPtr)32, (IntPtr)7);
                Equal(resized, (byte)0, "resize status with the matching count");
                byte refused = MirrorMethods.InvokeResize(
                    MirrorMethods.Address("TracyLive.Probe", "entries_resize"),
                    (IntPtr)32, (IntPtr)8);
                Equal(refused, (byte)1, "resize status with a mismatched count");

                byte item = MirrorMethods.InvokeItem(
                    MirrorMethods.Address("TracyLive.Probe", "entries_item"),
                    (IntPtr)32, (IntPtr)4, out IntPtr itemData, out IntPtr itemLength);
                Equal(item, (byte)2, "item status");
                Equal(itemData, (IntPtr)40, "item data");
                Equal(itemLength, (IntPtr)5, "item length");

                byte setItem = MirrorMethods.InvokeSetItem(
                    MirrorMethods.Address("TracyLive.Probe", "entries_set_item"),
                    (IntPtr)32, (IntPtr)4, (IntPtr)0x1000, (IntPtr)3);
                Equal(setItem, (byte)0, "set_item status");

                byte written = MirrorMethods.InvokeUtf8Write(
                    MirrorMethods.Address("TracyLive.Probe", "entries_push"),
                    (IntPtr)32, (IntPtr)0x1000, (IntPtr)3);
                Equal(written, (byte)0, "utf8 write status");
            });

            Console.WriteLine($"C# ECS runtime tests passed: {_passed}");
            return 0;
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine(exception);
            return 1;
        }
    }

    // =========================================================================
    // Benchmark mode
    // =========================================================================

    /// <summary>Accumulator the benchmark passes publish into, so the JIT
    /// cannot discard the work as dead code.</summary>
    private static float _benchSink;

    /// <summary>Columns the raw-cursor ceiling loop walks (pointer locals cannot
    /// be captured by a delegate, so the loop reads them from fields).</summary>
    private static unsafe TestPosition* _benchPositions;
    private static unsafe TestVelocity* _benchVelocities;
    private static unsafe NativeComponentTicks* _benchPositionTicks;
    private static uint _benchCount;

    /// <summary>
    /// Loop overhead alone: entity-only iteration touches no component column,
    /// so this isolates MoveNext plus the per-row row construction.
    /// </summary>
    private static void IterateLoopOnly()
    {
        float sum = 0;
        foreach (var row in new Query<EntityTerm>())
            sum += row.Entity.Id;
        _benchSink = sum;
    }

    /// <summary>MoveNext alone, with no Current access, to price the loop state machine.</summary>
    private static void IterateMoveNextOnly()
    {
        float sum = 0;
        var enumerator = new Query<EntityTerm>().GetEnumerator();
        while (enumerator.MoveNext())
            sum += 1;
        _benchSink = sum;
    }

    /// <summary>MoveNext plus Current (no accessor call), to separate row construction from access.</summary>
    private static void IterateMoveNextCurrent()
    {
        float sum = 0;
        var enumerator = new Query<EntityTerm>().GetEnumerator();
        while (enumerator.MoveNext())
            sum += enumerator.Current.Entity.Id;
        _benchSink = sum;
    }

    /// <summary>One accessor per row, to split its cost from the loop's.</summary>
    private static void IterateSingleRead()
    {
        float sum = 0;
        foreach (var row in new Query<Read<TestVelocity>>())
            sum += row.Read<TestVelocity>().X;
        _benchSink = sum;
    }

    /// <summary>
    /// Three read terms per row - the C# counterpart of the Rust
    /// `query_multi_component` bench (`(&Position, &Velocity, &Health)`).
    /// </summary>
    private static void IterateReadTriple()
    {
        float sum = 0;
        foreach (var row in new Query<
            Read<TestPosition>, Read<TestVelocity>, Read<TestHealth>>())
        {
            sum += row.Read<TestPosition>().X
                + row.Read<TestVelocity>().X
                + row.Read<TestHealth>().Value;
        }
        _benchSink = sum;
    }

    /// <summary>
    /// The same component work as the write pass with none of the query
    /// machinery: a raw cursor loop over the native columns. This is the floor
    /// the query API is measured against - Rust's iterator compiles to
    /// something very close to it.
    /// </summary>
    private static unsafe void IterateRawPointerPair()
    {
        float sum = 0;
        TestPosition* positions = _benchPositions;
        TestVelocity* velocities = _benchVelocities;
        for (uint index = 0; index < _benchCount; index++)
        {
            positions[index].X += velocities[index].X;
            positions[index].Y += velocities[index].Y;
            sum += positions[index].X;
        }
        _benchSink = sum;
    }

    /// <summary>One joined column as the row sees it: the row holds a ref to
    /// this and reads `Data` per access, exactly like `QueryRow` does.</summary>
    private struct SimColumn
    {
        public unsafe void* Data;
    }

    /// <summary>
    /// Simulation of the ref-row design: the row is built ONCE and the index
    /// advances per row, so the per-row cost is the index store plus whatever
    /// the accessors load. This is the floor the Rust-style `foreach` row loop
    /// could reach; the switch/match chains are omitted because the JIT folds
    /// them for closed shapes.
    /// </summary>
    private ref struct SimRow
    {
        public ref SimColumn C0;
        public ref SimColumn C1;
        public ref SimColumn C2;
        public int Index;

        public SimRow(ref SimColumn c0, ref SimColumn c1, ref SimColumn c2)
        {
            C0 = ref c0;
            C1 = ref c1;
            C2 = ref c2;
            Index = 0;
        }
    }

    /// <summary>Ref-row read pair: index store plus two accessor loads.</summary>
    private static unsafe void IterateStoredRowReadPair()
    {
        SimColumn c0 = new SimColumn { Data = _benchPositions };
        SimColumn c1 = new SimColumn { Data = _benchVelocities };
        SimColumn c2 = new SimColumn { Data = _benchPositionTicks };
        var row = new SimRow(ref c0, ref c1, ref c2);
        int count = (int)_benchCount;
        float sum = 0;
        for (int index = 0; index < count; index++)
        {
            row.Index = index;
            ref readonly TestPosition position =
                ref ((TestPosition*)row.C0.Data)[row.Index];
            ref readonly TestVelocity velocity =
                ref ((TestVelocity*)row.C1.Data)[row.Index];
            sum += position.X + velocity.X;
        }
        _benchSink = sum;
    }

    /// <summary>Ref-row write pair: the read shape plus the change-tick store.</summary>
    private static unsafe void IterateStoredRowWritePair()
    {
        SimColumn c0 = new SimColumn { Data = _benchPositions };
        SimColumn c1 = new SimColumn { Data = _benchVelocities };
        SimColumn c2 = new SimColumn { Data = _benchPositionTicks };
        var row = new SimRow(ref c0, ref c1, ref c2);
        int count = (int)_benchCount;
        float sum = 0;
        for (int index = 0; index < count; index++)
        {
            row.Index = index;
            ref TestPosition position = ref ((TestPosition*)row.C0.Data)[row.Index];
            ref readonly TestVelocity velocity =
                ref ((TestVelocity*)row.C1.Data)[row.Index];
            position.X += velocity.X;
            position.Y += velocity.Y;
            ((NativeComponentTicks*)row.C2.Data)[row.Index].Changed = 9;
            sum += position.X;
        }
        _benchSink = sum;
    }

    /// <summary>
    /// The read pair as a slice-API inner loop: two spans in locals, index
    /// loop with the length comparison the JIT elides bounds checks for. This
    /// is the shape a `Slices()`/span iteration path would compile to.
    /// </summary>
    private static unsafe void IterateSpanReadPair()
    {
        ReadOnlySpan<TestPosition> positions = new(_benchPositions, (int)_benchCount);
        ReadOnlySpan<TestVelocity> velocities = new(_benchVelocities, (int)_benchCount);
        float sum = 0;
        for (int index = 0; index < positions.Length; index++)
            sum += positions[index].X + velocities[index].X;
        _benchSink = sum;
    }

    /// <summary>
    /// The writable pass as a slice-API inner loop, including the per-row
    /// change-tick stamp the row path performs.
    /// </summary>
    private static unsafe void IterateSpanWritePair()
    {
        Span<TestPosition> positions = new(_benchPositions, (int)_benchCount);
        ReadOnlySpan<TestVelocity> velocities = new(_benchVelocities, (int)_benchCount);
        NativeComponentTicks* ticks = _benchPositionTicks;
        float sum = 0;
        for (int index = 0; index < positions.Length; index++)
        {
            positions[index].X += velocities[index].X;
            positions[index].Y += velocities[index].Y;
            ticks[index].Changed = 9;
            sum += positions[index].X;
        }
        _benchSink = sum;
    }

    /// <summary>
    /// Time the two query shapes the Rust `query_iteration` criterion bench
    /// covers - a read-only pair and a writable pair - over mock chunks of
    /// the same population, and report nanoseconds per row. Each scenario
    /// runs a fixed row budget, and every measurement is best-of-five so
    /// scheduler noise cannot inflate it. Build in Release to compare against
    /// the criterion numbers; Debug inflates them.
    /// </summary>
    private static unsafe void RunQueryBenchmark(uint[] counts)
    {
        const int rowBudget = 2_000_000;
        foreach (uint count in counts)
        {
            int passes = Math.Max(3, rowBudget / (int)count);
            // Step 1: native columns the mock chunks report, filled with the
            // same deterministic values the Rust benchmark world uses.
            TestPosition* positions = (TestPosition*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(TestPosition));
            TestVelocity* velocities = (TestVelocity*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(TestVelocity));
            TestHealth* healths = (TestHealth*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(TestHealth));
            Entity* entities = (Entity*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(Entity));
            NativeComponentTicks* positionTicks = (NativeComponentTicks*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(NativeComponentTicks));
            NativeComponentTicks* velocityTicks = (NativeComponentTicks*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(NativeComponentTicks));
            NativeComponentTicks* healthTicks = (NativeComponentTicks*)NativeMemory.Alloc(
                (nuint)count * (nuint)sizeof(NativeComponentTicks));
            for (uint index = 0; index < count; index++)
            {
                positions[index] = new TestPosition { X = index, Y = index * 2 };
                velocities[index] = new TestVelocity { X = 0.1f, Y = 0.2f };
                healths[index] = new TestHealth { Value = index % 100 };
                entities[index] = new Entity(index, 1);
                positionTicks[index] = velocityTicks[index] = healthTicks[index] =
                    new NativeComponentTicks { Added = 1, Changed = 2 };
            }
            MockNativeWorld.Positions = positions;
            MockNativeWorld.Velocities = velocities;
            MockNativeWorld.Healths = healths;
            MockNativeWorld.Entities = entities;
            MockNativeWorld.PositionTicks = positionTicks;
            MockNativeWorld.VelocityTicks = velocityTicks;
            MockNativeWorld.HealthTicks = healthTicks;
            MockNativeWorld.Length = count;
            MockNativeWorld.ChangeTick = 9;
            EngineApi api = MockNativeWorld.Api();
            Engine.Bind(&api);
            _benchPositions = positions;
            _benchVelocities = velocities;
            _benchPositionTicks = positionTicks;
            _benchCount = count;

            // Step 2: warm the JIT so the first timed pass is not the first
            // execution of the generic enumerator.
            for (int warmup = 0; warmup < 10; warmup++)
            {
                IterateMoveNextOnly();
                IterateMoveNextCurrent();
                IterateLoopOnly();
                IterateSingleRead();
                IterateReadTriple();
                IterateReadPair();
                IterateWritePair();
                IterateNamedRowsWritePair();
                IterateRawPointerPair();
                IterateSpanReadPair();
                IterateSpanWritePair();
                IterateStoredRowReadPair();
                IterateStoredRowWritePair();
            }

            double readSeconds = BestSeconds(5, passes, IterateReadPair);
            double writeSeconds = BestSeconds(5, passes, IterateWritePair);
            double namedSeconds = BestSeconds(5, passes, IterateNamedRowsWritePair);
            double legacySeconds = BestSeconds(5, passes, IterateLegacyWritePair);
            double loopSeconds = BestSeconds(5, passes, IterateLoopOnly);
            double moveNextSeconds = BestSeconds(5, passes, IterateMoveNextOnly);
            double moveNextCurrentSeconds = BestSeconds(5, passes, IterateMoveNextCurrent);
            double oneReadSeconds = BestSeconds(5, passes, IterateSingleRead);
            double tripleSeconds = BestSeconds(5, passes, IterateReadTriple);
            double ceilingSeconds = BestSeconds(5, passes, IterateRawPointerPair);
            double spanReadSeconds = BestSeconds(5, passes, IterateSpanReadPair);
            double spanWriteSeconds = BestSeconds(5, passes, IterateSpanWritePair);
            double storedReadSeconds = BestSeconds(5, passes, IterateStoredRowReadPair);
            double storedWriteSeconds = BestSeconds(5, passes, IterateStoredRowWritePair);
            double rows = (double)passes * count;
            Console.WriteLine(
                $"[bench] query rows={count} iters={passes} " +
                $"move_next_ns_per_row={moveNextSeconds * 1e9 / rows:F2} " +
                $"move_next_current_ns_per_row={moveNextCurrentSeconds * 1e9 / rows:F2} " +
                $"loop_only_ns_per_row={loopSeconds * 1e9 / rows:F2} " +
                $"one_read_ns_per_row={oneReadSeconds * 1e9 / rows:F2} " +
                $"read_triple_ns_per_row={tripleSeconds * 1e9 / rows:F2} " +
                $"read_pair_ns_per_row={readSeconds * 1e9 / rows:F2} " +
                $"write_pair_ns_per_row={writeSeconds * 1e9 / rows:F2} " +
                $"named_rows_write_ns_per_row={namedSeconds * 1e9 / rows:F2} " +
                $"raw_pointer_ns_per_row={ceilingSeconds * 1e9 / rows:F2} " +
                $"span_read_pair_ns_per_row={spanReadSeconds * 1e9 / rows:F2} " +
                $"span_write_tick_ns_per_row={spanWriteSeconds * 1e9 / rows:F2} " +
                $"ref_row_read_ns_per_row={storedReadSeconds * 1e9 / rows:F2} " +
                $"ref_row_write_ns_per_row={storedWriteSeconds * 1e9 / rows:F2} " +
                $"legacy_find_pair_ns_per_row={legacySeconds * 1e9 / rows:F2} " +
                $"checksum={_benchSink:F1}");

            // Step 3: release the benchmark columns.
            NativeMemory.Free(positions);
            NativeMemory.Free(velocities);
            NativeMemory.Free(healths);
            NativeMemory.Free(entities);
            NativeMemory.Free(positionTicks);
            NativeMemory.Free(velocityTicks);
            NativeMemory.Free(healthTicks);
            MockNativeWorld.Positions = null;
            MockNativeWorld.Velocities = null;
            MockNativeWorld.Healths = null;
            MockNativeWorld.Entities = null;
            MockNativeWorld.PositionTicks = null;
            MockNativeWorld.VelocityTicks = null;
            MockNativeWorld.HealthTicks = null;
            _benchPositions = null;
            _benchVelocities = null;
            _benchPositionTicks = null;
            _benchCount = 0;
        }
    }

    /// <summary>
    /// One read-only pass over the mock chunks - the C# shape of the Rust
    /// `Query::<(&Position, &Velocity)>` bench. Velocity and Health stand in
    /// for the Rust pair because the mock only maps Position as writable.
    /// </summary>
    private static void IterateReadPair()
    {
        float sum = 0;
        foreach (var row in new Query<Read<TestVelocity>, Read<TestHealth>>())
        {
            ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
            ref readonly TestHealth health = ref row.Read<TestHealth>();
            sum += velocity.X + health.Value;
        }
        _benchSink = sum;
    }

    /// <summary>One writable pass - the C# shape of the Rust `(&mut Position, &Velocity)` bench.</summary>
    private static void IterateWritePair()
    {
        float sum = 0;
        foreach (var row in new Query<Write<TestPosition>, Read<TestVelocity>>())
        {
            ref TestPosition position = ref row.Write<TestPosition>();
            ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
            position.X += velocity.X;
            position.Y += velocity.Y;
            sum += position.X;
        }
        _benchSink = sum;
    }

    /// <summary>
    /// The same pass through the source generator's named rows
    /// (`query.Rows()` with per-term properties), binding each term once -
    /// the idiomatic use, and the shape project_cs ships.
    /// </summary>
    private static void IterateNamedRowsWritePair()
    {
        float sum = 0;
        foreach (var row in new Query<Write<TestPosition>, Read<TestVelocity>>().Rows())
        {
            ref TestPosition position = ref row.TestPosition;
            ref readonly TestVelocity velocity = ref row.TestVelocity;
            position.X += velocity.X;
            position.Y += velocity.Y;
            sum += position.X;
        }
        _benchSink = sum;
    }

    /// <summary>
    /// The same writable pass through the pre-P0 shape-erased row, whose
    /// accessors scan the query's columns per row (`Find&lt;T&gt;`). This is the
    /// "before" the typed rows measured against, kept so the lookup cost the
    /// typed path removes stays quantified.
    /// </summary>
    private static void IterateLegacyWritePair()
    {
        float sum = 0;
        var query = new Query<Write<TestPosition>, Read<TestVelocity>>();
        var enumerator = ((QueryBase)query).GetEnumerator();
        while (enumerator.MoveNext())
        {
            QueryRow row = enumerator.Current;
            ref TestPosition position = ref row.Write<TestPosition>();
            ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
            position.X += velocity.X;
            position.Y += velocity.Y;
            sum += position.X;
        }
        _benchSink = sum;
    }

    /// <summary>Run <paramref name="pass"/> <paramref name="iterations"/> times and return the wall seconds.</summary>
    private static double MeasureSeconds(int iterations, Action pass)
    {
        var watch = new System.Diagnostics.Stopwatch();
        watch.Start();
        for (int iteration = 0; iteration < iterations; iteration++)
            pass();
        watch.Stop();
        return watch.Elapsed.TotalSeconds;
    }

    /// <summary>Best of <paramref name="repetitions"/> timed runs, to damp scheduler noise.</summary>
    private static double BestSeconds(int repetitions, int iterations, Action pass)
    {
        double best = double.MaxValue;
        for (int repetition = 0; repetition < repetitions; repetition++)
            best = Math.Min(best, MeasureSeconds(iterations, pass));
        return best;
    }

    /// <summary>
    /// Time the generated-accessor steady state: one generation compare, one
    /// cached raw view call, then the span walk a `{P}` property performs.
    /// Call and walk are timed separately so the boundary price and the
    /// element price do not hide each other.
    /// </summary>
    private static unsafe void RunAccessorBenchmark(int iterations)
    {
        const int elementCount = 64;
        float* elements = (float*)NativeMemory.Alloc((nuint)elementCount * sizeof(float));
        for (int index = 0; index < elementCount; index++)
            elements[index] = index;
        MockNativeWorld.AccessorElements = elements;
        MockNativeWorld.AccessorElementCount = elementCount;
        MirrorMethods.Reset();
        MirrorMethods.Register("Bench.Probe", "elements_view",
            (IntPtr)(delegate* unmanaged[Cdecl]<IntPtr, IntPtr*, IntPtr*, byte>)
                &MockNativeWorld.AccessorElementsView);

        // Mirror what a generated member caches between binds, then time the
        // call alone and the call plus the element walk.
        IntPtr address = MirrorMethods.Address("Bench.Probe", "elements_view");
        int boundGeneration = MirrorMethods.Generation;
        float callSink = 0;
        void CallOnly()
        {
            if (boundGeneration != MirrorMethods.Generation)
            {
                address = MirrorMethods.Address("Bench.Probe", "elements_view");
                boundGeneration = MirrorMethods.Generation;
            }
            MirrorMethods.InvokeView(address, (IntPtr)1, out IntPtr _, out IntPtr length);
            callSink += length;
        }

        float walkSink = 0;
        void CallAndWalk()
        {
            if (boundGeneration != MirrorMethods.Generation)
            {
                address = MirrorMethods.Address("Bench.Probe", "elements_view");
                boundGeneration = MirrorMethods.Generation;
            }
            MirrorMethods.InvokeView(address, (IntPtr)1, out IntPtr data, out IntPtr length);
            ReadOnlySpan<float> span = ComponentViews.AsReadOnlySpan<float>(data, checked((int)length));
            float sum = 0;
            for (int element = 0; element < span.Length; element++)
                sum += span[element];
            walkSink = sum;
        }

        // The idiomatic bounds-check-free walk: foreach over the span. Rust's
        // `iter().sum()` compiles to this shape, so it is the fair per-element
        // comparison.
        void CallAndWalkForeach()
        {
            if (boundGeneration != MirrorMethods.Generation)
            {
                address = MirrorMethods.Address("Bench.Probe", "elements_view");
                boundGeneration = MirrorMethods.Generation;
            }
            MirrorMethods.InvokeView(address, (IntPtr)1, out IntPtr data, out IntPtr length);
            ReadOnlySpan<float> span = ComponentViews.AsReadOnlySpan<float>(data, checked((int)length));
            float sum = 0;
            foreach (ref readonly float element in span)
                sum += element;
            walkSink = sum;
        }

        double callSeconds = BestSeconds(5, iterations, CallOnly);
        double fullSeconds = BestSeconds(5, iterations, CallAndWalk);
        double foreachSeconds = BestSeconds(5, iterations, CallAndWalkForeach);

        // The mock's trampolines are managed methods reached through an
        // unmanaged function pointer, so every call pays a managed ->
        // unmanaged -> managed round trip that real native trampolines do not.
        // Time that tax with the cheapest possible FFI call so it can be
        // subtracted from the accessor number.
        float taxSink = 0;
        void TaxLoop()
        {
            for (int iteration = 0; iteration < iterations; iteration++)
                taxSink += Engine.EntityCount();
        }
        double mockFfiSeconds = BestSeconds(5, 1, TaxLoop);
        double mockFfiNanoseconds = mockFfiSeconds * 1e9 / iterations;
        double callNanoseconds = callSeconds * 1e9 / iterations;
        double fullNanoseconds = fullSeconds * 1e9 / iterations;
        Console.WriteLine(
            $"[bench] accessor calls={iterations} elements={elementCount} " +
            $"call_ns={callNanoseconds:F1} full_ns={fullNanoseconds:F1} " +
            $"walk_ns={fullNanoseconds - callNanoseconds:F1} " +
            $"walk_element_ns={(fullNanoseconds - callNanoseconds) / elementCount:F2} " +
            $"foreach_ns={foreachSeconds * 1e9 / iterations:F1} " +
            $"foreach_element_ns={(foreachSeconds * 1e9 / iterations - callNanoseconds) / elementCount:F2} " +
            $"mock_ffi_tax_ns={mockFfiNanoseconds:F1} " +
            $"call_ns_minus_mock={callNanoseconds - mockFfiNanoseconds:F1} " +
            $"checksum={callSink + walkSink + taxSink + _benchSink:F1}");
        NativeMemory.Free(elements);
        MockNativeWorld.AccessorElements = null;
    }
}
