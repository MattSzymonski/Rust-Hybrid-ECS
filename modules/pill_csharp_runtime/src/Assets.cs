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
