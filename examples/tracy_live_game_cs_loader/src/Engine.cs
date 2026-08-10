using System.Text;

namespace TracyLive;

/// <summary>Marks a static method for automatic ECS system discovery.</summary>
[AttributeUsage(AttributeTargets.Method)]
public sealed class EcsSystemAttribute : Attribute;

/// <summary>
/// Safe facade over the type-erased native ECS query API. Queries are created
/// by the loader from an <see cref="EcsSystemAttribute"/> method's parameter;
/// gameplay code never separately declares its component accesses.
/// </summary>
public static unsafe class Engine
{
    private static EngineApi _api;

    public static void Bind(EngineApi* api) => _api = *api;
    public static void Bind(IntPtr api) => Bind((EngineApi*)api);

    public static uint EntityCount() => _api.EntityCount();

    internal static ulong ComponentKey(Type type) => HashName(type.Name);

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

internal readonly record struct ArchetypeKey(ulong Low, ulong High);

public ref struct WriteRow<T> where T : unmanaged
{
    private Span<T> _values;
    private int _index;

    internal WriteRow(Span<T> values, int index)
    {
        _values = values;
        _index = index;
    }

    public ref T Write => ref _values[_index];
}

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

    public ref TWrite Write => ref _writes[_index];
    public ref readonly TRead Read => ref _reads[_index];
}

/// <summary>A system parameter granting writable access to one component.</summary>
public sealed class WriteQuery<T> where T : unmanaged
{
    internal WriteQuery() { }

    public Enumerator GetEnumerator() => new(Engine.ComponentKey(typeof(T)));

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

        public WriteRow<T> Current => new(_values, _row);

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

/// <summary>
/// A system parameter granting writable access to <typeparamref name="TWrite"/>
/// and read-only access to <typeparamref name="TRead"/>.
/// </summary>
public sealed class WriteReadQuery<TWrite, TRead>
    where TWrite : unmanaged
    where TRead : unmanaged
{
    internal WriteReadQuery() { }

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

        public WriteReadRow<TWrite, TRead> Current => new(_writes, _reads, _row);

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
