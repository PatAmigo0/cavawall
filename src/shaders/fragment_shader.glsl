#version 430 core
// readonly: nothing here writes the palette
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    vec4 gradient_colors[];
};
// (stops - 1) / surface height, folded on the CPU: one multiply per fragment
uniform float GradientScale;
// A matte finish: every bar mixed toward one flat tone, so the gradient stops
// reading as a lit ramp. The tone is the palette's own mean, computed on the
// CPU, so matte = 1 is the palette flattened rather than an arbitrary grey
uniform vec3 MatteColor;
uniform float Matte;
// One multiplier over whatever alpha the stops already carry
uniform float Opacity;
out vec4 fragColor;
void main() {
    float findex = gl_FragCoord.y * GradientScale;
    // Clamped before the fraction is taken, so the top row lands on the last
    // stop rather than a step of 0.0 into the one below it. Branchless, and
    // gradient_buffer guarantees at least two stops so this cannot go negative
    int index = min(int(findex), gradient_colors_size - 2);
    vec4 c = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
    c.rgb = mix(c.rgb, MatteColor, Matte);
    c.a *= Opacity;
    fragColor = c;
}
