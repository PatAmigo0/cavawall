#version 430 core
// Colour writes are masked off while this runs; only the stencil op matters
out vec4 fragColor;
void main() {
    fragColor = vec4(0.0);
}
