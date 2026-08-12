//! Scheduler Benchmarks
//! ====================
//!
//! The scheduler analyzes component read/write access patterns across all
//! registered systems to build a dependency graph, then groups independent
//! systems into parallel batches. These benchmarks measure both halves of
//! that pipeline.
//!
//! Graph build (`scheduler_graph_build`): pure O(n²) pairwise conflict
//! analysis using bitmask AND operations. Simulates 10–200 systems with a
//! realistic 1/3-write conflict pattern across 20 distinct component types.
//! No real components or entities are involved - just `TypeId` values.
//!
//! Batch execution (`scheduler_batch_execution`): end-to-end frame dispatch
//! with 100 entities carrying 20 components. Each system does trivial per-entity
//! work so the benchmark measures scheduler overhead (graph walk, batch dispatch,
//! thread wake-up) rather than system compute time.
//!
//! Together these reveal whether scaling bottlenecks are in conflict analysis
//! or in the runtime dispatch machinery.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use pill_engine::*;
use std::any::TypeId;
use trait_type_map::impl_trait_accessible;

/// 20 distinct `TypeId` values for the graph-build benchmark (no real components needed).
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

fn build_scheduler(system_count: usize) -> SystemScheduler {
    let mut scheduler = SystemScheduler::new();
    for i in 0..system_count {
        let mut access = SystemAccess::new();
        let type_id = DISTINCT_TYPE_IDS[i % DISTINCT_TYPE_IDS.len()];
        // Every third system writes; others read - realistic conflict mix.
        if i % 3 == 0 {
            access.add_write(ComponentId::native(type_id));
        } else {
            access.add_read(ComponentId::native(type_id));
        }
        scheduler.register_system(access);
    }
    scheduler
}

// ---- Real components for batch execution benchmark ----

macro_rules! define_components {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Debug, Clone)]
            struct $name(f32);
            impl Component for $name {}
        )*
        impl_trait_accessible!(dyn Component; $($name),*);
    };
}

define_components!(
    C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11, C12, C13, C14, C15, C16, C17, C18, C19,
);

/// Register `system_count` systems into `engine`, cycling through the 20
/// component types. Every 3rd system reuses the same component index
/// (write-write conflict → sequential), while the other 2/3 get distinct
/// component types (no conflict → parallel batches).
macro_rules! register_batch_systems {
    ($engine:expr, $system_count:expr, $($index:literal => $component_type:ty),* $(,)?) => {
        for i in 0..$system_count {
            let _name = Box::leak(format!("sys_{}", i).into_boxed_str());
            match i % 20 {
                $($index => {
                    $engine.register_system(_name, |mut query: Query<&mut $component_type>| {
                        for mut component in query.iter_mut() { black_box(component.0 += 0.001); }
                    });
                })*
                _ => unreachable!(),
            }
        }
    };
}

fn build_engine_for_batch_execution(system_count: usize) -> Engine {
    let mut engine = Engine::new();
    engine.set_parallel_execution(true);

    // Register all 20 component types.
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

    // Spawn 100 entities with all 20 components so every system has work.
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

    register_batch_systems!(engine, system_count,
        0 => C0,  1 => C1,  2 => C2,  3 => C3,  4 => C4,
        5 => C5,  6 => C6,  7 => C7,  8 => C8,  9 => C9,
        10 => C10, 11 => C11, 12 => C12, 13 => C13, 14 => C14,
        15 => C15, 16 => C16, 17 => C17, 18 => C18, 19 => C19,
    );

    engine
}

// ---- Benchmarks ----

/// Builds the execution graph for `system_count` systems with a realistic read/write conflict mix.
/// Measures the O(n²) pairwise conflict analysis and batch-formation cost as system count grows.
fn bench_graph_build(criterion: &mut Criterion) {
    let mut group = criterion.benchmark_group("scheduler_graph_build");
    for &system_count in &[10, 50, 100, 200] {
        group.bench_with_input(
            BenchmarkId::from_parameter(system_count),
            &system_count,
            |benchmark, &system_count| {
                benchmark.iter_batched(
                    || build_scheduler(system_count),
                    |mut scheduler| {
                        scheduler.build_execution_graph();
                        black_box(scheduler.execution_graph().len());
                    },
                    criterion::BatchSize::LargeInput,
                );
            },
        );
    }
    group.finish();
}

/// Runs a full `engine.process_frame()` with `system_count` registered systems and 100 entities.
/// Measures end-to-end scheduler dispatch overhead - graph walk + parallel batch execution + system invocation.
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
