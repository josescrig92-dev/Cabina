//! Generación de loops con IA a partir de un texto. Cabina no lleva el modelo dentro:
//! habla con un servidor compatible con OBSIDIAN Neural GPU Server
//! (https://github.com/innermost47/obsidian-neural-gpu-server), que el DJ instala en su
//! Mac con Apple Silicon o en otro ordenador con GPU NVIDIA de su red.
//! API: `POST /process` con JSON {prompt, model, bpm, duration, key} devuelve un WAV;
//! `GET /` responde "Service OK".

use serde::Serialize;
use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DEFAULT_SERVER: &str = "http://localhost:8000";

/// Modelos del servidor. `bars`: el servidor genera siempre 8 compases y ajusta el BPM
/// a uno de `SNAP_BPM`; si no, la duración la decide el DJ (2–30 s).
#[derive(Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
pub struct LoopModel {
    pub id: &'static str,
    pub name: &'static str,
    pub hint: &'static str,
    pub bars: bool,
}

pub const MODELS: &[LoopModel] = &[
    LoopModel { id: "stable-audio-open-1.0", name: "Stable Audio Open 1.0", hint: "Versátil: baterías, percusión y texturas", bars: false },
    LoopModel { id: "stable-audio-3-medium", name: "Stable Audio 3 Medium", hint: "Pistas completas, stems aislados y efectos", bars: false },
    LoopModel { id: "foundation-1", name: "Foundation-1", hint: "Frases melódicas y armonías por etiquetas", bars: true },
    LoopModel { id: "audialab-edm-elements", name: "Audialab EDM Elements", hint: "Leads EDM, supersaws y plucks", bars: true },
    LoopModel { id: "rc-infinite-pianos", name: "RC Infinite Pianos", hint: "Piano de cola y piano eléctrico", bars: true },
    LoopModel { id: "rc-vocal-textures", name: "RC Vocal Textures", hint: "Coros y voces atmosféricas", bars: true },
    LoopModel { id: "sao-instrumental", name: "SAO Instrumental", hint: "Trap melódico, lofi e indie", bars: true },
    LoopModel { id: "stablebeat", name: "StableBeaT", hint: "Beats de trap y 808", bars: true },
    LoopModel { id: "gluten-v1", name: "gluten_v1", hint: "Motivos melódicos de trap en loop", bars: true },
];

/// BPM a los que el servidor ajusta los modelos de 8 compases.
pub const SNAP_BPM: &[f64] = &[100.0, 110.0, 120.0, 128.0, 130.0, 140.0, 150.0];
pub const MIN_DURATION: u32 = 2;
pub const MAX_DURATION: u32 = 30;

#[derive(Debug, Clone)]
pub struct LoopRequest {
    pub prompt: String,
    pub model: String,
    pub bpm: Option<f64>,
    pub duration: u32,
    /// Tonalidad en notación de Cabina ("Am", "F#"): se envía como "A minor", "F# major"
    pub key: Option<String>,
}

#[derive(Debug)]
pub struct LoopAudio {
    pub wav: Vec<u8>,
    /// BPM real del audio (el que pidió el DJ o el ajustado por el servidor)
    pub bpm: Option<f64>,
    pub seed: Option<i64>,
}

pub fn model(id: &str) -> Option<&'static LoopModel> {
    MODELS.iter().find(|m| m.id == id)
}

/// "Am" -> "A minor", "F#" -> "F# major"
pub fn key_text(key: &str) -> Option<String> {
    let k = key.trim();
    let (note, minor) = match k.strip_suffix('m') {
        Some(n) => (n, true),
        None => (k, false),
    };
    let ok = matches!(note.chars().next(), Some('A'..='G')) && note.len() <= 2 && note.chars().skip(1).all(|c| c == '#' || c == 'b');
    ok.then(|| format!("{note} {}", if minor { "minor" } else { "major" }))
}

/// Comprueba y normaliza la dirección del servidor.
pub fn normalize_server(url: &str) -> Result<String, String> {
    let u = url.trim().trim_end_matches('/');
    if u.is_empty() {
        return Ok(DEFAULT_SERVER.into());
    }
    if !(u.starts_with("http://") || u.starts_with("https://")) || u.len() < 10 {
        return Err("La dirección debe empezar por http:// o https://, por ejemplo http://localhost:8000".into());
    }
    Ok(u.to_string())
}

pub fn request_body(req: &LoopRequest) -> Value {
    let mut body = json!({
        "prompt": req.prompt.trim(),
        "model": req.model,
        "duration": req.duration.clamp(MIN_DURATION, MAX_DURATION),
    });
    if let Some(b) = req.bpm {
        body["bpm"] = json!(b.round() as i64);
    }
    if let Some(k) = req.key.as_deref().and_then(key_text) {
        body["key"] = json!(k);
    }
    body
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(timeout).build()
}

/// `GET /`: comprueba que el servidor responde.
pub fn check(server: &str) -> Result<(), String> {
    match agent(Duration::from_secs(8)).get(&format!("{server}/")).call() {
        Ok(r) => {
            let body = r.into_string().unwrap_or_default();
            if body.contains("OK") {
                Ok(())
            } else {
                Err(format!("{server} responde, pero no parece un servidor de OBSIDIAN Neural"))
            }
        }
        Err(ureq::Error::Status(code, _)) => Err(format!("{server} respondió con el error {code}")),
        Err(_) => Err(format!("No hay conexión con {server}. Comprueba que el servidor está arrancado y la dirección es correcta.")),
    }
}

fn error_detail(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    match &v["detail"] {
        Value::String(s) => Some(s.clone()),
        Value::Array(a) => a.first().and_then(|e| e["msg"].as_str()).map(String::from),
        _ => None,
    }
}

/// `POST /process`. Bloquea hasta que el servidor termina (de unos segundos a un par de minutos).
pub fn generate(server: &str, req: &LoopRequest) -> Result<LoopAudio, String> {
    let m = model(&req.model).ok_or("Modelo desconocido")?;
    if req.prompt.trim().is_empty() {
        return Err("Escribe qué quieres que suene".into());
    }
    if m.bars && req.bpm.is_none() {
        return Err(format!("{} necesita un BPM", m.name));
    }
    let res = agent(Duration::from_secs(600))
        .post(&format!("{server}/process"))
        .set("content-type", "application/json")
        .send_json(request_body(req));
    let resp = match res {
        Ok(r) => r,
        Err(ureq::Error::Status(code, r)) => {
            let detail = error_detail(&r.into_string().unwrap_or_default());
            let msg = match code {
                422 => "Petición no válida",
                503 => "El servidor está ocupado o no tiene ese modelo",
                500 => "El servidor falló al generar",
                _ => "Error del servidor",
            };
            return Err(match detail {
                Some(d) => format!("{msg} ({code}): {d}"),
                None => format!("{msg} ({code})"),
            });
        }
        Err(_) => return Err(format!("No hay conexión con {server}. Comprueba que el servidor está arrancado.")),
    };
    let header_f64 = |name: &str| resp.header(name).and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0);
    let bpm = header_f64("X-Snapped-BPM").or(req.bpm);
    let seed = resp.header("X-Seed").and_then(|v| v.trim().parse::<i64>().ok());
    let mut wav = Vec::new();
    std::io::Read::read_to_end(&mut resp.into_reader(), &mut wav).map_err(|e| format!("Se cortó la descarga del audio: {e}"))?;
    if wav.len() < 44 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err("El servidor no devolvió un archivo WAV".into());
    }
    Ok(LoopAudio { wav, bpm, seed })
}

/// Nombre de archivo "IA - <texto> (<bpm> BPM).wav", que la colección lee como artista "IA".
pub fn file_name(prompt: &str, bpm: Option<f64>, seed: Option<i64>) -> String {
    let mut title: String = prompt
        .chars()
        .map(|c| if c.is_alphanumeric() || " -,.'&+#()".contains(c) { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    title = title.replace(" - ", " ");
    if title.chars().count() > 60 {
        title = title.chars().take(60).collect::<String>().trim_end().to_string();
    }
    if title.is_empty() {
        title = "Loop".into();
    }
    let bpm = bpm.map(|b| format!(" ({} BPM)", b.round())).unwrap_or_default();
    let seed = seed.map(|s| format!(" #{}", s % 100_000)).unwrap_or_default();
    format!("IA - {title}{bpm}{seed}.wav")
}

/// Guarda el WAV en `dir` sin pisar archivos existentes.
pub fn save(dir: &Path, name: &str, wav: &[u8]) -> Result<PathBuf, String> {
    fs::create_dir_all(dir).map_err(|e| format!("No se pudo crear {}: {e}", dir.display()))?;
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, "wav"));
    let path = std::iter::once(dir.join(name))
        .chain((2..).map(|n| dir.join(format!("{stem} ({n}).{ext}"))))
        .find(|p| !p.exists())
        .unwrap();
    fs::write(&path, wav).map_err(|e| format!("No se pudo guardar el loop: {e}"))?;
    Ok(path)
}

/// El BPM que da el análisis de un loop corto puede salir a la mitad, al doble o con
/// algo de error; si se parece al BPM con el que se generó, manda este.
pub fn trust_bpm(analyzed: f64, generated: f64) -> Option<f64> {
    if !generated.is_finite() || !analyzed.is_finite() || generated <= 0.0 || analyzed <= 0.0 {
        return None;
    }
    [1.0, 0.5, 2.0, 2.0 / 3.0, 1.5]
        .iter()
        .any(|f| ((analyzed * f) / generated - 1.0).abs() < 0.04)
        .then_some(generated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_spelled_out() {
        assert_eq!(key_text("Am").as_deref(), Some("A minor"));
        assert_eq!(key_text("F#").as_deref(), Some("F# major"));
        assert_eq!(key_text("Ebm").as_deref(), Some("Eb minor"));
        assert_eq!(key_text("8A"), None);
        assert_eq!(key_text(""), None);
    }

    #[test]
    fn body_matches_server_schema() {
        let req = LoopRequest { prompt: "  deep house chord stabs ".into(), model: "foundation-1".into(), bpm: Some(124.4), duration: 99, key: Some("Am".into()) };
        let b = request_body(&req);
        assert_eq!(b["prompt"], "deep house chord stabs");
        assert_eq!(b["model"], "foundation-1");
        assert_eq!(b["bpm"], 124);
        assert_eq!(b["duration"], MAX_DURATION);
        assert_eq!(b["key"], "A minor");
        // el servidor rechaza campos desconocidos
        assert_eq!(b.as_object().unwrap().len(), 5);
        let req = LoopRequest { key: None, bpm: None, ..req };
        let b = request_body(&req);
        assert!(b.get("bpm").is_none() && b.get("key").is_none());
    }

    #[test]
    fn server_url() {
        assert_eq!(normalize_server(" http://192.168.1.20:8000/ ").unwrap(), "http://192.168.1.20:8000");
        assert_eq!(normalize_server("").unwrap(), DEFAULT_SERVER);
        assert!(normalize_server("localhost:8000").is_err());
    }

    #[test]
    fn file_names_are_safe() {
        let n = file_name("dark techno / rumble: kick?", Some(130.0), Some(1234567));
        assert_eq!(n, "IA - dark techno rumble kick (130 BPM) #34567.wav");
        assert!(!n.contains('/') && !n.contains(':'));
        assert_eq!(crate::library::names_from_file(Path::new(&n)).0, "IA");
        assert_eq!(file_name("a - b", None, None), "IA - a b.wav");
        assert_eq!(file_name("///", None, None), "IA - Loop.wav");
    }

    #[test]
    fn bpm_trust() {
        assert_eq!(trust_bpm(128.3, 128.0), Some(128.0));
        assert_eq!(trust_bpm(64.1, 128.0), Some(128.0));
        assert_eq!(trust_bpm(85.4, 128.0), Some(128.0));
        assert_eq!(trust_bpm(100.0, 128.0), None);
    }

    /// Servidor HTTP de una sola respuesta; devuelve su dirección y la petición recibida.
    fn mock(status: &str, headers: &str, body: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
        use std::io::{Read, Write};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let (status, headers) = (status.to_string(), headers.to_string());
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = vec![];
            let mut buf = [0u8; 4096];
            // cabecera y cuerpo JSON (Content-Length)
            loop {
                let n = s.read(&mut buf).unwrap();
                req.extend_from_slice(&buf[..n]);
                let txt = String::from_utf8_lossy(&req).to_string();
                if let Some(i) = txt.find("\r\n\r\n") {
                    let len = txt.lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    if req.len() >= i + 4 + len {
                        break;
                    }
                }
            }
            write!(s, "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n", body.len()).unwrap();
            s.write_all(&body).unwrap();
            String::from_utf8_lossy(&req).to_string()
        });
        (addr, h)
    }

    #[test]
    fn generate_against_server() {
        let mut wav = b"RIFF\0\0\0\0WAVEfmt ".to_vec();
        wav.resize(2000, 0);
        let (addr, h) = mock("200 OK", "Content-Type: audio/wav\r\nX-Snapped-BPM: 130\r\nX-Seed: 42\r\n", wav.clone());
        let req = LoopRequest { prompt: "acid bassline".into(), model: "stablebeat".into(), bpm: Some(128.0), duration: 10, key: Some("Am".into()) };
        let a = generate(&addr, &req).unwrap();
        assert_eq!(a.wav, wav);
        assert_eq!(a.bpm, Some(130.0));
        assert_eq!(a.seed, Some(42));
        let sent = h.join().unwrap();
        assert!(sent.starts_with("POST /process "), "{sent}");
        let body: Value = serde_json::from_str(&sent[sent.find("\r\n\r\n").unwrap() + 4..]).unwrap();
        assert_eq!(body, request_body(&req));

        let (addr, _h) = mock("503 Service Unavailable", "Content-Type: application/json\r\n", br#"{"detail":"Already generating"}"#.to_vec());
        let e = generate(&addr, &req).unwrap_err();
        assert!(e.contains("503") && e.contains("Already generating"), "{e}");

        let (addr, _h) = mock("200 OK", "", b"not audio".to_vec());
        assert!(generate(&addr, &req).unwrap_err().contains("WAV"));

        let (addr, _h) = mock("200 OK", "", b"Service OK".to_vec());
        assert!(check(&addr).is_ok());
        assert!(check("http://127.0.0.1:9").is_err());
    }

    #[test]
    fn save_does_not_overwrite() {
        let dir = std::env::temp_dir().join(format!("cabina-loops-{}", std::process::id()));
        let wav = b"RIFF0000WAVE".to_vec();
        let a = save(&dir, "IA - x.wav", &wav).unwrap();
        let b = save(&dir, "IA - x.wav", &wav).unwrap();
        assert_ne!(a, b);
        assert!(b.to_string_lossy().ends_with("IA - x (2).wav"));
        let _ = fs::remove_dir_all(&dir);
    }
}
