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
}

internal static unsafe class MockNativeWorld
{
    internal static TestPosition* Positions;
    internal static TestVelocity* Velocities;
    internal static Entity* Entities;
    internal static uint Length;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static uint EntityCount() => Length;

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetComponentChunk(
        ulong key, byte mode, uint index, NativeComponentChunk* output)
    {
        if (index != 0)
            return 0;
        if (key == Engine.ComponentKey(typeof(TestPosition)) && mode == 1)
        {
            *output = Chunk(Positions, sizeof(TestPosition));
            return 1;
        }
        if (key == Engine.ComponentKey(typeof(TestVelocity)) && mode == 0)
        {
            *output = Chunk(Velocities, sizeof(TestVelocity));
            return 1;
        }
        return 0;
    }

    [UnmanagedCallersOnly(CallConvs = [typeof(CallConvCdecl)])]
    internal static byte GetEntityChunk(uint index, NativeComponentChunk* output)
    {
        if (index != 0)
            return 0;
        *output = Chunk(Entities, sizeof(Entity));
        return 1;
    }

    internal static EngineApi Api() => new()
    {
        EntityCount = &EntityCount,
        GetComponentChunk = &GetComponentChunk,
        GetEntityChunk = &GetEntityChunk,
    };

    private static NativeComponentChunk Chunk(void* data, int elementSize) => new()
    {
        ArchetypeLow = 7,
        ArchetypeHigh = 11,
        Data = (IntPtr)data,
        Length = Length,
        ElementSize = checked((uint)elementSize),
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
            Test("current game discovers exactly the ball physics system", () =>
            {
                var systems = GameHost.DiscoverSystems(typeof(BallPhysicsSystem).Assembly);
                Equal(systems.Length, 1, "unexpected game system count");
                Equal(systems[0].Name, "TracyLive.BallPhysicsSystem.Run",
                    "unexpected managed system name");
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
                Equal(system.Accesses[1].ComponentKey, Engine.ComponentKey(typeof(Position)),
                    "wrong Position key");
                Equal(system.Accesses[2].ComponentKey, Engine.ComponentKey(typeof(Sprite)),
                    "wrong Sprite key");
            });

            Test("game component ABI layouts match the native bridge", () =>
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

            Test("single-term query reports one write", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.SingleWriter)));
                Equal(system.Accesses.Length, 1, "unexpected access count");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), 1),
                    "wrong write access");
            });

            Test("composed query reports write then read", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.MixedAccess)));
                Equal(system.Accesses.Length, 2, "unexpected access count");
                Equal(system.Accesses[0],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), 1),
                    "wrong writable access");
                Equal(system.Accesses[1],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestVelocity)), 0),
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
                    new ManagedAccess(Engine.ComponentKey(typeof(TestPosition)), 0),
                    "wrong required read access");
                Equal(system.Accesses[1],
                    new ManagedAccess(Engine.ComponentKey(typeof(TestHealth)), 1),
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
                positions[0].X = 1;
                positions[1].X = 2;
                velocities[0].X = 10;
                velocities[1].X = 20;
                entities[0] = new Entity(100, 3);
                entities[1] = new Entity(200, 4);

                MockNativeWorld.Positions = positions;
                MockNativeWorld.Velocities = velocities;
                MockNativeWorld.Entities = entities;
                MockNativeWorld.Length = 2;
                EngineApi api = MockNativeWorld.Api();
                Engine.Bind(&api);

                var query = new Query<
                    Write<TestPosition>, Read<TestVelocity>,
                    OptionalWrite<TestHealth>, EntityTerm>();
                var seen = 0;
                foreach (var row in query)
                {
                    ref TestPosition position = ref row.Write<TestPosition>();
                    ref readonly TestVelocity velocity = ref row.Read<TestVelocity>();
                    position.X += velocity.X;
                    Assert(!row.OptionalWrite<TestHealth>().HasValue,
                        "missing optional component unexpectedly has a value");
                    Equal(row.Entity.Id, entities[seen].Id, "wrong entity joined to row");
                    seen++;
                }

                Equal(seen, 2, "wrong composed query row count");
                Equal(positions[0].X, 11.0f, "first writable row was not updated");
                Equal(positions[1].X, 22.0f, "second writable row was not updated");
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
