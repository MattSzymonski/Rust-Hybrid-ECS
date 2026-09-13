// Managed mirror of examples/project_rs/src/lib.rs.
//
// Responsibilities
// - Runs the ball physics: five balls bouncing inside a fixed box.
// - Drives the project's single spline from the ball centres and keeps one
//   sample dot on the curve every SplineSampleStep of `t`.
// - Fills the world with those entities, up to a target count, the way the
//   Rust init does.
//
// Design
// The scene, the constants and the per-frame math are the Rust project's, so
// both projects run the same demo against the same world. Four things cannot be
// expressed the same way on the managed side, and each is named where it
// matters:
// 1. Managed systems have no resources. `SimulationTime` is a static, and the
//    ball system stamps it before reading it - the counterpart of the Rust
//    `update_time_system` writing the resource every other system reads.
// 2. A managed system declares ONE query, and no entity handle travels between
//    queries. The Rust `spline_path_system` reads the ball centres and writes
//    the spline and its dots in a single pass; here that pass is the snapshot,
//    the control-point writer and the sampler, sharing `SplinePath`. The curve
//    math itself is not duplicated: the sampler calls the module's mirrored
//    `get_location_at` getters on the spline row.
// 3. A startup cannot query, so filling the world up to a target count - which
//    the Rust init checks by counting entities - is done by the spawn systems:
//    they run every frame and create only what is missing, which is also what
//    keeps a hot reload from duplicating the scene.
// 4. Registration needs no code here: the loader discovers the attributed
//    systems and registers the project's components from the managed manifest,
//    where the Rust init calls `register_system` and `register_component`
//    itself.

using System.Diagnostics;
using System.Runtime.InteropServices;

using static TracyLive.ProjectConstants;
using Spline = pill_spline.Spline;

namespace TracyLive;

// =============================================================================
// Constants
// =============================================================================

/// <summary>Scene constants, the mirror of the Rust project's constants block.</summary>
internal static class ProjectConstants
{
    internal const float FixedDeltaTime = 1.0f / 60.0f;
    internal const float Gravity = 800.0f;
    internal const float BounceVelocityY = -800.0f;
    internal const float BounceVelocityX = 350.0f;
    internal const float Restitution = 0.7f;

    /// <summary>Upward speed restored when a floor bounce would decay to rest.</summary>
    internal const float MinimumBounceVelocityY = 500.0f;

    internal const float FloorY = 580.0f;
    internal const float CeilingY = 20.0f;
    internal const float LeftWall = 20.0f;
    internal const float RightWall = 780.0f;

    /// <summary>Number of balls, and with it the number of spline control points.</summary>
    internal const int BallCount = 5;

    /// <summary>Curve parameter between two neighbouring sample dots.</summary>
    internal const float SplineSampleStep = 0.05f;

    /// <summary>Sample dots on the curve: one every step, both endpoints included.</summary>
    internal const int SplineSampleCount = 21;

    /// <summary>Edge length of a sample dot, in pixels.</summary>
    internal const float SplineSampleDotSize = 6.0f;

    /// <summary>Fill colour of the ball sprites.</summary>
    internal static readonly Color BallColor = new() { R = 1.0f, G = 0.3f, B = 0.3f, A = 1.0f };

    /// <summary>Fill colour of the sample dots.</summary>
    internal static readonly Color SampleDotColor =
        new() { R = 0.25f, G = 0.85f, B = 1.0f, A = 1.0f };
}

// =============================================================================
// Simulation time
// =============================================================================

/// <summary>
/// Wall-clock delta between frames, standing in for the Rust `SimulationTime`
/// resource that managed systems do not have.
/// </summary>
internal static class SimulationTime
{
    private static long _lastFrame = Stopwatch.GetTimestamp();

    /// <summary>Seconds the previous frame took, starting at the fixed delta.</summary>
    internal static float DeltaSeconds { get; private set; } = FixedDeltaTime;

    /// <summary>
    /// Stamps the time elapsed since the previous frame.
    /// </summary>
    /// <remarks>
    /// Clamped because a breakpoint or a slow reload can stretch a single frame
    /// far enough to throw a ball straight through a wall.
    /// </remarks>
    internal static void Stamp()
    {
        long now = Stopwatch.GetTimestamp();
        DeltaSeconds = Math.Clamp(
            (float)Stopwatch.GetElapsedTime(_lastFrame, now).TotalSeconds,
            0.0f,
            0.1f);
        _lastFrame = now;
    }
}

// =============================================================================
// Ball physics
// =============================================================================

/// <summary>Steps every ball by the frame delta and copies its state into its sprite.</summary>
public static class BallPhysicsSystem
{
    /// <summary>Advances one ball by its own delta and bounces it off the box.</summary>
    public static void Simulate(ref PhysicsState state)
    {
        if (state.Active == 0)
            return;

        float delta = Math.Clamp(state.DeltaTime, 0.0f, 0.1f);
        state.VelocityY += Gravity * delta;
        state.PositionX += state.VelocityX * delta;
        state.PositionY += state.VelocityY * delta;

        if (state.PositionY + state.Radius >= FloorY)
        {
            state.PositionY = FloorY - state.Radius;
            state.VelocityY = -MathF.Abs(state.VelocityY) * Restitution;
            // Restitution alone decays each bounce towards rest; restore a
            // minimum upward speed so balls bounce forever.
            if (MathF.Abs(state.VelocityY) < MinimumBounceVelocityY)
                state.VelocityY = -MinimumBounceVelocityY;
        }
        if (state.PositionY - state.Radius <= CeilingY)
        {
            state.PositionY = CeilingY + state.Radius;
            state.VelocityY = MathF.Abs(state.VelocityY) * Restitution;
        }
        if (state.PositionX - state.Radius <= LeftWall)
        {
            state.PositionX = LeftWall + state.Radius;
            state.VelocityX = MathF.Abs(state.VelocityX) * Restitution;
        }
        if (state.PositionX + state.Radius >= RightWall)
        {
            state.PositionX = RightWall - state.Radius;
            state.VelocityX = -MathF.Abs(state.VelocityX) * Restitution;
        }
    }

    [EcsSystem]
    public static void Run(Query<Write<PhysicsState>, Write<Position>, Write<Sprite>> query)
    {
        SimulationTime.Stamp();
        float deltaSeconds = SimulationTime.DeltaSeconds;

        foreach (var row in query.Rows())
        {
            ref var physics = ref row.PhysicsState;
            ref var position = ref row.Position;
            ref var sprite = ref row.Sprite;

            physics.DeltaTime = deltaSeconds;
            Simulate(ref physics);

            // Physics coordinates describe the centre of the ball; the sprite
            // renderer expects the top-left corner of the quad.
            position.X = physics.PositionX - physics.Radius;
            position.Y = physics.PositionY - physics.Radius;
            sprite.Width = physics.Radius * 2.0f;
            sprite.Height = physics.Radius * 2.0f;
        }
    }
}

// =============================================================================
// Spline path
// =============================================================================

/// <summary>
/// The curve the sample dots are placed on, shared by the systems that build it.
/// </summary>
/// <remarks>
/// The Rust project samples the spline component itself, through the module's
/// `Spline::get_location_at`; the mirror exposes that same math as the
/// primitive getters `GetLocationX`/`GetLocationY`, so the curve is never
/// reimplemented on this side. The arrays exist because a managed system
/// declares a single query: the ball centres travel through
/// <see cref="ControlPoints"/> from the ball system to the writer, and the
/// sampled grid through <see cref="SamplePositions"/> from the writer to the
/// dots. The seed spline is built from the balls' spawn positions for the
/// dots' first frame.
/// </remarks>
internal static class SplinePath
{
    /// <summary>Bytes one control point occupies in the module's ABI blob.</summary>
    internal const int ControlPointStride = 12;

    /// <summary>The module's `MAX_CONTROL_POINTS`, the length of its array.</summary>
    private const int MaxControlPoints = 16;

    /// <summary>Ball centres for this frame, three floats per point.</summary>
    internal static readonly float[] ControlPoints = new float[MaxControlPoints * 3];

    /// <summary>How many leading <see cref="ControlPoints"/> the balls filled in.</summary>
    internal static int ControlPointCount;

    /// <summary>Sampled curve positions, an x/y pair per dot index.</summary>
    internal static readonly float[] SamplePositions = new float[SplineSampleCount * 2];

    /// <summary>Writes one control point into a spline row's ABI bytes.</summary>
    internal static void WriteControlPoint(Span<byte> bytes, int index, float x, float y)
    {
        int offset = index * ControlPointStride;
        MemoryMarshal.Write(bytes.Slice(offset), in x);
        MemoryMarshal.Write(bytes.Slice(offset + 4), in y);
        // The z axis stays zero: the whole scene lives in the z = 0 plane.
        MemoryMarshal.Write(bytes.Slice(offset + 8), 0.0f);
    }

    /// <summary>
    /// A spline through the balls' spawn positions, for seeding the dots
    /// before the first snapshot writes the live curve.
    /// </summary>
    internal static Spline CreateSeedSpline()
    {
        var spline = new Spline();
        Span<byte> bytes = spline.Raw;
        for (int index = 0; index < BallCount; index++)
        {
            PhysicsState ball = WorldSetup.BallSpawnState(index);
            WriteControlPoint(bytes, index, ball.PositionX, ball.PositionY);
        }
        spline.ControlPointCount = BallCount;
        // The module's `from_points` builds on `Spline::default()`, so a
        // freshly seeded spline carries that default rather than zero.
        spline.Elo = 30.0f;
        return spline;
    }

}

/// <summary>Copies the ball centres into <see cref="SplinePath"/> for this frame.</summary>
/// <remarks>
/// Query iteration walks the ball archetype row by row, and the balls are
/// created in index order, so the i-th centre seen belongs to the i-th ball -
/// the ordering assumption the Rust system makes as well.
/// </remarks>
public static class BallSnapshotSystem
{
    [EcsSystem]
    public static void Run(Query<Read<PhysicsState>> query)
    {
        int count = 0;
        foreach (var row in query.Rows())
        {
            if (count == BallCount)
                break;

            ref readonly var ball = ref row.PhysicsState;
            SplinePath.ControlPoints[count * 3] = ball.PositionX;
            SplinePath.ControlPoints[count * 3 + 1] = ball.PositionY;
            SplinePath.ControlPoints[count * 3 + 2] = 0.0f;
            count++;
        }

        SplinePath.ControlPointCount = count;
    }
}

/// <summary>Publishes the snapshot as the spline's control points.</summary>
/// <remarks>
/// The control point array lives behind the generated mirror's ABI blob:
/// `glam::Vec3` is not a mirrorable type, so the floats go through its `Raw`
/// span at the offsets the module registered. The spline keeps its own copy of
/// the centres, so the balls need no relationship to it and stay free to move.
/// The same pass samples the updated curve through the module's mirrored
/// getters, filling the grid the dots read.
/// </remarks>
public static class SplinePathSystem
{
    [EcsSystem]
    public static void Run(Query<Write<Spline>> query)
    {
        int count = SplinePath.ControlPointCount;
        if (count == 0)
            return;

        foreach (var row in query.Rows())
        {
            ref var spline = ref row.Spline;
            Span<byte> bytes = spline.Raw;
            for (int index = 0; index < count; index++)
            {
                SplinePath.WriteControlPoint(
                    bytes,
                    index,
                    SplinePath.ControlPoints[index * 3],
                    SplinePath.ControlPoints[index * 3 + 1]);
            }
            spline.ControlPointCount = (uint)count;

            // The curve is current: ask the module for the sample grid the
            // dots read, one dot index at a time.
            for (int index = 0; index < SplineSampleCount; index++)
            {
                float t = index * SplineSampleStep;
                SplinePath.SamplePositions[index * 2] = spline.GetLocationX(t);
                SplinePath.SamplePositions[index * 2 + 1] = spline.GetLocationY(t);
            }
        }
    }
}

/// <summary>Walks the sample dots along the curve the ball centres describe.</summary>
public static class SplineSampleSystem
{
    [EcsSystem]
    public static void Run(Query<Write<SplineSample>, Write<Position>> query)
    {
        // Before the first snapshot there is no curve to sample; the dots keep
        // the positions the spawn system seeded them with.
        if (SplinePath.ControlPointCount == 0)
            return;

        foreach (var row in query.Rows())
        {
            ref var sample = ref row.SplineSample;
            ref var position = ref row.Position;
            // The dots sit on the fixed sample grid, so `t` identifies which
            // grid entry holds their position.
            int index = Math.Clamp(
                (int)MathF.Round(sample.T / SplineSampleStep), 0, SplineSampleCount - 1);

            // Samples are curve points and the dot is centred on them; sprites
            // draw from the top-left corner of their quad.
            position.X = SplinePath.SamplePositions[index * 2] - SplineSampleDotSize * 0.5f;
            position.Y = SplinePath.SamplePositions[index * 2 + 1] - SplineSampleDotSize * 0.5f;
        }
    }
}

// =============================================================================
// World setup
// =============================================================================

/// <summary>Spawn layout, shared by the systems that fill the world.</summary>
internal static class WorldSetup
{
    /// <summary>Physics state for the <paramref name="index"/>-th ball in the spawn sequence.</summary>
    /// <remarks>
    /// Balls line up across the play area with alternating travel direction and
    /// a slightly different launch speed each, so they spread out over the box
    /// instead of crossing it as one block.
    /// </remarks>
    internal static PhysicsState BallSpawnState(int index) => new()
    {
        DeltaTime = FixedDeltaTime,
        PositionX = 90.0f + index * 150.0f,
        PositionY = 120.0f,
        VelocityX = index % 2 == 0 ? BounceVelocityX : -BounceVelocityX,
        VelocityY = BounceVelocityY - index * 25.0f,
        Radius = 10.0f + index % 4 * 2.0f,
        Active = 1,
    };
}

/// <summary>Creates the balls that are missing, up to <see cref="BallCount"/>.</summary>
/// <remarks>
/// The Rust init fills the world up to a target count instead of spawning a
/// fresh set on every rebuild, because hot reload preserves the entities that
/// already exist. A managed startup cannot query, so that fill runs here: once
/// per frame, creating only what the world is missing.
/// </remarks>
public static class BallSpawnSystem
{
    [EcsSystem]
    public static void Run(Query<Read<PhysicsState>> query, Commands commands)
    {
        int existing = 0;
        foreach (var row in query.Rows())
        {
            _ = row.PhysicsState;
            existing++;
        }

        for (int index = existing; index < BallCount; index++)
        {
            PhysicsState ball = WorldSetup.BallSpawnState(index);
            commands.CreateEntity()
                .With(ball)
                .With(new Position
                {
                    X = ball.PositionX - ball.Radius,
                    Y = ball.PositionY - ball.Radius,
                })
                .With(new Sprite
                {
                    Width = ball.Radius * 2.0f,
                    Height = ball.Radius * 2.0f,
                    Color = BallColor,
                })
                .Build();
        }
    }
}

/// <summary>Creates the project's spline when the world has none.</summary>
/// <remarks>
/// Seeded with the balls' spawn positions, so even the first frame draws a
/// curve through them instead of every sample dot stacked on the origin. In a
/// managed run the world usually already has a spline: `Spline` resolves to
/// the component type the `pill_spline` module registered, and the module
/// seeds a demo path of its own on load.
/// </remarks>
public static class SplineSpawnSystem
{
    [EcsSystem]
    public static void Run(Query<Read<Spline>> query, Commands commands)
    {
        foreach (var row in query.Rows())
        {
            _ = row.Spline;
            return;
        }

        commands.CreateEntity().With(SplinePath.CreateSeedSpline()).Build();
    }
}

/// <summary>Creates the sample dots that are missing, up to the target count.</summary>
/// <remarks>
/// Each new dot is seeded on the spawn curve; from the next frame on, the
/// sample system places it on the live one. Only the position is ever
/// rewritten, so the inspector can retune the dots' size and colour while the
/// scene runs.
/// </remarks>
public static class SplineSampleSpawnSystem
{
    [EcsSystem]
    public static void Run(Query<Read<SplineSample>> query, Commands commands)
    {
        int existing = 0;
        foreach (var row in query.Rows())
        {
            _ = row.SplineSample;
            existing++;
        }
        if (existing >= SplineSampleCount)
            return;

        // Seeded on the spawn curve through the module's math; from the next
        // frame on, the sample system places the dots on the live one.
        Spline seed = SplinePath.CreateSeedSpline();
        for (int index = existing; index < SplineSampleCount; index++)
        {
            float t = index * SplineSampleStep;
            float x = seed.GetLocationX(t);
            float y = seed.GetLocationY(t);
            commands.CreateEntity()
                .With(new SplineSample { T = t })
                .With(new Position
                {
                    X = x - SplineSampleDotSize * 0.5f,
                    Y = y - SplineSampleDotSize * 0.5f,
                })
                .With(new Sprite
                {
                    Width = SplineSampleDotSize,
                    Height = SplineSampleDotSize,
                    Color = SampleDotColor,
                })
                .Build();
        }
    }
}
