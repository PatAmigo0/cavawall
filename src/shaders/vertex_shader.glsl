#version 430 core
// One unit quad, drawn once per bar. corner.x picks the left or right edge,
// corner.y the base or the tip; everything that makes a bar a bar comes from
// gl_InstanceID and the per-instance height, so the only per-frame upload is
// cava's own two bytes per bar
layout(location = 0) in vec2 corner;
// Normalised by the vertex fetch: cava's u16 arrives as 0..1
layout(location = 1) in float height;
uniform float BarWidth;
// One bar plus one gap: the step from a bar's left edge to the next one's
uniform float Stride;
// +1 stands on the bottom edge and grows up, -1 hangs from the top
uniform float Grow;
// 0 at the base, the bar's height at its tip, as a fraction of the surface.
// The gradient is indexed by this, so it is fixed to the surface whichever
// way the row grows: a quiet bar shows only the first stops
out float vLevel;
#ifdef ROUND
// The surface in pixels, so the rounding can be done in pixels
uniform vec2 SurfacePx;
out vec2 vLocal;
flat out vec2 vSize;
#endif
void main() {
    float x = Stride * float(gl_InstanceID) - 1.0 + corner.x * BarWidth;
    vLevel = corner.y * height;
    gl_Position = vec4(x, Grow * (vLevel * 2.0 - 1.0), 0.0, 1.0);
#ifdef ROUND
    vSize = vec2(BarWidth * 0.5 * SurfacePx.x, height * SurfacePx.y);
    vLocal = vec2((corner.x - 0.5) * vSize.x, corner.y * vSize.y);
#endif
}
