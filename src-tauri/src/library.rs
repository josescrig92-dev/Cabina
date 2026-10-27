//! Colección de música: pistas, análisis guardado, cues y rejilla. Se guarda en
//! `library.json` dentro de la carpeta de datos de la aplicación; las ondas en `waves/`.

use crate::analysis::AUDIO_EXTENSIONS;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Track {
    pub id: String,
    pub path: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub duration: f64,
    #[serde(default)]
    pub bpm: f64,
    #[serde(default)]
    pub grid: f64,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub key_name: Option<String>,
    #[serde(default)]
    pub lufs: Option<f64>,
    #[serde(default)]
    pub cue: f64,
    #[serde(default = "empty_hot")]
    pub hot: Vec<Option<f64>>,
    #[serde(default)]
    pub added: u64,
    #[serde(default)]
    pub plays: u32,
    /// "pending" | "ready" | "error" | "missing"
    pub status: String,
    #[serde(default)]
    pub error: Option<String>,
}

fn empty_hot() -> Vec<Option<f64>> {
    vec![None; 8]
}

#[derive(Serialize, Deserialize, Default)]
struct LibraryFile {
    version: u32,
    tracks: Vec<Track>,
}

pub struct Library {
    pub dir: PathBuf,
    pub tracks: Vec<Track>,
    pub dirty: bool,
}

pub fn track_id(path: &str) -> String {
    // FNV-1a de 64 bits: estable entre ejecuciones
    let mut h: u64 = 0xcbf29ce484222325;
    for b in path.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

pub fn is_audio(p: &Path) -> bool {
    let hidden = p.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(true);
    !hidden && p.extension().and_then(|e| e.to_str()).map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str())).unwrap_or(false)
}

fn scan(p: &Path, out: &mut Vec<PathBuf>, depth: usize) {
    if depth > 24 {
        return;
    }
    if p.is_dir() {
        if let Ok(rd) = fs::read_dir(p) {
            let mut entries: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
            entries.sort();
            for e in entries {
                if e.file_name().and_then(|n| n.to_str()).map(|n| n.starts_with('.')).unwrap_or(false) {
                    continue;
                }
                scan(&e, out, depth + 1);
            }
        }
    } else if is_audio(p) {
        out.push(p.to_path_buf());
    }
}

/// "Artista - Título.mp3" -> (artista, título)
pub fn names_from_file(path: &Path) -> (String, String) {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("Sin título").replace('_', " ");
    match stem.split_once(" - ") {
        Some((a, t)) => (a.trim().to_string(), t.trim().to_string()),
        None => (String::new(), stem.trim().to_string()),
    }
}

impl Library {
    pub fn open(dir: PathBuf) -> Self {
        let _ = fs::create_dir_all(dir.join("waves"));
        let tracks = fs::read(dir.join("library.json"))
            .ok()
            .and_then(|b| serde_json::from_slice::<LibraryFile>(&b).ok())
            .map(|f| f.tracks)
            .unwrap_or_default();
        let mut lib = Self { dir, tracks, dirty: false };
        for t in lib.tracks.iter_mut() {
            if !Path::new(&t.path).exists() {
                t.status = "missing".into();
            } else if t.status == "missing" {
                t.status = if t.bpm > 0.0 { "ready".into() } else { "pending".into() };
            }
        }
        lib
    }

    pub fn save(&mut self) -> Result<(), String> {
        let file = LibraryFile { version: 1, tracks: self.tracks.clone() };
        let json = serde_json::to_vec(&file).map_err(|e| e.to_string())?;
        let tmp = self.dir.join("library.json.tmp");
        fs::write(&tmp, json).map_err(|e| e.to_string())?;
        fs::rename(&tmp, self.dir.join("library.json")).map_err(|e| e.to_string())?;
        self.dirty = false;
        Ok(())
    }

    pub fn wave_path(&self, id: &str) -> PathBuf {
        self.dir.join("waves").join(format!("{id}.wave"))
    }

    pub fn get(&self, id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Track> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Añade archivos o carpetas (recursivas). Devuelve las pistas nuevas.
    pub fn add_paths(&mut self, paths: &[PathBuf]) -> Vec<Track> {
        let mut files = vec![];
        for p in paths {
            scan(p, &mut files, 0);
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let mut added = vec![];
        for f in files {
            let path = f.to_string_lossy().to_string();
            let id = track_id(&path);
            if self.get(&id).is_some() {
                continue;
            }
            let (artist, title) = names_from_file(&f);
            let t = Track {
                id,
                path,
                title,
                artist,
                album: String::new(),
                genre: String::new(),
                duration: 0.0,
                bpm: 0.0,
                grid: 0.0,
                key: None,
                key_name: None,
                lufs: None,
                cue: 0.0,
                hot: empty_hot(),
                added: now,
                plays: 0,
                status: "pending".into(),
                error: None,
            };
            self.tracks.push(t.clone());
            added.push(t);
        }
        if !added.is_empty() {
            self.dirty = true;
        }
        added
    }

    pub fn remove(&mut self, ids: &[String]) {
        for id in ids {
            let _ = fs::remove_file(self.wave_path(id));
        }
        self.tracks.retain(|t| !ids.contains(&t.id));
        self.dirty = true;
    }
}

/// Formato del archivo de onda: "CBW1", u32 frecuencia, u32 longitud, luego graves|medios|agudos.
pub fn encode_wave(rate: u32, len: usize, data: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(12 + data.len());
    v.extend_from_slice(b"CBW1");
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(len as u32).to_le_bytes());
    v.extend_from_slice(data);
    v
}
