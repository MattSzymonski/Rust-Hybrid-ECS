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
// per-frame `[EcsSystem]` would. The asset loading reads like the Rust
// `asset_loading.rs` line for line: the renderer data crate mirrors its asset
// types, builders and resource to C#, so the same calls build the same
// meshes, textures, shaders and materials, in the renderer's own Rust code.

using System.Numerics;
// The module's mirrored free function and value type, generated from its Rust
// declarations.
using pill_dummy_color;
// The renderer's components, generated from their Rust registration.
using pill_master_renderer.component;
// The renderer's assets, builders and pipeline resource, generated from their
// Rust declarations.
using pill_master_renderer_data;

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
    [EcsStartup]
    public static void Start(
        Commands commands,
        ResMut<AssetManager> assets,
        ResMut<RenderingManager> rendering)
    {
        // The showcase is the three shading styles, one material each, so this
        // project runs the renderer's built-in frame: a single geometry pass
        // that draws every instance through its own material's shader. The
        // renderer registers the PBR chain as the manager's default - that
        // chain draws materials built for its `pill_pbr` shader alone - so
        // clearing the manager returns the renderer to the built-in pass the
        // styles need.
        rendering.Clear();

        // The OBJ decode lives in the renderer; the loader supplies only the bytes.
        byte[] obj = AssetLoader.Path("models/chimpanzini_bananini.obj").Load();
        Handle<Mesh> mesh = assets.AddNamed(
            "italian_brainrot.mesh",
            Mesh.FromObjBytes("chimpanzini_bananini", obj));
        Handle<Texture> color = assets.AddNamed(
            "italian_brainrot.color",
            Texture.New(
                "chimpanzini_bananini",
                TextureType.Color,
                AssetLoader.Path("textures/chimpanzini_bananini_color.png")));
        Handle<Shader> unlitShader = assets.AddNamed(
            "italian_brainrot.shader.unlit",
            Shader.New("italian_brainrot_unlit")
                .WithVertexSource(AssetLoader.Path("shaders/default_vertex.wgsl"))
                .WithFragmentSource(AssetLoader.Path("shaders/unlit_fragment.wgsl"))
                .WithParameterSlots([new ShaderParameterSlot("tint", ShaderParameterType.Color)])
                .WithTextureSlots([new ShaderTextureSlot("color", TextureType.Color, (0, 1))])
                .WithEngineParameters(true)
                .WithCameraParameters(true)
                .Build());
        Handle<Shader> cartoonShader = assets.AddNamed(
            "italian_brainrot.shader.cartoon",
            Shader.New("cartoon")
                .WithVertexSource(AssetLoader.Path("shaders/default_vertex.wgsl"))
                .WithFragmentSource(AssetLoader.Path("shaders/cartoon_fragment.wgsl"))
                .WithParameterSlots([new ShaderParameterSlot("posterize_level", ShaderParameterType.Scalar)])
                .WithTextureSlots([new ShaderTextureSlot("color", TextureType.Color, (0, 1))])
                .WithEngineParameters(true)
                .WithCameraParameters(true)
                .Build());

        // A material with no shader set keeps the renderer's built-in lit shader.
        Handle<Material> lit = assets.AddNamed(
            "italian_brainrot.material.lit",
            Material.Builder("chimpanzini_bananini_lit")
                .Texture("color", color)
                .ColorParameter("tint", Vector3.One)
                .ScalarParameter("specularity", 0.5f)
                .Build());
        Handle<Material> unlit = assets.AddNamed(
            "italian_brainrot.material.unlit",
            Material.Builder("chimpanzini_bananini_unlit")
                .Shader(unlitShader)
                .Texture("color", color)
                .ColorParameter("tint", Vector3.One)
                .Build());
        Handle<Material> cartoon = assets.AddNamed(
            "italian_brainrot.material.cartoon",
            Material.Builder("chimpanzini_bananini_cartoon")
                .Shader(cartoonShader)
                .Texture("color", color)
                .ScalarParameter("posterize_level", 3.0f)
                .Build());

        commands.CreateEntity()
            .With(CameraComponent.Perspective(60.0f, 0.1f, 1000.0f))
            .With(TransformComponent.At(0.0f, 0.0f, 5.0f, 1.0f))
            .Build();

        Handle<Material>[] materials = [lit, unlit, cartoon];
        for (int i = 0; i < ProjectConstants.ModelPositions.Length; i++)
        {
            var (x, y, z) = ProjectConstants.ModelPositions[i];
            commands.CreateEntity()
                .With(TransformComponent.At(x, y, z, 1.0f))
                .With(MeshRendererComponent.From(mesh, materials[i]))
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
    // PILL0301 warns that mutable statics in a system-declaring type are what a
    // parallel batch races on. Only this one system touches the field, and the
    // reset it takes on a reload is what makes the next value print again.
#pragma warning disable PILL0301
    private static float _reportedAlpha = float.NaN;
#pragma warning restore PILL0301

    [EcsSystem]
    public static void Run(
        ResMut<SimulationTime> time,
        Query<Write<TransformComponent>, Read<TagAlpha>> models)
    {
        ref SimulationTime simulation = ref time.Value;
        SimulationClock.Stamp(ref simulation);
        float deltaSeconds = simulation.DeltaSeconds * 30.0f;

        var color = pill_dummy_color.PillDummyColor.GetColorA();
        float angle = float.DegreesToRadians(ProjectConstants.RotationDegreesPerSecond - 100.0f + color) * deltaSeconds;
        Quaternion step = Quaternion.CreateFromAxisAngle(Vector3.UnitY, angle);

        foreach (var row in models.Rows())
        {
            ref var transform = ref row.TransformComponent;
            // The same normalize-or-identity rule the Rust side shares: a
            // non-finite or near-zero quaternion reads as identity, so a
            // broken component never spreads NaN through the transform chain.
            var current = RotationOrIdentity(transform.Rotation);
            transform.Rotation = Quaternion.Normalize(step * current);
        }
    }

    /// <summary>
    /// Normalize a rotation, or read a non-finite or near-zero one as identity.
    /// </summary>
    private static Quaternion RotationOrIdentity(Quaternion rotation)
    {
        float lengthSquared = rotation.LengthSquared();
        return float.IsFinite(lengthSquared) && lengthSquared > 1.0e-8f
            ? Quaternion.Normalize(rotation)
            : Quaternion.Identity;
    }
}
