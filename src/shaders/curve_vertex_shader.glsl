#version 430 core
// Same unit quad and same one-float-per-bar payload as the other two modes.
// The path is a static buffer rebuilt on configure, so a curve costs two
// lookups per vertex and nothing per frame
layout(location = 0) in vec2 corner;
// Normalised by the vertex fetch: cava's u16 arrives as 0..1, which is the
// amplitude this stage wants
layout(location = 1) in float height;
// One vec4 per bar: xy = base in the OUTPUT's NDC, zw = the unit normal the
// bar grows along. The vector itself, so no vertex turns an angle back into one
layout(std430, binding = 1) readonly buffer PathSamples {
    vec4 path[];
};
// Width and reach are per bar, because paths differ in both and the shader has
// no idea paths exist. A vec2 array packs to 8 bytes with no padding
layout(std430, binding = 3) readonly buffer BarGeometry {
    vec2 geom[];
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
    vec2 n = s.zw;
    // Tangent is the normal turned a quarter turn; no second lookup needed
    vec2 t = vec2(-n.y, n.x);
    // x = width, y = reach, both carrying the point's scale already
    vec2 g = geom[gl_InstanceID];
    vRadial = corner.y * height;
    vec2 p = s.xy + t * (corner.x - 0.5) * g.x + n * vRadial * g.y;
    gl_Position = vec4(p * PathScale + PathOffset, 0.0, 1.0);
}
