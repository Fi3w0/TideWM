// Horizontal half of a separable Gaussian blur. Pair it with blur-v.frag
// as the next stage. Params: radius (float, texels between taps).
vec4 tide_effect(vec2 uv) {
    vec2 offset = vec2(u_texel.x * radius, 0.0);
    vec4 sum = texture2D(tex, uv) * 0.2270270270;
    sum += texture2D(tex, uv + offset * 1.3846153846) * 0.3162162162;
    sum += texture2D(tex, uv - offset * 1.3846153846) * 0.3162162162;
    sum += texture2D(tex, uv + offset * 3.2307692308) * 0.0702702703;
    sum += texture2D(tex, uv - offset * 3.2307692308) * 0.0702702703;
    return sum;
}
