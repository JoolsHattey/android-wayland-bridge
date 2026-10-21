#version 450
// One textured quad per surface, drawn as a 4-vertex triangle strip.
layout(push_constant) uniform Quad {
    vec4 dst; // x0, y0, x1, y1 in NDC
    vec4 src; // u0, v0, u1, v1
} q;
layout(location = 0) out vec2 uv;

void main() {
    vec2 c = vec2(gl_VertexIndex & 1, gl_VertexIndex >> 1);
    gl_Position = vec4(mix(q.dst.xy, q.dst.zw, c), 0.0, 1.0);
    uv = mix(q.src.xy, q.src.zw, c);
}
