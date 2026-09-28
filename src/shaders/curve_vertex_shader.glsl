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
// Per bar, because paths differ and the shader has no idea paths exist:
// x = width, y = reach, z = the occluder mask, exact in a float up to 2^24
layout(std430, binding = 3) readonly buffer BarGeometry {
    vec4 geom[];
};
// The surface is the path's bounding box, not the output, so everything here
// is computed in the OUTPUT's NDC and mapped in at the end. One affine map
// covers positions, reach and width alike; identity when the surface is the
// whole output
uniform vec2 PathScale;
uniform vec2 PathOffset;
// Output width over height. The normal is unit length in PIXELS, which NDC
// stretches by this in x, so both vectors are mapped back before scaling:
// otherwise a leaning bar tilts further than its normal and shears
uniform vec2 Aspect;
// 0 at the base, 1 at the tip; the gradient and the alpha ramp run along it
out float vRadial;
flat out uint vMask;
#ifdef GRADIENT_ROW
// 0 at the first bar, 1 at the last
uniform float InvCount;
out float vAlong;
#endif
#ifdef REVEAL_PULSE
flat out float vPeak;
#endif
#ifdef ROUND
// The output in pixels, which width and reach are fractions of
uniform vec2 OutputPx;
out vec2 vLocal;
flat out vec2 vSize;
#endif
void main() {
    vec4 s = path[gl_InstanceID];
    vec2 n = s.zw;
    // Tangent is the normal turned a quarter turn; no second lookup needed
    vec2 t = vec2(-n.y, n.x);
    // Width is a fraction of output width and reach of output height, both
    // carrying the point's scale already
    vec4 g = geom[gl_InstanceID];
    vMask = uint(g.z);
#ifdef MIRROR
    // Both ways from the path. Signed, so it interpolates across the path;
    // the fragment stage takes abs
    float side = fma(corner.y, 2.0, -1.0);
#else
    float side = corner.y;
#endif
    vRadial = side * height;
    vec2 across = vec2(t.x, t.y * Aspect.x) * g.x;
    vec2 along = vec2(n.x * Aspect.y, n.y) * g.y;
    vec2 p = fma(along, vec2(vRadial), fma(across, vec2(corner.x - 0.5), s.xy));
    gl_Position = vec4(fma(p, PathScale, PathOffset), 0.0, 1.0);
#ifdef ROUND
    vSize = vec2(g.x * 0.5 * OutputPx.x, height * g.y * 0.5 * OutputPx.y);
    vLocal = vec2((corner.x - 0.5) * vSize.x, side * vSize.y);
#endif
#ifdef GRADIENT_ROW
    vAlong = (float(gl_InstanceID) + corner.x) * InvCount;
#endif
#ifdef REVEAL_PULSE
    vPeak = height;
#endif
}
