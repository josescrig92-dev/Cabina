//! Motor de audio en tiempo real. Todo lo que ocurre en `Engine::process` corre en el
//! hilo de audio: sin bloqueos, sin reservas de memoria y sin E/S.

pub mod deck;
pub mod dsp;
pub mod host;
mod stretch;

use crossbeam_queue::ArrayQueue;
use deck::{Deck, MAX_BLOCK};
use dsp::{eq_gain, ChannelFilter, Isolator, Limiter, Smooth};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;

/// Audio de una pista ya remuestreado a la frecuencia del motor.
pub struct TrackAudio {
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

pub enum Command {
    Load { deck: usize, audio: Arc<TrackAudio>, bpm: f64, grid: f64, cue: f64, hot: [f64; 8], gain: f32 },
    Unload(usize),
    Play(usize),
    Pause(usize),
    Toggle(usize),
    CueDown(usize),
    CueUp(usize),
    Seek(usize, f64),
    Scrub(usize, f64),
    HotCue(usize, usize),
    HotCueClear(usize, usize),
    Loop(usize, f64),
    LoopExit(usize),
    LoopResize(usize, f64),
    BeatJump(usize, f64),
    Tempo(usize, f64),
    Bend(usize, f64),
    Keylock(usize, bool),
    Sync(usize, bool),
    Grid(usize, f64, f64),
}

// ---------------------------------------------------------------------------
// Estado compartido (motor -> interfaz) y parámetros (interfaz -> motor)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct AtomicF64(AtomicU64);
impl AtomicF64 {
    pub fn new(v: f64) -> Self {
        Self(AtomicU64::new(v.to_bits()))
    }
    pub fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Relaxed))
    }
    pub fn set(&self, v: f64) {
        self.0.store(v.to_bits(), Relaxed)
    }
}

#[derive(Default)]
pub struct AtomicF32(AtomicU32);
impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Relaxed)
    }
    /// Máximo para valores positivos (los bits de un f32 positivo son monótonos).
    pub fn max(&self, v: f32) {
        self.0.fetch_max(v.abs().to_bits(), Relaxed);
    }
    pub fn take(&self) -> f32 {
        f32::from_bits(self.0.swap(0, Relaxed))
    }
}

#[derive(Default)]
pub struct DeckShared {
    pub pos: AtomicF64,
    pub speed: AtomicF64,
    pub duration: AtomicF64,
    pub playing: AtomicBool,
    pub rate: AtomicF64,
    pub keylock: AtomicBool,
    pub stretching: AtomicBool,
    pub synced: AtomicBool,
    pub loop_start: AtomicF64,
    pub loop_end: AtomicF64,
    pub loop_beats: AtomicF64,
    pub cue: AtomicF64,
    pub hot: [AtomicF64; 8],
    pub bpm: AtomicF64,
    pub grid: AtomicF64,
    pub cue_gen: AtomicU64,
    pub load_gen: AtomicU64,
    pub peak_l: AtomicF32,
    pub peak_r: AtomicF32,
}

pub struct Shared {
    pub decks: [DeckShared; 2],
    pub sample_rate: AtomicU32,
    pub channels: AtomicU32,
    pub buffer: AtomicU32,
    /// Nanosegundos (reloj monotónico del proceso) al empezar el último callback.
    pub cb_time: AtomicU64,
    pub latency_ns: AtomicU64,
    pub master_l: AtomicF32,
    pub master_r: AtomicF32,
    pub limiter_gr: AtomicF32,
    pub dsp_load: AtomicF32,
    pub engine_gen: AtomicU64,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            decks: Default::default(),
            sample_rate: AtomicU32::new(44100),
            channels: AtomicU32::new(2),
            buffer: AtomicU32::new(256),
            cb_time: AtomicU64::new(0),
            latency_ns: AtomicU64::new(0),
            master_l: AtomicF32::default(),
            master_r: AtomicF32::default(),
            limiter_gr: AtomicF32::new(1.0),
            dsp_load: AtomicF32::default(),
            engine_gen: AtomicU64::new(0),
        }
    }
}

pub fn now_ns() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static T0: OnceLock<Instant> = OnceLock::new();
    T0.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

/// Parámetros continuos del mezclador. La interfaz los escribe, el motor los lee en cada bloque.
pub struct Params {
    pub trim: [AtomicF32; 2],   // -1..1 => ±12 dB
    pub eq: [[AtomicF32; 3]; 2], // hi, mid, low  (-1..1)
    pub filter: [AtomicF32; 2], // -1..1
    pub fader: [AtomicF32; 2],  // 0..1
    pub cue: [AtomicF32; 2],    // 0/1 (escucha en auriculares)
    pub xfader: AtomicF32,      // 0..1
    pub xf_curve: AtomicF32,    // 0 suave, 1 corte
    pub master: AtomicF32,      // 0..1
    pub cue_mix: AtomicF32,     // 0 = cue, 1 = master
    pub hp_level: AtomicF32,    // 0..1
    pub limiter: AtomicF32,     // 0/1
    pub quantize: AtomicF32,    // 0/1
    pub routing: AtomicF32,     // 0 = sin auriculares, 1 = canales 3/4, 2 = split mono
    pub auto_gain: AtomicF32,   // 0/1
}

impl Default for Params {
    fn default() -> Self {
        let z = || AtomicF32::new(0.0);
        Self {
            trim: [z(), z()],
            eq: [[z(), z(), z()], [z(), z(), z()]],
            filter: [z(), z()],
            fader: [AtomicF32::new(0.9), AtomicF32::new(0.9)],
            cue: [z(), z()],
            xfader: AtomicF32::new(0.5),
            xf_curve: z(),
            master: AtomicF32::new(0.8),
            cue_mix: AtomicF32::new(0.3),
            hp_level: AtomicF32::new(0.7),
            limiter: AtomicF32::new(1.0),
            quantize: AtomicF32::new(1.0),
            routing: AtomicF32::new(1.0),
            auto_gain: AtomicF32::new(1.0),
        }
    }
}

impl Params {
    pub fn set(&self, name: &str, value: f32) -> bool {
        let v = value;
        match name {
            "trim0" => self.trim[0].set(v),
            "trim1" => self.trim[1].set(v),
            "hi0" => self.eq[0][0].set(v),
            "mid0" => self.eq[0][1].set(v),
            "low0" => self.eq[0][2].set(v),
            "hi1" => self.eq[1][0].set(v),
            "mid1" => self.eq[1][1].set(v),
            "low1" => self.eq[1][2].set(v),
            "filter0" => self.filter[0].set(v),
            "filter1" => self.filter[1].set(v),
            "fader0" => self.fader[0].set(v),
            "fader1" => self.fader[1].set(v),
            "cue0" => self.cue[0].set(v),
            "cue1" => self.cue[1].set(v),
            "xfader" => self.xfader.set(v),
            "xf_curve" => self.xf_curve.set(v),
            "master" => self.master.set(v),
            "cue_mix" => self.cue_mix.set(v),
            "hp_level" => self.hp_level.set(v),
            "limiter" => self.limiter.set(v),
            "quantize" => self.quantize.set(v),
            "routing" => self.routing.set(v),
            "auto_gain" => self.auto_gain.set(v),
            _ => return false,
        }
        true
    }
}

// ---------------------------------------------------------------------------
// Canal del mezclador
// ---------------------------------------------------------------------------

struct Strip {
    iso: Isolator,
    filter: ChannelFilter,
    trim: Smooth,
    hi: Smooth,
    mid: Smooth,
    low: Smooth,
    fader: Smooth,
    xf: Smooth,
    cue: Smooth,
}

impl Strip {
    fn new(sr: f64) -> Self {
        let s = |v| Smooth::new(v, sr, 8.0);
        Self {
            iso: Isolator::new(sr),
            filter: ChannelFilter::new(sr),
            trim: s(1.0),
            hi: s(1.0),
            mid: s(1.0),
            low: s(1.0),
            fader: s(0.81),
            xf: s(0.707),
            cue: s(0.0),
        }
    }
}

fn xf_gains(x: f32, curve: f32) -> (f32, f32) {
    let x = x.clamp(0.0, 1.0);
    let smooth = ((x * std::f32::consts::FRAC_PI_2).cos(), (x * std::f32::consts::FRAC_PI_2).sin());
    // curva de corte: los dos canales a tope salvo en los extremos
    let cut = (((1.0 - x) / 0.04).min(1.0), (x / 0.04).min(1.0));
    let c = curve.clamp(0.0, 1.0);
    (smooth.0 * (1.0 - c) + cut.0 * c, smooth.1 * (1.0 - c) + cut.1 * c)
}

// ---------------------------------------------------------------------------
// Motor
// ---------------------------------------------------------------------------

pub struct Engine {
    pub sr: f64,
    decks: [Deck; 2],
    strips: [Strip; 2],
    limiter: Limiter,
    master: Smooth,
    hp: Smooth,
    cue_mix: Smooth,
    shared: Arc<Shared>,
    params: Arc<Params>,
    cmds: Arc<ArrayQueue<Command>>,
    garbage: Arc<ArrayQueue<Arc<TrackAudio>>>,
    dl: [Vec<f32>; 2],
    dr: [Vec<f32>; 2],
    ml: Vec<f32>,
    mr: Vec<f32>,
    cl: Vec<f32>,
    cr: Vec<f32>,
    load_avg: f32,
}

impl Engine {
    pub fn new(sr: f64, shared: Arc<Shared>, params: Arc<Params>, cmds: Arc<ArrayQueue<Command>>, garbage: Arc<ArrayQueue<Arc<TrackAudio>>>) -> Self {
        let v = || vec![0.0f32; MAX_BLOCK];
        Self {
            sr,
            decks: [Deck::new(sr), Deck::new(sr)],
            strips: [Strip::new(sr), Strip::new(sr)],
            limiter: Limiter::new(sr),
            master: Smooth::new(0.8, sr, 10.0),
            hp: Smooth::new(0.5, sr, 10.0),
            cue_mix: Smooth::new(0.3, sr, 10.0),
            shared,
            params,
            cmds,
            garbage,
            dl: [v(), v()],
            dr: [v(), v()],
            ml: v(),
            mr: v(),
            cl: v(),
            cr: v(),
            load_avg: 0.0,
        }
    }

    fn discard(&self, a: Option<Arc<TrackAudio>>) {
        if let Some(a) = a {
            // Liberar varios cientos de MB en el hilo de audio sería un corte: lo hace otro hilo.
            let _ = self.garbage.push(a);
        }
    }

    fn quantize(&self) -> bool {
        self.params.quantize.get() > 0.5
    }

    /// Posición en la que debe arrancar `d` para quedar en fase con el otro plato.
    fn aligned_pos(&self, d: usize) -> f64 {
        let (me, other) = (&self.decks[d], &self.decks[1 - d]);
        if other.audio.is_none() || !other.playing {
            return me.pos;
        }
        let ph = other.phase();
        let b = me.beat();
        let k = ((me.pos - me.grid) / b - ph).round();
        let mut t = me.grid + (k + ph) * b;
        if t < 0.0 {
            t += b;
        }
        t
    }

    fn follow_tempo(&mut self, leader: usize) {
        let f = 1 - leader;
        if self.decks[f].synced && self.decks[f].audio.is_some() && self.decks[leader].audio.is_some() {
            let target = self.decks[leader].bpm * self.decks[leader].rate / self.decks[f].bpm;
            self.decks[f].rate = target.clamp(0.25, 4.0);
        }
    }

    fn handle(&mut self, cmd: Command) {
        let q = self.quantize();
        match cmd {
            Command::Load { deck, audio, bpm, grid, cue, hot, gain } => {
                let old = self.decks[deck].load(audio, bpm, grid, cue, hot, gain);
                self.discard(old);
                self.shared.decks[deck].load_gen.fetch_add(1, Relaxed);
                self.shared.decks[deck].cue_gen.fetch_add(1, Relaxed);
            }
            Command::Unload(d) => {
                let old = self.decks[d].unload();
                self.discard(old);
            }
            Command::Play(d) => {
                if !self.decks[d].playing {
                    let mut p = self.decks[d].pos;
                    if p >= self.decks[d].len - 1.0 {
                        p = self.decks[d].cue;
                    }
                    self.decks[d].pos = p;
                    if self.decks[d].synced {
                        p = self.aligned_pos(d);
                    }
                    self.decks[d].play_at(p);
                }
                self.decks[d].preview = false;
            }
            Command::Pause(d) => self.decks[d].pause(),
            Command::Toggle(d) => {
                if self.decks[d].playing && !self.decks[d].preview {
                    self.decks[d].pause()
                } else {
                    self.handle(Command::Play(d))
                }
            }
            Command::CueDown(d) => self.decks[d].cue_down(q),
            Command::CueUp(d) => self.decks[d].cue_up(),
            Command::Seek(d, s) => self.decks[d].seek(s * self.sr),
            Command::Scrub(d, s) => self.decks[d].scrub(s * self.sr),
            Command::HotCue(d, i) => self.decks[d].hot_cue(i, q),
            Command::HotCueClear(d, i) => self.decks[d].clear_hot_cue(i),
            Command::Loop(d, b) => self.decks[d].auto_loop(b, q),
            Command::LoopExit(d) => self.decks[d].loop_exit(),
            Command::LoopResize(d, f) => self.decks[d].loop_resize(f),
            Command::BeatJump(d, b) => self.decks[d].beat_jump(b),
            Command::Tempo(d, r) => {
                self.decks[d].rate = r.clamp(0.25, 4.0);
                self.decks[d].synced = false;
                self.follow_tempo(d);
            }
            Command::Bend(d, b) => self.decks[d].bend = b.clamp(0.5, 1.5),
            Command::Keylock(d, on) => self.decks[d].keylock = on,
            Command::Sync(d, on) => {
                if !on {
                    self.decks[d].synced = false;
                } else if self.decks[d].audio.is_some() && self.decks[1 - d].audio.is_some() {
                    self.decks[d].synced = true;
                    self.decks[1 - d].synced = false;
                    self.follow_tempo(1 - d);
                    if self.decks[d].playing && self.decks[1 - d].playing {
                        let p = self.aligned_pos(d);
                        if (p - self.decks[d].pos).abs() > 2.0 {
                            self.decks[d].start_at(p);
                        }
                    }
                }
            }
            Command::Grid(d, bpm, grid) => {
                if bpm > 0.0 {
                    self.decks[d].bpm = bpm;
                    self.decks[d].grid = grid * self.sr;
                    self.follow_tempo(1 - d);
                    self.follow_tempo(d);
                }
            }
        }
    }

    /// Procesa un buffer intercalado de `channels` canales.
    pub fn process(&mut self, out: &mut [f32], channels: usize) {
        let t0 = now_ns();
        self.shared.cb_time.store(t0, Relaxed);
        while let Some(c) = self.cmds.pop() {
            self.handle(c);
        }
        self.publish();
        let frames = out.len() / channels.max(1);
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(MAX_BLOCK);
            self.render(n);
            self.write_out(&mut out[done * channels..(done + n) * channels], channels, n);
            done += n;
        }
        let elapsed = (now_ns() - t0) as f32 / 1e9;
        let budget = frames as f32 / self.sr as f32;
        if budget > 0.0 {
            self.load_avg += (elapsed / budget - self.load_avg) * 0.05;
            self.shared.dsp_load.set(self.load_avg);
        }
    }

    fn render(&mut self, n: usize) {
        let p = &self.params;
        let xfg = xf_gains(p.xfader.get(), p.xf_curve.get());
        let auto = p.auto_gain.get() > 0.5;
        for c in [&mut self.ml, &mut self.mr, &mut self.cl, &mut self.cr] {
            c[..n].fill(0.0);
        }
        for d in 0..2 {
            let (l, r) = (&mut self.dl[d][..n], &mut self.dr[d][..n]);
            self.decks[d].render(l, r);
            let st = &mut self.strips[d];
            let trim_t = 10f32.powf(p.trim[d].get().clamp(-1.0, 1.0) * 12.0 / 20.0) * if auto { self.decks[d].auto_gain } else { 1.0 };
            let (hi_t, mid_t, low_t) = (eq_gain(p.eq[d][0].get()), eq_gain(p.eq[d][1].get()), eq_gain(p.eq[d][2].get()));
            let f = p.fader[d].get().clamp(0.0, 1.0);
            let fader_t = f * f;
            let xf_t = if d == 0 { xfg.0 } else { xfg.1 };
            let cue_t = if p.cue[d].get() > 0.5 { 1.0 } else { 0.0 };
            let mut pk_l = 0.0f32;
            let mut pk_r = 0.0f32;
            for i in 0..n {
                let g = st.trim.next(trim_t);
                let (gh, gm, gl) = (st.hi.next(hi_t), st.mid.next(mid_t), st.low.next(low_t));
                l[i] = st.iso.tick(0, l[i] * g, gl, gm, gh);
                r[i] = st.iso.tick(1, r[i] * g, gl, gm, gh);
            }
            st.filter.process(l, r, p.filter[d].get().clamp(-1.0, 1.0));
            for i in 0..n {
                pk_l = pk_l.max(l[i].abs());
                pk_r = pk_r.max(r[i].abs());
                let cg = st.cue.next(cue_t);
                self.cl[i] += l[i] * cg;
                self.cr[i] += r[i] * cg;
                let g = st.fader.next(fader_t) * st.xf.next(xf_t);
                self.ml[i] += l[i] * g;
                self.mr[i] += r[i] * g;
            }
            self.shared.decks[d].peak_l.max(pk_l);
            self.shared.decks[d].peak_r.max(pk_r);
        }
        let m = p.master.get().clamp(0.0, 1.0);
        let mt = m * m * 1.25;
        for i in 0..n {
            let g = self.master.next(mt);
            self.ml[i] *= g;
            self.mr[i] *= g;
        }
        self.limiter.process(&mut self.ml[..n], &mut self.mr[..n], p.limiter.get() > 0.5);
        self.shared.limiter_gr.set(self.limiter.reduction);
        let mut pk_l = 0.0f32;
        let mut pk_r = 0.0f32;
        for i in 0..n {
            pk_l = pk_l.max(self.ml[i].abs());
            pk_r = pk_r.max(self.mr[i].abs());
        }
        self.shared.master_l.max(pk_l);
        self.shared.master_r.max(pk_r);
        // Auriculares: mezcla cue / master
        let mix_t = p.cue_mix.get().clamp(0.0, 1.0);
        let hl = p.hp_level.get().clamp(0.0, 1.0);
        let hp_t = hl * hl * 1.4;
        for i in 0..n {
            let mx = self.cue_mix.next(mix_t);
            let g = self.hp.next(hp_t);
            self.cl[i] = ((self.cl[i] * (1.0 - mx) + self.ml[i] * mx) * g).clamp(-1.0, 1.0);
            self.cr[i] = ((self.cr[i] * (1.0 - mx) + self.mr[i] * mx) * g).clamp(-1.0, 1.0);
        }
    }

    fn write_out(&self, out: &mut [f32], ch: usize, n: usize) {
        let routing = self.params.routing.get().round() as i32;
        for i in 0..n {
            let o = &mut out[i * ch..(i + 1) * ch];
            o.fill(0.0);
            if ch == 1 {
                o[0] = 0.5 * (self.ml[i] + self.mr[i]);
                continue;
            }
            if routing == 2 {
                // split: izquierda = auriculares (mono), derecha = master (mono)
                o[0] = 0.5 * (self.cl[i] + self.cr[i]);
                o[1] = 0.5 * (self.ml[i] + self.mr[i]);
                continue;
            }
            o[0] = self.ml[i];
            o[1] = self.mr[i];
            if routing == 1 && ch >= 4 {
                o[2] = self.cl[i];
                o[3] = self.cr[i];
            }
        }
    }

    fn publish(&mut self) {
        let sr = self.sr;
        for (d, sh) in self.decks.iter_mut().zip(self.shared.decks.iter()) {
            sh.pos.set(d.pos / sr);
            sh.speed.set(if d.playing { d.eff() } else { 0.0 });
            sh.duration.set(d.len / sr);
            sh.playing.store(d.playing, Relaxed);
            sh.rate.set(d.rate);
            sh.keylock.store(d.keylock, Relaxed);
            sh.stretching.store(d.playing && d.is_stretching(), Relaxed);
            sh.synced.store(d.synced, Relaxed);
            sh.bpm.set(d.bpm);
            sh.grid.set(d.grid / sr);
            match d.lp {
                Some(lp) => {
                    sh.loop_start.set(lp.start / sr);
                    sh.loop_end.set(lp.end / sr);
                    sh.loop_beats.set(lp.beats);
                }
                None => {
                    sh.loop_start.set(f64::NAN);
                    sh.loop_end.set(f64::NAN);
                    sh.loop_beats.set(0.0);
                }
            }
            sh.cue.set(d.cue / sr);
            for (i, h) in d.hot.iter().enumerate() {
                sh.hot[i].set(*h / sr);
            }
            if d.cues_changed {
                d.cues_changed = false;
                sh.cue_gen.fetch_add(1, Relaxed);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_renders_mix_without_nan() {
        let sr = 44100.0;
        let shared = Arc::new(Shared::default());
        let params = Arc::new(Params::default());
        let cmds = Arc::new(ArrayQueue::new(64));
        let garbage = Arc::new(ArrayQueue::new(8));
        let mut e = Engine::new(sr, shared.clone(), params.clone(), cmds.clone(), garbage);
        let n = (sr * 3.0) as usize;
        let tone: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.8).collect();
        let audio = Arc::new(TrackAudio { l: tone.clone(), r: tone });
        for d in 0..2 {
            let _ = cmds.push(Command::Load { deck: d, audio: audio.clone(), bpm: 120.0, grid: 0.0, cue: 0.0, hot: [f64::NAN; 8], gain: 1.0 });
            let _ = cmds.push(Command::Keylock(d, d == 1));
            let _ = cmds.push(Command::Tempo(d, 1.05));
            let _ = cmds.push(Command::Play(d));
        }
        let _ = cmds.push(Command::Sync(1, true));
        params.set("low0", -1.0);
        params.set("filter1", 0.5);
        params.set("cue0", 1.0);
        let mut out = vec![0.0f32; 4 * 300];
        let mut peak = 0.0f32;
        for _ in 0..300 {
            e.process(&mut out, 4);
            for v in &out {
                assert!(v.is_finite());
                peak = peak.max(v.abs());
            }
        }
        assert!(peak > 0.05 && peak <= 1.0, "peak {peak}");
        assert!(shared.decks[1].synced.load(Relaxed));
        assert!(shared.decks[0].pos.get() > 1.0);
    }
}
