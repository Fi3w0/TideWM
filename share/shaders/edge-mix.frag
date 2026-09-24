// Shows `original` (a textures binding) near the window's edges and the
// stage input in the middle. Params: margin (float, fraction of the size).
vec4 tide_effect(vec2 uv) {
    vec2 edge = min(uv, 1.0 - uv);
    float inner = smoothstep(0.0, margin, min(edge.x, edge.y));
    return mix(texture2D(original, uv), texture2D(tex, uv), inner);
}
