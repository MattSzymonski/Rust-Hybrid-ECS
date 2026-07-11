//! Scheduler graph build benchmarks — conflict analysis + batch formation.
//!
//! Measures: O(n²) pairwise conflict check scaling, graph build time
//! vs system count.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use ecs_hybrid::*;
use std::any::TypeId;

/// 20 distinct `TypeId` values to simulate different component types.
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
        // Every third system writes; others read.  Creates a realistic
        // mix of conflicts and parallel batches.
        if i % 3 == 0 {
            access.add_write(ComponentId(type_id));
        } else {
            access.add_read(ComponentId(type_id));
        }
        scheduler.register_system(access);
    }
    scheduler
}

fn bench_graph_build(c: &mut Criterion) {
    let mut group = c.benchmark_group("scheduler_graph_build");
    for &system_count in &[10, 50, 100, 200] {
        group.bench_with_input(
            BenchmarkId::from_parameter(system_count),
            &system_count,
            |b, &system_count| {
                b.iter_batched(
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

criterion_group!(benches, bench_graph_build);
criterion_main!(benches);
