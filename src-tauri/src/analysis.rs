//! Decodificación de archivos y análisis de pistas: onda de 3 bandas, BPM y rejilla,
//! tonalidad (notación Camelot) y sonoridad integrada (LUFS, EBU R128).

use crate::engine::dsp::{Svf, SvfCoef};
use rustfft::{num_complex::Complex, FftPlanner};
use std::f64::consts::PI;
use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::{MetadataOptions, StandardTagKey, Tag};
use symphonia::core::probe::Hint;

pub const AUDIO_EXTENSIONS: [&str; 11] = ["mp3", "wav", "wave", "aif", "aiff", "flac", "m4a", "mp4", "aac", "ogg", "alac"];
pub const WAVE_RATE: usize = 150;

#[derive(Default, Debug, Clone)]
pub struct Tags {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
}

pub struct Decoded {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    pub sr: u32,
    pub tags: Tags,
}

fn read_tags(tags: &[Tag], out: &mut Tags) {
    for t in tags {
        let v = t.value.to_string().trim().to_string();
        if v.is_empty() {
            continue;
        }
        match t.std_key {
            Some(StandardTagKey::TrackTitle) => out.title = Some(v),
            Some(StandardTagKey::Artist) => out.artist = Some(v),
            Some(StandardTagKey::AlbumArtist) if out.artist.is_none() => out.artist = Some(v),
            Some(StandardTagKey::Album) => out.album = Some(v),
            Some(StandardTagKey::Genre) => out.genre = Some(v),
            _ => {}
        }
    }
}

pub fn decode(path: &Path) -> Result<Decoded, String> {
    let file = File::open(path).map_err(|e| format!("No se pudo abrir el archivo: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let fmt_opts = FormatOptions { enable_gapless: true, ..Default::default() };
    let mut probed = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &MetadataOptions::default())
        .map_err(|e| format!("Formato no reconocido: {e}"))?;
    let mut tags = Tags::default();
    if let Some(m) = probed.metadata.get() {
        if let Some(rev) = m.current() {
            read_tags(rev.tags(), &mut tags);
        }
    }
    let mut format = probed.format;
    if let Some(rev) = format.metadata().current() {
        read_tags(rev.tags(), &mut tags);
    }
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("El archivo no contiene audio")?;
    let track_id = track.id;
    let sr = track.codec_params.sample_rate.unwrap_or(44100);
    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Códec no soportado: {e}"))?;
    let mut l = Vec::new();
    let mut r = Vec::new();
    if let Some(n) = track.codec_params.n_frames {
        l.reserve(n as usize);
        r.reserve(n as usize);
    }
    let mut sbuf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(SymError::ResetRequired) => break,
            Err(e) => {
                if l.is_empty() {
                    return Err(format!("Error al leer: {e}"));
                }
                break;
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                let ch = spec.channels.count().max(1);
                if sbuf.as_ref().map_or(true, |b| b.capacity() < buf.capacity()) {
                    sbuf = Some(SampleBuffer::new(buf.capacity() as u64, spec));
                }
                let sb = sbuf.as_mut().unwrap();
                sb.copy_interleaved_ref(buf);
                for f in sb.samples().chunks_exact(ch) {
                    l.push(f[0]);
                    r.push(if ch > 1 { f[1] } else { f[0] });
                }
            }
            Err(SymError::DecodeError(_)) => continue,
            Err(e) => {
                if l.is_empty() {
                    return Err(format!("Error al decodificar: {e}"));
                }
                break;
            }
        }
    }
    if l.is_empty() {
        return Err("El archivo no contiene audio".into());
    }
    Ok(Decoded { l, r, sr, tags })
}

// ---------------------------------------------------------------------------

pub struct Analysis {
    pub duration: f64,
    pub bpm: f64,
    pub grid: f64,
    pub key: Option<String>,
    pub key_name: Option<String>,
    pub lufs: f64,
    /// Onda: [graves | medios | agudos], `WAVE_RATE` valores por segundo cada una.
    pub wave: Vec<u8>,
    pub wave_len: usize,
}

pub fn analyze(l: &[f32], r: &[f32], sr: u32) -> Analysis {
    let srf = sr as f64;
    let n = l.len();
    let q = std::f64::consts::FRAC_1_SQRT_2;
    let c_lo = SvfCoef::new(200.0, q, srf);
    let c_hi = SvfCoef::new(2500.0, q, srf);
    let (mut lo1, mut lo2, mut hi1, mut hi2) = (Svf::default(), Svf::default(), Svf::default(), Svf::default());

    let wave_len = ((n as f64 / srf) * WAVE_RATE as f64).ceil() as usize + 1;
    let mut wl = vec![0f64; wave_len];
    let mut wm = vec![0f64; wave_len];
    let mut wh = vec![0f64; wave_len];
    let mut wc = vec![0u32; wave_len];
    let env_len = n * 1000 / sr as usize + 1;
    let mut el = vec![0f64; env_len]; // energía grave por ms
    let mut em = vec![0f64; env_len]; // energía media por ms
    let mut ec = vec![0u32; env_len];

    for i in 0..n {
        let x = 0.5 * (l[i] + r[i]);
        let a = lo1.tick(&c_lo, x).lp;
        let low = lo2.tick(&c_lo, a).lp;
        let b = hi1.tick(&c_hi, x).hp;
        let high = hi2.tick(&c_hi, b).hp;
        let mid = x - low - high;
        let wb = i * WAVE_RATE / sr as usize;
        wl[wb] += (low * low) as f64;
        wm[wb] += (mid * mid) as f64;
        wh[wb] += (high * high) as f64;
        wc[wb] += 1;
        let eb = i * 1000 / sr as usize;
        el[eb] += (low * low) as f64;
        em[eb] += (mid * mid) as f64;
        ec[eb] += 1;
    }

    // Onda normalizada
    let mut maxsum = 1e-12f64;
    for b in 0..wave_len {
        let c = wc[b].max(1) as f64;
        wl[b] = (wl[b] / c).sqrt();
        wm[b] = (wm[b] / c).sqrt();
        wh[b] = (wh[b] / c).sqrt() * 1.6;
        maxsum = maxsum.max(wl[b] + wm[b] + wh[b]);
    }
    let mut wave = Vec::with_capacity(wave_len * 3);
    for band in [&wl, &wm, &wh] {
        wave.extend(band.iter().map(|v| ((v / maxsum) * 255.0).round().clamp(0.0, 255.0) as u8));
    }

    for b in 0..env_len {
        let c = ec[b].max(1) as f64;
        el[b] = (el[b] / c).sqrt();
        em[b] = (em[b] / c).sqrt();
    }
    let (bpm, grid) = detect_tempo(&el, &em);
    let (key, key_name) = detect_key(l, r, sr);
    Analysis { duration: n as f64 / srf, bpm, grid, key, key_name, lufs: loudness(l, r, srf), wave, wave_len }
}

// ---------------------------------------------------------------------------
// Tempo y rejilla
// ---------------------------------------------------------------------------

fn onset(lo: &[f64], mid: &[f64]) -> Vec<f64> {
    let mut o = vec![0.0; lo.len()];
    for i in 1..lo.len() {
        o[i] = (lo[i] - lo[i - 1]).max(0.0) + 0.5 * (mid[i] - mid[i - 1]).max(0.0);
    }
    o
}

/// Pliega la envolvente de ataques sobre el periodo del beat; devuelve (afilado, fase en bins).
fn fold(o: &[f64], period: f64, smooth: usize) -> (f64, f64) {
    let nb = period.ceil() as usize;
    let mut acc = vec![0.0; nb];
    for (i, v) in o.iter().enumerate() {
        acc[((i as f64) % period) as usize % nb] += v;
    }
    let tot: f64 = acc.iter().sum();
    let mut best = -1.0;
    let mut k0 = 0;
    for k in 0..nb {
        let mut s = 0.0;
        for j in 0..=smooth {
            s += acc[(k + j) % nb];
        }
        if s > best {
            best = s;
            k0 = k;
        }
    }
    // centro de masa dentro de la ventana
    let mut num = 0.0;
    let mut den = 0.0;
    for j in 0..=smooth {
        let v = acc[(k0 + j) % nb];
        num += v * j as f64;
        den += v;
    }
    let phase = k0 as f64 + if den > 0.0 { num / den } else { 0.0 };
    (best / ((tot / nb as f64) * (smooth + 1) as f64).max(1e-12), phase % period)
}

fn detect_tempo(el: &[f64], em: &[f64]) -> (f64, f64) {
    // Envolvente a 100 Hz para buscar el tempo
    let n100 = el.len() / 10;
    if n100 < 400 {
        return (120.0, 0.0);
    }
    let mut l100 = vec![0.0; n100];
    let mut m100 = vec![0.0; n100];
    for i in 0..n100 {
        for j in 0..10 {
            l100[i] += el[i * 10 + j];
            m100[i] += em[i * 10 + j];
        }
    }
    let o = onset(&l100, &m100);
    let score = |bpm: f64| fold(&o, 100.0 * 60.0 / bpm, 1).0;
    let mut best = (f64::MIN, 120.0);
    let mut b = 85.0;
    while b < 170.0 {
        let s = score(b);
        if s > best.0 {
            best = (s, b);
        }
        b += 0.25;
    }
    let mut fine = best;
    let mut b = best.1 - 0.3;
    while b <= best.1 + 0.3 {
        let s = score(b);
        if s > fine.0 {
            fine = (s, b);
        }
        b += 0.01;
    }
    // Afinado con la envolvente de 1 ms: en una pista entera, 0.01 BPM de error ya descuadra varios ms
    let o1k = onset(el, em);
    let score1k = |bpm: f64| fold(&o1k, 1000.0 * 60.0 / bpm, 6).0;
    let mut best1k = (score1k(fine.1), fine.1);
    let mut b = fine.1 - 0.06;
    while b <= fine.1 + 0.06 {
        let s = score1k(b);
        if s > best1k.0 {
            best1k = (s, b);
        }
        b += 0.002;
    }
    let mut bpm = best1k.1;
    let ib = bpm.round();
    if (ib - bpm).abs() < 0.03 && score1k(ib) >= best1k.0 * 0.98 {
        bpm = ib;
    } else {
        bpm = (bpm * 1000.0).round() / 1000.0;
    }

    // Fase con precisión de milisegundos
    let period = 1000.0 * 60.0 / bpm;
    let (_, phase_ms) = fold(&o1k, period, 6);
    let beat_s = 60.0 / bpm;
    // El filtro grave y la ventana de ataques retrasan la detección unos 4 ms: se compensa
    let mut grid = (phase_ms / 1000.0 - 0.0042).rem_euclid(beat_s);
    if grid > beat_s - 0.012 {
        grid -= beat_s; // el primer beat está en el instante 0
    }

    // Primer tiempo del compás. Si hay acento claro cada 4 beats se usa; si no, se toma
    // el primer beat con pegada en graves (casi todas las pistas de baile empiezan en el 1).
    let lo_on = onset(el, &vec![0.0; el.len()]);
    let strength = |t: f64| -> f64 {
        if t < -0.005 {
            return 0.0;
        }
        let i = (t.max(0.0) * 1000.0) as usize;
        lo_on[i.saturating_sub(5)..(i + 6).min(lo_on.len())].iter().sum()
    };
    let n_beats = ((el.len() as f64 / 1000.0 - grid) / beat_s) as usize;
    let mut acc = [0.0f64; 4];
    for j in 0..n_beats {
        acc[j % 4] += strength(grid + j as f64 * beat_s);
    }
    let mut order = [0usize, 1, 2, 3];
    order.sort_by(|a, b| acc[*b].partial_cmp(&acc[*a]).unwrap());
    let first_strong = {
        let max = (0..n_beats.min(256)).map(|j| strength(grid + j as f64 * beat_s)).fold(0.0, f64::max);
        (0..n_beats.min(256)).find(|&j| strength(grid + j as f64 * beat_s) > 0.3 * max).unwrap_or(0)
    };
    let k = if acc[order[0]] > acc[order[1]] * 1.15 { order[0] } else { first_strong % 4 };
    grid += k as f64 * beat_s;
    if grid >= 4.0 * beat_s - 0.012 {
        grid -= 4.0 * beat_s;
    }
    (bpm, grid)
}

// ---------------------------------------------------------------------------
// Tonalidad (Krumhansl-Kessler sobre un cromagrama)
// ---------------------------------------------------------------------------

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "Eb", "E", "F", "F#", "G", "Ab", "A", "Bb", "B"];
const CAMELOT_MAJOR: [&str; 12] = ["8B", "3B", "10B", "5B", "12B", "7B", "2B", "9B", "4B", "11B", "6B", "1B"];
const CAMELOT_MINOR: [&str; 12] = ["5A", "12A", "7A", "2A", "9A", "4A", "11A", "6A", "1A", "8A", "3A", "10A"];
const PROFILE_MAJOR: [f64; 12] = [6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88];
const PROFILE_MINOR: [f64; 12] = [6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17];

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let mut num = 0.0;
    let mut da = 0.0;
    let mut db = 0.0;
    for i in 0..a.len() {
        num += (a[i] - ma) * (b[i] - mb);
        da += (a[i] - ma).powi(2);
        db += (b[i] - mb).powi(2);
    }
    num / (da.sqrt() * db.sqrt()).max(1e-12)
}

pub fn detect_key(l: &[f32], r: &[f32], sr: u32) -> (Option<String>, Option<String>) {
    let dec = (sr as usize / 11025).max(1);
    let srd = sr as f64 / dec as f64;
    let mono: Vec<f32> = l.chunks(dec).zip(r.chunks(dec)).map(|(a, b)| (a.iter().sum::<f32>() + b.iter().sum::<f32>()) / (2 * a.len()) as f32).collect();
    const N: usize = 4096;
    const HOP: usize = 2048;
    if mono.len() < N * 4 {
        return (None, None);
    }
    let fft = FftPlanner::<f32>::new().plan_fft_forward(N);
    let win: Vec<f32> = (0..N).map(|i| (0.5 - 0.5 * (2.0 * PI * i as f64 / N as f64).cos()) as f32).collect();
    let lo_bin = (70.0 * N as f64 / srd).floor() as usize;
    let hi_bin = ((1800.0 * N as f64 / srd).ceil() as usize).min(N / 2 - 2);
    let mut chroma = [0.0f64; 12];
    let mut buf = vec![Complex::new(0.0f32, 0.0); N];
    let mut mag = vec![0.0f32; N / 2];
    let mut start = 0;
    while start + N <= mono.len() {
        for i in 0..N {
            buf[i] = Complex::new(mono[start + i] * win[i], 0.0);
        }
        fft.process(&mut buf);
        for k in 0..N / 2 {
            mag[k] = buf[k].norm();
        }
        let mut frame = [0.0f64; 12];
        for k in lo_bin.max(21)..hi_bin {
            let m = mag[k];
            if !(m > mag[k - 1] && m >= mag[k + 1]) {
                continue;
            }
            // solo picos que sobresalen del entorno (descarta el ruido de bombo y platos)
            let local: f32 = mag[k - 20..k + 20].iter().sum::<f32>() / 40.0;
            if m < local * 2.5 {
                continue;
            }
            // frecuencia exacta por interpolación parabólica
            let (a, b, c) = (mag[k - 1] as f64, m as f64, mag[k + 1] as f64);
            let delta = 0.5 * (a - c) / (a - 2.0 * b + c).min(-1e-12);
            let f = (k as f64 + delta) * srd / N as f64;
            let midi = 69.0 + 12.0 * (f / 440.0).log2();
            let dev = midi - midi.round();
            // los parciales afinados pesan más que los que caen entre notas
            let w = (PI * dev).cos().powi(2);
            frame[(midi.round() as i64).rem_euclid(12) as usize] += b * w;
        }
        let s: f64 = frame.iter().sum();
        if s > 0.0 {
            for i in 0..12 {
                chroma[i] += (frame[i] / s).sqrt();
            }
        }
        start += HOP;
    }
    if std::env::var("CABINA_DEBUG_KEY").is_ok() {
        let s: f64 = chroma.iter().sum();
        eprintln!("chroma {:?}", chroma.iter().zip(NOTE_NAMES).map(|(v, n)| format!("{n}:{:.3}", v / s)).collect::<Vec<_>>());
    }
    let mut best = (f64::MIN, 0usize, false);
    for tonic in 0..12 {
        let rot: Vec<f64> = (0..12).map(|i| chroma[(i + tonic) % 12]).collect();
        let cm = pearson(&rot, &PROFILE_MAJOR);
        let cn = pearson(&rot, &PROFILE_MINOR);
        if cm > best.0 {
            best = (cm, tonic, false);
        }
        if cn > best.0 {
            best = (cn, tonic, true);
        }
    }
    let (_, t, minor) = best;
    let camelot = if minor { CAMELOT_MINOR[t] } else { CAMELOT_MAJOR[t] };
    let name = format!("{}{}", NOTE_NAMES[t], if minor { "m" } else { "" });
    (Some(camelot.to_string()), Some(name))
}

// ---------------------------------------------------------------------------
// Sonoridad integrada (ITU-R BS.1770 / EBU R128)
// ---------------------------------------------------------------------------

struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}
impl Biquad {
    fn tick(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

fn k_weighting(sr: f64) -> (Biquad, Biquad) {
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (PI * f0 / sr).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (PI * f0 / sr).tan();
    let d = 1.0 + k / q + k * k;
    let hp = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / d, (1.0 - k / q + k * k) / d], z: [0.0; 2] };
    (shelf, hp)
}

pub fn loudness(l: &[f32], r: &[f32], sr: f64) -> f64 {
    let (mut s1, mut h1) = k_weighting(sr);
    let (mut s2, mut h2) = k_weighting(sr);
    let step = (sr * 0.1) as usize;
    let mut sq = Vec::with_capacity(l.len() / step + 1);
    let mut acc = 0.0;
    for i in 0..l.len() {
        let a = h1.tick(s1.tick(l[i] as f64));
        let b = h2.tick(s2.tick(r[i] as f64));
        acc += a * a + b * b;
        if (i + 1) % step == 0 {
            sq.push(acc / step as f64);
            acc = 0.0;
        }
    }
    if sq.len() < 4 {
        return -70.0;
    }
    // bloques de 400 ms con 75 % de solape
    let blocks: Vec<f64> = sq.windows(4).map(|w| w.iter().sum::<f64>() / 4.0).collect();
    let lk = |z: f64| -0.691 + 10.0 * z.max(1e-12).log10();
    let abs: Vec<f64> = blocks.iter().copied().filter(|&z| lk(z) > -70.0).collect();
    if abs.is_empty() {
        return -70.0;
    }
    let rel = lk(abs.iter().sum::<f64>() / abs.len() as f64) - 10.0;
    let gated: Vec<f64> = abs.into_iter().filter(|&z| lk(z) > rel).collect();
    if gated.is_empty() {
        return -70.0;
    }
    lk(gated.iter().sum::<f64>() / gated.len() as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bombo sintético a `bpm` con primer golpe en `offset` segundos y acorde de La menor.
    fn synth(bpm: f64, offset: f64, secs: f64, sr: u32) -> (Vec<f32>, Vec<f32>) {
        let n = (secs * sr as f64) as usize;
        let mut x = vec![0.0f32; n];
        let beat = 60.0 / bpm;
        let mut t = offset;
        let mut k = 0;
        while t < secs {
            let i0 = (t * sr as f64) as usize;
            let accent = if k % 4 == 0 { 1.0 } else { 0.7 };
            for j in 0..(0.25 * sr as f64) as usize {
                if i0 + j >= n {
                    break;
                }
                let tt = j as f64 / sr as f64;
                let f = 50.0 + 100.0 * (-tt * 30.0).exp();
                x[i0 + j] += (accent * (2.0 * PI * f * tt).sin() * (-tt * 12.0).exp()) as f32 * 0.8;
            }
            t += beat;
            k += 1;
        }
        for (i, v) in x.iter_mut().enumerate() {
            let tt = i as f64 / sr as f64;
            for f in [220.0, 261.63, 329.63, 440.0] {
                *v += (0.04 * (2.0 * PI * f * tt).sin()) as f32;
            }
        }
        (x.clone(), x)
    }

    #[test]
    fn tempo_grid_and_key() {
        let sr = 44100;
        for &(bpm, off) in &[(124.0, 0.25), (128.0, 0.0), (140.0, 0.1), (100.0, 0.37)] {
            let (l, r) = synth(bpm, off, 40.0, sr);
            let a = analyze(&l, &r, sr);
            assert!((a.bpm - bpm).abs() < 0.05, "bpm {} vs {}", a.bpm, bpm);
            let beat = 60.0 / bpm;
            let err = ((a.grid - off) / beat).rem_euclid(1.0);
            let err = err.min(1.0 - err) * beat;
            assert!(err < 0.0015, "grid {} vs {} (err {err})", a.grid, off);
            assert_eq!(a.key_name.as_deref(), Some("Am"), "key {:?}", a.key_name);
            assert_eq!(a.key.as_deref(), Some("8A"));
            assert!(a.lufs > -40.0 && a.lufs < 0.0);
        }
    }

    #[test]
    fn loudness_of_full_scale_sine() {
        // Seno de 1 kHz a 0 dBFS en ambos canales = 0 LUFS (cada canal solo daría -3 LUFS)
        let sr = 48000.0;
        let x: Vec<f32> = (0..(sr as usize * 5)).map(|i| (2.0 * PI * 997.0 * i as f64 / sr).sin() as f32).collect();
        let l = loudness(&x, &x, sr);
        assert!(l.abs() < 0.3, "lufs {l}");
    }
}
