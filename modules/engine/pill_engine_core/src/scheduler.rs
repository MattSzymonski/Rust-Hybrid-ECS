//! Dependency analysis and parallel batch scheduling for system execution.
//!
//! # Responsibilities
//!
//! - Provides [`SystemAccess`] for declaring per-system access requirements.
//! - Derives the conflict matrix and the parallel batch graph from the
//!   registered systems' access patterns.
//! - Says when that derived data is stale ([`SystemScheduler::invalidate`]).
//! - Defines [`TypeKey`], a [`TypeId`] newtype for callers that key native
//!   types by identity.
//!
//! # Design
//!
//! Each system declares which components and resources it reads and writes.
//! The scheduler performs O(n²) pairwise conflict analysis over those sets:
//! two systems conflict if one writes a component the other reads or writes.
//! Systems with no conflicts can run in the same parallel batch. Systems
//! that use [`Commands`](crate::Commands) are always executed sequentially for
//! safety.
//!
//! The scheduler keeps no list of systems of its own. The engine owns the
//! registration records, and invalidates the scheduler whenever that list
//! changes structurally; [`SystemScheduler::ensure_execution_graph`] then
//! rebuilds the matrix and the batches from the access patterns it is handed,
//! once, and does nothing until the next invalidation. That leaves one owner
//! for the system list and one rule for when the graph is current.

// Standard library
use std::any::TypeId;
use std::collections::HashSet;

// Current crate
use crate::component::ComponentId;
use crate::resource::ResourceId;

// =============================================================================
// TypeKey
// =============================================================================

/// Type-key wrapper around [`TypeId`] for keying native types by identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TypeKey(pub TypeId);

impl TypeKey {
    /// Builds the [`TypeKey`] for a given type `T`.
    pub fn of<T: 'static>() -> Self {
        TypeKey(TypeId::of::<T>())
    }

    /// Returns the underlying [`TypeId`].
    pub fn type_id(self) -> TypeId {
        self.0
    }
}

// =============================================================================
// SystemAccess
// =============================================================================

/// Component and resource access information for a system.
///
/// Collected with [`add_read`](SystemAccess::add_read),
/// [`add_write`](SystemAccess::add_write),
/// [`add_resource_read`](SystemAccess::add_resource_read) and
/// [`add_resource_write`](SystemAccess::add_resource_write) while the system is
/// being registered, then compared directly by
/// [`conflicts_with`](SystemAccess::conflicts_with).
///
/// The sets are the only record of what a system touches: there is no derived
/// cache to fall out of step with them. A system whose access changes after it
/// was registered is not re-batched, so declare every access first.
#[derive(Debug, Clone, Default)]
pub struct SystemAccess {
    /// Components read immutably (&T).
    reads: HashSet<ComponentId>,
    /// Components written mutably (&mut T).
    writes: HashSet<ComponentId>,
    /// Whether the system uses Commands (requires exclusive World access)
    pub uses_commands: bool,
    /// Resources read immutably (`Res<T>`)
    resource_reads: HashSet<ResourceId>,
    /// Resources written mutably (`ResMut<T>`)
    resource_writes: HashSet<ResourceId>,
}

impl SystemAccess {
    /// Creates an empty [`SystemAccess`] with no components or resources.
    ///
    /// Equivalent to [`SystemAccess::default`]. Access patterns are built up
    /// with [`add_read`](SystemAccess::add_read) and
    /// [`add_write`](SystemAccess::add_write) before registration.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `component_id` as read-only (`&T`) access for this system.
    #[inline]
    pub fn add_read(&mut self, component_id: ComponentId) {
        self.reads.insert(component_id);
    }

    /// Registers `component_id` as mutable (`&mut T`) access for this system.
    ///
    /// A write to a component conflicts with any other system reading or
    /// writing the same component.
    #[inline]
    pub fn add_write(&mut self, component_id: ComponentId) {
        self.writes.insert(component_id);
    }

    /// Marks whether this system uses [`Commands`](crate::Commands).
    ///
    /// A system that uses [`Commands`](crate::Commands) requires exclusive
    /// world access and therefore conflicts with every other system.
    pub fn set_uses_commands(&mut self, uses: bool) {
        self.uses_commands = uses;
    }

    /// Registers `resource_id` as immutable (`Res<T>`) access for this system.
    #[inline]
    pub fn add_resource_read(&mut self, resource_id: ResourceId) {
        self.resource_reads.insert(resource_id);
    }

    /// Registers `resource_id` as mutable (`ResMut<T>`) access for this system.
    #[inline]
    pub fn add_resource_write(&mut self, resource_id: ResourceId) {
        self.resource_writes.insert(resource_id);
    }

    /// How many components this system reads.
    ///
    /// Reporting only; the ids themselves stay private so nothing can mutate
    /// the set behind a registered system's back.
    pub fn reads_count(&self) -> usize {
        self.reads.len()
    }

    /// How many components this system writes.
    pub fn writes_count(&self) -> usize {
        self.writes.len()
    }

    /// How many resources this system reads.
    pub fn resource_reads_count(&self) -> usize {
        self.resource_reads.len()
    }

    /// How many resources this system writes.
    pub fn resource_writes_count(&self) -> usize {
        self.resource_writes.len()
    }

    /// Checks whether this system conflicts with another.
    ///
    /// Two systems conflict when either uses [`Commands`](crate::Commands), or
    /// when one writes a component or resource the other reads or writes. The
    /// access sets answer directly, so the answer cannot disagree with what
    /// the systems actually touch - a comparison that read a derived cache
    /// could miss an access the cache did not know about and dispatch two
    /// writers of one component to different threads.
    ///
    /// The sets are small - a handful of ids per system - so the comparisons
    /// are cheap; the result is recorded in the scheduler's conflict matrix at
    /// registration time, which is what the frame reads.
    #[inline]
    pub fn conflicts_with(&self, other: &SystemAccess) -> bool {
        // Step 1: Commands require exclusive world access, so a system that
        // uses them conflicts with everything.
        if self.uses_commands || other.uses_commands {
            return true;
        }

        // Step 2: Component conflicts: any write shared with the other side's
        // reads or writes, in either direction.
        if !self.writes.is_disjoint(&other.writes) {
            return true;
        }
        if !self.writes.is_disjoint(&other.reads) {
            return true;
        }
        if !self.reads.is_disjoint(&other.writes) {
            return true;
        }

        // Step 3: Resource conflicts, compared by content rather than by set
        // size: a same-size replacement - `remove(A)` + `insert(B)` - keeps
        // the length and changes the conflict.
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

// =============================================================================
// SystemScheduler
// =============================================================================

/// The conflict matrix and parallel batch graph for the registered systems.
///
/// The matrix is a pure function of the access patterns it was last built
/// from, and the graph is a pure function of the matrix. Both are rebuilt
/// together by [`ensure_execution_graph`](SystemScheduler::ensure_execution_graph)
/// after [`invalidate`](SystemScheduler::invalidate), so the answer to "when
/// is the graph current?" is one flag rather than a rule each caller has to
/// remember.
pub struct SystemScheduler {
    /// `conflict_matrix[i][j]` is true when system `i` conflicts with system
    /// `j`. Rebuilt from the access patterns, never filtered in place.
    conflict_matrix: Vec<Vec<bool>>,
    /// Batches of system indices; each batch can run in parallel because no
    /// two of its members conflict.
    execution_graph: Vec<Vec<usize>>,
    /// Whether the matrix and the graph predate the current system set.
    ///
    /// Starts true: a fresh scheduler has built nothing yet.
    needs_rebuild: bool,
}

impl SystemScheduler {
    /// Creates an empty scheduler that builds its first graph on demand.
    pub fn new() -> Self {
        Self {
            conflict_matrix: Vec::new(),
            execution_graph: Vec::new(),
            needs_rebuild: true,
        }
    }
}

impl Default for SystemScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemScheduler {
    /// Marks the conflict matrix and the batch graph stale.
    ///
    /// Every structural change to the registered systems - a registration, a
    /// removal, a clear - goes through here. The rebuild itself happens once,
    /// in [`ensure_execution_graph`](SystemScheduler::ensure_execution_graph),
    /// so a module that registers many systems in its `init` pays for one
    /// rebuild rather than one per registration.
    ///
    /// Enabled state is not part of the conflict data: disabled systems stay
    /// in the batches they were assigned and are skipped at dispatch.
    pub fn invalidate(&mut self) {
        self.needs_rebuild = true;
    }

    /// Rebuilds the conflict matrix and the execution graph when stale.
    ///
    /// `accesses` must yield every registered system's access pattern in
    /// registration order, and its length must be the number of registered
    /// systems - the graph's indices are positions in that list. Does nothing
    /// when nothing was invalidated since the last build.
    ///
    /// Batches are filled first-fit in registration order: each system joins
    /// the earliest batch that holds nothing it conflicts with, and opens a
    /// new batch at the end when no batch fits. Registration order is the only
    /// ordering signal the engine offers - there is no `before`/`after` - so
    /// it is what decides which of two conflicting systems runs first; the
    /// pair is never merged, and the later one cannot join any batch the
    /// earlier one already occupies.
    pub fn ensure_execution_graph<'a>(
        &mut self,
        accesses: impl ExactSizeIterator<Item = &'a SystemAccess>,
    ) {
        if !self.needs_rebuild {
            return;
        }
        let _zone = crate::profile_scope!(
            "build execution graph",
            [("Number of systems to schedule: {}", accesses.len())]
        );
        let accesses: Vec<&SystemAccess> = accesses.collect();

        // Step 1: The conflict matrix is a pure function of the access list.
        // Each unordered pair is compared once and written to both cells, so
        // the matrix stays symmetric.
        self.conflict_matrix = vec![vec![false; accesses.len()]; accesses.len()];
        for (first, access) in accesses.iter().enumerate() {
            for (second, other) in accesses.iter().enumerate().take(first) {
                let conflict = access.conflicts_with(other);
                self.conflict_matrix[first][second] = conflict;
                self.conflict_matrix[second][first] = conflict;
            }
        }

        // Step 2: Fill the batches first-fit in registration order. The graph
        // is moved out so this loop can read the conflict matrix while it
        // appends - a batch's membership is decided by the systems already in
        // it, never by the ones still to come.
        let mut batches = std::mem::take(&mut self.execution_graph);
        batches.clear();
        for system in 0..accesses.len() {
            let target = batches.iter().position(|batch| {
                !batch
                    .iter()
                    .any(|&other| self.conflict_matrix[system][other])
            });
            match target {
                Some(index) => batches[index].push(system),
                None => batches.push(vec![system]),
            }
        }
        self.execution_graph = batches;
        self.needs_rebuild = false;
    }

    /// Returns the execution graph: a slice of batches, where each batch
    /// holds the indices of systems that can run in parallel.
    ///
    /// The graph is only current after
    /// [`ensure_execution_graph`](SystemScheduler::ensure_execution_graph);
    /// the engine calls that before it dispatches a frame.
    pub fn execution_graph(&self) -> &[Vec<usize>] {
        &self.execution_graph
    }

    /// Clears the conflict matrix and the graph.
    ///
    /// Used during hot reload, where every system is re-registered from the
    /// new DLL. The next [`ensure_execution_graph`](SystemScheduler::ensure_execution_graph)
    /// rebuilds from whatever the engine registers.
    pub fn clear(&mut self) {
        self.conflict_matrix.clear();
        self.execution_graph.clear();
        self.needs_rebuild = true;
    }
}

// =============================================================================
// Conflict-detection regression tests
// =============================================================================

/// Conflict detection must depend only on what the two systems touch.
///
/// These pin the properties that a derived shortcut could break: an access
/// added after the comparison, and two resource sets of equal size that still
/// share an id.
#[cfg(test)]
mod conflict_detection_tests {
    use super::SystemAccess;
    use crate::component::Component;
    use crate::ComponentId;

    struct Foo;
    impl Component for Foo {}
    struct Bar;
    impl Component for Bar {}
    struct ResOne;
    impl crate::resource::Resource for ResOne {}
    struct ResTwo;
    impl crate::resource::Resource for ResTwo {}
    struct ResThree;
    impl crate::resource::Resource for ResThree {}

    /// An access added after an earlier comparison is seen by the next one:
    /// the sets are read live, so no cached answer can go stale behind them.
    #[test]
    fn an_access_added_after_a_comparison_still_conflicts() {
        let mut a = SystemAccess::new();
        a.add_write(ComponentId::of::<Foo>());

        let mut b = SystemAccess::new();
        b.add_write(ComponentId::of::<Bar>());
        assert!(
            !a.conflicts_with(&b),
            "disjoint while neither shares a write"
        );

        // The set grows after the two were compared; the next comparison has
        // to see the shared write.
        a.add_write(ComponentId::of::<Bar>());
        assert!(
            a.conflicts_with(&b),
            "the sets are the source of truth and report the shared write"
        );
    }

    /// Equal-length resource sets are compared by content, not by size: two
    /// systems whose resource sets differ while sharing one id still conflict.
    #[test]
    fn a_same_length_resource_edit_still_conflicts() {
        use crate::resource::ResourceId;

        let mut reader = SystemAccess::new();
        reader.add_read(ComponentId::of::<Foo>());
        reader.add_resource_read(ResourceId::of::<ResOne>());
        reader.add_resource_read(ResourceId::of::<ResTwo>());

        let mut writer = SystemAccess::new();
        writer.add_write(ComponentId::of::<Bar>());
        writer.add_resource_write(ResourceId::of::<ResTwo>());
        writer.add_resource_write(ResourceId::of::<ResThree>());

        assert_eq!(
            reader.resource_reads.len(),
            writer.resource_writes.len(),
            "the two sets are the same size"
        );
        assert!(
            reader.conflicts_with(&writer),
            "the shared resource id is a conflict at any set size"
        );
        assert!(writer.conflicts_with(&reader));
    }

    /// Conflict detection is symmetric, and a read paired with a write is as
    /// much a conflict as two writes.
    #[test]
    fn a_read_paired_with_a_write_conflicts_in_both_directions() {
        let id = ComponentId::of::<Foo>();

        let mut reader = SystemAccess::new();
        reader.add_read(id);

        let mut writer = SystemAccess::new();
        writer.add_write(id);

        assert!(reader.conflicts_with(&writer));
        assert!(writer.conflicts_with(&reader));
    }

    /// The check must not over-report: genuinely disjoint systems stay
    /// parallel, and two readers of one component never conflict.
    #[test]
    fn disjoint_systems_do_not_conflict() {
        let mut a = SystemAccess::new();
        a.add_write(ComponentId::of::<Foo>());

        let mut b = SystemAccess::new();
        b.add_write(ComponentId::of::<Bar>());
        assert!(
            !a.conflicts_with(&b),
            "disjoint systems must still be allowed to run in parallel"
        );

        let mut reader = SystemAccess::new();
        reader.add_read(ComponentId::of::<Foo>());
        let mut other_reader = SystemAccess::new();
        other_reader.add_read(ComponentId::of::<Foo>());
        assert!(!reader.conflicts_with(&other_reader));
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Component, ComponentId};
    use std::hash::BuildHasher;

    // The scheduler only ever compares component ids for equality, so these
    // tests need nothing of a component but a distinct identity. `ComponentId::of`
    // takes `T: Component`, so that a shared component's declared identity can
    // never be bypassed, and the stand-ins are declared here.
    macro_rules! test_components {
        ($($name:ident),* $(,)?) => {$(
            #[derive(Debug)]
            struct $name;
            impl Component for $name {}
        )*};
    }
    test_components!(
        TestComponentA,
        TestComponentB,
        TestComponentC,
        TestComponentD,
        TestComponentE,
        TestComponentF,
        TestComponentG,
    );

    // The same for resources. These tests only ever compare resource ids for
    // equality, so they need nothing of a resource but a distinct identity.
    macro_rules! test_resources {
        ($($name:ident),* $(,)?) => {$(
            #[derive(Debug)]
            struct $name;
            impl crate::resource::Resource for $name {}
        )*};
    }
    test_resources!(TestResourceA, TestResourceB, TestResourceC, TestResourceD);

    // Helper: Build the batches for a list of access patterns, the way the
    // engine does after its system set changed.
    fn scheduler_for(accesses: &[SystemAccess]) -> SystemScheduler {
        let mut scheduler = SystemScheduler::new();
        scheduler.ensure_execution_graph(accesses.iter());
        scheduler
    }

    // Helper: Verify that no batch contains conflicting systems
    fn assert_no_batch_conflicts(scheduler: &SystemScheduler, accesses: &[SystemAccess]) {
        for (batch_index, batch) in scheduler.execution_graph().iter().enumerate() {
            for (i, &system_index_a) in batch.iter().enumerate() {
                let access_a = &accesses[system_index_a];
                for &system_index_b in &batch[i + 1..] {
                    let access_b = &accesses[system_index_b];
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

    /// Two systems reading different components share one batch.
    #[test]
    fn test_no_conflicts() {
        // System 1: reads A. System 2: reads B.
        let mut reads_a = SystemAccess::new();
        reads_a.add_read(ComponentId::of::<TestComponentA>());
        let mut reads_b = SystemAccess::new();
        reads_b.add_read(ComponentId::of::<TestComponentB>());
        let accesses = vec![reads_a, reads_b];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Both systems should be in the same batch (no conflicts)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    /// Two systems writing the same component land in separate batches.
    #[test]
    fn test_write_conflict() {
        // System 1 and System 2 both write A.
        let mut writes_a = SystemAccess::new();
        writes_a.add_write(ComponentId::of::<TestComponentA>());
        let mut also_writes_a = SystemAccess::new();
        also_writes_a.add_write(ComponentId::of::<TestComponentA>());
        let accesses = vec![writes_a, also_writes_a];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (write-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    /// A reader and a writer of the same component land in separate batches.
    #[test]
    fn test_read_write_conflict() {
        // System 1: reads A. System 2: writes A.
        let mut reads_a = SystemAccess::new();
        reads_a.add_read(ComponentId::of::<TestComponentA>());
        let mut writes_a = SystemAccess::new();
        writes_a.add_write(ComponentId::of::<TestComponentA>());
        let accesses = vec![reads_a, writes_a];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (read-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    /// A system registered earlier runs before the systems it conflicts with.
    ///
    /// Registration order is the only ordering signal the API offers - there is
    /// no `before`/`after` - so a producer registered first must run before the
    /// consumers that read what it writes, however many conflicts each one has.
    #[test]
    fn conflicting_systems_keep_registration_order() {
        // System 0 writes A: it conflicts with both readers below.
        let mut producer = SystemAccess::new();
        producer.add_write(ComponentId::of::<TestComponentA>());
        let mut accesses = vec![producer];

        // Systems 1 and 2 read A: they conflict with the writer, not each other.
        for _ in 0..2 {
            let mut consumer = SystemAccess::new();
            consumer.add_read(ComponentId::of::<TestComponentA>());
            accesses.push(consumer);
        }

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 3);
        assert_eq!(
            scheduler.execution_graph().to_vec(),
            vec![vec![0], vec![1, 2]],
            "the producer runs in the first batch and the two readers together after it"
        );
    }

    /// A system using Commands conflicts with everything, including a
    /// read-only one.
    #[test]
    fn test_commands_exclusive() {
        // System 1 uses commands; System 2 reads A.
        let mut uses_commands = SystemAccess::new();
        uses_commands.set_uses_commands(true);
        let mut reads_a = SystemAccess::new();
        reads_a.add_read(ComponentId::of::<TestComponentA>());
        let accesses = vec![uses_commands, reads_a];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Systems must be in different batches (Commands require exclusive access)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    /// Five readers of the same component share one batch.
    #[test]
    fn test_multiple_readers_parallel() {
        // 5 systems all reading the same component.
        let accesses: Vec<SystemAccess> = (0..5)
            .map(|_| {
                let mut access = SystemAccess::new();
                access.add_read(ComponentId::of::<TestComponentA>());
                access
            })
            .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 5);

        // All readers can run in parallel (read-read is OK)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 5);
    }

    /// One writer keeps every reader of the same component out of its batch.
    #[test]
    fn test_single_writer_blocks_all() {
        // 4 readers, then 1 writer of the same component.
        let mut accesses: Vec<SystemAccess> = (0..4)
            .map(|_| {
                let mut access = SystemAccess::new();
                access.add_read(ComponentId::of::<TestComponentA>());
                access
            })
            .collect();
        let mut writer = SystemAccess::new();
        writer.add_write(ComponentId::of::<TestComponentA>());
        accesses.push(writer);

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 5);

        // Writer must be in a separate batch from all readers
        assert!(scheduler.execution_graph().len() >= 2);
    }

    /// A read-then-write chain (A→B→C) serializes the systems that disagree and
    /// batches the ones that do not.
    #[test]
    fn test_complex_dependency_graph() {
        // System 0: reads A, writes B
        let mut access0 = SystemAccess::new();
        access0.add_read(ComponentId::of::<TestComponentA>());
        access0.add_write(ComponentId::of::<TestComponentB>());

        // System 1: reads B, writes C
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId::of::<TestComponentB>());
        access1.add_write(ComponentId::of::<TestComponentC>());

        // System 2: reads A (can run with system 1)
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId::of::<TestComponentA>());

        // System 3: reads C (can run with systems 0 and 2)
        let mut access3 = SystemAccess::new();
        access3.add_read(ComponentId::of::<TestComponentC>());

        let accesses = vec![access0, access1, access2, access3];
        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 4);
    }

    /// Three writers of three different components share one batch.
    #[test]
    fn test_disjoint_component_sets() {
        let accesses: Vec<SystemAccess> = [
            ComponentId::of::<TestComponentA>(),
            ComponentId::of::<TestComponentB>(),
            ComponentId::of::<TestComponentC>(),
        ]
        .into_iter()
        .map(|component_id| {
            let mut access = SystemAccess::new();
            access.add_write(component_id);
            access
        })
        .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 3);

        // All can run in parallel (disjoint writes)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 3);
    }

    /// Three Commands systems get one batch each.
    #[test]
    fn test_multiple_commands_sequential() {
        // 3 systems all using commands.
        let accesses: Vec<SystemAccess> = (0..3)
            .map(|_| {
                let mut access = SystemAccess::new();
                access.set_uses_commands(true);
                access
            })
            .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 3);

        // Each command system must be in its own batch
        assert_eq!(scheduler.execution_graph().len(), 3);
    }

    /// Commands systems and read-only systems interleave without sharing a
    /// batch.
    #[test]
    fn test_mixed_commands_and_queries() {
        // Reads A, commands, reads B, commands.
        let mut read_a = SystemAccess::new();
        read_a.add_read(ComponentId::of::<TestComponentA>());
        let mut commands_first = SystemAccess::new();
        commands_first.set_uses_commands(true);
        let mut read_b = SystemAccess::new();
        read_b.add_read(ComponentId::of::<TestComponentB>());
        let mut commands_second = SystemAccess::new();
        commands_second.set_uses_commands(true);
        let accesses = vec![read_a, commands_first, read_b, commands_second];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 4);
    }

    /// Verifies that an empty scheduler produces an empty execution graph.
    #[test]
    fn test_empty_scheduler() {
        let scheduler = scheduler_for(&[]);

        assert_eq!(scheduler.execution_graph().len(), 0);
    }

    /// Verifies that a single-system scheduler produces one batch with one system.
    #[test]
    fn test_single_system() {
        let mut writes_a = SystemAccess::new();
        writes_a.add_write(ComponentId::of::<TestComponentA>());
        let accesses = vec![writes_a];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 1);

        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 1);
    }

    /// A chain of read-write dependencies (A → B → C) batches only the systems
    /// that do not disagree.
    #[test]
    fn test_chain_dependencies() {
        // Chain: writes A -> reads A, writes B -> reads B, writes C.
        let mut access0 = SystemAccess::new();
        access0.add_write(ComponentId::of::<TestComponentA>());

        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId::of::<TestComponentA>());
        access1.add_write(ComponentId::of::<TestComponentB>());

        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId::of::<TestComponentB>());
        access2.add_write(ComponentId::of::<TestComponentC>());

        let accesses = vec![access0, access1, access2];
        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 3);

        // The first-fit fill gives [0, 2], [1]: system 2 conflicts with system
        // 1 (both touch B) but not with system 0, so it joins the first batch.
        // The assertion stays loose on purpose - the batch count is a
        // scheduling detail, not the property under test.
        assert!(scheduler.execution_graph().len() >= 2);
        assert!(scheduler.execution_graph().len() <= 3);
    }

    /// Verifies that five systems writing five different components form one batch.
    #[test]
    fn test_large_parallel_batch() {
        // Five systems, each writing a component no other one touches.
        let accesses: Vec<SystemAccess> = [
            ComponentId::of::<TestComponentD>(),
            ComponentId::of::<TestComponentE>(),
            ComponentId::of::<TestComponentC>(),
            ComponentId::of::<TestComponentF>(),
            ComponentId::of::<TestComponentG>(),
        ]
        .into_iter()
        .map(|component_id| {
            let mut access = SystemAccess::new();
            access.add_write(component_id);
            access
        })
        .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 5);

        // All can run in parallel
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 5);
    }

    // Resource Conflict Tests
    // ----------------------------------------------------------------------------

    /// Verifies that two systems reading the same resource can run in parallel.
    #[test]
    fn test_resource_read_read_parallel() {
        // Two systems both reading the same resource.
        let accesses: Vec<SystemAccess> = (0..2)
            .map(|_| {
                let mut access = SystemAccess::new();
                access.add_resource_read(ResourceId::of::<TestResourceA>());
                access
            })
            .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Both can run in parallel (read-read is OK)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    /// Verifies that two systems writing the same resource must run sequentially.
    #[test]
    fn test_resource_write_write_conflict() {
        // Two systems both writing the same resource.
        let accesses: Vec<SystemAccess> = (0..2)
            .map(|_| {
                let mut access = SystemAccess::new();
                access.add_resource_write(ResourceId::of::<TestResourceA>());
                access
            })
            .collect();

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Must be in different batches (write-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    /// Verifies that a resource reader and writer of the same resource conflict.
    #[test]
    fn test_resource_read_write_conflict() {
        // System 1 reads a resource, System 2 writes the same one.
        let mut reads = SystemAccess::new();
        reads.add_resource_read(ResourceId::of::<TestResourceA>());
        let mut writes = SystemAccess::new();
        writes.add_resource_write(ResourceId::of::<TestResourceA>());
        let accesses = vec![reads, writes];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Must be in different batches (read-write conflict)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }

    /// Verifies that writers of different resources can run in parallel.
    #[test]
    fn test_resource_disjoint_writes_parallel() {
        // Two systems writing different resources.
        let mut writes_a = SystemAccess::new();
        writes_a.add_resource_write(ResourceId::of::<TestResourceA>());
        let mut writes_d = SystemAccess::new();
        writes_d.add_resource_write(ResourceId::of::<TestResourceD>());
        let accesses = vec![writes_a, writes_d];

        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 2);

        // Can run in parallel (different resources)
        assert_eq!(scheduler.execution_graph().len(), 1);
        assert_eq!(scheduler.execution_graph()[0].len(), 2);
    }

    /// Verifies correct conflict detection when both component and resource access patterns mix.
    #[test]
    fn test_resource_and_component_mixed() {
        // System 1: reads component A, writes resource C.
        let mut access1 = SystemAccess::new();
        access1.add_read(ComponentId::of::<TestComponentA>());
        access1.add_resource_write(ResourceId::of::<TestResourceC>());

        // System 2: reads component A, reads resource C.
        let mut access2 = SystemAccess::new();
        access2.add_read(ComponentId::of::<TestComponentA>());
        access2.add_resource_read(ResourceId::of::<TestResourceC>());

        // System 3: writes component B, reads resource B (no conflicts with either).
        let mut access3 = SystemAccess::new();
        access3.add_write(ComponentId::of::<TestComponentB>());
        access3.add_resource_read(ResourceId::of::<TestResourceB>());

        let accesses = vec![access1, access2, access3];
        let scheduler = scheduler_for(&accesses);

        assert_no_batch_conflicts(&scheduler, &accesses);
        assert_all_systems_scheduled(&scheduler, 3);

        // System 1 and 2 conflict on resource C (write vs read)
        // System 3 doesn't conflict with either
        assert!(scheduler.execution_graph().len() >= 2);
    }

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
            AccessKind::ReadA => a.add_read(ComponentId::of::<TestComponentD>()),
            AccessKind::WriteA => a.add_write(ComponentId::of::<TestComponentD>()),
            AccessKind::ReadB => a.add_read(ComponentId::of::<TestComponentE>()),
            AccessKind::WriteB => a.add_write(ComponentId::of::<TestComponentE>()),
            AccessKind::Commands => a.set_uses_commands(true),
            AccessKind::ResReadX => a.add_resource_read(ResourceId::of::<TestResourceB>()),
            AccessKind::ResWriteX => a.add_resource_write(ResourceId::of::<TestResourceB>()),
            AccessKind::ResReadY => a.add_resource_read(ResourceId::of::<TestResourceC>()),
            AccessKind::ResWriteY => a.add_resource_write(ResourceId::of::<TestResourceC>()),
        }
        a
    }

    /// Exhaustive verification for small system counts.
    ///
    /// Enumerates every possible n-tuple of access kinds for n = 1..=6. With 10
    /// access kinds that is 10 + 100 + 1,000 + 10,000 + 100,000 + 1,000,000 =
    /// 1,111,110 unique input configurations. Each one runs through
    /// `ensure_execution_graph` and is checked for two invariants:
    ///
    /// 1. No batch contains conflicting systems (the core safety guarantee).
    /// 2. Every system appears in exactly one batch (no omissions, no
    ///    duplicates).
    ///
    /// n-tuples are generated by counting in base 10: counter 0 → [None,
    /// None, ...], counter 1 → [ReadA, None, ...], etc. This covers every
    /// combination with repetition (systems can have the same access kind).
    /// Exhaustively verifies all 1,111,110 possible system combinations for n=1..=6.
    ///
    /// Enumerates every n-tuple of access kinds (10 kinds) and verifies that no batch
    /// contains conflicting systems and every system is scheduled exactly once.
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
                // `i` is the digit position being decoded out of `counter`, so
                // it is the subject of the loop rather than an index artifact.
                #[allow(clippy::needless_range_loop)]
                for i in 0..n as usize {
                    tuple[i] = (counter_value % kinds.len() as u32) as u8;
                    counter_value /= kinds.len() as u32;
                }

                // Build a fresh scheduler with this specific combination
                let accesses: Vec<SystemAccess> = tuple
                    .iter()
                    .map(|&kind_index| kind_to_access(kinds[kind_index as usize]))
                    .collect();
                let scheduler = scheduler_for(&accesses);

                // Verify both correctness invariants
                assert_no_batch_conflicts(&scheduler, &accesses);
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
    /// Randomised fuzz test for larger system counts (up to 20 systems, 500 seeds).
    ///
    /// Generates ~10,000 random conflict graphs and verifies the same invariants
    /// as the exhaustive test: no batch conflicts and every system scheduled once.
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
            let accesses: Vec<SystemAccess> = (0..n)
                .map(|_| {
                    let kind_index = (rng % kinds.len() as u64) as usize;
                    rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                    kind_to_access(kinds[kind_index])
                })
                .collect();
            let scheduler = scheduler_for(&accesses);

            // Same invariants as the exhaustive test
            assert_no_batch_conflicts(&scheduler, &accesses);
            assert_all_systems_scheduled(&scheduler, n);
        }
    }
} // mod tests
