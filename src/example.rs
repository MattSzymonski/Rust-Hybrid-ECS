use crate::ecs_core::World;

pub struct ScriptComponent {
    pub some_value: f32,
    pub behavior_type: BehaviorType,
}

#[derive(Debug, Clone, Copy)]
pub enum BehaviorType {
    Safe,
    DangerousModifyDuringIteration,
    DangerousAliasing,
    DangerousAliasingObvious,
    DangerousIteratorInvalidation,
    DangerousClear,
    DangerousTransmute,
}

// ScriptComponent trait - for components that have update logic
impl ScriptComponent {
    pub fn update(&mut self, world: &mut World) {
        match self.behavior_type {
            BehaviorType::Safe => {
                // Safe operation - just modify own state
                self.some_value += 1.0;
                println!("Safe update: value = {}", self.some_value);
            }
            BehaviorType::DangerousModifyDuringIteration => {
                // DANGER 1: Modify the components vector while iterating over it
                // This can cause iterator invalidation
                println!("\n🔥 DANGER: Adding component during iteration!");
                self.some_value += 1.0;
                world.add_component(ScriptComponent {
                    some_value: 999.0,
                    behavior_type: BehaviorType::Safe,
                });
                // This may cause vector reallocation! or not, depending on capacity, what is undefined behavior!
                // In such case iterator will iterate over freed memory!
            }
            BehaviorType::DangerousAliasing => {
                // DANGER 2: Create multiple mutable references to the same component
                // This violates Rust's aliasing rules
                println!("\n🔥 DANGER: Creating aliasing mutable references!");

                // We're already holding `&mut self` (the current component)
                // Now let's try to get another mutable reference to a component
                // (possibly the same one)
                if let Some(other_component) = world.get_component_mut(0) {
                    println!(
                        "Before: self.some_value = {}, other.some_value = {}",
                        self.some_value, other_component.some_value
                    );

                    // Modify both - if they're the same component, this is UB!
                    self.some_value *= 100.0;
                    other_component.some_value += 200.0;

                    // The compiler might:
                    // - Reorder these operations
                    // - Cache values in registers
                    // - Eliminate "redundant" reads/writes
                    // - Generate completely wrong code

                    // What the compiler might generate:
                    // let temp = self.some_value;         // Load: 10
                    // temp = temp * 100.0;                // Calculate: 1000
                    // other_component.some_value += 200.0; // Write: 10 + 200 = 210
                    // self.some_value = temp;             // Write: 1000 (overwrites 210!)
                    // Result: 1000 instead of 1200!

                    // Rust compiles to LLVM IR, which has the noalias attribute. This tells LLVM:
                    // "This pointer is the ONLY way to access this memory"
                    // LLVM can perform aggressive optimizations
                    // If you lie (by creating aliases), LLVM can generate anything

                    println!(
                        "After: self.some_value = {}, other.some_value = {}",
                        self.some_value, other_component.some_value
                    );

                    // If self and other_component point to the same memory,
                    // we've violated Rust's aliasing rules and the behavior is undefined
                }
            }
            BehaviorType::DangerousAliasingObvious => {
                // DANGER 2b: Even more obvious aliasing - read and write simultaneously
                println!("\n🔥 DANGER: Reading through one ref while writing through another!");

                println!("Initial self.some_value = {}", self.some_value);

                if let Some(other_component) = world.get_component_mut(0) {
                    // Hold a reference to other_component's value
                    let other_value = &other_component.some_value;

                    // Now modify self (which might be the same component!)
                    for i in 0..5 {
                        self.some_value += 1.0;
                        println!(
                            "  Loop {}: self.some_value = {}, reading other = {}",
                            i, self.some_value, other_value
                        );
                        // NOTE: In debug builds, you might see updated values because
                        // the compiler doesn't optimize. In release builds with -O3,
                        // the compiler might assume other_value is constant (since it's
                        // an immutable reference) and cache it, showing stale data!
                        // This is UNDEFINED BEHAVIOR - the compiler can do anything!
                        // If they're the same, we're reading stale data!
                        // The compiler might optimize based on the assumption that
                        // other_value doesn't change, but it does!
                    }
                }
            }
            BehaviorType::DangerousIteratorInvalidation => {
                // DANGER 3: Swap or remove components during iteration
                println!("\n🔥 DANGER: Removing component during iteration!");
                self.some_value += 1.0;

                // Try to remove a component - this will invalidate the iterator
                if world.component_count() > 1 {
                    world.remove_component(0);
                    println!("Iterator invalidated!");
                }

                // Iterator holds:
                // - A pointer to the current element
                // - Internal state about position/remaining elements
                // - Assumptions about the vector's structure (length, layout)

                // Removing element breaks the iterator's state:
                // - It expects 3 elements, but there are only 2
                // - It's pointing at what was index 1, now index 0
                // - Its position counter is out of sync
                // - The memory layout has changed

                // Consequences. Iterator now may:
                // - Crash if it goes out of bounds
                // - Skip elements (most common)
                // - Process the same element twice
                // - Read invalid indices
            }
            BehaviorType::DangerousTransmute => {
                // DANGER 5: Type confusion via transmute
                println!("\n🔥 DANGER: Using transmute to violate type safety!");

                // Imagine you're trying to "cast" or reinterpret component data
                // This is common in type-erased ECS systems

                println!("Original self.some_value: {}", self.some_value);

                // Example 1: Transmute float to u32 (breaks bit representation)
                let value_as_bits: u32 = unsafe { std::mem::transmute(self.some_value) };
                println!("Transmuted to u32 bits: 0x{:08X}", value_as_bits);

                // Example 2: Attempt to transmute references (VERY DANGEROUS)
                // Trying to reinterpret a component as a different type
                unsafe {
                    // This is what happens in type-erased ECS when you get the type wrong
                    // Pretend we're treating this ScriptComponent as if it were a different component

                    #[repr(C)]
                    struct FakeComponent {
                        value1: u64,
                        value2: u64,
                    }

                    // Cast our &mut ScriptComponent to &mut FakeComponent
                    // This is UB because:
                    // 1. Size mismatch (ScriptComponent is not same size as FakeComponent)
                    // 2. Layout mismatch (different field types)
                    // 3. Alignment issues
                    let fake_ref = &mut *(self as *mut ScriptComponent as *mut FakeComponent);

                    println!("Reading as FakeComponent:");
                    println!("  value1 (garbage): 0x{:016X}", fake_ref.value1);
                    println!("  value2 (garbage): 0x{:016X}", fake_ref.value2);

                    // Even worse: writing through the wrong type
                    fake_ref.value1 = 0xDEADBEEFCAFEBABE;

                    println!("\n⚠️  We just wrote 8 bytes through a pointer that only points");
                    println!(
                        "     to {} bytes! This corrupted adjacent memory!",
                        std::mem::size_of::<ScriptComponent>()
                    );
                }

                // Try to read the corrupted value
                println!(
                    "After transmute corruption: self.some_value = {}",
                    self.some_value
                );
                println!("Memory corruption likely occurred!");

                // In a real ECS, this could:
                // - Corrupt other components
                // - Corrupt the World's internal state
                // - Crash when accessing "components" vector (vtable corruption)
                // - Create security vulnerabilities
            }
            BehaviorType::DangerousClear => {
                // DANGER 4: Clear all components while iterating
                println!("\n🔥 DANGER: Clearing all components during iteration!");
                println!("Components before clear: {}", world.component_count());

                // This will free the memory of ALL components, including the one
                // we're currently executing (self)!
                world.clear_components();

                println!("Components after clear: {}", world.component_count());

                // Accessing self after this point is use-after-free!
                println!(
                    "Trying to access self.some_value after clear: {}",
                    self.some_value
                );
                // ☠️ This is undefined behavior - we're accessing freed memory!
            }
        }
    }
}

pub fn run_unsafety_test() {
    println!("=== UNSAFE SCENARIOS DEMONSTRATION ===\n");
    println!("These scenarios show why passing &mut World to update() is unsafe.\n");

    // Test 1: Safe scenario (baseline)
    println!("\n--- Test 1: Safe Scenario (Baseline) ---");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 10.0,
            behavior_type: BehaviorType::Safe,
        });
        world.add_component(ScriptComponent {
            some_value: 20.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("✅ Safe scenario completed successfully");
    }

    // Test 2: Modify during iteration
    println!("\n--- Test 2: Modify During Iteration ---");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 10.0,
            behavior_type: BehaviorType::DangerousModifyDuringIteration,
        });
        world.add_component(ScriptComponent {
            some_value: 20.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("Final component count: {}", world.component_count());
        println!("⚠️  This may or may not crash depending on vector reallocation");
    }

    // Test 3: Aliasing violation
    println!("\n--- Test 3: Aliasing Violation ---");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 10.0,
            behavior_type: BehaviorType::DangerousAliasing,
        });
        world.add_component(ScriptComponent {
            some_value: 20.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("⚠️  Aliasing violation occurred - undefined behavior!");
    }

    // Test 3b: Aliasing violation (more obvious)
    println!("\n--- Test 3b: Aliasing Violation (Stale Reads) ---");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 100.0,
            behavior_type: BehaviorType::DangerousAliasingObvious,
        });
        world.add_component(ScriptComponent {
            some_value: 200.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("⚠️  This demonstrates stale reads due to aliasing!");
    }

    // Test 4: Iterator invalidation
    println!("\n--- Test 4: Iterator Invalidation ---");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 10.0,
            behavior_type: BehaviorType::DangerousIteratorInvalidation,
        });
        world.add_component(ScriptComponent {
            some_value: 20.0,
            behavior_type: BehaviorType::Safe,
        });
        world.add_component(ScriptComponent {
            some_value: 30.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("⚠️  Iterator was invalidated during iteration");
    }

    // Test 5: Clear during iteration (MOST DANGEROUS)
    println!("\n--- Test 5: Clear During Iteration (Use-After-Free) ---");
    println!("⚠️  WARNING: This will likely cause undefined behavior or crash!");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 10.0,
            behavior_type: BehaviorType::DangerousClear,
        });
        world.add_component(ScriptComponent {
            some_value: 20.0,
            behavior_type: BehaviorType::Safe,
        });
        world.update_scripts();
        println!("☠️  If you see this, you accessed freed memory (use-after-free)!");
    }

    // Test 6: Transmute type confusion
    println!("\n--- Test 6: Transmute Type Confusion ---");
    println!("⚠️  WARNING: This will corrupt memory!");
    {
        let world = &mut World::new();
        world.add_component(ScriptComponent {
            some_value: 42.0,
            behavior_type: BehaviorType::DangerousTransmute,
        });
        world.add_component(ScriptComponent {
            some_value: 100.0,
            behavior_type: BehaviorType::Safe,
        });
        println!(
            "Components before transmute corruption: {}",
            world.component_count()
        );
        world.update_scripts();
        println!("Components after transmute: {}", world.component_count());
        println!("☠️  Memory corruption occurred! Adjacent memory may be corrupted!");
    }

    println!("\n=== Test Complete ===");
    println!("\nSummary of dangers:");
    println!("1. Iterator invalidation - adding/removing during iteration");
    println!("2. Aliasing violations - multiple mutable refs to same data");
    println!("3. Use-after-free - clearing memory being accessed");
    println!("4. Transmute type confusion - memory corruption");
    println!("5. Data races - with parallelization this gets even worse");
    println!("\nThese scenarios demonstrate why unsafe raw pointers bypass");
    println!("Rust's safety guarantees and can lead to undefined behavior!");
}
