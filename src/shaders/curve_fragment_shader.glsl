#version 430 core
// The circle's fragment stage plus an occluder. Kept separate rather than
// branching the shared one: circle mode never binds the occluder buffer, and
// reading an unbound SSBO is undefined rather than merely empty.
layout(std430, binding = 0) readonly buffer GradientColors {
    int gradient_colors_size;
    vec4 gradient_colors[];
};
// A 1-D horizon: height above the bottom, 0..1, sampled evenly across x. One
// lookup per fragment, where a polygon test would be O(vertices).
layout(std430, binding = 2) readonly buffer Occluder {
    int occ_len;
    float horizon[];
};
uniform vec2 Resolution;
// Where this surface sits on the output: xy = origin, zw = size, both
// normalised, y counted from the bottom. The horizon is authored against the
// output, so a fragment has to be put back there before it is sampled.
uniform vec4 OccMap;
in float vRadial;
uniform float InnerAlpha;
uniform float OuterAlpha;
out vec4 fragColor;
void main() {
    if (occ_len > 1) {
        float fx = clamp(OccMap.x + (gl_FragCoord.x / Resolution.x) * OccMap.z, 0.0, 1.0);
        float f = fx * float(occ_len - 1);
        int i = min(int(f), occ_len - 2);
        float h = mix(horizon[i], horizon[i + 1], f - float(i));
        // gl_FragCoord.y counts up from the bottom, which is the direction the
        // horizon is stored in, so neither needs flipping.
        float fy = OccMap.y + (gl_FragCoord.y / Resolution.y) * OccMap.w;
        if (fy < h) {
            discard;
        }
    }
    float t = clamp(vRadial, 0.0, 1.0);
    float findex = t * float(gradient_colors_size - 1);
    int index = min(int(findex), gradient_colors_size - 2);
    vec4 c = mix(gradient_colors[index], gradient_colors[index + 1], findex - float(index));
    c.a *= mix(InnerAlpha, OuterAlpha, t);
    fragColor = c;
}
