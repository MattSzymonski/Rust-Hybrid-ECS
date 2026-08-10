// Safe managed ECS facade and scheduler-aware query parameters.
//
// Responsibilities:
// - Binds the native engine function table supplied by the Rust host.
// - Converts native component columns into short-lived managed spans.
// - Exposes foreach-compatible queries whose generic arguments declare access.
// - Rejects layout mismatches and access outside a scheduled system call.
//
// Design:
// - Query objects contain no component data. Enumerators borrow matching native
//   archetype columns only while Rust is executing the owning managed system.
// - Writable/read-only intent is encoded in each query type and reflected by
//   GameHost, making the method signature the scheduler declaration.

using System.Text;

namespace TracyLive;

// =============================================================================
// System Declaration
// =============================================================================

/// <summary>Marks a static method for automatic ECS system discovery.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class EcsSystemAttribute : Attribute;

// =============================================================================
// Native Engine Facade
// =============================================================================

/// <summary>
/// Safe facade over the type-erased native ECS query API. Queries are created
/// by the loader from an <see cref="EcsSystemAttribute"/> method's parameter;
/// gameplay code never separately declares its component accesses.
/// </summary>
public static unsafe class Engine
{
    // Copied once during LoaderInterop.Init. The native host owns every target.
    private static EngineApi _api;

    /// <summary>Bind the native function table for all subsequent queries.</summary>
    public static void Bind(EngineApi* api) => _api = *api;
    /// <summary>Bind an unmanaged pointer received by the exported loader API.</summary>
    public static void Bind(IntPtr api) => Bind((EngineApi*)api);

    /// <summary>Return the active native world's entity count.</summary>
    public static uint EntityCount() => _api.EntityCount();

    /// <summary>Build the stable component key shared with the Rust adapter.</summary>
    internal static ulong ComponentKey(Type type) => HashName(type.Name);

    /// <summary>
    /// Borrow one native column and validate its scope, access mode, and layout.
    /// A false result means no chunk exists at <paramref name="index"/>.
    /// </summary>
    internal static bool TryGetChunk<T>(
        ulong key,
        byte mode,
        uint index,
        out ArchetypeKey archetype,
        out Span<T> values) where T : unmanaged
    {
        NativeComponentChunk chunk;
        byte status = _api.GetComponentChunk(key, mode, index, &chunk);
        if (status == 0)
        {
            archetype = default;
            values = default;
            return false;
        }
        if (status == 2)
            throw new InvalidOperationException(
                $"Component {typeof(T).FullName} is not registered by the Rust host.");
        if (status == 3)
            throw new InvalidOperationException("An ECS query was used outside its scheduled system call.");
        if (status != 1)
            throw new InvalidOperationException(
                $"The current system did not declare this {(mode == 1 ? "write" : "read")} access to {typeof(T).FullName}.");
        if (chunk.ElementSize != sizeof(T))
            throw new InvalidOperationException(
                $"Component {typeof(T).FullName} has size {sizeof(T)} in C# but " +
                $"{chunk.ElementSize} in Rust. The component layouts must match exactly.");

        archetype = new ArchetypeKey(chunk.ArchetypeLow, chunk.ArchetypeHigh);
        values = new Span<T>(chunk.Data.ToPointer(), checked((int)chunk.Length));
        return true;
    }

    /// <summary>Compute the FNV-1a hash used for cross-language type lookup.</summary>
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
// Row Views
// =============================================================================

/// <summary>Stable identity used to join columns from one native archetype.</summary>
internal readonly record struct ArchetypeKey(ulong Low, ulong High);

/// <summary>One writable component row returned by <see cref="WriteQuery{T}"/>.</summary>
public ref struct WriteRow<T> where T : unmanaged
{
    private Span<T> _values;
    private int _index;

    internal WriteRow(Span<T> values, int index)
    {
        _values = values;
        _index = index;
    }

    /// <summary>Borrow the writable component value for the current entity.</summary>
    public ref T Write => ref _values[_index];
}

/// <summary>One writable/read-only pair from the same archetype row.</summary>
public ref struct WriteReadRow<TWrite, TRead>
    where TWrite : unmanaged
    where TRead : unmanaged
{
    private Span<TWrite> _writes;
    private ReadOnlySpan<TRead> _reads;
    private int _index;

    internal WriteReadRow(Span<TWrite> writes, Span<TRead> reads, int index)
    {
        _writes = writes;
        _reads = reads;
        _index = index;
    }

    /// <summary>Borrow the writable component value.</summary>
    public ref TWrite Write => ref _writes[_index];
    /// <summary>Borrow the read-only component value.</summary>
    public ref readonly TRead Read => ref _reads[_index];
}

/// <summary>Three writable values from the same archetype row.</summary>
public ref struct Write3Row<TFirst, TSecond, TThird>
    where TFirst : unmanaged
    where TSecond : unmanaged
    where TThird : unmanaged
{
    private Span<TFirst> _first;
    private Span<TSecond> _second;
    private Span<TThird> _third;
    private int _index;

    internal Write3Row(Span<TFirst> first, Span<TSecond> second, Span<TThird> third, int index)
    {
        _first = first;
        _second = second;
        _third = third;
        _index = index;
    }

    /// <summary>Borrow the first writable component.</summary>
    public ref TFirst First => ref _first[_index];
    /// <summary>Borrow the second writable component.</summary>
    public ref TSecond Second => ref _second[_index];
    /// <summary>Borrow the third writable component.</summary>
    public ref TThird Third => ref _third[_index];
}

// =============================================================================
// Single-Write Query
// =============================================================================

/// <summary>A system parameter granting writable access to one component.</summary>
public sealed class WriteQuery<T> where T : unmanaged
{
    // Instances are created by GameHost while compiling a managed system.
    internal WriteQuery() { }

    /// <summary>Create a stack-only enumerator over matching native chunks.</summary>
    public Enumerator GetEnumerator() => new(Engine.ComponentKey(typeof(T)));

    /// <summary>Iterator that keeps borrowed native spans on the stack.</summary>
    public ref struct Enumerator
    {
        private readonly ulong _key;
        private uint _nextChunk;
        private Span<T> _values;
        private int _row;

        internal Enumerator(ulong key)
        {
            _key = key;
            _nextChunk = 0;
            _values = default;
            _row = -1;
        }

        /// <summary>Return the current writable component row.</summary>
        public WriteRow<T> Current => new(_values, _row);

        /// <summary>Advance within this column or borrow the next column.</summary>
        public bool MoveNext()
        {
            if (++_row < _values.Length)
                return true;

            while (Engine.TryGetChunk<T>(_key, 1, _nextChunk++, out _, out _values))
            {
                if (_values.Length == 0)
                    continue;
                _row = 0;
                return true;
            }
            return false;
        }
    }
}

// =============================================================================
// Write/Read Query
// =============================================================================

/// <summary>
/// A system parameter granting writable access to <typeparamref name="TWrite"/>
/// and read-only access to <typeparamref name="TRead"/>.
/// </summary>
public sealed class WriteReadQuery<TWrite, TRead>
    where TWrite : unmanaged
    where TRead : unmanaged
{
    // Instances are created by GameHost while compiling a managed system.
    internal WriteReadQuery() { }

    /// <summary>Create an iterator after validating distinct component types.</summary>
    public Enumerator GetEnumerator()
    {
        ulong writeKey = Engine.ComponentKey(typeof(TWrite));
        ulong readKey = Engine.ComponentKey(typeof(TRead));
        if (writeKey == readKey)
            throw new InvalidOperationException(
                "A query cannot borrow the same component as both writable and read-only.");
        return new Enumerator(writeKey, readKey);
    }

    public ref struct Enumerator
    {
        private readonly ulong _writeKey;
        private readonly ulong _readKey;
        private uint _nextWriteChunk;
        private Span<TWrite> _writes;
        private Span<TRead> _reads;
        private int _row;

        internal Enumerator(ulong writeKey, ulong readKey)
        {
            _writeKey = writeKey;
            _readKey = readKey;
            _nextWriteChunk = 0;
            _writes = default;
            _reads = default;
            _row = -1;
        }

        /// <summary>Return the current joined component row.</summary>
        public WriteReadRow<TWrite, TRead> Current => new(_writes, _reads, _row);

        /// <summary>Advance to the next row shared by both columns.</summary>
        public bool MoveNext()
        {
            if (++_row < _writes.Length)
                return true;

            while (Engine.TryGetChunk<TWrite>(
                _writeKey, 1, _nextWriteChunk++, out var writeArchetype, out _writes))
            {
                uint readChunk = 0;
                while (Engine.TryGetChunk<TRead>(
                    _readKey, 0, readChunk++, out var readArchetype, out _reads))
                {
                    if (readArchetype != writeArchetype)
                        continue;
                    if (_reads.Length != _writes.Length)
                        throw new InvalidOperationException("ECS component chunk lengths are inconsistent.");
                    if (_writes.Length == 0)
                        break;
                    _row = 0;
                    return true;
                }
            }
            return false;
        }
    }
}

// =============================================================================
// Three-Write Query
// =============================================================================

/// <summary>A system parameter granting writable access to three components.</summary>
public sealed class Write3Query<TFirst, TSecond, TThird>
    where TFirst : unmanaged
    where TSecond : unmanaged
    where TThird : unmanaged
{
    // Instances are created by GameHost while compiling a managed system.
    internal Write3Query() { }

    /// <summary>Create an iterator after validating three distinct components.</summary>
    public Enumerator GetEnumerator()
    {
        ulong first = Engine.ComponentKey(typeof(TFirst));
        ulong second = Engine.ComponentKey(typeof(TSecond));
        ulong third = Engine.ComponentKey(typeof(TThird));
        if (first == second || first == third || second == third)
            throw new InvalidOperationException("A writable query cannot borrow a component more than once.");
        return new Enumerator(first, second, third);
    }

    /// <summary>Joins writable and read-only columns by archetype identity.</summary>
    /// <summary>Joins three writable columns by archetype identity.</summary>
    public ref struct Enumerator
    {
        private readonly ulong _firstKey;
        private readonly ulong _secondKey;
        private readonly ulong _thirdKey;
        private uint _nextFirstChunk;
        private Span<TFirst> _first;
        private Span<TSecond> _second;
        private Span<TThird> _third;
        private int _row;

        internal Enumerator(ulong firstKey, ulong secondKey, ulong thirdKey)
        {
            _firstKey = firstKey;
            _secondKey = secondKey;
            _thirdKey = thirdKey;
            _nextFirstChunk = 0;
            _first = default;
            _second = default;
            _third = default;
            _row = -1;
        }

        /// <summary>Return the current three-component row.</summary>
        public Write3Row<TFirst, TSecond, TThird> Current => new(_first, _second, _third, _row);

        /// <summary>Advance to the next row shared by all three columns.</summary>
        public bool MoveNext()
        {
            if (++_row < _first.Length)
                return true;

            while (Engine.TryGetChunk<TFirst>(
                _firstKey, 1, _nextFirstChunk++, out var firstArchetype, out _first))
            {
                uint secondChunk = 0;
                while (Engine.TryGetChunk<TSecond>(
                    _secondKey, 1, secondChunk++, out var secondArchetype, out _second))
                {
                    if (secondArchetype != firstArchetype)
                        continue;
                    uint thirdChunk = 0;
                    while (Engine.TryGetChunk<TThird>(
                        _thirdKey, 1, thirdChunk++, out var thirdArchetype, out _third))
                    {
                        if (thirdArchetype != firstArchetype)
                            continue;
                        if (_second.Length != _first.Length || _third.Length != _first.Length)
                            throw new InvalidOperationException("ECS component chunk lengths are inconsistent.");
                        if (_first.Length == 0)
                            break;
                        _row = 0;
                        return true;
                    }
                }
            }
            return false;
        }
    }
}
