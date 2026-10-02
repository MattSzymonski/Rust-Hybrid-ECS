// Imports this project's source assets through their .meta files.
//
// Responsibilities
// - Import `res/textures/checker.png` and `res/models/cube.obj` through
//   `Assets.Import<T>`, which gives each a guid kept in a `.meta` file beside it
//   (written on the first run, read on every later one).
// - Log each asset's guid, so a run can be compared with the next one.
//
// Design
// The managed counterpart of a Rust project's `AssetManager::import` calls. The
// decoding and the `.meta` handling happen on the Rust side, in the renderer
// data crate's import exports; this file names paths and a policy only. It runs
// from an `[EcsStartup]` method because asset calls need an active invocation,
// and a startup runs once for the life of the host, so it needs no guard
// against importing twice - and an import of a loaded path would return the
// loaded asset anyway.

namespace TracyLive;

/// <summary>Imports the project's texture and mesh at startup.</summary>
public static class AssetImportStartup
{
    /// <summary>The texture, relative to <c>res</c>.</summary>
    internal const string CheckerTexture = "textures/checker.png";

    /// <summary>The mesh, relative to <c>res</c>.</summary>
    internal const string CubeMesh = "models/cube.obj";

    /// <summary>Imports both assets and logs their guids.</summary>
    [EcsStartup]
    public static void Start(Commands commands)
    {
        _ = commands;
        ImportedAsset texture = Assets.Import<TextureAsset>(CheckerTexture, MetadataPolicy.CreateIfMissing);
        ImportedAsset mesh = Assets.Import<MeshAsset>(CubeMesh, MetadataPolicy.CreateIfMissing);
        Log.Info(
            $"[project_cs] imported {CheckerTexture} guid={texture.Guid} already_loaded={texture.AlreadyLoaded}; " +
            $"{CubeMesh} guid={mesh.Guid} already_loaded={mesh.AlreadyLoaded}");
    }
}
