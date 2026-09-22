//! Renderer contract and asset-publication regression tests.
//!
//! # Responsibilities
//!
//! - Checks reflected component layouts, camera ordering, and projection depth.
//! - Exercises malformed payloads and all-or-nothing asset replacement.
//! - Checks cooking cache invalidation and preservation of the last good manifest.
//!
//! # Design
//!
//! Most tests operate on CPU packets and the existing ECS without a GPU.
//! Cooking-specific coverage is feature-gated and uses isolated temporary paths;
//! the native rendering test lives with the PBR pass.

// Current crate
use crate::{assets::*, *};

// =============================================================================
// Scene Contracts
// =============================================================================

/// Shared layouts, equal-priority camera selection, and zero-to-one depth stay stable.
#[test]
fn reflected_layouts_and_camera_selection() {
    assert_eq!(std::mem::size_of::<PbrRenderableComponent>(), 48);
    assert_eq!(std::mem::size_of::<TransformComponent>(), 40);
    assert_eq!(std::mem::size_of::<CameraComponent>(), 20);
    let mut e = pill_engine::Engine::new();
    register(&mut e);
    e.world_mut()
        .create_entity()
        .with(TransformComponent {
            translation: [0., 0., 3.],
            ..Default::default()
        })
        .with(CameraComponent::default())
        .build()
        .unwrap();
    e.world_mut()
        .create_entity()
        .with(TransformComponent {
            translation: [0., 0., 9.],
            ..Default::default()
        })
        .with(CameraComponent::default())
        .build()
        .unwrap();
    e.world_mut()
        .create_entity()
        .with(TransformComponent::default())
        .with(PbrRenderableComponent {
            visible: false,
            ..Default::default()
        })
        .build()
        .unwrap();
    e.process_frame().unwrap();
    let frame = e.world().get_resource::<RenderFrame>().unwrap();
    assert_eq!(frame.camera_position, [0., 0., 3.]);
    assert!(frame.instances.is_empty());
    let projection =
        glam::camera::rh::proj::directx::perspective(60f32.to_radians(), 1., 0.1, 100.);
    for (z, expected) in [(-0.1, 0.), (-100., 1.)] {
        let p = projection * glam::Vec4::new(0., 0., z, 1.);
        assert!((p.z / p.w - expected).abs() < 0.00001);
    }
}

// =============================================================================
// Runtime Asset Validation
// =============================================================================

/// Readers reject truncated headers, invalid indices, and unsupported texture versions.
#[test]
fn malformed_assets_are_rejected() {
    assert!(decode_mesh(b"RMSH").is_err());
    let mut mesh = b"RMSH".to_vec();
    for n in [1u32, 0, 3, 0, 0, 0] {
        mesh.extend(n.to_le_bytes());
    }
    assert!(decode_mesh(&mesh).is_err());
    for version in [0u32, 3, 99] {
        let mut bytes = b"RTEX".to_vec();
        for n in [version, 1, 1] {
            bytes.extend(n.to_le_bytes());
        }
        assert!(decode_texture(&bytes).is_err());
    }
    assert_eq!(Material::default().metallic, 0.);
}

/// Failed replacement is atomic, while a successful empty replacement removes old assets.
#[test]
fn failed_asset_batch_preserves_old_generation_and_empty_reload_removes_it() {
    let mut e = pill_engine::Engine::new();
    register(&mut e);
    e.world_mut()
        .get_resource_mut::<RenderAssetRequests>()
        .unwrap()
        .insert("a.material", b"{}".to_vec());
    e.process_frame().unwrap();
    let id = asset_id("a.material");
    assert!(e
        .world()
        .get_resource::<RenderFrame>()
        .unwrap()
        .assets
        .materials
        .get(&id)
        .is_some());
    {
        let q = e
            .world_mut()
            .get_resource_mut::<RenderAssetRequests>()
            .unwrap();
        q.replace_all = true;
        q.insert("bad.cooked_mesh", vec![0]);
    }
    e.process_frame().unwrap();
    assert!(e
        .world()
        .get_resource::<RenderFrame>()
        .unwrap()
        .assets
        .materials
        .get(&id)
        .is_some());
    e.world_mut()
        .get_resource_mut::<RenderAssetRequests>()
        .unwrap()
        .replace_all = true;
    e.process_frame().unwrap();
    assert!(e
        .world()
        .get_resource::<RenderFrame>()
        .unwrap()
        .assets
        .materials
        .get(&id)
        .is_none());
}

// =============================================================================
// Native Cooking
// =============================================================================

/// Whole-source hashing notices sidecars and failed conversions preserve the published manifest.
#[cfg(feature = "asset-cooking")]
#[test]
fn cook_cache_dependency_changes_and_atomic_failure() {
    let temp = std::env::temp_dir().join(format!(
        "pill-cook-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let input = temp.join("input");
    let output = temp.join("output");
    std::fs::create_dir_all(&input).unwrap();
    std::fs::write(
        input.join("sample.obj"),
        include_bytes!("../../../../../examples/project_rs/assets/render_source/sample.obj"),
    )
    .unwrap();
    let first = crate::pill_assets::cook(&input, &output).unwrap();
    assert!(!first.rebuilt.is_empty());
    assert!(crate::pill_assets::cook(&input, &output)
        .unwrap()
        .rebuilt
        .is_empty());
    let mut q = RenderAssetRequests::default();
    load_manifest(&DirectorySource(output.clone()), &mut q).unwrap();
    let mut assets = RenderAssets::default();
    for (name, bytes) in q.pending {
        assets.load(&name, &bytes).unwrap();
    }
    assert!(assets.meshes.get(&asset_id("sample.cooked_mesh")).is_some());
    std::fs::write(input.join("sidecar.txt"), b"changed dependency").unwrap();
    assert!(!crate::pill_assets::cook(&input, &output)
        .unwrap()
        .rebuilt
        .is_empty());
    let good = std::fs::read(output.join("manifest.json")).unwrap();
    std::fs::write(input.join("broken.png"), b"bad PNG").unwrap();
    assert!(crate::pill_assets::cook(&input, &output).is_err());
    assert_eq!(std::fs::read(output.join("manifest.json")).unwrap(), good);
    std::fs::remove_dir_all(temp).unwrap();
}
