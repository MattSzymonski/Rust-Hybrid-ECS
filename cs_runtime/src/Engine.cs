// Safe managed ECS facade and composable scheduler-aware queries.
//
// Responsibilities:
// - Binds the native engine function table supplied by the Rust host.
// - Describes queries as independent Read/Write/Optional/Entity terms.
// - Joins native archetype columns without reflection or allocation per frame.
// - Keeps every native pointer inside stack-only enumerator and row values.
//
// Design:
// - Closed Query<T...> types build and validate their descriptor once.
// - GameHost consumes IQueryDescriptor without knowing query arity or shape.
// - QueryRow is a ref struct. Typed accessors validate the declared term before
//   returning a writable or read-only reference into the active native chunk.

using System.Runtime.InteropServices;
using System.Text;

namespace TracyLive;

// =============================================================================
// System Declaration
// =============================================================================

/// <summary>Marks a static method for automatic ECS system discovery.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class EcsSystemAttribute : Attribute;

// =============================================================================
// Query Terms and Descriptors
// =============================================================================

/// <summary>Required read-only component term.</summary>
public readonly struct Read<T> where T : unmanaged;

/// <summary>Required writable component term.</summary>
public readonly struct Write<T> where T : unmanaged;

/// <summary>Optional read-only component term.</summary>
public readonly struct OptionalRead<T> where T : unmanaged;

/// <summary>Optional writable component term.</summary>
public readonly struct OptionalWrite<T> where T : unmanaged;

/// <summary>Term exposing the current entity without scheduler component access.</summary>
public readonly struct EntityTerm;

/// <summary>Native access mode declared to the Rust scheduler.</summary>
public enum QueryAccess : byte
{
    Read = 0,
    Write = 1,
}

/// <summary>Immutable metadata for one ordered query term.</summary>
public readonly record struct QueryTermDescriptor(
    Type? ComponentType,
    ulong ComponentKey,
    int ComponentSize,
    QueryAccess Access,
    bool Optional,
    bool IsEntity);

/// <summary>Validated metadata shared by discovery and iteration.</summary>
public sealed class QueryDescriptor
{
    private readonly QueryTermDescriptor[] _terms;

    internal QueryDescriptor(params Type[] termTypes)
    {
        if (termTypes.Length == 0)
            throw new InvalidOperationException("An ECS query must contain at least one term.");

        _terms = termTypes.Select(CreateTerm).ToArray();
        var components = new HashSet<ulong>();
        var hasEntity = false;
        foreach (var term in _terms)
        {
            if (term.IsEntity)
            {
                if (hasEntity)
                    throw new InvalidOperationException("An ECS query cannot contain EntityTerm more than once.");
                hasEntity = true;
            }
            else if (!components.Add(term.ComponentKey))
            {
                throw new InvalidOperationException(
                    $"An ECS query cannot contain component {term.ComponentType!.FullName} more than once.");
            }
        }
    }

    /// <summary>Ordered terms exactly as written in the system signature.</summary>
    public IReadOnlyList<QueryTermDescriptor> Terms => _terms;

    internal QueryTermDescriptor[] TermArray => _terms;

    private static QueryTermDescriptor CreateTerm(Type termType)
    {
        if (termType == typeof(EntityTerm))
            return new QueryTermDescriptor(null, 0, Marshal.SizeOf<Entity>(), QueryAccess.Read, false, true);
        if (!termType.IsGenericType)
            throw UnsupportedTerm(termType);

        Type definition = termType.GetGenericTypeDefinition();
        Type component = termType.GetGenericArguments()[0];
        QueryAccess access;
        bool optional;
        if (definition == typeof(Read<>))
            (access, optional) = (QueryAccess.Read, false);
        else if (definition == typeof(Write<>))
            (access, optional) = (QueryAccess.Write, false);
        else if (definition == typeof(OptionalRead<>))
            (access, optional) = (QueryAccess.Read, true);
        else if (definition == typeof(OptionalWrite<>))
            (access, optional) = (QueryAccess.Write, true);
        else
            throw UnsupportedTerm(termType);

        return new QueryTermDescriptor(
            component,
            Engine.ComponentKey(component),
            Marshal.SizeOf(component),
            access,
            optional,
            false);
    }

    private static InvalidOperationException UnsupportedTerm(Type termType) => new(
        $"{termType} is not a query term. Use Read<T>, Write<T>, OptionalRead<T>, " +
        "OptionalWrite<T>, or EntityTerm.");
}

/// <summary>Implemented by every composable closed query type.</summary>
public interface IQueryDescriptor
{
    /// <summary>Return cached ordered query metadata.</summary>
    QueryDescriptor Descriptor { get; }
}

// =============================================================================
// Native Engine Facade
// =============================================================================

/// <summary>Safe facade over the type-erased native ECS query API.</summary>
public static unsafe class Engine
{
    private static EngineApi _api;

    /// <summary>Bind the native function table for all subsequent queries.</summary>
    public static void Bind(EngineApi* api) => _api = *api;
    /// <summary>Bind an unmanaged pointer received by the exported loader API.</summary>
    public static void Bind(IntPtr api) => Bind((EngineApi*)api);
    /// <summary>Return the active native world's entity count.</summary>
    public static uint EntityCount() => _api.EntityCount();

    /// <summary>Build the stable component key shared with the Rust adapter.</summary>
    internal static ulong ComponentKey(Type type) => HashName(type.Name);

    internal static bool TryGetChunk(
        QueryTermDescriptor term,
        uint index,
        out NativeComponentChunk chunk)
    {
        NativeComponentChunk result;
        byte status = _api.GetComponentChunk(
            term.ComponentKey, (byte)term.Access, index, &result);
        chunk = result;
        if (status == 0)
            return false;
        ValidateStatus(status, term.ComponentType?.FullName ?? "<unknown>", term.Access);
        if (chunk.ElementSize != term.ComponentSize)
            throw new InvalidOperationException(
                $"Component {term.ComponentType!.FullName} has size {term.ComponentSize} in C# but " +
                $"{chunk.ElementSize} in Rust. The component layouts must match exactly.");
        if (term.Access == QueryAccess.Write && chunk.Ticks == IntPtr.Zero)
            throw new InvalidOperationException(
                $"Writable component {term.ComponentType!.FullName} has no native change-tick column.");
        return true;
    }

    internal static bool TryGetEntityChunk(uint index, out NativeComponentChunk chunk)
    {
        NativeComponentChunk result;
        byte status = _api.GetEntityChunk(index, &result);
        chunk = result;
        if (status == 0)
            return false;
        ValidateStatus(status, nameof(Entity), QueryAccess.Read);
        if (chunk.ElementSize != Marshal.SizeOf<Entity>())
            throw new InvalidOperationException(
                $"Entity has size {Marshal.SizeOf<Entity>()} in C# but {chunk.ElementSize} in Rust.");
        return true;
    }

    private static void ValidateStatus(byte status, string name, QueryAccess access)
    {
        if (status == 2)
            throw new InvalidOperationException($"Component {name} is not registered by the Rust host.");
        if (status == 3)
            throw new InvalidOperationException("An ECS query was used outside its scheduled system call.");
        if (status != 1)
            throw new InvalidOperationException(
                $"The current system did not declare this {access.ToString().ToLowerInvariant()} access to {name}.");
    }

    private static ulong HashName(string name)
    {
        const ulong offset = 0xcbf29ce484222325;
        const ulong prime = 0x100000001b3;
        ulong hash = offset;
        foreach (byte value in Encoding.UTF8.GetBytes(name))
        {
            hash ^= value;
            hash = unchecked(hash * prime);
        }
        return hash;
    }
}

// =============================================================================
// Stack-only Row Views
// =============================================================================

/// <summary>ABI-compatible managed entity handle.</summary>
[StructLayout(LayoutKind.Sequential)]
public readonly struct Entity
{
    public readonly ulong Id;
    public readonly uint Generation;

    public Entity(ulong id, uint generation) => (Id, Generation) = (id, generation);
}

internal readonly record struct ArchetypeKey(ulong Low, ulong High);

internal struct QueryColumn
{
    internal QueryTermDescriptor Term;
    internal IntPtr Data;
    internal int Length;
    internal bool Present;
    internal IntPtr Ticks;
    internal uint ChangeTick;
}

/// <summary>Optional writable reference valid only for the current row.</summary>
public readonly unsafe ref struct OptionalWriteRef<T> where T : unmanaged
{
    private readonly T* _value;
    private readonly NativeComponentTicks* _ticks;
    private readonly uint _changeTick;
    internal OptionalWriteRef(T* value, NativeComponentTicks* ticks, uint changeTick)
    {
        _value = value;
        _ticks = ticks;
        _changeTick = changeTick;
    }
    public bool HasValue => _value != null;
    public ref T Value
    {
        get
        {
            if (_value == null)
                throw new InvalidOperationException($"Optional component {typeof(T).FullName} is absent.");
            if (_ticks == null)
                throw new InvalidOperationException(
                    $"Writable component {typeof(T).FullName} has no native change-tick column.");
            _ticks->Changed = _changeTick;
            return ref *_value;
        }
    }
}

/// <summary>Optional read-only reference valid only for the current row.</summary>
public readonly unsafe ref struct OptionalReadRef<T> where T : unmanaged
{
    private readonly T* _value;
    internal OptionalReadRef(T* value) => _value = value;
    public bool HasValue => _value != null;
    public ref readonly T Value
    {
        get
        {
            if (_value == null)
                throw new InvalidOperationException($"Optional component {typeof(T).FullName} is absent.");
            return ref *_value;
        }
    }
}

/// <summary>Stack-only view over one joined ECS row.</summary>
public readonly unsafe ref struct QueryRow
{
    private readonly QueryColumn _c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7;
    private readonly int _count;
    private readonly int _row;

    internal QueryRow(
        QueryColumn c0, QueryColumn c1, QueryColumn c2, QueryColumn c3,
        QueryColumn c4, QueryColumn c5, QueryColumn c6, QueryColumn c7,
        int count, int row) =>
        (_c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7, _count, _row) =
        (c0, c1, c2, c3, c4, c5, c6, c7, count, row);

    /// <summary>Borrow a required writable component declared by Write&lt;T&gt;.</summary>
    public ref T Write<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Write, optional: false);
        MarkChanged(column);
        return ref ((T*)column.Data)[_row];
    }

    /// <summary>Borrow a required read-only component declared by Read&lt;T&gt;.</summary>
    public ref readonly T Read<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Read, optional: false);
        return ref ((T*)column.Data)[_row];
    }

    /// <summary>Borrow an optional writable component when it is present.</summary>
    public OptionalWriteRef<T> OptionalWrite<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Write, optional: true);
        return new OptionalWriteRef<T>(
            column.Present ? &((T*)column.Data)[_row] : null,
            column.Present ? &((NativeComponentTicks*)column.Ticks)[_row] : null,
            column.ChangeTick);
    }

    /// <summary>Borrow an optional read-only component when it is present.</summary>
    public OptionalReadRef<T> OptionalRead<T>() where T : unmanaged
    {
        QueryColumn column = Find<T>(QueryAccess.Read, optional: true);
        return new OptionalReadRef<T>(column.Present ? &((T*)column.Data)[_row] : null);
    }

    /// <summary>Return the current entity declared by EntityTerm.</summary>
    public Entity Entity
    {
        get
        {
            for (var i = 0; i < _count; i++)
            {
                QueryColumn column = Column(i);
                if (column.Term.IsEntity)
                    return ((Entity*)column.Data)[_row];
            }
            throw new InvalidOperationException("This query does not declare EntityTerm.");
        }
    }

    private QueryColumn Find<T>(QueryAccess access, bool optional) where T : unmanaged
    {
        ulong key = Engine.ComponentKey(typeof(T));
        for (var i = 0; i < _count; i++)
        {
            QueryColumn column = Column(i);
            if (!column.Term.IsEntity && column.Term.ComponentKey == key &&
                column.Term.Access == access && column.Term.Optional == optional)
                return column;
        }
        throw new InvalidOperationException(
            $"This query does not declare {(optional ? "optional " : "")}{access.ToString().ToLowerInvariant()} " +
            $"access to {typeof(T).FullName}.");
    }

    private void MarkChanged(QueryColumn column)
    {
        if (column.Ticks == IntPtr.Zero)
            throw new InvalidOperationException(
                $"Writable component {column.Term.ComponentType!.FullName} has no native change-tick column.");
        ((NativeComponentTicks*)column.Ticks)[_row].Changed = column.ChangeTick;
    }

    private QueryColumn Column(int index)
    {
        switch (index)
        {
            case 0: return _c0;
            case 1: return _c1;
            case 2: return _c2;
            case 3: return _c3;
            case 4: return _c4;
            case 5: return _c5;
            case 6: return _c6;
            case 7: return _c7;
            default: throw new ArgumentOutOfRangeException(nameof(index));
        }
    }
}

// =============================================================================
// Shared Query Iterator
// =============================================================================

/// <summary>Stack-only iterator shared by every composable query shape.</summary>
public ref struct QueryEnumerator
{
    private readonly QueryTermDescriptor[] _terms;
    private QueryColumn _c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7;
    private readonly int _driver;
    private uint _nextDriverChunk;
    private int _row;
    private int _length;

    internal QueryEnumerator(QueryDescriptor descriptor)
    {
        _terms = descriptor.TermArray;
        _c0 = _c1 = _c2 = _c3 = _c4 = _c5 = _c6 = _c7 = default;
        _driver = FindDriver(_terms);
        _nextDriverChunk = 0;
        _row = -1;
        _length = 0;
    }

    /// <summary>Return the current stack-only joined row.</summary>
    public QueryRow Current => new(
        _c0, _c1, _c2, _c3, _c4, _c5, _c6, _c7, _terms.Length, _row);

    /// <summary>Advance within the current archetype or join the next one.</summary>
    public bool MoveNext()
    {
        if (++_row < _length)
            return true;

        while (TryLoadDriver(_nextDriverChunk++, out var archetype, out var driverChunk))
        {
            _length = checked((int)driverChunk.Length);
            if (_length == 0)
                continue;

            var matched = true;
            for (var i = 0; i < _terms.Length; i++)
            {
                QueryTermDescriptor term = _terms[i];
                NativeComponentChunk chunk;
                bool present;
                if (i == _driver)
                {
                    chunk = driverChunk;
                    present = true;
                }
                else
                {
                    present = TryFindChunk(term, archetype, out chunk);
                    if (!present && !term.Optional)
                    {
                        matched = false;
                        break;
                    }
                }

                if (present && chunk.Length != driverChunk.Length)
                    throw new InvalidOperationException("ECS component chunk lengths are inconsistent.");
                SetColumn(i, new QueryColumn
                {
                    Term = term,
                    Data = chunk.Data,
                    Length = present ? checked((int)chunk.Length) : 0,
                    Present = present,
                    Ticks = present ? chunk.Ticks : IntPtr.Zero,
                    ChangeTick = present ? chunk.ChangeTick : 0,
                });
            }

            if (!matched)
                continue;
            _row = 0;
            return true;
        }
        return false;
    }

    private static int FindDriver(QueryTermDescriptor[] terms)
    {
        for (var i = 0; i < terms.Length; i++)
            if (!terms[i].IsEntity && !terms[i].Optional)
                return i;
        for (var i = 0; i < terms.Length; i++)
            if (terms[i].IsEntity)
                return i;
        return -1; // An implicit entity driver makes optional-only queries useful.
    }

    private bool TryLoadDriver(
        uint index, out ArchetypeKey archetype, out NativeComponentChunk chunk)
    {
        bool found = _driver >= 0 && !_terms[_driver].IsEntity
            ? Engine.TryGetChunk(_terms[_driver], index, out chunk)
            : Engine.TryGetEntityChunk(index, out chunk);
        archetype = found
            ? new ArchetypeKey(chunk.ArchetypeLow, chunk.ArchetypeHigh)
            : default;
        return found;
    }

    private static bool TryFindChunk(
        QueryTermDescriptor term, ArchetypeKey wanted, out NativeComponentChunk chunk)
    {
        for (uint index = 0; ; index++)
        {
            bool found = term.IsEntity
                ? Engine.TryGetEntityChunk(index, out chunk)
                : Engine.TryGetChunk(term, index, out chunk);
            if (!found)
                return false;
            if (new ArchetypeKey(chunk.ArchetypeLow, chunk.ArchetypeHigh) == wanted)
                return true;
        }
    }

    private void SetColumn(int index, QueryColumn value)
    {
        switch (index)
        {
            case 0: _c0 = value; break;
            case 1: _c1 = value; break;
            case 2: _c2 = value; break;
            case 3: _c3 = value; break;
            case 4: _c4 = value; break;
            case 5: _c5 = value; break;
            case 6: _c6 = value; break;
            case 7: _c7 = value; break;
            default: throw new ArgumentOutOfRangeException(nameof(index));
        }
    }
}

// =============================================================================
// Composable Query Arities
// =============================================================================

/// <summary>Shared behavior for all composable query arities.</summary>
public abstract class QueryBase : IQueryDescriptor
{
    protected QueryBase(QueryDescriptor descriptor) => Descriptor = descriptor;
    public QueryDescriptor Descriptor { get; }
    public QueryEnumerator GetEnumerator() => new(Descriptor);
}

public sealed class Query<T1> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3, T4> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3, T4, T5> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3, T4, T5, T6> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3, T4, T5, T6, T7> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6), typeof(T7));
    internal Query() : base(Cached) { }
}

public sealed class Query<T1, T2, T3, T4, T5, T6, T7, T8> : QueryBase
{
    private static readonly QueryDescriptor Cached = new(typeof(T1), typeof(T2), typeof(T3), typeof(T4), typeof(T5), typeof(T6), typeof(T7), typeof(T8));
    internal Query() : base(Cached) { }
}
