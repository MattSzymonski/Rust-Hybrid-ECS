// Executable regression tests for the managed ECS runtime and current C# game.
//
// Responsibilities:
// - Verifies scheduler access derived from every supported query shape.
// - Validates the actual bouncing-ball system discovered from game_cs.dll.
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
internal struct InvalidBoolComponent { public bool Value; }

internal static class TestSystems
{
    internal static bool WasRun;

    public static void Runner(Query<Write<TestPosition>, Read<TestVelocity>> query) =>
        WasRun = query is not null;

    public static void SingleWriter(Query<Write<TestPosition>> query) { }

    public static void MixedAccess(Query<Write<TestPosition>, Read<TestVelocity>> query) { }

    public static void TripleWriter(
        Query<Write<TestPosition>, Write<TestVelocity>, Write<TestHealth>> query) { }

    public static void OptionalAndEntity(
        Query<EntityTerm, Read<TestPosition>, OptionalWrite<TestHealth>> query) { }

    public static void DuplicateReadWrite(
        Query<Write<TestPosition>, Read<TestPosition>> query) { }

    public static void DuplicateTriple(
        Query<Write<TestPosition>, Write<TestPosition>, Write<TestHealth>> query) { }

    public static void DuplicateEntity(Query<EntityTerm, EntityTerm> query) { }

    public static void NoParameters() { }

    public static int NonVoid(Query<Write<TestPosition>> query) => 0;

    public static void Unsupported(string value) { }

    public static void InvalidLayout(Query<Read<InvalidBoolComponent>> query) { }
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

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static uint EntityCount() => Length;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetComponentChunk(
        ulong key, ulong keyHigh, byte mode, uint index, NativeComponentChunk* output)
    {
        if (index != 0)
            return 0;
        if (key == Engine.ComponentKey(typeof(TestPosition)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestPosition)) && mode == 1)
        {
            *output = Chunk(Positions, PositionTicks, sizeof(TestPosition));
            return 1;
        }
        if (key == Engine.ComponentKey(typeof(TestVelocity)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestVelocity)) && mode == 0)
        {
            *output = Chunk(Velocities, VelocityTicks, sizeof(TestVelocity));
            return 1;
        }
        if (key == Engine.ComponentKey(typeof(TestHealth)) &&
            keyHigh == Engine.ComponentKeyHigh(typeof(TestHealth)) && mode == 1 && Healths != null)
        {
            *output = Chunk(Healths, HealthTicks, sizeof(TestHealth));
            return 1;
        }
        return 0;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetEntityChunk(uint index, NativeComponentChunk* output)
    {
        if (index != 0)
            return 0;
        *output = Chunk(Entities, null, sizeof(Entity));
        return 1;
    }

    internal static EngineApi Api() => new()
    {
        EntityCount = &EntityCount,
        GetComponentChunk = &GetComponentChunk,
        GetEntityChunk = &GetEntityChunk,
    };

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

    public static unsafe int Main()
    {
        try
        {
            Test("current game discovers native and dynamic component systems", () =>
            {
                var systems = GameHost.DiscoverSystems(typeof(BallPhysicsSystem).Assembly);
                Equal(systems.Length, 2, "unexpected game system count");
                Assert(systems.Any(system => system.Name == "TracyLive.BallPhysicsSystem.Run"),
                    "ball physics system was not discovered");
                Assert(systems.Any(system => system.Name == "TracyLive.BallTagSystem.Observe"),
                    "dynamic component system was not discovered");
            });

            Test("component manifest separates runtime mirrors from game components", () =>
            {
                var systems = GameHost.DiscoverSystems(typeof(BallPhysicsSystem).Assembly);
                using var json = System.Text.Json.JsonDocument.Parse(
                    ComponentManifestBuilder.Build(systems));
                var components = json.RootElement.EnumerateArray().ToArray();
                Equal(components.Length, 4, "unexpected manifest component count");
                var position = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.Position");
                var sprite = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.Sprite");
                var physics = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.PhysicsState");
                var tag = components.Single(component =>
                    component.GetProperty("full_name").GetString() == "TracyLive.BallTag");
                Assert(position.GetProperty("shared").GetBoolean(),
                    "runtime Position mirror must be shared");
                Assert(sprite.GetProperty("shared").GetBoolean(),
                    "runtime Sprite mirror must be shared");
                Assert(!physics.GetProperty("shared").GetBoolean(),
                    "game-owned PhysicsState must be dynamic");
                Assert(!tag.GetProperty("shared").GetBoolean(),
                    "game-owned BallTag must be dynamic");
                Equal(tag.GetProperty("size").GetInt32(), 4, "BallTag size mismatch");
                Equal(tag.GetProperty("alignment").GetInt32(), 4,
                    "BallTag alignment mismatch");
                Equal(tag.GetProperty("fields").GetArrayLength(), 1,
                    "BallTag field schema mismatch");
            });

            Test("ball physics declares PhysicsState, Position, and Sprite writes", () =>
            {
                var method = typeof(BallPhysicsSystem).GetMethod(nameof(BallPhysicsSystem.Run))
                    ?? throw new InvalidOperationException("BallPhysicsSystem.Run is missing");
                var system = GameHost.CreateSystem(method);

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

            Test("game and shared component layouts match their manifests", () =>
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
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.SingleWriter)));
                Equal(system.Accesses.Length, 1, "unexpected access count");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), Engine.ComponentKeyHigh(typeof(TestPosition)), 1),
                    "wrong write access");
            });

            Test("composed query reports write then read", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.MixedAccess)));
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
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.TripleWriter)));
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
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.OptionalAndEntity)));
                Equal(system.Accesses.Length, 2, "EntityTerm must not create scheduler access");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), Engine.ComponentKeyHigh(typeof(TestPosition)), 0),
                    "wrong required read access");
                Equal(system.Accesses[1],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestHealth)), Engine.ComponentKeyHigh(typeof(TestHealth)), 1),
                    "wrong optional write access");
            });

            Test("query rows and enumerators are stack-only", () =>
            {
                Assert(typeof(QueryRow).IsByRefLike, "QueryRow must remain a ref struct");
                Assert(typeof(QueryEnumerator).IsByRefLike,
                    "QueryEnumerator must remain a ref struct");
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

            Test("compiled runner supplies its query parameter", () =>
            {
                TestSystems.WasRun = false;
                GameHost.CreateSystem(Method(nameof(TestSystems.Runner))).Run();
                Assert(TestSystems.WasRun, "compiled runner did not invoke the method");
            });

            Test("component keys are stable and type-specific", () =>
            {
                Equal(Engine.ComponentKey(typeof(Position)),
                    Engine.ComponentKey(typeof(Position)), "Position key is unstable");
                Assert(Engine.ComponentKey(typeof(PhysicsState)) !=
                       Engine.ComponentKey(typeof(Position)),
                    "different current game components produced the same key");
                Assert(Engine.ComponentKey(typeof(Position)) !=
                       Engine.ComponentKey(typeof(Sprite)),
                    "different current game components produced the same key");
            });

            Test("duplicate write/read component is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.DuplicateReadWrite))),
                    "duplicate write/read component should be rejected"));

            Test("duplicate component in a three-term query is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.DuplicateTriple))),
                    "duplicate component in a three-term query should be rejected"));

            Test("duplicate entity term is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.DuplicateEntity))),
                    "duplicate EntityTerm should be rejected"));

            Test("zero-parameter system is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.NoParameters))),
                    "zero-parameter system should be rejected"));

            Test("non-void system is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.NonVoid))),
                    "non-void system should be rejected"));

            Test("unsupported parameter is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.Unsupported))),
                    "unsupported parameter should be rejected"));

            Test("invalid managed component layout is rejected before registration", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.InvalidLayout)));
                Throws<InvalidOperationException>(
                    () => ComponentManifestBuilder.Build([system]),
                    "bool fields must be rejected from native component manifests");
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
}
