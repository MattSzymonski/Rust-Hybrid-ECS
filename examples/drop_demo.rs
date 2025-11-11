/// Demo of the new Drop-based deferred execution API
use ecs_hybrid::*;

fn main() {
    println!("=== Drop-Based Deferred Execution Demo ===\n");

    let scene = Scene::new();

    println!("--- Immediate Execution (default) ---\n");

    let entity1 = scene.instantiate();
    println!("1. Creating entity...");

    entity1.add_component(Transform::new(10.0, 20.0, 0.0));
    println!("2. Added Transform - EXECUTED IMMEDIATELY at semicolon!");

    entity1.add_component(Velocity::new(1.0, 0.0, 0.0));
    println!("3. Added Velocity - EXECUTED IMMEDIATELY at semicolon!");

    // Verify components exist immediately
    assert!(entity1.has_component::<Transform>());
    assert!(entity1.has_component::<Velocity>());
    println!("4. ✓ Components are immediately accessible!\n");

    println!("--- Deferred Execution (with .delay()) ---\n");

    let entity2 = scene.instantiate();
    println!("1. Creating entity...");

    entity2
        .add_component(Transform::new(30.0, 40.0, 0.0))
        .delay();
    println!("2. Added Transform with .delay() - QUEUED, not executed yet");

    entity2.add_component(Velocity::new(2.0, 0.0, 0.0)).delay();
    println!("3. Added Velocity with .delay() - QUEUED, not executed yet");

    // Components don't exist yet
    assert!(!entity2.has_component::<Transform>());
    assert!(!entity2.has_component::<Velocity>());
    println!("4. Components are NOT accessible yet (still queued)\n");

    scene.apply_commands();
    println!("5. Called apply_commands() - ALL QUEUED COMMANDS EXECUTED!");

    // Now components exist
    assert!(entity2.has_component::<Transform>());
    assert!(entity2.has_component::<Velocity>());
    println!("6. ✓ Components are now accessible!\n");

    println!("--- Mixed: Immediate + Deferred ---\n");

    let entity3 = scene.instantiate();

    entity3.add_component(Transform::new(50.0, 60.0, 0.0)); // Immediate
    println!("1. Added Transform - immediate");

    entity3.add_component(Velocity::new(3.0, 0.0, 0.0)).delay(); // Deferred
    println!("2. Added Velocity - deferred");

    entity3.add_component(Health::new(100.0)); // Immediate
    println!("3. Added Health - immediate");

    // Transform and Health exist, but not Velocity
    assert!(entity3.has_component::<Transform>());
    assert!(!entity3.has_component::<Velocity>()); // Not yet!
    assert!(entity3.has_component::<Health>());
    println!("4. Transform and Health exist, Velocity is queued");

    scene.apply_commands();
    println!("5. Applied commands - Velocity now exists!\n");

    assert!(entity3.has_component::<Velocity>());

    println!("--- Fluent API (chaining) ---\n");

    let entity4 = scene.instantiate();
    entity4
        .add_component(Transform::new(70.0, 80.0, 0.0))
        .add_component(Velocity::new(4.0, 0.0, 0.0))
        .add_component(Health::new(150.0));

    println!("Created entity with chained .add() calls - all immediate!");
    assert!(entity4.has_component::<Transform>());
    assert!(entity4.has_component::<Velocity>());
    assert!(entity4.has_component::<Health>());
    println!("✓ All components accessible immediately\n");

    println!("--- Destroy Operations ---\n");

    let entity5 = scene.instantiate();
    entity5.add_component(Transform::new(100.0, 100.0, 0.0));

    println!("1. Created entity with Transform");

    entity5.destroy(); // Immediate
    println!("2. Called destroy() - entity destroyed IMMEDIATELY!");

    assert!(!entity5.has_component::<Transform>());
    println!("3. ✓ Entity no longer exists\n");

    let entity6 = scene.instantiate();
    entity6.add_component(Transform::new(200.0, 200.0, 0.0));

    println!("4. Created another entity with Transform");

    entity6.destroy().delay(); // Deferred
    println!("5. Called destroy().delay() - entity still exists (queued)");

    assert!(entity6.has_component::<Transform>());
    println!("6. Entity still accessible");

    scene.apply_commands();
    println!("7. Applied commands - entity destroyed!");

    assert!(!entity6.has_component::<Transform>());
    println!("8. ✓ Entity no longer exists\n");

    println!("=== Key Insights ===");
    println!("1. Default behavior: Commands execute IMMEDIATELY (at semicolon)");
    println!("2. With .delay(): Commands are queued until apply_commands()");
    println!("3. Fluent API (.add): Always immediate, supports chaining");
    println!("4. Drop trait magic: Executes when builder goes out of scope");
    println!("5. Best of both worlds: Unity-like immediacy + ECS deferred control");
    println!("\n✓ Demo completed!");
}
