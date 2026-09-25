// Pulls the backdrop toward `tint_color` by `strength`.
// Params: strength (float), tint_color (color).
vec4 tide_effect(vec2 uv) {
    vec4 color = texture2D(tex, uv);
    vec3 tinted = mix(color.rgb, tint_color.rgb * color.a, strength);
    return vec4(tinted, color.a);
}
