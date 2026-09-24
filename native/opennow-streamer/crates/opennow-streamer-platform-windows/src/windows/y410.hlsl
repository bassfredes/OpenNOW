Texture2D<uint4> source_y410 : register(t0);
cbuffer Quantization : register(b0) { float4 scale_bias; float4 yuv_coefficients; };
float4 vertex_main(uint id : SV_VertexID) : SV_Position {
    float2 corner = float2((id << 1) & 2, id & 2);
    return float4(corner * float2(2, -2) + float2(-1, 1), 0, 1);
}
float4 pixel_main(float4 position : SV_Position) : SV_Target {
    float3 encoded = float3(source_y410.Load(int3(position.xy, 0)).xyz);
    float y = encoded.y * scale_bias.x + scale_bias.y;
    float u = encoded.x * scale_bias.z + scale_bias.w;
    float v = encoded.z * scale_bias.z + scale_bias.w;
    return float4(saturate(float3(y + yuv_coefficients.x * v,
        y + yuv_coefficients.y * u + yuv_coefficients.z * v, y + yuv_coefficients.w * u)), 1);
}
// Direct CUDA copies retain native 16-bit planes (10 significant high bits).
// P010 is handled as its actual 4:2:0 layout, never reported as 4:4:4.
Texture2D<uint> plane_y : register(t1);
Texture2D<uint2> plane_uv : register(t2);
Texture2D<uint> plane_v : register(t3);
cbuffer PlaneLayout : register(b1) { float4 plane_layout; };
float2 chroma_at(int2 at, int2 maximum) {
    return float2(plane_uv.Load(int3(clamp(at, int2(0, 0), maximum), 0)) >> 6);
}
float4 pixel_planes(float4 position : SV_Position) : SV_Target {
    int2 at = int2(position.xy);
    float ycode = float(plane_y.Load(int3(at, 0)) >> 6);
    float2 chroma;
    if (plane_layout.x > 0.5) {
        uint width, height;
        plane_uv.GetDimensions(width, height);
        float2 coord = (float2(at) - plane_layout.yz) * 0.5;
        int2 lo = int2(floor(coord));
        float2 fraction = frac(coord);
        int2 maximum = int2(width, height) - 1;
        chroma = lerp(lerp(chroma_at(lo, maximum), chroma_at(lo + int2(1,0), maximum), fraction.x),
            lerp(chroma_at(lo + int2(0,1), maximum), chroma_at(lo + int2(1,1), maximum), fraction.x), fraction.y);
    } else {
        chroma = float2(plane_uv.Load(int3(at, 0)).x >> 6, plane_v.Load(int3(at, 0)) >> 6);
    }
    float y = ycode * scale_bias.x + scale_bias.y;
    float u = chroma.x * scale_bias.z + scale_bias.w;
    float v = chroma.y * scale_bias.z + scale_bias.w;
    return float4(saturate(float3(y + yuv_coefficients.x * v,
        y + yuv_coefficients.y * u + yuv_coefficients.z * v, y + yuv_coefficients.w * u)), 1);
}
