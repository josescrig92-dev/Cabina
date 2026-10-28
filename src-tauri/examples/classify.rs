//! Herramienta de desarrollo: clasifica archivos por estilo con Claude.
//! Uso: ANTHROPIC_API_KEY=sk-ant-... cargo run --release --example classify -- pista1.mp3 pista2.flac
use cabina_lib::{analysis, styles};

fn main() {
    let key = std::env::var("ANTHROPIC_API_KEY").expect("falta ANTHROPIC_API_KEY");
    let tracks: Vec<styles::TrackInput> = std::env::args()
        .skip(1)
        .enumerate()
        .filter_map(|(i, p)| {
            let path = std::path::Path::new(&p);
            let d = analysis::decode(path).ok()?;
            let a = analysis::analyze(&d.l, &d.r, d.sr);
            let (artist, title) = cabina_lib::library::names_from_file(path);
            Some(styles::TrackInput {
                id: i.to_string(),
                file: path.file_name()?.to_string_lossy().to_string(),
                title: d.tags.title.unwrap_or(title),
                artist: d.tags.artist.unwrap_or(artist),
                album: d.tags.album.unwrap_or_default(),
                genre: d.tags.genre.unwrap_or_default(),
                bpm: a.bpm,
                key: a.key,
                duration: a.duration,
            })
        })
        .collect();
    match styles::classify_batch(&key, &tracks) {
        Ok(r) => {
            for x in r {
                let t = &tracks[x.id.parse::<usize>().unwrap()];
                println!("{:<50} {:>6.1} BPM  →  {} ({:.0} %)", t.file, t.bpm, x.style, x.confidence * 100.0);
            }
        }
        Err(e) => eprintln!("Error: {e}"),
    }
}
