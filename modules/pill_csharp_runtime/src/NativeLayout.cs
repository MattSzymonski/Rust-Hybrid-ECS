// AOT-safe native layout computation for component value types.
//
// `Marshal.SizeOf(Type)` and `Marshal.OffsetOf(Type, string)` require
// structure-marshalling data that NativeAOT does not generate - they throw
// "missing structure marshalling data" at runtime in an AOT build. This helper
// computes the same sequential-layout sizes, alignments and field offsets by
// hand, from reflection over fields, which works identically under JIT and
// AOT. The result must agree byte-for-byte with what `Marshal` reports on the
// JIT path, because the component manifest is the contract the native host
// registers.
//
// Layout rules implemented (matching .NET sequential unmanaged layout for the
// component shapes this runtime accepts):
// - a `[StructLayout(Size = N)]` value is authoritative (generated mirrors);
// - primitive fields and enums use their natural size/alignment;
// - nested value types recurse;
// - a struct's size is its fields' aligned sum, padded to its alignment
//   (the maximum field alignment).

using System.Reflection;

namespace TracyLive.Loader;

/// <summary>Manual size/alignment/offset computation for blittable structs.</summary>
internal static class NativeLayout
{
    private const BindingFlags InstanceFields =
        BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic;

    /// <summary>Instance fields in declaration order.</summary>
    private static FieldInfo[] Fields(Type type) =>
        type.GetFields(InstanceFields);

    /// <summary>Natural size of a primitive or enum, in bytes.</summary>
    private static int PrimitiveSize(Type type)
    {
        if (type.IsEnum)
            type = Enum.GetUnderlyingType(type);
        if (type == typeof(bool) || type == typeof(byte) || type == typeof(sbyte))
            return 1;
        if (type == typeof(char) || type == typeof(short) || type == typeof(ushort))
            return 2;
        if (type == typeof(int) || type == typeof(uint) || type == typeof(float))
            return 4;
        if (type == typeof(long) || type == typeof(ulong) || type == typeof(double))
            return 8;
        if (type == typeof(IntPtr) || type == typeof(UIntPtr))
            return IntPtr.Size;
        throw new InvalidOperationException(
            $"type {type.FullName} has no known primitive size");
    }

    /// <summary>Natural alignment of a primitive or enum, in bytes.</summary>
    private static int PrimitiveAlignment(Type type)
    {
        if (type.IsEnum)
            type = Enum.GetUnderlyingType(type);
        if (type == typeof(bool) || type == typeof(byte) || type == typeof(sbyte))
            return 1;
        if (type == typeof(char) || type == typeof(short) || type == typeof(ushort))
            return 2;
        if (type == typeof(long) || type == typeof(ulong) || type == typeof(double))
            return 8;
        return 4;
    }

    private static int AlignUp(int value, int alignment) =>
        alignment <= 1 ? value : (value + alignment - 1) / alignment * alignment;

    /// <summary>Native size of a component value type, in bytes.</summary>
    internal static int SizeOf(Type type)
    {
        // An explicit `[StructLayout(Size = N)]` is authoritative: generated
        // module mirrors declare the exact native size and keep only an
        // alignment pad as a real field.
        int? declaredSize = type.StructLayoutAttribute?.Size;
        if (declaredSize is > 0)
            return declaredSize.Value;
        if (type.IsPrimitive || type.IsEnum)
            return PrimitiveSize(type);
        int offset = 0;
        int alignment = 1;
        foreach (FieldInfo field in Fields(type))
        {
            int fieldAlignment = AlignmentOf(field.FieldType);
            offset = AlignUp(offset, fieldAlignment);
            offset += SizeOf(field.FieldType);
            if (fieldAlignment > alignment)
                alignment = fieldAlignment;
        }
        return AlignUp(offset, alignment);
    }

    /// <summary>Native alignment of a component value type, in bytes.</summary>
    internal static int AlignmentOf(Type type)
    {
        if (type.IsPrimitive || type.IsEnum)
            return PrimitiveAlignment(type);
        int alignment = 1;
        foreach (FieldInfo field in Fields(type))
        {
            int fieldAlignment = AlignmentOf(field.FieldType);
            if (fieldAlignment > alignment)
                alignment = fieldAlignment;
        }
        return alignment;
    }

    /// <summary>Byte offset of one instance field within its declaring struct.</summary>
    internal static int FieldOffset(Type type, string fieldName)
    {
        int offset = 0;
        foreach (FieldInfo field in Fields(type))
        {
            if (field.Name == fieldName)
                return offset;
            offset = AlignUp(offset, AlignmentOf(field.FieldType));
            offset += SizeOf(field.FieldType);
        }
        throw new InvalidOperationException(
            $"field {type.FullName}.{fieldName} does not exist");
    }
}
