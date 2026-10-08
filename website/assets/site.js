/* vedaos.org — the page's motion, in Veda's own visual language:
 *
 * - the hero's sky is the Aurora wallpaper (tools/assetgen/src/wallpapers.rs)
 *   brought to life in a WebGL shader; the ring's light is CSS;
 * - the agent's figure — OS1's ring and the coil from "Her" — is a port of
 *   apps/shell/src/presence.rs and the states of apps/shell/src/agent.rs;
 * - everything else is scroll-driven, and runs only while on screen.
 *
 * No dependencies. Respects prefers-reduced-motion. */
(() => {
  'use strict';

  const root = document.documentElement;
  const motion = matchMedia('(prefers-reduced-motion: reduce)');
  let still = motion.matches;
  motion.addEventListener?.('change', (e) => { still = e.matches; });
  const $ = (s) => document.querySelector(s);
  const $$ = (s) => Array.from(document.querySelectorAll(s));
  const TAU = Math.PI * 2;
  const clamp = (x, a = 0, b = 1) => Math.min(b, Math.max(a, x));
  // The shell's ease: smoothstep.
  const ease = (x) => { x = clamp(x); return x * x * (3 - 2 * x); };
  const smooth = (a, b, x) => ease((x - a) / (b - a));
  const lerp = (a, b, t) => a + (b - a) * t;
  const ratio = () => Math.min(window.devicePixelRatio || 1, 2);

  // ---- one frame loop for everything that moves ------------------------
  const loops = new Set();
  let rafId = 0;
  const tick = (now) => {
    rafId = 0;
    for (const f of [...loops]) f(now);
    if (loops.size && !document.hidden) rafId = requestAnimationFrame(tick);
  };
  const run = (f) => { loops.add(f); if (!rafId && !document.hidden) rafId = requestAnimationFrame(tick); };
  const halt = (f) => loops.delete(f);
  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && loops.size && !rafId) rafId = requestAnimationFrame(tick);
  });
  /** Runs `f` every frame while `el` is (nearly) on screen. */
  const onScreen = (el, f, margin = '120px') => {
    if (!('IntersectionObserver' in window)) return run(f);
    new IntersectionObserver(([e]) => (e.isIntersecting ? run(f) : halt(f)), { rootMargin: margin }).observe(el);
  };

  // ---- scroll-linked effects: all reads, then all writes, once a frame --
  const scrollFx = [];
  let queued = false;
  const flush = () => {
    queued = false;
    const vh = innerHeight;
    const reads = scrollFx.map((fx) => fx.read(vh));
    scrollFx.forEach((fx, i) => fx.write(reads[i], vh));
  };
  const request = () => { if (!queued) { queued = true; requestAnimationFrame(flush); } };
  addEventListener('scroll', request, { passive: true });
  addEventListener('resize', request);

  // ---- the agent's figure (apps/shell/src/presence.rs) ------------------
  const RING_WIDTH = 0.25;
  const tremble = (a, r, amount, t) => {
    if (amount <= 0) return 0;
    const wave = Math.sin(5 * a + 3.1 * t) + 0.6 * Math.sin(8 * a - 4.3 * t) + 0.4 * Math.sin(3 * a + 2.2 * t);
    return r * amount * 0.022 * wave;
  };
  /** OS1's ring: the band between two outlines, trembling by `amount`. */
  const ring = (c, cx, cy, r, w, amount, t, alpha) => {
    const n = Math.max(32, Math.min(160, Math.floor(r * 1.6)));
    c.beginPath();
    for (const half of [w / 2, -w / 2]) {
      for (let i = 0; i < n; i++) {
        const a = (i / n) * TAU;
        const rr = Math.max(0, r + half + tremble(a, r, amount, t));
        const x = cx + rr * Math.cos(a), y = cy + rr * Math.sin(a);
        if (i) c.lineTo(x, y); else c.moveTo(x, y);
      }
      c.closePath();
    }
    c.fillStyle = `rgba(255,255,255,${clamp(alpha)})`;
    c.fill('evenodd');
  };
  /** A soft glow: bands laid over each other, each wider than the last. */
  const glow = (c, cx, cy, r, w, spread, alpha) => {
    if (alpha <= 0) return;
    const each = 1 - Math.pow(1 - Math.min(alpha, 0.95), 1 / 8);
    for (let k = 1; k <= 8; k++) ring(c, cx, cy, r, w + (2 * spread * k) / 8, 0, 0, each);
  };
  /** The ring with a light running around it (the agent thinking). */
  const ringWithLight = (c, cx, cy, r, w, at, base, peak) => {
    ring(c, cx, cy, r, w, 0, 0, base / 255);
    const rest = Math.max(1 - peak / 255, 0.004) / Math.max(1 - base / 255, 0.004);
    c.fillStyle = `rgba(255,255,255,${1 - Math.pow(rest, 1 / 24)})`;
    const ri = r - w / 2, ro = r + w / 2;
    for (let k = 1; k <= 24; k++) {
      const half = ((TAU / 3) * k) / 24;
      c.beginPath();
      c.arc(cx, cy, ro, at - half, at + half);
      c.arc(cx, cy, ri, at + half, at - half, true);
      c.closePath();
      c.fill();
    }
  };

  // The coil: the film's loading figure, a ribbon wound three times around
  // a long loop that spins about its length. Turned to face the viewer and
  // brought closer, it becomes the ring.
  const LENGTH = 30, WIND = 5.6, RIBBON = 1.1, SAMPLES = 200, CAMERA = 150, APPROACH = 70;
  const NEAR = CAMERA / (CAMERA - LENGTH - APPROACH);
  const sub = (a, b) => [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
  const dot = (a, b) => a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
  const cross = (a, b) => [a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0]];
  const norm = (a) => { const l = Math.sqrt(dot(a, a)); return l < 1e-9 ? a : [a[0] / l, a[1] / l, a[2] / l]; };
  const rotate = (v, k, angle) => {
    const s = Math.sin(angle), c = Math.cos(angle), kv = cross(k, v), d = dot(k, v) * (1 - c);
    return [v[0] * c + kv[0] * s + k[0] * d, v[1] * c + kv[1] * s + k[1] * d, v[2] * c + kv[2] * s + k[2] * d];
  };
  const coilPoint = (t) => {
    const x = LENGTH * Math.sin(TAU * t);
    const y = WIND * Math.cos(TAU * 3 * t);
    const q = (t % 0.25) / 0.25;
    let shift = (t % 0.25) - (2 * (1 - q) * q * -0.0185 + q * q * 0.25);
    const quarter = Math.floor(t / 0.25);
    if (quarter === 0 || quarter === 2) shift = -shift;
    return [x, y, WIND * Math.sin(TAU * 2 * (t - shift))];
  };
  // The coil's points and, at each, the direction the ribbon spreads in: a
  // frame carried along it without twisting, its mismatch where the loop
  // closes spread evenly.
  const RIBBON_FRAME = (() => {
    const n = SAMPLES;
    const pts = Array.from({ length: n + 1 }, (_, i) => coilPoint(i / n));
    const tan = (i) => norm(sub(pts[(i + 1) % n], pts[(i + n - 1) % n]));
    const tangents = Array.from({ length: n + 1 }, (_, i) => tan(i % n));
    const t0 = tangents[0];
    const ax = Math.abs(t0[0]) <= Math.abs(t0[1]) && Math.abs(t0[0]) <= Math.abs(t0[2]) ? [1, 0, 0]
      : Math.abs(t0[1]) <= Math.abs(t0[2]) ? [0, 1, 0] : [0, 0, 1];
    const normals = [cross(t0, norm(cross(t0, ax)))];
    for (let i = 1; i <= n; i++) {
      const v = cross(tangents[i - 1], tangents[i]);
      const l = Math.sqrt(dot(v, v));
      normals.push(l > 1e-6
        ? rotate(normals[i - 1], [v[0] / l, v[1] / l, v[2] / l], Math.acos(clamp(dot(tangents[i - 1], tangents[i]), -1, 1)))
        : normals[i - 1]);
    }
    let theta = Math.acos(clamp(dot(normals[0], normals[n]), -1, 1)) / n;
    if (dot(tangents[0], cross(normals[0], normals[n])) > 0) theta = -theta;
    for (let i = 1; i <= n; i++) normals[i] = rotate(normals[i], tangents[i], theta * i);
    return { pts, normals };
  })();
  const coil = (c, cx, cy, scale, spin, turn, alpha, thin) => {
    const { pts, normals } = RIBBON_FRAME;
    const ss = Math.sin(spin), cs = Math.cos(spin);
    const st = Math.sin((-Math.PI / 2) * turn), ct = Math.cos((-Math.PI / 2) * turn);
    const view = (p) => {
      const y = p[1] * cs - p[2] * ss;
      const z = p[1] * ss + p[2] * cs;
      const x = p[0] * ct + z * st;
      const depth = -p[0] * st + z * ct + APPROACH * turn;
      const k = CAMERA / Math.max(CAMERA - depth, 1);
      return [cx + scale * x * k, cy - scale * y * k, z];
    };
    // How much of the white is left behind the film's faint veils, which
    // matter less as the coil turns to face the viewer.
    const fade = (z) => {
      const f = Math.pow(0.87, clamp((2 - z) / 0.5, 0, 10));
      return f + (1 - f) * turn;
    };
    const LEVELS = 20;
    const centre = pts.map(view);
    const edge = (s) => (thin ? [] : pts.map((p, i) => {
      const n = normals[i];
      return view([p[0] + s * n[0], p[1] + s * n[1], p[2] + s * n[2]]);
    }));
    const a = edge(RIBBON), b = edge(-RIBBON);
    c.lineWidth = thin ? 1.3 : Math.max(RIBBON * scale * 0.7, 1);
    c.lineJoin = 'round';
    const draw = (runPts, level) => {
      if (runPts.length < 2) return;
      const color = `rgba(255,255,255,${level / (LEVELS - 1)})`;
      if (!thin) {
        c.beginPath();
        c.moveTo(a[runPts[0]][0], a[runPts[0]][1]);
        for (let j = 1; j < runPts.length; j++) c.lineTo(a[runPts[j]][0], a[runPts[j]][1]);
        for (let j = runPts.length - 1; j >= 0; j--) c.lineTo(b[runPts[j]][0], b[runPts[j]][1]);
        c.closePath();
        c.fillStyle = color;
        c.fill('nonzero');
      }
      c.beginPath();
      c.moveTo(centre[runPts[0]][0], centre[runPts[0]][1]);
      for (let j = 1; j < runPts.length; j++) c.lineTo(centre[runPts[j]][0], centre[runPts[j]][1]);
      c.strokeStyle = color;
      c.stroke();
    };
    let runPts = [], runLevel = 0;
    for (let i = 0; i < SAMPLES; i++) {
      const shade = fade((centre[i][2] + centre[i + 1][2]) / 2) * alpha;
      const level = Math.min(Math.round(shade * (LEVELS - 1)), LEVELS - 1);
      if (level !== runLevel && runPts.length) { draw(runPts, runLevel); runPts = []; }
      runLevel = level;
      if (level === 0) continue;
      if (!runPts.length) runPts.push(i);
      runPts.push(i + 1);
    }
    draw(runPts, runLevel);
  };
  // How fast the coil spins, and how much more it turns as it becomes the
  // ring (apps/shell/src/agent.rs).
  const SPIN = 2.1, SPIN_EXTRA = 9;
  /** The figure for `f` = {state, v, voice, mic} around (cx, cy), a ring
   * of radius `base`; `v` below 1 is the coil turning into the ring. */
  const presence = (c, cx, cy, base, f, t) => {
    const width = Math.max(RING_WIDTH * base, 2.2);
    if (f.v < 1) {
      const ringA = ease((f.v - 0.85) / 0.15);
      const coilA = 1 - ease((f.v - 0.8) / 0.12);
      if (coilA > 0) {
        const turn = ease((f.v - 0.25) / 0.75);
        coil(c, cx, cy, base / (WIND * NEAR), SPIN * t + SPIN_EXTRA * f.v * f.v * f.v, turn, coilA, base < 16);
      }
      if (ringA > 0) ring(c, cx, cy, base * (0.9 + 0.1 * ringA), width, 0, t, ringA);
      return;
    }
    switch (f.state) {
      case 'speaking': {
        const w = width * (1 + 0.35 * f.voice);
        glow(c, cx, cy, base, w, base * (0.18 + 0.12 * f.voice), 0.16 + 0.3 * f.voice);
        ring(c, cx, cy, base, w, 0.1 + 0.5 * f.voice, t, 1);
        break;
      }
      case 'listening': {
        const breath = 0.5 + 0.5 * Math.sin(t * 1.6);
        const r = base * (0.98 + 0.02 * breath + 0.04 * f.mic);
        glow(c, cx, cy, r, width, base * 0.16, 0.06 + 0.08 * breath + 0.25 * f.mic);
        ring(c, cx, cy, r, width, 0.3 * f.mic, t * 0.6, 0.98);
        break;
      }
      case 'thinking':
        ringWithLight(c, cx, cy, base, width, t * 3, 150, 255);
        break;
      case 'asleep':
        ring(c, cx, cy, base * 0.94, width, 0, t, 0.6);
        break;
      default:
        ring(c, cx, cy, base, width, 0, t, 0.9);
    }
  };
  // A voice's loudness over time: syllables and words, never quite still.
  const voiceAt = (s) => clamp(0.32 + 0.38 * Math.sin(s * 11.3) * Math.sin(s * 3.7 + 1) + 0.3 * Math.sin(s * 23.1) ** 2);
  const micAt = (s) => clamp(0.25 + 0.3 * Math.sin(s * 9.1) * Math.sin(s * 2.3 + 2) + 0.25 * Math.sin(s * 17.7) ** 2);

  /** A canvas that sizes itself to its box at the device's pixel ratio.
   * Resizing clears it: `onFit` is called after each later resize, for a
   * figure that is not redrawn every frame. */
  const surface = (canvas, onFit) => {
    const ctx = canvas.getContext('2d');
    const s = { ctx, w: 0, h: 0 };
    const fit = () => {
      const r = canvas.getBoundingClientRect(), d = ratio();
      s.w = r.width; s.h = r.height;
      canvas.width = Math.max(1, Math.round(r.width * d));
      canvas.height = Math.max(1, Math.round(r.height * d));
      ctx.setTransform(d, 0, 0, d, 0, 0);
    };
    fit();
    const refit = () => { fit(); onFit?.(); };
    if ('ResizeObserver' in window) new ResizeObserver(refit).observe(canvas); else addEventListener('resize', refit);
    s.clear = () => ctx.clearRect(0, 0, s.w, s.h);
    return s;
  };

  // ---- header: solid once scrolled, the ring docks into the logo --------
  const hdr = $('#hdr');
  const menu = $('#menu');
  const sheet = $('#sheet');
  const setMenu = (open) => {
    root.classList.toggle('menu-open', open);
    menu.setAttribute('aria-expanded', String(open));
    menu.setAttribute('aria-label', open ? 'Close the menu' : 'Menu');
    // Behind the open menu, the page is out of reach.
    for (const el of [$('main'), $('.foot')]) if (el) el.inert = open;
    if (open) sheet.querySelector('a')?.focus();
  };
  menu?.addEventListener('click', () => setMenu(!root.classList.contains('menu-open')));
  sheet?.addEventListener('click', (e) => { if (e.target.closest('a')) setMenu(false); });
  addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && root.classList.contains('menu-open')) { setMenu(false); menu.focus(); }
  });

  const heroRing = $('#hero-ring'), fly = $('#ring-fly'), brandRing = $('#brand-ring');
  const flyGlow = fly?.querySelector('.glow-f');
  if (heroRing && fly && brandRing) {
    scrollFx.push({
      read: () => ({ a: heroRing.getBoundingClientRect(), b: brandRing.getBoundingClientRect(), y: scrollY }),
      write: ({ a, b, y }, vh) => {
        hdr.classList.toggle('solid', y > 8);
        if (still) { hdr.classList.add('docked'); fly.style.transform = ''; return; }
        // The hero's ring flies into the logo over the first half screen.
        const p = ease(y / (vh * 0.5));
        const docked = p >= 0.999;
        hdr.classList.toggle('docked', docked);
        fly.style.opacity = docked ? '0' : '';
        if (p <= 0) { fly.style.transform = ''; if (flyGlow) flyGlow.style.opacity = ''; return; }
        const dx = b.left + b.width / 2 - (a.left + a.width / 2);
        const dy = b.top + b.height / 2 - (a.top + a.height / 2);
        fly.style.transform = `translate3d(${dx * p}px,${dy * p}px,0) scale(${lerp(1, b.width / a.width, p)})`;
        if (flyGlow) flyGlow.style.opacity = String(1 - smooth(0, 0.6, p));
      },
    });
  } else {
    hdr?.classList.add('docked');
  }

  // ---- the hero's sky: the Aurora wallpaper, flowing --------------------
  const SKY = `
#ifdef GL_FRAGMENT_PRECISION_HIGH
precision highp float;
#else
precision mediump float;
#endif
uniform vec2 uRes;
uniform float uTime;
uniform float uShift;
const float PI = 3.14159265;
const float TAU = 6.2831853;
float sq(float x) { return x * x; }
float hash(vec2 p) { p = fract(p * vec2(234.34, 435.345)); p += dot(p, p + 34.23); return fract(p.x * p.y); }
float noise(vec2 p) {
  vec2 i = floor(p), f = fract(p);
  vec2 u = f * f * (3.0 - 2.0 * f);
  return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x), mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x), u.y) * 2.0 - 1.0;
}
float fbm(vec2 p) {
  float s = 0.0, a = 0.5;
  for (int i = 0; i < 3; i++) { s += a * noise(p); p = mat2(1.6, 1.2, -1.2, 1.6) * p + vec2(17.3, -9.1); a *= 0.5; }
  return s / 0.875;
}
vec3 bloom(vec3 c, vec2 uv, float aspect, vec2 ctr, float rad, vec3 col, float s) {
  vec2 d = (uv - ctr) * vec2(aspect, 1.0);
  float k = max(1.0 - dot(d, d) / (rad * rad), 0.0);
  return mix(c, col, k * k * s);
}
vec3 grad3(float u, vec3 a, vec3 b, vec3 c) {
  return u < 0.5 ? mix(a, b, smoothstep(0.0, 1.0, u * 2.0)) : mix(b, c, smoothstep(0.0, 1.0, u * 2.0 - 1.0));
}
// One silky ribbon: a glowing core, a soft body, a filament and a sheen,
// a halo around it, and faint curtains of rays rising above it.
vec3 ribbon(vec2 uv, float t, float y0, vec3 w0, vec3 w1, vec3 w2, float width, vec2 twist,
            vec3 c0, vec3 c1, vec3 c2, float strength, float halo, float curtain, float speed, float seed) {
  float u = uv.x;
  float tw = abs(cos(PI * (twist.x * u + twist.y + t * 0.021 * speed)));
  float centre = y0 + w0.x * sin(TAU * w0.y * u + w0.z + t * 0.11 * speed)
                    + w1.x * sin(TAU * w1.y * u + w1.z - t * 0.17 * speed)
                    + w2.x * sin(TAU * w2.y * u + w2.z + t * 0.23 * speed)
                    + 0.012 * noise(vec2(u * 3.0 + t * 0.04, seed * 7.0));
  vec3 col = grad3(u, c0, c1, c2);
  float hw = width * (0.16 + 0.84 * tw);
  float sheenW = 1.0 + 0.9 * pow(1.0 - tw, 4.0);
  float dv = uv.y - centre;
  float d = dv / hw;
  float ad = abs(d);
  vec3 light = vec3(0.0);
  if (ad < 1.2) {
    float core = exp(-sq(d / 0.32)) * 0.95;
    float body = pow(max(1.0 - pow(ad / 1.2, 2.0), 0.0), 1.5) * 0.42;
    float fil = exp(-sq((d + 0.45) / 0.07)) * 0.35;
    float sheen = 0.93 + 0.07 * sin(d * 26.0 + u * 5.0 + t * 0.6);
    light += mix(col, vec3(1.0), core * 0.35) * (core + body + fil) * sheen * sheenW * strength;
  }
  float sigma = width * 2.8;
  light += col * exp(-sq(dv / sigma)) * halo * strength * (0.6 + 0.4 * sheenW) * 0.55;
  if (curtain > 0.0 && dv < 0.0) {
    float rise = exp(dv / (0.16 - 0.06 * seed));
    float rays = pow(noise(vec2(u * 90.0 + seed * 13.0, t * 0.05)) * 0.5 + 0.5, 2.0)
               * (0.6 + 0.4 * noise(vec2(u * 7.0, seed * 3.0 + t * 0.03)));
    light += mix(col, vec3(0.486, 1.0, 0.784), 0.25) * rise * rays * 0.32 * strength * smoothstep(0.0, -1.5 * hw, dv) * curtain;
  }
  return light;
}
void main() {
  vec2 uv = gl_FragCoord.xy / uRes;
  uv.y = 1.0 - uv.y;
  float aspect = uRes.x / uRes.y;
  float t = uTime;
  vec2 q = vec2(uv.x, uv.y + uShift * 0.1);
  vec3 c = mix(vec3(0.0196, 0.0314, 0.102), vec3(0.0824, 0.0588, 0.227), smoothstep(0.0, 1.0, uv.y));
  c = bloom(c, uv, aspect, vec2(0.20 + 0.02 * sin(t * 0.05), 0.22), 0.70, vec3(0.169, 0.282, 0.839), 0.50);
  c = bloom(c, uv, aspect, vec2(0.86, 0.70 + 0.02 * sin(t * 0.04)), 0.62, vec3(0.478, 0.169, 0.851), 0.45);
  c = bloom(c, uv, aspect, vec2(0.62, 0.10), 0.45, vec3(0.086, 0.608, 0.82), 0.25);
  c = bloom(c, uv, aspect, vec2(0.40, 1.02), 0.55, vec3(0.761, 0.2, 0.541), 0.22);
  c = bloom(c, uv, aspect, vec2(0.05, 0.92), 0.40, vec3(0.227, 0.122, 0.62), 0.30);
  c *= 0.86 + 0.28 * (fbm(vec2(uv.x * 2.5 * aspect, uv.y * 2.5) + vec2(t * 0.012, 0.0)) * 0.5 + 0.5);
  c += ribbon(q, t, 0.745, vec3(0.13, 0.55, 2.3), vec3(0.045, 1.35, 0.4), vec3(0.015, 3.1, 1.7), 0.050, vec2(1.15, 0.30),
              vec3(0.184, 0.902, 1.0), vec3(0.361, 0.486, 1.0), vec3(0.761, 0.392, 1.0), 1.0, 0.55, 1.0, 1.0, 0.0);
  c += ribbon(q, t, 0.675, vec3(0.11, 0.62, 2.9), vec3(0.05, 1.1, 2.2), vec3(0.012, 2.7, 0.3), 0.022, vec2(1.7, 0.75),
              vec3(0.204, 0.961, 0.753), vec3(0.251, 0.722, 1.0), vec3(0.541, 0.49, 1.0), 0.75, 0.40, 1.0, 1.3, 1.0);
  c += ribbon(q, t, 0.85, vec3(0.09, 0.48, 1.6), vec3(0.04, 1.6, 3.0), vec3(0.01, 3.7, 2.2), 0.034, vec2(0.9, 0.1),
              vec3(0.416, 0.361, 1.0), vec3(0.753, 0.298, 0.941), vec3(1.0, 0.361, 0.659), 0.55, 0.45, 0.0, 0.8, 2.0);
  float r = length(vec2(uv.x - 0.5, (uv.y - 0.5) / aspect)) * 1.6;
  c *= 1.0 - 0.35 * smoothstep(0.35, 1.05, r);
  c = mix(max(c, 0.0), 0.7 + 0.3 * (1.0 - exp(-(c - 0.7) / 0.3)), step(0.7, c));
  gl_FragColor = vec4(c, 1.0);
}`;

  const sky = () => {
    const canvas = $('#aurora');
    if (!canvas) return;
    let gl = null;
    try {
      gl = canvas.getContext('webgl', { alpha: false, antialias: false, depth: false, stencil: false, powerPreference: 'low-power' });
    } catch (_) { /* no WebGL: the CSS sky stays */ }
    if (!gl) return;
    const shader = (type, src) => {
      const s = gl.createShader(type);
      gl.shaderSource(s, src);
      gl.compileShader(s);
      return gl.getShaderParameter(s, gl.COMPILE_STATUS) ? s : null;
    };
    const vs = shader(gl.VERTEX_SHADER, 'attribute vec2 p;void main(){gl_Position=vec4(p,0.0,1.0);}');
    const fs = shader(gl.FRAGMENT_SHADER, SKY);
    if (!vs || !fs) return;
    const prog = gl.createProgram();
    gl.attachShader(prog, vs);
    gl.attachShader(prog, fs);
    gl.linkProgram(prog);
    if (!gl.getProgramParameter(prog, gl.LINK_STATUS)) return;
    gl.useProgram(prog);
    gl.bindBuffer(gl.ARRAY_BUFFER, gl.createBuffer());
    gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 3, -1, -1, 3]), gl.STATIC_DRAW);
    const loc = gl.getAttribLocation(prog, 'p');
    gl.enableVertexAttribArray(loc);
    gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
    const uRes = gl.getUniformLocation(prog, 'uRes');
    const uTime = gl.getUniformLocation(prog, 'uTime');
    const uShift = gl.getUniformLocation(prog, 'uShift');
    // The sky is soft light: it is drawn below the screen's resolution and
    // scaled up, the more so on phones and when frames come slowly.
    let scale = matchMedia('(pointer: coarse)').matches ? 0.5 : 0.7;
    let cssW = 0, cssH = 0;
    const fit = () => {
      const d = Math.min(window.devicePixelRatio || 1, 1.5);
      const w = Math.max(1, Math.round(cssW * d * scale)), h = Math.max(1, Math.round(cssH * d * scale));
      if (canvas.width !== w || canvas.height !== h) { canvas.width = w; canvas.height = h; gl.viewport(0, 0, w, h); }
      gl.uniform2f(uRes, w, h);
    };
    const measure = () => { const r = canvas.getBoundingClientRect(); cssW = r.width; cssH = r.height; fit(); };
    measure();
    if ('ResizeObserver' in window) new ResizeObserver(measure).observe(canvas); else addEventListener('resize', measure);
    let shift = 0;
    scrollFx.push({ read: (vh) => scrollY / vh, write: (s) => { shift = clamp(s); } });
    const t0 = performance.now() - 9000;
    let shown = false, last = 0, slow = 0;
    const draw = (now) => {
      gl.uniform1f(uTime, (now - t0) / 1000);
      gl.uniform1f(uShift, shift);
      gl.drawArrays(gl.TRIANGLES, 0, 3);
      if (!shown) {
        shown = true;
        // As the desktop dissolves in after the splash.
        setTimeout(() => canvas.classList.add('on'), Math.max(0, 900 - performance.now()));
      }
      if (last && now - last > 30 && !document.hidden) slow++; else slow = Math.max(0, slow - 1);
      if (slow > 40 && scale > 0.3) { scale *= 0.8; slow = 0; fit(); }
      last = now;
    };
    canvas.addEventListener('webglcontextlost', (e) => { e.preventDefault(); halt(draw); canvas.classList.remove('on'); });
    if (still) { draw(performance.now()); return; }
    onScreen(canvas, draw, '0px');
  };

  // ---- "Most computers wait for your clicks." word by word --------------
  const statement = () => {
    const el = $('[data-words]');
    if (!el) return;
    const parts = el.textContent.split(/(\s+)/);
    el.textContent = '';
    for (const part of parts) {
      if (!part) continue;
      if (/^\s+$/.test(part)) { el.append(part); continue; }
      const w = document.createElement('span');
      w.className = 'w';
      w.textContent = part;
      el.append(w);
    }
    const words = [...el.querySelectorAll('.w')];
    let lit = -1;
    scrollFx.push({
      read: () => el.getBoundingClientRect(),
      write: (r, vh) => {
        const p = still ? 1 : clamp((vh * 0.9 - r.top) / (vh * 0.55));
        const n = Math.round(p * words.length);
        if (n === lit) return;
        lit = n;
        words.forEach((w, i) => w.classList.toggle('on', i < n));
      },
    });
  };

  // ---- the desktop's screenshot settles flat as it comes up -------------
  const screen = () => {
    const el = $('#screen');
    if (!el) return;
    scrollFx.push({
      read: () => el.getBoundingClientRect(),
      write: (r, vh) => {
        const p = still ? 1 : ease((vh - r.top) / (vh * 0.8));
        el.style.setProperty('--tilt', `${(1 - p) * 18}deg`);
        el.style.setProperty('--sc', String(0.9 + 0.1 * p));
      },
    });
  };

  // ---- reveals ----------------------------------------------------------
  const reveals = () => {
    const els = [...$$('[data-reveal]'), ...$$('#stack')];
    // What is on screen already stays as it is; the rest comes in as it
    // scrolls into view. Without this script, everything simply shows.
    for (const e of els) if (e.getBoundingClientRect().top < innerHeight) e.classList.add('in');
    root.classList.add('motion');
    if (still || !('IntersectionObserver' in window)) { els.forEach((e) => e.classList.add('in')); return; }
    const io = new IntersectionObserver((entries) => {
      for (const e of entries) {
        if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); }
      }
    }, { rootMargin: '0px 0px -8% 0px', threshold: 0.12 });
    els.forEach((e) => io.observe(e));
  };

  // ---- the agent's window: a simulated conversation ---------------------
  const TILES = {
    timer: ['#ffc14d', '#e0761f', 'i-clock'],
    photos: ['#ff7ab2', '#a64dff', 'i-image'],
    editor: ['#5b95ff', '#2f5bdb', 'i-doc'],
    tasks: ['#35d9ae', '#10897a', 'i-chart'],
    files: ['#ffc857', '#f08a24', 'i-folder'],
  };
  const inMinutes = (m) => new Date(Date.now() + m * 60000).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' });
  const DEMOS = {
    remind: {
      say: 'Remind me to stretch in twenty minutes.',
      steps: [['timer', 'Setting a reminder', 'timer · set · 1200 s · “Stretch”']],
      reply: () => `Done. I’ll remind you to stretch at ${inMinutes(20)}.`,
    },
    photo: {
      say: 'Show me the lighthouse photo, full screen.',
      steps: [
        ['photos', 'Opening Photos', 'open_app · photos'],
        ['photos', 'Showing “Lighthouse”', 'use_app · photos · show_picture · Lighthouse.jpg'],
        ['photos', 'Going full screen', 'use_app · photos · full_screen'],
      ],
      reply: () => 'Here’s the lighthouse, full screen.',
    },
    list: {
      say: 'Write a shopping list with eggs, milk and coffee.',
      steps: [
        ['editor', 'Opening the Text Editor', 'use_app · editor · new_document'],
        ['editor', 'Writing the list', 'use_app · editor · write · “Eggs, milk, coffee”'],
        ['editor', 'Saving it', 'use_app · editor · save_as · Shopping list.txt'],
      ],
      reply: () => 'Your shopping list is saved in Documents.',
    },
    memory: {
      say: 'What’s using the most memory?',
      steps: [['tasks', 'Looking at the running programs', 'tasks · list']],
      reply: () => 'Photos is using the most memory right now, then the window system.',
    },
    delete: {
      say: 'Delete the City Lights photo.',
      steps: [['files', 'Moving “City Lights.jpg” to the Trash', 'files · delete · ~/Pictures/City Lights.jpg']],
      ask: { title: 'Move “City Lights.jpg” to the Trash', detail: '~/Pictures/City Lights.jpg can be restored from the Trash until it is emptied.' },
      replies: { allow: 'Done — it’s in the Trash, if you change your mind.', deny: 'Okay, I’ve left it where it is.' },
    },
  };
  const AUTOPLAY = ['remind', 'photo', 'list', 'memory'];

  const demo = () => {
    const canvas = $('#orb'), log = $('#log'), caption = $('#caption'), prompts = $('#prompts');
    if (!canvas || !log) return;
    const s = surface(canvas);
    // The figure, as AgentModel has it: the state, how much of a ring it
    // is (0 the coil), and the voice and microphone levels.
    const fig = { state: 'asleep', v: 1, voice: 0, mic: 0, from: 1, to: 1, since: 0, span: 1 };
    const morph = (to, span, now) => {
      fig.from = figureAt(now); fig.to = to; fig.span = span; fig.since = now;
    };
    const figureAt = (now) => (fig.since ? fig.from + (fig.to - fig.from) * ease((now - fig.since) / fig.span) : fig.to);
    let talking = 0, hearing = 0;
    const draw = (now) => {
      fig.v = still ? 1 : figureAt(now);
      const sec = now / 1000;
      fig.voice = talking ? voiceAt(sec) : Math.max(0, fig.voice - 0.08);
      fig.mic = hearing ? micAt(sec) : Math.max(0, fig.mic - 0.06);
      s.clear();
      presence(s.ctx, s.w / 2, s.h / 2, Math.min(s.w, s.h) * 0.32, fig, still ? 0 : sec);
    };
    onScreen(canvas, draw);
    if (still) draw(0);

    let token = 0, auto = true, autoAt = 0, idle = 0;
    const STOP = Symbol('stop');
    const wait = (ms, my) => new Promise((res, rej) => setTimeout(() => (my === token ? res() : rej(STOP)), still ? Math.min(ms, 60) : ms));
    const say = (text) => { caption.textContent = text; };
    const add = (cls, html) => {
      const li = document.createElement('li');
      li.className = cls;
      if (html instanceof Node) li.append(html); else li.textContent = html;
      log.append(li);
      while (log.children.length > 7) log.firstElementChild.remove();
      return li;
    };
    const step = ([tile, label, detail]) => {
      const [a, b, icon] = TILES[tile];
      const li = document.createElement('li');
      li.className = 'act';
      li.innerHTML = `<span class="t" style="--a:${a};--b:${b}"><svg class="i" aria-hidden="true"><use href="#${icon}"/></svg></span><div><b></b><small></small></div><span class="st" aria-hidden="true"></span>`;
      li.querySelector('b').textContent = label;
      li.querySelector('small').textContent = detail;
      log.append(li);
      return li;
    };
    const words = async (li, text, my, per) => {
      const parts = text.split(' ');
      li.textContent = '';
      for (let i = 0; i < parts.length; i++) {
        li.textContent = parts.slice(0, i + 1).join(' ');
        await wait(per, my);
      }
    };

    const play = async (name) => {
      const my = ++token;
      const d = DEMOS[name];
      clearTimeout(idle);
      $$('#prompts .chip').forEach((c) => c.setAttribute('aria-pressed', String(c.dataset.demo === name)));
      log.textContent = '';
      talking = 0; hearing = 0;
      try {
        const now = performance.now();
        if (fig.state === 'asleep') {
          // Woken by its name: the ring becomes the coil, and the coil the
          // ring once it is ready to listen.
          fig.state = 'waking'; say('One moment…');
          morph(0, 700, now);
          await wait(950, my);
          fig.state = 'listening';
          morph(1, 1200, performance.now());
          await wait(900, my);
        }
        fig.state = 'listening'; say('Listening');
        hearing = 1;
        const you = add('you', '');
        await words(you, d.say, my, 190);
        hearing = 0;
        await wait(250, my);
        fig.state = 'thinking'; say('Thinking…');
        await wait(900, my);
        for (const st of d.steps) {
          const li = step(st);
          if (d.ask) {
            li.classList.add('held');
            fig.state = 'listening'; say('Waiting for your OK');
            const answer = await ask(d.ask, my);
            li.classList.remove('held');
            if (answer !== 'allow') { li.remove(); await reply(d.replies.deny, my); return finish(my); }
            fig.state = 'thinking'; say('');
            await wait(600, my);
            li.classList.add('done');
            await wait(350, my);
            await reply(d.replies.allow, my);
            return finish(my);
          }
          await wait(800, my);
          li.classList.add('done');
          await wait(280, my);
        }
        await reply(d.reply(), my);
        finish(my);
      } catch (e) {
        if (e !== STOP) throw e;
      }
    };
    const reply = async (text, my) => {
      fig.state = 'speaking'; say('');
      talking = 1;
      const li = add('veda', '');
      await words(li, text, my, 210);
      await wait(400, my);
      talking = 0;
    };
    const finish = (my) => {
      if (my !== token) return;
      fig.state = 'listening'; say('Listening');
      // Back to sleep after a while, as the agent does when you are done.
      idle = setTimeout(() => { if (my === token) { fig.state = 'asleep'; say('Say “Hey Veda”'); } }, 9000);
      if (auto) {
        autoAt = (autoAt + 1) % AUTOPLAY.length;
        setTimeout(() => { if (auto && my === token && visible) play(AUTOPLAY[autoAt]); }, 4200);
      }
    };
    const ask = (q, my) => new Promise((res, rej) => {
      const card = document.createElement('div');
      card.innerHTML = '<p class="k">Veda needs your OK</p><p class="tt"></p><p class="dd"></p><div class="row"><button type="button" data-a="deny">Deny</button><button type="button" class="ok" data-a="allow">Allow</button></div>';
      card.querySelector('.tt').textContent = q.title;
      card.querySelector('.dd').textContent = q.detail;
      const li = add('ask', card);
      li.querySelector('.ok').focus({ preventScroll: true });
      const check = setInterval(() => { if (my !== token) { clearInterval(check); rej(STOP); } }, 200);
      li.addEventListener('click', (e) => {
        const b = e.target.closest('button[data-a]');
        if (!b) return;
        clearInterval(check);
        li.remove();
        res(b.dataset.a);
      });
    });

    prompts?.addEventListener('click', (e) => {
      const chip = e.target.closest('.chip');
      if (!chip) return;
      auto = false;
      play(chip.dataset.demo);
    });
    // The first request plays by itself once the window is in view.
    let visible = false, started = false;
    if ('IntersectionObserver' in window) {
      new IntersectionObserver(([e]) => {
        visible = e.isIntersecting;
        if (visible && !started) { started = true; setTimeout(() => { if (auto) play(AUTOPLAY[0]); }, 700); }
      }, { threshold: 0.45 }).observe(canvas.closest('.win'));
    }
  };

  // ---- waking up: the coil becomes the ring as you scroll ---------------
  const wake = () => {
    const sec = $('#wake'), stage = $('#wake-stage'), canvas = $('#coil');
    const one = $('#wake-1'), two = $('#wake-2'), reply = $('#reply'), spot = $('#wake-spot');
    if (!sec || !canvas) return;
    const s = surface(canvas, () => { if (still) draw(0); });
    const fig = { state: 'listening', v: 0, voice: 0, mic: 0 };
    let p = 0, said = false, until = 0, typing = 0;
    const line = 'Hi, I’m listening.';
    scrollFx.push({
      read: () => sec.getBoundingClientRect(),
      write: (r, vh) => {
        p = still ? 1 : clamp(-r.top / Math.max(1, r.height - vh));
        if (still) return;
        one.style.opacity = String(smooth(0.0, 0.07, p) * (1 - smooth(0.42, 0.54, p)));
        one.style.transform = `translateY(${-24 * smooth(0.42, 0.54, p)}px)`;
        two.style.opacity = String(smooth(0.86, 0.94, p));
        two.style.transform = `translateY(${18 * (1 - smooth(0.86, 0.94, p))}px)`;
        stage.style.setProperty('--warm', String(0.35 + 0.65 * smooth(0.1, 0.8, p)));
      },
    });
    const draw = (now) => {
      const t = now / 1000;
      fig.v = still ? 1 : smooth(0.08, 0.86, p);
      if (fig.v >= 1 && p > 0.9 && !said) {
        said = true;
        until = now + 2400;
        let n = 0;
        clearInterval(typing);
        typing = setInterval(() => { n++; reply.textContent = line.slice(0, n); if (n >= line.length) clearInterval(typing); }, 45);
      }
      if (p < 0.75 && said) { said = false; clearInterval(typing); reply.textContent = ''; }
      fig.state = now < until ? 'speaking' : 'listening';
      fig.voice = fig.state === 'speaking' ? voiceAt(t) : 0;
      s.clear();
      // The ring rises a little as its words come in below it; at rest
      // (reduced motion) it has a place of its own between them.
      const lift = still ? 0 : smooth(0.84, 0.94, p);
      const base = Math.min(s.w, s.h) * (s.w < 700 ? 0.18 : 0.15) * (1 - 0.2 * lift);
      const cy = still && spot ? spot.offsetTop + spot.offsetHeight / 2 : s.h * (0.5 - 0.12 * lift);
      presence(s.ctx, s.w / 2, cy, still ? Math.min(base, spot ? spot.offsetHeight * 0.36 : base) : base, fig, t);
    };
    if (still) { reply.textContent = line; draw(0); return; }
    onScreen(stage, draw, '0px');
  };

  // ---- the consent card answers ----------------------------------------
  const approval = () => {
    const card = $('#approval'), result = $('#approval-result');
    if (!card) return;
    card.addEventListener('click', (e) => {
      if (e.target.closest('.again')) {
        card.classList.remove('answered', 'denied');
        card.querySelector('.ok').focus();
        return;
      }
      const b = e.target.closest('button[data-answer]');
      if (!b) return;
      const allow = b.dataset.answer === 'allow';
      card.classList.add('answered');
      card.classList.toggle('denied', !allow);
      result.textContent = allow ? 'Moved to the Trash — and it can come back from there. ' : 'Nothing was changed. ';
      const again = document.createElement('button');
      again.type = 'button';
      again.className = 'again';
      again.textContent = 'Ask again';
      result.append(again);
    });
  };

  // ---- small things -----------------------------------------------------
  const extras = () => {
    if (matchMedia('(hover: hover)').matches) {
      for (const a of $$('.app')) {
        a.addEventListener('pointermove', (e) => {
          const r = a.getBoundingClientRect();
          a.style.setProperty('--mx', `${e.clientX - r.left}px`);
          a.style.setProperty('--my', `${e.clientY - r.top}px`);
        });
      }
    }
    const shots = $('#shots');
    const by = (dir) => {
      const one = shots.querySelector('.shot');
      shots.scrollBy({ left: dir * ((one ? one.getBoundingClientRect().width : 600) + 18), behavior: still ? 'auto' : 'smooth' });
    };
    $('#prev')?.addEventListener('click', () => by(-1));
    $('#next')?.addEventListener('click', () => by(1));
    const copy = $('#copy');
    copy?.addEventListener('click', async () => {
      const label = copy.querySelector('span');
      try {
        await navigator.clipboard.writeText($('#cmd').textContent);
        label.textContent = 'Copied';
      } catch (_) {
        const range = document.createRange();
        range.selectNodeContents($('#cmd'));
        getSelection().removeAllRanges();
        getSelection().addRange(range);
        label.textContent = 'Selected';
      }
      setTimeout(() => { label.textContent = 'Copy'; }, 1800);
    });
    const year = $('#year');
    if (year) year.textContent = String(new Date().getFullYear());
  };

  sky();
  statement();
  screen();
  reveals();
  demo();
  wake();
  approval();
  extras();
  flush();
})();
