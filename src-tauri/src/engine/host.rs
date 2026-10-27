//! Salida de audio (CoreAudio en macOS vía cpal). Gestiona el dispositivo, el tamaño de
//! buffer y la frecuencia de muestreo; si no hay salida, mantiene el motor en marcha con
//! un reloj de software para que la aplicación siga funcionando.

use super::{now_ns, Command, Engine, Params, Shared, TrackAudio};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;
use serde::Serialize;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

pub const BUFFER_SIZES: [u32; 6] = [64, 128, 256, 512, 1024, 2048];
const PREFERRED_RATES: [u32; 2] = [44100, 48000];

#[derive(Clone, Serialize, Default)]
pub struct HostStatus {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u32,
    pub buffer: u32,
    pub error: Option<String>,
}

enum Msg {
    Configure { device: Option<String>, buffer: u32 },
    Lost,
    Retry,
}

pub struct AudioHost {
    pub shared: Arc<Shared>,
    pub params: Arc<Params>,
    pub cmds: Arc<ArrayQueue<Command>>,
    pub status: Arc<Mutex<HostStatus>>,
    tx: Sender<Msg>,
}

impl AudioHost {
    pub fn start(device: Option<String>, buffer: u32) -> Self {
        let shared = Arc::new(Shared::default());
        let params = Arc::new(Params::default());
        let cmds = Arc::new(ArrayQueue::new(1024));
        let garbage: Arc<ArrayQueue<Arc<TrackAudio>>> = Arc::new(ArrayQueue::new(64));
        let status = Arc::new(Mutex::new(HostStatus::default()));
        let (tx, rx) = channel::<Msg>();

        // Hilo que libera la memoria de las pistas descargadas (fuera del hilo de audio)
        {
            let g = garbage.clone();
            thread::Builder::new()
                .name("cabina-gc".into())
                .spawn(move || loop {
                    while g.pop().is_some() {}
                    thread::sleep(Duration::from_millis(250));
                })
                .ok();
        }

        let (sh, pa, cm, st, tx2) = (shared.clone(), params.clone(), cmds.clone(), status.clone(), tx.clone());
        thread::Builder::new()
            .name("cabina-audio-host".into())
            .spawn(move || {
                let mut want_device = device;
                let mut want_buffer = buffer;
                let mut engine: Option<Arc<Mutex<Engine>>> = None;
                loop {
                    let result = open_stream(&want_device, want_buffer, &sh, &pa, &cm, &garbage, &mut engine, tx2.clone());
                    let stream = match result {
                        Ok((stream, s)) => {
                            *st.lock().unwrap() = s;
                            Some(stream)
                        }
                        Err(e) => {
                            let mut s = st.lock().unwrap();
                            s.error = Some(format!("No se pudo abrir la salida de audio ({e})"));
                            s.device = "Sin salida de audio".into();
                            None
                        }
                    };
                    // Espera mensajes; sin stream, hace avanzar el motor con un reloj de software
                    // y vuelve a intentar abrir la salida cada 3 s (p. ej. si la controladora se enciende después)
                    let mut ticks = 0u32;
                    let msg = loop {
                        if stream.is_some() {
                            match rx.recv() {
                                Ok(m) => break Some(m),
                                Err(_) => break None,
                            }
                        } else {
                            match rx.recv_timeout(Duration::from_millis(6)) {
                                Ok(m) => break Some(m),
                                Err(RecvTimeoutError::Timeout) => {
                                    ticks += 1;
                                    if ticks > 500 {
                                        break Some(Msg::Retry);
                                    }
                                    if engine.is_none() {
                                        engine = Some(Arc::new(Mutex::new(Engine::new(44100.0, sh.clone(), pa.clone(), cm.clone(), garbage.clone()))));
                                        sh.sample_rate.store(44100, Relaxed);
                                        sh.engine_gen.fetch_add(1, Relaxed);
                                    }
                                    if let Some(e) = &engine {
                                        let mut buf = [0.0f32; 256 * 2];
                                        e.lock().unwrap().process(&mut buf, 2);
                                    }
                                }
                                Err(_) => break None,
                            }
                        }
                    };
                    drop(stream);
                    match msg {
                        Some(Msg::Configure { device, buffer }) => {
                            want_device = device;
                            want_buffer = buffer;
                        }
                        Some(Msg::Lost) => {
                            // el dispositivo desapareció: volvemos al predeterminado
                            want_device = None;
                            thread::sleep(Duration::from_millis(300));
                        }
                        Some(Msg::Retry) => {}
                        None => return,
                    }
                }
            })
            .expect("no se pudo crear el hilo de audio");

        Self { shared, params, cmds, status, tx }
    }

    pub fn configure(&self, device: Option<String>, buffer: u32) {
        let _ = self.tx.send(Msg::Configure { device, buffer });
    }

    pub fn send(&self, c: Command) {
        let _ = self.cmds.push(c);
    }
}

pub fn list_devices() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devs) => devs.filter_map(|d| d.name().ok()).collect(),
        Err(_) => vec![],
    }
}

#[allow(clippy::too_many_arguments)]
fn open_stream(
    want: &Option<String>,
    buffer: u32,
    shared: &Arc<Shared>,
    params: &Arc<Params>,
    cmds: &Arc<ArrayQueue<Command>>,
    garbage: &Arc<ArrayQueue<Arc<TrackAudio>>>,
    engine: &mut Option<Arc<Mutex<Engine>>>,
    tx: Sender<Msg>,
) -> Result<(cpal::Stream, HostStatus), String> {
    let host = cpal::default_host();
    let device = match want {
        Some(name) => host
            .output_devices()
            .map_err(|e| e.to_string())?
            .find(|d| d.name().map(|n| &n == name).unwrap_or(false))
            .or_else(|| host.default_output_device()),
        None => host.default_output_device(),
    }
    .ok_or_else(|| "No se encontró ninguna salida de audio".to_string())?;
    let name = device.name().unwrap_or_else(|_| "Salida".into());

    // Elige formato f32, la frecuencia preferida (44.1 kHz: la de casi toda la música)
    // y el máximo de canales disponible (para auriculares en 3/4 en controladoras).
    let ranges: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| e.to_string())?
        .filter(|r| r.sample_format() == cpal::SampleFormat::F32)
        .collect();
    if ranges.is_empty() {
        return Err(format!("{name}: no admite audio en coma flotante"));
    }
    let mut chosen = None;
    'outer: for rate in PREFERRED_RATES {
        let mut best: Option<cpal::SupportedStreamConfigRange> = None;
        for r in &ranges {
            if r.min_sample_rate().0 <= rate && rate <= r.max_sample_rate().0 && best.as_ref().map_or(true, |b| r.channels() > b.channels()) {
                best = Some(r.clone());
            }
        }
        if let Some(b) = best {
            chosen = Some(b.with_sample_rate(cpal::SampleRate(rate)));
            break 'outer;
        }
    }
    let supported = match chosen {
        Some(c) => c,
        None => device.default_output_config().map_err(|e| e.to_string())?,
    };
    let sr = supported.sample_rate().0;
    let channels = supported.channels().min(8);
    let buf = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => buffer.clamp(*min, *max),
        cpal::SupportedBufferSize::Unknown => buffer,
    };
    let config = cpal::StreamConfig { channels, sample_rate: cpal::SampleRate(sr), buffer_size: cpal::BufferSize::Fixed(buf) };

    // Reutiliza el motor si la frecuencia no cambia (las pistas cargadas siguen sonando)
    // (el motor nuevo solo sustituye al anterior cuando la salida ya está funcionando)
    let reuse = engine.as_ref().map(|e| e.lock().unwrap().sr as u32 == sr).unwrap_or(false);
    let eng = match (reuse, engine.as_ref()) {
        (true, Some(e)) => e.clone(),
        _ => Arc::new(Mutex::new(Engine::new(sr as f64, shared.clone(), params.clone(), cmds.clone(), garbage.clone()))),
    };
    let new_engine = eng.clone();
    let sh = shared.clone();
    let ch = channels as usize;

    let stream = device
        .build_output_stream(
            &config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                set_flush_denormals();
                let ts = info.timestamp();
                if let Some(lat) = ts.playback.duration_since(&ts.callback) {
                    sh.latency_ns.store(lat.as_nanos() as u64, Relaxed);
                }
                match eng.try_lock() {
                    Ok(mut e) => e.process(data, ch),
                    Err(_) => data.fill(0.0),
                }
            },
            move |err| {
                eprintln!("error de audio: {err}");
                if matches!(err, cpal::StreamError::DeviceNotAvailable) {
                    let _ = tx.send(Msg::Lost);
                }
            },
            None,
        )
        .map_err(|e| format!("{name}: {e}"))?;
    stream.play().map_err(|e| e.to_string())?;
    if !reuse {
        *engine = Some(new_engine);
        shared.engine_gen.fetch_add(1, Relaxed);
    }
    shared.sample_rate.store(sr, Relaxed);
    shared.channels.store(channels as u32, Relaxed);
    shared.buffer.store(buf, Relaxed);
    shared.cb_time.store(now_ns(), Relaxed);
    Ok((stream, HostStatus { device: name, sample_rate: sr, channels: channels as u32, buffer: buf, error: None }))
}

/// Evita los números desnormalizados (picos de CPU en filtros cuando hay silencio).
#[inline]
fn set_flush_denormals() {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        #[allow(deprecated)]
        {
            use std::arch::x86_64::{_mm_getcsr, _mm_setcsr};
            _mm_setcsr(_mm_getcsr() | 0x8040);
        }
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        let mut fpcr: u64;
        std::arch::asm!("mrs {}, fpcr", out(reg) fpcr);
        fpcr |= 1 << 24;
        std::arch::asm!("msr fpcr, {}", in(reg) fpcr);
    }
}
