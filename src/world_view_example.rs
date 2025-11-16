use crate::example::{BehaviorType, ScriptComponent};

/// World stores components directly - no RefCell needed
pub struct World {
    pub components: Vec<ScriptComponent>,
}

/// Read-only view that provides immutable access to all components
/// Cannot modify the Vec structure (no push/pop/remove)
pub struct WorldView<'a> {
    components: &'a [ScriptComponent],
}

impl<'a> WorldView<'a> {
    /// Borrow a component immutably
    pub fn get_component(&self, index: usize) -> &ScriptComponent {
        &self.components[index]
    }

    pub fn len(&self) -> usize {
        self.components.len()
    }
}

impl World {
    pub fn new() -> Self {
        World {
            components: Vec::new(),
        }
    }

    pub fn add_component(&mut self, component: ScriptComponent) {
        self.components.push(component);
    }

    /// Update with unsafe - bypassing borrow checker with raw pointers
    pub fn update_scripts(&mut self) {
        for i in 0..self.components.len() {
            // SAFETY: We create a shared slice view and a mutable reference to current component.
            // This violates Stacked Borrows but works in practice because:
            // 1. The view is read-only (&[T]) - can't modify Vec structure
            // 2. Index-based iteration means no dangling references
            // 3. We don't actually read 'current' through the view
            unsafe {
                let components_ptr = self.components.as_ptr();
                let components_len = self.components.len();

                // Create immutable view from raw pointer
                let view = WorldView {
                    components: std::slice::from_raw_parts(components_ptr, components_len),
                };

                // Get mutable reference to current component
                let current_ptr = self.components.as_mut_ptr().add(i);
                let current = &mut *current_ptr;

                current.update_with_world_view(&view);
            }
        }
    }
}

impl ScriptComponent {
    /// Update that accesses other components through WorldView
    pub fn update_with_world_view(&mut self, view: &WorldView) {
        println!("\n🔄 Component update (no RefCell pattern):");
        println!("   Initial value: {}", self.some_value);

        // Read from ALL components (including potentially self - causes UB!)
        println!("   Reading from other components:");
        for i in 0..view.len() {
            let other = view.get_component(i);
            println!("     - Component {}: value={}", i, other.some_value);
            self.some_value += other.some_value * 0.1;
        }

        // Also demonstrate we can mutate self freely
        self.some_value *= 1.5;

        println!("   Final value: {}", self.some_value);
    }
}

pub fn run_world_view_test() {
    let mut world = World::new();

    world.add_component(ScriptComponent {
        some_value: 100.0,
        behavior_type: BehaviorType::Safe,
    });

    world.add_component(ScriptComponent {
        some_value: 50.0,
        behavior_type: BehaviorType::Safe,
    });

    world.add_component(ScriptComponent {
        some_value: 25.0,
        behavior_type: BehaviorType::Safe,
    });

    println!("Created world with {} components", world.components.len());

    // Update with unsafe pointer manipulation
    world.update_scripts();

    for (i, component) in world.components.iter().enumerate() {
        println!("Component {}: value={}", i, component.some_value);
    }
}
