#version 430 core
// An occluder outline, already in the OUTPUT's NDC. The same affine map the
// bars use puts it in the surface, which is the curve's bounding box
layout(location = 0) in vec2 point;
uniform vec2 PathScale;
uniform vec2 PathOffset;
void main() {
    gl_Position = vec4(point * PathScale + PathOffset, 0.0, 1.0);
}
