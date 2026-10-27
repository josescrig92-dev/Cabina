//! Un plato: lectura de la pista con "voces" que se funden entre sí en cada salto
//! (hot cue, loop, seek, play/pausa), varispeed con interpolación sinc y keylock
//! con time-stretch espectral.

use super::dsp::SincReader;
use super::stretch::Stretcher;
use super::TrackAudio;
use std::sync::Arc;

pub const MAX_BLOCK: usize = 512;
/// Fundido para saltos (≈5.8 ms a 44.1 kHz).
const XF: f32 = 256.0;
/// Fundido al pausar.
const PAUSE_FADE: f32 = 160.0;
/// Fundido en la costura del loop.
const LOOP_XF: f32 = 96.0;
const N_VOICES: usize = 4;
const N_STRETCH: usize = 3;
const IN_CAP: usize = MAX_BLOCK * 4;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Off,
    Direct,
    Stretch,
}

#[derive(Clone, Copy)]
struct Voice {
    kind: Kind,
    /// Direct: posición de lectura. Stretch: posición de entrada (entera).
    pos: f64,
    tail: f64,
    tail_gain: f32,
    slot: usize,
    acc: f64,
    gain: f32,
    target: f32,
    step: f32,
    /// Muestras de espera antes de empezar la rampa de ganancia.
    hold: u32,
    /// Si > 0, cuenta atrás tras la cual la voz se desvanece.
    release_in: u32,
}

impl Voice {
    const OFF: Voice = Voice {
        kind: Kind::Off,
        pos: 0.0,
        tail: 0.0,
        tail_gain: 0.0,
        slot: 0,
        acc: 0.0,
        gain: 0.0,
        target: 0.0,
        step: 0.0,
        hold: 0,
        release_in: 0,
    };
    fn finished(&self) -> bool {
        self.gain <= 0.0 && self.target <= 0.0 && self.hold == 0
    }
}

#[inline(always)]
fn env(v: &mut Voice) -> f32 {
    if v.hold > 0 {
        v.hold -= 1;
    } else if v.gain < v.target {
        v.gain = (v.gain + v.step).min(v.target);
    } else if v.gain > v.target {
        v.gain = (v.gain - v.step).max(v.target);
    }
    if v.release_in > 0 {
        v.release_in -= 1;
        if v.release_in == 0 {
            v.target = 0.0;
            v.step = 1.0 / XF;
        }
    }
    v.gain
}

#[derive(Clone, Copy, Debug)]
pub struct Loop {
    pub start: f64,
    pub end: f64,
    pub beats: f64,
}

/// Copia muestras de la pista desde `pos` (entero), dando la vuelta en el loop.
fn fill(audio: &TrackAudio, pos: &mut f64, lp: Option<Loop>, l: &mut [f32], r: &mut [f32]) {
    let n = audio.l.len() as i64;
    let (ls, le) = match lp {
        Some(lp) => (lp.start.round(), lp.end.round()),
        None => (0.0, f64::INFINITY),
    };
    for k in 0..l.len() {
        let i = *pos as i64;
        if i >= 0 && i < n {
            l[k] = audio.l[i as usize];
            r[k] = audio.r[i as usize];
        } else {
            l[k] = 0.0;
            r[k] = 0.0;
        }
        *pos += 1.0;
        if *pos >= le {
            *pos -= le - ls;
        }
    }
}

pub struct Deck {
    sr: f64,
    pub audio: Option<Arc<TrackAudio>>,
    pub len: f64,
    pub bpm: f64,
    /// Primer beat de la rejilla (frames).
    pub grid: f64,
    pub cue: f64,
    pub hot: [f64; 8],
    pub playing: bool,
    pub preview: bool,
    /// Posición que se está oyendo (frames).
    pub pos: f64,
    pub rate: f64,
    pub bend: f64,
    pub keylock: bool,
    pub synced: bool,
    pub lp: Option<Loop>,
    pub auto_gain: f32,
    pub cues_changed: bool,
    voices: [Voice; N_VOICES],
    primary: usize,
    stretch: Vec<Stretcher>,
    stretch_used: [bool; N_STRETCH],
    reader: SincReader,
    in_l: Vec<f32>,
    in_r: Vec<f32>,
    tmp_l: Vec<f32>,
    tmp_r: Vec<f32>,
    unity_count: u32,
}

impl Deck {
    pub fn new(sr: f64) -> Self {
        let stretch: Vec<Stretcher> = (0..N_STRETCH).map(|_| Stretcher::new(sr)).collect();
        let pre = stretch[0].preroll;
        Self {
            sr,
            audio: None,
            len: 0.0,
            bpm: 120.0,
            grid: 0.0,
            cue: 0.0,
            hot: [f64::NAN; 8],
            playing: false,
            preview: false,
            pos: 0.0,
            rate: 1.0,
            bend: 1.0,
            keylock: false,
            synced: false,
            lp: None,
            auto_gain: 1.0,
            cues_changed: false,
            voices: [Voice::OFF; N_VOICES],
            primary: 0,
            stretch,
            stretch_used: [false; N_STRETCH],
            reader: SincReader::new(),
            in_l: vec![0.0; IN_CAP.max(pre)],
            in_r: vec![0.0; IN_CAP.max(pre)],
            tmp_l: vec![0.0; MAX_BLOCK],
            tmp_r: vec![0.0; MAX_BLOCK],
            unity_count: 0,
        }
    }

    pub fn eff(&self) -> f64 {
        self.rate * self.bend
    }

    pub fn beat(&self) -> f64 {
        60.0 / self.bpm.max(1.0) * self.sr
    }

    pub fn snap(&self, t: f64) -> f64 {
        let b = self.beat();
        self.grid + ((t - self.grid) / b).round() * b
    }

    /// Fase dentro del beat (0..1).
    pub fn phase(&self) -> f64 {
        ((self.pos - self.grid) / self.beat()).rem_euclid(1.0)
    }

    fn needs_stretch(&self) -> bool {
        self.keylock && (self.eff() - 1.0).abs() > 1e-9
    }

    pub fn is_stretching(&self) -> bool {
        self.voices[self.primary].kind == Kind::Stretch
    }

    fn kill(&mut self, i: usize) {
        if self.voices[i].kind == Kind::Stretch {
            self.stretch_used[self.voices[i].slot] = false;
        }
        self.voices[i] = Voice::OFF;
    }

    fn free_voice(&mut self) -> usize {
        if let Some(i) = self.voices.iter().position(|v| v.kind == Kind::Off) {
            return i;
        }
        let mut best = 0;
        for i in 1..N_VOICES {
            if self.voices[i].gain < self.voices[best].gain {
                best = i;
            }
        }
        self.kill(best);
        best
    }

    fn spawn_direct(&mut self, p: f64, fade: f32) -> usize {
        let i = self.free_voice();
        self.voices[i] = Voice { kind: Kind::Direct, pos: p, gain: 0.0, target: 1.0, step: 1.0 / fade, ..Voice::OFF };
        i
    }

    fn spawn_stretch(&mut self, p: f64) -> Option<(usize, u32)> {
        let slot = self.stretch_used.iter().position(|u| !u)?;
        let i = self.free_voice();
        self.stretch_used[slot] = true;
        let eff = self.eff();
        let st = &mut self.stretch[slot];
        st.reset();
        // La salida del stretcher va retrasada: arrancamos la entrada por delante para que
        // al terminar la latencia de salida suene exactamente la posición pedida.
        let q = (p + st.in_lat as f64 + st.out_lat as f64 * eff).round();
        let pre = st.preroll;
        let mut start = q - pre as f64;
        if let Some(a) = &self.audio {
            fill(a, &mut start, None, &mut self.in_l[..pre], &mut self.in_r[..pre]);
        }
        st.seek(&self.in_l[..pre], &self.in_r[..pre], eff);
        let out_lat = st.out_lat as u32;
        self.voices[i] = Voice {
            kind: Kind::Stretch,
            pos: q,
            slot,
            gain: 0.0,
            target: 1.0,
            step: 1.0 / XF,
            hold: out_lat,
            ..Voice::OFF
        };
        Some((i, out_lat))
    }

    /// Salta a `p` (frames) con fundido. Si está parado solo mueve la posición.
    pub fn start_at(&mut self, p: f64) {
        for v in self.voices.iter_mut() {
            if v.kind != Kind::Off {
                v.target = 0.0;
                v.step = 1.0 / XF;
                v.hold = 0;
                v.release_in = 0;
            }
        }
        self.pos = p.clamp(0.0, self.len);
        self.unity_count = 0;
        if !self.playing || self.audio.is_none() {
            return;
        }
        let p = self.pos;
        if self.needs_stretch() {
            if let Some((si, out_lat)) = self.spawn_stretch(p) {
                // Puente: mientras el stretcher se llena suena la lectura directa
                let bi = self.spawn_direct(p, XF);
                self.voices[bi].release_in = out_lat.max(1);
                self.primary = si;
                return;
            }
        }
        self.primary = self.spawn_direct(p, XF);
    }

    pub fn play_at(&mut self, p: f64) {
        if self.audio.is_none() {
            return;
        }
        self.playing = true;
        self.start_at(p);
    }

    pub fn pause(&mut self) {
        self.playing = false;
        self.preview = false;
        for v in self.voices.iter_mut() {
            if v.kind != Kind::Off {
                v.target = 0.0;
                v.step = 1.0 / PAUSE_FADE;
                v.hold = 0;
                v.release_in = 0;
            }
        }
    }

    pub fn load(&mut self, audio: Arc<TrackAudio>, bpm: f64, grid: f64, cue: f64, hot: [f64; 8], gain: f32) -> Option<Arc<TrackAudio>> {
        for i in 0..N_VOICES {
            self.kill(i);
        }
        let sr = self.sr;
        self.len = audio.l.len() as f64;
        self.bpm = if bpm > 0.0 { bpm } else { 120.0 };
        self.grid = grid * sr;
        self.cue = cue * sr;
        self.hot = hot.map(|h| if h.is_nan() { h } else { h * sr });
        self.playing = false;
        self.preview = false;
        self.pos = self.cue;
        self.bend = 1.0;
        self.lp = None;
        self.synced = false;
        self.auto_gain = gain;
        self.audio.replace(audio)
    }

    pub fn unload(&mut self) -> Option<Arc<TrackAudio>> {
        for i in 0..N_VOICES {
            self.kill(i);
        }
        self.playing = false;
        self.lp = None;
        self.len = 0.0;
        self.pos = 0.0;
        self.audio.take()
    }

    // ---- transporte ----

    pub fn cue_down(&mut self, quantize: bool) {
        if self.audio.is_none() {
            return;
        }
        if self.playing {
            self.pause();
            self.lp = None;
            self.pos = self.cue;
            return;
        }
        if (self.pos - self.cue).abs() < self.sr * 0.001 {
            self.preview = true;
            self.play_at(self.cue);
        } else {
            self.cue = if quantize { self.snap(self.pos).max(0.0) } else { self.pos };
            self.pos = self.cue;
            self.cues_changed = true;
        }
    }

    pub fn cue_up(&mut self) {
        if self.preview {
            self.pause();
            self.lp = None;
            self.pos = self.cue;
        }
    }

    pub fn hot_cue(&mut self, i: usize, quantize: bool) {
        if self.audio.is_none() || i >= 8 {
            return;
        }
        if self.hot[i].is_nan() {
            self.hot[i] = if quantize { self.snap(self.pos).max(0.0) } else { self.pos };
            self.cues_changed = true;
            return;
        }
        let mut target = self.hot[i];
        if quantize && self.playing {
            // conserva la fase dentro del beat para no descuadrar la mezcla
            target += self.phase() * self.beat();
            if (target - self.hot[i]) > self.beat() * 0.5 {
                target -= self.beat();
            }
        }
        if let Some(lp) = self.lp {
            if target < lp.start || target >= lp.end {
                self.lp = None;
            }
        }
        self.preview = false;
        if self.playing {
            self.start_at(target);
        } else {
            self.play_at(target);
        }
    }

    pub fn clear_hot_cue(&mut self, i: usize) {
        if i < 8 {
            self.hot[i] = f64::NAN;
            self.cues_changed = true;
        }
    }

    pub fn seek(&mut self, p: f64) {
        if self.audio.is_none() {
            return;
        }
        if let Some(lp) = self.lp {
            if p < lp.start || p >= lp.end {
                self.lp = None;
            }
        }
        self.start_at(p);
    }

    pub fn scrub(&mut self, delta: f64) {
        if !self.playing {
            self.pos = (self.pos + delta).clamp(0.0, self.len);
        }
    }

    fn wrap_into_loop(&mut self) {
        let Some(lp) = self.lp else { return };
        let len = lp.end - lp.start;
        let (ls, le) = (lp.start.round(), lp.end.round());
        for v in self.voices.iter_mut() {
            match v.kind {
                Kind::Direct if v.pos >= lp.end => v.pos = lp.start + (v.pos - lp.start).rem_euclid(len),
                Kind::Stretch if v.pos >= le => v.pos = ls + (v.pos - ls).rem_euclid(le - ls),
                _ => {}
            }
        }
        if self.pos >= lp.end {
            self.pos = lp.start + (self.pos - lp.start).rem_euclid(len);
        }
    }

    pub fn auto_loop(&mut self, beats: f64, quantize: bool) {
        if self.audio.is_none() || beats <= 0.0 {
            return;
        }
        if let Some(lp) = self.lp {
            if (lp.beats - beats).abs() < 1e-9 {
                self.lp = None;
                return;
            }
        }
        let b = self.beat();
        let start = match self.lp {
            Some(lp) => lp.start,
            None if quantize => {
                let s = self.grid + ((self.pos - self.grid) / b + 1e-6).floor() * b;
                if s < 0.0 { s + b } else { s }
            }
            None => self.pos,
        };
        let end = start + beats * b;
        if end > self.len {
            return;
        }
        self.lp = Some(Loop { start, end, beats });
        self.wrap_into_loop();
    }

    pub fn loop_resize(&mut self, factor: f64) {
        if let Some(lp) = self.lp {
            let beats = (lp.beats * factor).clamp(1.0 / 32.0, 512.0);
            let end = lp.start + beats * self.beat();
            if end <= self.len {
                self.lp = Some(Loop { start: lp.start, end, beats });
                self.wrap_into_loop();
            }
        }
    }

    pub fn loop_exit(&mut self) {
        self.lp = None;
    }

    pub fn beat_jump(&mut self, beats: f64) {
        if self.audio.is_none() {
            return;
        }
        let d = beats * self.beat();
        if let Some(lp) = self.lp {
            // desplaza el loop entero, como en los CDJ
            let nl = Loop { start: lp.start + d, end: lp.end + d, beats: lp.beats };
            if nl.start >= 0.0 && nl.end <= self.len {
                self.lp = Some(nl);
            } else {
                self.lp = None;
            }
        }
        let p = (self.pos + d).clamp(0.0, self.len);
        self.start_at(p);
    }

    // ---- render ----

    pub fn render(&mut self, out_l: &mut [f32], out_r: &mut [f32]) {
        let n = out_l.len();
        out_l.fill(0.0);
        out_r.fill(0.0);
        let Some(audio) = self.audio.clone() else { return };
        let eff = self.eff();

        if self.playing {
            let pk = self.voices[self.primary].kind;
            let stretching = self.needs_stretch();
            if stretching && pk == Kind::Direct {
                self.start_at(self.pos);
            } else if !stretching && pk == Kind::Stretch {
                if self.keylock {
                    // velocidad 1 con keylock: tras un momento estable volvemos a lectura directa (bit-perfect)
                    self.unity_count += n as u32;
                    if self.unity_count as f64 > self.sr * 0.15 {
                        self.start_at(self.pos);
                    }
                } else {
                    self.start_at(self.pos);
                }
            } else {
                self.unity_count = 0;
            }
        }

        let lp = self.lp;
        for vi in 0..N_VOICES {
            let mut v = self.voices[vi];
            match v.kind {
                Kind::Off => continue,
                Kind::Direct => {
                    for i in 0..n {
                        let g = env(&mut v);
                        if g > 0.0 || v.tail_gain > 0.0 {
                            let (mut a, mut b) = self.reader.read2(&audio.l, &audio.r, v.pos, eff);
                            if v.tail_gain > 0.0 {
                                let (ta, tb) = self.reader.read2(&audio.l, &audio.r, v.tail, eff);
                                let tg = v.tail_gain;
                                a = a * (1.0 - tg) + ta * tg;
                                b = b * (1.0 - tg) + tb * tg;
                                v.tail += eff;
                                v.tail_gain = (tg - 1.0 / LOOP_XF).max(0.0);
                            }
                            out_l[i] += a * g;
                            out_r[i] += b * g;
                        }
                        v.pos += eff;
                        if let Some(lp) = lp {
                            if v.pos >= lp.end {
                                v.tail = v.pos;
                                v.tail_gain = 1.0;
                                v.pos = lp.start + (v.pos - lp.start).rem_euclid(lp.end - lp.start);
                            }
                        }
                    }
                    if v.finished() {
                        v = Voice::OFF;
                    }
                }
                Kind::Stretch => {
                    let total = v.acc + n as f64 * eff;
                    let cnt = (total.floor() as usize).min(IN_CAP);
                    v.acc = total - cnt as f64;
                    fill(&audio, &mut v.pos, lp, &mut self.in_l[..cnt], &mut self.in_r[..cnt]);
                    self.stretch[v.slot].process(&self.in_l[..cnt], &self.in_r[..cnt], &mut self.tmp_l[..n], &mut self.tmp_r[..n]);
                    for i in 0..n {
                        let g = env(&mut v);
                        out_l[i] += self.tmp_l[i] * g;
                        out_r[i] += self.tmp_r[i] * g;
                    }
                    if v.finished() {
                        self.stretch_used[v.slot] = false;
                        v = Voice::OFF;
                    }
                }
            }
            self.voices[vi] = v;
        }

        if self.playing {
            let v = self.voices[self.primary];
            match v.kind {
                Kind::Direct => self.pos = v.pos,
                Kind::Stretch => {
                    let st = &self.stretch[v.slot];
                    let mut p = v.pos - st.in_lat as f64 - st.out_lat as f64 * eff;
                    if let Some(lp) = lp {
                        if p < lp.start && v.pos >= lp.start {
                            p += lp.end.round() - lp.start.round();
                        }
                    }
                    self.pos = p.max(0.0);
                }
                Kind::Off => {}
            }
            if self.lp.is_none() && self.pos >= self.len {
                self.pos = self.len;
                self.pause();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pista de clics cada 0.5 s (un pulso de 1 muestra seguido de un seno corto).
    fn clicks(sr: f64, secs: f64) -> TrackAudio {
        let n = (sr * secs) as usize;
        let mut l = vec![0.0f32; n];
        let period = (sr * 0.5) as usize;
        let mut i = 0;
        while i < n {
            for k in 0..200.min(n - i) {
                l[i + k] = ((k as f64 * 0.3).sin() * (1.0 - k as f64 / 200.0)) as f32;
            }
            i += period;
        }
        TrackAudio { r: l.clone(), l }
    }

    fn render_secs(d: &mut Deck, secs: f64, sr: f64) -> Vec<f32> {
        let mut out = vec![];
        let mut l = vec![0.0; 256];
        let mut r = vec![0.0; 256];
        for _ in 0..((secs * sr) as usize / 256) {
            d.render(&mut l, &mut r);
            out.extend_from_slice(&l);
        }
        out
    }

    fn onsets(x: &[f32]) -> Vec<usize> {
        let mut v = vec![];
        let mut last = 0usize;
        for (i, s) in x.iter().enumerate() {
            if s.abs() > 0.15 && (v.is_empty() || i - last > 4000) {
                v.push(i);
                last = i;
            } else if s.abs() > 0.15 {
                last = i;
            }
        }
        v
    }

    #[test]
    fn direct_playback_is_bit_exact_at_unity() {
        let sr = 44100.0;
        let a = Arc::new(clicks(sr, 4.0));
        let mut d = Deck::new(sr);
        d.load(a.clone(), 120.0, 0.0, 0.0, [f64::NAN; 8], 1.0);
        d.play_at(0.0);
        let out = render_secs(&mut d, 2.0, sr);
        // tras el fundido inicial, idéntico al original
        for i in 300..out.len() {
            assert!((out[i] - a.l[i]).abs() < 1e-6, "i={i}");
        }
    }

    #[test]
    fn keylock_output_is_aligned_with_reported_position() {
        let sr = 44100.0;
        let a = Arc::new(clicks(sr, 8.0));
        let mut d = Deck::new(sr);
        d.load(a, 120.0, 0.0, 0.0, [f64::NAN; 8], 1.0);
        d.keylock = true;
        d.rate = 1.06;
        let start = sr * 1.0;
        d.play_at(start);
        let out = render_secs(&mut d, 3.0, sr);
        // Los clics del original están en múltiplos de 0.5 s. A velocidad 1.06 y empezando en 1.0 s,
        // el clic de t=1.5 s debe sonar en (0.5/1.06) s de salida.
        let on = onsets(&out);
        let expect: Vec<f64> = (1..5).map(|k| k as f64 * 0.5 / 1.06 * sr).collect();
        for e in &expect {
            let near = on.iter().map(|&o| (o as f64 - e).abs()).fold(f64::MAX, f64::min);
            assert!(near < sr * 0.004, "clic esperado en {e}, encontrados {on:?}");
        }
    }

    #[test]
    fn loop_repeats_without_drift() {
        let sr = 44100.0;
        let a = Arc::new(clicks(sr, 8.0));
        let mut d = Deck::new(sr);
        d.load(a, 120.0, 0.0, 0.0, [f64::NAN; 8], 1.0);
        d.play_at(sr * 1.0);
        d.auto_loop(2.0, true); // 1 s
        let out = render_secs(&mut d, 5.0, sr);
        let on = onsets(&out);
        assert!(on.len() >= 9, "{on:?}");
        // el primer ataque cae dentro del fundido de entrada: se ignora
        for w in on[1..].windows(2) {
            let dt = (w[1] - w[0]) as f64;
            assert!((dt - sr * 0.5).abs() < 3.0, "{on:?}");
        }
        assert!(d.pos >= sr * 1.0 && d.pos < sr * 2.0);
    }

    /// Frecuencia por cruces por cero (con interpolación) de un tramo de señal.
    fn freq(x: &[f32], sr: f64) -> f64 {
        let mut cross = vec![];
        for i in 1..x.len() {
            if x[i - 1] < 0.0 && x[i] >= 0.0 {
                cross.push(i as f64 - 1.0 + (-x[i - 1] / (x[i] - x[i - 1])) as f64);
            }
        }
        (cross.len() - 1) as f64 / ((cross[cross.len() - 1] - cross[0]) / sr)
    }

    #[test]
    fn keylock_keeps_pitch_and_varispeed_shifts_it() {
        let sr = 44100.0;
        let n = (sr * 10.0) as usize;
        let tone: Vec<f32> = (0..n).map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / sr).sin() as f32 * 0.5).collect();
        let a = Arc::new(TrackAudio { l: tone.clone(), r: tone });
        for (keylock, expect) in [(true, 440.0), (false, 440.0 * 1.08)] {
            let mut d = Deck::new(sr);
            d.load(a.clone(), 120.0, 0.0, 0.0, [f64::NAN; 8], 1.0);
            d.keylock = keylock;
            d.rate = 1.08;
            d.play_at(sr);
            let out = render_secs(&mut d, 3.0, sr);
            let f = freq(&out[(sr * 0.5) as usize..(sr * 2.5) as usize], sr);
            assert!((f - expect).abs() / expect < 0.002, "keylock={keylock}: {f:.2} Hz, esperado {expect:.2}");
            // la posición avanza al 108 %
            assert!((d.pos / sr - (1.0 + 3.0 * 1.08)).abs() < 0.02, "pos {}", d.pos / sr);
        }
    }
}
