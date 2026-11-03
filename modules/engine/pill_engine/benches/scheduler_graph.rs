//! Benchmarks for the engine's scheduler: building the dependency graph and
//! running frames of systems through it.
//!
//! The scheduler compares what each pair of systems reads and writes, then
//! puts systems that do not conflict into the same parallel batch. One
//! benchmark measures building that graph, the other measures running frames
//! on it.
//!
//! # Responsibilities
//!
//! - `scheduler_graph_build` measures the pairwise conflict comparison and the
//!   batch assignment for 10, 50, 100 and 200 systems. One system in three
//!   writes its component and the other two only read theirs. The systems
//!   cycle through 20 type ids, so some pairs conflict and most do not.
//! - `scheduler_batch_execution` measures whole frames with the same system
//!   counts over 100 entities. Each system does almost no work, so the frame
//!   time is mostly scheduler overhead rather than system compute.
//!
//! # Design
//!
//! The two benchmarks share nothing but the idea of a set of systems. Graph
//! build needs no components and no entities: its access patterns are built
//! from [`TypeId`] values alone. Batch execution builds a real engine with
//! real systems, then measures `process_frame`, which is where the scheduler
//! consults the graph and dispatches the batches.

// Standard library
use std::any::TypeId;

// External crates
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use pill_engine::*;

// =============================================================================
// Constants
// =============================================================================

/// Twenty distinct component type ids, taken from Rust's own types so the
/// graph-build benchmark needs no component registrations.
static DISTINCT_TYPE_IDS: [TypeId; 20] = [
    TypeId::of::<u8>(),
    TypeId::of::<u16>(),
    TypeId::of::<u32>(),
    TypeId::of::<u64>(),
    TypeId::of::<i8>(),
    TypeId::of::<i16>(),
    TypeId::of::<i32>(),
    TypeId::of::<i64>(),
    TypeId::of::<f32>(),
    TypeId::of::<f64>(),
    TypeId::of::<bool>(),
    TypeId::of::<char>(),
    TypeId::of::<usize>(),
    TypeId::of::<isize>(),
    TypeId::of::<String>(),
    TypeId::of::<Vec<u8>>(),
    TypeId::of::<Option<u8>>(),
    TypeId::of::<Result<u8, u8>>(),
    TypeId::of::<[u8; 4]>(),
    TypeId::of::<()>(),
];

// =============================================================================
// Types
// =============================================================================

/// Declares the twenty component structs (`C0`..`C19`) used by the
/// batch-execution benchmark.
///
/// The macro takes the struct names. Each generated struct holds one `f32`
/// and implements [`Component`].
macro_rules! define_components {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Debug, Clone)]
            struct $name(f32);
            impl Component for $name {}
        )*
    };
}

define_components!(
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14, C15, C16, C17, C18, C19,
);

// =============================================================================
// Macros
// =============================================================================

/// Registers `system_count` systems into `engine`, each writing one of the
/// twenty component types.
///
/// System `i` writes component `C(i % 20)`, so the component type repeats
/// every twenty systems, and two systems twenty apart never share a batch.
/// The `index => type` arms give the struct for each remainder from 0 to 19;
/// a missing arm panics.
macro_rules! register_batch_systems {
    ($engine:expr, $system_count:expr, $($index:literal => $component_type:ty),* $(,)?) => {
        for i in 0..$system_count {
            let system_name = Box::leak(format!("sys_{}", i).into_boxed_str());
            match i % 20 {
                $($index => {
                    $engine.register_system(system_name, |mut query: Query<&mut $component_type>| {
                        for mut component in query.iter_mut() { black_box(component.0 += 0.001); }
                    });
                })*
                _ => unreachable!(),
            }
        }
    };
}

// =============================================================================
// Helper Functions
// =============================================================================

/// Builds the access patterns the graph-build benchmark schedules.
///
/// Each system touches exactly one component: system `i` uses the type id at
/// `i % 20`, so systems twenty apart share a component. One system in three
/// writes its component and the other two only read theirs, so some of those
/// shared pairs conflict and some do not.
fn build_accesses(system_count: usize) -> Vec<SystemAccess> {
    let mut accesses = Vec::with_capacity(system_count);
    for i in 0..system_count {
        let mut access = SystemAccess::new();
        let type_id = DISTINCT_TYPE_IDS[i % DISTINCT_TYPE_IDS.len()];
        if i % 3 == 0 {
            access.add_write(ComponentId::native(type_id));
        } else {
            access.add_read(ComponentId::native(type_id));
        }
        accesses.push(access);
    }
    accesses
}

/// Builds an engine with `system_count` systems and 100 fully-populated
/// entities for the batch-execution benchmark.
///
/// Every entity carries all twenty components, so every system's query finds
/// all 100 of them. The engine is built once per system count, before the
/// timed runs start.
fn build_engine_for_batch_execution(system_count: usize) -> Engine {
    // Step 1: Create an engine and enable parallel batch execution.
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Step 2: Register all 20 component types.
    engine.world_mut().register_component::<C0>();
    engine.world_mut().register_component::<C1>();
    engine.world_mut().register_component::<C2>();
    engine.world_mut().register_component::<C3>();
    engine.world_mut().register_component::<C4>();
    engine.world_mut().register_component::<C5>();
    engine.world_mut().register_component::<C6>();
    engine.world_mut().register_component::<C7>();
    engine.world_mut().register_component::<C8>();
    engine.world_mut().register_component::<C9>();
    engine.world_mut().register_component::<C10>();
    engine.world_mut().register_component::<C11>();
    engine.world_mut().register_component::<C12>();
    engine.world_mut().register_component::<C13>();
    engine.world_mut().register_component::<C14>();
    engine.world_mut().register_component::<C15>();
    engine.world_mut().register_component::<C16>();
    engine.world_mut().register_component::<C17>();
    engine.world_mut().register_component::<C18>();
    engine.world_mut().register_component::<C19>();

    // Step 3: Spawn 100 entities with all 20 components so every system has work.
    for _ in 0..100 {
        engine
            .world_mut()
            .create_entity()
            .with(C0(0.0))
            .with(C1(0.0))
            .with(C2(0.0))
            .with(C3(0.0))
            .with(C4(0.0))
            .with(C5(0.0))
            .with(C6(0.0))
            .with(C7(0.0))
            .with(C8(0.0))
            .with(C9(0.0))
            .with(C10(0.0))
            .with(C11(0.0))
            .with(C12(0.0))
            .with(C13(0.0))
            .with(C14(0.0))
            .with(C15(0.0))
            .with(C16(0.0))
            .with(C17(0.0))
            .with(C18(0.0))
            .with(C19(0.0))
            .build()
            .unwrap();
    }

    // Step 4: Register `system_count` systems; system `i` writes component `C(i % 20)`.
    register_batch_systems!(engine, system_count,
        0 => C0,  1 => C1,  2 => C2,  3 => C3,  4 => C4,
        5 => C5,  6 => C6,  7 => C7,  8 => C8,  9 => C9,
        10 => C10, 11 => C11, 12 => C12, 13 => C13, 14 => C14,
        15 => C15, 16 => C16, 17 => C17, 18 => C18, 19 => C19,
    );

    engine
}

// =============================================================================
// Benchmarks
// =============================================================================

/// Measures building the execution graph from ready-made access patterns.
///
/// The patterns are built before each timed run, so the measurement starts
/// from an empty scheduler and covers the scheduler's own work: the pairwise
/// conflict comparison and the batch assignment. Sizes run from 10 to 200
/// systems.
fn bench_graph_build(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("scheduler_graph_build");
    for &system_count in &[10, 50, 100, 200] {
        group.bench_with_input(
            BenchmarkId::from_parameter(system_count),
            &system_count,
            |benchmark, &system_count| {
                benchmark.iter_batched(
                    || build_accesses(system_count),
                    |accesses| {
                        let mut scheduler = SystemScheduler::new();
                        scheduler.ensure_execution_graph(accesses.iter());
                        black_box(scheduler.execution_graph().len());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Measures whole frames on an engine with `system_count` systems and 100
/// entities.
///
/// The engine is built before the timed runs, so every iteration is one call
/// to `process_frame`. Most of that time is the scheduler: checking that its
/// graph is current, walking the batches, and calling every system, each of
/// which does almost no work.
fn bench_batch_execution(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("scheduler_batch_execution");
    for &system_count in &[10, 50, 100, 200] {
        group.bench_with_input(
            BenchmarkId::from_parameter(system_count),
            &system_count,
            |benchmark, &system_count| {
                let mut engine = build_engine_for_batch_execution(system_count);
                benchmark.iter(|| black_box(engine.process_frame().is_ok()));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_graph_build, bench_batch_execution);
criterion_main!(benches);
