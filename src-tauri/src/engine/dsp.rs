//! Bloques de DSP: interpolación sinc, filtros SVF (TPT), ecualizador aislador LR4,
//! filtro de canal, suavizado de parámetros y limitador con anticipación.

use std::f64::consts::PI;
use std::sync::OnceLock;

// ---------------------------------------------------------------------------
// Interpolación sinc con ventana Kaiser
// ---------------------------------------------------------------------------

/// Medio ancho del núcleo en muestras de entrada (80 coeficientes a velocidad 1).
pub const SINC_HALF: usize = 40;
/// Resolución de la tabla (posiciones por muestra).
const SINC_OS: usize = 512;
/// Frecuencia de corte normalizada (1.0 = Nyquist). Banda pasante plana hasta ~19 kHz a 44.1 kHz.
const SINC_CUTOFF: f64 = 0.92;
const KAISER_BETA: f64 = 9.0;

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k as f64 * k as f64);
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

fn sinc_table() -> &'static [f32] {
    static TABLE: OnceLock<Vec<f32>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let n = SINC_HALF * SINC_OS;
        let i0b = bessel_i0(KAISER_BETA);
        (0..=n + 1)
            .map(|i| {
                let t = i as f64 / SINC_OS as f64;
                if t >= SINC_HALF as f64 {
                    return 0.0;
                }
                let x = PI * SINC_CUTOFF * t;
                let s = if t == 0.0 { 1.0 } else { x.sin() / x };
                let r = t / SINC_HALF as f64;
                let w = bessel_i0(KAISER_BETA * (1.0 - r * r).max(0.0).sqrt()) / i0b;
                (SINC_CUTOFF * s * w) as f32
            })
            .collect()
    })
}

/// Núcleo continuo evaluado en |t| (muestras), con interpolación lineal en la tabla.
#[inline(always)]
fn kernel(table: &[f32], t: f64) -> f32 {
    let x = t.abs() * SINC_OS as f64;
    let i = x as usize;
    if i >= SINC_HALF * SINC_OS {
        return 0.0;
    }
    let f = (x - i as f64) as f32;
    let a = table[i];
    a + (table[i + 1] - a) * f
}

/// Lector de alta calidad para leer una señal en posiciones fraccionarias.
/// `ratio` es el avance en muestras de entrada por muestra de salida; si es > 1 el
/// núcleo se ensancha para filtrar el aliasing.
pub struct SincReader {
    table: &'static [f32],
}

impl Default for SincReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SincReader {
    pub fn new() -> Self {
        Self { table: sinc_table() }
    }

    /// Lee dos canales a la vez (comparten pesos).
    #[inline]
    pub fn read2(&self, l: &[f32], r: &[f32], pos: f64, ratio: f64) -> (f32, f32) {
        let n = l.len() as i64;
        let ip = pos.floor();
        let frac = pos - ip;
        let ip = ip as i64;
        // Lectura exacta: sin interpolación cuando cae en una muestra entera a velocidad 1
        if frac == 0.0 && ratio <= 1.0 {
            if ip >= 0 && ip < n {
                return (l[ip as usize], r[ip as usize]);
            }
            return (0.0, 0.0);
        }
        let scale = if ratio > 1.0 { 1.0 / ratio } else { 1.0 };
        let reach = (SINC_HALF as f64 / scale).ceil() as i64;
        let lo = (ip - reach + 1).max(0);
        let hi = (ip + reach).min(n - 1);
        if lo > hi {
            return (0.0, 0.0);
        }
        let mut sl = 0.0f32;
        let mut sr = 0.0f32;
        let mut wsum = 0.0f32;
        let mut j = lo;
        while j <= hi {
            let w = kernel(self.table, (j as f64 - pos) * scale);
            sl += w * l[j as usize];
            sr += w * r[j as usize];
            wsum += w;
            j += 1;
        }
        // Normaliza la ganancia de continua solo en el interior; en los bordes deja caer a cero
        if lo > ip - reach + 1 || hi < ip + reach {
            let full = scale as f32;
            return (sl * full, sr * full);
        }
        let g = if wsum.abs() > 1e-6 { 1.0 / wsum } else { 0.0 };
        (sl * g, sr * g)
    }
}

/// Remuestreo offline de alta calidad (pistas a la frecuencia del motor).
pub fn resample(input: &[f32], from_sr: u32, to_sr: u32) -> Vec<f32> {
    if from_sr == to_sr || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_sr as f64 / to_sr as f64;
    let out_len = ((input.len() as f64) / ratio).floor() as usize;
    let reader = SincReader::new();
    let mut out = Vec::with_capacity(out_len);
    for k in 0..out_len {
        let (v, _) = reader.read2(input, input, k as f64 * ratio, ratio.max(1.0000001));
        out.push(v);
    }
    out
}

// ---------------------------------------------------------------------------
// Filtro de estado variable (Cytomic / Zavalishin TPT): estable al modular
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Default)]
pub struct SvfCoef {
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
}

impl SvfCoef {
    pub fn new(freq: f64, q: f64, sr: f64) -> Self {
        let f = freq.clamp(5.0, sr * 0.49);
        let g = (PI * f / sr).tan();
        let k = 1.0 / q;
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        Self { k: k as f32, a1: a1 as f32, a2: a2 as f32, a3: a3 as f32 }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Svf {
    ic1: f32,
    ic2: f32,
}

pub struct SvfOut {
    pub lp: f32,
    pub bp: f32,
    pub hp: f32,
}

impl Svf {
    #[inline(always)]
    pub fn tick(&mut self, c: &SvfCoef, x: f32) -> SvfOut {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = 2.0 * v1 - self.ic1;
        self.ic2 = 2.0 * v2 - self.ic2;
        SvfOut { lp: v2, bp: v1, hp: x - c.k * v1 - v2 }
    }
    #[inline(always)]
    pub fn allpass(&mut self, c: &SvfCoef, x: f32) -> f32 {
        let o = self.tick(c, x);
        x - 2.0 * c.k * o.bp
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

// ---------------------------------------------------------------------------
// Ecualizador aislador de 3 bandas (Linkwitz-Riley 24 dB/oct, kill total)
// A ganancia unidad la suma es un pasa-todo: magnitud perfectamente plana.
// ---------------------------------------------------------------------------

pub const ISO_LOW_HZ: f64 = 250.0;
pub const ISO_HIGH_HZ: f64 = 3000.0;

#[derive(Default, Clone, Copy)]
struct IsoChan {
    s1: Svf,      // primera etapa en f1 (da LP y HP)
    s1lp: Svf,    // segunda etapa LP en f1
    s1hp: Svf,    // segunda etapa HP en f1
    ap: Svf,      // pasa-todo en f2 para alinear la fase de la banda grave
    s2: Svf,      // primera etapa en f2 sobre la parte aguda
    s2lp: Svf,
    s2hp: Svf,
}

pub struct Isolator {
    c1: SvfCoef,
    c2: SvfCoef,
    ch: [IsoChan; 2],
}

impl Isolator {
    pub fn new(sr: f64) -> Self {
        let q = std::f64::consts::FRAC_1_SQRT_2;
        Self { c1: SvfCoef::new(ISO_LOW_HZ, q, sr), c2: SvfCoef::new(ISO_HIGH_HZ, q, sr), ch: [IsoChan::default(); 2] }
    }

    #[inline(always)]
    pub fn tick(&mut self, c: usize, x: f32, gl: f32, gm: f32, gh: f32) -> f32 {
        let s = &mut self.ch[c];
        let a = s.s1.tick(&self.c1, x);
        let low = s.s1lp.tick(&self.c1, a.lp).lp;
        let rest = s.s1hp.tick(&self.c1, a.hp).hp;
        let low = s.ap.allpass(&self.c2, low);
        let b = s.s2.tick(&self.c2, rest);
        let mid = s.s2lp.tick(&self.c2, b.lp).lp;
        let high = s.s2hp.tick(&self.c2, b.hp).hp;
        // LR4 = Butterworth² ; los signos se compensan entre bandas con las dos etapas
        gl * low + gm * mid + gh * high
    }

    pub fn reset(&mut self) {
        self.ch = [IsoChan::default(); 2];
    }
}

/// Curva del potenciómetro de EQ: -1 = kill (−∞), 0 = 0 dB, +1 = +6 dB.
pub fn eq_gain(knob: f32) -> f32 {
    if knob >= 0.0 {
        10f32.powf(6.0 * knob / 20.0)
    } else {
        (1.0 + knob).max(0.0).powf(1.6)
    }
}

// ---------------------------------------------------------------------------
// Filtro de canal (estilo "Color FX"): izquierda = paso bajo, derecha = paso alto
// ---------------------------------------------------------------------------

pub struct ChannelFilter {
    sr: f64,
    st: [Svf; 2],
    st2: [Svf; 2],
    coef: SvfCoef,
    knob: f32,
    counter: u32,
}

impl ChannelFilter {
    pub fn new(sr: f64) -> Self {
        Self { sr, st: [Svf::default(); 2], st2: [Svf::default(); 2], coef: SvfCoef::default(), knob: 0.0, counter: 0 }
    }

    /// Procesa un bloque estéreo en el sitio. `target` es el valor del mando (-1..1).
    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], target: f32) {
        if self.knob.abs() < 1e-4 && target.abs() < 1e-4 {
            self.knob = 0.0;
            for s in self.st.iter_mut().chain(self.st2.iter_mut()) {
                s.reset();
            }
            return;
        }
        for i in 0..l.len() {
            if self.counter == 0 {
                self.knob += (target - self.knob) * 0.08;
                let k = self.knob as f64;
                let freq = if k < 0.0 {
                    // 20 kHz -> 60 Hz
                    20000.0 * (60.0f64 / 20000.0).powf(-k)
                } else {
                    // 20 Hz -> 12 kHz
                    20.0 * (12000.0f64 / 20.0).powf(k)
                };
                let q = 0.707 + 0.6 * k.abs();
                self.coef = SvfCoef::new(freq, q, self.sr);
            }
            self.counter = (self.counter + 1) % 16;
            let k = self.knob;
            let wet = (k.abs() / 0.06).min(1.0);
            for (c, x) in [(0usize, &mut l[i]), (1usize, &mut r[i])] {
                let o = self.st[c].tick(&self.coef, *x);
                let y = if k < 0.0 { o.lp } else { o.hp };
                let o2 = self.st2[c].tick(&self.coef, y);
                let y2 = if k < 0.0 { o2.lp } else { o2.hp };
                *x = *x * (1.0 - wet) + y2 * wet;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Suavizado de parámetros
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Smooth {
    pub value: f32,
    coef: f32,
}

impl Smooth {
    pub fn new(value: f32, sr: f64, ms: f64) -> Self {
        Self { value, coef: (1.0 - (-1.0 / (sr * ms / 1000.0)).exp()) as f32 }
    }
    #[inline(always)]
    pub fn next(&mut self, target: f32) -> f32 {
        self.value += (target - self.value) * self.coef;
        self.value
    }
}

// ---------------------------------------------------------------------------
// Limitador de pico con anticipación (1.5 ms), techo -0.3 dBFS
// ---------------------------------------------------------------------------

pub struct Limiter {
    look: usize,
    buf_l: Vec<f32>,
    buf_r: Vec<f32>,
    gains: Vec<f32>,
    idx: usize,
    env: f32,
    att: f32,
    rel: f32,
    ceiling: f32,
    pub reduction: f32,
}

impl Limiter {
    pub fn new(sr: f64) -> Self {
        let look = ((sr * 0.0015) as usize).max(8);
        Self {
            look,
            buf_l: vec![0.0; look],
            buf_r: vec![0.0; look],
            gains: vec![1.0; look],
            idx: 0,
            env: 1.0,
            att: (1.0 - (-1.0 / (look as f64 / 5.0)).exp()) as f32,
            rel: (1.0 - (-1.0 / (sr * 0.12)).exp()) as f32,
            ceiling: 10f32.powf(-0.3 / 20.0),
            reduction: 1.0,
        }
    }

    pub fn process(&mut self, l: &mut [f32], r: &mut [f32], enabled: bool) {
        let mut min_env = 1.0f32;
        for i in 0..l.len() {
            let (xl, xr) = (l[i], r[i]);
            let peak = xl.abs().max(xr.abs());
            let need = if peak > self.ceiling { self.ceiling / peak } else { 1.0 };
            self.gains[self.idx] = need;
            let mut target = 1.0f32;
            for &g in &self.gains {
                if g < target {
                    target = g;
                }
            }
            if target < self.env {
                self.env += (target - self.env) * self.att;
            } else {
                self.env += (target - self.env) * self.rel;
            }
            let dl = self.buf_l[self.idx];
            let dr = self.buf_r[self.idx];
            self.buf_l[self.idx] = xl;
            self.buf_r[self.idx] = xr;
            self.idx = (self.idx + 1) % self.look;
            if enabled {
                let g = self.env;
                min_env = min_env.min(g);
                l[i] = (dl * g).clamp(-1.0, 1.0);
                r[i] = (dr * g).clamp(-1.0, 1.0);
            } else {
                l[i] = dl;
                r[i] = dr;
            }
        }
        self.reduction = if enabled { min_env } else { 1.0 };
    }

    pub fn latency(&self) -> usize {
        self.look
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, sr: f64, n: usize) -> Vec<f32> {
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / sr).sin() as f32).collect()
    }
    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    #[test]
    fn isolator_flat_at_unity_and_kills() {
        let sr = 44100.0;
        for &f in &[40.0, 250.0, 1000.0, 3000.0, 10000.0] {
            let x = sine(f, sr, 44100);
            let mut iso = Isolator::new(sr);
            let y: Vec<f32> = x.iter().map(|&v| iso.tick(0, v, 1.0, 1.0, 1.0)).collect();
            let g = rms(&y[22050..]) / rms(&x[22050..]);
            assert!((g - 1.0).abs() < 0.01, "f={f} gain={g}");
        }
        // kill de graves a 50 Hz
        let x = sine(50.0, sr, 44100);
        let mut iso = Isolator::new(sr);
        let y: Vec<f32> = x.iter().map(|&v| iso.tick(0, v, 0.0, 1.0, 1.0)).collect();
        let db = 20.0 * (rms(&y[22050..]) / rms(&x[22050..])).log10();
        assert!(db < -30.0, "low kill {db} dB");
        // kill de agudos a 12 kHz
        let x = sine(12000.0, sr, 44100);
        let mut iso = Isolator::new(sr);
        let y: Vec<f32> = x.iter().map(|&v| iso.tick(0, v, 1.0, 1.0, 0.0)).collect();
        let db = 20.0 * (rms(&y[22050..]) / rms(&x[22050..])).log10();
        assert!(db < -30.0, "high kill {db} dB");
    }

    #[test]
    fn sinc_reads_fractional_sine_accurately() {
        let sr = 44100.0;
        let x = sine(1000.0, sr, 8000);
        let rd = SincReader::new();
        let mut err = 0.0f64;
        for k in 0..2000 {
            let p = 2000.0 + k as f64 * 0.731;
            let (v, _) = rd.read2(&x, &x, p, 0.731);
            let truth = (2.0 * PI * 1000.0 * p / sr).sin();
            err = err.max((v as f64 - truth).abs());
        }
        assert!(err < 1e-3, "max err {err}");
    }

    #[test]
    fn resample_preserves_tone() {
        let x = sine(1000.0, 48000.0, 48000);
        let y = resample(&x, 48000, 44100);
        assert!((y.len() as i64 - 44100).abs() <= 1);
        let mut err = 0.0f64;
        for i in 1000..40000 {
            let truth = (2.0 * PI * 1000.0 * i as f64 / 44100.0).sin();
            err = err.max((y[i] as f64 - truth).abs());
        }
        assert!(err < 2e-3, "max err {err}");
    }

    #[test]
    fn limiter_keeps_ceiling() {
        let mut l: Vec<f32> = sine(100.0, 44100.0, 44100).iter().map(|v| v * 2.0).collect();
        let mut r = l.clone();
        let mut lim = Limiter::new(44100.0);
        lim.process(&mut l, &mut r, true);
        let peak = l.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        assert!(peak <= 1.0);
    }
}
