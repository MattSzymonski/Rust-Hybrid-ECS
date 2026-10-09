// Overlay shaders drawn through the surface's camera: debug lines and gizmos,
// the selection mask, and the pick buffer.

// The camera uniform, in the layout of `CameraParametersData`.
struct Camera {
    position: vec4<f32>,
    view_projection: mat4x4<f32>,
    inverse_view_projection: mat4x4<f32>,
};

@group(0) @binding(0) var<uniform> camera: Camera;

// Debug lines, gizmo lines and gizmo triangles: a world position and a colour.
struct ColorOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn color_vertex(@location(0) position: vec3<f32>, @location(1) color: vec4<f32>) -> ColorOutput {
    var output: ColorOutput;
    output.clip = camera.view_projection * vec4<f32>(position, 1.0);
    output.color = color;
    return output;
}

@fragment
fn color_fragment(input: ColorOutput) -> @location(0) vec4<f32> {
    return input.color;
}

// What the pick buffer holds at a pixel.
struct ValueOutput {
    @builtin(position) clip: vec4<f32>,
    @location(0) @interpolate(flat) value: u32,
};

// Gizmo handles in the pick buffer: the value is the handle with its top bit set.
@vertex
fn handle_vertex(@location(0) position: vec3<f32>, @location(1) value: u32) -> ValueOutput {
    var output: ValueOutput;
    output.clip = camera.view_projection * vec4<f32>(position, 1.0);
    output.value = value;
    return output;
}

// A mesh vertex moved by its instance's three model rows, as the chain's
// shaders do it.
fn instance_clip(position: vec3<f32>, row_0: vec4<f32>, row_1: vec4<f32>, row_2: vec4<f32>) -> vec4<f32> {
    let model_position = vec4<f32>(position, 1.0);
    let world = vec4<f32>(dot(row_0, model_position), dot(row_1, model_position), dot(row_2, model_position), 1.0);
    return camera.view_projection * world;
}

// Meshes in the pick buffer: the value is one more than the instance's queue
// position, so zero stays "nothing".
@vertex
fn mesh_pick_vertex(
    @location(0) position: vec3<f32>,
    @location(1) row_0: vec4<f32>,
    @location(2) row_1: vec4<f32>,
    @location(3) row_2: vec4<f32>,
    @builtin(instance_index) instance: u32,
) -> ValueOutput {
    var output: ValueOutput;
    output.clip = instance_clip(position, row_0, row_1, row_2);
    output.value = instance + 1u;
    return output;
}

@fragment
fn value_fragment(input: ValueOutput) -> @location(0) u32 {
    return input.value;
}

// The outlined meshes' silhouettes, for the edge pass to trace.
@vertex
fn mesh_mask_vertex(
    @location(0) position: vec3<f32>,
    @location(1) row_0: vec4<f32>,
    @location(2) row_1: vec4<f32>,
    @location(3) row_2: vec4<f32>,
) -> @builtin(position) vec4<f32> {
    return instance_clip(position, row_0, row_1, row_2);
}

@fragment
fn mask_fragment() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
