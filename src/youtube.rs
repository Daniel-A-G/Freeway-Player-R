//! Buscador de YouTube. El hilo llama a `yt-dlp` y devuelve resultados en
//! memoria: nada de lo que ocurre aquí se guarda ni se añade a las listas.
//!
//! Para reproducir, un segundo hilo pide a `yt-dlp` solo el audio, lo convierte
//! a Opus en la caché de la app y avisa cuando ya puede abrirse con `rodio`.

use image::ImageFormat;
use rodio::Source;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ffi::OsString;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ureq::Agent;

/// Número de resultados que se piden por búsqueda.
pub const RESULT_LIMIT: usize = 10;

/// Cómo se reproduce el audio de YouTube. `Streaming` es el predeterminado
/// porque empieza a sonar sin esperar a la descarga completa; `FullDownload`
/// conserva el comportamiento anterior de bajar y convertir a Opus primero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum YoutubePlayback {
    #[default]
    Streaming,
    FullDownload,
}

impl YoutubePlayback {
    pub const ALL: [YoutubePlayback; 2] = [Self::Streaming, Self::FullDownload];

    pub fn label(self) -> &'static str {
        match self {
            Self::Streaming => "Streaming (empieza a sonar antes)",
            Self::FullDownload => "Descarga completa (espera a convertir a Opus)",
        }
    }

    pub fn index(self) -> usize {
        match self {
            Self::Streaming => 0,
            Self::FullDownload => 1,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct YoutubeTrack {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub duration: Option<Duration>,
    /// URL de la miniatura, por si hiciera falta volver a descargarla.
    pub thumbnail: Option<String>,
    /// Miniatura ya descargada y convertida a PNG, lista para `Rtexture_create`.
    pub thumbnail_data: Option<Vec<u8>>,
    pub url: String,
    pub live: bool,
}

#[derive(Debug)]
pub enum YoutubeMessage {
    Searching { generation: u64 },
    Results {
        generation: u64,
        tracks: Vec<YoutubeTrack>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

pub enum YoutubeAudioMessage {
    Ready {
        generation: u64,
        index: usize,
        track: YoutubeTrack,
        path: PathBuf,
    },
    /// Audio AAC/m4a leído directamente del pipe de `yt-dlp`. No hay archivo
    /// en caché y el proceso sigue vivo mientras rodio consume la fuente.
    StreamReady {
        generation: u64,
        index: usize,
        track: YoutubeTrack,
        source: rodio::Decoder<GrowingPipe>,
        duration: Option<Duration>,
    },
    Error {
        generation: u64,
        index: usize,
        message: String,
    },
}

impl std::fmt::Debug for YoutubeAudioMessage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ready {
                generation,
                index,
                track,
                path,
            } => formatter
                .debug_struct("Ready")
                .field("generation", generation)
                .field("index", index)
                .field("track", track)
                .field("path", path)
                .finish(),
            Self::StreamReady {
                generation,
                index,
                track,
                duration,
                ..
            } => formatter
                .debug_struct("StreamReady")
                .field("generation", generation)
                .field("index", index)
                .field("track", track)
                .field("duration", duration)
                .finish(),
            Self::Error {
                generation,
                index,
                message,
            } => formatter
                .debug_struct("Error")
                .field("generation", generation)
                .field("index", index)
                .field("message", message)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub enum YoutubeDownloadMessage {
    Done {
        generation: u64,
        title: String,
        path: PathBuf,
    },
    Error {
        generation: u64,
        message: String,
    },
}

/// `FREEWAY_YTDLP` permite probar compilaciones nativas con un ejecutable
/// concreto. En el Flatpak se usa el zipapp instalado en `/app/bin`.
fn program() -> OsString {
    std::env::var_os("FREEWAY_YTDLP").unwrap_or_else(|| OsString::from("yt-dlp"))
}

/// URL de búsqueda de yt-dlp. `ytsearchN:` pide hasta N resultados; junto con
/// `--flat-playlist` evita abrir cada vídeo, así que no hace falta un motor JS.
fn search_url(query: &str) -> String {
    format!("ytsearch{RESULT_LIMIT}:{query}")
}

fn search_command(query: &str) -> Command {
    let mut command = Command::new(program());
    command
        .arg("--ignore-config")
        .arg("--no-warnings")
        .arg("--flat-playlist")
        .arg("--dump-json")
        .arg("--socket-timeout")
        .arg("15")
        .arg("--retries")
        .arg("1")
        .arg(search_url(query));
    command
}

/// Arranca una búsqueda en su propio hilo. Cada búsqueda tiene su generación:
/// la interfaz descarta mensajes de búsquedas anteriores si el usuario vuelve
/// a pulsar Enter antes de que terminen.
pub fn start_search(query: &str, generation: u64) -> Result<Receiver<YoutubeMessage>, String> {
    let query = query.trim().to_owned();
    if query.is_empty() {
        return Err("Escribe algo para buscar en YouTube".to_owned());
    }
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-youtube".to_owned())
        .spawn(move || {
            let _ = sender.send(YoutubeMessage::Searching { generation });
            match run_search(&query) {
                Ok(tracks) => {
                    let _ = sender.send(YoutubeMessage::Results { generation, tracks });
                }
                Err(message) => {
                    let _ = sender.send(YoutubeMessage::Error { generation, message });
                }
            }
        })
        .map_err(|error| format!("No se pudo iniciar la búsqueda: {error}"))?;
    Ok(receiver)
}

fn run_search(query: &str) -> Result<Vec<YoutubeTrack>, String> {
    let mut command = search_command(query);
    let output = command
        .output()
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                "No se encontró yt-dlp. Instala la última versión de Freeway Player; el buscador necesita esta herramienta.".to_owned()
            } else {
                format!("No se pudo ejecutar yt-dlp: {error}")
            }
        })?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut tracks = parse_search_output(&stdout);
    if !output.status.success() && tracks.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("yt-dlp no dio más detalles")
            .trim();
        return Err(format!("No se pudo buscar en YouTube: {detail}"));
    }
    // Un solo Agent comparte el pool de conexiones: todas las miniaturas
    // vienen de i.ytimg.com, así que crear uno por imagen solo añadía diez
    // handshakes TLS seguidos. Se descargan en paralelo antes de publicar los
    // resultados, sin que la interfaz tenga que convertir bytes en textura.
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let indices: Vec<usize> = tracks
        .iter()
        .enumerate()
        .filter_map(|(index, track)| track.thumbnail.as_ref().map(|_| index))
        .collect();
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(indices.len());
        for &index in &indices {
            let url = tracks[index].thumbnail.clone().unwrap();
            let agent = &agent;
            handles.push(scope.spawn(move || download_thumbnail(&url, agent)));
        }
        for (index, handle) in indices.into_iter().zip(handles) {
            tracks[index].thumbnail_data = handle.join().ok().flatten();
        }
    });
    Ok(tracks)
}

fn text_field(value: &Value, names: &[&str]) -> String {
    names
        .iter()
        .find_map(|name| value.get(*name).and_then(Value::as_str))
        .unwrap_or("")
        .to_owned()
}

/// YouTube ofrece varias miniaturas del mismo vídeo. La interfaz solo la usa
/// como guía visual en el panel lateral, así que se elige la más pequeña de
/// las que declaran tamaño (por ejemplo 360×202 en vez de 720×404). Si no hay
/// dimensiones, se mantiene el criterio anterior de tomar la última.
fn thumbnail_field(value: &Value) -> Option<String> {
    if let Some(list) = value.get("thumbnails").and_then(Value::as_array) {
        let sized = list.iter().filter_map(|entry| {
            let url = entry.get("url").and_then(Value::as_str)?;
            let width = entry.get("width").and_then(Value::as_u64)?;
            let height = entry.get("height").and_then(Value::as_u64)?;
            (width > 0 && height > 0).then_some((width.saturating_mul(height), url))
        });
        if let Some((_, url)) = sized.min_by_key(|(area, _)| *area) {
            return Some(url.to_owned());
        }
        if let Some(url) = list
            .iter()
            .rev()
            .filter_map(|entry| entry.get("url").and_then(Value::as_str))
            .find(|url| !url.is_empty())
        {
            return Some(url.to_owned());
        }
    }
    value
        .get("thumbnail")
        .and_then(Value::as_str)
        .filter(|url| !url.is_empty())
        .map(str::to_owned)
}

fn duration_field(value: &Value) -> Option<Duration> {
    let seconds = value
        .get("duration")
        .and_then(|duration| duration.as_u64().or_else(|| duration.as_f64().map(|n| n as u64)))?;
    Some(Duration::from_secs(seconds))
}

/// Convierte una línea de `--dump-json` en un resultado. Las líneas que no
/// tienen identificador o título se descartan: yt-dlp a veces escribe avisos
/// por stdout cuando se mezclan formatos de salida.
fn parse_result_line(line: &str) -> Option<YoutubeTrack> {
    let line = line.trim();
    if line.is_empty() || !line.starts_with('{') {
        return None;
    }
    let value: Value = serde_json::from_str(line).ok()?;
    let id = text_field(&value, &["id"]);
    let title = text_field(&value, &["title"]);
    if id.is_empty() || title.is_empty() {
        return None;
    }
    let channel = text_field(&value, &["channel", "uploader"]);
    let url = text_field(&value, &["webpage_url", "url"]);
    let url = if url.is_empty() {
        format!("https://www.youtube.com/watch?v={id}")
    } else {
        url
    };
    let live = value
        .get("live_status")
        .and_then(Value::as_str)
        .is_some_and(|status| status == "is_live")
        || value
            .get("is_live")
            .and_then(Value::as_bool)
            .unwrap_or(false);
    Some(YoutubeTrack {
        id,
        title,
        channel,
        duration: duration_field(&value),
        thumbnail: thumbnail_field(&value),
        thumbnail_data: None,
        url,
        live,
    })
}

fn parse_search_output(output: &str) -> Vec<YoutubeTrack> {
    output.lines().filter_map(parse_result_line).collect()
}

/// Descarga la miniatura y la deja en PNG. Los resultados suelen traer WebP o
/// JPEG; `image` ya tiene ambos decodificadores activados en `Cargo.toml`.
fn download_thumbnail(url: &str, agent: &Agent) -> Option<Vec<u8>> {
    let mut response = agent
        .get(url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .ok()?;
    let bytes = response.body_mut().read_to_vec().ok()?;
    let image = image::load_from_memory(&bytes).ok()?;
    let mut png = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .ok()?;
    (!png.is_empty()).then_some(png)
}

fn cache_dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| "No se encontró la carpeta de caché del usuario".to_owned())?;
    Ok(base.join("freeway-player").join("youtube-audio"))
}

fn sanitize_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "audio".to_owned()
    } else {
        cleaned
    }
}

/// Nombre de archivo legible para las descargas explícitas. Se conservan
/// letras acentuados y espacios; el resto se vuelve `_`.
fn safe_filename(value: &str) -> String {
    if !value.chars().any(char::is_alphanumeric) {
        return "audio".to_owned();
    }
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, ' ' | '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_owned();
    if trimmed.is_empty() {
        "audio".to_owned()
    } else {
        trimmed
    }
}

fn unique_stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

fn audio_command(url: &str, template: &Path) -> Command {
    let mut command = Command::new(program());
    command
        .arg("--ignore-config")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("--extract-audio")
        .arg("--audio-format")
        .arg("opus")
        .arg("--audio-quality")
        .arg("0")
        .arg("--socket-timeout")
        .arg("15")
        .arg("--retries")
        .arg("1")
        .arg("-o")
        .arg(template)
        .arg("--print")
        .arg("after_move:filepath")
        .arg(url);
    command
}

/// yt-dlp imprime la ruta final con `after_move:filepath`; a veces escribe
/// avisos por stdout, así que se toma la última línea que parezca una ruta.
fn parse_audio_output(output: &str) -> Option<PathBuf> {
    output
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
}

/// Ejecuta la conversión a Opus y devuelve el archivo final. Es la misma para
/// la reproducción temporal y para la descarga elegida por el usuario.
fn download_audio(track: &YoutubeTrack, dir: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("No se pudo crear la carpeta de destino: {error}"))?;
    let base = format!(
        "{} - {}",
        safe_filename(&track.title),
        sanitize_id(&track.id)
    );
    let mut name = base.clone();
    let mut duplicate = 1;
    while dir.join(format!("{name}.opus")).exists() {
        name = format!("{base} ({duplicate})");
        duplicate += 1;
    }
    let template = dir.join(format!("{name}.%(ext)s"));
    let output = audio_command(&track.url, &template).output();
    match output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            match parse_audio_output(&stdout) {
                Some(path) if path.is_file() => Ok(path),
                _ => Err("yt-dlp no devolvió un archivo de audio".to_owned()),
            }
        }
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let detail = stderr
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("yt-dlp no dio más detalles")
                .trim();
            Err(format!("No se pudo descargar el audio: {detail}"))
        }
        Err(error) => Err(format!("No se pudo ejecutar yt-dlp: {error}")),
    }
}

struct PipeInner {
    child: Child,
    stdout: ChildStdout,
    buffer: Vec<u8>,
    pos: u64,
    eof: bool,
}

/// Lector que va creciendo con lo que sale por stdout de `yt-dlp`. Todo lo ya
/// recibido se expone como bytes aleatorios hacia atrás; hacia delante puede
/// seguir descargando. El `moov` del m4a de YouTube va al principio, así que
/// Symphonia inicializa el decodificador con unos pocos kilobytes.
pub struct GrowingPipe {
    inner: Mutex<PipeInner>,
}

impl GrowingPipe {
    fn new(child: Child, stdout: ChildStdout) -> Self {
        Self {
            inner: Mutex::new(PipeInner {
                child,
                stdout,
                buffer: Vec::new(),
                pos: 0,
                eof: false,
            }),
        }
    }
}

impl Read for GrowingPipe {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let mut inner = self.inner.lock().unwrap();
        if inner.pos as usize >= inner.buffer.len() && !inner.eof {
            let mut chunk = [0u8; 64 * 1024];
            match inner.stdout.read(&mut chunk)? {
                0 => inner.eof = true,
                n => inner.buffer.extend_from_slice(&chunk[..n]),
            }
        }
        let start = inner.pos as usize;
        let available = inner.buffer.len().saturating_sub(start);
        if available == 0 {
            return Ok(0);
        }
        let n = available.min(out.len());
        out[..n].copy_from_slice(&inner.buffer[start..start + n]);
        inner.pos += n as u64;
        Ok(n)
    }
}

impl Seek for GrowingPipe {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let mut inner = self.inner.lock().unwrap();
        let target = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::Current(n) => inner.pos as i128 + n as i128,
            SeekFrom::End(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "el final del pipe aún no se conoce",
                ));
            }
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek negativo",
            ));
        }
        let target = target as u64;
        let mut chunk = [0u8; 64 * 1024];
        while target > inner.buffer.len() as u64 && !inner.eof {
            match inner.stdout.read(&mut chunk)? {
                0 => inner.eof = true,
                n => inner.buffer.extend_from_slice(&chunk[..n]),
            }
        }
        inner.pos = target;
        Ok(target)
    }
}

impl Drop for PipeInner {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Abre el audio m4a/AAC original de YouTube directamente desde stdout, sin
/// conversión. Si el vídeo no ofrece m4a, `yt-dlp` termina con error y el
/// llamador cae a la descarga completa.
fn stream_source(url: &str) -> Result<rodio::Decoder<GrowingPipe>, String> {
    let mut child = Command::new(program())
        .arg("--ignore-config")
        .arg("--no-warnings")
        .arg("--no-playlist")
        .arg("-f")
        .arg("bestaudio[ext=m4a]")
        .arg("--socket-timeout")
        .arg("15")
        .arg("--retries")
        .arg("1")
        .arg("-o")
        .arg("-")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("No se pudo iniciar el streaming: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "yt-dlp no devolvió datos para el streaming".to_owned())?;
    rodio::Decoder::builder()
        .with_data(GrowingPipe::new(child, stdout))
        .with_hint("m4a")
        .with_mime_type("audio/mp4")
        .build()
        .map_err(|error| format!("No se pudo decodificar el audio AAC: {error}"))
}

/// Prepara un archivo Opus en la caché con un nombre único por petición.
fn cache_audio(track: &YoutubeTrack) -> Result<PathBuf, String> {
    let dir = cache_dir()?;
    let cache_track = YoutubeTrack {
        id: format!("{}_{}", track.id, unique_stamp()),
        ..track.clone()
    };
    download_audio(&cache_track, &dir)
}

/// Reproducción según el ajuste del usuario. Con `Streaming` se intenta el
/// pipe AAC primero; cualquier fallo (formato no disponible, decodificación,
/// red) cae a la descarga completa sin mostrar un error al usuario.
pub fn start_audio_auto(
    track: YoutubeTrack,
    generation: u64,
    index: usize,
    mode: YoutubePlayback,
) -> Result<Receiver<YoutubeAudioMessage>, String> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-youtube-audio".to_owned())
        .spawn(move || {
            let message = if mode == YoutubePlayback::Streaming {
                match stream_source(&track.url) {
                    Ok(source) => {
                        let duration = source.total_duration().or(track.duration);
                        YoutubeAudioMessage::StreamReady {
                            generation,
                            index,
                            track,
                            source,
                            duration,
                        }
                    }
                    Err(stream_error) => match cache_audio(&track) {
                        Ok(path) => YoutubeAudioMessage::Ready {
                            generation,
                            index,
                            track,
                            path,
                        },
                        Err(download_error) => YoutubeAudioMessage::Error {
                            generation,
                            index,
                            message: format!(
                                "No se pudo reproducir el streaming ({stream_error}) ni descargar el audio ({download_error})"
                            ),
                        },
                    },
                }
            } else {
                match cache_audio(&track) {
                    Ok(path) => YoutubeAudioMessage::Ready {
                        generation,
                        index,
                        track,
                        path,
                    },
                    Err(message) => YoutubeAudioMessage::Error {
                        generation,
                        index,
                        message,
                    },
                }
            };
            let _ = sender.send(message);
        })
        .map_err(|error| format!("No se pudo iniciar la reproducción: {error}"))?;
    Ok(receiver)
}

/// Descarga explícita en Opus a la carpeta elegida en Ajustes.
pub fn start_download(
    track: YoutubeTrack,
    dir: PathBuf,
    generation: u64,
) -> Result<Receiver<YoutubeDownloadMessage>, String> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-youtube-download".to_owned())
        .spawn(move || {
            let title = track.title.clone();
            let message = match download_audio(&track, &dir) {
                Ok(path) => YoutubeDownloadMessage::Done {
                    generation,
                    title,
                    path,
                },
                Err(message) => YoutubeDownloadMessage::Error {
                    generation,
                    message,
                },
            };
            let _ = sender.send(message);
        })
        .map_err(|error| format!("No se pudo iniciar la descarga: {error}"))?;
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FLAT_RESULT: &str = r#"{"_type":"url","id":"abc123","url":"https://www.youtube.com/watch?v=abc123","title":"Canción · prueba","channel":"Canal de prueba","duration":215,"thumbnails":[{"url":"https://i.ytimg.com/vi/abc123/hq.jpg","height":202,"width":360},{"url":"https://i.ytimg.com/vi/abc123/maxres.jpg","height":1080,"width":1920}],"webpage_url":"https://www.youtube.com/watch?v=abc123"}"#;

    #[test]
    fn search_url_limits_results() {
        assert_eq!(search_url("lofi hip hop"), "ytsearch10:lofi hip hop");
    }

    #[test]
    fn parses_flat_search_results() {
        let tracks = parse_search_output(FLAT_RESULT);
        assert_eq!(tracks.len(), 1);
        let track = &tracks[0];
        assert_eq!(track.id, "abc123");
        assert_eq!(track.title, "Canción · prueba");
        assert_eq!(track.channel, "Canal de prueba");
        assert_eq!(track.duration, Some(Duration::from_secs(215)));
        assert_eq!(
            track.thumbnail.as_deref(),
            Some("https://i.ytimg.com/vi/abc123/hq.jpg")
        );
        assert!(track.thumbnail_data.is_none());
        assert_eq!(track.url, "https://www.youtube.com/watch?v=abc123");
        assert!(!track.live);
    }

    #[test]
    fn thumbnail_without_dimensions_keeps_the_last_url() {
        let value: Value =
            serde_json::from_str(r#"{"thumbnails":[{"url":"pequeña.jpg"},{"url":"grande.jpg"}]}"#)
                .unwrap();
        assert_eq!(thumbnail_field(&value).as_deref(), Some("grande.jpg"));
    }

    #[test]
    fn live_results_have_no_duration_and_keep_the_url() {
        let line = r#"{"id":"live1","title":"En directo","duration":null,"live_status":"is_live","uploader":"Radio"}"#;
        let track = parse_result_line(line).unwrap();
        assert!(track.live);
        assert_eq!(track.duration, None);
        assert_eq!(track.channel, "Radio");
        assert_eq!(track.url, "https://www.youtube.com/watch?v=live1");
        assert!(track.thumbnail.is_none());
    }

    #[test]
    fn ignores_empty_and_broken_lines() {
        let output = format!("aviso suelto\n{FLAT_RESULT}\n{{no es json}}\n");
        let tracks = parse_search_output(&output);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "abc123");
        assert!(parse_result_line("   ").is_none());
        assert!(parse_result_line(r#"{"title":"sin id"}"#).is_none());
    }

    #[test]
    fn audio_output_uses_the_last_printed_path() {
        let output = "Descargando...\n/ruta/a/canción.opus\n";
        assert_eq!(
            parse_audio_output(output),
            Some(PathBuf::from("/ruta/a/canción.opus"))
        );
        assert_eq!(parse_audio_output("  \n"), None);
    }

    #[test]
    fn sanitizes_ids_before_using_them_as_file_names() {
        assert_eq!(sanitize_id("abc-123_X"), "abc-123_X");
        assert_eq!(sanitize_id("../raro"), "___raro");
        assert_eq!(sanitize_id(""), "audio");
        assert_eq!(safe_filename("Canción / raro?"), "Canción _ raro_");
        assert_eq!(safe_filename("..."), "audio");
    }

    /// Prueba opt-in: necesita red y un `yt-dlp` accesible. En el Flatpak se
    /// ejecuta con el zipapp de `/app/bin`; en desarrollo se puede señalar con
    /// `FREEWAY_YTDLP` y lanzar con `cargo test -- --ignored`.
    #[test]
    #[ignore = "requiere red y yt-dlp"]
    fn real_search_returns_tracks() {
        let tracks = run_search("lofi hip hop").expect("la búsqueda real falló");
        assert!(!tracks.is_empty(), "yt-dlp no devolvió resultados");
        assert!(tracks.iter().all(|track| track.url.starts_with("https://")));
        assert!(
            tracks.iter().any(|track| track.thumbnail_data.is_some()),
            "ninguna miniatura se pudo descargar"
        );
    }

    #[test]
    #[ignore = "requiere red y yt-dlp"]
    fn real_audio_download_returns_a_file() {
        let track = YoutubeTrack {
            id: "wMzIHuWh_I0".to_owned(),
            title: "Test & Recognise".to_owned(),
            channel: "Flume".to_owned(),
            duration: Some(Duration::from_secs(304)),
            thumbnail: None,
            thumbnail_data: None,
            url: "https://www.youtube.com/watch?v=wMzIHuWh_I0".to_owned(),
            live: false,
        };
        let messages = start_audio_auto(track, 1, 0, YoutubePlayback::FullDownload)
            .expect("no se pudo arrancar la descarga");
        let message = messages.recv_timeout(Duration::from_secs(180));
        match message {
            Ok(YoutubeAudioMessage::Ready { path, .. }) => assert!(path.is_file()),
            other => panic!("la descarga real falló: {other:?}"),
        }
    }

    #[test]
    #[ignore = "requiere red y yt-dlp"]
    fn real_streaming_returns_decoded_samples() {
        let track = YoutubeTrack {
            id: "wMzIHuWh_I0".to_owned(),
            title: "Test & Recognise".to_owned(),
            channel: "Flume".to_owned(),
            duration: Some(Duration::from_secs(304)),
            thumbnail: None,
            thumbnail_data: None,
            url: "https://www.youtube.com/watch?v=wMzIHuWh_I0".to_owned(),
            live: false,
        };
        let started = std::time::Instant::now();
        let messages = start_audio_auto(track, 1, 0, YoutubePlayback::Streaming)
            .expect("no se pudo arrancar el streaming");
        match messages.recv_timeout(Duration::from_secs(30)) {
            Ok(YoutubeAudioMessage::StreamReady {
                mut source,
                duration,
                ..
            }) => {
                let elapsed = started.elapsed();
                assert!(
                    elapsed < Duration::from_secs(20),
                    "el streaming tardó demasiado en inicializar: {elapsed:?}"
                );
                assert_eq!(duration.map(|value| value.as_secs()), Some(304));
                assert!(source.next().is_some(), "el pipe no dio muestras de audio");
            }
            other => panic!("el streaming real falló: {other:?}"),
        }
    }
}
