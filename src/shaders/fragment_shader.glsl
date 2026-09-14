#version 430 core
// readonly: nothing here writes the palette.
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    vec4 gradient_colors[];
};
// (stops - 1) / surface height, folded on the CPU. One multiply per fragment
// where this used to be an int-to-float conversion, a multiply and a divide.
uniform float GradientScale;
out vec4 fragColor;
void main() {
    float findex = gl_FragCoord.y * GradientScale;
    // Clamped before the fraction is taken, so the top row lands on the last
    // stop rather than a step of 0.0 into the one below it. Branchless, and
    // gradient_buffer guarantees at least two stops so this cannot go negative.
    int index = min(int(findex), gradient_colors_size - 2);
    fragColor = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
}
