/// Demo of runtime borrow checking that prevents deadlocks
use ecs_hybrid::*;

fn main() {
    println!("=== Runtime Borrow Checking Demo ===\n");

    let scene = Scene::new();
    let entity = scene.instantiate();
    entity.add_component(Transform::new(1.0, 2.0, 3.0));
    entity.add_component(Velocity::new(0.1, 0.2, 0.3));

    println!("✓ Created entity with Transform and Velocity\n");

    // SAFE: Read borrow, use it, then release
    println!("Test 1: Single read borrow");
    {
        let transform = entity.get_component_raw::<Transform>().unwrap();
        println!(
            "  Transform: ({}, {}, {})",
            transform.x, transform.y, transform.z
        );
    } // Lock released here
    println!("  ✓ Read borrow released\n");

    // SAFE: Multiple sequential read borrows
    println!("Test 2: Sequential read borrows");
    {
        let transform = entity.get_component_raw::<Transform>().unwrap();
        println!("  First read: x = {}", transform.x);
        drop(transform);

        let transform2 = entity.get_component_raw::<Transform>().unwrap();
        println!("  Second read: y = {}", transform2.y);
    }
    println!("  ✓ Both read borrows worked\n");

    // UNSAFE: This would fail at runtime!
    println!("Test 3: Read borrow, then try to get write borrow (should fail)");
    {
        let transform_read = entity.get_component_raw::<Transform>().unwrap();
        println!("  Got read borrow: x = {}", transform_read.x);

        // Try to get mutable borrow while read borrow is active
        match entity.get_component_raw_mut::<Transform>() {
            Some(_) => println!("  ✗ ERROR: Should have failed but got write borrow!"),
            None => println!("  ✓ Correctly prevented write borrow (read borrow active)"),
        }

        drop(transform_read);
    }
    println!();

    // SAFE: After releasing read borrow, write borrow works
    println!("Test 4: Release read, then get write borrow");
    {
        {
            let transform_read = entity.get_component_raw::<Transform>().unwrap();
            println!("  Read: x = {}", transform_read.x);
        } // Read borrow released

        let mut transform_write = entity.get_component_raw_mut::<Transform>().unwrap();
        transform_write.x = 99.0;
        println!("  Write: x = {}", transform_write.x);
    }
    println!("  ✓ Write borrow worked after releasing read\n");

    // UNSAFE: Multiple write borrows
    println!("Test 5: Try to get two write borrows (should fail)");
    {
        let mut transform1 = entity.get_component_raw_mut::<Transform>().unwrap();
        transform1.x = 10.0;
        println!("  Got first write borrow");

        // Try to get second write borrow
        match entity.get_component_raw_mut::<Transform>() {
            Some(_) => println!("  ✗ ERROR: Should have failed but got second write borrow!"),
            None => println!("  ✓ Correctly prevented second write borrow"),
        }
    }
    println!();

    // Show that closure API doesn't have these restrictions
    println!("Test 6: Closure API (always safe)");
    entity.with_component::<Transform, _>(|t| {
        println!("  Read in closure: x = {}", t.x);
    });
    entity.with_component_mut::<Transform, _>(|t| {
        t.x = 5.0;
        println!("  Write in closure: x = {}", t.x);
    });
    println!("  ✓ Closures automatically manage locks\n");

    println!("=== Summary ===");
    println!("✓ Runtime borrow checking prevents deadlocks");
    println!("✓ Returns None instead of deadlocking");
    println!("✓ Works like RefCell but for entity components");
    println!("✓ Zero overhead when using closure API");
}
