#version 430 core
// readonly: nothing here writes the palette.
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    vec4 gradient_colors[];
};
// 0 at the inner edge, 1 at the rim. Stop 1 is therefore the centre and the
// last stop the rim, where the linear mode runs bottom to top.
in float vRadial;
uniform float InnerAlpha;
uniform float OuterAlpha;
out vec4 fragColor;
void main() {
    float t = clamp(vRadial, 0.0, 1.0);
    float findex = t * float(gradient_colors_size - 1);
    // Clamped before the fraction is taken, as in the linear shader. Safe with
    // no lower bound only because gradient_buffer uploads a lone configured
    // stop twice - do not "optimise" that duplication away.
    int index = min(int(findex), gradient_colors_size - 2);
    vec4 c = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
    // Radial alpha ramp, applied on top of whatever alpha the stop carries.
    c.a *= mix(InnerAlpha, OuterAlpha, t);
    fragColor = c;
}
