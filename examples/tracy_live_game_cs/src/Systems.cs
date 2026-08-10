namespace TracyLive;

/// <summary>
/// The systems that actually drive the simulation — straight ports of
/// <c>tracy_live_game/src/game.rs</c>'s <c>movement_system</c>/
/// <c>health_decay_system</c>/<c>gravity_system</c>, operating on the same
/// native component storage via the <see cref="Engine"/> facade's generic
/// queries. The first type parameter is writable and the second is read-only.
///
/// <b>This is the file to edit to test hot-reload</b> — change something
/// (e.g. the health-decay increment below, or the gravity formula), save,
/// then run <c>dotnet build examples/tracy_live_game_cs -c Release</c> in
/// another terminal. `TracyLive.Loader.GameHost` picks up the new build
/// within about half a second.
///
/// The query parameter is also the scheduler declaration: its generic types
/// are reflected at load time into Rust read/write access metadata. Native
/// access is authorized only for the duration of this scheduled call.
/// </summary>
public static class MovementSystem
{
    [EcsSystem]
    public static void Run(WriteReadQuery<Position, Velocity> query)
    {
        foreach (var components in query)
        {
            ref var position = ref components.Write;
            ref readonly var velocity = ref components.Read;
            position.X += velocity.X;
            position.Y += velocity.Y;
        }
    }
}

public static class HealthDecaySystem
{
    [EcsSystem]
    public static void Run(WriteQuery<Health> query)
    {
        foreach (var components in query)
        {
            ref var health = ref components.Write;
            health.Value = MathF.Max(health.Value + 10.1f, 0f);
        }
    }
}

/// <summary>
/// Heavy per-entity work — trig, sqrt, mul — mirroring
/// <c>tracy_live_game::game::gravity_system</c>'s inverse-square gravity
/// toward the origin.
/// </summary>
public static class GravitySystem
{
    [EcsSystem]
    public static void Run(WriteReadQuery<GravityForce, Mass> query)
    {
        foreach (var components in query)
        {
            ref var force = ref components.Write;
            ref readonly var mass = ref components.Read;
            float distanceSq = force.X * force.X + MathF.Sqrt(force.Y) * force.Y + 0.01f;
            float distance = MathF.Sqrt(distanceSq);
            float magnitude = mass.Value / (distanceSq * distance); // 1/d^3
            force.X = Math.Clamp(-force.X * MathF.Sqrt(magnitude), -1f, 1f);
            force.Y = Math.Clamp(-force.Y * MathF.Sqrt(magnitude), -1f, 1f);
        }
    }
}
