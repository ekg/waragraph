#version 450

layout (location = 0) in vec2 i_uv;
layout (location = 1) in vec4 i_color;

layout (location = 0) out vec4 f_color;

layout (set = 0, binding = 0) uniform sampler u_sampler;
layout (set = 0, binding = 1) uniform texture2D u_image_in;

layout (push_constant) uniform Input {
  vec2 window_dims;
} inputs;

void main() {
  float alpha = texture(sampler2D(u_image_in, u_sampler), i_uv).r;

  /*
  vec4 nbors = textureGather(sampler2D(u_image_in, u_sampler), i_uv, 0);

  float nbor_alpha = (nbors.r + nbors.g + nbors.b + nbors.a) / 4.0;

  float a = 0.75 * alpha + 0.25 * nbor_alpha;
  */

  f_color = i_color * alpha;
  // f_color = i_color * a;
}
