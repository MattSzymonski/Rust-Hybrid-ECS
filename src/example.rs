use crate::ecs_core::{ScriptComponent, World};

pub fn run_unsafety_test() {
    let world = &mut World::new();
    world.add_component(ScriptComponent { some_value: 10.0 });
    world.add_component(ScriptComponent { some_value: 20.0 });
    world.add_component(ScriptComponent { some_value: 30.0 });

    world.update_scripts();

    println!("\n=== Test Complete ===\n");
}
