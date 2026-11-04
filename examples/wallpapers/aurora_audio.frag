// Aurora & Audio-Reactive Horizon Shader for Anima
// Uses iTime, iResolution, iMouse, iAudio (bass/mid/high/level), iBeat, iChannel0 (64-band spectrum), and u_scheme_color.

void mainImage(out vec4 fragColor, in vec2 fragCoord) {
    vec2 uv = fragCoord / iResolution.xy;
    vec2 aspect = vec2(iResolution.x / max(iResolution.y, 1.0), 1.0);
    vec2 p = (uv - 0.5) * aspect;

    vec2 mouse = (iMouse.xy / max(iResolution.xy, vec2(1.0)) - 0.5) * aspect;
    p -= mouse * 0.08;

    // Deep night sky gradient
    vec3 skyTop = vec3(0.01, 0.02, 0.06);
    vec3 skyBot = vec3(0.04, 0.08, 0.16) + u_scheme_color * (0.08 + iBeat * 0.15);
    vec3 col = mix(skyBot, skyTop, clamp(uv.y * 1.2, 0.0, 1.0));

    // Aurora ribbons
    float t = iTime * 0.35;
    for (float i = 1.0; i <= 3.0; i += 1.0) {
        float wave = sin(p.x * (2.2 + i * 0.7) + t * i + sin(p.x * 4.5 - t)) * 0.12;
        wave += (iAudio.x * 0.08 + iBeat * 0.05) * sin(p.x * 8.0 + iTime * 3.0);
        float band = exp(-abs(p.y - 0.12 - wave * i * 0.5) * (14.0 - i * 2.0));
        vec3 auroraCol = mix(vec3(0.1, 0.95, 0.65), u_scheme_color, (i - 1.0) * 0.45);
        col += auroraCol * band * (0.45 + iAudio.w * 0.5);
    }

    // 64-band subtle horizon equalizer reflection
    float barX = clamp(uv.x, 0.0, 1.0);
    float spec = texture(iChannel0, vec2(barX, 0.5)).r;
    float glow = exp(-abs(uv.y - 0.08) * 18.0) * spec;
    col += u_scheme_color * glow * 0.9;

    fragColor = vec4(col, 1.0);
}
