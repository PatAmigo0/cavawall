#version 430 core
// The circle's fragment stage plus occlusion: the gradient and alpha ramp run
// along the bar by vRadial exactly as there
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    // count - 1 and count - 2, written by the CPU once per palette
    float stop_span;
    int last_pair;
    vec4 gradient_colors[];
};
// One bit per occluder, rasterised once per configure. Surface-sized, so a
// fragment reads its own texel
uniform usampler2D Occluders;
// The occluders that cut this bar's path
flat in uint vMask;
in float vRadial;
uniform float InnerAlpha;
uniform float OuterAlpha;
uniform vec3 MatteColor;
uniform float Matte;
uniform float Opacity;
#ifdef ROUND
// A fraction of the bar's width; only the tip is rounded, the base stands
// on its line
uniform float Radius;
in vec2 vLocal;
flat in vec2 vSize;
#endif
#ifdef REVEAL
// The x-ray image, and the map from this fragment to its texel, composed on
// the CPU from where the surface sits and how the image crops onto the output
uniform sampler2D Reveal;
uniform vec4 RevealMap;
uniform float RevealMix;
#endif
#ifdef GRADIENT_ROW
in float vAlong;
#endif
#ifdef BLOCKS
// Segments a full bar is split into
uniform float Blocks;
#endif
#ifdef REVEAL_PULSE
flat in float vPeak;
#endif
#ifdef PATH_PALETTES
// This bar's palette: its first stop, its count less one and less two
flat in int vFirst;
flat in float vSpan;
flat in int vLast;
#ifdef MATTE
flat in vec3 vMatte;
#endif
#endif
out vec4 fragColor;
void main() {
    // First, before any gradient work is spent on a fragment that is hidden
#ifdef OCCLUDE
    if ((texelFetch(Occluders, ivec2(gl_FragCoord.xy), 0).r & vMask) != 0u) {
        discard;
    }
#endif
#ifdef MIRROR
    float t = clamp(abs(vRadial), 0.0, 1.0);
#else
    float t = clamp(vRadial, 0.0, 1.0);
#endif
#ifdef BLOCKS
    // A quarter of every segment is the gap below the next one
    if (fract(t * Blocks) > 0.75) {
        discard;
    }
#endif
#ifdef PATH_PALETTES
    int first = vFirst;
    float span = vSpan;
    int last = vLast;
#else
    const int first = 0;
    float span = stop_span;
    int last = last_pair;
#endif
#ifdef GRADIENT_ROW
    float findex = vAlong * span;
#else
    float findex = t * span;
#endif
    // Safe with no lower bound only because gradient_buffer uploads a lone
    // configured stop twice
    int index = min(int(findex), last);
    vec4 c = mix(gradient_colors[first + index], gradient_colors[first + index + 1], findex - float(index));
#ifdef RAMP
    c.a *= mix(InnerAlpha, OuterAlpha, t);
#endif
#ifdef REVEAL
#ifdef REVEAL_PULSE
    float reveal = RevealMix * smoothstep(0.1, 0.8, vPeak);
#else
    float reveal = RevealMix;
#endif
    c.rgb = mix(c.rgb, texture(Reveal, fma(gl_FragCoord.xy, RevealMap.xy, RevealMap.zw)).rgb, reveal);
#endif
#ifdef ROUND
    float r = min(Radius * vSize.x, vSize.y);
#ifdef MIRROR
    vec2 q = vec2(abs(vLocal.x) - fma(0.5, vSize.x, -r), abs(vLocal.y) - (vSize.y - r));
#else
    vec2 q = vec2(abs(vLocal.x) - fma(0.5, vSize.x, -r), vLocal.y - (vSize.y - r));
#endif
    if (q.x > 0.0 && q.y > 0.0) {
        // One pixel of antialiasing across the arc
        c.a *= clamp(r - length(q) + 0.5, 0.0, 1.0);
    }
#endif
#ifdef MATTE
#ifdef PATH_PALETTES
    c.rgb = mix(c.rgb, vMatte, Matte);
#else
    c.rgb = mix(c.rgb, MatteColor, Matte);
#endif
#endif
#ifdef OPACITY
    c.a *= Opacity;
#endif
    fragColor = c;
}
