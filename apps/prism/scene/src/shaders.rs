//! The demo's shaders (GLSL ES 3.00).

/// A full-screen triangle.
pub const FULLSCREEN_VS: &str = "#version 300 es
in vec2 pos;
out vec2 p;
void main() {
    p = pos;
    gl_Position = vec4(pos, 0.0, 1.0);
}";

/// One face of the sky's cube map: the direction through each pixel from
/// the face's basis.
pub const SKY_FS: &str = "#version 300 es
precision highp float;
uniform vec3 forward;
uniform vec3 right;
uniform vec3 up;
uniform vec3 sunDir;
in vec2 p;
out vec4 color;

vec3 sky(vec3 d) {
    float h = d.y;
    vec3 zenith = vec3(0.07, 0.13, 0.33);
    vec3 horizon = vec3(0.88, 0.60, 0.46);
    vec3 below = vec3(0.06, 0.05, 0.08);
    vec3 c = h >= 0.0 ? mix(horizon, zenith, pow(h, 0.45)) : mix(horizon * 0.92, below, min(pow(-h, 0.6) * 1.1, 1.0));
    // High, thin cloud bands.
    if (h > 0.02) {
        vec2 q = d.xz / (h + 0.15);
        float bands = sin(q.x * 3.1 + sin(q.y * 1.7)) * sin(q.y * 2.3 + q.x * 0.6);
        c = mix(c, vec3(0.95, 0.82, 0.78), smoothstep(0.55, 0.95, bands) * 0.22 * smoothstep(0.02, 0.25, h));
    }
    float s = max(dot(d, sunDir), 0.0);
    c += vec3(1.0, 0.72, 0.45) * (pow(s, 12.0) * 0.30 + pow(s, 900.0) * 3.0);
    return min(c, vec3(1.0));
}

void main() {
    color = vec4(sky(normalize(forward + p.x * right + p.y * up)), 1.0);
}";

/// The sky behind everything: drawn last, at the far plane, where nothing
/// else was.
pub const BACKGROUND_VS: &str = "#version 300 es
in vec2 pos;
uniform mat4 invViewProj;
out vec3 dir;
void main() {
    vec4 far = invViewProj * vec4(pos, 1.0, 1.0);
    vec4 near = invViewProj * vec4(pos, -1.0, 1.0);
    dir = far.xyz / far.w - near.xyz / near.w;
    gl_Position = vec4(pos, 1.0, 1.0);
}";

pub const BACKGROUND_FS: &str = "#version 300 es
precision mediump float;
uniform samplerCube sky;
in vec3 dir;
out vec4 color;
void main() {
    color = vec4(texture(sky, dir).rgb, 1.0);
}";

/// Objects: the knot and the floor (one model matrix), or the crystals
/// (instanced: orbit angle, radius, height and scale, and a colour, per
/// instance).
pub const OBJECT_VS: &str = "#version 300 es
in vec3 pos;
in vec3 normal;
in vec4 inst;
in vec3 instColor;
uniform mat4 model;
uniform mat4 viewProj;
uniform mat4 lightViewProj;
uniform bool instanced;
uniform float time;
uniform vec3 baseColor;
out vec3 vPos;
out vec3 vNormal;
out vec3 vColor;
out vec4 vShadow;

mat3 rotY(float a) {
    float c = cos(a);
    float s = sin(a);
    return mat3(c, 0.0, -s, 0.0, 1.0, 0.0, s, 0.0, c);
}

void main() {
    vec3 p;
    vec3 n;
    vec3 col;
    if (instanced) {
        float a = inst.x + time * 0.35;
        mat3 spin = rotY(time * 1.7 + inst.x * 3.0);
        vec3 center = vec3(cos(a) * inst.y, inst.z + 0.35 * sin(time * 1.3 + inst.x * 5.0), sin(a) * inst.y);
        p = center + spin * (pos * inst.w);
        n = spin * normal;
        col = instColor;
    } else {
        p = (model * vec4(pos, 1.0)).xyz;
        n = mat3(model) * normal;
        col = baseColor;
    }
    vPos = p;
    vNormal = n;
    vColor = col;
    vShadow = lightViewProj * vec4(p, 1.0);
    gl_Position = viewProj * vec4(p, 1.0);
}";

/// Sunlight with shadows (a comparison sampler, filtered), specular
/// highlights, environment reflections weighted by Fresnel, a procedural
/// floor, tone mapping.
pub const OBJECT_FS: &str = "#version 300 es
precision highp float;
uniform vec3 eye;
uniform vec3 sunDir;
uniform vec3 sunColor;
uniform float reflectivity;
uniform bool floorPattern;
uniform bool shadows;
uniform samplerCube env;
uniform highp sampler2DShadow shadowMap;
in vec3 vPos;
in vec3 vNormal;
in vec3 vColor;
in vec4 vShadow;
out vec4 color;

float shadow() {
    if (!shadows) {
        return 1.0;
    }
    vec3 s = vShadow.xyz / vShadow.w * 0.5 + 0.5;
    if (s.x < 0.0 || s.x > 1.0 || s.y < 0.0 || s.y > 1.0 || s.z > 1.0) {
        return 1.0;
    }
    return texture(shadowMap, vec3(s.xy, s.z - 0.002));
}

void main() {
    vec3 n = normalize(vNormal);
    if (!gl_FrontFacing) {
        n = -n;
    }
    vec3 v = normalize(eye - vPos);
    vec3 base = vColor;
    if (floorPattern) {
        vec2 cell = floor(vPos.xz * 0.5);
        float check = mod(cell.x + cell.y, 2.0);
        float r = length(vPos.xz);
        base = mix(vec3(0.17, 0.18, 0.22), vec3(0.30, 0.31, 0.36), check);
    }
    float sh = shadow();
    float ndl = max(dot(n, sunDir), 0.0);
    vec3 h = normalize(sunDir + v);
    float s = max(dot(n, h), 0.0);
    s *= s;
    s *= s;
    s *= s;
    s *= s;
    s *= s;
    s *= s;
    float f = 1.0 - max(dot(n, v), 0.0);
    float f2 = f * f;
    float fresnel = 0.04 + 0.96 * f2 * f2 * f;
    vec3 reflected = texture(env, reflect(-v, n)).rgb;
    vec3 ambient = mix(vec3(0.09, 0.08, 0.09), vec3(0.20, 0.25, 0.36), n.y * 0.5 + 0.5);
    vec3 c = base * (ambient + sunColor * ndl * sh)
           + sunColor * (s * 0.9 * sh)
           + reflected * mix(fresnel, 1.0, reflectivity) * (0.15 + 0.85 * reflectivity);
    c = sqrt(c / (1.0 + c));
    // Far away, the floor fades into the sky at the horizon.
    if (floorPattern) {
        float fog = smoothstep(14.0, 38.0, length(vPos.xz));
        if (fog > 0.0) {
            // The sky behind, so that the edge of the floor is not seen.
            c = mix(c, texture(env, -v).rgb, fog);
        }
    }
    color = vec4(c, 1.0);
}";

/// Depth only (the shadow map).
pub const DEPTH_FS: &str = "#version 300 es
void main() {}";

/// Particle simulation, captured by transform feedback: fall, bounce off
/// the floor, and respawn in a fountain from the knot.
pub const PARTICLE_UPDATE_VS: &str = "#version 300 es
in vec3 pos;
in vec3 vel;
in float life;
uniform float time;
uniform float dt;
out vec3 outPos;
out vec3 outVel;
out float outLife;

float hash(float n) {
    return fract(sin(n) * 43758.5453);
}

void main() {
    vec3 p = pos;
    vec3 v = vel;
    float l = life - dt;
    if (l <= 0.0) {
        float id = float(gl_VertexID);
        float a = hash(id * 0.731 + time) * 6.2831853;
        float u = hash(id * 1.373 + time * 3.1);
        p = vec3(cos(a) * 0.3, 2.2, sin(a) * 0.3);
        v = vec3(cos(a) * (0.6 + u * 1.6), 2.5 + u * 3.0, sin(a) * (0.6 + u * 1.6));
        l = 1.2 + u * 1.8;
    } else {
        v.y -= 4.5 * dt;
        p += v * dt;
        if (p.y < 0.03) {
            p.y = 0.03;
            v.y = -v.y * 0.45;
            v.xz *= 0.8;
        }
    }
    outPos = p;
    outVel = v;
    outLife = l;
    gl_Position = vec4(0.0);
}";

pub const PARTICLE_DRAW_VS: &str = "#version 300 es
in vec3 pos;
in float life;
uniform mat4 viewProj;
uniform float pointScale;
out float vLife;
void main() {
    vec4 c = viewProj * vec4(pos, 1.0);
    gl_Position = c;
    gl_PointSize = clamp(pointScale * 0.07 / max(c.w, 0.1), 1.0, 48.0);
    vLife = life;
}";

/// A soft glowing disc, premultiplied for additive blending.
pub const PARTICLE_DRAW_FS: &str = "#version 300 es
precision mediump float;
in float vLife;
out vec4 color;
void main() {
    vec2 d = gl_PointCoord * 2.0 - 1.0;
    float r = dot(d, d);
    if (r > 1.0) {
        discard;
    }
    float a = (1.0 - r) * clamp(vLife, 0.0, 1.0);
    vec3 c = mix(vec3(1.0, 0.32, 0.08), vec3(1.0, 0.92, 0.65), clamp(vLife * 0.5, 0.0, 1.0));
    color = vec4(c * a, a);
}";
