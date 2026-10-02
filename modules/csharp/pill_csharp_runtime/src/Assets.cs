// Managed-side value types for the asset-loading bridge (see Engine's
// LoadMeshObj/LoadTexturePng/LoadShader/CreateMaterial). Kept apart from
// Engine.cs because these are plain data the caller builds, not part of the
// native-callback facade itself.

namespace TracyLive;

/// <summary>
/// A handle to an asset the host's <c>AssetManager</c> now owns.
/// </summary>
/// <remarks>
/// Store this in a component (for example as the raw index/generation pair
/// backing <c>MeshRendererComponent</c>) to reference the asset from an
/// entity.
/// </remarks>
public readonly struct AssetHandle
{
    public readonly uint Index;
    public readonly uint Generation;

    internal AssetHandle(uint index, uint generation)
    {
        Index = index;
        Generation = generation;
    }

    /// <summary>No asset - for a material, leaves the renderer's default shader in place.</summary>
    public static readonly AssetHandle None = new(uint.MaxValue, uint.MaxValue);
}

/// <summary>The kind of value a shader parameter slot holds.</summary>
public enum ShaderParameterKind : byte
{
    Scalar = 0,
    Bool = 1,
    Color = 2,
}

/// <summary>One parameter slot declared when loading a shader.</summary>
public readonly struct ShaderParameter
{
    public readonly string Name;
    public readonly ShaderParameterKind Kind;

    public ShaderParameter(string name, ShaderParameterKind kind)
    {
        Name = name;
        Kind = kind;
    }
}

/// <summary>One texture slot declared when loading a shader.</summary>
/// <remarks>The bound texture is always color-typed.</remarks>
public readonly struct ShaderTextureBinding
{
    public readonly string Name;
    public readonly uint TextureBinding;
    public readonly uint SamplerBinding;

    public ShaderTextureBinding(string name, uint textureBinding, uint samplerBinding)
    {
        Name = name;
        TextureBinding = textureBinding;
        SamplerBinding = samplerBinding;
    }
}

/// <summary>One texture a material binds to a shader slot.</summary>
public readonly struct MaterialTextureBinding
{
    public readonly string Slot;
    public readonly AssetHandle Texture;

    public MaterialTextureBinding(string slot, AssetHandle texture)
    {
        Slot = slot;
        Texture = texture;
    }
}

/// <summary>One scalar parameter a material sets.</summary>
public readonly struct MaterialScalarParameter
{
    public readonly string Name;
    public readonly float Value;

    public MaterialScalarParameter(string name, float value)
    {
        Name = name;
        Value = value;
    }
}

/// <summary>One color parameter a material sets.</summary>
public readonly struct MaterialColorParameter
{
    public readonly string Name;
    public readonly float R;
    public readonly float G;
    public readonly float B;

    public MaterialColorParameter(string name, float r, float g, float b)
    {
        Name = name;
        R = r;
        G = g;
        B = b;
    }
}

/// <summary>What <see cref="Assets.Import{T}"/> does when the source has no <c>.meta</c> file yet.</summary>
public enum MetadataPolicy : byte
{
    /// <summary>Use the initial settings and write nothing.</summary>
    ReadIfPresent = 0,
    /// <summary>Use the initial settings and write the <c>.meta</c> file beside the source.</summary>
    CreateIfMissing = 1,
}

/// <summary>The result of <see cref="Assets.Import{T}"/>.</summary>
public readonly struct ImportedAsset
{
    /// <summary>The asset's handle in the host's <c>AssetManager</c>.</summary>
    public readonly AssetHandle Handle;
    /// <summary>The asset's guid, as the 32 hexadecimal digits its <c>.meta</c> file holds.</summary>
    public readonly string Guid;
    /// <summary>The path was already loaded, and its existing handle was returned.</summary>
    public readonly bool AlreadyLoaded;

    internal ImportedAsset(AssetHandle handle, string guid, bool alreadyLoaded)
    {
        Handle = handle;
        Guid = guid;
        AlreadyLoaded = alreadyLoaded;
    }
}

/// <summary>Which import slot an <see cref="IImportedAssetType"/> uses.</summary>
internal enum ImportedAssetKind : byte
{
    Texture,
    Mesh,
    Sound,
}

/// <summary>An asset type <see cref="Assets.Import{T}"/> can import.</summary>
/// <remarks>Implemented only by this runtime's marker types.</remarks>
public interface IImportedAssetType
{
    /// <summary>The import slot the type maps to.</summary>
    internal ImportedAssetKind Kind { get; }
}

/// <summary>A texture (<c>.png</c>, <c>.jpg</c>), for <see cref="Assets.Import{T}"/>.</summary>
public readonly struct TextureAsset : IImportedAssetType
{
    ImportedAssetKind IImportedAssetType.Kind => ImportedAssetKind.Texture;
}

/// <summary>A mesh (<c>.obj</c>), for <see cref="Assets.Import{T}"/>.</summary>
public readonly struct MeshAsset : IImportedAssetType
{
    ImportedAssetKind IImportedAssetType.Kind => ImportedAssetKind.Mesh;
}

/// <summary>A sound (<c>.mp3</c>, <c>.wav</c>, <c>.ogg</c>, <c>.flac</c>), for <see cref="Assets.Import{T}"/>; needs the project to load <c>pill_audio</c>.</summary>
public readonly struct SoundAsset : IImportedAssetType
{
    ImportedAssetKind IImportedAssetType.Kind => ImportedAssetKind.Sound;
}

/// <summary>
/// Importing source assets from <c>res</c> through their <c>.meta</c> files,
/// the way a Rust project's <c>AssetManager::import</c> does.
/// </summary>
public static class Assets
{
    /// <summary>
    /// Imports the source at <paramref name="path"/> (relative to <c>res</c>)
    /// as a <typeparamref name="T"/>. The asset is named by its path and keyed
    /// by the guid in its <c>.meta</c> file; importing a loaded path returns
    /// its handle with <see cref="ImportedAsset.AlreadyLoaded"/> set.
    /// </summary>
    /// <param name="path">The source file, relative to the project's <c>res</c>.</param>
    /// <param name="policy">Whether to write a missing <c>.meta</c> file.</param>
    /// <param name="initialSettingsJson">
    /// The type's import settings as JSON, used only when no <c>.meta</c> file
    /// exists yet; <c>null</c> for the type's defaults.
    /// </param>
    /// <remarks>Only valid inside an active invocation, such as an <see cref="EcsStartupAttribute"/> method.</remarks>
    /// <exception cref="InvalidOperationException">The import failed; the message says why.</exception>
    public static ImportedAsset Import<T>(string path, MetadataPolicy policy, string? initialSettingsJson = null)
        where T : struct, IImportedAssetType
        => Engine.ImportAsset(default(T).Kind, path, policy, initialSettingsJson);
}
