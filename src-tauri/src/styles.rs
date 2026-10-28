//! Estilos musicales: clasificación de pistas con Claude (a partir del nombre, las
//! etiquetas, el BPM y la tonalidad) y organización de los archivos en carpetas.

use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Estilos posibles. Los nombres son también los de las carpetas (sin "/" ni ":").
pub const STYLES: &[&str] = &[
    "House",
    "Deep House",
    "Tech House",
    "Progressive House",
    "Afro House",
    "Melodic House & Techno",
    "Minimal - Deep Tech",
    "Techno",
    "Hard Techno",
    "Trance",
    "Psytrance",
    "Drum & Bass",
    "Dubstep - Bass",
    "UK Garage",
    "Breaks",
    "Electro",
    "Big Room - EDM",
    "Hardstyle",
    "Disco - Nu Disco",
    "Funk - Soul",
    "Hip Hop",
    "R&B",
    "Reggaeton",
    "Latin",
    "Dancehall",
    "Afrobeats",
    "Pop",
    "Rock",
    "Indie - Alternative",
    "Ambient - Downtempo",
    "Jazz",
    "Classics",
];
pub const UNCLASSIFIED: &str = "Sin clasificar";

pub const MODEL: &str = "claude-opus-5-5";
pub const BATCH: usize = 40;

const SYSTEM: &str = "You sort the tracks of a DJ's music library into styles, so each track can go into one folder. \
For every track you get its file name, tags, duration, BPM and Camelot key. Use what you know about the artist, \
label, remixer and the track itself, together with the BPM and naming conventions (\"Extended Mix\", \"Original Mix\", \
\"Dub\", \"Bootleg\"), to choose the single style from the allowed list that a working DJ would file it under. \
The genre tag can be missing, too broad (\"Electronic\", \"Dance\") or wrong; trust your knowledge of the artist and \
track over it. Choose the specific style (for example Tech House rather than House) when you are reasonably sure, \
and the broader one when you are not. A remix belongs to the style of the remix, not of the original. \
\"Classics\" is for older dance and pop hits played as throwbacks. \
Use \"Sin clasificar\" only when there is nothing to go on, such as \"Track 01\" with no tags. \
confidence: about 0.9 when you recognize the track or artist and the style is clear, about 0.6 when you infer it \
from BPM, tags or naming, below 0.4 when you are guessing. Return every input id exactly once.";

#[derive(Debug, Clone)]
pub struct TrackInput {
    pub id: String,
    pub file: String,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub genre: String,
    pub bpm: f64,
    pub key: Option<String>,
    pub duration: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StyleResult {
    pub id: String,
    pub style: String,
    pub confidence: f64,
}

#[derive(Debug)]
pub enum ClassifyError {
    /// La clave de API no es válida o no tiene permiso.
    Auth(String),
    /// Error que no se arregla reintentando (petición rechazada, respuesta ilegible).
    Fatal(String),
    /// Error pasajero (límite de uso, servidor ocupado, red).
    Transient(String, Option<u64>),
}

impl std::fmt::Display for ClassifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClassifyError::Auth(m) | ClassifyError::Fatal(m) | ClassifyError::Transient(m, _) => f.write_str(m),
        }
    }
}

fn schema() -> Value {
    let mut allowed: Vec<&str> = STYLES.to_vec();
    allowed.push(UNCLASSIFIED);
    json!({
        "type": "object",
        "properties": {
            "tracks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string" },
                        "style": { "type": "string", "enum": allowed },
                        "confidence": { "type": "number" }
                    },
                    "required": ["id", "style", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["tracks"],
        "additionalProperties": false
    })
}

pub fn request_body(tracks: &[TrackInput]) -> Value {
    let items: Vec<Value> = tracks
        .iter()
        .map(|t| {
            json!({
                "id": t.id,
                "file": t.file,
                "title": t.title,
                "artist": t.artist,
                "album": t.album,
                "genre_tag": t.genre,
                "bpm": (t.bpm * 10.0).round() / 10.0,
                "key": t.key,
                "duration": format!("{}:{:02}", (t.duration / 60.0) as u32, (t.duration % 60.0) as u32),
            })
        })
        .collect();
    json!({
        "model": MODEL,
        "max_tokens": 16000,
        // si los filtros de seguridad rechazan un lote, la API lo repite con el modelo alternativo recomendado
        "fallbacks": "default",
        "output_config": { "effort": "low", "format": { "type": "json_schema", "schema": schema() } },
        "system": SYSTEM,
        "messages": [{ "role": "user", "content": serde_json::to_string(&items).unwrap_or_default() }]
    })
}

fn api_base() -> String {
    std::env::var("CABINA_API_BASE").unwrap_or_else(|_| "https://api.anthropic.com".into())
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(20))
        .timeout(Duration::from_secs(300))
        .try_proxy_from_env(true)
        .build()
}

/// Una petición para un lote. No reintenta: eso lo decide `classify_batch`.
fn call(api_key: &str, tracks: &[TrackInput]) -> Result<Vec<StyleResult>, ClassifyError> {
    let res = agent()
        .post(&format!("{}/v1/messages", api_base()))
        .set("x-api-key", api_key)
        .set("anthropic-version", "2026-10-21")
        .set("anthropic-beta", "server-side-fallback-2026-07-01")
        .set("content-type", "application/json")
        .send_json(request_body(tracks));
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let retry = r.header("retry-after").and_then(|v| v.trim().parse::<u64>().ok());
            let body = r.into_string().unwrap_or_default();
            let msg = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|v| v["error"]["message"].as_str().map(String::from))
                .unwrap_or(body);
            return Err(match code {
                401 | 403 => ClassifyError::Auth(format!("La clave de API no es válida o no tiene permiso ({code}): {msg}")),
                408 | 409 | 429 | 500..=599 => ClassifyError::Transient(format!("Servicio ocupado ({code}): {msg}"), retry),
                _ => ClassifyError::Fatal(format!("La API rechazó la petición ({code}): {msg}")),
            });
        }
        Err(e) => return Err(ClassifyError::Transient(format!("Sin conexión con la API: {e}"), None)),
    };
    let v: Value = resp.into_json().map_err(|e| ClassifyError::Transient(format!("Respuesta incompleta: {e}"), None))?;
    match v["stop_reason"].as_str() {
        Some("refusal") => return Err(ClassifyError::Fatal("El modelo no quiso clasificar este lote".into())),
        Some("max_tokens") => return Err(ClassifyError::Fatal("La respuesta se cortó por longitud".into())),
        _ => {}
    }
    let text: String = v["content"]
        .as_array()
        .map(|blocks| blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect())
        .unwrap_or_default();
    #[derive(Deserialize)]
    struct Out {
        tracks: Vec<StyleResult>,
    }
    let out: Out = serde_json::from_str(&text).map_err(|e| ClassifyError::Fatal(format!("Respuesta con formato inesperado: {e}")))?;
    let valid: Vec<StyleResult> = out
        .tracks
        .into_iter()
        .filter(|r| r.style == UNCLASSIFIED || STYLES.contains(&r.style.as_str()))
        .map(|mut r| {
            r.confidence = r.confidence.clamp(0.0, 1.0);
            r
        })
        .collect();
    Ok(valid)
}

/// Clasifica un lote, reintentando los errores pasajeros con espera creciente.
pub fn classify_batch(api_key: &str, tracks: &[TrackInput]) -> Result<Vec<StyleResult>, ClassifyError> {
    let mut wait = 2u64;
    for attempt in 0..5 {
        match call(api_key, tracks) {
            Err(ClassifyError::Transient(m, retry)) => {
                if attempt == 4 {
                    return Err(ClassifyError::Transient(m, retry));
                }
                std::thread::sleep(Duration::from_secs(retry.unwrap_or(wait).min(60)));
                wait *= 2;
            }
            other => return other,
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------------------
// Organización en carpetas
// ---------------------------------------------------------------------------

pub struct OrganizeItem {
    pub id: String,
    pub path: String,
    pub style: String,
}

#[derive(Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrganizeReport {
    pub dest: String,
    pub copied: usize,
    pub moved: usize,
    pub missing: usize,
    pub errors: Vec<String>,
    pub folders: Vec<(String, usize)>,
    /// Nuevas rutas de los archivos movidos (id -> ruta)
    #[serde(skip)]
    pub new_paths: HashMap<String, String>,
}

fn free_name(dir: &Path, file: &str) -> PathBuf {
    let p = dir.join(file);
    if !p.exists() {
        return p;
    }
    let (stem, ext) = match file.rsplit_once('.') {
        Some((s, e)) => (s.to_string(), format!(".{e}")),
        None => (file.to_string(), String::new()),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .unwrap()
}

/// Copia (o mueve) cada archivo a `dest/<estilo>/` y deja una lista `.m3u8` por carpeta.
pub fn organize(items: &[OrganizeItem], dest: &Path, move_files: bool, mut progress: impl FnMut(usize, usize, &str)) -> OrganizeReport {
    let mut rep = OrganizeReport { dest: dest.to_string_lossy().to_string(), ..Default::default() };
    let mut playlists: HashMap<String, Vec<String>> = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        progress(i, items.len(), &it.style);
        let src = Path::new(&it.path);
        if !src.exists() {
            rep.missing += 1;
            continue;
        }
        let dir = dest.join(&it.style);
        if let Err(e) = fs::create_dir_all(&dir) {
            rep.errors.push(format!("{}: {e}", dir.display()));
            continue;
        }
        let file = src.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_else(|| "pista".into());
        // si ya está en su carpeta (organizado antes), no se duplica
        let target = if src.parent() == Some(dir.as_path()) { src.to_path_buf() } else { free_name(&dir, &file) };
        let result = if target == src {
            Ok(())
        } else if move_files {
            fs::rename(src, &target).or_else(|_| fs::copy(src, &target).and_then(|_| fs::remove_file(src)))
        } else {
            fs::copy(src, &target).map(|_| ())
        };
        match result {
            Ok(()) => {
                if move_files {
                    rep.moved += 1;
                    rep.new_paths.insert(it.id.clone(), target.to_string_lossy().to_string());
                } else {
                    rep.copied += 1;
                }
                let name = target.file_name().unwrap().to_string_lossy().to_string();
                playlists.entry(it.style.clone()).or_default().push(name);
            }
            Err(e) => rep.errors.push(format!("{file}: {e}")),
        }
    }
    progress(items.len(), items.len(), "");
    let mut folders: Vec<(String, usize)> = playlists.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    folders.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (style, files) in &playlists {
        let mut m3u = String::from("#EXTM3U\n");
        for f in files {
            m3u.push_str(f);
            m3u.push('\n');
        }
        if let Err(e) = fs::write(dest.join(style).join(format!("{style}.m3u8")), m3u) {
            rep.errors.push(format!("{style}.m3u8: {e}"));
        }
    }
    rep.folders = folders;
    rep
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_and_body_are_well_formed() {
        let t = TrackInput {
            id: "a1".into(),
            file: "Fisher - Losing It (Original Mix).mp3".into(),
            title: "Losing It".into(),
            artist: "Fisher".into(),
            album: String::new(),
            genre: String::new(),
            bpm: 125.0,
            key: Some("4A".into()),
            duration: 380.0,
        };
        let b = request_body(&[t]);
        assert_eq!(b["model"], MODEL);
        assert_eq!(b["output_config"]["format"]["schema"]["properties"]["tracks"]["items"]["properties"]["style"]["enum"][0], "House");
        let content: Value = serde_json::from_str(b["messages"][0]["content"].as_str().unwrap()).unwrap();
        assert_eq!(content[0]["duration"], "6:20");
        assert!(STYLES.iter().all(|s| !s.contains('/') && !s.contains(':')));
    }

    #[test]
    fn organize_copies_into_style_folders_with_playlists() {
        let base = std::env::temp_dir().join(format!("cabina-org-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let src = base.join("src");
        fs::create_dir_all(&src).unwrap();
        for f in ["a.mp3", "b.mp3", "c.mp3"] {
            fs::write(src.join(f), f).unwrap();
        }
        // dos archivos con el mismo nombre en carpetas distintas
        fs::create_dir_all(src.join("otra")).unwrap();
        fs::write(src.join("otra/a.mp3"), "otra").unwrap();
        let it = |id: &str, p: &str, s: &str| OrganizeItem { id: id.into(), path: src.join(p).to_string_lossy().into(), style: s.into() };
        let items = vec![it("1", "a.mp3", "Techno"), it("2", "b.mp3", "Techno"), it("3", "c.mp3", "House"), it("4", "otra/a.mp3", "Techno")];
        let dest = base.join("dest");
        let rep = organize(&items, &dest, false, |_, _, _| {});
        assert_eq!(rep.copied, 4);
        assert!(dest.join("Techno/a.mp3").exists() && dest.join("Techno/a (2).mp3").exists());
        let m3u = fs::read_to_string(dest.join("Techno/Techno.m3u8")).unwrap();
        assert!(m3u.contains("a (2).mp3"));
        assert_eq!(rep.folders[0], ("Techno".to_string(), 3));
        // mover
        let rep = organize(&items[2..3], &base.join("movido"), true, |_, _, _| {});
        assert_eq!(rep.moved, 1);
        assert!(!src.join("c.mp3").exists());
        assert!(rep.new_paths["3"].ends_with("House/c.mp3"));
        let _ = fs::remove_dir_all(&base);
    }
}
