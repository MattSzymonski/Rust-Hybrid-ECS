namespace TracyLive;

/// <summary>
/// The systems that actually drive the simulation — straight ports of
/// <c>tracy_live_game/src/game.rs</c>'s <c>movement_system</c>/
/// <c>health_decay_system</c>/<c>gravity_system</c>, operating on the same
/// native component storage via the <see cref="Engine"/> facade's
/// <see cref="Span{T}"/> accessors instead of Rust's <c>Query&lt;T&gt;</c>.
///
/// <b>This is the file to edit to test hot-reload</b> — change something
/// (e.g. the health-decay increment below, or the gravity formula), save,
/// then run <c>dotnet build examples/tracy_live_game_cs -c Release</c> in
/// another terminal. `TracyLive.Loader.GameHost` picks up the new build
/// within about half a second.
///
/// Every method here must re-fetch its spans each call rather than caching
/// them across frames — the same rule Rust's own `SystemParam` doc comment
/// states ("must not escape the system function"), just enforced by
/// convention here instead of the type system.
/// </summary>
public static class MovementSystem
{
    public static void Run()
    {
        var positions = Engine.Positions();
        var velocities = Engine.Velocities();
        for (int i = 0; i < positions.Length; i++)
        {
            positions[i].X += velocities[i].X;
            positions[i].Y += velocities[i].Y;
        }
    }
}

public static class HealthDecaySystem
{
    public static void Run()
    {
        var healths = Engine.Healths();
        for (int i = 0; i < healths.Length; i++)
        {
            healths[i].Value = MathF.Max(healths[i].Value + 0.1f, 0f);
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
    public static void Run()
    {
        var forces = Engine.GravityForces();
        var masses = Engine.Masses();
        for (int i = 0; i < forces.Length; i++)
        {
            float distanceSq = forces[i].X * forces[i].X + MathF.Sqrt(forces[i].Y) * forces[i].Y + 0.01f;
            float distance = MathF.Sqrt(distanceSq);
            float magnitude = masses[i].Value / (distanceSq * distance); // 1/d^3
            forces[i].X = Math.Clamp(-forces[i].X * MathF.Sqrt(magnitude), -1f, 1f);
            forces[i].Y = Math.Clamp(-forces[i].Y * MathF.Sqrt(magnitude), -1f, 1f);
        }
    }
}
