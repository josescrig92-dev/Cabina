pub mod analysis;
pub mod engine;
pub mod library;
pub mod loops;
pub mod styles;

use engine::host::{list_devices, AudioHost, HostStatus, BUFFER_SIZES};
use engine::{now_ns, Command, TrackAudio};
use library::{encode_wave, Library, Track};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tauri::{Emitter, Manager};

/// Sonoridad de referencia para la ganancia automática.
const TARGET_LUFS: f64 = -12.0;

#[derive(Serialize, Deserialize, Default, Clone)]
struct Settings {
    device: Option<String>,
    buffer: Option<u32>,
    #[serde(default)]
    api_key: Option<String>,
    /// Servidor de generación de loops (compatible con OBSIDIAN Neural GPU Server)
    #[serde(default)]
    loop_server: Option<String>,
}

fn save_settings(dir: &std::path::Path, s: &Settings) {
    if let Ok(json) = serde_json::to_vec_pretty(s) {
        let path = dir.join("settings.json");
        let _ = std::fs::write(&path, json);
        // contiene la clave de API: solo legible por el usuario
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
    }
}

struct AppState {
    host: AudioHost,
    lib: Arc<Mutex<Library>>,
    jobs: Sender<String>,
    loaded: Arc<Mutex<[Option<String>; 2]>>,
    settings: Mutex<Settings>,
    data_dir: PathBuf,
    /// Hay una clasificación u organización en marcha
    busy: Arc<std::sync::atomic::AtomicBool>,
    /// Hay un loop generándose (el servidor solo atiende uno a la vez)
    generating: Arc<std::sync::atomic::AtomicBool>,
    /// Carpeta donde se guardan los loops generados
    loops_dir: PathBuf,
}

// ---------------------------------------------------------------------------
// Análisis en segundo plano
// ---------------------------------------------------------------------------

fn analysis_worker(app: tauri::AppHandle, lib: Arc<Mutex<Library>>, rx: Arc<Mutex<Receiver<String>>>) {
    loop {
        let id = match rx.lock().unwrap().recv() {
            Ok(id) => id,
            Err(_) => return,
        };
        let path = match lib.lock().unwrap().get(&id) {
            Some(t) => t.path.clone(),
            None => continue,
        };
        let result = analysis::decode(std::path::Path::new(&path)).map(|d| {
            let a = analysis::analyze(&d.l, &d.r, d.sr);
            (d.tags, a)
        });
        let mut l = lib.lock().unwrap();
        let wave_path = l.wave_path(&id);
        let Some(t) = l.get_mut(&id) else { continue };
        match result {
            Ok((tags, a)) => {
                if let Some(v) = tags.title {
                    t.title = v;
                }
                if let Some(v) = tags.artist {
                    t.artist = v;
                }
                if let Some(v) = tags.album {
                    t.album = v;
                }
                if let Some(v) = tags.genre {
                    t.genre = v;
                }
                t.duration = a.duration;
                t.bpm = a.bpm;
                t.grid = a.grid;
                if let Some(b) = t.gen_bpm.and_then(|g| loops::trust_bpm(a.bpm, g)) {
                    // los loops generados empiezan en un beat: si la rejilla cae casi en 0, se deja en 0
                    let beat = 60.0 / b;
                    let g = a.grid.rem_euclid(beat);
                    t.bpm = b;
                    t.grid = if g < 0.08 * beat || g > 0.92 * beat { 0.0 } else { g };
                }
                // el cue empieza en el primer beat de la rejilla
                if t.cue == 0.0 {
                    t.cue = t.grid;
                }
                t.key = a.key;
                t.key_name = a.key_name;
                t.lufs = Some(a.lufs);
                t.status = "ready".into();
                t.error = None;
                let _ = std::fs::write(&wave_path, encode_wave(analysis::WAVE_RATE as u32, a.wave_len, &a.wave));
            }
            Err(e) => {
                t.status = "error".into();
                t.error = Some(e);
            }
        }
        let t = t.clone();
        l.dirty = true;
        drop(l);
        let _ = app.emit("track-updated", &t);
    }
}

// ---------------------------------------------------------------------------
// Comandos: colección
// ---------------------------------------------------------------------------

#[tauri::command]
fn library_list(state: tauri::State<AppState>) -> Vec<Track> {
    state.lib.lock().unwrap().tracks.clone()
}

#[tauri::command]
fn library_add(state: tauri::State<AppState>, paths: Vec<String>) -> Vec<Track> {
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let added = state.lib.lock().unwrap().add_paths(&paths);
    for t in &added {
        let _ = state.jobs.send(t.id.clone());
    }
    added
}

#[tauri::command]
fn library_remove(state: tauri::State<AppState>, ids: Vec<String>) {
    state.lib.lock().unwrap().remove(&ids);
}

#[tauri::command]
fn library_reanalyze(state: tauri::State<AppState>, ids: Vec<String>) {
    let mut l = state.lib.lock().unwrap();
    for id in ids {
        if let Some(t) = l.get_mut(&id) {
            t.status = "pending".into();
            let _ = state.jobs.send(id);
        }
    }
    l.dirty = true;
}

#[tauri::command]
fn track_waveform(state: tauri::State<AppState>, id: String) -> Result<tauri::ipc::Response, String> {
    let path = state.lib.lock().unwrap().wave_path(&id);
    std::fs::read(path).map(tauri::ipc::Response::new).map_err(|_| "Onda no disponible".to_string())
}

#[tauri::command]
fn track_save_cues(state: tauri::State<AppState>, id: String, cue: f64, hot: Vec<Option<f64>>) {
    let mut l = state.lib.lock().unwrap();
    if let Some(t) = l.get_mut(&id) {
        t.cue = cue;
        t.hot = hot;
        t.hot.resize(8, None);
        l.dirty = true;
    }
}

#[tauri::command]
fn track_set_grid(state: tauri::State<AppState>, id: String, bpm: f64, grid: f64) -> Result<Track, String> {
    if !(20.0..=400.0).contains(&bpm) {
        return Err("BPM fuera de rango".into());
    }
    let t = {
        let mut l = state.lib.lock().unwrap();
        let t = l.get_mut(&id).ok_or("Pista no encontrada")?;
        t.bpm = bpm;
        t.grid = grid;
        let t = t.clone();
        l.dirty = true;
        t
    };
    let loaded = state.loaded.lock().unwrap().clone();
    for (d, lid) in loaded.iter().enumerate() {
        if lid.as_deref() == Some(id.as_str()) {
            state.host.send(Command::Grid(d, bpm, grid));
        }
    }
    Ok(t)
}

// ---------------------------------------------------------------------------
// Comandos: platos y mezclador
// ---------------------------------------------------------------------------

#[tauri::command]
fn load_track(app: tauri::AppHandle, state: tauri::State<AppState>, deck: usize, id: String) -> Result<(), String> {
    if deck > 1 {
        return Err("Plato no válido".into());
    }
    if state.host.shared.decks[deck].playing.load(Relaxed) {
        return Err(format!("El plato {} está sonando. Páralo antes de cargar otra pista.", deck + 1));
    }
    let track = state.lib.lock().unwrap().get(&id).cloned().ok_or("Pista no encontrada")?;
    if track.status != "ready" {
        return Err("La pista aún no está analizada".into());
    }
    let sr = state.host.shared.sample_rate.load(Relaxed);
    let cmds = state.host.cmds.clone();
    let loaded = state.loaded.clone();
    let lib = state.lib.clone();
    let _ = app.emit("deck-loading", serde_json::json!({ "deck": deck, "id": id }));
    thread::spawn(move || {
        let res = analysis::decode(std::path::Path::new(&track.path)).map(|d| {
            let (l, r) = if d.sr == sr {
                (d.l, d.r)
            } else {
                let (dl, dr) = (d.l, d.r);
                thread::scope(|s| {
                    let a = s.spawn(|| engine::dsp::resample(&dl, d.sr, sr));
                    let b = s.spawn(|| engine::dsp::resample(&dr, d.sr, sr));
                    (a.join().unwrap(), b.join().unwrap())
                })
            };
            TrackAudio { l, r }
        });
        match res {
            Ok(audio) => {
                let mut hot = [f64::NAN; 8];
                for (i, h) in track.hot.iter().take(8).enumerate() {
                    if let Some(v) = h {
                        hot[i] = *v;
                    }
                }
                let gain_db = track.lufs.map(|l| (TARGET_LUFS - l).clamp(-12.0, 9.0)).unwrap_or(0.0);
                let gain = 10f64.powf(gain_db / 20.0) as f32;
                let cmd = Command::Load { deck, audio: Arc::new(audio), bpm: track.bpm, grid: track.grid, cue: track.cue, hot, gain };
                if cmds.push(cmd).is_err() {
                    let _ = app.emit("deck-error", serde_json::json!({ "deck": deck, "message": "El motor está ocupado" }));
                    return;
                }
                loaded.lock().unwrap()[deck] = Some(track.id.clone());
                if let Some(t) = lib.lock().unwrap().get_mut(&track.id) {
                    t.plays += 1;
                }
                let _ = app.emit("deck-loaded", serde_json::json!({ "deck": deck, "id": track.id }));
            }
            Err(e) => {
                let _ = app.emit("deck-error", serde_json::json!({ "deck": deck, "message": e }));
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn deck(state: tauri::State<AppState>, deck: usize, action: String, value: Option<f64>) -> Result<(), String> {
    if deck > 1 {
        return Err("Plato no válido".into());
    }
    let v = value.unwrap_or(0.0);
    let cmd = match action.as_str() {
        "play" => Command::Play(deck),
        "pause" => Command::Pause(deck),
        "toggle" => Command::Toggle(deck),
        "cue_down" => Command::CueDown(deck),
        "cue_up" => Command::CueUp(deck),
        "seek" => Command::Seek(deck, v),
        "scrub" => Command::Scrub(deck, v),
        "hot" => Command::HotCue(deck, v as usize),
        "hot_clear" => Command::HotCueClear(deck, v as usize),
        "loop" => Command::Loop(deck, v),
        "loop_exit" => Command::LoopExit(deck),
        "loop_half" => Command::LoopResize(deck, 0.5),
        "loop_double" => Command::LoopResize(deck, 2.0),
        "jump" => Command::BeatJump(deck, v),
        "tempo" => Command::Tempo(deck, v),
        "bend" => Command::Bend(deck, v),
        "keylock" => Command::Keylock(deck, v > 0.5),
        "sync" => Command::Sync(deck, v > 0.5),
        "unload" => {
            state.loaded.lock().unwrap()[deck] = None;
            Command::Unload(deck)
        }
        _ => return Err(format!("Acción desconocida: {action}")),
    };
    state.host.send(cmd);
    Ok(())
}

#[tauri::command]
fn set_param(state: tauri::State<AppState>, name: String, value: f32) -> Result<(), String> {
    if state.host.params.set(&name, value) {
        Ok(())
    } else {
        Err(format!("Parámetro desconocido: {name}"))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeckState {
    track_id: Option<String>,
    pos: f64,
    speed: f64,
    duration: f64,
    playing: bool,
    rate: f64,
    keylock: bool,
    stretching: bool,
    synced: bool,
    loop_start: Option<f64>,
    loop_end: Option<f64>,
    loop_beats: f64,
    cue: f64,
    hot: Vec<Option<f64>>,
    bpm: f64,
    grid: f64,
    cue_gen: u64,
    load_gen: u64,
    peak: [f32; 2],
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EngineState {
    decks: Vec<DeckState>,
    master: [f32; 2],
    limiter: f32,
    dsp_load: f32,
    sample_rate: u32,
    channels: u32,
    buffer: u32,
    latency_ms: f64,
    engine_gen: u64,
}

fn opt(v: f64) -> Option<f64> {
    if v.is_nan() {
        None
    } else {
        Some(v)
    }
}

#[tauri::command]
fn engine_state(state: tauri::State<AppState>) -> EngineState {
    let sh = &state.host.shared;
    let loaded = state.loaded.lock().unwrap().clone();
    let since = now_ns().saturating_sub(sh.cb_time.load(Relaxed)) as f64 / 1e9;
    let latency = sh.latency_ns.load(Relaxed) as f64 / 1e9;
    let decks = sh
        .decks
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let speed = d.speed.get();
            // posición que se oye ahora mismo: extrapola desde el último callback y descuenta la latencia de salida
            let mut pos = d.pos.get() + (since.min(0.1) - latency) * speed;
            let (ls, le) = (d.loop_start.get(), d.loop_end.get());
            if !ls.is_nan() && pos >= le {
                pos = ls + (pos - ls).rem_euclid(le - ls);
            }
            DeckState {
                track_id: if d.duration.get() > 0.0 { loaded[i].clone() } else { None },
                pos: pos.max(0.0),
                speed,
                duration: d.duration.get(),
                playing: d.playing.load(Relaxed),
                rate: d.rate.get(),
                keylock: d.keylock.load(Relaxed),
                stretching: d.stretching.load(Relaxed),
                synced: d.synced.load(Relaxed),
                loop_start: opt(ls),
                loop_end: opt(le),
                loop_beats: d.loop_beats.get(),
                cue: d.cue.get(),
                hot: d.hot.iter().map(|h| opt(h.get())).collect(),
                bpm: d.bpm.get(),
                grid: d.grid.get(),
                cue_gen: d.cue_gen.load(Relaxed),
                load_gen: d.load_gen.load(Relaxed),
                peak: [d.peak_l.take(), d.peak_r.take()],
            }
        })
        .collect();
    EngineState {
        decks,
        master: [sh.master_l.take(), sh.master_r.take()],
        limiter: sh.limiter_gr.get(),
        dsp_load: sh.dsp_load.get(),
        sample_rate: sh.sample_rate.load(Relaxed),
        channels: sh.channels.load(Relaxed),
        buffer: sh.buffer.load(Relaxed),
        latency_ms: latency * 1000.0,
        engine_gen: sh.engine_gen.load(Relaxed),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AudioInfo {
    devices: Vec<String>,
    status: HostStatus,
    buffers: Vec<u32>,
    selected: Option<String>,
}

#[tauri::command]
fn audio_info(state: tauri::State<AppState>) -> AudioInfo {
    AudioInfo {
        devices: list_devices(),
        status: state.host.status.lock().unwrap().clone(),
        buffers: BUFFER_SIZES.to_vec(),
        selected: state.settings.lock().unwrap().device.clone(),
    }
}

#[tauri::command]
fn audio_configure(state: tauri::State<AppState>, device: Option<String>, buffer: u32) {
    let s = {
        let mut s = state.settings.lock().unwrap();
        s.device = device.clone();
        s.buffer = Some(buffer);
        s.clone()
    };
    save_settings(&state.data_dir, &s);
    state.host.configure(device, buffer);
}

// ---------------------------------------------------------------------------
// Comandos: estilos y organización en carpetas
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StylesInfo {
    styles: Vec<&'static str>,
    unclassified: &'static str,
    has_key: bool,
    busy: bool,
    batch: usize,
}

#[tauri::command]
fn styles_info(state: tauri::State<AppState>) -> StylesInfo {
    StylesInfo {
        styles: styles::STYLES.to_vec(),
        unclassified: styles::UNCLASSIFIED,
        has_key: state.settings.lock().unwrap().api_key.as_deref().is_some_and(|k| !k.is_empty()),
        busy: state.busy.load(Relaxed),
        batch: styles::BATCH,
    }
}

#[tauri::command]
fn set_api_key(state: tauri::State<AppState>, key: String) -> Result<(), String> {
    let key = key.trim().to_string();
    if !key.is_empty() && !key.starts_with("sk-ant-") {
        return Err("La clave debe empezar por «sk-ant-». Cópiala entera desde la consola de Anthropic.".into());
    }
    let s = {
        let mut s = state.settings.lock().unwrap();
        s.api_key = if key.is_empty() { None } else { Some(key) };
        s.clone()
    };
    save_settings(&state.data_dir, &s);
    Ok(())
}

#[tauri::command]
fn set_track_style(state: tauri::State<AppState>, id: String, style: Option<String>) -> Result<Track, String> {
    if let Some(st) = &style {
        if st != styles::UNCLASSIFIED && !styles::STYLES.contains(&st.as_str()) {
            return Err("Estilo desconocido".into());
        }
    }
    let mut l = state.lib.lock().unwrap();
    let t = l.get_mut(&id).ok_or("Pista no encontrada")?;
    t.style_confidence = style.as_ref().map(|_| 1.0);
    t.style = style;
    let t = t.clone();
    l.dirty = true;
    Ok(t)
}

/// Clasifica por estilo las pistas indicadas (o todas las analizadas que aún no tienen estilo).
/// Devuelve cuántas se van a clasificar; el progreso llega con el evento `styles-progress`.
#[tauri::command]
fn classify_styles(app: tauri::AppHandle, state: tauri::State<AppState>, ids: Option<Vec<String>>) -> Result<usize, String> {
    let key = state.settings.lock().unwrap().api_key.clone().filter(|k| !k.is_empty()).ok_or("Falta la clave de API de Anthropic")?;
    if state.busy.swap(true, Relaxed) {
        return Err("Ya hay una clasificación u organización en marcha".into());
    }
    let inputs: Vec<styles::TrackInput> = {
        let l = state.lib.lock().unwrap();
        l.tracks
            .iter()
            .filter(|t| t.status == "ready")
            .filter(|t| match &ids {
                Some(ids) => ids.contains(&t.id),
                None => t.style.is_none(),
            })
            .map(|t| styles::TrackInput {
                id: t.id.clone(),
                file: std::path::Path::new(&t.path).file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default(),
                title: t.title.clone(),
                artist: t.artist.clone(),
                album: t.album.clone(),
                genre: t.genre.clone(),
                bpm: t.bpm,
                key: t.key.clone(),
                duration: t.duration,
            })
            .collect()
    };
    let total = inputs.len();
    if total == 0 {
        state.busy.store(false, Relaxed);
        return Ok(0);
    }
    let lib = state.lib.clone();
    let busy = state.busy.clone();
    thread::spawn(move || {
        let batches: Vec<Vec<styles::TrackInput>> = inputs.chunks(styles::BATCH).map(|c| c.to_vec()).collect();
        let queue = Arc::new(Mutex::new(batches.into_iter()));
        let done = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fatal: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let errors = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _ = app.emit("styles-progress", serde_json::json!({ "done": 0, "total": total }));
        // varios lotes a la vez: cada respuesta tarda unos segundos
        thread::scope(|s| {
            for _ in 0..4 {
                let (queue, done, fatal, errors, lib, app, key) = (&queue, &done, &fatal, &errors, &lib, &app, &key);
                s.spawn(move || loop {
                    if fatal.lock().unwrap().is_some() {
                        return;
                    }
                    let Some(batch) = queue.lock().unwrap().next() else { return };
                    match styles::classify_batch(key, &batch) {
                        Ok(results) => {
                            let mut updated = vec![];
                            {
                                let mut l = lib.lock().unwrap();
                                for r in results {
                                    if let Some(t) = l.get_mut(&r.id) {
                                        t.style = Some(r.style);
                                        t.style_confidence = Some(r.confidence);
                                        updated.push(t.clone());
                                    }
                                }
                                l.dirty = true;
                            }
                            for t in &updated {
                                let _ = app.emit("track-updated", t);
                            }
                        }
                        Err(styles::ClassifyError::Auth(m)) => {
                            *fatal.lock().unwrap() = Some(m);
                            return;
                        }
                        Err(e) => {
                            errors.fetch_add(batch.len(), Relaxed);
                            eprintln!("clasificación: {e}");
                            let _ = app.emit("styles-error", e.to_string());
                        }
                    }
                    let d = done.fetch_add(batch.len(), Relaxed) + batch.len();
                    let _ = app.emit("styles-progress", serde_json::json!({ "done": d, "total": total }));
                });
            }
        });
        let err = fatal.lock().unwrap().clone();
        let _ = app.emit(
            "styles-done",
            serde_json::json!({ "total": total, "failed": errors.load(Relaxed), "error": err }),
        );
        busy.store(false, Relaxed);
    });
    Ok(total)
}

/// Copia o mueve las pistas a `dest/<estilo>/`. Devuelve enseguida; el resultado llega con `organize-done`.
#[tauri::command]
fn organize_folders(app: tauri::AppHandle, state: tauri::State<AppState>, dest: String, move_files: bool, ids: Option<Vec<String>>) -> Result<usize, String> {
    let dest = PathBuf::from(dest);
    if !dest.is_dir() {
        return Err("La carpeta de destino no existe".into());
    }
    let loaded = state.loaded.lock().unwrap().clone();
    let items: Vec<styles::OrganizeItem> = {
        let l = state.lib.lock().unwrap();
        l.tracks
            .iter()
            .filter(|t| t.status == "ready" || t.status == "missing")
            .filter(|t| ids.as_ref().map_or(true, |ids| ids.contains(&t.id)))
            // no se mueve una pista que está cargada en un plato
            .filter(|t| !move_files || !loaded.iter().any(|l| l.as_deref() == Some(t.id.as_str())))
            .map(|t| styles::OrganizeItem {
                id: t.id.clone(),
                path: t.path.clone(),
                style: t.style.clone().unwrap_or_else(|| styles::UNCLASSIFIED.to_string()),
            })
            .collect()
    };
    if items.is_empty() {
        return Err("No hay pistas que organizar".into());
    }
    if state.busy.swap(true, Relaxed) {
        return Err("Ya hay una clasificación u organización en marcha".into());
    }
    let n = items.len();
    let lib = state.lib.clone();
    let busy = state.busy.clone();
    thread::spawn(move || {
        let mut last = std::time::Instant::now();
        let rep = styles::organize(&items, &dest, move_files, |done, total, style| {
            if last.elapsed().as_millis() > 100 || done == total {
                last = std::time::Instant::now();
                let _ = app.emit("organize-progress", serde_json::json!({ "done": done, "total": total, "style": style }));
            }
        });
        if !rep.new_paths.is_empty() {
            let mut l = lib.lock().unwrap();
            for (id, p) in &rep.new_paths {
                if let Some(t) = l.get_mut(id) {
                    t.path = p.clone();
                }
            }
            l.dirty = true;
            let tracks = l.tracks.clone();
            drop(l);
            let _ = app.emit("library-reload", tracks.len());
        }
        let _ = app.emit("organize-done", &rep);
        busy.store(false, Relaxed);
    });
    Ok(n)
}

// ---------------------------------------------------------------------------
// Comandos: loops generados con IA
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LoopsInfo {
    server: String,
    models: &'static [loops::LoopModel],
    snap_bpm: &'static [f64],
    min_duration: u32,
    max_duration: u32,
    busy: bool,
    dir: String,
}

fn loop_server(state: &AppState) -> String {
    state.settings.lock().unwrap().loop_server.clone().unwrap_or_else(|| loops::DEFAULT_SERVER.into())
}

#[tauri::command]
fn loops_info(state: tauri::State<AppState>) -> LoopsInfo {
    LoopsInfo {
        server: loop_server(&state),
        models: loops::MODELS,
        snap_bpm: loops::SNAP_BPM,
        min_duration: loops::MIN_DURATION,
        max_duration: loops::MAX_DURATION,
        busy: state.generating.load(Relaxed),
        dir: state.loops_dir.to_string_lossy().to_string(),
    }
}

/// Guarda la dirección del servidor y comprueba que responde.
#[tauri::command]
async fn set_loop_server(state: tauri::State<'_, AppState>, url: String) -> Result<String, String> {
    let url = loops::normalize_server(&url)?;
    let s = {
        let mut s = state.settings.lock().unwrap();
        s.loop_server = Some(url.clone());
        s.clone()
    };
    save_settings(&state.data_dir, &s);
    let u = url.clone();
    tauri::async_runtime::spawn_blocking(move || loops::check(&u)).await.map_err(|e| e.to_string())??;
    Ok(url)
}

/// Pide un loop al servidor. Devuelve enseguida; el resultado llega con `loop-done` o `loop-error`.
#[tauri::command]
fn generate_loop(app: tauri::AppHandle, state: tauri::State<AppState>, prompt: String, model: String, bpm: Option<f64>, duration: u32, key: Option<String>) -> Result<(), String> {
    let m = loops::model(&model).ok_or("Modelo desconocido")?;
    if prompt.trim().is_empty() {
        return Err("Escribe qué quieres que suene".into());
    }
    if let Some(b) = bpm {
        if !(40.0..=250.0).contains(&b) {
            return Err("BPM fuera de rango".into());
        }
    }
    if state.generating.swap(true, Relaxed) {
        return Err("Ya se está generando un loop".into());
    }
    let req = loops::LoopRequest { prompt, model: m.id.into(), bpm, duration, key };
    let server = loop_server(&state);
    let (lib, jobs, dir, generating) = (state.lib.clone(), state.jobs.clone(), state.loops_dir.clone(), state.generating.clone());
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let res = loops::generate(&server, &req).and_then(|a| {
            let name = loops::file_name(&req.prompt, a.bpm, a.seed);
            loops::save(&dir, &name, &a.wav).map(|p| (p, a.bpm))
        });
        match res {
            Ok((path, bpm)) => {
                let added = {
                    let mut l = lib.lock().unwrap();
                    let mut added = l.add_paths(&[path]);
                    if let Some(t) = added.first_mut() {
                        t.gen_bpm = bpm;
                        t.genre = "Loop IA".into();
                        if let Some(lt) = l.get_mut(&t.id) {
                            lt.gen_bpm = bpm;
                            lt.genre = t.genre.clone();
                        }
                    }
                    added
                };
                match added.into_iter().next() {
                    Some(t) => {
                        let _ = jobs.send(t.id.clone());
                        let _ = app.emit("loop-done", serde_json::json!({ "track": t, "seconds": started.elapsed().as_secs_f64() }));
                    }
                    None => {
                        let _ = app.emit("loop-error", "El loop se guardó, pero no se pudo añadir a la colección");
                    }
                }
            }
            Err(e) => {
                let _ = app.emit("loop-error", e);
            }
        }
        generating.store(false, Relaxed);
    });
    Ok(())
}

#[tauri::command]
fn app_version(app: tauri::AppHandle) -> String {
    app.package_info().version.to_string()
}

// ---------------------------------------------------------------------------

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let data_dir = app.path().app_data_dir().unwrap_or_else(|_| std::env::temp_dir().join("cabina"));
            std::fs::create_dir_all(&data_dir)?;
            let settings: Settings = std::fs::read(data_dir.join("settings.json"))
                .ok()
                .and_then(|b| serde_json::from_slice(&b).ok())
                .unwrap_or_default();
            let loops_dir = app
                .path()
                .audio_dir()
                .map(|d| d.join("Cabina").join("Loops IA"))
                .unwrap_or_else(|_| data_dir.join("loops"));
            let host = AudioHost::start(settings.device.clone(), settings.buffer.unwrap_or(256));
            let lib = Arc::new(Mutex::new(Library::open(data_dir.clone())));

            // Análisis: varios hilos, dejando núcleos libres para el audio y la interfaz
            let (tx, rx) = channel::<String>();
            let rx = Arc::new(Mutex::new(rx));
            let workers = thread::available_parallelism().map(|n| n.get()).unwrap_or(2).saturating_sub(2).clamp(1, 4);
            for _ in 0..workers {
                let (h, l, r) = (app.handle().clone(), lib.clone(), rx.clone());
                thread::Builder::new().name("cabina-analysis".into()).spawn(move || analysis_worker(h, l, r))?;
            }
            // pistas que quedaron a medias en la sesión anterior
            for t in lib.lock().unwrap().tracks.iter().filter(|t| t.status == "pending") {
                let _ = tx.send(t.id.clone());
            }
            // guardado periódico
            {
                let l = lib.clone();
                thread::spawn(move || loop {
                    thread::sleep(Duration::from_secs(2));
                    let mut g = l.lock().unwrap();
                    if g.dirty {
                        if let Err(e) = g.save() {
                            eprintln!("no se pudo guardar la colección: {e}");
                        }
                    }
                });
            }
            app.manage(AppState { host, lib, jobs: tx, loaded: Arc::new(Mutex::new([None, None])), settings: Mutex::new(settings), data_dir, busy: Default::default(), generating: Default::default(), loops_dir });
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                if let Some(state) = window.try_state::<AppState>() {
                    let _ = state.lib.lock().unwrap().save();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            library_list,
            library_add,
            library_remove,
            library_reanalyze,
            track_waveform,
            track_save_cues,
            track_set_grid,
            load_track,
            deck,
            set_param,
            engine_state,
            audio_info,
            audio_configure,
            styles_info,
            set_api_key,
            set_track_style,
            classify_styles,
            organize_folders,
            loops_info,
            set_loop_server,
            generate_loop,
            app_version
        ])
        .run(tauri::generate_context!())
        .expect("error al iniciar Cabina");
}
