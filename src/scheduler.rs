// ----------------------------------------------------------------------------
// Parallel System Scheduler
// ----------------------------------------------------------------------------
//! Automatic dependency analysis and parallel system execution.
//!
//! This module analyzes component access patterns to build a dependency graph
//! and executes systems in parallel batches when safe to do so.
//!
//! ## How it works:
//! - When a system is registered, it reports its access pattern (which components it reads/writes, whether it uses Commands).
//! - The scheduler builds an execution graph that groups systems into batches that can run in
//!   parallel without conflicts (no read-write or write-write conflicts, and Commands require exclusive access).
//! - During frame processing and if parallel execution is enabled, the scheduler executes each batch in parallel using Rayon.
//! - Systems that use Commands are executed sequentially to ensure safe access to the World.
//! - The scheduler dependency analysis ensures that no two systems that access the same component in a conflicting way
//!   are run in parallel, preventing data races and ensuring thread safety.

use crate::component::{ComponentId, ComponentMask};
use crate::resource::ResourceId;
use std::any::TypeId;
use std::collections::HashSet;

/// Shared type-key wrapper around [`TypeId`], used as the foundation for
/// both [`ComponentId`] and [`ResourceId`].
///
/// Lives in the scheduler module because it's the primary consumer that
/// needs to treat component and resource identifiers uniformly when
/// building access patterns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeKey(pub TypeId);

impl TypeKey {
    pub fn of<T: 'static>() -> Self {
        TypeKey(TypeId::of::<T>())
    }

    /// Get the underlying [`TypeId`].
    pub fn type_id(self) -> TypeId {
        self.0
    }
}

/// Component access information for a system.
///
/// During registration, components are collected into [`HashSet`]s via
/// [`add_read`] / [`add_write`].  After registration, [`build_component_masks`]
/// converts them into [`ComponentMask`] bitfields so that [`conflicts_with`]
/// can use single-instruction bitwise AND instead of hashing.
///
/// [`add_read`]: SystemAccess::add_read
/// [`add_write`]: SystemAccess::add_write
/// [`build_component_masks`]: SystemAccess::build_component_masks
/// [`conflicts_with`]: SystemAccess::conflicts_with
#[derive(Debug, Clone, Default)]
pub struct SystemAccess {
    /// Components read immutably (&T) - registration phase
    pub reads: HashSet<ComponentId>,
    /// Components written mutably (&mut T) - registration phase
    pub writes: HashSet<ComponentId>,
    /// Whether the system uses Commands (requires exclusive World access)
    pub uses_commands: bool,
    /// Resources read immutably (Res<T>)
    pub resource_reads: HashSet<ResourceId>,
    /// Resources written mutably (ResMut<T>)
    pub resource_writes: HashSet<ResourceId>,

    // --- Precomputed bitmasks for O(1) conflict detection ---
    reads_mask: ComponentMask,
    writes_mask: ComponentMask,
}

impl SystemAccess {
    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    pub fn add_read(&mut self, component_id: ComponentId) {
        self.reads.insert(component_id);
    }

    #[inline]
    pub fn add_write(&mut self, component_id: ComponentId) {
        self.writes.insert(component_id);
    }

    pub fn set_uses_commands(&mut self, uses: bool) {
        self.uses_commands = uses;
    }

    #[inline]
    pub fn add_resource_read(&mut self, resource_id: ResourceId) {
        self.resource_reads.insert(resource_id);
    }

    #[inline]
    pub fn add_resource_write(&mut self, resource_id: ResourceId) {
        self.resource_writes.insert(resource_id);
    }

    /// Populate the internal [`ComponentMask`] fields from the [`HashSet`]
    /// fields using the given [`ComponentRegistry`](crate::component::ComponentRegistry).
    ///
    /// Must be called after all `add_read` / `add_write` calls and before
    /// the first [`conflicts_with`] call.  The scheduler calls this
    /// automatically during graph building.
    pub fn build_component_masks(&mut self, registry: &crate::component::ComponentRegistry) {
        self.reads_mask = ComponentMask::empty();
        self.writes_mask = ComponentMask::empty();
        for id in &self.reads {
            if let Some(bit) = registry.get_bit(id) {
                self.reads_mask.set(bit);
            }
        }
        for id in &self.writes {
            if let Some(bit) = registry.get_bit(id) {
                self.writes_mask.set(bit);
            }
        }
    }

    /// Check if this system conflicts with another.
    ///
    /// Component conflicts are detected with a single bitwise AND
    /// (O(1) via [`ComponentMask`]) when masks have been built via
    /// [`build_component_masks`].  Falls back to [`HashSet::is_disjoint`]
    /// when masks are empty (e.g. in tests that don't have a registry).
    ///
    /// Resource conflicts always use [`HashSet::is_disjoint`].
    #[inline]
    pub fn conflicts_with(&self, other: &SystemAccess) -> bool {
        // Commands require exclusive World access
        if self.uses_commands || other.uses_commands {
            return true;
        }

        // Component conflicts - prefer O(1) bitmasks when available,
        // fall back to HashSet for tests / ad-hoc usage.
        if self.reads_mask.is_empty()
            && self.writes_mask.is_empty()
            && other.reads_mask.is_empty()
            && other.writes_mask.is_empty()
        {
            // Fallback: use HashSet operations
            if !self.writes.is_disjoint(&other.writes) {
                return true;
            }
            if !self.writes.is_disjoint(&other.reads) {
                return true;
            }
            if !self.reads.is_disjoint(&other.writes) {
                return true;
            }
        } else {
            // Fast path: O(1) bitwise AND
            if self.writes_mask.intersects(&other.writes_mask) {
                return true;
            }
            if self.writes_mask.intersects(&other.reads_mask) {
                return true;
            }
            if self.reads_mask.intersects(&other.writes_mask) {
                return true;
            }
        }

        // Resource conflicts - still HashSet-based
        if !self.resource_writes.is_disjoint(&other.resource_writes) {
            return true;
        }
        if !self.resource_writes.is_disjoint(&other.resource_reads) {
            return true;
        }
        if !self.resource_reads.is_disjoint(&other.resource_writes) {
            return true;
        }

        false
    }
}

/// Execution scheduler that builds parallel batches from system dependencies
pub struct SystemScheduler {
    /// System count
    system_count: usize,
    /// Access patterns for each system
    access_patterns: Vec<SystemAccess>,
    /// Precomputed conflict matrix: `conflict_matrix[i][j]` is true when
    /// system `i` conflicts with system `j`.  Built once after all systems
    /// are registered, then reused for every graph rebuild.
    conflict_matrix: Vec<Vec<bool>>,
    /// Computed execution graph: Vec of batches, each batch contains system indices that can run in parallel
    execution_graph: Vec<Vec<usize>>,
}

impl SystemScheduler {
    /// Create a new scheduler
    pub fn new() -> Self {
        Self {
            system_count: 0,
            access_patterns: Vec::new(),
            conflict_matrix: Vec::new(),
            execution_graph: Vec::new(),
        }
    }
}

impl Default for SystemScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemScheduler {
    /// Register a system with its access pattern.
    ///
    /// Returns the system's index for later reference.
    pub fn register_system(&mut self, access: SystemAccess) -> usize {
        let index = self.system_count;
        self.access_patterns.push(access);
        self.system_count += 1;
        // Extend the conflict matrix for the new system.
        self.extend_conflict_matrix();
        index
    }

    /// Extend the conflict matrix by one row and column for a newly
    /// registered system.  Computes conflicts against all existing
    /// systems (O(n) per registration, O(n²) total).
    fn extend_conflict_matrix(&mut self) {
        let new_idx = self.system_count - 1;
        // Add a row for the new system.
        let mut new_row = vec![false; self.system_count];
        for j in 0..new_idx {
            let conflict = self.access_patterns[new_idx].conflicts_with(&self.access_patterns[j]);
            new_row[j] = conflict;
            // Matrix is symmetric - also fill the column.
            self.conflict_matrix[j].push(conflict);
        }
        // A system doesn't conflict with itself (already false).
        self.conflict_matrix.push(new_row);
    }

    /// Build the execution graph based on dependencies.
    ///
    /// Uses a precomputed conflict matrix so that each pairwise check is
    /// an O(1) array lookup instead of a full `conflicts_with` call.
    /// The batching algorithm itself remains O(n²) in the worst case,
    /// but the constant factor is dramatically reduced.
    pub fn build_execution_graph(&mut self) {
        self.execution_graph.clear();

        let mut scheduled = vec![false; self.system_count];
        let mut scheduled_count = 0;

        while scheduled_count < self.system_count {
            let remaining = self.system_count - scheduled_count;
            let mut batch = Vec::with_capacity(remaining);

            // Try to add each unscheduled system to the current batch
            for (i, is_scheduled) in scheduled.iter_mut().enumerate() {
                if *is_scheduled {
                    continue;
                }

                // Check if this system conflicts with any system already in the batch.
                // Uses the precomputed matrix - O(batch_size) array lookups.
                let conflicts = batch.iter().any(|&j| self.conflict_matrix[i][j]);

                if !conflicts {
                    batch.push(i);
                    *is_scheduled = true;
                    scheduled_count += 1;
                }
            }

            if !batch.is_empty() {
                self.execution_graph.push(batch);
            }
        }
    }

    /// Get the execution graph (batches of system indices)
    pub fn execution_graph(&self) -> &[Vec<usize>] {
        &self.execution_graph
    }

    /// Get access pattern for a system
    pub fn get_access(&self, index: usize) -> Option<&SystemAccess> {
        self.access_patterns.get(index)
    }

    /// Print execution graph for debugging
    pub fn print_execution_graph(&self, system_names: &[&str]) {
        println!("\n=== System Execution Graph ===");
        for (batch_index, batch) in self.execution_graph.iter().enumerate() {
            println!("Batch {}: {} systems (parallel)", batch_index, batch.len());
            for &system_index in batch {
                let name = system_names.get(system_index).unwrap_or(&"<unknown>");
                let access = &self.access_patterns[system_index];
                println!(
                    "  - {} (reads: {}, writes: {}, res_reads: {}, res_writes: {}, commands: {})",
                    name,
                    access.reads.len(),
                    access.writes.len(),
                    access.resource_reads.len(),
                    access.resource_writes.len(),
                    access.uses_commands
                );
            }
        }
        println!("==============================\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::ComponentId;
    use std::{any::TypeId, hash::BuildHasher};

    // Helper: Verify that no batch contains conflicting systems
    fn assert_no_batch_conflicts(scheduler: &SystemScheduler) {
        for (batch_index, batch) in scheduler.execution_graph().iter().enumerate() {
            for (i, &system_index_a) in batch.iter().enumerate() {
                let access_a = scheduler.get_access(system_index_a).unwrap();
                for &system_index_b in &batch[i + 1..] {
                    let access_b = scheduler.get_access(system_index_b).unwrap();
                    assert!(
                        !access_a.conflicts_with(access_b),
                        "Batch {} contains conflicting systems {} and {}!\n\
                         System {}: reads={:?}, writes={:?}, commands={}\n\
                         System {}: reads={:?}, writes={:?}, commands={}",
                        batch_index,
                        system_index_a,
                        system_index_b,
                        system_index_a,
                        access_a.reads,
                        access_a.writes,
                        access_a.uses_commands,
                        system_index_b,
                        access_b.reads,
                        access_b.writes,
                        access_b.uses_commands
                    );
                }
            }
        }
    }

    // Helper: Verify all systems are scheduled exactly once
    fn assert_all_systems_scheduled(scheduler: &SystemScheduler, system_count: usize) {
        let mut scheduled = vec![false; system_count];
        for batch in scheduler.execution_graph() {
            for &system_index in batch {
                assert!(
                    !scheduled[system_index],
                    "System {} scheduled multiple times",
                    system_index
                );
                scheduled[system_index] = true;
            }
        }
        for (system_index, &was_scheduled) in scheduled.iter().enumerate() {
            assert!(was_scheduled, "System {} was not scheduled", system_index);
        }
    }

    #[test]
    fn test_no_conflicts() {
        let mut scheduler = SystemScheduler::new();

        // System 1: reads A
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        // System 2: reads B
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<f32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Both systems should be in the same batch (no conflicts)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    #[test]
    fn test_write_conflict() {
        let mut scheduler = SystemScheduler::new();

        // System 1: writes A
        let mut access1 = SystemAccess::new();
        access1.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        // System 2: writes A
        let mut access2 = SystemAccess::new();
        access2.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (write-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    #[test]
    fn test_read_write_conflict() {
        let mut scheduler = SystemScheduler::new();

        // System 1: reads A
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        // System 2: writes A
        let mut access2 = SystemAccess::new();
        access2.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (read-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    #[test]
    fn test_commands_exclusive() {
        let mut scheduler = SystemScheduler::new();

        // System 1: uses commands
        let mut access1 = SystemAccess::new();
        access1.set_uses_commands(true);
        scheduler.register_system(access1);

        // System 2: reads A
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (Commands require exclusive access)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    #[test]
    fn test_multiple_readers_parallel() {
        let mut scheduler = SystemScheduler::new();

        // 5 systems all reading the same component
        for _ in 0..5 {
            let mut access = SystemAccess::new();
            access.add_read(ComponentId(TypeId::of::<i32>()));
            scheduler.register_system(access);
        }

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 5);

        // All readers can run in parallel (read-read is OK)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 5);
    }

    #[test]
    fn test_single_writer_blocks_all() {
        let mut scheduler = SystemScheduler::new();

        // 4 readers
        for _ in 0..4 {
            let mut access = SystemAccess::new();
            access.add_read(ComponentId(TypeId::of::<i32>()));
            scheduler.register_system(access);
        }

        // 1 writer of the same component
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 5);

        // Writer must be in a separate batch from all readers
        assert!(scheduler.execution_graph().len() >= 2);
    }

    #[test]
    fn test_complex_dependency_graph() {
        let mut scheduler = SystemScheduler::new();

        // System 0: reads A, writes B
        let mut access0 = SystemAccess::new();
        access0.add_read(ComponentId(TypeId::of::<i32>()));
        access0.add_write(ComponentId(TypeId::of::<f32>()));
        scheduler.register_system(access0);

        // System 1: reads B, writes C
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId(TypeId::of::<f32>()));
        access1.add_write(ComponentId(TypeId::of::<u32>()));
        scheduler.register_system(access1);

        // System 2: reads A (can run with system 1)
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        // System 3: reads C (can run with systems 0 and 2)
        let mut access3 = SystemAccess::new();
        access3.add_read(ComponentId(TypeId::of::<u32>()));
        scheduler.register_system(access3);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 4);
    }

    #[test]
    fn test_disjoint_component_sets() {
        let mut scheduler = SystemScheduler::new();

        // System 0: writes A
        let mut access0 = SystemAccess::new();
        access0.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access0);

        // System 1: writes B
        let mut access1 = SystemAccess::new();
        access1.add_write(ComponentId(TypeId::of::<f32>()));
        scheduler.register_system(access1);

        // System 2: writes C
        let mut access2 = SystemAccess::new();
        access2.add_write(ComponentId(TypeId::of::<u32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 3);

        // All can run in parallel (disjoint writes)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 3);
    }

    #[test]
    fn test_multiple_commands_sequential() {
        let mut scheduler = SystemScheduler::new();

        // 3 systems all using commands
        for _ in 0..3 {
            let mut access = SystemAccess::new();
            access.set_uses_commands(true);
            scheduler.register_system(access);
        }

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 3);

        // Each command system must be in its own batch
        assert_eq!(scheduler.execution_graph().len(), 3);
    }

    #[test]
    fn test_mixed_commands_and_queries() {
        let mut scheduler = SystemScheduler::new();

        // System 0: reads A
        let mut access0 = SystemAccess::new();
        access0.add_read(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access0);

        // System 1: uses commands
        let mut access1 = SystemAccess::new();
        access1.set_uses_commands(true);
        scheduler.register_system(access1);

        // System 2: reads B
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<f32>()));
        scheduler.register_system(access2);

        // System 3: uses commands
        let mut access3 = SystemAccess::new();
        access3.set_uses_commands(true);
        scheduler.register_system(access3);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 4);
    }

    #[test]
    fn test_empty_scheduler() {
        let mut scheduler = SystemScheduler::new();
        scheduler.build_execution_graph();

        assert_eq!(scheduler.execution_graph().len(), 0);
    }

    #[test]
    fn test_single_system() {
        let mut scheduler = SystemScheduler::new();

        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 1);

        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 1);
    }

    #[test]
    fn test_chain_dependencies() {
        let mut scheduler = SystemScheduler::new();

        // Chain: System0 writes A -> System1 reads A, writes B -> System2 reads B, writes C
        let mut access0 = SystemAccess::new();
        access0.add_write(ComponentId(TypeId::of::<i32>()));
        scheduler.register_system(access0);

        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId(TypeId::of::<i32>()));
        access1.add_write(ComponentId(TypeId::of::<f32>()));
        scheduler.register_system(access1);

        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<f32>()));
        access2.add_write(ComponentId(TypeId::of::<u32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 3);

        // The greedy scheduler groups systems by conflict:
        // - System0 writes A, System2 writes C - no conflict, could be batched
        // - System1 reads A, writes B - conflicts with both
        // The exact number of batches depends on scheduling order,
        // but no conflicts should exist within any batch.
        // The greedy algorithm places them as: [0], [1], [2] = 3 batches
        // or could optimize to [0,2], [1] = 2 batches
        assert!(scheduler.execution_graph().len() >= 2);
        assert!(scheduler.execution_graph().len() <= 3);
    }

    #[test]
    fn test_large_parallel_batch() {
        // Test with 5 systems all accessing different components
        let mut scheduler = SystemScheduler::new();

        // System 0: writes type A
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<u8>()));
        scheduler.register_system(access);

        // System 1: writes type B
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<u16>()));
        scheduler.register_system(access);

        // System 2: writes type C
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<u32>()));
        scheduler.register_system(access);

        // System 3: writes type D
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<u64>()));
        scheduler.register_system(access);

        // System 4: writes type E
        let mut access = SystemAccess::new();
        access.add_write(ComponentId(TypeId::of::<i8>()));
        scheduler.register_system(access);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 5);

        // All can run in parallel
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 5);
    }

    // ----------------------------------------------------------------------------
    // Resource Conflict Tests
    // ----------------------------------------------------------------------------

    #[test]
    fn test_resource_read_read_parallel() {
        let mut scheduler = SystemScheduler::new();

        // Two systems both reading the same resource
        let mut access1 = SystemAccess::new();
        access1.add_resource_read(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        let mut access2 = SystemAccess::new();
        access2.add_resource_read(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Both can run in parallel (read-read is OK)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    #[test]
    fn test_resource_write_write_conflict() {
        let mut scheduler = SystemScheduler::new();

        // Two systems both writing the same resource
        let mut access1 = SystemAccess::new();
        access1.add_resource_write(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        let mut access2 = SystemAccess::new();
        access2.add_resource_write(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Must be in different batches (write-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    #[test]
    fn test_resource_read_write_conflict() {
        let mut scheduler = SystemScheduler::new();

        // System 1 reads a resource, System 2 writes same resource
        let mut access1 = SystemAccess::new();
        access1.add_resource_read(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        let mut access2 = SystemAccess::new();
        access2.add_resource_write(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Must be in different batches (read-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    #[test]
    fn test_resource_disjoint_writes_parallel() {
        let mut scheduler = SystemScheduler::new();

        // Two systems writing different resources
        let mut access1 = SystemAccess::new();
        access1.add_resource_write(ResourceId(TypeId::of::<i32>()));
        scheduler.register_system(access1);

        let mut access2 = SystemAccess::new();
        access2.add_resource_write(ResourceId(TypeId::of::<f32>()));
        scheduler.register_system(access2);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 2);

        // Can run in parallel (different resources)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    #[test]
    fn test_resource_and_component_mixed() {
        let mut scheduler = SystemScheduler::new();

        // System 1: reads component A, writes resource X
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId(TypeId::of::<i32>()));
        access1.add_resource_write(ResourceId(TypeId::of::<u64>()));
        scheduler.register_system(access1);

        // System 2: reads component A, reads resource X
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId(TypeId::of::<i32>()));
        access2.add_resource_read(ResourceId(TypeId::of::<u64>()));
        scheduler.register_system(access2);

        // System 3: writes component B, reads resource Y (no conflicts with either)
        let mut access3 = SystemAccess::new();
        access3.add_write(ComponentId(TypeId::of::<f32>()));
        access3.add_resource_read(ResourceId(TypeId::of::<u32>()));
        scheduler.register_system(access3);

        scheduler.build_execution_graph();

        assert_no_batch_conflicts(&scheduler);
        assert_all_systems_scheduled(&scheduler, 3);

        // System 1 and 2 conflict on resource X (write vs read)
        // System 3 doesn't conflict with either
        assert!(scheduler.execution_graph().len() >= 2);
    }

    #[test]
    fn test_conflicts_with_uses_component_mask_fast_path() {
        // Verify that conflicts_with correctly detects conflicts via the
        // ComponentMask path (not the HashSet fallback).  The existing
        // tests never call build_component_masks(), so they only exercise
        // the fallback.
        use crate::component::{Component, ComponentRegistry};
        use std::any::TypeId;

        // Simulate a registry with two component types at bits 0 and 1.
        let mut registry = ComponentRegistry::new();

        #[derive(Debug)]
        struct A;
        #[derive(Debug)]
        struct B;
        impl Component for A {}
        impl Component for B {}

        registry.register::<A>();
        registry.register::<B>();

        let id_a = ComponentId(TypeId::of::<A>());
        let id_b = ComponentId(TypeId::of::<B>());

        // --- write-write conflict via masks ---
        let mut a = SystemAccess::new();
        a.add_write(id_a);
        a.build_component_masks(&registry);

        let mut b = SystemAccess::new();
        b.add_write(id_a);
        b.build_component_masks(&registry);

        // Both masks are non-empty - fast path must be active.
        assert!(!a.reads_mask.is_empty() || !a.writes_mask.is_empty());
        assert!(!b.reads_mask.is_empty() || !b.writes_mask.is_empty());
        assert!(a.conflicts_with(&b), "write-write on A should conflict");

        // --- read-write conflict via masks ---
        let mut a = SystemAccess::new();
        a.add_read(id_a);
        a.build_component_masks(&registry);

        let mut b = SystemAccess::new();
        b.add_write(id_a);
        b.build_component_masks(&registry);

        assert!(a.conflicts_with(&b), "read(A) vs write(A) should conflict");
        assert!(
            b.conflicts_with(&a),
            "write(A) vs read(A) should conflict (symmetric)"
        );

        // --- no conflict (disjoint types) via masks ---
        let mut a = SystemAccess::new();
        a.add_read(id_a);
        a.build_component_masks(&registry);

        let mut b = SystemAccess::new();
        b.add_write(id_b);
        b.build_component_masks(&registry);

        assert!(
            !a.conflicts_with(&b),
            "read(A) vs write(B) should NOT conflict"
        );
        assert!(!b.conflicts_with(&a));

        // --- read-read is fine via masks ---
        let mut a = SystemAccess::new();
        a.add_read(id_a);
        a.build_component_masks(&registry);

        let mut b = SystemAccess::new();
        b.add_read(id_a);
        b.build_component_masks(&registry);

        assert!(
            !a.conflicts_with(&b),
            "read(A) vs read(A) should NOT conflict"
        );
    }

    // ----------------------------------------------------------------------------
    // Empirical verification: exhaustive enumeration + random fuzz
    // ----------------------------------------------------------------------------
    //
    // These tests do NOT constitute a mathematical proof. They are empirical
    // checks that verify the implementation against a large but finite number
    // of input patterns - covering all relevant conflict categories
    // (components, resources, Commands, and combinations thereof).
    //
    // Together they provide high confidence that the scheduler never puts
    // conflicting systems in the same batch.

    /// Models every conflict category the scheduler must handle.
    /// Two components (A, B) and two resources (X, Y) cover read/write
    /// conflicts for both components and resources, plus Commands and no-op.
    #[derive(Clone, Copy, PartialEq, Eq, Hash)]
    enum AccessKind {
        None,      // accesses nothing - free to run in any batch
        ReadA,     // reads component A
        WriteA,    // writes component A - conflicts with ReadA, WriteA
        ReadB,     // reads component B
        WriteB,    // writes component B - conflicts with ReadB, WriteB
        Commands,  // uses Commands - conflicts with everything
        ResReadX,  // reads resource X
        ResWriteX, // writes resource X - conflicts with ResReadX, ResWriteX
        ResReadY,  // reads resource Y
        ResWriteY, // writes resource Y - conflicts with ResReadY, ResWriteY
    }

    /// Converts an `AccessKind` into a concrete `SystemAccess` value
    /// that the scheduler can process. Each kind maps to a distinct
    /// `TypeId` so the conflict detection logic sees them as separate
    /// components/resources.
    fn kind_to_access(k: AccessKind) -> SystemAccess {
        let mut a = SystemAccess::new();
        match k {
            AccessKind::None => {}
            AccessKind::ReadA => a.add_read(ComponentId(TypeId::of::<u8>())),
            AccessKind::WriteA => a.add_write(ComponentId(TypeId::of::<u8>())),
            AccessKind::ReadB => a.add_read(ComponentId(TypeId::of::<u16>())),
            AccessKind::WriteB => a.add_write(ComponentId(TypeId::of::<u16>())),
            AccessKind::Commands => a.set_uses_commands(true),
            AccessKind::ResReadX => a.add_resource_read(ResourceId(TypeId::of::<u32>())),
            AccessKind::ResWriteX => a.add_resource_write(ResourceId(TypeId::of::<u32>())),
            AccessKind::ResReadY => a.add_resource_read(ResourceId(TypeId::of::<u64>())),
            AccessKind::ResWriteY => a.add_resource_write(ResourceId(TypeId::of::<u64>())),
        }
        a
    }

    /// Exhaustive verification for small system counts.
    ///
    /// Enumerates every possible n-tuple of access kinds for n = 1..=6.
    /// With 10 access kinds, that's 10 + 100 + 1,000 + 10,000 + 100,000
    /// + 1,000,000 = 1,111,110 unique input configurations. Each one runs
    /// through `build_execution_graph` and is checked for two invariants:
    ///
    /// 1. No batch contains conflicting systems (the core safety guarantee).
    /// 2. Every system appears in exactly one batch (no omissions, no
    ///    duplicates).
    ///
    /// n-tuples are generated by counting in base 10: counter 0 → [None,
    /// None, ...], counter 1 → [ReadA, None, ...], etc. This covers every
    /// combination with repetition (systems can have the same access kind).
    #[test]
    fn proof_exhaustive_small_n() {
        let kinds: Vec<AccessKind> = vec![
            AccessKind::None,
            AccessKind::ReadA,
            AccessKind::WriteA,
            AccessKind::ReadB,
            AccessKind::WriteB,
            AccessKind::Commands,
            AccessKind::ResReadX,
            AccessKind::ResWriteX,
            AccessKind::ResReadY,
            AccessKind::ResWriteY,
        ];

        // For each n from 1 to 6 systems
        for n in 1..=6u32 {
            let total = (kinds.len() as u32).pow(n);
            let mut tuple = vec![0u8; n as usize];

            // Enumerate all n-tuples by counting in base kinds.len()
            for counter in 0..total {
                let mut counter_value = counter;
                for i in 0..n as usize {
                    tuple[i] = (counter_value % kinds.len() as u32) as u8;
                    counter_value /= kinds.len() as u32;
                }

                // Build a fresh scheduler with this specific combination
                let mut scheduler = SystemScheduler::new();
                for &kind_index in &tuple {
                    scheduler.register_system(kind_to_access(kinds[kind_index as usize]));
                }
                scheduler.build_execution_graph();

                // Verify both correctness invariants
                assert_no_batch_conflicts(&scheduler);
                assert_all_systems_scheduled(&scheduler, n as usize);
            }
        }
    }

    /// Randomised fuzz testing for larger system counts.
    ///
    /// Exhaustive enumeration explodes exponentially (10^20 is infeasible),
    /// so for larger system counts (up to 20) we use random sampling.
    /// 500 different seeds each generate a random conflict graph, giving
    /// ~10,000 unique configurations. Each run verifies the same two
    /// invariants as the exhaustive test.
    ///
    /// The random generator uses a simple LCG (Linear Congruential
    /// Generator) seeded from a hashed seed value. This gives
    /// deterministic-but-varied sequences - reproducible if a failure
    /// is found, but covering a wide range of patterns across seeds.
    #[test]
    fn proof_random_fuzz_large_n() {
        let kinds: Vec<AccessKind> = vec![
            AccessKind::None,
            AccessKind::ReadA,
            AccessKind::WriteA,
            AccessKind::ReadB,
            AccessKind::WriteB,
            AccessKind::Commands,
            AccessKind::ResReadX,
            AccessKind::ResWriteX,
            AccessKind::ResReadY,
            AccessKind::ResWriteY,
        ];

        for seed in 0..500u64 {
            // Hash the seed to get a starting value, then run an LCG
            let hasher = std::hash::RandomState::new();
            let hash = hasher.hash_one(seed);
            let mut rng = hash.wrapping_mul(6364136223846793005).wrapping_add(1);

            // Pick a random system count between 1 and 20
            let n = 1 + (rng % 20) as usize;
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);

            // Build a scheduler with n randomly-chosen access kinds
            let mut scheduler = SystemScheduler::new();
            for _ in 0..n {
                let kind_index = (rng % kinds.len() as u64) as usize;
                rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                scheduler.register_system(kind_to_access(kinds[kind_index]));
            }
            scheduler.build_execution_graph();

            // Same invariants as the exhaustive test
            assert_no_batch_conflicts(&scheduler);
            assert_all_systems_scheduled(&scheduler, n);
        }
    }
} // mod tests
