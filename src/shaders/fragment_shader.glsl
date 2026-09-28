#version 430 core
// readonly: nothing here writes the palette
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    // count - 1 and count - 2, written by the CPU once per palette
    float stop_span;
    int last_pair;
    vec4 gradient_colors[];
};
// 0 at a bar's base to 1 at the top of the surface
in float vLevel;
// A matte finish: every bar mixed toward one flat tone, so the gradient stops
// reading as a lit ramp. The tone is the palette's own mean, computed on the
// CPU, so matte = 1 is the palette flattened rather than an arbitrary grey
uniform vec3 MatteColor;
uniform float Matte;
// One multiplier over whatever alpha the stops already carry
uniform float Opacity;
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
out vec4 fragColor;
void main() {
#ifdef MIRROR
    float level = abs(vLevel);
#else
    float level = vLevel;
#endif
#ifdef BLOCKS
    // A quarter of every segment is the gap below the next one
    if (fract(level * Blocks) > 0.75) {
        discard;
    }
#endif
#ifdef GRADIENT_ROW
    float findex = vAlong * stop_span;
#else
    float findex = level * stop_span;
#endif
    // Clamped before the fraction is taken, so the top row lands on the last
    // stop rather than a step of 0.0 into the one below it. Branchless, and
    // gradient_buffer guarantees at least two stops so this cannot go negative
    int index = min(int(findex), last_pair);
    vec4 c = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
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
    c.rgb = mix(c.rgb, MatteColor, Matte);
#endif
#ifdef OPACITY
    c.a *= Opacity;
#endif
    fragColor = c;
}
