// The demo's level, its scattered pills and its live tuning keys.
//
// Managed counterpart of
// examples/pills_awakens/pills_awakens_rust/src/game.rs's remaining half:
// `create_scene`'s camera, pillars, ground and pills become the startup below,
// and `demo_control_system` becomes the system of the same name. Unlike the
// Rust project's `#[pill_project] fn init`, which runs again on every hot
// reload, a managed startup runs exactly once for the life of the host, so
// nothing here needs the "already built" guards the Rust version keeps.
//
// Asset loading goes through the asset store the renderer data crate mirrors
// to C#: `assets.Import<Mesh>`/`<Texture>` for the meshes and maps (whose
// `.meta` files this project ships, so they import under their own guids),
// and `assets.ImportStandalone<Material>` for the `.material` files, which
// are standalone assets with no `.meta` of their own. A startup declares the
// resources it needs as parameters, like every system in this project.

using System.Numerics;
// The renderer's components, generated from their Rust registration.
using pill_master_renderer.component;
// The renderer's assets, builders and pipeline resource, generated from their
// Rust declarations.
using pill_master_renderer_data;
// The PBR chain's mirrored functions (`PbrPipeline.SetSkybox`).
using pill_master_renderer_data.config;
// The ECS vocabulary (`Query`, `Commands`, `Res`) and `Handle`.
using TracyLive;

namespace PillsAwakens;

/// <summary>The level's constants: what to load, and what to scatter where.</summary>
internal static class ProjectConstants
{
    /// <summary>How many pills drift through the field.</summary>
    internal const int PillCount = 200_000;

    /// <summary>Uniform scale of every pill.</summary>
    internal const float PillScale = 0.35f;

    /// <summary>Where the camera starts: at eye height, ten units in front of the origin.</summary>
    internal static readonly Vector3 CameraStart = new(0.0f, 1.6f, -10.0f);

    /// <summary>
    /// The camera starts turned around to face the origin, because a camera
    /// looks down its local -Z axis.
    /// </summary>
    internal const float CameraStartYawDegrees = 180.0f;

    /// <summary>Meshes, relative to `res`. Each path is also the asset's name.</summary>
    internal const string PillMesh = "models/pill.obj";
    internal const string PillarsMesh = "models/pillars.obj";
    internal const string GroundMesh = "models/ground.obj";

    /// <summary>Material files, relative to `res`.</summary>
    internal const string PillarsMaterial = "materials/pillars.material";
    internal const string GroundMaterial = "materials/ground.material";
    internal const string PillMaterial = "materials/dark.material";
    internal const string SkyMaterial = "materials/sky.material";
}

/// <summary>Loads the level's assets and builds the scene, once.</summary>
public static class SceneStartup
{
    [EcsStartup]
    public static void Start(
        Commands commands,
        ResMut<AssetManager> assets)
    {
        // The renderer's data crate installed the PBR chain and its skybox
        // shaders when the host loaded it, and every material file below names
        // that shader by guid; this project's job is the assets themselves.
        Handle<Mesh> pillMesh = assets.Import<Mesh>(
            ProjectConstants.PillMesh, MetadataPolicy.CreateIfMissing).Handle;
        Handle<Mesh> pillarsMesh = assets.Import<Mesh>(
            ProjectConstants.PillarsMesh, MetadataPolicy.CreateIfMissing).Handle;
        Handle<Mesh> groundMesh = assets.Import<Mesh>(
            ProjectConstants.GroundMesh, MetadataPolicy.CreateIfMissing).Handle;

        // The materials name their maps by guid, so the maps load first. A map
        // with no `.meta` file yet is read with the type named here and gets
        // one written beside it, exactly as the Rust project's imports do.
        ImportTexture(assets, "textures/grid.png", "Color");
        ImportTexture(assets, "textures/pillars/color.png", "Color");
        ImportTexture(assets, "textures/pillars/normal.png", "Normal");
        ImportTexture(assets, "textures/pillars/roughness.png", "Color");
        ImportTexture(assets, "textures/ground/color.jpg", "Color");
        ImportTexture(assets, "textures/ground/normal.jpg", "Normal");
        ImportTexture(assets, "textures/ground/roughness.jpg", "Color");
        // A Radiance panorama, read as linear half floats for the skybox.
        ImportTexture(assets, "textures/clear_night.hdr", "Equirect");

        Handle<Material> pillarsMaterial = assets.ImportStandalone<Material>(
            ProjectConstants.PillarsMaterial).Handle;
        Handle<Material> groundMaterial = assets.ImportStandalone<Material>(
            ProjectConstants.GroundMaterial).Handle;
        Handle<Material> pillMaterial = assets.ImportStandalone<Material>(
            ProjectConstants.PillMaterial).Handle;
        Handle<Material> skyMaterial = assets.ImportStandalone<Material>(
            ProjectConstants.SkyMaterial).Handle;

        // The sky draws behind everything the lit pass drew.
        PbrPipeline.SetSkybox(assets, skyMaterial);

        SpawnCamera(commands);
        SpawnLevel(commands, pillarsMesh, pillarsMaterial, groundMesh, groundMaterial);
        SpawnPills(commands, pillMesh, pillMaterial, ProjectConstants.PillCount);
    }

    /// <summary>Imports one map with the texture type to read it as.</summary>
    private static void ImportTexture(ResMut<AssetManager> assets, string path, string textureType) =>
        assets.Import<Texture>(
            path,
            MetadataPolicy.CreateIfMissing,
            $"{{\"texture_type\":\"{textureType}\"}}");

    /// <summary>Spawns the free flying camera.</summary>
    private static void SpawnCamera(Commands commands)
    {
        Vector3 startRotation = new(0.0f, ProjectConstants.CameraStartYawDegrees, 0.0f);
        var transform = TransformComponent.At(
            ProjectConstants.CameraStart.X,
            ProjectConstants.CameraStart.Y,
            ProjectConstants.CameraStart.Z,
            1.0f);
        transform.Rotation = CameraMath.ViewRotation(startRotation);
        commands.CreateEntity()
            .With(transform)
            .With(CameraComponent.Perspective(75.0f, 0.1f, 1000.0f))
            .With(new CameraMovementComponent
            {
                MoveSpeed = 25.0f,
                SprintMultiplier = 2.0f,
                LerpSpeed = 8.0f,
                MouseSensitivity = 0.1f,
                RotationLerpSpeed = 25.0f,
                CurrentRotation = startRotation,
                TargetRotation = startRotation,
            })
            .Build();
    }

    /// <summary>Spawns the pillars and the ground, both at the origin.</summary>
    private static void SpawnLevel(
        Commands commands,
        Handle<Mesh> pillarsMesh,
        Handle<Material> pillarsMaterial,
        Handle<Mesh> groundMesh,
        Handle<Material> groundMaterial)
    {
        (Handle<Mesh> Mesh, Handle<Material> Material)[] pieces =
        [
            (pillarsMesh, pillarsMaterial),
            (groundMesh, groundMaterial),
        ];
        foreach ((Handle<Mesh> mesh, Handle<Material> material) in pieces)
        {
            commands.CreateEntity()
                .With(TransformComponent.Identity)
                .With(MeshRendererComponent.From(mesh, material))
                .Build();
        }
    }

    /// <summary>Scatters `count` pills above the ground, each turned and pushed at random.</summary>
    private static void SpawnPills(
        Commands commands, Handle<Mesh> mesh, Handle<Material> material, int count)
    {
        var random = SmallRandom.FromSeed(0x00C1_4C05);
        MeshRendererComponent renderer = MeshRendererComponent.From(mesh, material);

        for (int index = 0; index < count; index++)
        {
            Quaternion rotation =
                Quaternion.CreateFromAxisAngle(Vector3.UnitX, random.Range(0.0f, 2.0f * MathF.PI)) *
                Quaternion.CreateFromAxisAngle(Vector3.UnitY, random.Range(0.0f, 2.0f * MathF.PI)) *
                Quaternion.CreateFromAxisAngle(Vector3.UnitZ, random.Range(0.0f, 2.0f * MathF.PI));

            var transform = TransformComponent.At(
                random.Range(-20.0f, 20.0f),
                random.Range(0.0f, 40.0f),
                random.Range(-20.0f, 20.0f),
                ProjectConstants.PillScale);
            transform.Rotation = rotation;

            commands.CreateEntity()
                .With(new CurlNoiseComponent
                {
                    Velocity = Vector3.Zero,
                    CurlStrength = random.Range(70.0f, 120.0f),
                    NoiseScale = random.Range(0.1f, 0.2f),
                })
                .With(transform)
                .With(renderer)
                .Build();
        }
    }
}

/// <summary>Tunes the curl noise field while its keys are held.</summary>
public static class DemoControlSystem
{
    [EcsSystem]
    public static void Run(ResMut<DemoState> demoState)
    {
        ref DemoState demo = ref demoState.Value;
        bool changed = false;
        foreach ((KeyCode key, TuningField field, float step) in TuningKeys.Entries)
        {
            if (!Engine.Input.KeyHeld(key))
                continue;
            switch (field)
            {
                case TuningField.Scale:
                    demo.CurlScale += step;
                    break;
                case TuningField.Attraction:
                    demo.CurlAttraction += step;
                    break;
                case TuningField.Damping:
                    demo.CurlDamping += step;
                    break;
                default:
                    demo.CurlEpsilon += step;
                    break;
            }
            changed = true;
        }

        if (changed)
        {
            Log.Info(
                $"[pills_awakens] curl scale: {demo.CurlScale:F4}, " +
                $"attraction: {demo.CurlAttraction:F2}, " +
                $"damping: {demo.CurlDamping:F4}, " +
                $"epsilon: {demo.CurlEpsilon:F4}");
        }
    }
}

/// <summary>A <see cref="DemoState"/> field a tuning key changes.</summary>
internal enum TuningField
{
    Scale,
    Attraction,
    Damping,
    Epsilon,
}

/// <summary>
/// The keys that tune the field, apart from the system that reads them: a
/// static array in a system-declaring type is the state PILL0301 flags.
/// </summary>
internal static class TuningKeys
{
    /// <summary>The keys, as (key, field it changes, step per frame).</summary>
    internal static readonly (KeyCode Key, TuningField Field, float Step)[] Entries =
    [
        (KeyCode.KeyO, TuningField.Scale, 0.0005f),
        (KeyCode.KeyP, TuningField.Scale, -0.0005f),
        (KeyCode.KeyL, TuningField.Attraction, 5.0f),
        (KeyCode.KeyK, TuningField.Attraction, -5.0f),
        (KeyCode.KeyM, TuningField.Damping, 0.01f),
        (KeyCode.KeyN, TuningField.Damping, -0.01f),
        (KeyCode.KeyV, TuningField.Epsilon, 0.001f),
        (KeyCode.KeyB, TuningField.Epsilon, -0.001f),
    ];
}

/// <summary>
/// A small deterministic random number generator (SplitMix64).
/// </summary>
/// <remarks>
/// The Rust project uses this instead of a thread-local generator: a
/// thread-local value owned by a project DLL would register a destructor in
/// that DLL, which a hot reload unmaps while the thread lives on. The port
/// keeps the same generator so the scene scatters the same way.
/// </remarks>
internal struct SmallRandom
{
    private ulong _state;

    /// <summary>A generator seeded with <paramref name="seed"/>; equal seeds give equal sequences.</summary>
    internal static SmallRandom FromSeed(ulong seed) => new() { _state = seed };

    /// <summary>The next value, uniform in <c>[minimum, maximum)</c>.</summary>
    internal float Range(float minimum, float maximum)
    {
        _state = unchecked(_state + 0x9E37_79B9_7F4A_7C15);
        ulong mixed = _state;
        mixed = unchecked((mixed ^ (mixed >> 30)) * 0xBF58_476D_1CE4_E5B9);
        mixed = unchecked((mixed ^ (mixed >> 27)) * 0x94D0_49BB_1331_11EB);
        mixed ^= mixed >> 31;
        // The top 24 bits fill an f32 mantissa exactly.
        float unit = (mixed >> 40) / (float)(1UL << 24);
        return minimum + unit * (maximum - minimum);
    }
}
