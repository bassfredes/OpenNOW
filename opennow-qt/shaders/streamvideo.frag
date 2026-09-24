#version 440
#extension GL_GOOGLE_include_directive : enable
#include "hdrcolor.glsl"
layout(location = 0) in vec2 itemPosition;
layout(location = 0) out vec4 fragColor;
layout(std140, binding = 0) uniform Composition {
    mat4 matrix;
    vec4 bounds;
    vec4 videoRect;
    vec4 parameters;
    vec4 colorParameters;
};
layout(binding = 1) uniform sampler2D videoTexture;
void main()
{
    vec2 uv = (itemPosition - videoRect.xy) / max(videoRect.zw, vec2(1.0));
    // parameters.zw is the sampled texture's texel size (1 / sourceSize),
    // uploaded by the renderer just before the draw. The clamp keeps the
    // footprint finite if the value has not been uploaded yet.
    vec2 texel = max(parameters.zw, vec2(1.0 / 16384.0));
    // Source texels covered by one output pixel. Computed before the viewport
    // branch so the derivatives are taken in uniform control flow.
    vec2 footprint = fwidth(uv) / texel;
    vec3 color = vec3(0.0);
    if (all(greaterThanEqual(uv, vec2(0.0))) && all(lessThanEqual(uv, vec2(1.0)))) {
        vec3 sampled;
        if (max(footprint.x, footprint.y) <= 1.0) {
            sampled = texture(videoTexture, uv).rgb;
        } else {
            // Minification: a single bilinear tap averages at most 2x2 source
            // texels, which aliases a 2.67x 5K->1080p downscale into mushy
            // text and shimmer. Spread a fixed 4x4 grid of taps across the
            // footprint (clamped to four texels so the cost stays constant) -
            // a box prefilter over every source texel the output pixel covers.
            // Averaging stays in the encoded signal, exactly as the previous
            // bilinear tap did, so the videoColor/PQ contract is unchanged.
            vec2 span = min(footprint, vec2(4.0));
            vec3 sum = vec3(0.0);
            for (int y = 0; y < 4; ++y) {
                for (int x = 0; x < 4; ++x) {
                    vec2 offset = ((vec2(x, y) + 0.5) / 4.0 - 0.5) * span;
                    sum += texture(videoTexture, uv + offset * texel).rgb;
                }
            }
            sampled = sum * (1.0 / 16.0);
        }
        color = videoColor(sampled, colorParameters.x,
                           colorParameters.y, colorParameters.z, colorParameters.w);
        if (colorParameters.y < 0.5 && parameters.y > 0.0) {
            uvec2 pixel = uvec2(gl_FragCoord.xy) & uvec2(7u);
            uint rank = ((pixel.x ^ pixel.y) & 1u) * 32u + (pixel.y & 1u) * 16u
                      + ((pixel.x ^ pixel.y) & 2u) * 4u + (pixel.y & 2u) * 2u
                      + ((pixel.x ^ pixel.y) & 4u) / 2u + (pixel.y & 4u) / 4u;
            float dither = ((float(rank) + 0.5) / 64.0 - 0.5) * parameters.y;
            color = floor(clamp(color + vec3(dither), 0.0, 1.0) * 255.0 + 0.5) / 255.0;
        }
    }
    fragColor = vec4(color * parameters.x, parameters.x);
}
