#version 450
// Textures are premultiplied; the pipeline blends with ONE, ONE_MINUS_SRC_ALPHA.
layout(set = 0, binding = 0) uniform sampler2D tex;
layout(location = 0) in vec2 uv;
layout(location = 0) out vec4 color;

void main() {
    color = texture(tex, uv);
}
