//! Native rule and dependency-order regression tests.
//!
//! # Responsibilities
//!
//! - Ensures dependency cycles fail before rule execution.
//! - Round-trips generated mesh, material, image, and environment outputs.
//! - Exercises shader compilation or the missing-tool diagnostic.
//!
//! # Design
//!
//! Fixtures use their own temporary directory and small self-contained source
//! assets. The GLB triangle is assembled in memory so the test documents exactly
//! which geometry/material attributes the importer must preserve.

// Current crate
use super::*;

// =============================================================================
// Fixtures
// =============================================================================

/// Unique scratch directory removed when the fixture leaves scope.
struct Temp(PathBuf);
impl Temp {
    /// Create a process/time-qualified directory for one independent fixture.
    fn new() -> Self {
        let p = std::env::temp_dir().join(format!(
            "pill-rules-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        Self(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).unwrap();
    }
}

/// Deliberately self-dependent rule used to exercise cycle rejection.
struct LoopRule;
impl Rule for LoopRule {
    fn name(&self) -> &'static str {
        "cycle"
    }
    fn input_glob(&self) -> &'static str {
        "*.never"
    }
    fn dependencies(&self) -> &[&str] {
        &["cycle"]
    }
    fn output_for(&self, p: &Path) -> PathBuf {
        p.to_path_buf()
    }
    fn build(&self, _: &Path, _: &Path) -> Result<()> {
        Ok(())
    }
}

// =============================================================================
// Tests
// =============================================================================

/// Dependency traversal detects a cycle even when no source file matches the rule.
#[test]
fn dependency_cycles_fail_before_any_output() {
    let t = Temp::new();
    let error = Pipeline {
        root: t.0.clone(),
        rules: vec![Box::new(LoopRule)],
    }
    .run()
    .unwrap_err();
    assert!(error.to_string().contains("cycle"));
}

/// Cook representative built-in formats and decode their outputs with runtime readers.
#[test]
fn png_glb_shader_and_environment_rules_round_trip() {
    let t = Temp::new();
    let png = t.0.join("checker.png");
    std::fs::write(
        &png,
        include_bytes!("../../../../../../examples/project_rs/assets/render_source/checker.png"),
    )
    .unwrap();
    let out = PngToCookedTex.output_for(&png);
    PngToCookedTex.build(&png, &out).unwrap();
    let image = crate::assets::decode_texture(&std::fs::read(out).unwrap()).unwrap();
    assert_eq!((image.width, image.height), (4, 4));
    // A self-contained GLB triangle exercises material factors and generated tangents.
    let mut data = Vec::new();
    for f in [
        -1f32, 0., 0., 1., 0., 0., 0., 1., 0., 0., 0., 1., 0., 0., 1., 0., 0., 1., 0., 0., 1., 0.,
        0.5, 1.,
    ] {
        data.extend(f.to_le_bytes());
    }
    for i in [0u32, 1, 2] {
        data.extend(i.to_le_bytes());
    }
    let doc = serde_json::json!({"asset":{"version":"2.0"},"buffers":[{"byteLength":data.len()}],"bufferViews":[{"buffer":0,"byteOffset":0,"byteLength":36},{"buffer":0,"byteOffset":36,"byteLength":36},{"buffer":0,"byteOffset":72,"byteLength":24},{"buffer":0,"byteOffset":96,"byteLength":12}],"accessors":[{"bufferView":0,"componentType":5126,"count":3,"type":"VEC3","min":[-1,0,0],"max":[1,1,0]},{"bufferView":1,"componentType":5126,"count":3,"type":"VEC3"},{"bufferView":2,"componentType":5126,"count":3,"type":"VEC2"},{"bufferView":3,"componentType":5125,"count":3,"type":"SCALAR"}],"materials":[{"pbrMetallicRoughness":{"metallicFactor":0.7,"roughnessFactor":0.3}}],"meshes":[{"primitives":[{"attributes":{"POSITION":0,"NORMAL":1,"TEXCOORD_0":2},"indices":3,"material":0}]}],"nodes":[{"mesh":0}],"scenes":[{"nodes":[0]}],"scene":0});
    let mut json = serde_json::to_vec(&doc).unwrap();
    while json.len() % 4 != 0 {
        json.push(b' ');
    }
    let mut glb = b"glTF".to_vec();
    glb.extend(2u32.to_le_bytes());
    glb.extend(((12 + 8 + json.len() + 8 + data.len()) as u32).to_le_bytes());
    glb.extend((json.len() as u32).to_le_bytes());
    glb.extend(b"JSON");
    glb.extend(json);
    glb.extend((data.len() as u32).to_le_bytes());
    glb.extend(b"BIN\0");
    glb.extend(data);
    let input = t.0.join("triangle.glb");
    std::fs::write(&input, glb).unwrap();
    let output = GlbToCookedMesh.output_for(&input);
    GlbToCookedMesh.build(&input, &output).unwrap();
    let mesh = crate::assets::decode_mesh(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(mesh.indices.len(), 3);
    let material: crate::assets::Material =
        serde_json::from_slice(&std::fs::read(input.with_extension("material")).unwrap()).unwrap();
    assert_eq!(material.metallic, 0.7);
    let input = t.0.join("test_vertex.hlsl");
    std::fs::write(
        &input,
        b"float4 vs_main(uint id : SV_VertexID) : SV_Position { return float4(0,0,0,1); }",
    )
    .unwrap();
    if std::process::Command::new("slangc")
        .arg("-version")
        .output()
        .is_ok()
    {
        let out = HlslToWgsl.output_for(&input);
        HlslToWgsl.build(&input, &out).unwrap();
        assert!(std::fs::read_to_string(out).unwrap().contains("@vertex"));
    } else {
        assert!(HlslToWgsl
            .build(&input, &HlslToWgsl.output_for(&input))
            .unwrap_err()
            .to_string()
            .contains("slangc"));
    }
    let input = t.0.join("studio.procedural_equirect");
    std::fs::write(&input, b"").unwrap();
    let env = ProceduralEquirect.output_for(&input);
    ProceduralEquirect.build(&input, &env).unwrap();
    EquirectToIBL
        .build(&env, &EquirectToIBL.output_for(&env))
        .unwrap();
    let diffuse = crate::assets::decode_texture(
        &std::fs::read(t.0.join("studio_diffuse_ibl.cooked_tex")).unwrap(),
    )
    .unwrap();
    assert!(diffuse.float);
    let spec = crate::assets::decode_texture(
        &std::fs::read(t.0.join("studio_specular_ibl.cooked_tex")).unwrap(),
    )
    .unwrap();
    assert_eq!(spec.mips.len(), 5);
}
