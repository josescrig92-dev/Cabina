# Cabina

Software DJ de escritorio para macOS, al estilo de Rekordbox, con motor de audio nativo escrito en Rust.

La interfaz es una página web dentro de una ventana [Tauri](https://tauri.app), pero **el audio no pasa por el navegador**: lo genera un motor en Rust que habla directamente con CoreAudio.

## Instalar

1. Ve a la pestaña **Actions** (o **Releases**) del repositorio y descarga `Cabina-macOS` (contiene el `.dmg`).
2. Abre el `.dmg` y arrastra **Cabina** a **Aplicaciones**.
3. La primera vez macOS avisará de que la app no está firmada por un desarrollador identificado.
   Haz **clic derecho sobre Cabina → Abrir → Abrir**. En macOS 15 o posterior: *Ajustes del Sistema → Privacidad y seguridad → Abrir igualmente*.

Funciona en Mac con Apple Silicon e Intel (binario universal), macOS 11 o posterior.

## Qué hace

**Colección**
- Añade carpetas enteras o archivos sueltos (también arrastrándolos a la ventana).
- Formatos: MP3, AAC/M4A, ALAC, FLAC, WAV, AIFF y OGG Vorbis. Lee título, artista, álbum y género de las etiquetas.
- Análisis automático en segundo plano: **BPM** (con decimales), **rejilla de beats** con precisión de milisegundos y primer tiempo de compás, **tonalidad** en notación Camelot y **sonoridad** (LUFS, EBU R128).
- Tonos compatibles para mezcla armónica resaltados respecto a la pista que suena.
- Búsqueda, orden por columnas y lista virtualizada (miles de pistas sin ralentizarse).
- Todo se guarda: cues, hot cues y correcciones de rejilla persisten entre sesiones.

**Organizar por estilo**
- Botón *Organizar por estilo* en la colección: Claude (la IA de Anthropic) asigna a cada pista uno de 32 estilos de DJ (Tech House, Deep House, Melodic House & Techno, Techno, Reggaeton…) a partir del artista, el título, las etiquetas, el BPM y el tono. El audio no sale del Mac.
- Columna *Estilo* editable a mano; las asignaciones dudosas aparecen en cursiva.
- Crea una carpeta por estilo copiando (o moviendo) los archivos, con una lista `.m3u8` en cada carpeta para importarla en Rekordbox.
- Necesita una clave de API de Anthropic (console.anthropic.com → API Keys), que se guarda solo en el Mac. Coste orientativo: 1–2 $ por cada 1.000 pistas.

**Platos**
- Onda de 3 bandas (graves, medios, agudos) ampliada y vista general con marcas de frase cada 16 compases.
- CUE al estilo CDJ (fijar, volver, pre-escucha manteniendo pulsado), 8 hot cues por pista.
- Loops automáticos de 1 a 16 beats, ½ y ×2, saltos de 4 beats que mueven el loop.
- Tempo ±6/8/16/50 %, **Master Tempo (keylock)**, **Sync** de tempo y fase, cuantización.
- Nudge arrastrando la onda mientras suena, desplazamiento con la pista parada.
- Edición de rejilla: ×½, ×2, mover ±5 ms y "compás aquí".
- Medidor de fase entre platos y aviso de final de pista (últimos 30 s).

**Mezclador** (estilo DJM)
- Trim, ecualizador aislador de 3 bandas con *kill* total, filtro paso bajo/paso alto por canal, faders con medidor.
- Crossfader con curva suave o de corte.
- Auriculares: botón CUE por canal, mezcla cue/master y volumen. Salida por los canales 3/4 de una controladora o tarjeta de 4 canales, o "split mono" con unos auriculares en una salida estéreo normal.
- Ganancia automática por pista y limitador en el master.

## Calidad de audio

| | |
|---|---|
| Ruta de señal | 32 bits en coma flotante de principio a fin, sin pasar por el navegador |
| Salida | CoreAudio directo. Abre el dispositivo a 44,1 kHz si lo admite (la frecuencia de casi toda la música); las pistas a otra frecuencia se convierten al cargarlas con el mismo filtro sinc de alta calidad. Buffer configurable desde 64 muestras |
| Velocidad 0 % | El plato lee las muestras del archivo **tal cual**, sin interpolar |
| Cambio de tempo sin keylock | Interpolación sinc con ventana Kaiser de 80 coeficientes, banda pasante plana hasta ~19 kHz y filtrado antialiasing automático al acelerar |
| Keylock | Time-stretch espectral [Signalsmith Stretch](https://signalsmith-audio.co.uk/code/stretch/); el tono se mantiene con un error menor del 0,2 % |
| Saltos, loops, play/pausa | Fundidos de 2 a 6 ms en cada salto para que nunca haya clics; costura del loop fundida |
| Ecualizador | Aislador Linkwitz-Riley de 24 dB/octava (cortes en 250 Hz y 3 kHz); con los mandos a 0 dB la respuesta en frecuencia es perfectamente plana |
| Parámetros | Todos los mandos se suavizan muestra a muestra: sin "escalones" audibles al girarlos |
| Master | Limitador de picos con 1,5 ms de anticipación y techo a −0,3 dBFS |
| Tiempo real | El hilo de audio no reserva memoria ni espera bloqueos; la memoria de las pistas se libera en otro hilo |

Rendimiento medido: dos platos con keylock, ecualizador y filtro usan alrededor de un 5 % de un núcleo.

## Atajos de teclado

| Tecla | Acción |
|---|---|
| Z / M | Play / pausa plato 1 / 2 |
| X / N | CUE plato 1 / 2 |
| 1–4 / 7–0 | Hot cues A–D plato 1 / 2 (con Mayús: borrar) |
| Supr | Quitar las pistas seleccionadas de la colección (no borra los archivos) |

En los mandos: arrastrar en vertical, rueda del ratón, flechas del teclado; doble clic los devuelve a su posición inicial. Mayús mientras arrastras da ajuste fino.

## Desarrollo

Requisitos: [Rust](https://rustup.rs) y [Node.js](https://nodejs.org) 18 o posterior. En macOS, las herramientas de línea de comandos de Xcode (`xcode-select --install`).

```sh
npm install
npm run dev          # abre la app en modo desarrollo
npm run build:mac    # genera .app y .dmg universales en src-tauri/target/universal-apple-darwin/release/bundle
```

Pruebas y herramientas del motor:

```sh
cd src-tauri
cargo test --release --lib                         # pruebas de DSP, platos, keylock, análisis
cargo run --release --example analyze -- pista.mp3 # BPM, rejilla, tono y LUFS de archivos
cargo run --release --example bench                # coste del motor en tiempo real
ANTHROPIC_API_KEY=sk-ant-... cargo run --release --example classify -- *.mp3   # estilo de cada archivo
```

Cada push a `main` compila la app en GitHub Actions (`.github/workflows/macos.yml`) y deja el `.dmg` en los artefactos. Al crear una etiqueta `v*` (por ejemplo `git tag v0.1.0 && git push --tags`) se publica además como *Release*.

### Estructura

```
ui/                       interfaz (HTML, CSS, JS sin dependencias)
src-tauri/src/
  engine/mod.rs           motor: mezclador, comandos y estado compartido
  engine/deck.rs          plato: voces con fundidos, loops, cues, keylock
  engine/dsp.rs           interpolación sinc, filtros SVF, aislador LR4, limitador
  engine/host.rs          salida CoreAudio (cpal), dispositivo y buffer
  engine/stretch.rs       puente con Signalsmith Stretch (C++)
  analysis.rs             decodificación, onda, BPM y rejilla, tono, LUFS
  library.rs              colección y guardado
  styles.rs               clasificación por estilo (Claude) y carpetas por estilo
  lib.rs                  comandos que llama la interfaz
src-tauri/vendor/         Signalsmith Stretch (licencia MIT)
prototype/                primer prototipo web
```

Los datos se guardan en `~/Library/Application Support/app.cabina.dj/` (`library.json`, `settings.json` y las ondas en `waves/`).

## Próximos pasos

- Controladoras MIDI (mapeos para Pioneer DDJ y similares) con jog wheels y scratch.
- Grabación de la sesión a WAV.
- Playlists y carpetas inteligentes; importar la colección de Rekordbox (XML).
- Efectos (eco, reverb, flanger) sincronizados al BPM.
- Firma y notarización con un certificado de Apple Developer para que macOS no muestre el aviso.

## Licencias

Incluye Signalsmith Stretch (MIT) y las fuentes Barlow, Barlow Condensed y JetBrains Mono (SIL Open Font License).
