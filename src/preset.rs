//! Preset model, shared by built-in presets, Lightroom files and LUTs.

use std::path::PathBuf;

/// Lightroom's HSL / B&W mixer bands, in slider order.
pub const BANDS: [&str; 8] = ["Red", "Orange", "Yellow", "Green", "Aqua", "Blue", "Purple", "Magenta"];

/// Tone curve points in Lightroom's 0..255 space.
pub type Curve = Vec<[f32; 2]>;

/// Slider values, using Lightroom's units (mostly -100..100, exposure in EV).
#[derive(Clone, Debug, Default)]
pub struct Adjustments {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub clarity: f32,
    pub dehaze: f32,
    pub temperature: f32,
    pub tint: f32,
    pub vibrance: f32,
    pub saturation: f32,
    /// Hue, saturation, luminance per band.
    pub hsl: [[f32; 3]; 8],
    pub grayscale: bool,
    pub gray_mix: [f32; 8],
    /// Hue in degrees, saturation 0..100.
    pub shadow_tint: [f32; 2],
    pub highlight_tint: [f32; 2],
    pub midtone_tint: [f32; 2],
    pub tint_balance: f32,
    pub vignette: f32,
    pub grain: f32,
    pub curve: Curve,
    pub curve_rgb: [Curve; 3],
    /// Parametric curve: shadows, darks, lights, highlights (-100..100).
    pub parametric: [f32; 4],
}

/// A 3D colour lookup table, red index varying fastest.
#[derive(Clone, Debug)]
pub struct Lut3d {
    pub size: u32,
    pub data: Vec<[f32; 3]>,
}

#[derive(Clone, Debug)]
pub struct Preset {
    pub name: String,
    pub group: String,
    pub adj: Adjustments,
    pub lut: Option<Lut3d>,
    pub source: Option<PathBuf>,
}

impl Preset {
    pub fn builtin(name: &str, group: &str, adj: Adjustments) -> Self {
        Self { name: name.into(), group: group.into(), adj, lut: None, source: None }
    }

    /// Packs the sliders into the shader's `Params` layout (see shader.wgsl).
    pub fn uniforms(&self) -> [[f32; 4]; 16] {
        let a = &self.adj;
        let c = |v: f32| v / 100.0;
        let mut u = [[0.0; 4]; 16];
        u[0] = [a.exposure, c(a.contrast), c(a.highlights), c(a.shadows)];
        u[1] = [c(a.whites), c(a.blacks), c(a.temperature), c(a.tint)];
        u[2] = [c(a.vibrance), c(a.saturation), c(a.clarity), c(a.dehaze)];
        u[3] = [
            c(a.vignette),
            c(a.grain),
            if a.grayscale { 1.0 } else { 0.0 },
            if self.lut.is_some() { 1.0 } else { 0.0 },
        ];
        u[4] = [a.shadow_tint[0], c(a.shadow_tint[1]), a.highlight_tint[0], c(a.highlight_tint[1])];
        u[5] = [c(a.tint_balance), a.midtone_tint[0], c(a.midtone_tint[1]), 0.0];
        for (i, [h, s, l]) in a.hsl.iter().enumerate() {
            u[6 + i] = [c(*h), c(*s), c(*l), 0.0];
        }
        for (i, g) in a.gray_mix.iter().enumerate() {
            u[14 + i / 4][i % 4] = c(*g);
        }
        u
    }

    /// 256-entry curve per channel (0..1), with the parametric and master curves
    /// composed into each channel.
    pub fn curve_table(&self) -> Vec<[f32; 3]> {
        let a = &self.adj;
        let master = Spline::new(&a.curve);
        let chans: Vec<Spline> = a.curve_rgb.iter().map(|c| Spline::new(c)).collect();
        (0..256)
            .map(|i| {
                let x = i as f32 / 255.0;
                let m = master.eval(parametric(x, &a.parametric));
                [chans[0].eval(m), chans[1].eval(m), chans[2].eval(m)]
            })
            .collect()
    }
}

/// Lightroom's parametric curve, approximated with one smooth bump per region.
fn parametric(x: f32, p: &[f32; 4]) -> f32 {
    let bump = |center: f32| {
        let d = (x - center) / 0.25;
        (1.0 - d * d).max(0.0).powi(2)
    };
    let centers = [0.125, 0.375, 0.625, 0.875];
    let mut y = x;
    for (v, c) in p.iter().zip(centers) {
        y += v / 100.0 * 0.2 * bump(c);
    }
    y.clamp(0.0, 1.0)
}

/// Monotone cubic (Fritsch–Carlson) interpolation through curve points.
struct Spline {
    xs: Vec<f32>,
    ys: Vec<f32>,
    ms: Vec<f32>,
}

impl Spline {
    fn new(points: &[[f32; 2]]) -> Self {
        let mut pts: Vec<[f32; 2]> = points.iter().map(|p| [p[0] / 255.0, p[1] / 255.0]).collect();
        pts.sort_by(|a, b| a[0].total_cmp(&b[0]));
        pts.dedup_by(|a, b| (a[0] - b[0]).abs() < 1e-6);
        if pts.len() < 2 {
            pts = vec![[0.0, 0.0], [1.0, 1.0]];
        }
        let xs: Vec<f32> = pts.iter().map(|p| p[0]).collect();
        let ys: Vec<f32> = pts.iter().map(|p| p[1]).collect();
        let n = xs.len();
        let d: Vec<f32> = (0..n - 1).map(|i| (ys[i + 1] - ys[i]) / (xs[i + 1] - xs[i])).collect();
        let mut ms = vec![0.0; n];
        ms[0] = d[0];
        ms[n - 1] = d[n - 2];
        for i in 1..n - 1 {
            ms[i] = if d[i - 1] * d[i] <= 0.0 { 0.0 } else { (d[i - 1] + d[i]) / 2.0 };
        }
        for i in 0..n - 1 {
            if d[i] == 0.0 {
                ms[i] = 0.0;
                ms[i + 1] = 0.0;
                continue;
            }
            let (a, b) = (ms[i] / d[i], ms[i + 1] / d[i]);
            let s = a * a + b * b;
            if s > 9.0 {
                let t = 3.0 / s.sqrt();
                ms[i] = t * a * d[i];
                ms[i + 1] = t * b * d[i];
            }
        }
        Self { xs, ys, ms }
    }

    fn eval(&self, x: f32) -> f32 {
        let n = self.xs.len();
        if x <= self.xs[0] {
            return self.ys[0];
        }
        if x >= self.xs[n - 1] {
            return self.ys[n - 1];
        }
        let i = self.xs.partition_point(|&v| v <= x) - 1;
        let h = self.xs[i + 1] - self.xs[i];
        let t = (x - self.xs[i]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let y = (2.0 * t3 - 3.0 * t2 + 1.0) * self.ys[i]
            + (t3 - 2.0 * t2 + t) * h * self.ms[i]
            + (-2.0 * t3 + 3.0 * t2) * self.ys[i + 1]
            + (t3 - t2) * h * self.ms[i + 1];
        y.clamp(0.0, 1.0)
    }
}

/// HSL band indices, for readable built-in definitions.
const RED: usize = 0;
const ORANGE: usize = 1;
const YELLOW: usize = 2;
const GREEN: usize = 3;
const AQUA: usize = 4;
const BLUE: usize = 5;

fn hsl(entries: &[(usize, [f32; 3])]) -> [[f32; 3]; 8] {
    let mut out = [[0.0; 3]; 8];
    for (band, v) in entries {
        out[*band] = *v;
    }
    out
}

fn curve(points: &[(f32, f32)]) -> Curve {
    points.iter().map(|&(x, y)| [x, y]).collect()
}

pub fn builtins() -> Vec<Preset> {
    use Adjustments as A;
    let d = A::default;
    let p = Preset::builtin;
    let bw = |mix: [f32; 8], rest: A| A { grayscale: true, gray_mix: mix, ..rest };

    vec![
        p("Original", "Basic", d()),
        p("Auto Tone", "Basic", A { contrast: 12.0, highlights: -25.0, shadows: 25.0, whites: 10.0, blacks: -8.0, vibrance: 12.0, ..d() }),
        p("Punchy", "Basic", A { contrast: 30.0, clarity: 20.0, vibrance: 25.0, saturation: 5.0, blacks: -10.0, ..d() }),
        p("Soft & Airy", "Basic", A { exposure: 0.3, contrast: -20.0, highlights: -30.0, shadows: 40.0, saturation: -10.0, clarity: -10.0, curve: curve(&[(0.0, 20.0), (128.0, 136.0), (255.0, 250.0)]), ..d() }),
        p("Clarity Pop", "Basic", A { clarity: 45.0, dehaze: 10.0, vibrance: 15.0, ..d() }),
        p("HDR Look", "Basic", A { highlights: -80.0, shadows: 70.0, clarity: 40.0, vibrance: 20.0, whites: 15.0, blacks: -15.0, ..d() }),
        p("Warm Sunset", "Colour", A { temperature: 35.0, tint: 8.0, vibrance: 20.0, highlight_tint: [40.0, 25.0], shadow_tint: [15.0, 10.0], ..d() }),
        p("Cool Morning", "Colour", A { temperature: -30.0, exposure: 0.15, contrast: -5.0, shadow_tint: [210.0, 15.0], saturation: -5.0, ..d() }),
        p("Teal & Orange", "Colour", A {
            contrast: 20.0,
            shadow_tint: [195.0, 35.0],
            highlight_tint: [35.0, 30.0],
            hsl: hsl(&[(ORANGE, [0.0, 15.0, 5.0]), (AQUA, [15.0, 10.0, 0.0]), (BLUE, [-15.0, 10.0, -10.0]), (GREEN, [40.0, -30.0, 0.0])]),
            ..d()
        }),
        p("Golden Hour", "Colour", A { temperature: 25.0, highlights: -20.0, shadows: 15.0, vibrance: 15.0, hsl: hsl(&[(ORANGE, [5.0, 10.0, 10.0]), (YELLOW, [-10.0, 15.0, 0.0])]), highlight_tint: [45.0, 20.0], ..d() }),
        p("Lush Greens", "Colour", A { vibrance: 15.0, hsl: hsl(&[(GREEN, [15.0, 25.0, -10.0]), (YELLOW, [15.0, 10.0, 0.0]), (AQUA, [0.0, 10.0, 0.0])]), ..d() }),
        p("Deep Blue Sky", "Colour", A { hsl: hsl(&[(BLUE, [-5.0, 25.0, -25.0]), (AQUA, [0.0, 15.0, -10.0])]), dehaze: 10.0, ..d() }),
        p("Portrait Glow", "Colour", A { exposure: 0.1, clarity: -15.0, highlights: -15.0, shadows: 15.0, hsl: hsl(&[(ORANGE, [0.0, -8.0, 15.0]), (RED, [0.0, -5.0, 5.0])]), highlight_tint: [35.0, 10.0], ..d() }),
        p("Matte", "Film", A { contrast: -10.0, curve: curve(&[(0.0, 35.0), (70.0, 72.0), (190.0, 195.0), (255.0, 240.0)]), saturation: -10.0, ..d() }),
        p("Faded Film", "Film", A { contrast: -15.0, curve: curve(&[(0.0, 40.0), (128.0, 130.0), (255.0, 230.0)]), saturation: -20.0, shadow_tint: [200.0, 12.0], highlight_tint: [45.0, 12.0], grain: 25.0, ..d() }),
        p("Vintage", "Film", A {
            temperature: 15.0,
            contrast: 10.0,
            saturation: -25.0,
            curve: curve(&[(0.0, 30.0), (128.0, 128.0), (255.0, 235.0)]),
            curve_rgb: [curve(&[(0.0, 10.0), (255.0, 255.0)]), curve(&[(0.0, 0.0), (255.0, 250.0)]), curve(&[(0.0, 25.0), (255.0, 220.0)])],
            vignette: -25.0,
            grain: 30.0,
            ..d()
        }),
        p("Kodachrome-ish", "Film", A { contrast: 25.0, saturation: 10.0, hsl: hsl(&[(RED, [5.0, 15.0, -5.0]), (BLUE, [-10.0, 10.0, -15.0]), (YELLOW, [-5.0, 10.0, 0.0])]), temperature: 8.0, grain: 12.0, ..d() }),
        p("Portra-ish", "Film", A { contrast: -5.0, temperature: 10.0, saturation: -10.0, highlights: -20.0, shadows: 10.0, hsl: hsl(&[(ORANGE, [0.0, 5.0, 10.0]), (GREEN, [25.0, -20.0, 0.0]), (BLUE, [-5.0, -10.0, 0.0])]), curve: curve(&[(0.0, 15.0), (255.0, 250.0)]), grain: 15.0, ..d() }),
        p("Cross Process", "Film", A {
            contrast: 20.0,
            curve_rgb: [curve(&[(0.0, 0.0), (64.0, 50.0), (192.0, 210.0), (255.0, 255.0)]), curve(&[(0.0, 0.0), (64.0, 70.0), (192.0, 205.0), (255.0, 255.0)]), curve(&[(0.0, 40.0), (255.0, 200.0)])],
            ..d()
        }),
        p("Cinematic", "Moody", A {
            contrast: 15.0,
            highlights: -30.0,
            blacks: -10.0,
            saturation: -15.0,
            shadow_tint: [190.0, 30.0],
            highlight_tint: [40.0, 20.0],
            curve: curve(&[(0.0, 15.0), (64.0, 55.0), (192.0, 200.0), (255.0, 245.0)]),
            vignette: -20.0,
            ..d()
        }),
        p("Moody Dark", "Moody", A { exposure: -0.3, contrast: 20.0, highlights: -40.0, shadows: -10.0, saturation: -25.0, clarity: 15.0, vignette: -35.0, shadow_tint: [210.0, 12.0], ..d() }),
        p("Dramatic", "Moody", A { contrast: 40.0, clarity: 40.0, dehaze: 15.0, highlights: -50.0, shadows: 20.0, saturation: -20.0, vignette: -30.0, ..d() }),
        p("Pastel", "Moody", A { exposure: 0.25, contrast: -30.0, saturation: -20.0, vibrance: 10.0, curve: curve(&[(0.0, 45.0), (255.0, 245.0)]), highlight_tint: [320.0, 10.0], shadow_tint: [190.0, 10.0], ..d() }),
        p("Classic B&W", "Black & White", bw([0.0; 8], A { contrast: 15.0, ..d() })),
        p("High Contrast B&W", "Black & White", bw([10.0, 15.0, 10.0, -20.0, -20.0, -30.0, 0.0, 0.0], A { contrast: 45.0, clarity: 25.0, blacks: -20.0, whites: 15.0, ..d() })),
        p("Soft B&W", "Black & White", bw([15.0, 20.0, 10.0, 0.0, 0.0, -10.0, 0.0, 0.0], A { contrast: -15.0, highlights: -20.0, shadows: 25.0, curve: curve(&[(0.0, 25.0), (255.0, 245.0)]), ..d() })),
        p("Noir", "Black & White", bw([-10.0, 0.0, 0.0, -30.0, -30.0, -40.0, 0.0, 0.0], A { contrast: 55.0, exposure: -0.2, blacks: -30.0, vignette: -45.0, grain: 35.0, ..d() })),
        p("Sepia", "Black & White", bw([10.0, 10.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0], A { contrast: 5.0, shadow_tint: [35.0, 35.0], highlight_tint: [45.0, 25.0], ..d() })),
        p("Selenium", "Black & White", bw([0.0; 8], A { contrast: 20.0, shadow_tint: [250.0, 20.0], highlight_tint: [40.0, 8.0], ..d() })),
    ]
}
