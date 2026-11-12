// The one fragment shader. Modes (src/render/gpu.rs): 0 solid fill, 1 glyph
// coverage from the R8 atlas, 2 emoji from the BGRA atlas, 3 an image texture,
// 4 rounded-rect coverage. Glyph and emoji reads are texelFetch at integer
// coords so a 1:1 atlas quad reproduces its source texels exactly. An image is
// an sRGB texture sampled with normalised UVs, so the GPU decodes and scales its
// native-size texture to the display quad in linear space, keeping resize cheap
// and smooth. A picture shown much smaller than it is gets several taps a
// pixel (see image_color).
#version 450

layout(location = 0) in vec2 v_uv;
layout(location = 1) in vec4 v_color;
layout(location = 2) in vec4 v_extra;
layout(location = 3) flat in uint v_mode;

layout(set = 0, binding = 0) uniform sampler2D glyph_atlas;
layout(set = 0, binding = 1) uniform sampler2D emoji_atlas;
layout(set = 1, binding = 0) uniform sampler2D image_tex;

layout(location = 0) out vec4 out_color;

// Shared with the vertex stage (which reads viewport). The fragment stage reads
// only the glyph mask gamma; both members sit at the offsets the CPU packs.
layout(push_constant) uniform Push {
    vec2 viewport;
    float glyph_coverage_gamma;
} push;

// sRGB to linear. Vertex colours and the emoji atlas carry sRGB-encoded bytes
// and are decoded here; the image texture is an sRGB format the sampler decodes
// on its own. The colour attachment is an sRGB view, so blending runs in linear
// space and the hardware re-encodes on store, leaving opaque fills byte-exact
// while anti-aliased edges blend gamma-correct.
vec3 srgb_to_linear(vec3 c) {
    bvec3 hi = greaterThan(c, vec3(0.04045));
    vec3 lo = c / 12.92;
    vec3 hs = pow((c + 0.055) / 1.055, vec3(2.4));
    return mix(lo, hs, hi);
}

// Coverage of a rounded rectangle of `size` at rect-local pixel `p`: full
// everywhere except inside a rounded corner's cell, where the pixel centre's
// distance to the corner circle fades coverage across the one-pixel band at the
// radius. The top corners are always rounded, the bottom ones when `bottom`.
// v_uv already sits at the fragment (pixel) centre.
float round_coverage(vec2 p, vec2 size, float r, bool bottom) {
    vec2 c;
    if (p.x < r && p.y < r) {
        c = vec2(r, r);
    } else if (p.x >= size.x - r && p.y < r) {
        c = vec2(size.x - r, r);
    } else if (bottom && p.x < r && p.y >= size.y - r) {
        c = vec2(r, size.y - r);
    } else if (bottom && p.x >= size.x - r && p.y >= size.y - r) {
        c = vec2(size.x - r, size.y - r);
    } else {
        return 1.0;
    }
    return clamp(r - length(p - c) + 0.5, 0.0, 1.0);
}

// The most taps along one axis of a pixel. Eight covers a picture shown at a
// sixteenth of its size, which is a 16K-wide panorama on a small window.
const int MAX_TAPS = 8;

// The image at `uv`, averaged over the area of the picture this pixel covers.
// `dx` and `dy` are how far `uv` moves across one pixel; quads are upright, so
// only dx.x and dy.y are non-zero.
//
// One bilinear tap blends a 2x2 block of texels. That is all a pixel needs
// down to half size; below that a single tap skips texels, and fine detail
// turns to grain that crawls as the window is resized. So the pixel's
// footprint is covered by a grid of taps two texels apart, each placed on the
// corner where four texels meet, where bilinear weighs the four equally:
//
//      one pixel, picture at a quarter size (4x4 texels)
//      +---+---+---+---+
//      |   |   |   |   |
//      +---x---+---x---+    x = a tap on a texel corner,
//      |   |   |   |   |        averaging the four texels around it
//      +---+---+---+---+
//      |   |   |   |   |
//      +---x---+---x---+
//      |   |   |   |   |
//      +---+---+---+---+
//
// Every texel under the pixel then counts exactly once, which is a box filter:
// what a mip chain would give, without the texture having one. Snapping to
// corners moves the sample by at most half a texel, a quarter of a pixel at
// worst.
//
// Past MAX_TAPS the taps spread out to span the footprint instead, and skip
// texels between them. At half size or larger this is one plain tap.
vec4 image_color(vec2 uv, vec2 dx, vec2 dy) {
    vec2 size = vec2(textureSize(image_tex, 0));
    vec2 texels = abs(vec2(dx.x, dy.y)) * size;
    ivec2 taps = clamp(ivec2(ceil(texels * 0.5)), ivec2(1), ivec2(MAX_TAPS));
    if (taps == ivec2(1)) {
        return textureLod(image_tex, uv, 0.0);
    }
    vec2 spacing = max(vec2(2.0), texels / vec2(taps));
    vec2 start = uv * size - spacing * vec2(taps) * 0.5;
    start = mix(start, round(start), lessThanEqual(spacing, vec2(2.0)));
    vec4 sum = vec4(0.0);
    for (int j = 0; j < taps.y; j++) {
        for (int i = 0; i < taps.x; i++) {
            vec2 at = start + (vec2(i, j) + 0.5) * spacing;
            sum += textureLod(image_tex, at / size, 0.0);
        }
    }
    return sum / float(taps.x * taps.y);
}

void main() {
    // Taken before any branch: a derivative is only defined where every
    // fragment of the primitive runs the same code.
    vec2 uv_dx = dFdx(v_uv);
    vec2 uv_dy = dFdy(v_uv);
    if (v_mode == 0u) {
        out_color = vec4(srgb_to_linear(v_color.rgb), v_color.a);
    } else if (v_mode == 1u) {
        // Mask gamma thins the anti-aliased edges: linear-light compositing is
        // physically correct but renders light-on-dark text heavier than the
        // gamma-space stacks beside it, so raising coverage to this power lowers
        // partial-coverage alpha back to a matching weight. Solid pixels
        // (coverage 1) are unchanged, so colour fills keep full strength.
        float cov = pow(texelFetch(glyph_atlas, ivec2(v_uv), 0).r, push.glyph_coverage_gamma);
        out_color = vec4(srgb_to_linear(v_color.rgb), v_color.a * cov);
    } else if (v_mode == 2u) {
        vec4 e = texelFetch(emoji_atlas, ivec2(v_uv), 0);
        out_color = vec4(srgb_to_linear(e.rgb), e.a);
    } else if (v_mode == 3u) {
        // Image: the sRGB texture is decoded and scaled to the quad in linear
        // space by the sampler, so its result is already linear.
        out_color = image_color(v_uv, uv_dx, uv_dy);
    } else {
        // v_extra is the rect's width, height and corner radius, then the corner
        // mask: 1 rounds the top corners, 3 all four.
        float cov = round_coverage(v_uv, v_extra.xy, v_extra.z, v_extra.w > 2.0);
        out_color = vec4(srgb_to_linear(v_color.rgb), v_color.a * cov);
    }
}
