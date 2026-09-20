#version 430 core
// Same unit quad and same one-float-per-bar payload as the other two modes.
// The path is a static buffer built once at startup, so a curve costs a lookup
// per vertex and nothing per frame
layout(location = 0) in vec2 corner;
layout(location = 1) in float height;
// One vec4 per bar: xy = base in the OUTPUT's NDC, z = the normal's angle,
// w = how far a full-volume bar reaches. Angle rather than a normal vector so
// a bar stays ONE vec4 - two trig calls on four vertices is cheaper than a
// second buffer
layout(std430, binding = 1) readonly buffer PathSamples {
    vec4 path[];
};
// Width is per bar too, because paths differ in it and the shader has no idea
// paths exist. A float array rather than a fifth component: std430 would pad a
// vec4 back out to 16 bytes and waste three times what this costs
layout(std430, binding = 3) readonly buffer BarWidths {
    float widths[];
};
// The surface is the path's bounding box, not the output, so everything here
// is computed in the OUTPUT's NDC and mapped in at the end. One affine map
// covers positions, reach and width alike; identity when the surface is the
// whole output
uniform vec2 PathScale;
uniform vec2 PathOffset;
// 0 at the base, 1 at the tip. The fragment stage is the circle's, which
// indexes the gradient and the alpha ramp by exactly this
out float vRadial;
void main() {
    vec4 s = path[gl_InstanceID];
    vec2 n = vec2(cos(s.z), sin(s.z));
    // Tangent is the normal turned a quarter turn; no second lookup needed
    vec2 t = vec2(-n.y, n.x);
    // draw() hands heights over in NDC because the linear mode uses them as a
    // y coordinate; here they are an amplitude, so undo that rather than
    // making draw() mode-aware
    float amp = (height + 1.0) * 0.5;
    vRadial = corner.y * amp;
    vec2 p = s.xy
           + t * (corner.x - 0.5) * widths[gl_InstanceID]
           + n * vRadial * s.w;
    gl_Position = vec4(p * PathScale + PathOffset, 0.0, 1.0);
}
