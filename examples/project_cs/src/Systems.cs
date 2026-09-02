using System.Diagnostics;
using System.Runtime.InteropServices;

namespace TracyLive;

public static class ProjectStartup
{
    [EcsStartup]
    public static void Start(Commands commands)
    {
        for (int index = 0; index < 100; index++)
        {
            float column = index % 10;
            float row = index / 10;
            float radius = 10.0f + index % 4 * 2.0f;
            float positionX = 60.0f + column * 72.0f;
            float positionY = 60.0f + row * 42.0f;
            commands.CreateEntity()
                .With(new PhysicsState
                {
                    PositionX = positionX,
                    PositionY = positionY,
                    VelocityX = index % 2 == 0
                        ? BallPhysicsSystem.BounceVelocityX + row * 8.0f
                        : -BallPhysicsSystem.BounceVelocityX - row * 8.0f,
                    VelocityY = BallPhysicsSystem.BounceVelocityY + column * 18.0f,
                    Radius = radius,
                    Active = 1,
                })
                .With(new Position
                {
                    X = positionX - radius,
                    Y = positionY - radius,
                })
                .With(new Sprite
                {
                    Width = radius * 2.0f,
                    Height = radius * 2.0f,
                    Color = new Color { R = 1.0f, G = 0.3f, B = 0.3f, A = 1.0f },
                })
                .With(new BallTag { Kind = 1 })
                .Build();
        }
    }
}

public static class BallPhysicsSystem
{
    private const float Gravity = 800.0f;
    private const float Restitution = 0.7f;
    private const float FloorY = 580.0f;
    private const float CeilingY = 20.0f;
    private const float LeftWall = 20.0f;
    private const float RightWall = 780.0f;
    internal const float BounceVelocityY = -500.0f;
    internal const float BounceVelocityX = 150.0f;

    private static long _lastFrame = Stopwatch.GetTimestamp();

    [EcsSystem]
    public static void Run(
        Query<EntityTerm, Write<PhysicsState>, Write<Position>, Write<Sprite>> query)
    {
        long now = Stopwatch.GetTimestamp();
        float deltaTime = Math.Clamp(
            (float)Stopwatch.GetElapsedTime(_lastFrame, now).TotalSeconds,
            0.0f,
            0.1f);
        _lastFrame = now;

        foreach (var components in query)
        {
            ref var physics = ref components.Write<PhysicsState>();
            ref var position = ref components.Write<Position>();
            ref var sprite = ref components.Write<Sprite>();

            physics.DeltaTime = deltaTime;
            if (physics.Active != 0)
            {
                physics.VelocityY += Gravity * deltaTime;
                physics.PositionX += physics.VelocityX * deltaTime;
                physics.PositionY += physics.VelocityY * deltaTime;

                if (physics.PositionY + physics.Radius >= FloorY)
                {
                    physics.PositionY = FloorY - physics.Radius;
                    physics.VelocityY = -MathF.Abs(physics.VelocityY) * Restitution;
                    if (MathF.Abs(physics.VelocityY) < 10.0f)
                        physics.VelocityY = 0.0f;
                }
                if (physics.PositionY - physics.Radius <= CeilingY)
                {
                    physics.PositionY = CeilingY + physics.Radius;
                    physics.VelocityY = MathF.Abs(physics.VelocityY) * Restitution;
                }
                if (physics.PositionX - physics.Radius <= LeftWall)
                {
                    physics.PositionX = LeftWall + physics.Radius;
                    physics.VelocityX = MathF.Abs(physics.VelocityX) * Restitution;
                }
                if (physics.PositionX + physics.Radius >= RightWall)
                {
                    physics.PositionX = RightWall - physics.Radius;
                    physics.VelocityX = -MathF.Abs(physics.VelocityX) * Restitution;
                }
            }

            position.X = physics.PositionX - physics.Radius;
            position.Y = physics.PositionY - physics.Radius;
            sprite.Width = physics.Radius * 2.0f;
            sprite.Height = physics.Radius * 2.0f;
            sprite.Color = physics.Active != 0
                ? new Color { R = 1.0f, G = 0.3f, B = 0.3f, A = 1.0f }
                : new Color { R = 0.5f, G = 0.5f, B = 0.5f, A = 1.0f };
        }
    }
}

public static class BallTagSystem
{
    [EcsSystem]
    public static void Observe(Query<Read<BallTag>> query)
    {
        // Declaring and iterating this type exercises automatic registration
        // of a component owned entirely by project_cs.
        foreach (var row in query)
            _ = row.Read<BallTag>().Kind;
    }
}

// =============================================================================
// Optional-module bridge demo: reads the `Spline` component the Rust
// `pill_spline` module registered and writes one of its own through Commands.
//
// The C# type is AUTO-GENERATED by the host into generated/<module>_Components.g.cs
// (see project_cs.csproj) from the module's real registered layout - nothing is
// hand-written in this project. It is a typed mirror: the scalar fields
// (`ControlPointCount`, `Elo`) are real C# fields, while the control-point
// array stays a per-element opaque `Vector3f` (glam is not a PillMirror type),
// exposed through a safe `Raw` span. The probe prints how many splines C# can
// see - the module seeds one, this system creates a second - plus the first
// control point of the first spline, proving the raw bytes are read with the
// correct layout (the module's spline starts at (0,0,0)).
// =============================================================================

public static class ModuleSplineBridgeDemo
{
    private static bool _seeded;
    private static long _lastReport;

    // Offsets inside the generated `pill_spline.Spline` ABI blob.
    private const int ControlPointStride = 12; // Vector3f = 3 floats
    private const int ControlPointCountOffset = 16 * ControlPointStride;

    private static void WriteFloat(Span<byte> bytes, int offset, float value) =>
        MemoryMarshal.Write(bytes.Slice(offset), in value);

    [EcsSystem]
    public static void Run(Query<Read<global::pill_spline.Spline>> query, Commands commands)
    {
        if (!_seeded)
        {
            _seeded = true;
            var spline = new global::pill_spline.Spline();
            Span<byte> raw = spline.Raw;
            // Four control points: (10,20,0) (50,80,0) (90,30,0) (130,60,0).
            // The mirror keeps `ControlPoints` opaque (glam::Vec3 is not a
            // PillMirror type), so the floats go through the Raw span...
            WriteFloat(raw, 0 * ControlPointStride, 10.0f);
            WriteFloat(raw, 0 * ControlPointStride + 4, 20.0f);
            WriteFloat(raw, 1 * ControlPointStride, 50.0f);
            WriteFloat(raw, 1 * ControlPointStride + 4, 80.0f);
            WriteFloat(raw, 2 * ControlPointStride, 90.0f);
            WriteFloat(raw, 2 * ControlPointStride + 4, 30.0f);
            WriteFloat(raw, 3 * ControlPointStride, 130.0f);
            WriteFloat(raw, 3 * ControlPointStride + 4, 60.0f);
            // ...but the scalar fields are typed in the generated mirror.
            spline.ControlPointCount = 4;
            spline.Elo = 30.0f;
            commands.CreateEntity().With(spline).Build();
        }

        long now = Stopwatch.GetTimestamp();
        if (Stopwatch.GetElapsedTime(_lastReport, now).TotalMilliseconds < 2000)
            return;
        _lastReport = now;

        int visibleSplines = 0;
        float firstPointX = float.NaN;
        uint firstPointCount = 0;
        foreach (var row in query)
        {
            var mirror = row.Read<global::pill_spline.Spline>();
            Span<byte> bytes = mirror.Raw;
            if (visibleSplines == 0)
            {
                firstPointX = MemoryMarshal.Read<float>(bytes.Slice(0));
                firstPointCount = mirror.ControlPointCount;
            }
            visibleSplines++;
        }

        var omo = new global::pill_spline.OmoMO();
        omo.X = 12;
        omo.Y = 34;
        ulong omoSum = omo.GetSum();
        ulong omoA = omo.GetA();
        int omoB = omo.GetD(2, 4);

        Console.WriteLine(
            $"[project_cs] cs spline bridge: sees {visibleSplines} spline(s), " +
            $"first P0.X={firstPointX}, count={firstPointCount}, omo=({omo.X},{omo.Y}) sum={omoSum} a={omoA} b={omoB}");
    }
}
