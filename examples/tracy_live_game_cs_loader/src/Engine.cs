namespace TracyLive;

/// <summary>
/// Friendly, safe-looking facade over the raw <see cref="EngineApi"/> table.
/// This is the *only* place in the whole C# side that touches a raw
/// pointer — every method here returns a bounds-checked <see cref="Span{T}"/>
/// instead of a pointer, so <c>tracy_live_game_cs</c> (the project you
/// actually edit) never needs <c>unsafe</c> at all, and its .csproj leaves
/// <c>AllowUnsafeBlocks</c> off on purpose.
/// </summary>
public static unsafe class Engine
{
    private static EngineApi _api;

    /// <summary>Capture the API table handed to us by the host (copied by value).</summary>
    public static void Bind(EngineApi* api) => _api = *api;

    /// <summary>
    /// Same as <see cref="Bind(EngineApi*)"/>, but takes the pointer as an
    /// <see cref="IntPtr"/> so <c>Interop.Init</c> — which lives in the
    /// unsafe-forbidden reloadable project — never needs an <c>unsafe</c>
    /// block to call it.
    /// </summary>
    public static void Bind(IntPtr api) => Bind((EngineApi*)api);

    public static uint EntityCount() => _api.EntityCount();

    public static Span<Position> Positions()
    {
        Position* ptr;
        uint len;
        _api.GetPositions(&ptr, &len);
        return new Span<Position>(ptr, (int)len);
    }

    public static Span<Velocity> Velocities()
    {
        Velocity* ptr;
        uint len;
        _api.GetVelocities(&ptr, &len);
        return new Span<Velocity>(ptr, (int)len);
    }

    public static Span<Health> Healths()
    {
        Health* ptr;
        uint len;
        _api.GetHealths(&ptr, &len);
        return new Span<Health>(ptr, (int)len);
    }

    public static Span<Mass> Masses()
    {
        Mass* ptr;
        uint len;
        _api.GetMasses(&ptr, &len);
        return new Span<Mass>(ptr, (int)len);
    }

    public static Span<GravityForce> GravityForces()
    {
        GravityForce* ptr;
        uint len;
        _api.GetGravityForces(&ptr, &len);
        return new Span<GravityForce>(ptr, (int)len);
    }
}
