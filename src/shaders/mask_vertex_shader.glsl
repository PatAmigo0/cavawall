#version 430 core
// Occluder triangles, already in the OUTPUT's NDC. The same affine map the
// bars use puts them in the surface, which is the curve's bounding box
layout(location = 0) in vec2 point;
// The occluder's bit. Integer attributes are not interpolated, and this one
// must not be: it is written as is
layout(location = 1) in uint bit;
uniform vec2 PathScale;
uniform vec2 PathOffset;
flat out uint vBit;
void main() {
    vBit = bit;
    gl_Position = vec4(fma(point, PathScale, PathOffset), 0.0, 1.0);
}
