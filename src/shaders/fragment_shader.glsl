#version 430 core
// readonly: nothing here writes the palette
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
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
    float findex = vLevel * float(gradient_colors_size - 1);
    // Clamped before the fraction is taken, so the top row lands on the last
    // stop rather than a step of 0.0 into the one below it. Branchless, and
    // gradient_buffer guarantees at least two stops so this cannot go negative
    int index = min(int(findex), gradient_colors_size - 2);
    vec4 c = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
#ifdef REVEAL
    c.rgb = mix(c.rgb, texture(Reveal, gl_FragCoord.xy * RevealMap.xy + RevealMap.zw).rgb, RevealMix);
#endif
#ifdef ROUND
    float r = min(Radius * vSize.x, vSize.y);
    vec2 q = vec2(abs(vLocal.x) - (0.5 * vSize.x - r), vLocal.y - (vSize.y - r));
    if (q.x > 0.0 && q.y > 0.0) {
        // One pixel of antialiasing across the arc
        c.a *= clamp(r - length(q) + 0.5, 0.0, 1.0);
    }
#endif
    c.rgb = mix(c.rgb, MatteColor, Matte);
    c.a *= Opacity;
    fragColor = c;
}
