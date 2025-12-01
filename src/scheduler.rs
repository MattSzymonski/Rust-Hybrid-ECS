// ============================================================================
// Parallel System Scheduler
// ============================================================================
//! Automatic dependency analysis and parallel system execution.
//!
//! This module analyzes component access patterns to build a dependency graph
//! and executes systems in parallel batches when safe to do so.

use crate::component::ComponentId;
use std::collections::{HashMap, HashSet};

/// Component access information for a system
#[derive(Debug, Clone, Default)]
pub struct SystemAccess {
    /// Components read immutably (&T)
    pub reads: HashSet<ComponentId>,
    /// Components written mutably (&mut T)
    pub writes: HashSet<ComponentId>,
    /// Whether the system uses Commands (requires exclusive World access)
    pub uses_commands: bool,
}

impl SystemAccess {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_read(&mut self, component_id: ComponentId) {
        self.reads.insert(component_id);
    }

    pub fn add_write(&mut self, component_id: ComponentId) {
        self.writes.insert(component_id);
    }

    pub fn set_uses_commands(&mut self, uses: bool) {
        self.uses_commands = uses;
    }

    /// Check if this system conflicts with another
    ///
    /// Two systems conflict if:
    /// - Either uses Commands (Commands require exclusive access)
    /// - Both write to the same component (write-write conflict)
    /// - One writes and the other reads the same component (read-write conflict)
    ///
    /// Multiple systems can read the same component simultaneously (read-read is OK)
    pub fn conflicts_with(&self, other: &SystemAccess) -> bool {
        // Commands require exclusive World access
        if self.uses_commands || other.uses_commands {
            return true;
        }

        // Check for write-write conflicts
        if !self.writes.is_disjoint(&other.writes) {
            return true;
        }

        // Check for read-write conflicts (write on one side, read on the other)
        if !self.writes.is_disjoint(&other.reads) {
            return true;
        }
        if !self.reads.is_disjoint(&other.writes) {
            return true;
        }

        // No conflicts - systems can run in parallel
        false
    }
}

/// Execution scheduler that builds parallel batches from system dependencies
pub struct SystemScheduler {
    /// System count
    system_count: usize,
    /// Access patterns for each system
    access_patterns: Vec<SystemAccess>,
    /// Computed execution graph: Vec of batches, each batch contains system indices that can run in parallel
    execution_graph: Vec<Vec<usize>>,
}

impl SystemScheduler {
    /// Create a new scheduler
    pub fn new() -> Self {
        Self {
            system_count: 0,
            access_patterns: Vec::new(),
            execution_graph: Vec::new(),
        }
    }

    /// Register a system with its access pattern
    pub fn register_system(&mut self, access: SystemAccess) -> usize {
        let index = self.system_count;
        self.access_patterns.push(access);
        self.system_count += 1;
        index
    }

    /// Build the execution graph based on dependencies
    ///
    /// Algorithm:
    /// 1. Start with all systems unscheduled
    /// 2. For each batch:
    ///    - Find all systems that don't conflict with each other
    ///    - Add them to the batch
    ///    - Mark them as scheduled
    /// 3. Repeat until all systems are scheduled
    pub fn build_execution_graph(&mut self) {
        self.execution_graph.clear();

        let mut scheduled = vec![false; self.system_count];
        let mut scheduled_count = 0;

        while scheduled_count < self.system_count {
            let mut batch = Vec::new();

            // Try to add each unscheduled system to the current batch
            for i in 0..self.system_count {
                if scheduled[i] {
                    continue;
                }

                // Check if this system conflicts with any system already in the batch
                let mut conflicts = false;
                for &j in &batch {
                    if self.access_patterns[i].conflicts_with(&self.access_patterns[j]) {
                        conflicts = true;
                        break;
                    }
                }

                if !conflicts {
                    batch.push(i);
                    scheduled[i] = true;
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
        for (batch_idx, batch) in self.execution_graph.iter().enumerate() {
            println!("Batch {}: {} systems (parallel)", batch_idx, batch.len());
            for &sys_idx in batch {
                let name = system_names.get(sys_idx).unwrap_or(&"<unknown>");
                let access = &self.access_patterns[sys_idx];
                println!(
                    "  - {} (reads: {}, writes: {}, commands: {})",
                    name,
                    access.reads.len(),
                    access.writes.len(),
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
    use std::any::TypeId;

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

        // Systems must be in different batches (Commands require exclusive access)
        assert_eq!(scheduler.execution_graph().len(), 2);
    }
}
