#version 430 core
// Same unit quad, same one-float-per-bar payload as the linear mode; only the
// mapping from gl_InstanceID to position differs. corner.x picks a side of the
// bar, corner.y its base or its tip
layout(location = 0) in vec2 corner;
layout(location = 1) in float height;
// 2pi / bar count. Folded on the CPU so this is one multiply, not a divide
uniform float AngleStep;
// Half a bar's angular width, radians. Gap is already taken out of it
uniform float AngularHalf;
// Where a bar starts and how far a full-volume one reaches, both NDC radii
uniform float InnerRadius;
uniform float RadialSpan;
// 0 at the inner edge, 1 at the rim a full-volume bar reaches. The fragment
// stage indexes the gradient by this instead of gl_FragCoord.y
out float vRadial;
void main() {
    float theta = AngleStep * float(gl_InstanceID) + (corner.x * 2.0 - 1.0) * AngularHalf;
    // draw() hands heights over already in NDC, -1 silent to +1 full, because
    // the linear mode uses them as a y coordinate directly. Here they are an
    // amplitude, so undo that rather than making draw() mode-aware
    float amp = (height + 1.0) * 0.5;
    vRadial = corner.y * amp;
    float r = InnerRadius + vRadial * RadialSpan;
    // sin/cos swapped against the usual convention so bar 0 points up: the
    // spectrum then reads clockwise from twelve o'clock
    gl_Position = vec4(r * sin(theta), r * cos(theta), 0.0, 1.0);
}
