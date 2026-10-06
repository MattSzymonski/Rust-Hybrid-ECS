struct _MatrixStorage_float4x4_ColMajorstd140_0
{
    @align(16) data_0 : array<vec4<f32>, i32(4)>,
};

struct CameraParams_std140_0
{
    @align(16) camera_position_0 : vec3<f32>,
    @align(16) camera_view_projection_0 : _MatrixStorage_float4x4_ColMajorstd140_0,
    @align(16) camera_inverse_view_projection_0 : _MatrixStorage_float4x4_ColMajorstd140_0,
};

@binding(0) @group(1) var<uniform> camera_0 : CameraParams_std140_0;
fn inverse_mat3_0( m_0 : mat3x3<f32>) -> mat3x3<f32>
{
    var _S1 : f32 = m_0[i32(1)][i32(1)] * m_0[i32(2)][i32(2)] - m_0[i32(1)][i32(2)] * m_0[i32(2)][i32(1)];
    var _S2 : f32 = m_0[i32(1)][i32(0)] * m_0[i32(2)][i32(2)] - m_0[i32(1)][i32(2)] * m_0[i32(2)][i32(0)];
    var _S3 : f32 = m_0[i32(1)][i32(0)] * m_0[i32(2)][i32(1)] - m_0[i32(1)][i32(1)] * m_0[i32(2)][i32(0)];
    var det_0 : f32 = m_0[i32(0)][i32(0)] * _S1 - m_0[i32(0)][i32(1)] * _S2 + m_0[i32(0)][i32(2)] * _S3;
    var _S4 : bool = (abs(det_0)) < 9.99999997475242708e-07f;
    if(_S4)
    {
        return mat3x3<f32>(1.0f, 0.0f, 0.0f, 0.0f, 1.0f, 0.0f, 0.0f, 0.0f, 1.0f);
    }
    var invDet_0 : f32 = 1.0f / det_0;
    var inv_0 : mat3x3<f32>;
    var _S5 : f32 = _S1 * invDet_0;
    inv_0[i32(0)][i32(0)] = _S5;
    var _S6 : f32 = - (m_0[i32(0)][i32(1)] * m_0[i32(2)][i32(2)] - m_0[i32(0)][i32(2)] * m_0[i32(2)][i32(1)]) * invDet_0;
    inv_0[i32(0)][i32(1)] = _S6;
    var _S7 : f32 = (m_0[i32(0)][i32(1)] * m_0[i32(1)][i32(2)] - m_0[i32(0)][i32(2)] * m_0[i32(1)][i32(1)]) * invDet_0;
    inv_0[i32(0)][i32(2)] = _S7;
    var _S8 : f32 = - _S2 * invDet_0;
    inv_0[i32(1)][i32(0)] = _S8;
    var _S9 : f32 = (m_0[i32(0)][i32(0)] * m_0[i32(2)][i32(2)] - m_0[i32(0)][i32(2)] * m_0[i32(2)][i32(0)]) * invDet_0;
    inv_0[i32(1)][i32(1)] = _S9;
    var _S10 : f32 = - (m_0[i32(0)][i32(0)] * m_0[i32(1)][i32(2)] - m_0[i32(0)][i32(2)] * m_0[i32(1)][i32(0)]) * invDet_0;
    inv_0[i32(1)][i32(2)] = _S10;
    var _S11 : f32 = _S3 * invDet_0;
    inv_0[i32(2)][i32(0)] = _S11;
    var _S12 : f32 = - (m_0[i32(0)][i32(0)] * m_0[i32(2)][i32(1)] - m_0[i32(0)][i32(1)] * m_0[i32(2)][i32(0)]) * invDet_0;
    inv_0[i32(2)][i32(1)] = _S12;
    var _S13 : f32 = (m_0[i32(0)][i32(0)] * m_0[i32(1)][i32(1)] - m_0[i32(0)][i32(1)] * m_0[i32(1)][i32(0)]) * invDet_0;
    inv_0[i32(2)][i32(2)] = _S13;
    return inv_0;
}

struct VertexOutput_0
{
    @location(0) vertex_position_0 : vec3<f32>,
    @location(1) vertex_texture_coords_0 : vec2<f32>,
    @location(2) TBN_tangent_0 : vec3<f32>,
    @location(3) TBN_bitangent_0 : vec3<f32>,
    @location(4) TBN_normal_0 : vec3<f32>,
    @location(5) world_position_0 : vec3<f32>,
    @builtin(position) sv_position_0 : vec4<f32>,
};

struct vertexInput_0
{
    @location(0) vertex_position_1 : vec3<f32>,
    @location(4) vertex_texture_coords_1 : vec2<f32>,
    @location(5) vertex_normal_0 : vec3<f32>,
    @location(6) vertex_tangent_0 : vec3<f32>,
    @location(7) vertex_bitangent_0 : vec3<f32>,
    @location(1) model_row_0_0 : vec4<f32>,
    @location(2) model_row_1_0 : vec4<f32>,
    @location(3) model_row_2_0 : vec4<f32>,
};

struct VertexInput_0
{
     vertex_position_2 : vec3<f32>,
     vertex_texture_coords_2 : vec2<f32>,
     vertex_normal_1 : vec3<f32>,
     vertex_tangent_1 : vec3<f32>,
     vertex_bitangent_1 : vec3<f32>,
     model_row_0_1 : vec4<f32>,
     model_row_1_1 : vec4<f32>,
     model_row_2_1 : vec4<f32>,
};

@vertex
fn vs_main( _S14 : vertexInput_0) -> VertexOutput_0
{
    var _S15 : VertexInput_0 = VertexInput_0( _S14.vertex_position_1, _S14.vertex_texture_coords_1, _S14.vertex_normal_0, _S14.vertex_tangent_0, _S14.vertex_bitangent_0, _S14.model_row_0_0, _S14.model_row_1_0, _S14.model_row_2_0 );
    var model_matrix_0 : mat4x4<f32> = mat4x4<f32>(_S14.model_row_0_0, _S14.model_row_1_0, _S14.model_row_2_0, vec4<f32>(0.0f, 0.0f, 0.0f, 1.0f));
    var _S16 : mat3x3<f32> = mat3x3<f32>(model_matrix_0[i32(0)].xyz, model_matrix_0[i32(1)].xyz, model_matrix_0[i32(2)].xyz);
    var _S17 : mat3x3<f32> = inverse_mat3_0(_S16);
    var normal_matrix_0 : mat3x3<f32> = transpose(_S17);
    var tangent_0 : vec3<f32> = normalize((((_S14.vertex_tangent_0) * (normal_matrix_0))));
    var bitangent_0 : vec3<f32> = normalize((((_S14.vertex_bitangent_0) * (normal_matrix_0))));
    var normal_0 : vec3<f32> = normalize((((_S14.vertex_normal_0) * (normal_matrix_0))));
    var TBN_matrix_0 : mat3x3<f32> = transpose(mat3x3<f32>(tangent_0, bitangent_0, normal_0));
    var model_space_0 : vec4<f32> = (((vec4<f32>(_S14.vertex_position_1, 1.0f)) * (model_matrix_0)));
    var output_0 : VertexOutput_0;
    output_0.TBN_tangent_0 = TBN_matrix_0[i32(0)];
    output_0.TBN_bitangent_0 = TBN_matrix_0[i32(1)];
    output_0.TBN_normal_0 = TBN_matrix_0[i32(2)];
    var _S18 : vec3<f32> = model_space_0.xyz;
    var _S19 : vec3<f32> = (((_S18) * (TBN_matrix_0)));
    output_0.vertex_position_0 = _S19;
    output_0.world_position_0 = _S18;
    output_0.vertex_texture_coords_0 = _S14.vertex_texture_coords_1;
    var _S20 : vec4<f32> = (((model_space_0) * (mat4x4<f32>(camera_0.camera_view_projection_0.data_0[i32(0)][i32(0)], camera_0.camera_view_projection_0.data_0[i32(1)][i32(0)], camera_0.camera_view_projection_0.data_0[i32(2)][i32(0)], camera_0.camera_view_projection_0.data_0[i32(3)][i32(0)], camera_0.camera_view_projection_0.data_0[i32(0)][i32(1)], camera_0.camera_view_projection_0.data_0[i32(1)][i32(1)], camera_0.camera_view_projection_0.data_0[i32(2)][i32(1)], camera_0.camera_view_projection_0.data_0[i32(3)][i32(1)], camera_0.camera_view_projection_0.data_0[i32(0)][i32(2)], camera_0.camera_view_projection_0.data_0[i32(1)][i32(2)], camera_0.camera_view_projection_0.data_0[i32(2)][i32(2)], camera_0.camera_view_projection_0.data_0[i32(3)][i32(2)], camera_0.camera_view_projection_0.data_0[i32(0)][i32(3)], camera_0.camera_view_projection_0.data_0[i32(1)][i32(3)], camera_0.camera_view_projection_0.data_0[i32(2)][i32(3)], camera_0.camera_view_projection_0.data_0[i32(3)][i32(3)]))));
    output_0.sv_position_0 = _S20;
    return output_0;
}

