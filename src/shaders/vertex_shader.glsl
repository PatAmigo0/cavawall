#version 430 core
// Location pinned rather than left to the linker. The Rust side hardcodes
// attribute 0; that is only what a linker happens to give a lone input.
layout(location = 0) in vec2 position;
void main() {
    gl_Position = vec4(position, 0.0, 1.0);
}
