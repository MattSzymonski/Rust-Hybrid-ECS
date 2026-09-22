// Managed port of examples/italian_brainrot/src/{lib,scene,systems}.rs.
//
// Responsibilities
// - Loads the bundled OBJ mesh, PNG texture and two custom WGSL shaders from
//   res/, and builds the lit/unlit/cartoon materials from them - the managed
//   twin of the Rust `asset_loading::load`.
// - Creates a camera and the three tagged entities once, the managed twin of
//   `scene::create`.
// - Rotates every tagged entity around its local Y axis at 90 degrees per
//   second, the managed twin of the Rust `rotation_system`.
//
// Design
// Asset loading and scene creation both run from one `[EcsStartup]` method:
// unlike a Rust project's `#[pill_project] fn init`, a managed startup runs
// exactly once for the life of the host rather than on every hot reload, so
// neither needs the "already exists" guards the Rust project or an ordinary
// per-frame `[EcsSystem]` would. The actual asset construction - decoding the
// OBJ, the PNG, and building the shader/material records - happens on the
// Rust side of `Engine.LoadMeshObj`/`LoadTexturePng`/`LoadShader`/`CreateMaterial`;
// this file only supplies the bytes and the same parameter values
// `asset_loading.rs` does.

using System.Numerics;

namespace TracyLive;

// =============================================================================
// Constants
// =============================================================================

/// <summary>Scene constants, the mirror of the Rust project's scene positions.</summary>
internal static class ProjectConstants
{
    /// <summary>Degrees the tagged entities rotate around Y per second.</summary>
    internal const float RotationDegreesPerSecond = 90.0f;

    /// <summary>Spawn positions of the three model variants, in the original scene's order.</summary>
    internal static readonly (float X, float Y, float Z)[] ModelPositions =
    [
        (-1.25f, 0.0f, 0.0f),
        (1.25f, 0.0f, 0.0f),
        (0.0f, 0.0f, 1.5f),
    ];

    /// <summary>Fallback frame delta used only before the clock has a first stamp.</summary>
    internal const float FixedDeltaTime = 1.0f / 60.0f;
}

// =============================================================================
// Simulation time
// =============================================================================

/// <summary>
/// Wall-clock delta between frames. The Rust project reads the engine's own
/// `Res&lt;Time&gt;`, which has no managed equivalent, so this resource stamps
/// its own clock the way project_cs's `SimulationTime` does.
/// </summary>
[EcsResource]
[System.Runtime.InteropServices.StructLayout(System.Runtime.InteropServices.LayoutKind.Sequential)]
public struct SimulationTime
{
    /// <summary>Seconds the previous frame took.</summary>
    public float DeltaSeconds;

    /// <summary>Stopwatch timestamp of the previous stamp, or zero if never.</summary>
    public long LastFrameTimestamp;
}

/// <summary>Stamping helper for <see cref="SimulationTime"/>.</summary>
internal static class SimulationClock
{
    /// <summary>Stamps the time elapsed since the previous frame into the resource.</summary>
    internal static void Stamp(ref SimulationTime time)
    {
        long now = System.Diagnostics.Stopwatch.GetTimestamp();
        time.DeltaSeconds = time.LastFrameTimestamp == 0
            ? ProjectConstants.FixedDeltaTime
            : Math.Clamp(
                (float)System.Diagnostics.Stopwatch.GetElapsedTime(time.LastFrameTimestamp, now).TotalSeconds,
                0.0f,
                0.1f);
        time.LastFrameTimestamp = now;
    }
}

// =============================================================================
// Scene setup
// =============================================================================

/// <summary>Loads the bundled assets and creates the camera and three model entities, once.</summary>
/// <remarks>
/// The managed twin of `asset_loading::load` plus `scene::create` combined:
/// an `[EcsStartup]` method never re-runs, so there is nothing here to guard
/// against a duplicate scene the way a per-frame system would need to.
/// </remarks>
public static class SceneStartup
{
    // Mirrors pill_engine::AssetLoader's own fallback resolution (see
    // resolve_asset_path in pill_engine/src/asset.rs): `PROJECT_PATH` names
    // this project's directory relative to the host's working directory,
    // which is also this hosted process's working directory. Neither
    // AppContext.BaseDirectory nor Assembly.Location works here - the
    // managed runtime is hosted by the Rust process rather than launched as
    // its own executable, and an assembly loaded into a collectible context
    // reports an empty Location - so PROJECT_PATH is the one path component
    // that is always right.
    private static readonly string ResRoot = Path.Combine(
        Environment.CurrentDirectory,
        Environment.GetEnvironmentVariable("PROJECT_PATH") ?? string.Empty,
        "res");

    private static byte[] ReadRes(string relativePath) => File.ReadAllBytes(Path.Combine(ResRoot, relativePath));

    [EcsStartup]
    public static void Start(Commands commands)
    {
        AssetHandle mesh = Engine.LoadMeshObj(
            "italian_brainrot.mesh", ReadRes(Path.Combine("models", "chimpanzini_bananini.obj")));
        AssetHandle color = Engine.LoadTexturePng(
            "italian_brainrot.color", ReadRes(Path.Combine("textures", "chimpanzini_bananini_color.png")));

        string vertexWgsl = System.Text.Encoding.UTF8.GetString(
            ReadRes(Path.Combine("shaders", "default_vertex.wgsl")));
        ShaderTextureBinding[] colorTextureSlot = [new ShaderTextureBinding("color", 0, 1)];

        AssetHandle unlitShader = Engine.LoadShader(
            "italian_brainrot.shader.unlit",
            vertexWgsl,
            System.Text.Encoding.UTF8.GetString(ReadRes(Path.Combine("shaders", "unlit_fragment.wgsl"))),
            [new ShaderParameter("tint", ShaderParameterKind.Color)],
            colorTextureSlot,
            passEngineParameters: true,
            passCameraParameters: true);
        AssetHandle cartoonShader = Engine.LoadShader(
            "italian_brainrot.shader.cartoon",
            vertexWgsl,
            System.Text.Encoding.UTF8.GetString(ReadRes(Path.Combine("shaders", "cartoon_fragment.wgsl"))),
            [new ShaderParameter("posterize_level", ShaderParameterKind.Scalar)],
            colorTextureSlot,
            passEngineParameters: true,
            passCameraParameters: true);

        // An invalid shader handle selects the renderer's built-in lit shader.
        AssetHandle lit = Engine.CreateMaterial(
            "italian_brainrot.material.lit",
            AssetHandle.None,
            [new MaterialTextureBinding("color", color)],
            [new MaterialScalarParameter("specularity", 0.5f)],
            [new MaterialColorParameter("tint", 1.0f, 1.0f, 1.0f)]);
        AssetHandle unlit = Engine.CreateMaterial(
            "italian_brainrot.material.unlit",
            unlitShader,
            [new MaterialTextureBinding("color", color)],
            [],
            [new MaterialColorParameter("tint", 1.0f, 1.0f, 1.0f)]);
        AssetHandle cartoon = Engine.CreateMaterial(
            "italian_brainrot.material.cartoon",
            cartoonShader,
            [new MaterialTextureBinding("color", color)],
            [new MaterialScalarParameter("posterize_level", 3.0f)],
            []);

        commands.CreateEntity()
            .With(new CameraComponent { Enabled = 1, Priority = 0, VerticalFov = 60.0f, Near = 0.1f, Far = 1000.0f })
            .With(new TransformComponent { Z = 5.0f, RotationW = 1.0f, ScaleX = 1.0f, ScaleY = 1.0f, ScaleZ = 1.0f })
            .Build();

        AssetHandle[] materials = [lit, unlit, cartoon];
        for (int i = 0; i < ProjectConstants.ModelPositions.Length; i++)
        {
            var (x, y, z) = ProjectConstants.ModelPositions[i];
            AssetHandle material = materials[i];
            commands.CreateEntity()
                .With(new TransformComponent
                {
                    X = x,
                    Y = y,
                    Z = z,
                    RotationW = 1.0f,
                    ScaleX = 1.0f,
                    ScaleY = 1.0f,
                    ScaleZ = 1.0f,
                })
                .With(new MeshRendererComponent
                {
                    MeshIndex = mesh.Index,
                    MeshGeneration = mesh.Generation,
                    MaterialIndex = material.Index,
                    MaterialGeneration = material.Generation,
                })
                .With(new TagAlpha())
                .Build();
        }
    }
}

// =============================================================================
// Rotation
// =============================================================================

/// <summary>Rotates each tagged entity around its local Y axis at 90 degrees per second.</summary>
public static class RotationSystem
{
    [EcsSystem]
    public static void Run(
        ResMut<SimulationTime> time,
        Query<Write<TransformComponent>, Read<TagAlpha>> models)
    {
        ref SimulationTime simulation = ref time.Value;
        SimulationClock.Stamp(ref simulation);
        float deltaSeconds = simulation.DeltaSeconds * 30.0f;

        float angle = float.DegreesToRadians(ProjectConstants.RotationDegreesPerSecond) * deltaSeconds;
        Quaternion step = Quaternion.CreateFromAxisAngle(Vector3.UnitY, angle);

        foreach (var row in models.Rows())
        {
            ref var transform = ref row.TransformComponent;
            var current = new Quaternion(transform.RotationX, transform.RotationY, transform.RotationZ, transform.RotationW);
            current = current.LengthSquared() > 1.0e-8f ? Quaternion.Normalize(current) : Quaternion.Identity;
            var next = Quaternion.Normalize(step * current);
            transform.RotationX = next.X;
            transform.RotationY = next.Y;
            transform.RotationZ = next.Z;
            transform.RotationW = next.W;
        }
    }
}
