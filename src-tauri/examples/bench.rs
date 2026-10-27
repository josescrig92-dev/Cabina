//! Mide el coste del motor: dos platos con keylock, EQ, filtro y limitador.
//! Uso: cargo run --release --example bench
use cabina_lib::engine::{Command, Engine, Params, Shared, TrackAudio};
use crossbeam_queue::ArrayQueue;
use std::sync::Arc;
use std::time::Instant;

fn main() {
    let sr = 44100.0;
    let n = (sr * 120.0) as usize;
    let x: Vec<f32> = (0..n).map(|i| ((i as f32 * 0.031).sin() + (i as f32 * 0.0071).sin()) * 0.3).collect();
    let audio = Arc::new(TrackAudio { l: x.clone(), r: x });
    for (label, keylock) in [("sin keylock (varispeed sinc)", false), ("con keylock en los dos platos", true)] {
        let cmds = Arc::new(ArrayQueue::new(64));
        let params = Arc::new(Params::default());
        let mut e = Engine::new(sr, Arc::new(Shared::default()), params.clone(), cmds.clone(), Arc::new(ArrayQueue::new(8)));
        for d in 0..2 {
            let _ = cmds.push(Command::Load { deck: d, audio: audio.clone(), bpm: 124.0, grid: 0.0, cue: 0.0, hot: [f64::NAN; 8], gain: 1.0 });
            let _ = cmds.push(Command::Keylock(d, keylock));
            let _ = cmds.push(Command::Tempo(d, 1.0 + 0.03 * (d as f64 + 1.0)));
            let _ = cmds.push(Command::Play(d));
        }
        params.set("filter0", 0.4);
        params.set("low1", -0.5);
        let buf = 256;
        let mut out = vec![0.0f32; buf * 2];
        let secs = 60.0;
        let t0 = Instant::now();
        for _ in 0..((secs * sr) as usize / buf) {
            e.process(&mut out, 2);
        }
        let el = t0.elapsed().as_secs_f64();
        println!("{label}: {secs} s de audio en {:.2} s → {:.1} % de un núcleo", el, el / secs * 100.0);
    }
}
