// C# component discovery and native manifest generation.
//
// Every unmanaged query component and supported game-declared struct is
// described before Rust registers managed systems or runs startup commands.
// Native-owned mirrors and game-owned structs use the same schema format and
// stable 128-bit identity.

using System.Runtime.InteropServices;
using System.Reflection;
using System.Runtime.CompilerServices;
using System.Text;
using System.Text.Json;

namespace TracyLive.Loader;

internal sealed record ComponentFieldManifest(
    string Name, int Offset, int Size, string PrimitiveType,
    ComponentFieldManifest[] Fields);

internal sealed record ComponentManifest(
    ulong StableIdLow, ulong StableIdHigh, string FullName,
    int Size, int Alignment, ulong SchemaHash, bool Shared,
    ComponentFieldManifest[] Fields);

internal static class ComponentManifestBuilder
{
    [StructLayout(LayoutKind.Sequential)]
    private struct AlignmentProbe<T> where T : unmanaged
    {
        public byte Prefix;
        public T Value;
    }

    internal static byte[] Build(IEnumerable<ManagedSystem> systems, Assembly? gameAssembly = null)
    {
        IEnumerable<Type> queryComponents = systems
            .Where(system => system.QueryDescriptor is not null)
            .SelectMany(system => system.QueryDescriptor!.Terms)
            .Where(term => !term.IsEntity)
            .Select(term => term.ComponentType!);
        IEnumerable<Type> declaredGameComponents = gameAssembly is null
            ? []
            : gameAssembly.GetTypes().Where(IsGameComponentCandidate);
        ComponentManifest[] components = queryComponents
            .Concat(declaredGameComponents)
            .Distinct()
            .Select(Describe)
            .OrderBy(component => component.StableIdHigh)
            .ThenBy(component => component.StableIdLow)
            .ToArray();
        return JsonSerializer.SerializeToUtf8Bytes(components, new JsonSerializerOptions
        {
            PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
        });
    }

    /// <summary>
    /// Include supported unmanaged structs declared by the game even when
    /// they currently appear only in Commands.With/Add/Remove calls. Query
    /// terms remain authoritative for shared runtime component discovery.
    /// </summary>
    private static bool IsGameComponentCandidate(Type type)
    {
        if (!type.IsValueType || type.IsEnum || type.IsPrimitive || type.IsGenericType ||
            type.IsDefined(typeof(CompilerGeneratedAttribute), inherit: false))
            return false;
        try
        {
            ValidateValueType(type, new HashSet<Type>());
            return true;
        }
        catch (InvalidOperationException)
        {
            return false;
        }
    }

    private static ComponentManifest Describe(Type type)
    {
        ValidateValueType(type, new HashSet<Type>());
        StableComponentId id = Engine.ComponentStableId(type);
        ComponentFieldManifest[] fields = DescribeFields(type);
        string schema = SchemaText(type, fields);
        return new ComponentManifest(
            id.Low,
            id.High,
            type.FullName ?? type.Name,
            Marshal.SizeOf(type),
            AlignmentOf(type),
            Hash64(schema),
            type.Assembly == typeof(Engine).Assembly,
            fields);
    }

    private static void ValidateValueType(Type type, HashSet<Type> visiting)
    {
        if (!type.IsValueType || type.IsAutoLayout)
            throw new InvalidOperationException(
                $"Component {type.FullName} must be a sequential or explicit-layout value type.");
        if (!visiting.Add(type))
            throw new InvalidOperationException($"Component {type.FullName} has a recursive layout.");
        foreach (var field in type.GetFields(
                     System.Reflection.BindingFlags.Instance |
                     System.Reflection.BindingFlags.Public |
                     System.Reflection.BindingFlags.NonPublic))
        {
            Type fieldType = field.FieldType;
            if (fieldType.IsEnum)
                fieldType = Enum.GetUnderlyingType(fieldType);
            if (fieldType == typeof(bool) || fieldType == typeof(char) ||
                (!fieldType.IsPrimitive && !fieldType.IsValueType) ||
                fieldType.IsPointer || fieldType.IsByRef)
                throw new InvalidOperationException(
                    $"Component field {type.FullName}.{field.Name} has unsupported type {field.FieldType}.");
            if (!fieldType.IsPrimitive)
                ValidateValueType(fieldType, visiting);
        }
        visiting.Remove(type);
    }

    private static ComponentFieldManifest[] DescribeFields(Type type) => type
        .GetFields(System.Reflection.BindingFlags.Instance |
                   System.Reflection.BindingFlags.Public |
                   System.Reflection.BindingFlags.NonPublic)
        .OrderBy(field => Marshal.OffsetOf(type, field.Name).ToInt32())
        .Select(field =>
        {
            Type valueType = field.FieldType.IsEnum
                ? Enum.GetUnderlyingType(field.FieldType)
                : field.FieldType;
            bool primitive = valueType.IsPrimitive;
            return new ComponentFieldManifest(
                field.Name,
                Marshal.OffsetOf(type, field.Name).ToInt32(),
                Marshal.SizeOf(valueType),
                primitive ? valueType.FullName! : "struct",
                primitive ? [] : DescribeFields(valueType));
        })
        .ToArray();

    private static int AlignmentOf(Type type)
    {
        Type probe = typeof(AlignmentProbe<>).MakeGenericType(type);
        return Marshal.OffsetOf(probe, nameof(AlignmentProbe<int>.Value)).ToInt32();
    }

    private static string SchemaText(Type type, ComponentFieldManifest[] fields)
    {
        var text = new StringBuilder(type.FullName).Append('|')
            .Append(Marshal.SizeOf(type)).Append('|').Append(AlignmentOf(type));
        AppendFields(text, fields);
        return text.ToString();
    }

    private static void AppendFields(StringBuilder text, ComponentFieldManifest[] fields)
    {
        foreach (var field in fields)
        {
            text.Append('|').Append(field.Name).Append('@').Append(field.Offset)
                .Append(':').Append(field.Size).Append(':').Append(field.PrimitiveType);
            AppendFields(text, field.Fields);
        }
    }

    private static ulong Hash64(string value)
    {
        const ulong prime = 0x100000001b3;
        ulong hash = 0xcbf29ce484222325;
        foreach (byte item in Encoding.UTF8.GetBytes(value))
        {
            hash ^= item;
            hash = unchecked(hash * prime);
        }
        return hash;
    }
}
