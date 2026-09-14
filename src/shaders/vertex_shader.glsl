#version 430 core
// One unit quad, drawn once per bar. corner.x picks the left or right edge,
// corner.y the bottom or the top; everything that makes a bar a bar comes from
// gl_InstanceID and the per-instance height, so the only per-frame upload is
// one float per bar.
layout(location = 0) in vec2 corner;
layout(location = 1) in float height;
uniform float BarWidth;
// One bar plus one gap: the step from a bar's left edge to the next one's.
uniform float Stride;
void main() {
    float x = Stride * float(gl_InstanceID) - 1.0 + corner.x * BarWidth;
    gl_Position = vec4(x, mix(-1.0, height, corner.y), 0.0, 1.0);
}
