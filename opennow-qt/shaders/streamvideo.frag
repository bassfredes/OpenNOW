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
    // x: downscale HQ (1 = footprint-scaled Lanczos2, 0 = uniform box),
    // y: sharpen strength (0 = off; CAS-style blend, default low),
    // zw: reserved.
    vec4 downscaleFlags;
};
layout(binding = 1) uniform sampler2D videoTexture;

const float PI = 3.14159265358979;

float sinc(float x)
{
    return abs(x) < 1e-5 ? 1.0 : sin(PI * x) / (PI * x);
}

// Lanczos window 2: sinc(t) * sinc(t / 2), zero for |t| >= 2. The negative
// lobes are what restore edge contrast a plain box average removes.
float lanczos2(float t)
{
    float a = abs(t);
    return a >= 2.0 ? 0.0 : sinc(a) * sinc(0.5 * a);
}

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
            // Minification. The tap grid spans min(footprint, 4) source
            // texels (four samples per axis, cost fixed); HQ selects
            // footprint-scaled Lanczos2 weights over the previous uniform
            // box. Averaging stays in the encoded signal exactly as the
            // original bilinear tap did, so the videoColor/PQ contract is
            // unchanged.
            vec2 span = min(footprint, vec2(4.0));
            vec3 weighted = vec3(0.0);
            vec3 plain = vec3(0.0);
            vec3 mn = vec3(1.0);
            vec3 mx = vec3(0.0);
            float weightSum = 0.0;
            bool hq = downscaleFlags.x >= 0.5;
            for (int y = 0; y < 4; ++y) {
                for (int x = 0; x < 4; ++x) {
                    vec2 offset = ((vec2(x, y) + 0.5) / 4.0 - 0.5) * span;
                    vec3 tap = texture(videoTexture, uv + offset * texel).rgb;
                    plain += tap;
                    mn = min(mn, tap);
                    mx = max(mx, tap);
                    float w = hq
                        ? lanczos2(offset.x / max(0.5 * footprint.x, 1e-4))
                              * lanczos2(offset.y / max(0.5 * footprint.y, 1e-4))
                        : 1.0;
                    weighted += w * tap;
                    weightSum += w;
                }
            }
            vec3 box = plain * (1.0 / 16.0);
            sampled = weightSum > 1e-5 ? weighted / weightSum : box;
            // CAS-style adaptive sharpen (FidelityFX amplitude term + unsharp
            // blend over this footprint): boost the Lanczos-vs-box detail where
            // the neighborhood has headroom, back off near clipping to avoid
            // halos. Strength comes from OPENNOW_DOWNSCALE_SHARPEN.
            float strength = downscaleFlags.y;
            if (strength > 0.0) {
                vec3 amp = sqrt(clamp(min(mn, 2.0 - mx) / max(mx, vec3(1e-4)),
                                      0.0, 1.0));
                sampled = clamp(sampled + strength * amp * (sampled - box),
                                0.0, 1.0);
            }
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
