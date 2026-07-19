# Binary Size 101 - Understanding and Reducing Rust Release Binary Size

A comprehensive, practical guide for Rust developers who want to understand
what goes into their release binaries and how to keep them small.

**Audience:** Rust developers familiar with Cargo and `--release` builds.
**Scope:** Rust-native binaries (ELF, PE, Mach-O). Does not cover WASM or embedded in depth, though many principles transfer.
**Rust edition:** 2021 and later. Examples assume Rust 1.95+.

---

## Table of Contents

1. [What "Binary Size" Means](#1-what-binary-size-means)
2. [What Source Code Increases Binary Size](#2-what-source-code-increases-binary-size)
3. [Common Dependency-Level Offenders](#3-common-dependency-level-offenders)
4. [Compiler and Release-Profile Controls](#4-compiler-and-release-profile-controls)
5. [Relationships and Misconceptions](#5-relationships-and-misconceptions)
6. [Investigation Workflow](#6-investigation-workflow)
7. [Case Study: Applying the Workflow to a Real Crate](#7-case-study-applying-the-workflow-to-a-real-crate)

---

## 1. What "Binary Size" Means

### 1.1 Unstripped vs Stripped

| Variant | What it contains | Typical size factor |
|---------|-----------------|---------------------|
| **Unstripped** | Code + data + debug info (DWARF/PDB) + symbol table + relocation info | 2–10× stripped |
| **Stripped** | Code + data only; symbols and debug info removed | 1× (baseline) |

Setting `strip = true` in `[profile.release]` removes debug info and symbol tables. Most release profiles should enable this unless you need symbols for profiling.

### 1.2 Binary Sections

Every native executable is divided into sections. On Windows PE (`ecs_hybrid.exe`):

```
$ llvm-size target\release\ecs_hybrid.exe
   text    data     bss     dec     hex filename
 358297   74830       0  433127   69be7 target\release\ecs_hybrid.exe
```

| Section | Size | Meaning |
|---------|------|---------|
| **`.text`** | 358 KB (82.7%) | Executable machine code: functions, generic instantiations, inlined code |
| **`.data` / `.rdata`** | 73 KB (17.3%) | Read-only data: string literals, `static` values, const data, vtable data, panic messages |
| **`.bss`** | 0 | Zero-initialised writable static data (allocated at load time, not stored on disk) |

On Linux ELF, the sections are similar but use different names: `.text`, `.rodata`, `.data`, `.bss`.

### 1.3 On-Disk, Compressed, Installed, and Runtime Sizes

| Measure | Typical (small Rust CLI) | How to measure |
|---------|--------------------------|----------------|
| **On-disk (stripped)** | 200–800 KB | `ls -l` / `Get-Item` |
| **On-disk (unstripped)** | 1–5 MB | Build without `strip = true` |
| **Compressed (`.tar.gz`)** | 30–50% of on-disk | `tar czf` / `Compress-Archive` |
| **Compressed (`.tar.zst`)** | 35–55% of on-disk | `zstd` |
| **Installed / deployed** | Same as on-disk | Copy the file |
| **Runtime memory** | Varies by workload | OS-specific tools (`taskmgr`, `top`, `ps`) |

- **Compressed distribution size** matters for downloads (crates.io, containers, embedded OTA).
- **Installed size** matters for disk-constrained environments (embedded, containers).
- **Runtime memory** is NOT the same as binary size. A 100 KB binary can allocate gigabytes at runtime. Conversely, a 100 MB binary with many unused pages may consume little RSS if pages are never faulted in.

### 1.4 Static vs Dynamic Linking

| Approach | Binary size | Deployment | Runtime |
|----------|------------|------------|---------|
| **Static** (`rustc` default) | Larger (includes all Rust code) | Single file, self-contained | No external Rust runtime |
| **Dynamic** (`cdylib` + `dylib`) | Smaller binary + separate `.so`/`.dll` | Multiple files | Requires matching library versions |
| **System libraries** (libc, OpenSSL) | Can link dynamically on Linux | OS-provided | Depends on OS version |

Rust defaults to static linking for Rust dependencies. The C runtime (libc on Linux, msvcrt on Windows) is typically linked dynamically by the system linker. On Windows with MSVC, the VCRuntime is a DLL (`vcruntime140.dll`) unless you use `-C target-feature=+crt-static`.

### 1.5 Platform, Target Triple, and Format Differences

| Platform | Target triple | Format | Notes |
|----------|--------------|--------|-------|
| Windows | `x86_64-pc-windows-msvc` | PE/COFF | `.exe` / `.dll` |
| Linux | `x86_64-unknown-linux-gnu` | ELF | No extension by convention |
| macOS | `x86_64-apple-darwin` | Mach-O | `.dylib` for dynamic libs |
| WASM | `wasm32-unknown-unknown` | Wasm | Stripped via `wasm-opt` |

Different platforms and linkers produce different binary overhead. ELF typically adds a few KB of section headers; PE adds a DOS stub and PE header (~512 bytes + section table). The **same Rust code** can produce binaries that differ by 5–15% across platforms due to linker differences, CRT code, and ABI conventions.

### 1.6 Debug vs Release

| Profile | Typical relative size | Notes |
|---------|----------------------|-------|
| `debug` (dev) | 3–10× release | Full debug info, no optimisations |
| `release` (default) | 1× (baseline) | `opt-level=3`, no LTO, 16 CGUs |
| `release` (tuned) | 0.4–0.8× default | `opt-level="s"`, `lto=true`, `codegen-units=1`, `strip=true`, `panic="abort"` |
| `release-with-debug` | 1.2–2.0× tuned | Release opts + debug symbols |

The default `release` profile is a reasonable baseline, but substantial size reductions are available by tuning a few settings. See §4 for the full menu.

### 1.7 Reproducible Measurement Commands

```bash
# Windows (PowerShell)
Get-Item target\release\ecs_hybrid.exe | ForEach-Object {
    [PSCustomObject]@{Bytes=$_.Length; KB=[math]::Round($_.Length/1KB,1)}
}

# Linux / macOS
ls -l target/release/ecs_hybrid
wc -c target/release/ecs_hybrid

# Section breakdown (llvm-size, cross-platform)
llvm-size target/release/ecs_hybrid

# Compressed size (Linux)
gzip -c target/release/ecs_hybrid | wc -c

# Compressed size (Windows PowerShell)
$bytes = [IO.File]::ReadAllBytes("target\release\ecs_hybrid.exe")
$ms = New-Object IO.MemoryStream
$gz = New-Object IO.Compression.GZipStream($ms, [IO.Compression.CompressionMode]::Compress)
$gz.Write($bytes, 0, $bytes.Length); $gz.Close()
$ms.Length
```

---

## 2. What Source Code Increases Binary Size

### 2.1 Monomorphization: Generics Instantiated Many Times

Every unique combination of concrete type parameters produces a separate copy of the generic function in the binary. This is called **monomorphization**.

```rust
// ONE copy in binary
fn sort_i32(data: &mut [i32]) { data.sort(); }

// TWO copies: sort_by_key::<i32, _> and sort_by_key::<String, _>
fn sort_both(ints: &mut [i32], strs: &mut [String]) {
    ints.sort_by_key(|x| *x);
    strs.sort_by_key(|s| s.len());
}
```

**Detection:** `cargo bloat --release -n 30` shows identical-looking function names with different generic parameters. `cargo llvm-lines` (install via `cargo install cargo-llvm-lines`) shows which generic functions generate the most LLVM IR.

**Real-world example:** An ECS (Entity Component System) query engine typically implements a `QueryTarget` trait for `&T`, `&mut T`, `Entity`, and tuples up to arity 4 or 5, plus a `QueryFilter` trait for `()`, `With<T>`, `Without<T>`, `Changed<T>`, `Added<T>`, and `Or<...>`. Each concrete query a user writes creates new monomorphised code. In a typical ECS binary, system-closure instantiations can account for 20–50 KB across 5–10 variants.

**Mitigations:**

| Technique | Size impact | Performance impact | Readability |
|-----------|------------|-------------------|-------------|
| Use trait objects (`dyn`) instead of generics for rarely-called paths | ↓ | ↓ (vtable dispatch) | ↔ |
| Extract non-generic inner functions | ↓↓ | ↔ | ↑ |
| Use `#[inline(never)]` on cold generic paths | ↓ | ↔ (if truly cold) | ↔ |
| Limit tuple arity (macro expansion) | ↓ | ↔ | ↓ (less flexible) |

### 2.2 Large Generic APIs and Cross-Crate Monomorphization

When a generic function in crate A is called from crate B with crate C's types, all three crates' code interacts during monomorphization. LLVM must generate and potentially inline code across crate boundaries.

```rust
// Crate A (utility library)
pub fn process<T: Display>(items: &[T]) { /* ... */ }

// Crate B (your code) - generates copy of `process::<MyType>`
process(&my_items);
```

**Real-world example:** A type-erased container like `TypeMap<Trait>` is generic over the stored trait object type. Each `register::<ConcreteType>()` call generates a monomorphic copy of the registration and accessor functions. With three stored types, that is three copies - typically a few KB each.

### 2.3 Trait Objects vs Static Dispatch

```rust
// Static dispatch: monomorphised at each call site
fn process_static(items: &[impl Display]) { /* one copy per type */ }

// Dynamic dispatch: single function, vtable lookup at runtime
fn process_dyn(items: &[&dyn Display]) { /* one copy total */ }
```

- **Static dispatch**: larger binary, faster (no indirection), inlinable
- **Dynamic dispatch**: smaller binary, slightly slower (pointer chase), opaque to optimizer

In hot loops, prefer static dispatch. In cold paths (error handling, configuration, rare code paths), `dyn` can save significant space.

### 2.4 Inlining: `#[inline]`, `#[inline(always)]`, and Heuristic Inlining

LLVM decides when to inline based on heuristics (function size, call frequency, optimisation level). Attributes override the heuristics:

```rust
#[inline]         // Hint: inline across crate boundaries (moderate)
#[inline(always)] // Force: always inline (very aggressive - can bloat)
#[inline(never)]  // Force: never inline (saves size, can hurt perf)
```

**Over-inlining symptoms:**
- A small function appears in `cargo bloat` as many separate copies
- Functions with identical assembly appear under different names
- IPC (instructions per cycle) is good but binary is unexpectedly large

**Guidance:** `opt-level=3` with `lto="thin"` provides aggressive but not reckless inlining. Small accessor functions like iterator `next()` or container `get()` are typically inlined at each call site - this is usually the right trade-off for performance-sensitive code. If a particular generic function shows up as many separate copies in `cargo bloat` and the copies are identical, try `#[inline(never)]` on its cold-path variant.

### 2.5 Iterator Chains, Closures, and Combinators

Each closure has a unique anonymous type, even if the body is identical:

```rust
// TWO distinct closure types -> TWO copies of .map()
vec![1,2,3].iter().map(|x| x + 1).collect::<Vec<_>>();
vec![4,5,6].iter().map(|x| x + 1).collect::<Vec<_>>();
```

Long iterator chains (`filter().map().flat_map().collect()`) produce deeply nested types. The compiler generates code for each intermediate adaptation step.

**Mitigation:** extract repeated iterator logic into named functions, or use `for` loops for simple cases (same performance, often smaller code).

### 2.6 Async Functions and Generated State Machines

Every `async fn` compiles to a state machine struct whose size is proportional to the data held across `.await` points. Deeply nested futures produce large state machines:

```rust
async fn big() {
    let data = vec![0u8; 10_000];   // Lives across await - copied into state machine
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("{}", data.len());
}
```

If your application does not use async, ensure no dependency accidentally pulls in an async runtime. Tools like `cargo tree -i tokio` can reveal unexpected async dependencies. The `futures-*` family of crates is lightweight (~5 KB) on their own; the runtime is what adds bulk.

### 2.7 Macro Expansion and Repeated Generated Code

Macros expand at each invocation site. If a macro generates a 200-line function and is invoked 20 times, that is 4,000 lines of LLVM IR before inlining:

```rust
macro_rules! register_component {
    ($world:expr, $t:ty) => {
        $world.register_component::<$t>();
        // ... 20 more lines of setup code ...
    };
}
// Three invocations -> three copies of the expanded block
register_component!(world, Position);
register_component!(world, Velocity);
register_component!(world, Health);
```

**Mitigation:** extract the body of a macro into a non-generic helper function and keep the macro as a thin wrapper that calls it.

### 2.8 Derive Macros (serde, Debug, Clone, etc.)

Each `#[derive(Serialize, Deserialize)]` generates trait implementations that include reflection data, field-name strings, and serialization logic. For 20 types, this can add hundreds of KB:

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
struct LargeType { /* 30 fields */ }
// Generates: Serialize impl, Deserialize impl, Debug impl, Clone impl
// Each impl contains field-name strings and visitor code
```

`Debug` and `Clone` derives add a few hundred bytes per type - negligible for most applications. `serde`'s `Serialize`/`Deserialize` derives, by contrast, add several KB per type plus the format-specific machinery. If you use `serde` on many types, consider compile-time serialization (`rkyv`, `postcard`) or hand-written impls for the hottest types.

### 2.9 Formatting Machinery (`format!`, `println!`, `Display`/`Debug`)

Every unique format string and its argument types generate formatting code:

```rust
println!("Entity {} has position ({}, {})", id, x, y);
// Generates: Arguments struct + format implementation for (u64, f32, f32)
```

Widely-used `Display` and `Debug` implementations (especially recursive ones for large types) accumulate. The standard library's formatting infrastructure is shared, but the per-invocation glue adds up.

A single function that does many `println!`/`format!` calls with different types can easily reach 20–30 KB. For libraries where the binary is only an example/demo, this is harmless. For a CLI tool, consider whether all those format paths are truly needed in the release binary.

### 2.10 Large Error Enums and Error-Context Strings

```rust
#[derive(Debug)]
enum MyError {
    Io(std::io::Error),               // 16+ bytes
    Parse(String, usize),              // 24+ bytes + heap
    Network { code: u16, msg: String }, // 24+ bytes + heap
}
```

Large error enums carry embedded context strings. Libraries like `anyhow` and `thiserror` add backtrace capture and error-context formatting. Standard library `Error` trait implementations add vtable entries.

Small, hand-written error enums with 1–3 variants add negligible size. The `thiserror` crate adds 5–10 KB for the derive macro infrastructure; `anyhow` adds 20–50 KB plus backtrace support. For size-sensitive applications, prefer simple enums with manual `Display` and `Error` impls.

### 2.11 Panic Paths, Panic Messages, and Unwinding

`panic!` and `assert!` generate:
- A format string (stored in `.rodata`)
- The format arguments
- An unwind path (landing pad)

With `panic = "abort"`, unwinding support is removed. The panic handler still exists (it calls `core::intrinsics::abort`) but the unwind tables and landing pads are eliminated. This typically saves 5–15% of binary size.

`debug_assert!` is completely removed in release builds - zero binary cost. Prefer `debug_assert!` over `assert!` for invariants that need not be checked in production.

### 2.12 Logging and Profiling Call Sites

Logging macros (`log::info!`, `profiling::info!`) compile to:
- Disabled-at-compile-time (feature off): zero binary cost
- Disabled-at-runtime (filtered out): a branch + the format arguments (the string and args remain in the binary)
- Enabled: full formatting code

To completely remove logging from release builds without changing source:
```toml
[profile.release]
# log and profiling compile-time filtering
rustflags = ["--cfg", "tokio_unstable"]  # example
```

Or use the `log` crate's `max_level_*` features and `profiling`'s `max_level_*` features.

If you use the `log` or `profiling` crates, check whether compile-time filters (`max_level_*` features) can remove unwanted levels from the release binary. A `trace!` call that is compiled out costs nothing; one that is compiled in but filtered at runtime still carries its format string and argument types.

### 2.13 Serialization and Deserialization

`serde` + `serde_json` / `bincode` / `postcard` generate substantial code:
- Serialize/Deserialize impls per type
- Visitor patterns for each format
- Format-specific machinery (JSON escaping, CBOR tagging, etc.)

Even the compact `postcard` format adds several KB per serialized type.

Dev-dependencies like `criterion` or `proptest` pull in heavyweight crates (`serde`, `regex`, `clap`) - but these are NOT linked into the release binary. Verify with `cargo tree -e no-dev` that your production dependency graph is clean.

### 2.14 Regex Engines, Unicode Tables, Parsers

The `regex` crate bundles Unicode tables (~100 KB) unless you disable Unicode support:
```toml
regex = { version = "1", default-features = false, features = ["std"] }
```

Parsers generated by `nom`, `pest`, `combine`, or `chumsky` produce code proportional to grammar complexity. `serde` deserialization is also a form of parser.

`regex` is often pulled in by dev-dependencies (e.g., `criterion` for benchmark name matching). Verify with `cargo tree -i regex` that it is not in your production dependency graph.

### 2.15 Embedded Data: `include_bytes!`, `include_str!`

```rust
static FONT: &[u8] = include_bytes!("font.ttf");  // 200 KB embedded
static CERT: &str = include_str!("ca.pem");         // 5 KB embedded
```

These become part of `.rodata`. Every additional file increases binary size by its exact byte count (plus alignment padding).

Run `cargo bloat --release --filter ".rodata"` or inspect the `.data` section size with `llvm-size` to check for unexpectedly large data sections - a common sign of embedded blobs.

### 2.16 Large Constants and Static Data

```rust
const LOOKUP: [f32; 65536] = [/* ... */];  // 256 KB in .rodata
static CACHE: LazyLock<HashMap<...>> = LazyLock::new(|| /* ... */);
```

Large `const` arrays generate data sections. Large `static` values with `LazyLock` store initialisation code plus the final data.

### 2.17 Repeated String Literals

Identical string literals are typically merged by the linker (string merging / identical code folding). However, strings embedded in different generic instantiations may not be merged. Use `&'static str` constants for repeated diagnostic text.

### 2.18 FFI and Statically Linked Native Libraries

`#[link(name = "foo", kind = "static")]` embeds the entire native library. Build scripts (`build.rs`) that compile C/C++ code with `cc` produce additional `.text` and `.data`.

Be aware that some Rust crates wrap native C libraries and link them statically by default (e.g., `openssl-sys`, `libsqlite3-sys`). Check `Cargo.lock` for `*-sys` crates and review their linkage settings.

### 2.19 Multiple Versions of the Same Dependency

When crate A depends on `serde 1.0` and crate B depends on `serde 1.3`, Cargo unifies them at `1.3` (semver-compatible). When two dependencies require incompatible versions (`serde 1.0` and `serde 0.9`), Cargo includes **both**, roughly doubling the size contribution.

**Detection:** `cargo tree -d` lists duplicate packages.

Run `cargo tree -d` after adding or updating dependencies. Duplicate versions are easiest to fix early, before they become entrenched in the dependency graph.

### 2.20 Feature-Gated Code and Cargo Default Features

Many crates enable optional features by default. For example, `regex` enables `unicode` by default (pulls in Unicode tables). `tokio` enables the multi-threaded runtime by default.

**Investigation:** `cargo tree -e features` shows which features are active.

Audit the default features of every direct dependency. Common savings:
- `regex`: disable `unicode` if you only need ASCII patterns (~100 KB saved)
- `tokio`: disable `full`, enable only `rt` + `sync` + `net` as needed
- `serde`: disable `derive` if you hand-write impls, though this is rarely worth it
- `reqwest`: disable `default-tls` and enable `rustls-tls` to avoid OpenSSL

### 2.21 Allocator Choice

Rust uses the system allocator by default. On Windows with MSVC, this is the Windows heap allocator. On Linux with GNU, this is glibc's `malloc`. Custom allocators (`jemalloc`, `mimalloc`, `snmalloc`) add their own code (typically 50–200 KB) but can improve runtime performance.

### 2.22 Nuances and Common Misconceptions

- **Source file count does not inherently increase binary size.** Dead code is eliminated. Module structure is erased during compilation.
- **Lines of code are not a reliable predictor.** A single `include_bytes!` can outweigh 100,000 lines of abstract logic.
- **Procedural macros and build scripts** mostly affect compilation time, not binary size. They produce tokens that are compiled normally. Only their *generated output* contributes to binary size.
- **Abstractions are not automatically expensive.** `Iterator::map` with a simple closure compiles to the same machine code as a `for` loop. Zero-cost abstractions are real - but monomorphization can create many copies.
- **Dynamic dispatch reduces monomorphization** at the cost of vtable indirection and lost inlining opportunities. Use it purposefully for cold code paths.
- **Inlining is a double-edged sword.** It removes function-call overhead but duplicates code. Sometimes inlining a small function enables the optimizer to eliminate large amounts of surrounding code, *reducing* total size.
- **A large dependency repository does not mean a large binary.** Only the code reachable from your crate's public API and `main` is linked. Large crates often have most of their code behind features or in modules you never import.
- **Tests, examples, and benchmarks** do not become part of the main release executable. They are separate binaries or linked only in `cfg(test)`.
- **Generics instantiated with the same concrete types across crates** are deduplicated by the linker when LTO is enabled.

---

## 3. Common Dependency-Level Offenders

### 3.1 Async Runtimes

| Runtime | Typical size contribution | Notes |
|---------|--------------------------|-------|
| `tokio` (full) | 300–800 KB | Multi-threaded + I/O + timers + sync |
| `tokio` (minimal) | 50–150 KB | `rt` only, single-threaded |
| `async-std` | 200–500 KB | |
| `smol` | 30–80 KB | Lightweight |
| `embassy` | 5–20 KB | Embedded, no_std |

Mitigation: Use only the features you need. `tokio = { version = "1", default-features = false, features = ["rt", "sync"] }`.



### 3.2 TLS and Cryptography

OpenSSL (`openssl-sys`) statically links ~1–5 MB of C code. `rustls` + `ring` are pure Rust but still add 200–500 KB. `rustls` with `aws-lc-rs` is similarly sized.

Mitigation: Use platform-native TLS where possible (`schannel` on Windows, `Security.framework` on macOS), or `rustls` with minimal cipher suites.



### 3.3 HTTP Clients and Servers

`reqwest` (with default features) pulls in `tokio`, `hyper`, `h2`, `rustls`/`native-tls`, and can add 500 KB–2 MB. `hyper` alone (HTTP only) is 100–300 KB. `ureq` (blocking, minimal) is 50–100 KB.



### 3.4 Unicode-Aware Processing

`unicode-normalization`, `unicode-segmentation`, `icu_*` crates carry Unicode data tables. The `regex` crate's `unicode` feature adds ~100 KB of Unicode character-class tables.



### 3.5 Regex Engines

`regex` with `unicode` and `perf` features: ~200 KB. Without Unicode: ~50 KB. The `regex-lite` crate (no Unicode, smaller, slower) is ~15 KB.

`regex` is often pulled in by dev-dependencies (e.g., `criterion` for benchmark name matching). Verify with `cargo tree -i regex` that it is not in your production dependency graph.

### 3.6 Serialization Frameworks

| Format | Size (per type) | Total dependency size |
|--------|----------------|----------------------|
| `serde` (traits only) | ~0 | ~10 KB |
| `serde_json` | 5–30 KB per type | ~100 KB total |
| `bincode` | 1–5 KB per type | ~20 KB total |
| `postcard` | 0.5–2 KB per type | ~10 KB total |
| `rkyv` (zero-copy) | 2–10 KB per type | ~30 KB total |
| `prost` (protobuf) | 10–50 KB per message | ~100 KB total |



### 3.7 CLI Frameworks

`clap` with derive: 50–200 KB. `bpaf`: 20–50 KB. `lexopt`: 5–10 KB.



### 3.8 Logging, Profiling, Backtrace, Error-Reporting

`log` + `env_logger`: ~30 KB. `profiling` + `profiling-subscriber`: 50–150 KB. `anyhow`: 20–50 KB. `thiserror`: 5–10 KB. `color-eyre`: 100–200 KB. Backtrace capture (`backtrace` crate): 50–100 KB.



### 3.9 Database Drivers

`sqlx` (compile-time checked): 200–500 KB plus TLS. `rusqlite` (bundles SQLite): ~1 MB. `postgres`: 200–400 KB plus TLS. `redis`: 50–150 KB.



### 3.10 Compression and Image Libraries

`flate2` (miniz_oxide, pure Rust): ~30 KB. `zstd`: ~100 KB. `image` crate: 200–500 KB (codecs + color types). `png`: 30–50 KB. `jpeg-decoder`: 50–80 KB.



### 3.11 How to Investigate Dependency Features

```bash
# Show active features for all dependencies
cargo tree -e features

# Find packages that appear in multiple versions
cargo tree -d

# Show why a specific dependency is included
cargo tree -i regex

# Estimate per-crate code size
cargo bloat --release --crates
```

### 3.12 Building Your Own Dependency Profile

Use `cargo bloat --release --crates` to produce a table like the one below for your own project. Then work through each line:

| Step | Question |
|------|----------|
| 1. Identify | Which crate contributes the most to `.text` after `std` and your own code? |
| 2. Justify | Is that crate pulling its weight? Could a lighter alternative work? |
| 3. Feature-audit | Are all of the crate's default features needed? (`cargo tree -e features -p <crate>`) |
| 4. Deduplicate | Does the same functionality appear in multiple crates? |
| 5. Replace | For each heavy crate, list 1–2 lighter alternatives and estimate the savings. |

**Example workflow on a real crate:**

```bash
# Generate the profile
cargo bloat --release --crates -n 20 > profile.txt

# For each suspicious crate, trace why it is included
cargo tree -i suspicious_crate

# Check if features can be pruned
cargo tree -e features -p suspicious_crate
```

A typical small-to-medium Rust CLI or library binary (without async, TLS, or serialization) can land between 200 KB and 1 MB after tuning. If you are above 2 MB, there is almost certainly low-hanging fruit in your dependency or feature configuration.

---

## 4. Compiler and Release-Profile Controls

### 4.1 `opt-level`

| Setting | Binary size | Compile time | Runtime speed |
|---------|------------|-------------|---------------|
| `0` (debug) | Largest (no optimisation) | Fastest | Slowest |
| `1` | Moderate | Moderate | Moderate |
| `2` (release default) | Smaller | Slower | Fast |
| `3` | Smallest (most aggressive) | Slowest | Fastest (usually) |
| `"s"` | Smaller (optimise for size) | Similar to 2 | Slightly slower than 3 |
| `"z"` | Smallest (aggressive size) | Similar to 2 | Slower than "s" (disables loop vectorisation) |

```toml
[profile.release]
opt-level = 3    # Default release: balanced speed+size
# opt-level = "s"  # ~5-15% smaller, 2-5% slower
# opt-level = "z"  # ~10-25% smaller, 5-15% slower (disables loop vectorisation)
```

`opt-level = 3` is the default for `[profile.release]` and prioritises speed. `"s"` optimises for size while retaining most optimisations - typically 5–15% smaller with 2–5% speed reduction. `"z"` goes further, disabling loop vectorisation, for an additional 3–8% size reduction but a more noticeable 5–15% speed penalty in vectorisable loops.

### 4.2 LTO (Link-Time Optimisation)

| Setting | Binary size reduction | Link time increase | Notes |
|---------|----------------------|-------------------|-------|
| `false` (off) | 0% (baseline) | Fastest | No cross-crate optimisation |
| `"thin"` | 10–30% smaller | 2–3× slower | Good balance, stable |
| `"fat"` | 15–40% smaller | 5–20× slower | Single LLVM module, more RAM |
| `"fat"` + `codegen-units = 1` | Max reduction | Slowest link | Best for release |

```toml
[profile.release]
lto = "thin"    # Good balance: cross-crate inlining, moderate link time
# lto = "fat"    # Maximum reduction: whole-program optimisation, slow link
```

`lto = "thin"` is a good default for release builds - it enables cross-crate inlining with modest link-time overhead. `"fat"` performs a single whole-program optimisation pass and can save an additional 5–15% over thin LTO, but link time grows significantly (5–20× for large projects). For binaries under 1 MB, the absolute link time is still seconds, making fat LTO a practical choice for final release builds.

### 4.3 `codegen-units`

| Setting | Binary size | Compile time | Notes |
|---------|------------|-------------|-------|
| 16 (default) | Largest | Parallel compilation | One LLVM module per CGU |
| 1 | Smallest | Serial LLVM codegen | Best optimisation, worst compile time |

```toml
[profile.release]
codegen-units = 1   # Single LLVM module: best optimisation and size reduction
```

`codegen-units = 1` is the single most important setting for minimising release binary size. With the default of 16 CGUs, the compiler cannot inline or eliminate dead code across codegen-unit boundaries. The trade-off is that compilation becomes single-threaded during LLVM codegen - but for release builds, the size and performance gains are almost always worth it.

### 4.4 `panic`

| Setting | Binary size | Runtime behaviour |
|---------|------------|-------------------|
| `"unwind"` (default) | Larger (unwind tables + landing pads) | `catch_unwind` works |
| `"abort"` | Smaller (5–15%) | Process terminates on panic |

```toml
[profile.release]
panic = "abort"    # No unwind tables: saves 5-15% binary size
```

`panic = "abort"` is safe for most applications - panics represent programmer errors that should terminate the process. If your application uses `catch_unwind` (e.g., to prevent panics in a thread from taking down the whole process, or in a web server to keep serving after a handler panics), you must keep `panic = "unwind"`. Unwinding is also required if you link against C++ code that uses exceptions.

### 4.5 `strip`

| Setting | Effect |
|---------|--------|
| `"none"` (default) | Keeps debug info, symbol table |
| `"debuginfo"` | Removes DWARF/PDB, keeps symbol names |
| `"symbols"` | Removes both debug info and symbol table |
| `true` | Equivalent to `"symbols"` |

```toml
[profile.release]
strip = true    # Remove debug info and symbol table
```

**Caveat:** Stripping removes symbol names, making profiling output (perf, Instruments) show addresses instead of function names. For profiling builds, use a separate profile that inherits from `release` but disables stripping:

```toml
[profile.release-with-debug]
inherits = "release"
strip = "none"
debug = 1
```

Note: if the parent profile sets `strip = true`, the child must explicitly override it with `strip = "none"` - profile inheritance merges settings, and explicit keys in the child take precedence.

### 4.6 Debug Information Settings

| Setting | Binary size impact | Notes |
|---------|-------------------|-------|
| `debug = false` | None (no debug info) | Default for `release` |
| `debug = true` | +30–200% | Full DWARF/PDB |
| `debug = 1` | +10–50% | Line tables only (no type info) |
| `debug = 2` | +20–100% | Line tables + type info |

### 4.7 Incremental Compilation

`incremental = true` (default in dev) improves compile time at the cost of slightly larger binaries. Always `false` (or unset, which defaults to false) in `release`.

### 4.8 Linker Garbage Collection (`--gc-sections`)

The linker removes unreferenced sections. This is the linker-level complement to Rust's dead-code elimination. On Linux, this is automatic with LTO. On Windows, enable via:

```toml
[profile.release]
link-args = ["/OPT:REF", "/OPT:ICF"]
```

`/OPT:REF` removes unreferenced functions and data. `/OPT:ICF` performs identical code folding (merges functions with identical machine code into one copy). Both are generally on by default for MSVC release builds.

### 4.9 Target CPU and `target-feature`

```toml
[profile.release]
# Reduces binary size by not including instruction-set-specific code paths
# (useful for distribution, but may reduce performance)
rustflags = ["-C", "target-cpu=generic"]
```

Specifying `target-cpu=native` enables all CPU features of the build machine, which can *increase* binary size (multiple SIMD code paths) but improves performance. For distribution binaries, leave at `generic`.

### 4.10 Nightly: `build-std`

On nightly Rust, you can rebuild the standard library with custom profiles:

```toml
# .cargo/config.toml (requires nightly)
[unstable]
build-std = ["std", "panic_abort"]
build-std-features = ["panic_immediate_abort"]

[profile.release]
# Applied to std as well
opt-level = "z"
lto = "fat"
```

This can reduce `std`'s contribution from 148 KB to ~80–100 KB by applying `opt-level="z"` and removing panic-formatting strings. Requires nightly. Marked unstable - may break.

### 4.11 Platform-Specific Linker Options

**Linux:**
```toml
[profile.release]
rustflags = ["-C", "link-arg=-Wl,--gc-sections", "-C", "link-arg=-Wl,--icf=all"]
```

**macOS:**
```toml
[profile.release]
rustflags = ["-C", "link-arg=-Wl,-dead_strip"]
```

**Windows MSVC:**
```toml
[profile.release]
rustflags = ["-C", "link-arg=/OPT:REF", "-C", "link-arg=/OPT:ICF"]
```

These are typically on by default for release builds but can be explicitly enforced.

### 4.12 Effect Summary Table

| Setting | Binary size effect | Compile time | Runtime perf | When to use |
|---------|-------------------|-------------|-------------|-------------|
| `opt-level = 3` | Baseline | Slow | Fastest | Performance-critical; ECS, game engines, numerics |
| `lto = "thin"` | -20% vs off | +2× link | Faster | Good default for release |
| `codegen-units = 1` | -10% vs 16 | +3× compile | Faster | Always for release binaries |
| `panic = "abort"` | -10% vs unwind | ↔ | ↔ | Unless you need `catch_unwind` |
| `strip = true` | -30% vs none | ↔ | ↔ | Always for distribution; use separate profile for profiling |
| `opt-level = "s"` | -10% vs 3 | Slightly faster | -2% | Good first experiment for size reduction |
| `opt-level = "z"` | -15% vs 3 | Slightly faster | -5% | Aggressive; measure vectorised loops |
| `lto = "fat"` | -5% vs thin | +3× link | Sometimes faster | Final release builds; CI nightlies |

---

## 5. Relationships and Misconceptions

### 5.1 Binary Size vs Runtime Speed

Smaller ≠ faster. A smaller binary may be slower because:
- `opt-level = "z"` disables loop vectorisation and some inlining
- Dynamic dispatch (`dyn`) is smaller but has indirection overhead
- Aggressive size optimisation can prevent the CPU's instruction cache from being used effectively

Larger ≠ faster. A larger binary may be slower because:
- More i-cache pressure (instruction cache misses)
- More TLB pressure (page table entries)
- Longer load times (especially on embedded/spinning-disk)

For binaries under ~1 MB on modern x86-64 CPUs, i-cache pressure is rarely a concern - the entire `.text` section fits in L3 cache. For larger binaries (10 MB+), instruction-cache misses can become a measurable performance issue in hot loops, and size optimisation can actually *improve* performance by keeping the hot path in L1i.

### 5.2 Binary Size vs Compilation Speed

Optimising for size almost always slows down compilation:
- `codegen-units = 1` serialises LLVM codegen
- `lto = "fat"` runs a single LLVM pass over the whole program
- `opt-level = "z"` runs additional size-focused passes

Debug builds are fast to compile and large. Release builds are slow to compile and small. CI pipelines often use separate profiles:
- `dev` for fast iteration (test, check, clippy)
- `release` for final binaries
- `release-opt-size` for size-constrained targets

### 5.3 Binary Size vs Linking Time

Linking is often the longest step in release builds:
- `lto = "fat"` makes linking 5–20× slower
- `lto = "thin"` is 2–3× slower than no LTO
- macOS `ld` is particularly slow with large binaries

For binaries under ~1 MB, linking time is negligible regardless of LTO setting. For multi-MB binaries (especially with many dependencies), fat LTO can push link times into minutes.

### 5.4 Why a Smaller Binary Is Not Necessarily Faster or More Memory-Efficient

- A binary may be small because it uses dynamic dispatch everywhere - but each call incurs a vtable lookup.
- A binary may be large because it has been aggressively inlined - but this enables further optimisations like constant folding and dead-code elimination.
- Runtime memory use depends on allocations, not binary size. A 100 KB binary that allocates 10 GB of Vecs at runtime uses 10 GB of RAM.

### 5.5 Why Results Vary Across Versions

LLVM optimisation behaviour changes between versions. Rust 1.95 uses LLVM 20. A change to `opt-level`, `lto`, or inlining heuristics in LLVM 21 may produce different results from the same source. Always record the exact Rust/LLVM version with measurements.

---

## 6. Investigation Workflow

### Step 1: Establish a Clean Baseline

```bash
# Record the build environment
rustc --version > baseline-env.txt
cargo --version >> baseline-env.txt
git rev-parse HEAD >> baseline-env.txt

# Clean and build
cargo clean
cargo build --release

# Measure
ls -l target/release/ecs_hybrid | tee baseline-size.txt
llvm-size target/release/ecs_hybrid | tee -a baseline-size.txt
```

### Step 2: Inspect Binary Sections and Symbols

```bash
# Section sizes
llvm-size target/release/ecs_hybrid

# All symbols sorted by size
nm --size-sort --radix=d target/release/ecs_hybrid | tail -30

# Or on Windows:
llvm-nm --size-sort --radix=d target/release/ecs_hybrid.exe
```

### Step 3: Find Largest Functions, Data, Crates

```bash
# Largest functions (top 30)
cargo bloat --release -n 30

# Per-crate breakdown
cargo bloat --release --crates

# Filter to a specific crate or regex
cargo bloat --release --filter ecs_hybrid::system

# LLVM IR lines (monomorphization analysis)
cargo install cargo-llvm-lines
cargo llvm-lines --release | head -30
```

### Step 4: Inspect Dependency Features and Duplicates

```bash
# Feature tree
cargo tree -e features > features.txt

# Duplicate versions
cargo tree -d

# Why is X in my build?
cargo tree -i regex

# Build plan (all packages)
cargo tree --depth 0 -e no-dev
```

### Step 5: Change One Factor at a Time

For each experiment:
1. Modify `Cargo.toml` (one profile setting, one feature, one dependency change)
2. `cargo clean && cargo build --release`
3. Record: binary size, compressed size, `llvm-size` output, `cargo bloat` output
4. Run benchmarks to check performance impact
5. Revert or keep based on data

### Step 6: Compare

```bash
# Compare two builds side by side
cargo bloat --release > after.txt
diff before.txt after.txt
```

### Step 7: CI Regression Check

Add a size budget to CI. Example (using `cargo-bloat` and `jq`):

```bash
#!/bin/bash
SIZE=$(cargo bloat --release --crates -n 0 2>&1 | grep 'file size is' | grep -oP '[\d.]+KiB')
THRESHOLD=500  # KiB
if (( $(echo "$SIZE > $THRESHOLD" | bc -l) )); then
    echo "Binary size $SIZE exceeds budget $THRESHOLD KiB"
    exit 1
fi
```

### Tools Reference

| Tool | Purpose | Install |
|------|---------|---------|
| `cargo tree` | Dependency graph, features, duplicates | Built-in |
| `cargo bloat` | Largest symbols and crates | `cargo install cargo-bloat` |
| `cargo llvm-lines` | LLVM IR contribution by generic function | `cargo install cargo-llvm-lines` |
| `llvm-size` / `size` | Section sizes | Included with LLVM / binutils |
| `nm` / `llvm-nm` | Symbol table | Included with LLVM / binutils |
| `readelf` / `llvm-readobj` | ELF/PE structure details | Included with LLVM / binutils |
| `objdump` / `llvm-objdump` | Disassembly and section analysis | Included with LLVM / binutils |
| `gzip` / `zstd` / `Compress-Archive` | Compressed distribution size | System package |
| Linker map files | Per-object-file size breakdown | `rustflags = ["-C", "link-arg=-Wl,-Map=map.txt"]` (Linux) |

---

## 7. Case Study: Applying the Workflow to a Real Crate

This section walks through the investigation workflow (§6) applied to a
real Rust crate - an archetype-based Entity Component System library with
a small CLI demo binary. Use it as a template for your own projects.

> **Crate:** `ecs_hybrid` (ECS engine + demo binary)
> **Rust:** 1.95.0, **Target:** `x86_64-pc-windows-msvc`, **Binary:** 425 KB stripped

### 7.1 The Starting Profile

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
strip = true
panic = "abort"
```

**Assessment:** Four of the five most impactful size-reduction settings are
already enabled (LTO, single CGU, strip, panic=abort). The remaining lever is
`opt-level` - currently `3` (speed), could be `"s"` or `"z"`.

### 7.2 Baseline Numbers

| Metric | Value |
|--------|-------|
| On-disk size | 425 KB |
| `.text` (code) | 350 KB |
| `.data` (read-only data) | 73 KB |
| Dependencies (production) | 3 direct, ~15 transitive |
| Duplicate deps | None |

### 7.3 What `cargo bloat` Revealed

| Size | Crate | What it is |
|------|-------|-----------|
| 148 KB (42%) | `std` | Formatting, collections, alloc - unavoidable without `-Zbuild-std` |
| 140 KB (40%) | own code | ECS engine, queries, scheduler, demo code |
| 19 KB (5%) | `rayon` + `rayon_core` | Parallel iteration infrastructure |
| 11 KB (3%) | `crossbeam-*` | rayon's internal work-stealing deque |

**Key insight:** After `std` and your own code, the largest contributor was
`rayon` (5%). For a parallel-compute library this is expected and reasonable.
If this were a single-threaded tool, replacing rayon with `std::thread::scope`
would save ~30 KB.

### 7.4 Largest Functions

The single largest function was `main` at 25 KB - containing demo/example code
with many `println!` calls. Moving example code from `src/main.rs` to
`examples/` would save up to 25 KB with zero risk.

Several closure-based system instantiations appeared at 3–7 KB each, totalling
~25 KB - a textbook example of monomorphization from generic ECS query APIs.

### 7.5 What Was Already Good

- No duplicate dependency versions
- No async runtime, no TLS, no serialization, no regex in production
- Dev-dependencies (`criterion` and its heavy transitive deps) correctly excluded
- `panic = "abort"` saved ~10% vs default unwind
- Optional `tracy-client` profiling dependency properly feature-gated

### 7.6 The Experiment Table (what to try next)

| Experiment | Expected saving | Risk | Notes |
|-----------|----------------|------|-------|
| `opt-level = "s"` | 30–50 KB (7–12%) | Low | Slight speed trade-off |
| `opt-level = "z"` | 50–70 KB (12–16%) | Moderate | Disables loop vectorisation |
| `lto = "fat"` | 20–40 KB (5–10%) | Low | Longer link time |
| Move demos out of `main.rs` | Up to 25 KB (6%) | None | Pure code organisation |
| `-Zbuild-std` (nightly) | 30–50 KB from `std` | Moderate | Requires nightly toolchain |
| Replace rayon with `std::thread` | ~30 KB | High | Loses work-stealing |

### 7.7 Template for Your Own Project

Copy this checklist and fill it in for your crate:

```
□ Record: rustc --version, target triple, git rev
□ Build: cargo build --release
□ Measure: ls -l, llvm-size
□ Profile: cargo bloat --release --crates -n 20
□ Largest fns: cargo bloat --release -n 30
□ Deps: cargo tree -e features -e no-dev
□ Duplicates: cargo tree -d
□ Experiment: change ONE setting, rebuild, measure, compare
□ Document: record before/after in a table like §7.6
□ CI: add a size budget check
```

A small-to-medium pure-Rust binary without heavy dependencies should land
between 200 KB and 800 KB after applying the techniques in this guide.
If you are above 2 MB, there is almost certainly low-hanging fruit.
