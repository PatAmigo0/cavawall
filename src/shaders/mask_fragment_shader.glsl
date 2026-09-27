#version 430 core
// Written with glLogicOp(GL_XOR) into an integer target: every pixel a fan
// covers an odd number of times keeps the bit, which is a parity fill
flat in uint vBit;
out uint mask;
void main() {
    mask = vBit;
}
