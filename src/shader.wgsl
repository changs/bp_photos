// Preset rendering: one full-screen triangle per output image.
//
// The source texture holds linear-light RGB (sRGB-encoded 8-bit textures are
// decoded by the sampler, RAW files are uploaded as linear f16). Most
// adjustments run in sRGB-encoded "perceptual" space, like Lightroom's sliders.

struct Params {
    // v0: exposure (EV), contrast, highlights, shadows
    // v1: whites, blacks, temperature, tint
    // v2: vibrance, saturation, clarity, dehaze
    // v3: vignette, grain, grayscale (0/1), lut amount
    // v4: shadow tint hue (deg), shadow tint sat, highlight tint hue, highlight tint sat
    // v5: tint balance, midtone tint hue, midtone tint sat, unused
    // v6..v13: HSL (hue, sat, lum, _) for red, orange, yellow, green, aqua, blue, purple, magenta
    // v14..v15: B&W mix for the same 8 bands
    v: array<vec4<f32>, 16>,
}

struct Target {
    // x: strength (0 = original, 1 = full preset)
    v: vec4<f32>,
    // Crop in source UVs: x, y, width, height.
    crop: vec4<f32>,
}

@group(0) @binding(0) var src_tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
// Source colour space → linear sRGB (identity, or Display P3 → sRGB). Rows.
@group(0) @binding(2) var<uniform> gamut: array<vec4<f32>, 3>;
@group(1) @binding(0) var<uniform> P: Params;
@group(1) @binding(1) var curve_tex: texture_2d<f32>;
@group(1) @binding(2) var lut_tex: texture_3d<f32>;
@group(2) @binding(0) var<uniform> T: Target;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: VsOut;
    o.pos = vec4<f32>(uv * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    o.uv = uv;
    return o;
}

const LUMA = vec3<f32>(0.2126, 0.7152, 0.0722);

fn to_srgb_gamut(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(dot(gamut[0].xyz, c), dot(gamut[1].xyz, c), dot(gamut[2].xyz, c));
}

fn to_srgb(c: vec3<f32>) -> vec3<f32> {
    let x = max(c, vec3<f32>(0.0));
    let hi = 1.055 * pow(x, vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, x * 12.92, x <= vec3<f32>(0.0031308));
}

fn to_linear(c: vec3<f32>) -> vec3<f32> {
    let x = max(c, vec3<f32>(0.0));
    let hi = pow((x + 0.055) / 1.055, vec3<f32>(2.4));
    return select(hi, x / 12.92, x <= vec3<f32>(0.04045));
}

fn rgb2hsv(c: vec3<f32>) -> vec3<f32> {
    let mx = max(c.r, max(c.g, c.b));
    let mn = min(c.r, min(c.g, c.b));
    let d = mx - mn;
    var h = 0.0;
    if d > 1e-5 {
        if mx == c.r {
            h = (c.g - c.b) / d;
        } else if mx == c.g {
            h = (c.b - c.r) / d + 2.0;
        } else {
            h = (c.r - c.g) / d + 4.0;
        }
        h = fract(h / 6.0 + 1.0);
    }
    let s = select(0.0, d / mx, mx > 1e-5);
    return vec3<f32>(h, s, mx);
}

fn hsv2rgb(c: vec3<f32>) -> vec3<f32> {
    let k = vec3<f32>(1.0, 2.0 / 3.0, 1.0 / 3.0);
    let p = abs(fract(vec3<f32>(c.x) + k) * 6.0 - 3.0);
    return c.z * mix(vec3<f32>(1.0), clamp(p - 1.0, vec3<f32>(0.0), vec3<f32>(1.0)), c.y);
}

// Symmetric S-curve around 0.5; k > 1 adds contrast, k < 1 removes it.
fn sigmoid(x: f32, k: f32) -> f32 {
    let a = pow(x, k);
    let b = pow(1.0 - x, k);
    return a / max(a + b, 1e-6);
}

// Interpolates a per-band vec4 (from uniform slot `base`) at the given hue.
fn band_adjust(hue01: f32, base: u32) -> vec4<f32> {
    var centers = array<f32, 9>(0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0, 360.0);
    let h = hue01 * 360.0;
    var i = 0u;
    for (var j = 0u; j < 8u; j++) {
        if h >= centers[j] {
            i = j;
        }
    }
    let t = clamp((h - centers[i]) / (centers[i + 1u] - centers[i]), 0.0, 1.0);
    return mix(P.v[base + i], P.v[base + (i + 1u) % 8u], t);
}

fn gray_mix(hue01: f32) -> f32 {
    var centers = array<f32, 9>(0.0, 30.0, 60.0, 120.0, 180.0, 240.0, 270.0, 300.0, 360.0);
    var vals = array<f32, 9>(
        P.v[14].x, P.v[14].y, P.v[14].z, P.v[14].w,
        P.v[15].x, P.v[15].y, P.v[15].z, P.v[15].w, P.v[14].x,
    );
    let h = hue01 * 360.0;
    var i = 0u;
    for (var j = 0u; j < 8u; j++) {
        if h >= centers[j] {
            i = j;
        }
    }
    let t = clamp((h - centers[i]) / (centers[i + 1u] - centers[i]), 0.0, 1.0);
    return mix(vals[i], vals[i + 1u], t);
}

fn tint(hue_deg: f32, sat: f32, w: f32) -> vec3<f32> {
    let c = hsv2rgb(vec3<f32>(hue_deg / 360.0, 1.0, 1.0));
    return (c - vec3<f32>(dot(c, LUMA))) * sat * w * 0.35;
}

fn pcg(v: u32) -> u32 {
    let s = v * 747796405u + 2891336453u;
    let w = ((s >> ((s >> 28u) + 4u)) ^ s) * 277803737u;
    return (w >> 22u) ^ w;
}

fn noise(p: vec2<f32>) -> f32 {
    let q = vec2<u32>(vec2<i32>(floor(p)) + vec2<i32>(65536));
    return f32(pcg(q.x + pcg(q.y))) / 4294967295.0 - 0.5;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    let v0 = P.v[0];
    let v1 = P.v[1];
    let v2 = P.v[2];
    let v3 = P.v[3];
    let v4 = P.v[4];
    let v5 = P.v[5];

    // `in.uv` spans the output frame; `uv` is the matching point in the (cropped) source.
    let uv = T.crop.xy + in.uv * T.crop.zw;
    // Implicit-LOD sampling so smaller outputs (preview, thumbnails) read the mip chain.
    let src = to_srgb_gamut(textureSample(src_tex, samp, uv).rgb);
    let orig = to_srgb(src);

    // White balance + exposure, in linear light.
    var wb = vec3<f32>(1.0 + 0.25 * v1.z, 1.0 - 0.2 * v1.w, 1.0 - 0.25 * v1.z);
    wb = wb / dot(wb, LUMA);
    let gain = wb * exp2(v0.x);
    var p = to_srgb(src * gain);

    // Clarity: local contrast against a blurred copy read from the mip chain.
    if abs(v2.z) > 0.001 {
        let dims = vec2<f32>(textureDimensions(src_tex, 0));
        let lvl = max(log2(max(dims.x, dims.y) / 160.0), 0.0);
        let o = 1.5 * exp2(lvl) / dims;
        var b = textureSampleLevel(src_tex, samp, uv, lvl).rgb * 0.4;
        b += textureSampleLevel(src_tex, samp, uv + vec2<f32>(o.x, 0.0), lvl).rgb * 0.15;
        b += textureSampleLevel(src_tex, samp, uv - vec2<f32>(o.x, 0.0), lvl).rgb * 0.15;
        b += textureSampleLevel(src_tex, samp, uv + vec2<f32>(0.0, o.y), lvl).rgb * 0.15;
        b += textureSampleLevel(src_tex, samp, uv - vec2<f32>(0.0, o.y), lvl).rgb * 0.15;
        let bl = dot(to_srgb(to_srgb_gamut(b) * gain), LUMA);
        let l = dot(p, LUMA);
        let mid = 1.0 - pow(clamp(abs(2.0 * l - 1.0), 0.0, 1.0), 2.0);
        p += vec3<f32>((l - bl) * v2.z * 1.5 * mid);
    }

    // Dehaze: lower the haze floor and restore saturation.
    if abs(v2.w) > 0.001 {
        let floor_ = v2.w * 0.08;
        p = (p - vec3<f32>(floor_)) / (1.0 - floor_);
        let l = dot(p, LUMA);
        p = mix(vec3<f32>(l), p, 1.0 + v2.w * 0.25);
    }

    // Highlights / shadows / whites / blacks, applied to luminance.
    {
        let l = max(dot(p, LUMA), 0.0);
        let lc = clamp(l, 0.0, 1.0);
        var nl = l;
        if v0.z < 0.0 {
            // Highlight recovery: compress everything above the midpoint.
            if nl > 0.5 {
                nl = 0.5 + (nl - 0.5) / (1.0 - v0.z * 1.5 * (nl - 0.5));
            }
        } else {
            nl += v0.z * 0.25 * 6.75 * lc * lc * (1.0 - lc);
        }
        nl += v0.w * 0.3 * 6.75 * lc * (1.0 - lc) * (1.0 - lc);
        nl *= 1.0 + v1.x * 0.3 * lc * lc;
        // Scale to keep saturation; near black, add instead to avoid division blow-ups.
        p = select(p * (nl / max(l, 1e-4)), p + vec3<f32>(nl - l), l < 0.02);
        p += vec3<f32>(v1.y * 0.12 * pow(1.0 - lc, 4.0));
    }
    p = clamp(p, vec3<f32>(0.0), vec3<f32>(1.0));

    // Contrast.
    if abs(v0.y) > 0.001 {
        let k = exp2(v0.y * 1.2);
        p = vec3<f32>(sigmoid(p.r, k), sigmoid(p.g, k), sigmoid(p.b, k));
    }

    // Tone curve (RGB master already composed into each channel on the CPU).
    let cu = (p * 255.0 + 0.5) / 256.0;
    p = vec3<f32>(
        textureSampleLevel(curve_tex, samp, vec2<f32>(cu.r, 0.5), 0.0).r,
        textureSampleLevel(curve_tex, samp, vec2<f32>(cu.g, 0.5), 0.0).g,
        textureSampleLevel(curve_tex, samp, vec2<f32>(cu.b, 0.5), 0.0).b,
    );

    if v3.z > 0.5 {
        // Black & white with a per-hue mix.
        let hsv = rgb2hsv(p);
        var l = dot(p, LUMA);
        l *= 1.0 + gray_mix(hsv.x) * 0.8 * hsv.y;
        p = vec3<f32>(l);
    } else {
        // HSL per colour band.
        var hsv = rgb2hsv(p);
        let adj = band_adjust(hsv.x, 6u);
        let w = smoothstep(0.0, 0.15, hsv.y);
        hsv.x = fract(hsv.x + adj.x * (30.0 / 360.0) * w + 1.0);
        hsv.y = clamp(hsv.y * (1.0 + adj.y), 0.0, 1.0);
        hsv.z = hsv.z * (1.0 + adj.z * 0.5 * hsv.y);
        p = hsv2rgb(hsv);

        // Vibrance (favours muted colours) and saturation.
        let l = dot(p, LUMA);
        let s = max(p.r, max(p.g, p.b)) - min(p.r, min(p.g, p.b));
        let amount = 1.0 + v2.y + v2.x * (1.0 - clamp(s, 0.0, 1.0));
        p = mix(vec3<f32>(l), p, max(amount, 0.0));
    }

    // Colour grading / split toning.
    {
        let l = clamp(dot(p, LUMA), 0.0, 1.0);
        let pivot = clamp(0.5 - v5.x * 0.35, 0.1, 0.9);
        let wsh = 1.0 - smoothstep(0.0, pivot * 1.6, l);
        let whi = smoothstep(1.0 - (1.0 - pivot) * 1.6, 1.0, l);
        let wmid = max(1.0 - abs(l - 0.5) * 2.0, 0.0);
        p += tint(v4.x, v4.y, wsh) + tint(v4.z, v4.w, whi) + tint(v5.y, v5.z, wmid);
    }
    p = clamp(p, vec3<f32>(0.0), vec3<f32>(1.0));

    // 3D LUT (.cube), applied to display-referred values.
    if v3.w > 0.001 {
        let n = f32(textureDimensions(lut_tex).x);
        let c = p * ((n - 1.0) / n) + vec3<f32>(0.5 / n);
        p = mix(p, textureSampleLevel(lut_tex, samp, c, 0.0).rgb, v3.w);
    }

    // Vignette, relative to the cropped frame (like Lightroom's post-crop vignette).
    if abs(v3.x) > 0.001 {
        let r = length((in.uv - 0.5) * 2.0) / 1.41421356;
        let w = smoothstep(0.3, 1.0, r);
        if v3.x < 0.0 {
            p *= 1.0 + v3.x * w * 0.85;
        } else {
            p = mix(p, vec3<f32>(1.0), v3.x * w * 0.85);
        }
    }

    // Grain, in output pixels.
    if v3.y > 0.001 {
        let n = noise(in.pos.xy / 1.3) * 0.7 + noise(in.pos.xy / 3.1 + 17.0) * 0.3;
        let l = dot(p, LUMA);
        p += vec3<f32>(n * v3.y * 0.18 * (1.0 - abs(2.0 * l - 1.0) * 0.6));
    }

    let outp = mix(orig, clamp(p, vec3<f32>(0.0), vec3<f32>(1.0)), T.v.x);
    return vec4<f32>(to_linear(clamp(outp, vec3<f32>(0.0), vec3<f32>(1.0))), 1.0);
}
