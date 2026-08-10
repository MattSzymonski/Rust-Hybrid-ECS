using System.Reflection;
using System.Runtime.InteropServices;
using TracyLive;
using TracyLive.Loader;

namespace TracyLive.Tests;

[StructLayout(LayoutKind.Sequential)]
public struct Position { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Velocity { public float X, Y; }

[StructLayout(LayoutKind.Sequential)]
public struct Health { public float Value; }

internal static class TestSystems
{
    internal static bool WasRun;

    [EcsSystem]
    public static void Movement(WriteReadQuery<Position, Velocity> query) => WasRun = query is not null;

    [EcsSystem]
    public static void PositionWriter(WriteQuery<Position> query) { }
    public static void TripleWriter(Write3Query<Position, Velocity, Health> query) { }

    public static void Duplicate(WriteReadQuery<Position, Position> query) { }
    public static void NoParameters() { }
    public static int NonVoid(WriteQuery<Position> query) => 0;
    public static void Unsupported(string value) { }
}

internal static class Program
{
    private static int _passed;

    private static MethodInfo Method(string name) => typeof(TestSystems).GetMethod(name)!
        ?? throw new InvalidOperationException($"Missing test method {name}");

    private static void Assert(bool condition, string message)
    {
        if (!condition)
            throw new InvalidOperationException(message);
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

    public static int Main()
    {
        try
        {
            Test("WriteQuery reports one write", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.PositionWriter)));
                Assert(system.Accesses.Length == 1, "expected one access");
                Assert(system.Accesses[0].Mode == 1, "Position must be writable");
                Assert(system.Accesses[0].ComponentKey == Engine.ComponentKey(typeof(Position)),
                    "wrong Position component key");
            });

            Test("WriteReadQuery reports write then read", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.Movement)));
                Assert(system.Accesses.Length == 2, "expected two accesses");
                Assert(system.Accesses[0] == new ManagedAccess(Engine.ComponentKey(typeof(Position)), 1),
                    "Position access must be write");
                Assert(system.Accesses[1] == new ManagedAccess(Engine.ComponentKey(typeof(Velocity)), 0),
                    "Velocity access must be read");
            });

            Test("Write3Query reports three writes", () =>
            {
                var system = GameHost.CreateSystem(Method(nameof(TestSystems.TripleWriter)));
                Assert(system.Accesses.Length == 3, "expected three accesses");
                Assert(system.Accesses.All(access => access.Mode == 1),
                    "all Write3Query accesses must be writable");
                Assert(system.Accesses[0].ComponentKey == Engine.ComponentKey(typeof(Position)),
                    "wrong first component key");
                Assert(system.Accesses[1].ComponentKey == Engine.ComponentKey(typeof(Velocity)),
                    "wrong second component key");
                Assert(system.Accesses[2].ComponentKey == Engine.ComponentKey(typeof(Health)),
                    "wrong third component key");
            });

            Test("compiled runner supplies query parameter", () =>
            {
                TestSystems.WasRun = false;
                GameHost.CreateSystem(Method(nameof(TestSystems.Movement))).Run();
                Assert(TestSystems.WasRun, "generated runner did not invoke the method with a query");
            });

            Test("discovery finds only attributed systems in deterministic order", () =>
            {
                var systems = GameHost.DiscoverSystems(Assembly.GetExecutingAssembly());
                Assert(systems.Length == 2, $"expected two systems, got {systems.Length}");
                Assert(systems[0].Name.EndsWith(".Movement", StringComparison.Ordinal),
                    "systems were not deterministically sorted");
                Assert(systems[1].Name.EndsWith(".PositionWriter", StringComparison.Ordinal),
                    "systems were not deterministically sorted");
            });

            Test("duplicate read/write component is rejected", () =>
                Throws<InvalidOperationException>(
                    () => GameHost.CreateSystem(Method(nameof(TestSystems.Duplicate))),
                    "duplicate read/write component should be rejected"));
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

            Console.WriteLine($"C# ECS access tests passed: {_passed}");
            return 0;
        }
        catch (Exception exception)
        {
            Console.Error.WriteLine(exception);
            return 1;
        }
    }
}
