//! Herramienta de desarrollo: analiza archivos y muestra BPM, rejilla, tono y LUFS.
//! Uso: cargo run --release --example analyze -- archivo1.mp3 archivo2.flac ...
use std::time::Instant;

fn main() {
    for p in std::env::args().skip(1) {
        let t0 = Instant::now();
        match cabina_lib::analysis::decode(std::path::Path::new(&p)) {
            Ok(d) => {
                let t1 = t0.elapsed();
                let a = cabina_lib::analysis::analyze(&d.l, &d.r, d.sr);
                println!(
                    "{p}\n  {:?} | {} Hz | {:.1} s | BPM {:.2} | rejilla {:.4} s | tono {} ({}) | {:.1} LUFS | decodificar {:.0} ms, analizar {:.0} ms",
                    d.tags.title.as_deref().unwrap_or("-"),
                    d.sr,
                    a.duration,
                    a.bpm,
                    a.grid,
                    a.key.unwrap_or_default(),
                    a.key_name.unwrap_or_default(),
                    a.lufs,
                    t1.as_secs_f64() * 1000.0,
                    (t0.elapsed() - t1).as_secs_f64() * 1000.0
                );
            }
            Err(e) => println!("{p}: ERROR {e}"),
        }
    }
}
