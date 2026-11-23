use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{Accessor, Tag};
use raven::*;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{
    hash_map::{DefaultHasher, RandomState},
    HashSet,
};
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::fs::{self, File};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};
use url::Url;



#[cfg(unix)]
mod activation;
mod converter;
mod instance;
mod mpris;
mod opus;
mod updates;
mod youtube;

use youtube::YoutubePlayback;

const EMBEDDED_UI: &str = include_str!("../ui/main.xml");
const EMBEDDED_THEME: &str = include_str!("../ui/theme.xml");
const APP_ICON_PNG: &[u8] = include_bytes!("../icon.png");
const ACTION_ICONS_PNG: &[u8] = include_bytes!("../data/action-icons.png");
const SYMBOLS_2_FONT: &[u8] = include_bytes!("../data/fonts/NotoSansSymbols2-Regular.ttf");
const SYMBOLS_FONT: &[u8] = include_bytes!("../data/fonts/NotoSansSymbols-Regular.ttf");
const INITIAL_VOLUME: f32 = 0.8;
const PENDING_REPAIR_HINT: &str =
    "Archivo reparado. Restaura el original o confirma esta versión para cerrar el aviso.";
/// Nombre de la lista donde llegan los archivos abiertos desde el lanzador.
///
/// Es la única lista temporal: no se guarda en `playlists.json` y se descarta
/// al cerrar. Renombrarla la convierte en una lista permanente.
const SESSION_PLAYLIST_NAME: &str = "Sesión";

struct AudioOutput {
    player: Player,
    _stream: MixerDeviceSink,
}

#[derive(Clone, Debug, Default)]
struct TrackMetadata {
    title: String,
    artist: String,
    album: String,
    genre: String,
    track: String,
    duration: Option<Duration>,
}

#[derive(Clone)]
enum CoverChange {
    Replace(PathBuf),
    Remove,
}

/// Identifica la carátula que debe estar dibujada. En modo buscador la fila
/// seleccionada es un resultado de YouTube, no una pista de ninguna lista.
#[derive(Clone, Debug, PartialEq)]
enum CoverMarker {
    Local(usize, usize, u64),
    Youtube {
        generation: u64,
        id: String,
        index: usize,
    },
}

/// Pista añadida desde YouTube que no se descarga: se guarda la URL y varios
/// datos para poder mostrarla en la lista sin volver a consultar la red.
#[derive(Clone, Serialize, Deserialize)]
struct StreamingTrack {
    url: String,
    title: String,
    artist: String,
    thumbnail: Option<String>,
    duration_secs: Option<u64>,
}

impl StreamingTrack {
    fn from_youtube(track: &youtube::YoutubeTrack) -> Self {
        Self {
            url: track.url.clone(),
            title: track.title.clone(),
            artist: track.channel.clone(),
            thumbnail: track.thumbnail.clone(),
            duration_secs: track.duration.map(|duration| duration.as_secs()),
        }
    }

    fn metadata(&self) -> TrackMetadata {
        TrackMetadata {
            title: self.title.clone(),
            artist: self.artist.clone(),
            duration: self.duration_secs.map(Duration::from_secs),
            ..TrackMetadata::default()
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedPlaylist {
    name: String,
    tracks: Vec<PathBuf>,
    /// Entradas en streaming, paralelas a `tracks`. `None` es una pista local.
    /// El campo es opcional para leer listas guardadas antes de esta versión.
    #[serde(default)]
    streams: Vec<Option<StreamingTrack>>,
    #[serde(default)]
    selected: Option<usize>,
    #[serde(default)]
    shuffle: bool,
    #[serde(default)]
    repeat: bool,
    #[serde(default)]
    is_temporal: bool,
    /// Orden aleatorio ya elegido. Se guarda para no empezar de cero al volver
    /// a abrir la aplicación. Vacío en listas sin aleatorio.
    #[serde(default)]
    order: Vec<usize>,
    /// Posición dentro de `order` de la última pista reproducida.
    #[serde(default)]
    order_position: Option<usize>,
}

#[derive(Serialize, Deserialize)]
struct PlaylistStore {
    playlists: Vec<SavedPlaylist>,
    active_playlist: usize,
}

/// Qué hacer cuando se abre un archivo con la app ya en marcha.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OpenBehavior {
    /// Añadir a la sesión y empezar a reproducirlo (opción predeterminada).
    #[default]
    AddAndPlay,
    /// Añadir a la sesión sin interrumpir lo que esté sonando.
    AddOnly,
    /// Vaciar la sesión y quedarse solo con los archivos abiertos.
    Replace,
}

impl OpenBehavior {
    const ALL: [OpenBehavior; 3] = [
        OpenBehavior::AddAndPlay,
        OpenBehavior::AddOnly,
        OpenBehavior::Replace,
    ];

    fn label(self) -> &'static str {
        match self {
            OpenBehavior::AddAndPlay => "Añadir a Sesión y reproducir",
            OpenBehavior::AddOnly => "Añadir a Sesión sin cambiar la reproducción",
            OpenBehavior::Replace => "Reemplazar Sesión y reproducir",
        }
    }

    fn index(self) -> usize {
        match self {
            OpenBehavior::AddAndPlay => 0,
            OpenBehavior::AddOnly => 1,
            OpenBehavior::Replace => 2,
        }
    }
}

/// Posición de las opciones en el diálogo de ajustes; `None` si el índice no
/// corresponde a ninguna (por ejemplo, si se cierra sin elegir nada).
fn behavior_from_option_index(index: usize) -> Option<OpenBehavior> {
    OpenBehavior::ALL.get(index).copied()
}

/// Qué hacer al añadir pistas a una lista desde la propia aplicación. Es una
/// categoría distinta de `OpenBehavior`: aquella rige los archivos abiertos
/// desde fuera (gestor de archivos, «Abrir con» o la línea de comandos).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AddBehavior {
    /// Arrancar la primera pista añadida si no había nada sonando (predeterminado).
    #[default]
    PlayFirst,
    /// Añadir las pistas sin tocar la reproducción, la selección ni la cola.
    AddOnly,
}

impl AddBehavior {
    const ALL: [AddBehavior; 2] = [AddBehavior::PlayFirst, AddBehavior::AddOnly];

    fn label(self) -> &'static str {
        match self {
            AddBehavior::PlayFirst => "Añadir y reproducir la primera pista si no hay nada sonando",
            AddBehavior::AddOnly => "Añadir solo a la lista, sin iniciar ni cambiar la reproducción",
        }
    }

    fn index(self) -> usize {
        match self {
            AddBehavior::PlayFirst => 0,
            AddBehavior::AddOnly => 1,
        }
    }
}

/// Posición de las opciones de «añadir a la lista» en el diálogo de ajustes.
fn add_behavior_from_option_index(index: usize) -> Option<AddBehavior> {
    AddBehavior::ALL.get(index).copied()
}

/// Posición de las opciones de reproducción de YouTube en el diálogo de ajustes.
fn youtube_playback_from_option_index(index: usize) -> Option<YoutubePlayback> {
    YoutubePlayback::ALL.get(index).copied()
}

/// Preferencias que sobreviven al cierre del programa. Se guardan aparte de
/// las listas para que un fallo al escribir una no arrastre a las otras.
#[derive(Serialize, Deserialize)]
struct SavedSettings {
    #[serde(default)]
    last_cover_dir: Option<PathBuf>,
    #[serde(default)]
    open_behavior: OpenBehavior,
    #[serde(default)]
    add_behavior: AddBehavior,
    /// Reproducción de YouTube; los archivos de ajustes antiguos caen en
    /// `Streaming` gracias a `default`.
    #[serde(default)]
    youtube_playback: YoutubePlayback,
    /// Carpeta elegida para guardar descargas. `None` usa la de descargas del
    /// sistema; nunca se escribe nada ahí hasta que el usuario lo pida.
    #[serde(default)]
    download_dir: Option<PathBuf>,
    #[serde(default = "default_volume")]
    volume: f32,
}

fn default_volume() -> f32 {
    INITIAL_VOLUME
}

/// Texto que describe la carpeta de descargas en el diálogo de ajustes.
fn download_dir_text(dir: Option<&Path>) -> String {
    match dir {
        Some(path) => {
            let real = document_portal_host_path(path).unwrap_or_else(|| path.to_path_buf());
            format!("Carpeta: {}", real.display())
        }
        None => match default_download_dir() {
            Some(path) => format!("Carpeta: {} (predeterminada)", path.display()),
            None => "Carpeta: predeterminada del sistema".to_owned(),
        },
    }
}

/// Carpeta de descargas del sistema, usada mientras no haya una elegida en
/// Ajustes. Dentro del Flatpak `HOME` apunta al sandbox, que ya tiene
/// `--filesystem=home`.
fn default_download_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("XDG_DOWNLOAD_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir));
        }
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Downloads"))
}

impl Default for SavedSettings {
    fn default() -> Self {
        Self {
            last_cover_dir: None,
            open_behavior: OpenBehavior::default(),
            add_behavior: AddBehavior::default(),
            youtube_playback: YoutubePlayback::default(),
            download_dir: None,
            volume: default_volume(),
        }
    }
}

/// Reparación todavía sin resolver: el archivo ya es la versión reparada y la
/// copia del original espera en la caché del programa.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PendingRepair {
    path: PathBuf,
    backup: PathBuf,
}

/// Error al escribir etiquetas. Solo los fallos al leer o escribir el audio
/// permiten ofrecer una reparación; los de validación no tocan el archivo.
#[derive(Clone, Debug)]
enum MetadataWriteError {
    Validation(String),
    Read(String),
    Write(String),
}

impl MetadataWriteError {
    fn message(&self) -> &str {
        match self {
            Self::Validation(message) | Self::Read(message) | Self::Write(message) => message,
        }
    }

    fn can_repair(&self) -> bool {
        matches!(self, Self::Read(_) | Self::Write(_))
    }
}

impl std::fmt::Display for MetadataWriteError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.message())
    }
}

struct Playlist {
    name: String,
    tracks: Vec<PathBuf>,
    streams: Vec<Option<StreamingTrack>>,
    selected: Option<usize>,
    shuffle: bool,
    repeat: bool,
    order: Vec<usize>,
    order_position: Option<usize>,
    revision: u64,
    is_temporal: bool,
}

#[derive(Default)]
struct TrackListRows {
    tracks: Vec<PathBuf>,
    streams: Vec<Option<StreamingTrack>>,
    text: Vec<Option<CString>>,
    pending_repairs: HashSet<PathBuf>,
    /// Fila única que se muestra en el modo YouTube mientras no haya
    /// resultados: nunca sustituye a las pistas de la lista activa.
    placeholder: Option<CString>,
    /// En modo YouTube la lista muestra resultados en memoria, no pistas.
    youtube: bool,
    youtube_results: Vec<youtube::YoutubeTrack>,
}

impl Playlist {
    fn new(name: String) -> Self {
        Self {
            name,
            tracks: Vec::new(),
            streams: Vec::new(),
            selected: None,
            shuffle: false,
            repeat: false,
            order: Vec::new(),
            order_position: None,
            revision: 0,
            is_temporal: false,
        }
    }

    fn new_temporal() -> Self {
        Self {
            name: SESSION_PLAYLIST_NAME.to_owned(),
            tracks: Vec::new(),
            streams: Vec::new(),
            selected: None,
            shuffle: false,
            repeat: false,
            order: Vec::new(),
            order_position: None,
            revision: 0,
            is_temporal: true,
        }
    }

    fn from_saved(saved: SavedPlaylist) -> Self {
        let selected = saved.selected.filter(|index| *index < saved.tracks.len());
        let mut streams = saved.streams;
        streams.truncate(saved.tracks.len());
        streams.resize_with(saved.tracks.len(), || None);
        let mut playlist = Self {
            name: saved.name,
            tracks: saved.tracks,
            streams,
            selected,
            shuffle: saved.shuffle,
            repeat: saved.repeat,
            order: Vec::new(),
            order_position: None,
            revision: 0,
            is_temporal: saved.is_temporal,
        };
        // Solo reutilizamos un orden guardado si es una permutación válida de
        // las pistas actuales; si la lista cambió, se regenera como siempre.
        let valid_order = saved.order.len() == playlist.tracks.len()
            && saved.order.iter().all(|index| *index < playlist.tracks.len())
            && saved.order.iter().collect::<HashSet<_>>().len() == playlist.tracks.len();
        if playlist.shuffle && valid_order {
            playlist.order = saved.order;
            playlist.order_position = saved
                .order_position
                .filter(|position| *position < playlist.order.len())
                .or_else(|| {
                    playlist
                        .selected
                        .and_then(|track| playlist.order.iter().position(|item| *item == track))
                });
            playlist.selected = playlist
                .order_position
                .and_then(|position| playlist.order.get(position).copied())
                .or(playlist.selected);
        } else {
            playlist.rebuild_order(selected);
        }
        playlist
    }

    fn saved(&self) -> SavedPlaylist {
        SavedPlaylist {
            name: self.name.clone(),
            tracks: self.tracks.clone(),
            streams: self.streams.clone(),
            selected: self.selected,
            shuffle: self.shuffle,
            repeat: self.repeat,
            is_temporal: self.is_temporal,
            order: if self.shuffle { self.order.clone() } else { Vec::new() },
            order_position: if self.shuffle { self.order_position } else { None },
        }
    }

    fn stream_at(&self, index: usize) -> Option<&StreamingTrack> {
        self.streams.get(index).and_then(Option::as_ref)
    }

    /// Cambia el nombre de la lista y devuelve si dejó de ser temporal.
    ///
    /// Renombrar la sesión con otro nombre es la forma de conservarla; dejarle
    /// el mismo nombre la mantiene como sesión.
    fn rename(&mut self, name: String) -> bool {
        self.name = name;
        if self.is_temporal && self.name != SESSION_PLAYLIST_NAME {
            self.is_temporal = false;
            true
        } else {
            false
        }
    }

    fn rebuild_order(&mut self, first_track: Option<usize>) {
        let first_track = first_track.filter(|track| *track < self.tracks.len());
        if self.shuffle {
            self.order = shuffled_track_indices(self.tracks.len(), first_track);
            if let Some(first_track) = first_track {
                self.order.insert(0, first_track);
            }
        } else {
            self.order = (0..self.tracks.len()).collect();
        }
        self.order_position = first_track.and_then(|track| {
            self.order
                .iter()
                .position(|ordered_track| *ordered_track == track)
        });
    }

    /// Incorpora las pistas recién añadidas al final de la cola sin barajar el
    /// orden vigente ni mover `order_position`, para no alterar lo que suena.
    fn append_new_to_order(&mut self, first_added: usize) {
        self.order.extend(first_added..self.tracks.len());
    }

    /// Coloca las pistas recién insertadas en la cola sin perder la posición
    /// de la que ya sonaba. Se usa cuando el drop ocurre en medio de la lista
    /// con «Añadir solo a la lista»: las pistas nuevas entran justo antes del
    /// primer elemento desplazado, y `order_position` sigue señalando la misma
    /// pista de antes.
    fn insert_new_into_order(&mut self, first_added: usize, count: usize) {
        if count == 0 {
            return;
        }
        if self.order.is_empty() {
            self.order = (0..self.tracks.len()).collect();
            return;
        }
        for index in &mut self.order {
            if *index >= first_added {
                *index += count;
            }
        }
        let insertion = self
            .order
            .iter()
            .position(|&index| index >= first_added + count)
            .unwrap_or(self.order.len());
        for offset in 0..count {
            self.order.insert(insertion + offset, first_added + offset);
        }
        if let Some(position) = self.order_position {
            if position >= insertion {
                self.order_position = Some(position + count);
            }
        }
    }

    fn reshuffle_cycle_after(&mut self, current_track: usize) {
        let previous = std::mem::take(&mut self.order);
        self.order = reshuffled_cycle_order(self.tracks.len(), Some(current_track), &previous);
        self.order_position = self.order.len().checked_sub(1);
    }
}

/// Opciones elegidas en el diálogo que debe aplicar el hilo principal cuando
/// termina cada archivo. Las listas se reescriben una sola vez al final.
struct PendingConversion {
    total: usize,
    finished: usize,
    failed: usize,
    skipped: usize,
    replacements: Vec<(PathBuf, PathBuf)>,
    /// FLAC y Opus final generado para cada borrado pendiente.
    deletions: Vec<(PathBuf, PathBuf)>,
    replace_in_lists: bool,
    delete_flac: bool,
    /// FLAC que estaba sonando al iniciar el lote: no se borra en caliente.
    playing_source: Option<PathBuf>,
}

fn conversion_generation(message: &converter::ConversionMessage) -> u64 {
    use converter::ConversionMessage as Message;
    match message {
        Message::Started { generation, .. }
        | Message::Done { generation, .. }
        | Message::Error { generation, .. } => *generation,
    }
}

fn replacement_target(replacements: &[(PathBuf, PathBuf)], path: PathBuf) -> PathBuf {
    replacements
        .iter()
        .find(|(source, _)| source == &path)
        .map(|(_, output)| output.clone())
        .unwrap_or(path)
}

struct PlayerState {
    output: Option<AudioOutput>,
    playlists: Vec<Playlist>,
    active_playlist: usize,
    current: Option<(usize, usize)>,
    current_metadata: Option<TrackMetadata>,
    duration: Option<Duration>,
    volume: f32,
    playing: bool,
    stopped: bool,
    /// La última cola aleatoria terminó sin repetición; el siguiente «Play»
    /// debe estrenar ciclo en lugar de reanudar la última pista.
    cycle_finished: bool,
    status: String,
    update_messages: Option<Receiver<updates::UpdateMessage>>,
    update_decision: Option<Sender<bool>>,
    restart_requested: bool,
    metadata_hint: String,
    cover_hint: String,
    ui_rendered_playlist: Option<(usize, u64)>,
    editor_loaded_for: Option<(usize, usize)>,
    ui_selection_dirty: bool,
    pending_cover: Option<((usize, usize), CoverChange)>,
    cover_rendered_for: Option<CoverMarker>,
    cover_texture: Rid,
    track_list_widget: Rid,
    playlist_tabs_widget: Rid,
    last_cover_dir: Option<PathBuf>,
    open_behavior: OpenBehavior,
    add_behavior: AddBehavior,
    youtube_playback: YoutubePlayback,
    download_dir: Option<PathBuf>,
    /// El modo YouTube es solo una vista de búsqueda: no guarda resultados ni
    /// añade pistas a las listas sin una acción explícita del usuario.
    youtube_mode: bool,
    /// Resultados de la última búsqueda, en memoria. Se descartan al salir del
    /// modo YouTube o al abrir una lista.
    youtube_results: Vec<youtube::YoutubeTrack>,
    youtube_query: String,
    youtube_messages: Option<Receiver<youtube::YoutubeMessage>>,
    youtube_generation: u64,
    youtube_hint: String,
    /// Fila seleccionada en el buscador; la sincroniza `refresh_ui` con Raven.
    youtube_selected: Option<usize>,
    /// Descarga de audio en curso y resultado al que pertenece.
    youtube_audio: Option<Receiver<youtube::YoutubeAudioMessage>>,
    youtube_audio_generation: u64,
    youtube_pending_audio: Option<(u64, usize)>,
    /// Resultado que suena y archivo temporal que lo contiene.
    youtube_current: Option<usize>,
    youtube_audio_path: Option<PathBuf>,
    /// Descarga explícita en Opus pedida desde el menú contextual.
    youtube_downloads: Option<Receiver<youtube::YoutubeDownloadMessage>>,
    youtube_download_generation: u64,
    /// Reproducción de pistas añadidas como streaming a una lista normal.
    stream_audio: Option<Receiver<youtube::YoutubeAudioMessage>>,
    stream_generation: u64,
    stream_pending: Option<((usize, usize), u64)>,
    stream_current: Option<(usize, usize)>,
    stream_audio_path: Option<PathBuf>,
    /// La fuente actual sale directamente del pipe de `yt-dlp`: se reproduce
    /// en cuanto inicializa, pero no admite un seek real.
    stream_pipe_active: bool,
    /// Rutas que envían las siguientes instancias de la app.
    instance_paths: Option<Receiver<Vec<PathBuf>>>,
    /// Publicador MPRIS y canal de órdenes entrantes (el hilo D-Bus no toca
    /// `PlayerState`; solo envía comandos aquí).
    mpris: Option<mpris::MprisHandle>,
    mpris_commands: Option<Receiver<mpris::MprisCommand>>,
    /// Carátula ya exportada: (lista, pista, revisión) -> URI del archivo.
    mpris_art: Option<((usize, usize, u64), String)>,
    pending_repairs: Vec<PendingRepair>,
    /// Lote de conversión en curso y opciones del diálogo que lo originó.
    converter_messages: Option<Receiver<converter::ConversionMessage>>,
    converter_generation: u64,
    converter_pending: Option<PendingConversion>,
    drag: DragState,
    tab_drag: TabDragState,
    /// Raven no expone el desplazamiento del `ListView`. Lo marcamos cuando
    /// detectamos scroll propio; mientras esté activo, un drop externo cae al
    /// final como antes, para no insertar en un índice equivocado.
    track_list_scrolled: bool,
    /// Fila donde se insertarían los archivos que se están arrastrando sobre
    /// la lista. Solo se usa para pintar la guía durante el drop.
    external_drop_index: Option<usize>,
}

/// Estado del gesto de reordenación por arrastre.
///
/// `Pressed` guarda la posición del clic. Cuando el ratón supera el umbral de
/// movimiento se captura el índice real del modelo (Raven ya ha procesado el
/// clic) y pasamos a `Dragging`. El destino se calcula siempre desde `start_y`,
/// sin reescribir la selección del modelo durante el gesto. `target` es la fila
/// donde quedará la pista y también ancla la guía visual entre filas.
/// `last_scroll` limita el auto-scroll en los bordes a una fila cada cierto tiempo.
#[derive(Clone, Copy, Debug)]
enum DragState {
    Idle,
    Pressed { y: f32 },
    Dragging {
        from: usize,
        start_y: f32,
        target: usize,
        last_scroll: Option<Instant>,
    },
}

impl DragState {
    fn is_dragging(&self) -> bool {
        matches!(self, DragState::Dragging { .. })
    }
}

/// Estado del arrastre de pestañas de listas.
///
/// `Pressed` guarda la x del clic para aplicar el mismo umbral que en las
/// pistas. Al superarlo se captura el índice activo real de Raven (`from`) y
/// el destino se recalcula con los centros de las demás pestañas. Al soltar se
/// reordena una sola vez, así que el modelo y el widget no se tocan a medias.
#[derive(Clone, Copy, Debug)]
enum TabDragState {
    Idle,
    Pressed { start_x: f32 },
    Dragging {
        from: usize,
        start_x: f32,
        target: usize,
        /// Hueco de destino relativo al TabView, para pintar la guía.
        guide_x: f32,
    },
}

impl PlayerState {
    fn new() -> Self {
        let (saved, active_playlist) = load_playlists();
        let settings = load_settings();
        let volume = settings.volume.clamp(0.0, 1.0);
        let mut playlists: Vec<_> = saved.into_iter().map(Playlist::from_saved).collect();
        if playlists.is_empty() {
            playlists.push(Playlist::new("Mi música".to_owned()));
        }
        let active_playlist = active_playlist.min(playlists.len() - 1);
        // `selected` es la última pista que se puso en marcha. La recuperamos
        // como pista actual para que «siguiente» continúe la cola aleatoria
        // guardada en vez de volver a empezar desde el principio.
        let current = playlists[active_playlist]
            .selected
            .map(|track| (active_playlist, track));

        let (output, status) = match create_audio_output(volume) {
            Ok(output) => (Some(output), "Listo".to_owned()),
            Err(error) => (
                None,
                format!("No se pudo iniciar la salida de audio: {error}"),
            ),
        };

        Self {
            output,
            playlists,
            active_playlist,
            current,
            current_metadata: None,
            duration: None,
            volume,
            playing: false,
            stopped: true,
            cycle_finished: false,
            status,
            update_messages: None,
            update_decision: None,
            restart_requested: false,
            metadata_hint: "Selecciona una pista para editar sus etiquetas".to_owned(),
            cover_hint: "No hay una carátula seleccionada".to_owned(),
            ui_rendered_playlist: None,
            editor_loaded_for: None,
            ui_selection_dirty: false,
            pending_cover: None,
            cover_rendered_for: None,
            cover_texture: R_INVALID_ID,
            track_list_widget: R_INVALID_ID,
            playlist_tabs_widget: R_INVALID_ID,
            last_cover_dir: settings.last_cover_dir,
            open_behavior: settings.open_behavior,
            add_behavior: settings.add_behavior,
            youtube_playback: settings.youtube_playback,
            download_dir: settings.download_dir,
            youtube_mode: false,
            youtube_results: Vec::new(),
            youtube_query: String::new(),
            youtube_messages: None,
            youtube_generation: 0,
            youtube_hint: "Busca en YouTube… los resultados aparecerán aquí".to_owned(),
            youtube_selected: None,
            youtube_audio: None,
            youtube_audio_generation: 0,
            youtube_pending_audio: None,
            youtube_current: None,
            youtube_audio_path: None,
            youtube_downloads: None,
            youtube_download_generation: 0,
            stream_audio: None,
            stream_generation: 0,
            stream_pending: None,
            stream_current: None,
            stream_audio_path: None,
            stream_pipe_active: false,
            instance_paths: None,
            mpris: None,
            mpris_commands: None,
            mpris_art: None,
            pending_repairs: load_pending_repairs(),
            converter_messages: None,
            converter_generation: 0,
            converter_pending: None,
            drag: DragState::Idle,
            tab_drag: TabDragState::Idle,
            track_list_scrolled: false,
            external_drop_index: None,
        }
    }

    fn save_playlists(&mut self) {
        let path = playlist_store_path();
        let Some(parent) = path.parent() else {
            self.status = "No se pudo guardar la ubicación de las listas".to_owned();
            return;
        };
        // No guardar listas temporales (se borran al cerrar)
        let non_temporal: Vec<_> = self
            .playlists
            .iter()
            .filter(|p| !p.is_temporal)
            .map(Playlist::saved)
            .collect();
        // `active_playlist` incluye la sesión, que no se guarda: hay que
        // traducirlo al índice de la lista persistente que quede antes.
        let active_playlist = self
            .playlists
            .iter()
            .take(self.active_playlist + 1)
            .filter(|p| !p.is_temporal)
            .count()
            .saturating_sub(1);
        let store = PlaylistStore {
            playlists: non_temporal,
            active_playlist,
        };

        let result = (|| -> Result<(), String> {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let temporary = path.with_extension("json.tmp");
            let file = File::create(&temporary).map_err(|error| error.to_string())?;
            serde_json::to_writer_pretty(file, &store).map_err(|error| error.to_string())?;
            fs::rename(&temporary, &path).map_err(|error| error.to_string())
        })();

        if let Err(error) = result {
            self.status = format!("No se pudieron guardar las listas: {error}");
        }
    }

    /// Escribe todas las preferencias en `settings.json` desde el estado actual.
    fn persist_settings(&mut self) -> Result<(), String> {
        let settings = SavedSettings {
            last_cover_dir: self.last_cover_dir.clone(),
            open_behavior: self.open_behavior,
            add_behavior: self.add_behavior,
            youtube_playback: self.youtube_playback,
            download_dir: self.download_dir.clone(),
            volume: self.volume,
        };
        save_settings(&settings)
    }

    fn start_update_check(&mut self) {
        if self.update_messages.is_some() {
            self.status = "La búsqueda de actualizaciones ya está en curso".to_owned();
            return;
        }
        match updates::start_update_check() {
            Ok(task) => {
                self.update_messages = Some(task.messages);
                self.update_decision = Some(task.decision);
                self.status = "Buscando actualizaciones en GitHub…".to_owned();
            }
            Err(error) => self.status = error,
        }
    }

    fn poll_update_check(&mut self, window: Rid) {
        let message = self.update_messages.as_ref().map(Receiver::try_recv);
        match message {
            Some(Ok(updates::UpdateMessage::Available(tag))) => {
                let should_install = confirm_dialog(
                    window,
                    "Actualización disponible",
                    &format!("Se encontró una nueva versión ({tag}). ¿Quieres instalarla ahora?"),
                    "Instalar",
                    "Más tarde",
                );
                if let Some(decision) = self.update_decision.as_ref() {
                    let _ = decision.send(should_install);
                }
                self.status = if should_install {
                    format!("Preparando la instalación de {tag}…")
                } else {
                    format!("Instalación de {tag} cancelada")
                };
            }
            Some(Ok(updates::UpdateMessage::Installing(tag))) => {
                self.status = format!("Instalando {tag} desde Flatpak…");
            }
            Some(Ok(updates::UpdateMessage::Progress { tag, percent })) => {
                self.status = format!("Instalando {tag}… {percent}%");
            }
            Some(Ok(updates::UpdateMessage::Finished(result))) => {
                self.update_messages = None;
                self.update_decision = None;
                self.status = match result {
                    Ok(updates::UpdateOutcome::UpToDate(tag)) => {
                        format!("Ya tienes la última versión estable ({tag})")
                    }
                    Ok(updates::UpdateOutcome::InstalledIsNewer { latest_tag }) => {
                        format!("La versión instalada es más reciente que {latest_tag}")
                    }
                    Ok(updates::UpdateOutcome::Declined(tag)) => {
                        format!("Instalación de {tag} cancelada")
                    }
                    Ok(updates::UpdateOutcome::ManualRequired { tag, has_remote }) => {
                        open_manual_update_dialog(window, &tag, has_remote);
                        format!("Actualización {tag} requiere intervención manual")
                    }
                    Ok(updates::UpdateOutcome::InstalledAndRestarting(tag)) => {
                        self.restart_requested = true;
                        format!("{tag} instalada; reiniciando Freeway Player…")
                    }
                    Err(error) => error,
                };
            }
            Some(Err(TryRecvError::Disconnected)) => {
                self.update_messages = None;
                self.update_decision = None;
                self.status = "La comprobación de actualizaciones se interrumpió".to_owned();
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn create_playlist(&mut self) -> (usize, String) {
        let mut number = 1;
        let name = loop {
            let candidate = format!("Lista {number}");
            if !self
                .playlists
                .iter()
                .any(|playlist| playlist.name == candidate)
            {
                break candidate;
            }
            number += 1;
        };
        self.playlists.push(Playlist::new(name.clone()));
        self.active_playlist = self.playlists.len() - 1;
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;
        self.status = format!("Creada {name}");
        self.save_playlists();
        (self.active_playlist, name)
    }

    fn set_active_playlist(&mut self, index: usize) {
        if index >= self.playlists.len() {
            return;
        }
        // Elegir una lista normal (aunque sea la misma que estaba activa antes
        // de entrar en el buscador) sale siempre del modo YouTube.
        let leaving_youtube = self.youtube_mode;
        if index == self.active_playlist && !leaving_youtube {
            return;
        }
        self.youtube_mode = false;
        if leaving_youtube {
            self.clear_youtube_results();
        }
        self.active_playlist = index;
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;
        self.save_playlists();
        if leaving_youtube {
            self.status = format!("Lista activa: {}", self.playlists[index].name);
        }
    }

    fn rename_active_playlist(&mut self, name: String) -> Option<(usize, String)> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            self.status = "El nombre de la lista no puede estar vacío".to_owned();
            return None;
        }
        let playlist = self.playlists.get_mut(self.active_playlist)?;
        let promoted = playlist.rename(name.clone());
        self.status = if promoted {
            format!("Lista renombrada: {name} (se guardará al cerrar)")
        } else {
            format!("Lista renombrada: {name}")
        };
        self.save_playlists();
        Some((self.active_playlist, name))
    }

    fn close_playlist(&mut self, index: usize) -> bool {
        if self.playlists.len() <= 1 || index >= self.playlists.len() {
            return false;
        }

        if let Some((playlist, track)) = self.current {
            if playlist == index {
                if let Some(output) = &self.output {
                    output.player.stop();
                }
                self.current = None;
                self.current_metadata = None;
                self.duration = None;
                self.playing = false;
                self.stopped = true;
            } else if playlist > index {
                self.current = Some((playlist - 1, track));
            }
        }

        self.playlists.remove(index);
        if self.active_playlist > index {
            self.active_playlist -= 1;
        } else if self.active_playlist == index {
            self.active_playlist = index.min(self.playlists.len() - 1);
        }
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;
        self.status = "Lista cerrada".to_owned();
        self.save_playlists();
        true
    }

    /// Reordena las listas y remapea los índices que apuntan a ellas.
    ///
    /// `remove` + `insert` es el mismo criterio que usa `Rwidget_insert_child`
    /// en el TabView: el destino es la posición final en la lista reducida.
    fn move_playlist(&mut self, from: usize, to: usize) -> bool {
        if from == to || from >= self.playlists.len() || to >= self.playlists.len() {
            return false;
        }
        let playlist = self.playlists.remove(from);
        self.playlists.insert(to, playlist);

        self.active_playlist = remap_index(self.active_playlist, from, to);
        if let Some((playlist, track)) = self.current {
            self.current = Some((remap_index(playlist, from, to), track));
        }
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;
        self.save_playlists();
        true
    }

    fn add_tracks(&mut self, paths: Vec<PathBuf>) {
        self.add_tracks_at(paths, None);
    }

    /// Añade pistas al final (`insert_at = None`) o en la posición indicada.
    /// Los drops sobre el `ListView` usan la inserción cuando sabemos que la
    /// lista no está desplazada; en ese caso hay que reubicar también la cola
    /// y los índices de la pista en reproducción.
    fn add_tracks_at(&mut self, paths: Vec<PathBuf>, insert_at: Option<usize>) {
        if self.playlists.is_empty() || paths.is_empty() {
            return;
        }
        let playlist_index = self.active_playlist;
        let len = self.playlists[playlist_index].tracks.len();
        let target = insert_at.unwrap_or(len).min(len);

        let mut existing: HashSet<_> = self.playlists[playlist_index]
            .tracks
            .iter()
            .cloned()
            .collect();
        let mut new_paths = Vec::new();
        for path in paths {
            if existing.insert(path.clone()) {
                new_paths.push(path);
            }
        }
        if new_paths.is_empty() {
            self.status = "Esos archivos ya están en la lista".to_owned();
            return;
        }
        let added = new_paths.len();
        let add_only = self.add_behavior == AddBehavior::AddOnly;
        let anchor = self
            .current
            .filter(|(playlist, _)| *playlist == playlist_index)
            .map(|(_, track)| if track >= target { track + added } else { track });

        {
            let playlist = &mut self.playlists[playlist_index];
            // Listas guardadas antes de que existiera `streams` pueden traer
            // menos entradas que pistas.
            playlist.streams.resize_with(playlist.tracks.len(), || None);
            for (offset, path) in new_paths.into_iter().enumerate() {
                playlist.tracks.insert(target + offset, path);
                playlist.streams.insert(target + offset, None);
            }
            playlist.revision += 1;
            if add_only {
                // «Solo añadir» no cambia la reproducción: las pistas nuevas
                // entran en la cola justo antes de la primera desplazada.
                if insert_at.is_some() {
                    playlist.insert_new_into_order(target, added);
                } else {
                    playlist.append_new_to_order(target);
                }
                self.status = format!(
                    "{} pistas en la lista · reproducción sin cambios",
                    playlist.tracks.len()
                );
            } else {
                playlist.selected = Some(target);
                playlist.rebuild_order(anchor.or(Some(target)));
                self.editor_loaded_for = None;
                self.status = format!("{} pistas en la lista", playlist.tracks.len());
            }
        }
        // Un editor o una carátula pendiente apuntaban a la fila antigua.
        self.pending_cover = None;

        if let Some((playlist, track)) = self.current {
            if playlist == playlist_index && track >= target {
                self.current = Some((playlist, track + added));
            }
        }
        self.save_playlists();

        if !add_only && self.is_stopped() {
            if let Err(error) = self.play_track(playlist_index, target, true) {
                self.status = error;
            }
        }
    }

    /// Abre archivos en la lista de sesión según el ajuste configurado.
    ///
    /// La sesión se crea si no existe y se activa su pestaña. En `AddOnly` no
    /// se toca la reproducción: solo se añaden pistas y se selecciona la nueva.
    fn open_in_session(&mut self, paths: Vec<PathBuf>, behavior: OpenBehavior) {
        if paths.is_empty() {
            return;
        }

        let existing_session = self.playlists.iter().position(|playlist| playlist.is_temporal);
        let playlist_index = match existing_session {
            Some(index) => index,
            None => {
                self.playlists.push(Playlist::new_temporal());
                self.playlists.len() - 1
            }
        };

        let mut first_new = None;
        match behavior {
            OpenBehavior::Replace => {
                let playlist = &mut self.playlists[playlist_index];
                playlist.tracks = paths.clone();
                playlist.streams.clear();
                playlist.streams.resize_with(playlist.tracks.len(), || None);
                playlist.selected = Some(0);
                playlist.order.clear();
                playlist.order_position = None;
                playlist.revision += 1;
                first_new = Some(0);
            }
            OpenBehavior::AddAndPlay | OpenBehavior::AddOnly => {
                let playlist = &mut self.playlists[playlist_index];
                let previous_len = playlist.tracks.len();
                playlist.streams.resize_with(previous_len, || None);
                let mut existing: HashSet<_> = playlist.tracks.iter().cloned().collect();
                for path in &paths {
                    if existing.insert(path.clone()) {
                        playlist.tracks.push(path.clone());
                        playlist.streams.push(None);
                    }
                }
                if playlist.tracks.len() > previous_len {
                    playlist.revision += 1;
                    first_new = Some(previous_len);
                }
            }
        }

        self.active_playlist = playlist_index;
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;

        if existing_session.is_none() {
            let (name, is_temporal) = {
                let playlist = &self.playlists[playlist_index];
                (playlist.name.clone(), playlist.is_temporal)
            };
            add_playlist_tab(self.playlist_tabs_widget, &name, true, is_temporal);
        } else if self.playlist_tabs_widget != R_INVALID_ID {
            unsafe { Rtab_view_set_active(self.playlist_tabs_widget, playlist_index as i32) };
        }

        let play_index = first_new.or_else(|| {
            paths.first().and_then(|first| {
                self.playlists[playlist_index]
                    .tracks
                    .iter()
                    .position(|track| track == first)
            })
        });

        if behavior == OpenBehavior::AddOnly {
            if let Some(index) = play_index {
                self.playlists[playlist_index].selected = Some(index);
                self.ui_selection_dirty = true;
                self.editor_loaded_for = None;
                self.status = if first_new.is_some() {
                    format!("Añadido a {SESSION_PLAYLIST_NAME} · reproducción sin cambios")
                } else {
                    format!("Ya estaba en {SESSION_PLAYLIST_NAME} · reproducción sin cambios")
                };
            }
            return;
        }

        if let Some(index) = play_index {
            if let Err(error) = self.play_track(playlist_index, index, true) {
                self.status = error;
            } else {
                self.status = format!(
                    "Reproduciendo · {SESSION_PLAYLIST_NAME} no se guarda; renómbrala para conservarla"
                );
            }
        }
    }

    /// Recoge las rutas que envían otras instancias y trae la ventana delante.
    fn poll_instance_messages(&mut self, window: Rid) {
        let mut batches = Vec::new();
        let mut disconnected = false;
        if let Some(receiver) = &self.instance_paths {
            loop {
                match receiver.try_recv() {
                    Ok(paths) => batches.push(paths),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.instance_paths = None;
        }
        if batches.is_empty() {
            return;
        }

        #[cfg(unix)]
        activation::bring_to_front(window);
        #[cfg(not(unix))]
        unsafe {
            Rwindow_restore(window);
            Rwindow_show(window);
        }
        let behavior = self.open_behavior;
        for paths in batches {
            self.open_in_session(paths, behavior);
        }
    }

    /// Estado que expone el servicio MPRIS2. Se calcula en el hilo de UI para
    /// que el hilo D-Bus nunca tenga que leer `Rc<RefCell<PlayerState>>`.
    fn mpris_snapshot(&self) -> mpris::MprisSnapshot {
        let (shuffle, repeat, track_count) = self
            .playlists
            .get(self.active_playlist)
            .map(|playlist| (playlist.shuffle, playlist.repeat, playlist.tracks.len()))
            .unwrap_or((false, false, 0));
        let stopped = self.is_stopped();

        let mut track = mpris::MprisTrack {
            id: mpris::NO_TRACK_ID.to_owned(),
            ..mpris::MprisTrack::default()
        };
        if !stopped {
            if let Some(location) = self.current {
                track.id = mpris::track_id(location.0, location.1);
                let marker = (
                    location.0,
                    location.1,
                    self.playlists
                        .get(location.0)
                        .map(|playlist| playlist.revision)
                        .unwrap_or(0),
                );
                if let Some((_, uri)) = self
                    .mpris_art
                    .as_ref()
                    .filter(|(cached, _)| *cached == marker)
                {
                    track.art_uri = uri.clone();
                }
                if let Some(metadata) = &self.current_metadata {
                    track.title = metadata.title.clone();
                    if !metadata.artist.is_empty() {
                        track.artist = vec![metadata.artist.clone()];
                    }
                    if !metadata.album.is_empty() {
                        track.album = metadata.album.clone();
                    }
                    if !metadata.genre.is_empty() {
                        track.genre = vec![metadata.genre.clone()];
                    }
                    track.track_number = metadata
                        .track
                        .split('/')
                        .next()
                        .and_then(|number| number.trim().parse::<i32>().ok());
                }
                if let Some(path) = self.path_at(location) {
                    track.url = Url::from_file_path(path)
                        .map(|url| url.to_string())
                        .unwrap_or_default();
                }
                track.length_micros = self
                    .duration
                    .or_else(|| self.current_metadata.as_ref().and_then(|meta| meta.duration))
                    .map(mpris::to_micros);
            }
        }

        mpris::MprisSnapshot {
            playback_status: if stopped {
                "Stopped"
            } else if self.playing {
                "Playing"
            } else {
                "Paused"
            }
            .to_owned(),
            metadata: mpris::metadata_for(&track),
            position_micros: if stopped {
                0
            } else {
                mpris::to_micros(self.position())
            },
            volume: f64::from(self.volume),
            shuffle,
            loop_status: if repeat { "Playlist" } else { "None" }.to_owned(),
            can_play: true,
            can_pause: !stopped,
            can_go_next: track_count > 0,
            can_go_previous: track_count > 0,
            can_seek: !stopped && self.duration.is_some(),
            can_control: true,
            seeked_position: None,
        }
    }

    /// Extrae la carátula incrustada a la caché para exponerla como
    /// `mpris:artUrl`. Solo se repite al cambiar de pista o de revisión.
    fn update_mpris_art(&mut self) {
        let Some(location) = self.current.filter(|_| !self.is_stopped()) else {
            self.mpris_art = None;
            return;
        };
        let Some(playlist) = self.playlists.get(location.0) else {
            self.mpris_art = None;
            return;
        };
        let marker = (location.0, location.1, playlist.revision);
        if self
            .mpris_art
            .as_ref()
            .is_some_and(|(cached, _)| *cached == marker)
        {
            return;
        }
        let path = playlist.tracks.get(location.1).cloned();
        let dir = cache_dir().join("freeway-player").join("mpris");
        let uri = path
            .as_deref()
            .and_then(read_cover_art)
            .and_then(|data| mpris::write_art_cache(&dir, &data))
            .and_then(|file| Url::from_file_path(&file).ok().map(|url| url.to_string()));
        self.mpris_art = uri.map(|uri| (marker, uri));
    }

    /// Publica el estado tras cada frame, igual que se drenan los otros canales.
    fn publish_mpris_snapshot(&mut self) {
        let Some(handle) = self.mpris.clone() else {
            return;
        };
        self.update_mpris_art();
        handle.publish(self.mpris_snapshot());
    }

    /// Ningún salto MPRIS debe dejar la posición fuera de la pista.
    fn mpris_clamp_position(&self, position: i64) -> i64 {
        let limit = self.duration.map(mpris::to_micros).unwrap_or(i64::MAX);
        position.clamp(0, limit)
    }

    /// Marca un salto para que el hilo D-Bus emita la señal `Seeked`.
    fn notify_mpris_seek(&self) {
        if let Some(handle) = &self.mpris {
            handle.mark_seeked(mpris::to_micros(self.position()));
        }
    }

    /// Aplica las órdenes recibidas desde D-Bus en el hilo de UI.
    fn poll_mpris_commands(&mut self) {
        let Some(receiver) = self.mpris_commands.take() else {
            return;
        };
        let mut disconnected = false;
        loop {
            let command = match receiver.try_recv() {
                Ok(command) => command,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            };
            match command {
                mpris::MprisCommand::Play => self.play(),
                mpris::MprisCommand::Pause => self.pause(),
                mpris::MprisCommand::Stop => self.stop(),
                mpris::MprisCommand::Next => self.next(),
                mpris::MprisCommand::Previous => self.previous(),
                mpris::MprisCommand::Seek(offset) => {
                    if !self.is_stopped() {
                        let target = mpris::seek_target(mpris::to_micros(self.position()), offset);
                        let target = self.mpris_clamp_position(target);
                        self.seek(target as f32 / 1_000_000.0);
                        self.notify_mpris_seek();
                    }
                }
                mpris::MprisCommand::SetPosition { track_id, position } => {
                    let is_current = self
                        .current
                        .is_some_and(|location| mpris::track_id(location.0, location.1) == track_id);
                    if is_current && !self.is_stopped() {
                        let target = self.mpris_clamp_position(position);
                        self.seek(target as f32 / 1_000_000.0);
                        self.notify_mpris_seek();
                    }
                }
                mpris::MprisCommand::SetVolume(volume) => {
                    if volume.is_finite() {
                        self.set_volume(volume as f32);
                    }
                }
                mpris::MprisCommand::SetShuffle(shuffle) => {
                    let should_toggle = self
                        .playlists
                        .get(self.active_playlist)
                        .is_some_and(|playlist| playlist.shuffle != shuffle);
                    if should_toggle {
                        self.toggle_shuffle();
                    }
                }
                mpris::MprisCommand::SetLoopStatus(status) => {
                    if let Some(repeat) = mpris::loop_status_to_repeat(&status) {
                        let should_toggle = self
                            .playlists
                            .get(self.active_playlist)
                            .is_some_and(|playlist| playlist.repeat != repeat);
                        if should_toggle {
                            self.toggle_repeat();
                        }
                    }
                }
            }
        }
        if disconnected {
            self.mpris = None;
        } else {
            self.mpris_commands = Some(receiver);
        }
    }

    fn move_track(&mut self, from_index: usize, to_index: usize) {
        let playlist_index = self.active_playlist;
        let Some(playlist) = self.playlists.get_mut(playlist_index) else {
            return;
        };
        if from_index == to_index || to_index >= playlist.tracks.len() {
            return;
        }
        let track = playlist.tracks.remove(from_index);
        playlist.tracks.insert(to_index, track);
        let stream = playlist.streams.remove(from_index);
        playlist.streams.insert(to_index, stream);

        // Adjust current track if needed
        if let Some((pl, tr)) = self.current {
            if pl == playlist_index {
                if tr == from_index && tr != to_index {
                    self.current = Some((pl, to_index));
                } else if from_index < to_index && tr > from_index && tr <= to_index {
                    self.current = Some((pl, tr - 1));
                } else if to_index < from_index && tr >= to_index && tr < from_index {
                    self.current = Some((pl, tr + 1));
                }
            }
        }

        playlist.selected = Some(to_index);
        playlist.revision += 1;
        self.ui_selection_dirty = true;
        self.save_playlists();
    }

    fn remove_selected_track(&mut self) {
        let playlist_index = self.active_playlist;
        let Some(track_index) = self
            .playlists
            .get(playlist_index)
            .and_then(|playlist| playlist.selected)
        else {
            self.status = "Selecciona una pista para quitarla".to_owned();
            return;
        };

        if self.current == Some((playlist_index, track_index)) {
            if self.stream_pending.is_some() || self.stream_current.is_some() {
                self.reset_stream_playback();
            } else if let Some(output) = &self.output {
                output.player.stop();
            }
            self.current = None;
            self.current_metadata = None;
            self.duration = None;
            self.playing = false;
            self.stopped = true;
        } else if let Some((current_playlist, current_track)) = self.current {
            if current_playlist == playlist_index && current_track > track_index {
                self.current = Some((current_playlist, current_track - 1));
            }
        }

        let playlist = &mut self.playlists[playlist_index];
        playlist.tracks.remove(track_index);
        if track_index < playlist.streams.len() {
            playlist.streams.remove(track_index);
        }
        playlist.selected = if playlist.tracks.is_empty() {
            None
        } else {
            Some(track_index.min(playlist.tracks.len() - 1))
        };
        let order_anchor = self
            .current
            .filter(|(current_playlist, _)| *current_playlist == playlist_index)
            .map(|(_, current_track)| current_track)
            .or(playlist.selected);
        playlist.rebuild_order(order_anchor);
        playlist.revision += 1;
        self.editor_loaded_for = None;
        self.status = "Pista quitada de la lista".to_owned();
        self.save_playlists();
    }

    fn play_track(
        &mut self,
        playlist_index: usize,
        track_index: usize,
        start_playing: bool,
    ) -> Result<(), String> {
        let stream = self
            .playlists
            .get(playlist_index)
            .and_then(|playlist| playlist.stream_at(track_index))
            .cloned();
        if let Some(stream) = stream {
            return self.play_streaming_track((playlist_index, track_index), stream);
        }
        let path = self
            .playlists
            .get(playlist_index)
            .and_then(|playlist| playlist.tracks.get(track_index))
            .cloned()
            .ok_or_else(|| "No hay una pista seleccionada".to_owned())?;
        let source = opus::source_for_path(&path)?;
        let duration = source.total_duration();
        let output = self
            .output
            .as_ref()
            .ok_or_else(|| "No hay un dispositivo de salida de audio disponible".to_owned())?;

        output.player.clear();
        if output.player.empty() {
            // `try_seek` guarda la orden aunque no haya sonido; sin esto, un seek
            // pendiente de la pista anterior se aplicaría al primer sample de esta.
            let _ = output.player.try_seek(Duration::ZERO);
        }
        output.player.set_volume(self.volume);
        output.player.append(source);
        if start_playing {
            output.player.play();
        }
        self.stopped = false;
        self.cycle_finished = false;

        let playlist = &mut self.playlists[playlist_index];
        if playlist.order.len() != playlist.tracks.len() || !playlist.order.contains(&track_index) {
            playlist.rebuild_order(Some(track_index));
        }
        playlist.order_position = playlist
            .order
            .iter()
            .position(|ordered_track| *ordered_track == track_index);
        self.current = Some((playlist_index, track_index));
        self.current_metadata = Some(read_track_metadata(&path));
        playlist.selected = Some(track_index);
        if playlist_index == self.active_playlist {
            self.ui_selection_dirty = true;
        }
        self.duration = duration;
        self.playing = start_playing;
        self.status = if start_playing {
            "Reproduciendo".to_owned()
        } else {
            "En pausa".to_owned()
        };
        Ok(())
    }

    /// Escoge una pista al azar de la lista activa, excluyendo la que suena
    /// cuando hay más de una para que el botón siempre cambie de pista. No
    /// depende del modo aleatorio: sirve también para listas normales.
    fn play_random(&mut self) {
        let playlist_index = self.active_playlist;
        let Some(playlist) = self.playlists.get(playlist_index) else {
            return;
        };
        let track_count = playlist.tracks.len();
        let current_track = self
            .current
            .filter(|(current_playlist, _)| *current_playlist == playlist_index)
            .map(|(_, current_track)| current_track);
        let Some(track_index) = random_track_index(track_count, current_track) else {
            self.status = "Añade pistas a esta lista".to_owned();
            return;
        };
        if let Err(error) = self.play_track(playlist_index, track_index, true) {
            self.status = error;
        }
    }

    fn play_selected(&mut self, selected: Option<usize>) {
        // En el buscador el índice es de resultado, no de pista: usarlo con
        // `play_track` sería lo que hacía recorrer la lista que estaba abierta.
        if self.youtube_mode {
            self.play_youtube_result(selected);
            return;
        }
        let playlist_index = self.active_playlist;
        let Some(playlist) = self.playlists.get(playlist_index) else {
            return;
        };
        let Some(track_index) = selected
            .or(playlist.selected)
            .or_else(|| (!playlist.tracks.is_empty()).then_some(0))
        else {
            self.status = "Añade pistas a esta lista".to_owned();
            return;
        };
        if self.current != Some((playlist_index, track_index)) {
            if let Some(playlist) = self.playlists.get_mut(playlist_index) {
                playlist.rebuild_order(Some(track_index));
            }
        }
        if let Err(error) = self.play_track(playlist_index, track_index, true) {
            self.status = error;
        }
    }

    /// Tras terminar una cola aleatoria sin repetición, arranca un ciclo nuevo
    /// en vez de reanudar la última pista. Devuelve si ha hecho algo, para que
    /// `play()` no siga con sus otras vías de arranque.
    fn restart_finished_shuffle_cycle(&mut self) -> bool {
        if !self.cycle_finished {
            return false;
        }
        let Some((playlist_index, track)) = self.current else {
            return false;
        };
        if playlist_index != self.active_playlist {
            return false;
        }
        let previous = self
            .playlists
            .get(playlist_index)
            .map(|playlist| playlist.order.clone());
        let Some(previous) = previous else {
            return false;
        };
        let next_track = {
            let Some(playlist) = self.playlists.get_mut(playlist_index) else {
                return false;
            };
            if !playlist.shuffle || previous.len() < 2 || previous.last().copied() != Some(track) {
                return false;
            }
            playlist.order = reshuffled_cycle_order(previous.len(), Some(track), &previous);
            playlist.order_position = playlist.order.len().checked_sub(1);
            playlist.order.first().copied()
        };
        let Some(next_track) = next_track else {
            return false;
        };
        if let Err(error) = self.play_track(playlist_index, next_track, true) {
            self.status = error;
        }
        true
    }

    fn play(&mut self) {
        if self.output.is_none() {
            self.status = "No hay un dispositivo de salida de audio disponible".to_owned();
            return;
        }
        if self.youtube_mode {
            self.play_youtube_result(None);
            return;
        }
        if let Some(location) = self.selected_location() {
            if play_starts_selected(self.current, Some(location)) {
                if let Err(error) = self.play_track(location.0, location.1, true) {
                    self.status = error;
                }
                return;
            }
        }
        if self.restart_finished_shuffle_cycle() {
            return;
        }
        if let Some((playlist, track)) = self.current {
            if playlist == self.active_playlist {
                let empty = self
                    .output
                    .as_ref()
                    .map(|output| output.player.empty())
                    .unwrap_or(true);
                // Tras `stop()`, rodio conserva la fuente en la cola durante unos
                // milisegundos con el flag interno «stopped» activo. `play()` solo
                // quita la pausa, así que hay que volver a añadir la fuente para
                // que `append()` limpie ese flag; si no, la pista se detiene sola.
                if empty || self.stopped {
                    if let Err(error) = self.play_track(playlist, track, true) {
                        self.status = error;
                    }
                } else {
                    self.output.as_ref().unwrap().player.play();
                    self.playing = true;
                    self.stopped = false;
                    self.status = "Reproduciendo".to_owned();
                }
                return;
            }
        }
        self.play_selected(None);
    }

    fn pause(&mut self) {
        if self.is_stopped() {
            return;
        }
        if let Some(output) = &self.output {
            output.player.pause();
            self.playing = false;
            self.status = "En pausa".to_owned();
        }
    }

    fn stop(&mut self) {
        if self.youtube_mode
            && (self.youtube_current.is_some() || self.youtube_pending_audio.is_some())
        {
            self.reset_youtube_playback();
        } else if self.stream_pending.is_some() || self.stream_current.is_some() {
            self.reset_stream_playback();
        } else if let Some(output) = &self.output {
            output.player.stop();
        }
        self.playing = false;
        self.stopped = true;
        self.stream_pipe_active = false;
        self.cycle_finished = false;
        self.status = "Detenido".to_owned();
    }

    fn toggle_shuffle(&mut self) {
        let playlist_index = self.active_playlist;
        let current_track = self
            .current
            .filter(|(current_playlist, _)| *current_playlist == playlist_index)
            .map(|(_, current_track)| current_track);
        let Some(playlist) = self.playlists.get_mut(playlist_index) else {
            return;
        };
        playlist.shuffle = !playlist.shuffle;
        playlist.rebuild_order(current_track.or(playlist.selected));
        self.status = if playlist.shuffle {
            "Aleatorio activado para esta lista".to_owned()
        } else {
            "Aleatorio desactivado para esta lista".to_owned()
        };
        self.save_playlists();
    }

    fn toggle_repeat(&mut self) {
        let Some(playlist) = self.playlists.get_mut(self.active_playlist) else {
            return;
        };
        playlist.repeat = !playlist.repeat;
        self.status = if playlist.repeat {
            "Repetición de lista activada".to_owned()
        } else {
            "Repetición de lista desactivada".to_owned()
        };
        self.save_playlists();
    }

    fn step_track(&mut self, forward: bool) {
        if self.youtube_mode {
            let length = self.youtube_results.len();
            let Some(from) = self.displayed_youtube_index() else {
                self.status = "Busca un resultado para reproducirlo".to_owned();
                return;
            };
            let Some(next) = Self::youtube_step_index(length, from, forward) else {
                self.status = "No hay resultados para reproducir".to_owned();
                return;
            };
            self.play_youtube_result(Some(next));
            return;
        }
        let playlist_index = self
            .current
            .filter(|(playlist, _)| *playlist == self.active_playlist)
            .map(|(playlist, _)| playlist)
            .unwrap_or(self.active_playlist);
        self.advance_in_playlist(playlist_index, forward, false);
    }

    fn advance_in_playlist(&mut self, playlist_index: usize, forward: bool, automatic: bool) {
        let Some(playlist) = self.playlists.get(playlist_index) else {
            return;
        };
        if playlist.tracks.is_empty() {
            self.status = "Añade pistas a esta lista".to_owned();
            return;
        }

        let current_track = self
            .current
            .filter(|(current_playlist, _)| *current_playlist == playlist_index)
            .map(|(_, track)| track);
        let selected = playlist.selected;
        let len = playlist.tracks.len();
        if playlist.order.len() != len {
            self.playlists[playlist_index].rebuild_order(current_track.or(selected));
        }

        let mut order = self.playlists[playlist_index].order.clone();
        let mut position = current_track.and_then(|track| {
            order
                .iter()
                .position(|ordered_track| *ordered_track == track)
        });
        let repeat = self.playlists[playlist_index].repeat;
        let shuffle = self.playlists[playlist_index].shuffle;

        // Toda reanudación de la cola (por repetición o con «siguiente» manual)
        // estrena orden. Cuando termina sin repetición no se baraja todavía: la
        // lista se ha acabado y será «Play» quien empiece un ciclo nuevo.
        if forward && shuffle && position == Some(len - 1) && (repeat || !automatic) {
            if let Some(current_track) = current_track {
                self.playlists[playlist_index].reshuffle_cycle_after(current_track);
                order = self.playlists[playlist_index].order.clone();
                position = self.playlists[playlist_index].order_position;
            }
        }

        let candidates = if let Some(position) = position {
            if forward && automatic && !repeat {
                (position + 1..len).collect::<Vec<_>>()
            } else if forward {
                (1..=len)
                    .map(|offset| (position + offset) % len)
                    .collect::<Vec<_>>()
            } else {
                (1..=len)
                    .map(|offset| (position + len - offset % len) % len)
                    .collect::<Vec<_>>()
            }
        } else {
            let start = selected
                .and_then(|track| {
                    order
                        .iter()
                        .position(|ordered_track| *ordered_track == track)
                })
                .unwrap_or(0);
            (0..len)
                .map(|offset| (start + offset) % len)
                .collect::<Vec<_>>()
        };

        let mut last_error = None;
        for candidate in candidates {
            let Some(track) = order.get(candidate).copied() else {
                continue;
            };
            match self.play_track(playlist_index, track, true) {
                Ok(()) => return,
                Err(error) => last_error = Some(error),
            }
        }
        self.playing = false;
        self.stopped = true;
        self.cycle_finished = automatic && shuffle;
        self.status = last_error.unwrap_or_else(|| {
            if automatic {
                "Reproducción finalizada".to_owned()
            } else {
                "No hay pistas reproducibles".to_owned()
            }
        });
    }

    fn next(&mut self) {
        self.step_track(true);
    }

    fn previous(&mut self) {
        self.step_track(false);
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        if let Some(output) = &self.output {
            output.player.set_volume(self.volume);
        }
    }

    fn seek(&mut self, seconds: f32) {
        // El pipe de yt-dlp no es aleatorio: la fuente solo avanza. Se deja la
        // barra informativa, pero no se envía una orden que descuadre el audio.
        if self.stream_pipe_active {
            self.status = "El streaming de YouTube no permite cambiar la posición".to_owned();
            return;
        }
        // rodio guarda la orden de seek incluso si no hay sonido y la aplica a
        // la siguiente pista que se añada. Evita envenenar esa pista.
        if self.is_stopped() {
            return;
        }
        let Some(output) = &self.output else {
            return;
        };
        let seconds = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        if let Err(error) = output.player.try_seek(Duration::from_secs_f32(seconds)) {
            self.status = format!("No se pudo cambiar la posición: {error}");
        }
    }

    fn update_after_frame(&mut self) {
        let ended = self.playing
            && self
                .output
                .as_ref()
                .map(|output| output.player.empty())
                .unwrap_or(false);
        if !ended {
            return;
        }

        if self.youtube_mode {
            let Some(index) = self.youtube_current else {
                self.playing = false;
                return;
            };
            let Some(next) = Self::youtube_step_index(self.youtube_results.len(), index, true) else {
                self.youtube_current = None;
                self.current_metadata = None;
                self.duration = None;
                self.playing = false;
                self.stopped = true;
                self.status = "Reproducción finalizada".to_owned();
                return;
            };
            self.play_youtube_result(Some(next));
            return;
        }

        let Some((playlist_index, _)) = self.current else {
            self.playing = false;
            return;
        };
        self.advance_in_playlist(playlist_index, true, true);
    }

    fn position(&self) -> Duration {
        if self.is_stopped() {
            return Duration::ZERO;
        }
        self.output
            .as_ref()
            .map(|output| output.player.get_pos())
            .unwrap_or_default()
    }

    /// Detenido no es lo mismo que en pausa: el reproductor se vacía y, desde
    /// ahí, la app se comporta como recién abierta. `Player::stop()` de rodio
    /// conserva la última posición interna, así que no se puede usar para
    /// decidir esto por sí solo.
    fn is_stopped(&self) -> bool {
        if self.playing {
            return false;
        }
        match &self.output {
            Some(output) => self.stopped || output.player.empty(),
            None => true,
        }
    }

    /// Pista que alimentan la carátula y los textos inferiores. Detenido manda
    /// la fila seleccionada (como al abrir la app); sonando o en pausa manda la
    /// pista cargada en el reproductor.
    fn displayed_location(&self) -> Option<(usize, usize)> {
        displayed_location_for(self.is_stopped(), self.current, self.selected_location())
    }

    fn selected_location(&self) -> Option<(usize, usize)> {
        self.playlists
            .get(self.active_playlist)?
            .selected
            .map(|track| (self.active_playlist, track))
    }

    fn selected_is_flac(&self) -> bool {
        self.selected_location()
            .and_then(|(playlist, track)| self.playlists.get(playlist)?.tracks.get(track))
            .is_some_and(|path| converter::is_flac(path))
    }

    /// Ruta local cargada en el reproductor ahora mismo, si la hay.
    /// En estado detenido `current` solo marca la fila mostrada: no hay archivo
    /// abierto, así que borrar ese FLAC no interrumpe nada.
    fn playing_source_path(&self) -> Option<PathBuf> {
        if self.is_stopped() {
            return None;
        }
        let (playlist, track) = self.current?;
        let playlist = self.playlists.get(playlist)?;
        if playlist.stream_at(track).is_some() {
            return None;
        }
        playlist.tracks.get(track).cloned()
    }

    fn start_conversion_job(
        &mut self,
        tasks: Vec<converter::ConversionTask>,
        generation: u64,
        replace_in_lists: bool,
        delete_flac: bool,
        skipped: usize,
    ) -> Result<(), String> {
        let total = tasks.len() + skipped;
        let pending = PendingConversion {
            total,
            // Los omitidos no generan mensajes: los contamos como cerrados.
            finished: skipped,
            failed: 0,
            skipped,
            replacements: Vec::new(),
            deletions: Vec::new(),
            replace_in_lists,
            delete_flac,
            playing_source: self.playing_source_path(),
        };
        let converting = tasks.len();
        let receiver = converter::start_conversion(tasks, generation)?;
        self.converter_generation = generation;
        self.converter_messages = Some(receiver);
        self.converter_pending = Some(pending);
        self.status = format!("Convirtiendo {} archivo…", converting);
        Ok(())
    }

    fn poll_conversion(&mut self) {
        loop {
            let message = match self.converter_messages.as_ref().map(Receiver::try_recv) {
                Some(Ok(message)) => message,
                Some(Err(TryRecvError::Empty)) | None => return,
                Some(Err(TryRecvError::Disconnected)) => {
                    let interrupted = self
                        .converter_pending
                        .as_ref()
                        .is_some_and(|pending| pending.finished < pending.total);
                    self.converter_messages = None;
                    if interrupted {
                        self.converter_pending = None;
                        self.status = "La conversión se interrumpió".to_owned();
                    }
                    return;
                }
            };
            if self.converter_generation != conversion_generation(&message) {
                continue;
            }
            self.handle_conversion_message(message);
        }
    }

    fn handle_conversion_message(&mut self, message: converter::ConversionMessage) {
        use converter::ConversionMessage as Message;

        let Some(pending) = self.converter_pending.as_mut() else {
            return;
        };
        match message {
            Message::Started {
                index, source, ..
            } => {
                let name = source.file_name().unwrap_or_default().to_string_lossy();
                self.status = format!(
                    "Convirtiendo «{name}» ({}/{})…",
                    index + 1,
                    pending.total
                );
                return;
            }
            Message::Done { source, output, .. } => {
                if pending.replace_in_lists {
                    pending.replacements.push((source.clone(), output.clone()));
                }
                if pending.delete_flac {
                    pending.deletions.push((source, output));
                }
            }
            Message::Error {
                source, message, ..
            } => {
                pending.failed += 1;
                let name = source.file_name().unwrap_or_default().to_string_lossy();
                self.status = format!("No se pudo convertir «{name}»: {message}");
            }
        }
        pending.finished += 1;
        if pending.finished >= pending.total {
            self.finish_conversion_job();
        }
    }

    fn finish_conversion_job(&mut self) {
        let Some(pending) = self.converter_pending.take() else {
            return;
        };
        self.converter_messages = None;

        let mut changed_lists = 0;
        if pending.replace_in_lists && !pending.replacements.is_empty() {
            let mut entries: Vec<Vec<(PathBuf, Option<StreamingTrack>)>> = self
                .playlists
                .iter()
                .map(|playlist| {
                    playlist
                        .tracks
                        .iter()
                        .cloned()
                        .zip(playlist.streams.iter().cloned())
                        .collect()
                })
                .collect();
            changed_lists = converter::replace_entries_in_lists(&mut entries, &pending.replacements);

            let old_current = self.current.and_then(|(playlist, track)| {
                self.playlists
                    .get(playlist)
                    .and_then(|list| list.tracks.get(track).cloned())
                    .map(|path| (playlist, path))
            });
            for (playlist, converted) in self.playlists.iter_mut().zip(entries) {
                let previous = playlist.tracks.clone();
                let selected_path = playlist
                    .selected
                    .and_then(|index| previous.get(index).cloned())
                    .map(|path| replacement_target(&pending.replacements, path));
                playlist.tracks = converted.iter().map(|(path, _)| path.clone()).collect();
                playlist.streams = converted
                    .into_iter()
                    .map(|(_, stream)| stream)
                    .collect();
                let selected = selected_path
                    .and_then(|path| playlist.tracks.iter().position(|item| item == &path));
                playlist.selected = selected;
                playlist.rebuild_order(selected);
            }
            if let Some((playlist, old_path)) = old_current {
                let new_path = replacement_target(&pending.replacements, old_path);
                if let Some(track) = self.playlists[playlist]
                    .tracks
                    .iter()
                    .position(|path| path == &new_path)
                {
                    self.current = Some((playlist, track));
                }
            }
            self.ui_rendered_playlist = None;
            self.editor_loaded_for = None;
            self.save_playlists();
        }

        let mut deleted = 0;
        let mut in_use = 0;
        let mut delete_errors = 0;
        if pending.delete_flac {
            let current_source = self.playing_source_path();
            for (source, output) in &pending.deletions {
                let playing = pending.playing_source.as_ref() == Some(source)
                    || current_source.as_ref() == Some(source);
                let replacement_ok = !pending.replace_in_lists
                    || pending
                        .replacements
                        .iter()
                        .any(|(old, new)| old == source && new == output);
                if !converter::can_delete_source(true, output.is_file(), replacement_ok, playing) {
                    if playing {
                        in_use += 1;
                    } else {
                        delete_errors += 1;
                    }
                    continue;
                }
                match fs::remove_file(source) {
                    Ok(()) => deleted += 1,
                    Err(_) => delete_errors += 1,
                }
            }
        }

        let converted = pending.total - pending.failed - pending.skipped;
        let mut notes = vec![format!("{converted} convertidos")];
        if pending.failed > 0 {
            notes.push(format!("{} fallidos", pending.failed));
        }
        if pending.skipped > 0 {
            notes.push(format!("{} omitidos", pending.skipped));
        }
        if changed_lists > 0 {
            notes.push(format!("{changed_lists} en listas"));
        }
        if deleted > 0 {
            notes.push(format!("{deleted} FLAC borrados"));
        }
        if in_use > 0 {
            notes.push(format!("{in_use} en uso, sin borrar"));
        }
        if delete_errors > 0 {
            notes.push(format!("{delete_errors} borrados con error"));
        }
        self.status = format!("Conversión terminada: {}", notes.join(", "));
    }

    fn save_metadata(
        &mut self,
        window: Rid,
        location: (usize, usize),
        metadata: TrackMetadata,
    ) {
        let Some(path) = self
            .playlists
            .get(location.0)
            .and_then(|playlist| playlist.tracks.get(location.1))
            .cloned()
        else {
            self.status = "La pista seleccionada ya no está disponible".to_owned();
            return;
        };
        if self
            .playlists
            .get(location.0)
            .and_then(|playlist| playlist.stream_at(location.1))
            .is_some()
        {
            self.status = "Las pistas en streaming no se editan".to_owned();
            return;
        }

        let was_current = self.current == Some(location);
        let was_playing = self.playing;
        let position = if was_current {
            self.position()
        } else {
            Duration::ZERO
        };
        if was_current {
            if let Some(output) = &self.output {
                output.player.stop();
            }
            self.playing = false;
        }

        let cover_change = self
            .pending_cover
            .as_ref()
            .filter(|(pending_location, _)| *pending_location == location)
            .map(|(_, change)| change.clone());
        let cover_was_changed = cover_change.is_some();

        let mut repaired = false;
        let mut repair_warning = None;
        let mut save_error = None;
        match write_track_metadata(&path, &metadata, cover_change.clone()) {
            Ok(()) => {}
            Err(error) if error.can_repair() && ask_repair_confirmation(window, error.message()) => {
                repaired = true;
                match self.repair_track(window, &path, &metadata, cover_change) {
                    Ok(warning) => repair_warning = warning,
                    Err(error) => save_error = Some(error),
                }
            }
            Err(error) => save_error = Some(error.message().to_owned()),
        }

        let restore_result = if was_current {
            self.play_track(location.0, location.1, was_playing)
                .and_then(|()| {
                    if position.is_zero() {
                        Ok(())
                    } else {
                        self.output
                            .as_ref()
                            .ok_or_else(|| "No hay una salida de audio disponible".to_owned())?
                            .player
                            .try_seek(position)
                            .map_err(|error| format!("No se pudo restaurar la posición: {error}"))
                    }
                })
        } else {
            Ok(())
        };

        match (save_error, restore_result) {
            (None, Ok(())) => {
                if !repaired {
                    self.playlists[location.0].revision += 1;
                }
                if self.current == Some(location) {
                    self.current_metadata = Some(metadata);
                }
                self.pending_cover = None;
                if cover_was_changed {
                    self.cover_hint = "Carátula guardada".to_owned();
                }
                if let Some(warning) = repair_warning {
                    self.metadata_hint = warning;
                    self.status = "Archivo reparado; copia original guardada".to_owned();
                } else if repaired || self.is_pending_repair(&path) {
                    self.metadata_hint = PENDING_REPAIR_HINT.to_owned();
                    self.status = if repaired {
                        "Archivo reparado; copia original guardada".to_owned()
                    } else {
                        "Metadatos guardados".to_owned()
                    };
                } else {
                    self.metadata_hint = "Etiquetas guardadas en el archivo".to_owned();
                    self.status = "Metadatos guardados".to_owned();
                }
            }
            (None, Err(error)) => {
                self.metadata_hint = repair_warning
                    .unwrap_or_else(|| "Etiquetas guardadas; revisa la reproducción".to_owned());
                self.status = error;
            }
            (Some(error), _) => {
                self.metadata_hint = error.clone();
                self.status = error;
            }
        }
    }

    fn repair_track(
        &mut self,
        window: Rid,
        path: &Path,
        metadata: &TrackMetadata,
        cover_change: Option<CoverChange>,
    ) -> Result<Option<String>, String> {
        let existing_backup = self
            .pending_repair_for(path)
            .map(|pending| pending.backup.clone());
        let backup = match existing_backup {
            Some(backup) => backup,
            None => {
                let backup = repair_backup_path(path);
                if let Some(parent) = backup.parent() {
                    fs::create_dir_all(parent).map_err(|error| {
                        format!("No se pudo crear la carpeta de reparaciones: {error}")
                    })?;
                }
                fs::copy(path, &backup).map_err(|error| {
                    format!("No se pudo guardar una copia del original: {error}")
                })?;
                backup
            }
        };

        let temporary = match repair_audio_file(path) {
            Ok(temporary) => temporary,
            Err(error) => {
                if self.pending_repair_for(path).is_none() {
                    let _ = fs::remove_file(&backup);
                }
                return Err(error);
            }
        };
        if let Err(error) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            if self.pending_repair_for(path).is_none() {
                let _ = fs::remove_file(&backup);
            }
            return Err(format!(
                "No se pudo sustituir el archivo original: {error}"
            ));
        }

        // Verificación: intentar guardar metadatos. Incluso sin cambios, escribirlos
        // debe detectar contenedores/etiquetas que siguen dañados tras el remux normal.
        let warning = match write_track_metadata(path, metadata, cover_change.clone()) {
            Ok(()) => None,
            Err(error) => {
                if error.can_repair()
                    && ask_aggressive_repair_confirmation(window, error.message())
                {
                    let aggressive_temporary = repair_audio_file_aggressive(path).map_err(|aggressive_error| {
                        format!(
                            "La reparación normal no bastó ({}) y la reparación agresiva también falló: {}",
                            error.message(),
                            aggressive_error
                        )
                    })?;
                    if let Err(rename_error) = fs::rename(&aggressive_temporary, path) {
                        let _ = fs::remove_file(&aggressive_temporary);
                        return Err(format!(
                            "No se pudo sustituir el archivo tras la reparación agresiva: {rename_error}"
                        ));
                    }
                    write_track_metadata(path, metadata, cover_change).err().map(|write_error| {
                        format!(
                            "El archivo se reparó de forma agresiva, pero no se pudieron guardar los metadatos: {}",
                            write_error.message()
                        )
                    })
                } else {
                    Some(format!(
                        "El archivo se reparó, pero no se pudieron reaplicar los metadatos: {}",
                        error.message()
                    ))
                }
            }
        };

        if self.pending_repair_for(path).is_none() {
            self.pending_repairs.push(PendingRepair {
                path: path.to_path_buf(),
                backup,
            });
            self.save_pending_repairs();
        }
        self.bump_playlists_for(path);
        let is_current = self
            .current
            .and_then(|location| self.path_at(location))
            .is_some_and(|current| current == path);
        if is_current {
            self.current_metadata = Some(read_track_metadata(path));
            self.cover_rendered_for = None;
        }
        Ok(warning)
    }

    fn restore_repair(&mut self, location: (usize, usize)) {
        let Some(path) = self.path_at(location).map(Path::to_path_buf) else {
            self.status = "La pista seleccionada ya no está disponible".to_owned();
            return;
        };
        let Some(pending) = self.pending_repair_for(&path).cloned() else {
            self.status = "Esta pista no tiene una reparación pendiente".to_owned();
            return;
        };

        let was_current = self.current == Some(location);
        let was_playing = self.playing;
        let position = if was_current {
            self.position()
        } else {
            Duration::ZERO
        };
        if was_current {
            if let Some(output) = &self.output {
                output.player.stop();
            }
            self.playing = false;
        }

        let restored = fs::copy(&pending.backup, &path)
            .map(|_| ())
            .map_err(|error| format!("No se pudo restaurar el archivo original: {error}"));
        if restored.is_ok() {
            let _ = fs::remove_file(&pending.backup);
            self.pending_repairs.retain(|entry| entry.path != path);
            self.save_pending_repairs();
            self.bump_playlists_for(&path);
            if self.current == Some(location) {
                self.current_metadata = Some(read_track_metadata(&path));
                self.cover_rendered_for = None;
            }
            if self
                .pending_cover
                .as_ref()
                .is_some_and(|(pending_location, _)| *pending_location == location)
            {
                self.pending_cover = None;
            }
            self.editor_loaded_for = None;
        }

        let resumed = if was_current {
            self.play_track(location.0, location.1, was_playing)
                .and_then(|()| {
                    if position.is_zero() {
                        Ok(())
                    } else {
                        self.output
                            .as_ref()
                            .ok_or_else(|| "No hay una salida de audio disponible".to_owned())?
                            .player
                            .try_seek(position)
                            .map_err(|error| format!("No se pudo restaurar la posición: {error}"))
                    }
                })
        } else {
            Ok(())
        };

        match (restored, resumed) {
            (Ok(()), Ok(())) => {
                self.metadata_hint = "Archivo original restaurado".to_owned();
                self.status = "Archivo original restaurado".to_owned();
            }
            (Ok(()), Err(error)) => self.status = error,
            (Err(error), _) => {
                self.metadata_hint = error.clone();
                self.status = error;
            }
        }
    }

    fn confirm_repair(&mut self, location: (usize, usize)) {
        let Some(path) = self.path_at(location).map(Path::to_path_buf) else {
            self.status = "La pista seleccionada ya no está disponible".to_owned();
            return;
        };
        let Some(pending) = self.pending_repair_for(&path).cloned() else {
            self.status = "Esta pista no tiene una reparación pendiente".to_owned();
            return;
        };

        let _ = fs::remove_file(&pending.backup);
        self.pending_repairs.retain(|entry| entry.path != path);
        self.save_pending_repairs();
        self.bump_playlists_for(&path);
        self.editor_loaded_for = None;
        self.metadata_hint = "Versión reparada confirmada como definitiva".to_owned();
        self.status = "Versión reparada confirmada".to_owned();
    }

    fn path_at(&self, location: (usize, usize)) -> Option<&Path> {
        self.playlists
            .get(location.0)?
            .tracks
            .get(location.1)
            .map(PathBuf::as_path)
    }

    fn pending_repair_for(&self, path: &Path) -> Option<&PendingRepair> {
        self.pending_repairs
            .iter()
            .find(|pending| pending.path == path)
    }

    fn is_pending_repair(&self, path: &Path) -> bool {
        self.pending_repair_for(path).is_some()
    }

    fn bump_playlists_for(&mut self, path: &Path) {
        for playlist in &mut self.playlists {
            if playlist.tracks.iter().any(|track| track == path) {
                playlist.revision += 1;
            }
        }
    }

    fn save_pending_repairs(&mut self) {
        let path = repairs_store_path();
        let Some(parent) = path.parent() else {
            return;
        };
        let result = (|| -> Result<(), String> {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            let temporary = path.with_extension("json.tmp");
            let file = File::create(&temporary).map_err(|error| error.to_string())?;
            serde_json::to_writer_pretty(file, &self.pending_repairs)
                .map_err(|error| error.to_string())?;
            fs::rename(&temporary, &path).map_err(|error| error.to_string())
        })();
        if let Err(error) = result {
            self.status = format!("No se pudo guardar el estado de las reparaciones: {error}");
        }
    }

    /// Recuerda la carpeta del archivo elegido, en memoria y en disco. Si el
    /// portal de documentos devolvió una ruta efímera, se guarda la carpeta
    /// real que el portal anota en el archivo.
    fn remember_cover_directory(&mut self, file: &Path) {
        let host = document_portal_host_path(file).unwrap_or_else(|| file.to_path_buf());
        let Some(parent) = host.parent() else {
            return;
        };
        if is_document_portal_path(parent) {
            return;
        }
        self.last_cover_dir = Some(parent.to_path_buf());
        if let Err(error) = self.persist_settings() {
            self.status = format!("No se pudo guardar la última carpeta de carátulas: {error}");
        }
    }

    fn set_open_behavior(&mut self, behavior: OpenBehavior) -> Result<(), String> {
        self.open_behavior = behavior;
        self.persist_settings()
    }

    fn set_add_behavior(&mut self, behavior: AddBehavior) -> Result<(), String> {
        self.add_behavior = behavior;
        self.persist_settings()
    }

    /// El cambio afecta a la siguiente reproducción: la pista que ya suena no
    /// se interrumpe al aplicar el ajuste.
    fn set_youtube_playback(&mut self, mode: YoutubePlayback) -> Result<(), String> {
        self.youtube_playback = mode;
        self.persist_settings()
    }

    fn set_download_dir(&mut self, dir: Option<PathBuf>) -> Result<(), String> {
        self.download_dir = dir;
        self.persist_settings()
    }

    /// Entra o sale del buscador de YouTube. No toca listas ni reproducción:
    /// es solo un cambio de vista. Al salir se descartan los resultados, porque
    /// el buscador no guarda nada.
    fn toggle_youtube_mode(&mut self) {
        self.youtube_mode = !self.youtube_mode;
        if !self.youtube_mode {
            self.clear_youtube_results();
        }
        // La lista de la izquierda cambia de contenido: hay que forzar el
        // renderizado aunque la lista activa no haya cambiado de revisión.
        self.ui_rendered_playlist = None;
        self.status = if self.youtube_mode {
            "Modo YouTube: la lista está desactivada; escribe una búsqueda y pulsa Enter".to_owned()
        } else {
            "Modo YouTube desactivado: vuelve la lista activa".to_owned()
        };
    }

    fn clear_youtube_results(&mut self) {
        self.reset_youtube_playback();
        self.youtube_results.clear();
        self.youtube_query.clear();
        self.youtube_messages = None;
        self.youtube_generation = self.youtube_generation.wrapping_add(1);
        self.youtube_selected = None;
        self.youtube_hint = "Busca en YouTube… los resultados aparecerán aquí".to_owned();
    }

    /// Detiene cualquier audio de YouTube y descarta la descarga en curso. Se
    /// usa al salir del buscador, al cambiar de lista y al lanzar otra búsqueda,
    /// para que un resultado nunca siga sonando fuera del modo buscador.
    fn reset_youtube_playback(&mut self) {
        let has_youtube_audio = self.youtube_current.is_some()
            || self.youtube_pending_audio.is_some()
            || self.youtube_audio.is_some()
            || self.youtube_audio_path.is_some();
        if has_youtube_audio {
            if let Some(output) = &self.output {
                output.player.clear();
            }
            if self.current_metadata.take().is_some() {
                // Los textos «suena ahora» pertenecían al resultado anterior.
                self.duration = None;
            }
            self.playing = false;
            self.stopped = true;
        }
        self.youtube_audio = None;
        self.youtube_pending_audio = None;
        self.youtube_audio_generation = self.youtube_audio_generation.wrapping_add(1);
        self.youtube_current = None;
        if let Some(path) = self.youtube_audio_path.take() {
            let _ = fs::remove_file(path);
        }
        self.stream_pipe_active = false;
    }

    /// Lanza una búsqueda en un hilo aparte. La generación evita que una
    /// búsqueda antigua sobrescriba los resultados de la última consulta.
    fn start_youtube_search(&mut self, query: String) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            self.status = "Escribe algo para buscar en YouTube".to_owned();
            return;
        }
        if !self.youtube_mode {
            self.youtube_mode = true;
        }
        self.reset_youtube_playback();
        self.youtube_generation = self.youtube_generation.wrapping_add(1);
        let generation = self.youtube_generation;
        self.youtube_results.clear();
        self.youtube_query = query.clone();
        self.youtube_hint = "Buscando en YouTube…".to_owned();
        self.status = format!("YouTube: buscando «{query}»…");
        self.ui_rendered_playlist = None;
        match youtube::start_search(&query, generation) {
            Ok(messages) => self.youtube_messages = Some(messages),
            Err(error) => {
                self.youtube_messages = None;
                self.youtube_hint = error.clone();
                self.status = error;
            }
        }
    }

    /// Sondea el canal del hilo de búsqueda desde el bucle de la interfaz, igual
    /// que se hace con las actualizaciones y los comandos MPRIS.
    fn poll_youtube_search(&mut self) {
        let message = self.youtube_messages.as_ref().map(Receiver::try_recv);
        match message {
            Some(Ok(youtube::YoutubeMessage::Searching { generation }))
                if generation == self.youtube_generation =>
            {
                self.youtube_hint = "Buscando en YouTube…".to_owned();
                self.ui_rendered_playlist = None;
            }
            Some(Ok(youtube::YoutubeMessage::Results { generation, tracks }))
                if generation == self.youtube_generation =>
            {
                self.youtube_messages = None;
                self.youtube_results = tracks;
                let count = self.youtube_results.len();
                self.youtube_hint = if count == 0 {
                    "Sin resultados. Prueba con otra búsqueda".to_owned()
                } else {
                    String::new()
                };
                self.status = format!(
                    "YouTube: {count} resultado{} para «{}»",
                    if count == 1 { "" } else { "s" },
                    self.youtube_query
                );
                self.ui_rendered_playlist = None;
            }
            Some(Ok(youtube::YoutubeMessage::Error { generation, message }))
                if generation == self.youtube_generation =>
            {
                self.youtube_messages = None;
                self.youtube_hint = message.clone();
                self.status = message;
                self.ui_rendered_playlist = None;
            }
            // Mensajes de una búsqueda ya sustituida: se ignoran.
            Some(Ok(_)) => {}
            Some(Err(TryRecvError::Disconnected)) => {
                self.youtube_messages = None;
                if !self.youtube_results.is_empty() {
                    self.status = format!(
                        "YouTube: {} resultado{} para «{}»",
                        self.youtube_results.len(),
                        if self.youtube_results.len() == 1 { "" } else { "s" },
                        self.youtube_query
                    );
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// Raven no avisa de cada cambio de selección, así que se lee en cada
    /// fotograma. En modo buscador esa fila nunca se copia a una lista.
    fn sync_youtube_selection(&mut self, selected: i32) {
        if !self.youtube_mode {
            return;
        }
        self.youtube_selected = if selected >= 0 && (selected as usize) < self.youtube_results.len()
        {
            Some(selected as usize)
        } else {
            // Raven devuelve -1 justo después de repintar la lista; mientras
            // siga sonando un resultado, ese es el marcador válido.
            self.youtube_current
        };
    }

    /// En el buscador manda la fila seleccionada: así la carátula cambia en
    /// cuanto el usuario elige otro resultado, aunque esté sonando otro.
    fn displayed_youtube_index(&self) -> Option<usize> {
        self.youtube_selected.or(self.youtube_current)
    }

    fn youtube_step_index(length: usize, from: usize, forward: bool) -> Option<usize> {
        if length == 0 {
            return None;
        }
        let from = from.min(length - 1);
        Some(if forward {
            (from + 1) % length
        } else {
            (from + length - 1) % length
        })
    }

    /// Entrega una fuente al reproductor. La usan tanto el Opus en caché como
    /// el pipe AAC en streaming; en este último caso la barra será informativa.
    fn append_audio_source(&mut self, source: Box<dyn Source + Send>) -> Result<(), String> {
        let output = self
            .output
            .as_ref()
            .ok_or_else(|| "No hay un dispositivo de salida de audio disponible".to_owned())?;
        output.player.clear();
        if output.player.empty() {
            let _ = output.player.try_seek(Duration::ZERO);
        }
        output.player.set_volume(self.volume);
        output.player.append(source);
        output.player.play();
        Ok(())
    }

    /// Reproduce un resultado sin tocar `playlists` ni `active_playlist`. Según
    /// el ajuste, abre el pipe AAC en streaming o deja un Opus en la caché.
    fn play_youtube_result(&mut self, selected: Option<usize>) {
        if !self.youtube_mode {
            return;
        }
        let Some(index) = selected
            .or(self.youtube_selected)
            .or(self.youtube_current)
            .or_else(|| (!self.youtube_results.is_empty()).then_some(0))
        else {
            self.status = "Busca un resultado para reproducirlo".to_owned();
            return;
        };
        let Some(track) = self.youtube_results.get(index).cloned() else {
            self.status = "Ese resultado ya no está disponible".to_owned();
            return;
        };
        self.youtube_selected = Some(index);

        // Si es el mismo resultado y sigue cargado, Play solo reanuda. Si ya
        // terminó, se reutiliza el Opus descargado en vez de volver a bajarlo.
        {
            let Some(output) = &self.output else {
                self.status = "No hay un dispositivo de salida de audio disponible".to_owned();
                return;
            };
            if self.youtube_current == Some(index) && self.youtube_pending_audio.is_none() {
                if !output.player.empty() {
                    output.player.play();
                    self.playing = true;
                    self.stopped = false;
                    self.cycle_finished = false;
                    self.status = "Reproduciendo desde YouTube".to_owned();
                    return;
                }
                if let Some(path) = self.youtube_audio_path.clone().filter(|path| path.is_file()) {
                    match opus::source_for_path(&path) {
                        Ok(source) => {
                            let duration = source.total_duration().or(track.duration);
                            output.player.clear();
                            if output.player.empty() {
                                let _ = output.player.try_seek(Duration::ZERO);
                            }
                            output.player.set_volume(self.volume);
                            output.player.append(source);
                            output.player.play();
                            self.youtube_current = Some(index);
                            self.youtube_selected = Some(index);
                            self.current = None;
                            self.current_metadata = Some(TrackMetadata {
                                title: track.title,
                                artist: track.channel,
                                duration,
                                ..TrackMetadata::default()
                            });
                            self.duration = duration;
                            self.playing = true;
                            self.stopped = false;
                            self.cycle_finished = false;
                            self.status = "Reproduciendo desde YouTube".to_owned();
                            return;
                        }
                        Err(error) => {
                            self.status =
                                format!("No se pudo volver a abrir el audio de YouTube: {error}");
                            return;
                        }
                    }
                }
            }
            output.player.clear();
        }

        // Un streaming de la lista podría seguir en el reproductor: al pasar a
        // un resultado de YouTube se descarta su audio temporal.
        self.reset_stream_playback();
        self.youtube_audio = None;
        self.youtube_pending_audio = None;
        self.youtube_audio_generation = self.youtube_audio_generation.wrapping_add(1);
        let generation = self.youtube_audio_generation;
        self.youtube_current = None;
        // `current` apunta a una pista local; si se deja ahí, Siguiente o el
        // avance automático intentarían seguir la lista que había abierta.
        self.current = None;
        self.current_metadata = Some(TrackMetadata {
            title: track.title.clone(),
            artist: track.channel.clone(),
            duration: track.duration,
            ..TrackMetadata::default()
        });
        self.duration = track.duration;
        self.playing = false;
        self.stopped = true;
        self.cycle_finished = false;
        self.stream_pipe_active = false;
        self.status = if self.youtube_playback == YoutubePlayback::Streaming {
            format!("YouTube: preparando audio de «{}»…", track.title)
        } else {
            format!("YouTube: descargando audio de «{}»…", track.title)
        };
        match youtube::start_audio_auto(track, generation, index, self.youtube_playback) {
            Ok(receiver) => {
                self.youtube_audio = Some(receiver);
                self.youtube_pending_audio = Some((generation, index));
            }
            Err(error) => {
                self.current_metadata = None;
                self.duration = None;
                self.status = error;
            }
        }
    }

    /// Recoge el resultado de la descarga de audio y lo entrega a `rodio` sin
    /// escribir nada en las listas ni en la configuración.
    fn poll_youtube_audio(&mut self) {
        let message = self.youtube_audio.as_ref().map(Receiver::try_recv);
        match message {
            Some(Ok(youtube::YoutubeAudioMessage::Ready {
                generation,
                index,
                track,
                path,
            })) if self.youtube_pending_audio == Some((generation, index)) => {
                self.youtube_audio = None;
                self.youtube_pending_audio = None;
                let source = match opus::source_for_path(&path) {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = fs::remove_file(&path);
                        self.current_metadata = None;
                        self.duration = None;
                        self.status = format!("No se pudo reproducir el audio de YouTube: {error}");
                        self.playing = false;
                        self.stopped = true;
                        return;
                    }
                };
                let duration = source.total_duration().or(track.duration);
                if let Err(error) = self.append_audio_source(source) {
                    self.current_metadata = None;
                    self.duration = None;
                    self.status = error;
                    self.playing = false;
                    self.stopped = true;
                    return;
                }
                self.stream_pipe_active = false;
                if let Some(previous) = self.youtube_audio_path.replace(path) {
                    let _ = fs::remove_file(previous);
                }
                self.youtube_current = Some(index);
                self.youtube_selected = Some(index);
                self.current = None;
                self.current_metadata = Some(TrackMetadata {
                    title: track.title,
                    artist: track.channel,
                    duration,
                    ..TrackMetadata::default()
                });
                self.duration = duration;
                self.playing = true;
                self.stopped = false;
                self.cycle_finished = false;
                self.status = "Reproduciendo desde YouTube".to_owned();
            }
            Some(Ok(youtube::YoutubeAudioMessage::StreamReady {
                generation,
                index,
                track,
                source,
                duration,
            })) if self.youtube_pending_audio == Some((generation, index)) => {
                self.youtube_audio = None;
                self.youtube_pending_audio = None;
                let duration = duration.or(track.duration);
                if let Err(error) = self.append_audio_source(Box::new(source)) {
                    self.current_metadata = None;
                    self.duration = None;
                    self.status = error;
                    self.playing = false;
                    self.stopped = true;
                    return;
                }
                self.stream_pipe_active = true;
                if let Some(previous) = self.youtube_audio_path.take() {
                    let _ = fs::remove_file(previous);
                }
                self.youtube_current = Some(index);
                self.youtube_selected = Some(index);
                self.current = None;
                self.current_metadata = Some(TrackMetadata {
                    title: track.title,
                    artist: track.channel,
                    duration,
                    ..TrackMetadata::default()
                });
                self.duration = duration;
                self.playing = true;
                self.stopped = false;
                self.cycle_finished = false;
                self.status = "Reproduciendo desde YouTube en streaming".to_owned();
            }
            Some(Ok(youtube::YoutubeAudioMessage::Error {
                generation,
                index,
                message,
            })) if self.youtube_pending_audio == Some((generation, index)) => {
                self.youtube_audio = None;
                self.youtube_pending_audio = None;
                self.current_metadata = None;
                self.duration = None;
                self.playing = false;
                self.stopped = true;
                self.stream_pipe_active = false;
                self.status = message;
            }
            Some(Ok(_)) => {}
            Some(Err(TryRecvError::Disconnected)) => {
                self.youtube_audio = None;
                if self.youtube_pending_audio.take().is_some() {
                    self.current_metadata = None;
                    self.duration = None;
                    self.playing = false;
                    self.stopped = true;
                    self.stream_pipe_active = false;
                    self.status = "La descarga del audio de YouTube se interrumpió".to_owned();
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// Añade el resultado elegido a la lista activa guardando solo la URL. Es
    /// una acción explícita: el buscador sigue sin persistir nada por sí mismo.
    fn add_youtube_to_playlist(&mut self, selected: Option<usize>) {
        let index = selected
            .or(self.youtube_selected)
            .or(self.youtube_current)
            .or_else(|| (!self.youtube_results.is_empty()).then_some(0));
        let Some(index) = index else {
            self.status = "Selecciona un resultado para añadirlo".to_owned();
            return;
        };
        let Some(track) = self.youtube_results.get(index).cloned() else {
            self.status = "Ese resultado ya no está disponible".to_owned();
            return;
        };
        let playlist_index = self.active_playlist;
        let Some(playlist) = self.playlists.get(playlist_index) else {
            return;
        };
        let name = playlist.name.clone();
        if playlist
            .streams
            .iter()
            .flatten()
            .any(|entry| entry.url == track.url)
        {
            self.status = format!("«{}» ya está en la lista {name}", track.title);
            return;
        }
        let new_index = playlist.tracks.len();
        let playlist = &mut self.playlists[playlist_index];
        playlist.tracks.push(PathBuf::from(&track.url));
        playlist
            .streams
            .push(Some(StreamingTrack::from_youtube(&track)));
        playlist.selected = Some(new_index);
        playlist.revision += 1;
        self.save_playlists();
        self.status = format!("Añadido «{}» a {name} (streaming)", track.title);
    }

    /// Descarga explícita en Opus a la carpeta configurada en Ajustes.
    fn download_youtube_track(&mut self, selected: Option<usize>) {
        let index = selected
            .or(self.youtube_selected)
            .or(self.youtube_current)
            .or_else(|| (!self.youtube_results.is_empty()).then_some(0));
        let Some(index) = index else {
            self.status = "Selecciona un resultado para descargarlo".to_owned();
            return;
        };
        let Some(track) = self.youtube_results.get(index).cloned() else {
            self.status = "Ese resultado ya no está disponible".to_owned();
            return;
        };
        let Some(directory) = self.download_dir.clone().or_else(default_download_dir) else {
            self.status = "Configura una carpeta de descargas en Ajustes".to_owned();
            return;
        };
        self.youtube_download_generation = self.youtube_download_generation.wrapping_add(1);
        let generation = self.youtube_download_generation;
        self.status = format!("YouTube: descargando «{}» en Opus…", track.title);
        match youtube::start_download(track, directory, generation) {
            Ok(receiver) => self.youtube_downloads = Some(receiver),
            Err(error) => {
                self.youtube_downloads = None;
                self.status = error;
            }
        }
    }

    fn poll_youtube_downloads(&mut self) {
        let message = self.youtube_downloads.as_ref().map(Receiver::try_recv);
        match message {
            Some(Ok(youtube::YoutubeDownloadMessage::Done {
                generation,
                title,
                path,
            })) if generation == self.youtube_download_generation => {
                self.youtube_downloads = None;
                self.status = format!("Descargado «{title}»: {}", path.display());
            }
            Some(Ok(youtube::YoutubeDownloadMessage::Error {
                generation, message,
            })) if generation == self.youtube_download_generation => {
                self.youtube_downloads = None;
                self.status = message;
            }
            Some(Ok(_)) => {}
            Some(Err(TryRecvError::Disconnected)) => {
                self.youtube_downloads = None;
                if self.youtube_download_generation > 0 {
                    self.status = "La descarga se interrumpió".to_owned();
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    /// Prepara una pista guardada como URL. Igual que con los resultados del
    /// buscador, respeta el ajuste de streaming/descarga; aquí además se
    /// conserva la posición `(lista, pista)` para que Siguiente funcione.
    fn play_streaming_track(
        &mut self,
        location: (usize, usize),
        stream: StreamingTrack,
    ) -> Result<(), String> {
        if self.output.is_none() {
            return Err("No hay un dispositivo de salida de audio disponible".to_owned());
        }
        self.reset_stream_playback();
        {
            let output = self.output.as_ref().unwrap();
            output.player.clear();
            if output.player.empty() {
                let _ = output.player.try_seek(Duration::ZERO);
            }
            output.player.set_volume(self.volume);
        }
        self.stream_generation = self.stream_generation.wrapping_add(1);
        let generation = self.stream_generation;
        self.current = Some(location);
        self.current_metadata = Some(stream.metadata());
        self.duration = stream.duration_secs.map(Duration::from_secs);
        self.playing = false;
        self.stopped = true;
        self.cycle_finished = false;
        self.status = if self.youtube_playback == YoutubePlayback::Streaming {
            format!("Preparando audio de «{}» en streaming…", stream.title)
        } else {
            format!(
                "Descargando audio de «{}» para reproducir en streaming…",
                stream.title
            )
        };
        let track = youtube::YoutubeTrack {
            id: stream.url.clone(),
            title: stream.title,
            channel: stream.artist,
            duration: self.duration,
            thumbnail: stream.thumbnail,
            thumbnail_data: None,
            url: stream.url,
            live: false,
        };
        self.stream_pipe_active = false;
        match youtube::start_audio_auto(track, generation, 0, self.youtube_playback) {
            Ok(receiver) => {
                self.stream_audio = Some(receiver);
                self.stream_pending = Some((location, generation));
                Ok(())
            }
            Err(error) => {
                self.current_metadata = None;
                self.duration = None;
                Err(error)
            }
        }
    }

    fn poll_stream_audio(&mut self) {
        let message = self.stream_audio.as_ref().map(Receiver::try_recv);
        match message {
            Some(Ok(youtube::YoutubeAudioMessage::Ready {
                generation,
                track,
                path,
                ..
            })) if self
                .stream_pending
                .is_some_and(|(location, pending)| pending == generation && Some(location) == self.current) =>
            {
                let location = match self.stream_pending.take() {
                    Some((location, _)) => location,
                    None => return,
                };
                self.stream_audio = None;
                let source = match opus::source_for_path(&path) {
                    Ok(source) => source,
                    Err(error) => {
                        let _ = fs::remove_file(&path);
                        self.current_metadata = None;
                        self.duration = None;
                        self.playing = false;
                        self.stopped = true;
                        self.status = format!("No se pudo reproducir el streaming: {error}");
                        return;
                    }
                };
                let duration = source.total_duration().or(track.duration);
                if let Err(error) = self.append_audio_source(source) {
                    self.current_metadata = None;
                    self.duration = None;
                    self.playing = false;
                    self.stopped = true;
                    self.status = error;
                    return;
                }
                self.stream_pipe_active = false;
                if let Some(previous) = self.stream_audio_path.replace(path) {
                    let _ = fs::remove_file(previous);
                }
                self.stream_current = Some(location);
                self.current = Some(location);
                self.current_metadata = Some(TrackMetadata {
                    title: track.title,
                    artist: track.channel,
                    duration,
                    ..TrackMetadata::default()
                });
                self.duration = duration;
                self.playing = true;
                self.stopped = false;
                self.cycle_finished = false;
                self.status = "Reproduciendo en streaming".to_owned();
            }
            Some(Ok(youtube::YoutubeAudioMessage::StreamReady {
                generation,
                track,
                source,
                duration,
                ..
            })) if self
                .stream_pending
                .is_some_and(|(location, pending)| pending == generation && Some(location) == self.current) =>
            {
                let location = match self.stream_pending.take() {
                    Some((location, _)) => location,
                    None => return,
                };
                self.stream_audio = None;
                let duration = duration.or(track.duration);
                if let Err(error) = self.append_audio_source(Box::new(source)) {
                    self.current_metadata = None;
                    self.duration = None;
                    self.playing = false;
                    self.stopped = true;
                    self.status = error;
                    return;
                }
                self.stream_pipe_active = true;
                if let Some(previous) = self.stream_audio_path.take() {
                    let _ = fs::remove_file(previous);
                }
                self.stream_current = Some(location);
                self.current = Some(location);
                self.current_metadata = Some(TrackMetadata {
                    title: track.title,
                    artist: track.channel,
                    duration,
                    ..TrackMetadata::default()
                });
                self.duration = duration;
                self.playing = true;
                self.stopped = false;
                self.cycle_finished = false;
                self.status = "Reproduciendo en streaming (AAC)".to_owned();
            }
            Some(Ok(youtube::YoutubeAudioMessage::Error {
                generation, message, ..
            })) if self
                .stream_pending
                .is_some_and(|(_, pending)| pending == generation) =>
            {
                self.stream_audio = None;
                self.stream_pending = None;
                self.current_metadata = None;
                self.duration = None;
                self.playing = false;
                self.stopped = true;
                self.stream_pipe_active = false;
                self.status = message;
            }
            Some(Ok(_)) => {}
            Some(Err(TryRecvError::Disconnected)) => {
                self.stream_audio = None;
                if self.stream_pending.take().is_some() {
                    self.current_metadata = None;
                    self.duration = None;
                    self.playing = false;
                    self.stopped = true;
                    self.stream_pipe_active = false;
                    self.status = "La descarga del streaming se interrumpió".to_owned();
                }
            }
            Some(Err(TryRecvError::Empty)) | None => {}
        }
    }

    fn reset_stream_playback(&mut self) {
        let has_stream_audio = self.stream_audio.is_some()
            || self.stream_pending.is_some()
            || self.stream_current.is_some()
            || self.stream_audio_path.is_some();
        if has_stream_audio {
            if self.stream_current.is_some() {
                if let Some(output) = &self.output {
                    output.player.clear();
                }
            }
            self.playing = false;
            self.stopped = true;
        }
        self.stream_audio = None;
        self.stream_pending = None;
        self.stream_generation = self.stream_generation.wrapping_add(1);
        self.stream_current = None;
        if let Some(path) = self.stream_audio_path.take() {
            let _ = fs::remove_file(path);
        }
        self.stream_pipe_active = false;
    }

    fn export_playlist(&mut self, path: &Path) {
        let Some(playlist) = self.playlists.get(self.active_playlist) else {
            self.status = "No hay lista activa para exportar".to_owned();
            return;
        };
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("m3u8")
            .to_lowercase();
        let result = match ext.as_str() {
            "xspf" => export_playlist_xspf(playlist, path),
            _ => export_playlist_m3u8(playlist, path),
        };
        match result {
            Ok(()) => self.status = format!("Lista exportada a {}", path.display()),
            Err(e) => self.status = e,
        }
    }

    fn import_playlist(&mut self, path: &Path) {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .unwrap_or("m3u8")
            .to_lowercase();
        let result = match ext.as_str() {
            "xspf" => import_playlist_xspf(path),
            _ => import_playlist_m3u8(path),
        };
        match result {
            Ok((name, tracks)) => {
                if tracks.is_empty() {
                    self.status = "La lista importada no contiene pistas".to_owned();
                    return;
                }
                let mut playlist = Playlist::new(name.clone());
                playlist.tracks = tracks;
                playlist.rebuild_order(None);
                self.playlists.push(playlist);
                self.active_playlist = self.playlists.len() - 1;
                self.ui_rendered_playlist = None;
                self.editor_loaded_for = None;
                self.status = format!("Lista importada: {} ({} pistas)", name, self.playlists[self.active_playlist].tracks.len());
                self.save_playlists();
            }
            Err(e) => self.status = e,
        }
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

fn playlist_store_path() -> PathBuf {
    config_dir().join("freeway-player").join("playlists.json")
}

fn settings_store_path() -> PathBuf {
    config_dir().join("freeway-player").join("settings.json")
}

fn load_playlists() -> (Vec<SavedPlaylist>, usize) {
    let Ok(file) = File::open(playlist_store_path()) else {
        return (Vec::new(), 0);
    };
    let Ok(store) = serde_json::from_reader::<_, PlaylistStore>(file) else {
        return (Vec::new(), 0);
    };
    (store.playlists, store.active_playlist)
}

fn load_settings() -> SavedSettings {
    let Ok(file) = File::open(settings_store_path()) else {
        return SavedSettings::default();
    };
    serde_json::from_reader(file).unwrap_or_default()
}

fn save_settings(settings: &SavedSettings) -> Result<(), String> {
    let path = settings_store_path();
    let Some(parent) = path.parent() else {
        return Err("No se pudo determinar la carpeta de configuración".to_owned());
    };
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    let temporary = path.with_extension("json.tmp");
    let file = File::create(&temporary).map_err(|error| error.to_string())?;
    serde_json::to_writer_pretty(file, settings).map_err(|error| error.to_string())?;
    fs::rename(&temporary, &path).map_err(|error| error.to_string())
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .unwrap_or_else(std::env::temp_dir)
}

fn repairs_dir() -> PathBuf {
    cache_dir().join("freeway-player").join("repairs")
}

fn repairs_store_path() -> PathBuf {
    cache_dir().join("freeway-player").join("repairs.json")
}

fn load_pending_repairs() -> Vec<PendingRepair> {
    let Ok(file) = File::open(repairs_store_path()) else {
        return Vec::new();
    };
    let Ok(entries) = serde_json::from_reader::<_, Vec<PendingRepair>>(file) else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter(|pending| pending.backup.is_file())
        .collect()
}

/// Nombre estable en la caché para la copia del original. El hash evita que
/// dos archivos con el mismo nombre en carpetas distintas compartan copia.
fn repair_backup_path(path: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_owned());
    repairs_dir().join(format!("{:016x}-{name}", hasher.finish()))
}

/// Remuxa el archivo a un temporal junto al original. `-c copy` no recodifica,
/// así que la reparación es rápida y conserva la calidad y los metadatos.
fn repair_audio_file(path: &Path) -> Result<PathBuf, String> {
    repair_audio_file_with(path, false)
}

/// Reparación más agresiva: solo conserva el audio y descarta metadatos y
/// contenedores secundarios. Puede arreglar archivos con datos corruptos que el
/// remux normal no soluciona, pero borra los metadatos existentes.
fn repair_audio_file_aggressive(path: &Path) -> Result<PathBuf, String> {
    repair_audio_file_with(path, true)
}

fn repair_audio_file_with(path: &Path, aggressive: bool) -> Result<PathBuf, String> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("audio");
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or_else(|| {
            "El archivo no tiene una extensión que ffmpeg pueda reconocer".to_owned()
        })?;
    let suffix = if aggressive {
        "freeway-repair-aggressive"
    } else {
        "freeway-repair"
    };
    let temporary = parent.join(format!(".{stem}.{suffix}.{extension}"));
    let _ = fs::remove_file(&temporary);

    let mut command = Command::new("ffmpeg");
    command
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(path);
    if aggressive {
        // Mantiene solo audio y descarta metadatos/corrupciones fuera del stream.
        command.arg("-map").arg("0:a").arg("-map_metadata").arg("-1");
    } else {
        command.arg("-c").arg("copy").arg("-map_metadata").arg("0");
    }
    command.arg("-c").arg("copy").arg(&temporary);

    let output = command.output().map_err(|error| {
        format!("No se pudo ejecutar ffmpeg: {error}. Comprueba que está instalado.")
    })?;
    if !output.status.success() {
        let _ = fs::remove_file(&temporary);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        let action = if aggressive {
            "reparar el archivo de forma agresiva"
        } else {
            "reparar el archivo"
        };
        return Err(if detail.is_empty() {
            format!("ffmpeg no pudo {action}")
        } else {
            format!("ffmpeg no pudo {action}: {detail}")
        });
    }

    let metadata = fs::metadata(&temporary)
        .map_err(|error| format!("No se pudo comprobar el archivo reparado: {error}"))?;
    if metadata.len() == 0 {
        let _ = fs::remove_file(&temporary);
        return Err("ffmpeg generó un archivo vacío".to_owned());
    }
    if let Ok(original) = fs::metadata(path) {
        let _ = fs::set_permissions(&temporary, original.permissions());
    }
    Ok(temporary)
}

fn ask_repair_confirmation(window: Rid, error: &str) -> bool {
    confirm_dialog(
        window,
        "Archivo dañado",
        &format!(
            "No se pudieron guardar los metadatos: {error}\n\n\
             ¿Quieres intentar reparar el archivo con ffmpeg? Se guardará una copia del original \
             por si necesitas restaurarlo después."
        ),
        "Reparar",
        "Cancelar",
    )
}

fn ask_aggressive_repair_confirmation(window: Rid, error: &str) -> bool {
    confirm_dialog(
        window,
        "Reparación agresiva",
        &format!(
            "La reparación normal no permitió guardar los metadatos: {error}\n\n\
             Se puede intentar una reparación más agresiva con ffmpeg. Esta opción conserva solo \
             el audio y elimina los metadatos existentes del archivo. Se intentarán volver a \
             guardar los metadatos que has editado, pero la carátula y otras etiquetas \
             originales pueden perderse.\n\n\
             ¿Quieres continuar?"
        ),
        "Reparar y borrar metadatos",
        "Cancelar",
    )
}

/// Aplica un atributo de Raven desde texto cómodo de usar.
fn dialog_attribute(widget: Rid, name: &str, value: &str) {
    let name = cstr(name);
    let value = cstr(value);
    unsafe { Rwidget_set_attribute(widget, name.as_ptr(), value.as_ptr()) };
}

/// Cierra el diálogo con el resultado guardado en `user_data` (0 o 1).
unsafe extern "C" fn close_dialog_with_result(button: Rid, user_data: *mut c_void) {
    Rdialog_close(button, user_data as isize as c_int);
}

/// Enter responde lo mismo que el botón principal, como en `Rmessage_box`.
unsafe extern "C" fn confirm_dialog_key(
    widget: Rid,
    event: *const REvent,
    _user_data: *mut c_void,
) -> c_int {
    let event = &*event;
    if event.type_ != R_EVENT_KEY_DOWN
        || (event.key != R_KEY_ENTER && event.key != R_KEY_KP_ENTER)
    {
        return 0;
    }
    Rdialog_close(widget, 1);
    1
}

/// Diálogo de confirmación con botones en español.
///
/// `Rmessage_box` escribe «Yes»/«No» fijos en inglés (Raven 0.2.1), así que
/// este diálogo reproduce su estructura con las etiquetas que le pasamos.
/// Se cierra con Escape (respuesta 0) o con el botón de confirmar (respuesta 1).
fn confirm_dialog(
    window: Rid,
    title: &str,
    text: &str,
    confirm_label: &str,
    cancel_label: &str,
) -> bool {
    unsafe {
        let dialog = Rwidget_create(R_WIDGET_VBOX);
        Rwidget_set_size(dialog, 380.0, -1.0);
        Rwidget_set_event_callback(dialog, Some(confirm_dialog_key), std::ptr::null_mut());
        dialog_attribute(dialog, "Spacing", "12");

        if !title.is_empty() {
            let heading = Rwidget_create(R_WIDGET_LABEL);
            dialog_attribute(heading, "Text", title);
            dialog_attribute(heading, "TextSize", "17");
            Rwidget_add_child(dialog, heading);
        }

        let message = Rwidget_create(R_WIDGET_LABEL);
        dialog_attribute(message, "Text", text);
        dialog_attribute(message, "Wrap", "true");
        Rwidget_set_opacity(message, if title.is_empty() { 1.0 } else { 0.8 });
        Rwidget_add_child(dialog, message);

        let row = Rwidget_create(R_WIDGET_HBOX);
        dialog_attribute(row, "Spacing", "8");
        Rslot_set_halign(row, R_HALIGN_RIGHT);
        Rslot_set_padding(row, 0.0, 6.0, 0.0, 0.0);
        Rwidget_add_child(dialog, row);

        for (label, primary, result) in [
            (cancel_label, false, 0),
            (confirm_label, true, 1),
        ] {
            let button = Rwidget_create(R_WIDGET_BUTTON);
            dialog_attribute(button, "Text", label);
            if primary {
                dialog_attribute(button, "Style", "Primary");
            }
            Rwidget_set_min_size(button, 84.0, -1.0);
            Rbutton_set_clicked_callback(
                button,
                Some(close_dialog_with_result),
                result as isize as *mut c_void,
            );
            Rwidget_add_child(row, button);
        }

        Rdialog_run(window, dialog) != 0
    }
}

fn read_track_metadata(path: &Path) -> TrackMetadata {
    let fallback_title = path
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Pista sin título".to_owned());
    let fallback = TrackMetadata {
        title: fallback_title,
        ..TrackMetadata::default()
    };
    let Ok(file) = lofty::read_from_path(path) else {
        return fallback;
    };

    let duration = file.properties().duration();
    let tag = file.primary_tag().or_else(|| file.first_tag());
    let Some(tag) = tag else {
        return TrackMetadata {
            duration: (!duration.is_zero()).then_some(duration),
            ..fallback
        };
    };
    TrackMetadata {
        title: tag
            .title()
            .map(|value| value.into_owned())
            .unwrap_or(fallback.title),
        artist: tag
            .artist()
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        album: tag
            .album()
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        genre: tag
            .genre()
            .map(|value| value.into_owned())
            .unwrap_or_default(),
        track: tag
            .track()
            .map(|value| value.to_string())
            .unwrap_or_default(),
        duration: (!duration.is_zero()).then_some(duration),
    }
}

fn read_cover_art(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    let picture = tag
        .get_picture_type(PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    Some(picture.data().to_vec())
}

fn cover_hint_for_location(state: &PlayerState, location: Option<(usize, usize)>) -> String {
    if let Some((pending_location, change)) = &state.pending_cover {
        if Some(*pending_location) == location {
            return match change {
                CoverChange::Replace(path) => format!(
                    "Carátula seleccionada: {}",
                    path.file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_default()
                ),
                CoverChange::Remove => "La carátula se quitará al guardar".to_owned(),
            };
        }
    }

    let has_cover = location
        .and_then(|(playlist, track)| state.playlists.get(playlist)?.tracks.get(track))
        .is_some_and(|path| read_cover_art(path).is_some());
    if has_cover {
        "Carátula incrustada en el archivo".to_owned()
    } else {
        "Este archivo no tiene carátula".to_owned()
    }
}

fn displayed_location_for(
    stopped: bool,
    current: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    if stopped {
        selected
    } else {
        current.or(selected)
    }
}

/// Play arranca la pista seleccionada cuando es distinta de la cargada; si
/// coinciden (o no hay selección) se reanuda/relanza la que ya está cargada.
fn play_starts_selected(
    current: Option<(usize, usize)>,
    selected: Option<(usize, usize)>,
) -> bool {
    selected.is_some_and(|selected| current != Some(selected))
}

fn update_cover_art(state: &mut PlayerState, ui: Ui) {
    let marker = if state.youtube_mode {
        state.displayed_youtube_index().and_then(|index| {
            state.youtube_results.get(index).map(|track| CoverMarker::Youtube {
                generation: state.youtube_generation,
                id: track.id.clone(),
                index,
            })
        })
    } else {
        state.displayed_location().and_then(|(playlist, track)| {
            state
                .playlists
                .get(playlist)
                .map(|entry| CoverMarker::Local(playlist, track, entry.revision))
        })
    };
    if marker == state.cover_rendered_for {
        return;
    }

    let image_data = match &marker {
        Some(CoverMarker::Youtube { index, .. }) => state
            .youtube_results
            .get(*index)
            .and_then(|track| track.thumbnail_data.clone()),
        Some(CoverMarker::Local(playlist, track, _)) => state
            .playlists
            .get(*playlist)
            .and_then(|entry| entry.tracks.get(*track))
            .and_then(|path| read_cover_art(path)),
        None => None,
    };
    if state.youtube_mode {
        state.cover_hint = match (&marker, &image_data) {
            (Some(CoverMarker::Youtube { .. }), Some(_)) => {
                "Miniatura del resultado de YouTube".to_owned()
            }
            (Some(CoverMarker::Youtube { .. }), None) => {
                "Este resultado no tiene miniatura".to_owned()
            }
            _ => "Selecciona un resultado para ver su carátula".to_owned(),
        };
    }
    let new_texture = image_data
        .filter(|data| !data.is_empty())
        .map(|data| unsafe { Rtexture_create(data.as_ptr().cast(), data.len()) })
        .filter(|texture| *texture != R_INVALID_ID)
        .unwrap_or(R_INVALID_ID);
    let old_texture = state.cover_texture;

    unsafe {
        if new_texture == R_INVALID_ID {
            Rimage_set_texture(ui.cover_image, R_INVALID_ID);
            Rswitcher_set_index(ui.cover_switcher, 0);
        } else {
            Rimage_set_texture(ui.cover_image, new_texture);
            Rswitcher_set_index(ui.cover_switcher, 1);
        }
        if old_texture != R_INVALID_ID {
            Rtexture_destroy(old_texture);
        }
    }

    state.cover_texture = new_texture;
    state.cover_rendered_for = marker;
}

fn write_track_metadata(
    path: &Path,
    metadata: &TrackMetadata,
    cover_change: Option<CoverChange>,
) -> Result<(), MetadataWriteError> {
    let validation = |message: String| MetadataWriteError::Validation(message);
    let track_number = if metadata.track.trim().is_empty() {
        None
    } else {
        let number = metadata
            .track
            .trim()
            .parse::<u32>()
            .map_err(|_| validation("El número de pista debe ser un entero positivo".to_owned()))?;
        if number == 0 {
            return Err(validation(
                "El número de pista debe ser mayor que cero".to_owned(),
            ));
        }
        Some(number)
    };
    let mut file = lofty::read_from_path(path).map_err(|error| {
        MetadataWriteError::Read(format!("No se pudieron leer los metadatos: {error}"))
    })?;
    let tag_type = file.primary_tag_type();
    if !file.tag_support(tag_type).is_writable() {
        return Err(validation(
            "Este formato no admite escribir metadatos".to_owned(),
        ));
    }
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file.primary_tag_mut().ok_or_else(|| {
        validation("No se pudo crear la etiqueta del archivo".to_owned())
    })?;

    if metadata.title.trim().is_empty() {
        tag.remove_title();
    } else {
        tag.set_title(metadata.title.trim().to_owned());
    }
    if metadata.artist.trim().is_empty() {
        tag.remove_artist();
    } else {
        tag.set_artist(metadata.artist.trim().to_owned());
    }
    if metadata.album.trim().is_empty() {
        tag.remove_album();
    } else {
        tag.set_album(metadata.album.trim().to_owned());
    }
    if metadata.genre.trim().is_empty() {
        tag.remove_genre();
    } else {
        tag.set_genre(metadata.genre.trim().to_owned());
    }
    match track_number {
        Some(number) => tag.set_track(number),
        None => tag.remove_track(),
    }
    match cover_change {
        Some(CoverChange::Replace(image_path)) => {
            let image_data = fs::read(&image_path).map_err(|error| {
                validation(format!("No se pudo abrir la carátula: {error}"))
            })?;
            
            let picture = if image_path.extension().and_then(|e| e.to_str()) == Some("webp") {
                // WebP no es soportado por lofy, convertir a PNG
                let img = image::load_from_memory(&image_data)
                    .map_err(|error| validation(format!("La imagen no es compatible: {error}")))?;
                let mut png_buffer = Vec::new();
                img.write_to(&mut std::io::Cursor::new(&mut png_buffer), image::ImageFormat::Png)
                    .map_err(|error| validation(format!("No se pudo convertir la imagen: {error}")))?;
                Picture::unchecked(png_buffer).mime_type(MimeType::Png).build()
            } else {
                let mut image_file = std::io::Cursor::new(&image_data);
                Picture::from_reader(&mut image_file)
                    .map_err(|error| validation(format!("La imagen no es compatible: {error}")))?
            };
            
            let mut picture = picture;
            picture.set_pic_type(PictureType::CoverFront);
            if let Some(index) = tag
                .pictures()
                .iter()
                .position(|p| p.pic_type() == PictureType::CoverFront)
            {
                tag.set_picture(index, picture);
            } else {
                tag.push_picture(picture);
            }
        }
        Some(CoverChange::Remove) => {
            while !tag.pictures().is_empty() {
                tag.remove_picture(0);
            }
        }
        None => {}
    }
    file.save_to_path(path, WriteOptions::default())
        .map_err(|error| {
            MetadataWriteError::Write(format!(
                "No se pudieron guardar los metadatos: {error}"
            ))
        })
}

fn create_audio_output(volume: f32) -> Result<AudioOutput, String> {
    #[cfg(target_os = "linux")]
    let stream = {
        use cpal::traits::HostTrait;

        let pulse_device = cpal::host_from_id(cpal::HostId::PulseAudio)
            .ok()
            .and_then(|host| host.default_output_device());
        let device = pulse_device
            .or_else(|| {
                cpal::host_from_id(cpal::HostId::Alsa)
                    .ok()
                    .and_then(|host| host.default_output_device())
            })
            .ok_or_else(|| "No se encontró un dispositivo de salida de audio".to_owned())?;

        DeviceSinkBuilder::from_device(device)
            .and_then(|builder| builder.open_sink_or_fallback())
            .map_err(|error| format!("No se pudo abrir la salida de audio: {error}"))?
    };

    #[cfg(not(target_os = "linux"))]
    let stream = DeviceSinkBuilder::open_default_sink()
        .map_err(|error| format!("No se pudo abrir la salida de audio: {error}"))?;

    let player = Player::connect_new(stream.mixer());
    player.set_volume(volume);
    Ok(AudioOutput {
        player,
        _stream: stream,
    })
}

#[derive(Clone, Copy)]
struct Ui {
    app_logo: Rid,
    playlist_tabs: Rid,
    playlist_name: Rid,
    playlist_summary: Rid,
    library_title: Rid,
    youtube_search: Rid,
    track_list: Rid,
    youtube_button: Rid,
    youtube_icon: Rid,
    right_panel: Rid,
    cover_switcher: Rid,
    cover_image: Rid,
    cover_status: Rid,
    now_title: Rid,
    now_artist: Rid,
    status: Rid,
    elapsed: Rid,
    total: Rid,
    seek: Rid,
    volume: Rid,
    shuffle: Rid,
    shuffle_icon: Rid,
    repeat: Rid,
    repeat_icon: Rid,
    random_icon: Rid,
    import_icon: Rid,
    export_icon: Rid,
    repair_actions: Rid,
    restore_icon: Rid,
    confirm_icon: Rid,
    volume_value: Rid,
    meta_title: Rid,
    meta_artist: Rid,
    meta_album: Rid,
    meta_genre: Rid,
    meta_track: Rid,
    metadata_hint: Rid,
}

impl Ui {
    fn from_window(window: Rid) -> Self {
        Self {
            app_logo: widget_find(window, "app-logo"),
            playlist_tabs: widget_find(window, "playlist-tabs"),
            playlist_name: widget_find(window, "playlist-name"),
            playlist_summary: widget_find(window, "playlist-summary"),
            library_title: widget_find(window, "library-title"),
            youtube_search: widget_find(window, "youtube-search"),
            track_list: widget_find(window, "track-list"),
            youtube_button: widget_find(window, "youtube-button"),
            youtube_icon: widget_find(window, "youtube-icon"),
            right_panel: widget_find(window, "right-panel"),
            cover_switcher: widget_find(window, "cover-switcher"),
            cover_image: widget_find(window, "cover-image"),
            cover_status: widget_find(window, "cover-status"),
            now_title: widget_find(window, "now-playing-title"),
            now_artist: widget_find(window, "now-playing-artist"),
            status: widget_find(window, "player-status"),
            elapsed: widget_find(window, "elapsed"),
            total: widget_find(window, "total"),
            seek: widget_find(window, "seek"),
            volume: widget_find(window, "volume"),
            shuffle: widget_find(window, "shuffle"),
            shuffle_icon: widget_find(window, "shuffle-icon"),
            repeat: widget_find(window, "repeat"),
            repeat_icon: widget_find(window, "repeat-icon"),
            random_icon: widget_find(window, "random-icon"),
            import_icon: widget_find(window, "import-icon"),
            export_icon: widget_find(window, "export-icon"),
            repair_actions: widget_find(window, "repair-actions"),
            restore_icon: widget_find(window, "restore-icon"),
            confirm_icon: widget_find(window, "confirm-icon"),
            volume_value: widget_find(window, "volume-value"),
            meta_title: widget_find(window, "meta-title"),
            meta_artist: widget_find(window, "meta-artist"),
            meta_album: widget_find(window, "meta-album"),
            meta_genre: widget_find(window, "meta-genre"),
            meta_track: widget_find(window, "meta-track"),
            metadata_hint: widget_find(window, "metadata-hint"),
        }
    }
}

fn load_symbol_fallbacks() -> (Rid, Rid) {
    unsafe {
        let symbols_2 = Rfont_create(SYMBOLS_2_FONT.as_ptr().cast(), SYMBOLS_2_FONT.len());
        let symbols = Rfont_create(SYMBOLS_FONT.as_ptr().cast(), SYMBOLS_FONT.len());

        if symbols_2 != R_INVALID_ID {
            Rfont_set_fallback(Rfont_get_default(), symbols_2);
            if symbols != R_INVALID_ID {
                Rfont_set_fallback(symbols_2, symbols);
            }
        } else {
            eprintln!("No se pudo cargar la fuente de símbolos Noto Sans Symbols 2");
        }
        if symbols == R_INVALID_ID {
            eprintln!("No se pudo cargar la fuente de símbolos Noto Sans Symbols");
        }

        (symbols_2, symbols)
    }
}

fn destroy_symbol_fallbacks(symbols_2: Rid, symbols: Rid) {
    unsafe {
        if symbols_2 != R_INVALID_ID {
            Rfont_set_fallback(Rfont_get_default(), R_INVALID_ID);
            Rfont_set_fallback(symbols_2, R_INVALID_ID);
            Rfont_destroy(symbols_2);
        }
        if symbols != R_INVALID_ID {
            Rfont_destroy(symbols);
        }
    }
}

fn load_action_icons(ui: Ui) -> Rid {
    let texture = unsafe {
        Rtexture_create(ACTION_ICONS_PNG.as_ptr().cast(), ACTION_ICONS_PNG.len())
    };
    if texture == R_INVALID_ID {
        eprintln!("No se pudieron cargar los iconos de acciones");
        return R_INVALID_ID;
    }

    unsafe {
        for (image, min_u, max_u) in [
            (ui.shuffle_icon, 0.0, 1.0 / 8.0),
            (ui.repeat_icon, 1.0 / 8.0, 2.0 / 8.0),
            (ui.import_icon, 2.0 / 8.0, 3.0 / 8.0),
            (ui.export_icon, 3.0 / 8.0, 4.0 / 8.0),
            (ui.restore_icon, 4.0 / 8.0, 5.0 / 8.0),
            (ui.confirm_icon, 5.0 / 8.0, 6.0 / 8.0),
            (ui.random_icon, 6.0 / 8.0, 7.0 / 8.0),
            (ui.youtube_icon, 7.0 / 8.0, 1.0),
        ] {
            if image != R_INVALID_ID {
                Rimage_set_texture(image, texture);
                Rimage_set_region(image, min_u, 0.0, max_u, 1.0);
                Rimage_set_stretch(image, R_STRETCH_FIT);
                Rimage_set_tint_text(image, R_TRUE);
            }
        }
    }
    texture
}

fn load_app_icon(window: Rid, ui: Ui) -> Rid {
    let mut width = 0;
    let mut height = 0;
    let pixels = unsafe {
        Rimage_decode(
            APP_ICON_PNG.as_ptr().cast(),
            APP_ICON_PNG.len(),
            &mut width,
            &mut height,
        )
    };
    if pixels.is_null() {
        return R_INVALID_ID;
    }
    if width <= 0 || height <= 0 {
        unsafe { Rpixels_free(pixels) };
        return R_INVALID_ID;
    }

    unsafe {
        Rwindow_set_icon(window, pixels, width, height);
        let texture = Rtexture_create_from_pixels(pixels, width, height);
        Rpixels_free(pixels);
        if texture != R_INVALID_ID && ui.app_logo != R_INVALID_ID {
            Rimage_set_texture(ui.app_logo, texture);
            Rimage_set_stretch(ui.app_logo, R_STRETCH_FIT);
        }
        texture
    }
}

fn set_label(widget: Rid, value: &str) {
    unsafe { Rlabel_set_text(widget, cstr(value).as_ptr()) };
}

fn set_input(widget: Rid, value: &str) {
    unsafe { Rtext_input_set_text(widget, cstr(value).as_ptr()) };
}

fn input_value(widget: Rid) -> String {
    unsafe { text(Rtext_input_get_text(widget)) }
}

fn format_time(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3_600,
            (seconds / 60) % 60,
            seconds % 60
        )
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}

#[cfg(test)]
fn next_track_index(current: Option<usize>, playlist_len: usize) -> Option<usize> {
    if playlist_len == 0 {
        return None;
    }
    Some(current.map_or(0, |index| (index + 1) % playlist_len))
}

/// Primera posición de una cola aleatoria. Si la lista tiene más de una
/// pista, se excluye la actual para que «al azar» nunca repita la pista que
/// ya suena.
fn random_track_index(playlist_len: usize, current_track: Option<usize>) -> Option<usize> {
    if playlist_len == 0 {
        return None;
    }
    let excluded = current_track.filter(|_| playlist_len > 1);
    shuffled_track_indices(playlist_len, excluded)
        .into_iter()
        .next()
}

/// Orden de un ciclo nuevo de aleatorio. La pista que acaba de sonar queda la
/// última para no repetirla al reiniciar, y el resto se baraja de cero, de modo
/// que el historial del ciclo anterior desaparece. Con listas cortas puede
/// salir el mismo orden por azar; se vuelve a barajar para que el cambio sea
/// visible siempre que sea posible.
fn reshuffled_cycle_order(
    playlist_len: usize,
    current_track: Option<usize>,
    previous_order: &[usize],
) -> Vec<usize> {
    let build = || {
        let mut order = shuffled_track_indices(playlist_len, current_track);
        if let Some(track) = current_track.filter(|track| *track < playlist_len) {
            order.push(track);
        }
        order
    };
    let mut order = build();
    let mut retries = if playlist_len > 2 { 8 } else { 0 };
    while order == previous_order && retries > 0 {
        order = build();
        retries -= 1;
    }
    order
}

fn shuffled_track_indices(playlist_len: usize, excluded_track: Option<usize>) -> Vec<usize> {
    let random_state = RandomState::new();
    let mut tracks: Vec<_> = (0..playlist_len)
        .filter(|track| Some(*track) != excluded_track)
        .collect();
    tracks.sort_by_key(|track| {
        let mut hasher = random_state.build_hasher();
        track.hash(&mut hasher);
        hasher.finish()
    });
    tracks
}

#[cfg(test)]
fn previous_track_index(current: Option<usize>, playlist_len: usize) -> Option<usize> {
    if playlist_len == 0 {
        return None;
    }
    Some(current.map_or(0, |index| {
        if index == 0 {
            playlist_len - 1
        } else {
            index - 1
        }
    }))
}

fn youtube_result_row(track: &youtube::YoutubeTrack, index: usize) -> String {
    let duration = if track.live {
        "EN DIRECTO".to_owned()
    } else {
        track
            .duration
            .map(format_time)
            .unwrap_or_else(|| "--:--".to_owned())
    };
    let channel = if track.channel.is_empty() {
        "Canal desconocido"
    } else {
        track.channel.as_str()
    };
    format!(
        "{:02} {}  ·  {}     {}",
        index + 1,
        track.title,
        channel,
        duration
    )
}

fn track_row(
    metadata: &TrackMetadata,
    index: usize,
    pending_repair: bool,
    streaming: bool,
) -> String {
    let duration = metadata
        .duration
        .map(format_time)
        .unwrap_or_else(|| "--:--".to_owned());
    let artist = if metadata.artist.is_empty() {
        "Artista desconocido".to_owned()
    } else {
        metadata.artist.clone()
    };
    // El triángulo va justo antes del título, como el distintivo «▶» del
    // buscador. `(YouTube)` lo hace explícito aunque se confunda con el icono
    // de reproducción; una pista en streaming no es un archivo local.
    let marker = if pending_repair {
        "⚠"
    } else if streaming {
        "▶"
    } else {
        " "
    };
    let artist = if streaming {
        format!("{artist} (YouTube)")
    } else {
        artist
    };
    format!(
        "{:02} {marker} {}  ·  {}     {}",
        index + 1,
        metadata.title,
        artist,
        duration
    )
}

unsafe extern "C" fn track_list_row_text(
    _list_view: Rid,
    index: i32,
    user_data: *mut c_void,
) -> *const c_char {
    const EMPTY: &[u8] = b"\0";

    if user_data.is_null() || index < 0 {
        return EMPTY.as_ptr().cast();
    }
    let rows = &*(user_data as *const RefCell<TrackListRows>);
    let Ok(mut rows) = rows.try_borrow_mut() else {
        return EMPTY.as_ptr().cast();
    };
    if rows.youtube {
        if let Some(placeholder) = rows.placeholder.as_ref() {
            return placeholder.as_ptr();
        }
        let index = index as usize;
        let Some(track) = rows.youtube_results.get(index) else {
            return EMPTY.as_ptr().cast();
        };
        if rows.text[index].is_none() {
            rows.text[index] = Some(cstr(&youtube_result_row(track, index)));
        }
        return rows.text[index]
            .as_ref()
            .map(|text| text.as_ptr())
            .unwrap_or(EMPTY.as_ptr().cast());
    }
    if let Some(placeholder) = rows.placeholder.as_ref() {
        return placeholder.as_ptr();
    }
    let index = index as usize;
    if index >= rows.tracks.len() {
        return EMPTY.as_ptr().cast();
    }

    if rows.text[index].is_none() {
        // Las pistas en streaming guardan una URL, no un archivo: usar su
        // metadato cacheado evita intentar leer la URL como si fuera local.
        let streaming = rows.streams.get(index).and_then(Option::as_ref);
        let (metadata, pending_repair) = match streaming {
            Some(stream) => (stream.metadata(), false),
            None => (
                read_track_metadata(&rows.tracks[index]),
                rows.pending_repairs.contains(&rows.tracks[index]),
            ),
        };
        rows.text[index] = Some(cstr(&track_row(
            &metadata,
            index,
            pending_repair,
            streaming.is_some(),
        )));
    }
    rows.text[index]
        .as_ref()
        .map(|text| text.as_ptr())
        .unwrap_or(EMPTY.as_ptr().cast())
}

/// Posición de la pista que suena dentro de la cola mezclada, en base 1. Solo
/// se muestra si el aleatorio está activo y esa lista es la que se está
/// reproduciendo; si no, el número no tendría sentido para el usuario.
fn shuffle_progress(
    playlist: &Playlist,
    current: Option<(usize, usize)>,
    playlist_index: usize,
) -> Option<String> {
    if !playlist.shuffle || current?.0 != playlist_index {
        return None;
    }
    let position = playlist.order_position?;
    if playlist.order.is_empty() || position >= playlist.order.len() {
        return None;
    }
    Some(format!("{}/{}", position + 1, playlist.order.len()))
}

/// Texto bajo «Biblioteca». Se calcula en cada refresco, no dentro de
/// `update_track_list`, porque esa función sale pronto mientras la lista no
/// cambie y la posición de la cola sí cambia al avanzar de pista.
fn playlist_summary_text(
    playlist: &Playlist,
    current: Option<(usize, usize)>,
    playlist_index: usize,
) -> String {
    let track_count = playlist.tracks.len();
    let base = if playlist.is_temporal {
        format!("{track_count} pistas · no se guarda; renómbrala para conservarla")
    } else {
        format!("{track_count} pistas · {}", playlist.name)
    };
    match shuffle_progress(playlist, current, playlist_index) {
        Some(progress) => format!("{progress} · {base}"),
        None => base,
    }
}

fn update_track_list(
    state: &mut PlayerState,
    ui: Ui,
    row_cache: &RefCell<TrackListRows>,
) {
    let pending_repairs: HashSet<PathBuf> = state
        .pending_repairs
        .iter()
        .map(|pending| pending.path.clone())
        .collect();

    if state.youtube_mode {
        let marker = (usize::MAX, 0);
        if state.ui_rendered_playlist == Some(marker) {
            return;
        }
        let count = state.youtube_results.len();
        let placeholder = (count == 0).then(|| cstr(&state.youtube_hint));
        {
            let mut rows = row_cache.borrow_mut();
            rows.tracks.clear();
            rows.streams.clear();
            rows.text.clear();
            rows.text.resize_with(count, || None);
            rows.pending_repairs.clear();
            rows.youtube = true;
            rows.youtube_results = state.youtube_results.clone();
            rows.placeholder = placeholder;
        }
        let user_data = row_cache as *const RefCell<TrackListRows> as *mut c_void;
        unsafe {
            Rlist_view_set_virtual_items(
                ui.track_list,
                count.max(1) as i32,
                Some(track_list_row_text),
                user_data,
            );
            Rlist_view_set_selected(ui.track_list, -1);
        }
        state.ui_rendered_playlist = Some(marker);
        state.editor_loaded_for = None;
        state.ui_selection_dirty = false;
        set_input(ui.playlist_name, "");
        return;
    }

    let Some(playlist) = state.playlists.get(state.active_playlist) else {
        return;
    };
    let marker = (state.active_playlist, playlist.revision);
    if state.ui_rendered_playlist == Some(marker) {
        return;
    }

    let name = playlist.name.clone();
    let tracks = playlist.tracks.clone();
    let streams = playlist.streams.clone();
    let selected = playlist.selected;
    let track_count = tracks.len();
    {
        let mut rows = row_cache.borrow_mut();
        rows.tracks = tracks;
        rows.streams = streams;
        rows.text.clear();
        rows.text.resize_with(track_count, || None);
        rows.pending_repairs = pending_repairs;
        rows.placeholder = None;
        rows.youtube = false;
        rows.youtube_results.clear();
    }

    let user_data = row_cache as *const RefCell<TrackListRows> as *mut c_void;
    unsafe {
        Rlist_view_set_virtual_items(
            ui.track_list,
            i32::try_from(track_count).unwrap_or(i32::MAX),
            Some(track_list_row_text),
            user_data,
        );
        if let Some(index) = selected {
            Rlist_view_set_selected(ui.track_list, index as i32);
        }
    }
    state.ui_rendered_playlist = Some(marker);
    state.editor_loaded_for = None;
    state.ui_selection_dirty = false;
    set_input(ui.playlist_name, &name);
}

fn update_metadata_editor(state: &mut PlayerState, ui: Ui) {
    // En el buscador la fila seleccionada es un resultado, no una pista de la
    // lista: sincronizarla cambiaría `playlist.selected` y podría abrir el
    // editor de una pista local que no tiene nada que ver.
    if state.youtube_mode {
        state.ui_selection_dirty = false;
        state.metadata_hint = "Los resultados de YouTube no se editan".to_owned();
        return;
    }
    // Durante un arrastre la selección de Raven es la guía visual del destino.
    // Sincronizarla de vuelta al modelo aquí provocaría un bucle que movía la
    // selección sin sentido; al soltar, `move_track` ya fija el estado final.
    if !state.drag.is_dragging() {
        if state.ui_selection_dirty {
            let selected = state
                .playlists
                .get(state.active_playlist)
                .and_then(|playlist| playlist.selected)
                .map(|index| index as i32)
                .unwrap_or(-1);
            unsafe { Rlist_view_set_selected(ui.track_list, selected) };
            state.ui_selection_dirty = false;
        }
        let selected = unsafe { Rlist_view_get_selected(ui.track_list) };
        if selected >= 0 {
            if let Some(playlist) = state.playlists.get_mut(state.active_playlist) {
                let selected = selected as usize;
                if selected < playlist.tracks.len() && playlist.selected != Some(selected) {
                    playlist.selected = Some(selected);
                    state.editor_loaded_for = None;
                    state.save_playlists();
                }
            }
        }
    }

    if unsafe { Rswitcher_get_index(ui.right_panel) } != 1 {
        return;
    }

    let location = state.selected_location();
    if location == state.editor_loaded_for {
        return;
    }
    if state
        .pending_cover
        .as_ref()
        .is_some_and(|(pending_location, _)| Some(*pending_location) != location)
    {
        state.pending_cover = None;
    }
    state.editor_loaded_for = location;
    let stream = location
        .and_then(|(playlist, track)| state.playlists.get(playlist)?.stream_at(track))
        .cloned();
    let path = location
        .and_then(|(playlist, track)| state.playlists.get(playlist)?.tracks.get(track))
        .cloned();
    let metadata = stream
        .as_ref()
        .map(StreamingTrack::metadata)
        .or_else(|| path.as_deref().map(read_track_metadata));
    if let Some(metadata) = metadata {
        set_input(ui.meta_title, &metadata.title);
        set_input(ui.meta_artist, &metadata.artist);
        set_input(ui.meta_album, &metadata.album);
        set_input(ui.meta_genre, &metadata.genre);
        set_input(ui.meta_track, &metadata.track);
        state.metadata_hint = if stream.is_some() {
            "Las pistas en streaming no se editan".to_owned()
        } else if path
            .as_deref()
            .is_some_and(|path| state.is_pending_repair(path))
        {
            PENDING_REPAIR_HINT.to_owned()
        } else {
            "Los cambios se escriben en el archivo de audio".to_owned()
        };
        state.cover_hint = if stream.is_some() {
            "Las pistas en streaming no tienen carátula editable".to_owned()
        } else {
            cover_hint_for_location(state, location)
        };
    } else {
        set_input(ui.meta_title, "");
        set_input(ui.meta_artist, "");
        set_input(ui.meta_album, "");
        set_input(ui.meta_genre, "");
        set_input(ui.meta_track, "");
        state.metadata_hint = "Selecciona una pista para editar sus etiquetas".to_owned();
        state.cover_hint = "Selecciona una pista para editar su carátula".to_owned();
    }
    set_label(ui.metadata_hint, &state.metadata_hint);
    set_label(ui.cover_status, &state.cover_hint);
}

fn refresh_ui(
    state: &mut PlayerState,
    ui: Ui,
    row_cache: &RefCell<TrackListRows>,
    menus: &mut ContextMenus,
    window: Rid,
) {
    menus.sync(ui, state.youtube_mode, state.selected_is_flac());
    state.poll_update_check(window);
    state.poll_instance_messages(window);
    state.update_after_frame();
    state.poll_mpris_commands();
    state.poll_youtube_search();
    state.poll_youtube_audio();
    state.poll_youtube_downloads();
    state.poll_stream_audio();
    state.poll_conversion();
    update_track_list(state, ui, row_cache);
    let selected = unsafe { Rlist_view_get_selected(ui.track_list) };
    state.sync_youtube_selection(selected);
    set_label(
        ui.library_title,
        if state.youtube_mode {
            "YouTube"
        } else {
            "Biblioteca"
        },
    );
    if state.youtube_mode {
        let count = state.youtube_results.len();
        let summary = if count == 0 {
            "Buscador · los resultados no se guardan".to_owned()
        } else {
            format!(
                "{count} resultado{} · los resultados no se guardan",
                if count == 1 { "" } else { "s" }
            )
        };
        set_label(ui.playlist_summary, &summary);
    } else if let Some(playlist) = state.playlists.get(state.active_playlist) {
        let summary = playlist_summary_text(playlist, state.current, state.active_playlist);
        set_label(ui.playlist_summary, &summary);
    }
    let youtube_tooltip = if state.youtube_mode {
        "Volver a la lista de música"
    } else {
        "Buscar música en YouTube (solo búsqueda, no guarda nada)"
    };
    unsafe { Rwidget_set_tooltip(ui.youtube_button, cstr(youtube_tooltip).as_ptr()) };
    update_metadata_editor(state, ui);
    update_cover_art(state, ui);

    let pending_repair = state
        .editor_loaded_for
        .and_then(|location| state.path_at(location))
        .is_some_and(|path| state.is_pending_repair(path));
    unsafe {
        Rwidget_set_visibility(
            ui.repair_actions,
            if pending_repair {
                R_VISIBLE
            } else {
                R_COLLAPSED
            },
        );
    }

    if state.is_stopped() {
        set_label(ui.now_title, "Ninguna pista");
        set_label(ui.now_artist, "Abre una lista para empezar");
    } else if let Some(metadata) = &state.current_metadata {
        set_label(ui.now_title, &metadata.title);
        set_label(
            ui.now_artist,
            if metadata.artist.is_empty() {
                "Artista desconocido"
            } else {
                metadata.artist.as_str()
            },
        );
    } else {
        set_label(ui.now_title, "Ninguna pista");
        set_label(ui.now_artist, "Abre una lista para empezar");
    }
    set_label(ui.status, &state.status);
    set_label(ui.elapsed, &format_time(state.position()));
    let total = if state.is_stopped() {
        "--:--".to_owned()
    } else {
        state
            .duration
            .map(format_time)
            .unwrap_or_else(|| "--:--".to_owned())
    };
    set_label(ui.total, &total);
    set_label(
        ui.volume_value,
        &format!("{}%", (state.volume * 100.0).round() as u32),
    );
    set_label(ui.metadata_hint, &state.metadata_hint);
    set_label(ui.cover_status, &state.cover_hint);

    let (shuffle, repeat) = state
        .playlists
        .get(state.active_playlist)
        .map(|playlist| (playlist.shuffle, playlist.repeat))
        .unwrap_or((false, false));
    unsafe {
        Rbutton_set_style(
            ui.shuffle,
            if shuffle {
                R_BUTTON_PRIMARY
            } else {
                R_BUTTON_FLAT
            },
        );
        Rbutton_set_style(
            ui.repeat,
            if repeat {
                R_BUTTON_PRIMARY
            } else {
                R_BUTTON_FLAT
            },
        );
        Rwidget_set_tooltip(
            ui.shuffle,
            cstr(if shuffle {
                "Aleatorio activado en esta lista"
            } else {
                "Activar reproducción aleatoria para esta lista"
            })
            .as_ptr(),
        );
        Rwidget_set_tooltip(
            ui.repeat,
            cstr(if repeat {
                "Repetición de lista activada"
            } else {
                "Repetir esta lista al terminar"
            })
            .as_ptr(),
        );
    }

    let duration_seconds = if state.is_stopped() {
        0.0
    } else {
        state.duration.unwrap_or_default().as_secs_f32()
    };
    let position = state.position().as_secs_f32();
    unsafe {
        Rslider_set_range(ui.seek, 0.0, duration_seconds.max(1.0));
        Rslider_set_value(ui.seek, position.min(duration_seconds));
    }
    state.publish_mpris_snapshot();
}

fn add_playlist_tab(tabs: Rid, title: &str, make_active: bool, is_temporal: bool) {
    let page = unsafe { Rwidget_create(R_WIDGET_VBOX) };
    let title = cstr(title);
    let index = unsafe { Rtab_view_add_tab(tabs, title.as_ptr(), page) };
    if index < 0 {
        return;
    }
    let page = unsafe { Rtab_view_get_tab(tabs, index) };
    unsafe {
        Rtab_view_set_tab_closable(tabs, page, R_TRUE);
        if is_temporal {
            let hint = cstr(
                "Lista de sesión: se descarta al cerrar. Renómbrala para conservarla.",
            );
            Rwidget_set_tooltip(page, hint.as_ptr());
        }
        if make_active {
            Rtab_view_set_active(tabs, index);
        }
    }
}

unsafe extern "C" fn close_playlist_tab(
    _tabs: Rid,
    index: i32,
    _content: Rid,
    user_data: *mut c_void,
) -> i32 {
    if index < 0 {
        return R_FALSE;
    }
    let state = &*(user_data as *const RefCell<PlayerState>);
    let Ok(mut state) = state.try_borrow_mut() else {
        return R_FALSE;
    };
    if state.close_playlist(index as usize) {
        R_TRUE
    } else {
        R_FALSE
    }
}

fn is_audio_file(path: &Path) -> bool {
    const EXTENSIONS: [&str; 6] = ["mp3", "flac", "wav", "ogg", "opus", "m4a"];
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Archivos de audio recibidos por línea de comandos.
///
/// Acepta rutas normales y URIs `file://` (con `%20` y demás escapes), ignora
/// los marcadores que añade Flatpak al exportar el lanzador
/// (`--file-forwarding`, `@@`, `@@u`) y descarta lo que no sea audio. La
/// existencia en disco se comprueba al arrancar: aquí interesa que la función
/// sea pura y fácil de probar.
fn audio_paths_from_args(args: impl IntoIterator<Item = String>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut paths = Vec::new();
    for arg in args {
        if arg.is_empty() || matches!(arg.as_str(), "--file-forwarding" | "@@" | "@@u") {
            continue;
        }
        let path = if arg.starts_with("file:") {
            match Url::parse(&arg).ok().and_then(|url| url.to_file_path().ok()) {
                Some(path) => path,
                None => continue,
            }
        } else if has_uri_scheme(&arg) {
            continue;
        } else {
            PathBuf::from(&arg)
        };
        if is_audio_file(&path) && seen.insert(path.clone()) {
            paths.push(path);
        }
    }
    paths
}

/// Distingue `https://…` de una ruta con dos puntos en el nombre. Solo interesa
/// descartar esquemas ajenos a `file:`, que ya se procesa antes.
fn has_uri_scheme(arg: &str) -> bool {
    let Some((scheme, _)) = arg.split_once(':') else {
        return false;
    };
    scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
}

fn open_audio_files(_window: Rid) -> Vec<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Añadir pistas a la lista")
        .add_filter(
            "Archivos de audio",
            &["mp3", "flac", "wav", "ogg", "opus", "m4a"],
        )
        .pick_files()
        .unwrap_or_default()
}

fn open_cover_file(_window: Rid, start_dir: Option<&Path>) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new()
        .set_title("Seleccionar carátula")
        .add_filter("Imágenes de carátula", &["png", "jpg", "jpeg", "bmp", "gif", "webp"]);
    if let Some(dir) = start_dir {
        dialog = dialog.set_directory(dir);
    }
    dialog.pick_file()
}

/// La carpeta existente más cercana subiendo por los ancestros. Se excluye la
/// raíz porque abrir el diálogo en `/` no ayuda a encontrar las carátulas.
fn existing_directory(path: &Path) -> Option<PathBuf> {
    let mut candidate = Some(path);
    while let Some(directory) = candidate {
        if directory != Path::new("/") && directory.is_dir() {
            return Some(directory.to_path_buf());
        }
        candidate = directory.parent();
    }
    None
}

/// Rutas que el portal de documentos monta en `$XDG_RUNTIME_DIR/doc` mientras
/// dura la sesión. No sirven como última carpeta: desaparecen al cerrar.
fn is_document_portal_path(path: &Path) -> bool {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    is_document_portal_path_in(path, runtime_dir.as_deref())
}

fn is_document_portal_path_in(path: &Path, runtime_dir: Option<&Path>) -> bool {
    if let Some(runtime_dir) = runtime_dir {
        if path.starts_with(runtime_dir.join("doc")) {
            return true;
        }
    }
    // Sin XDG_RUNTIME_DIR, reconoce el patrón /run/user/<uid>/doc/<id>.
    let components: Vec<_> = path.components().collect();
    components.len() >= 5
        && components[0].as_os_str() == "/"
        && components[1].as_os_str() == "run"
        && components[2].as_os_str() == "user"
        && components[3]
            .as_os_str()
            .to_string_lossy()
            .chars()
            .all(|character| character.is_ascii_digit())
        && components[4].as_os_str() == "doc"
}

/// La ruta real del archivo cuando el portal de documentos lo expone como
/// `/run/user/<uid>/doc/<id>/<nombre>` (lo habitual dentro de Flatpak). El
/// portal guarda la ruta original en un atributo extendido del archivo.
fn document_portal_host_path(path: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};

    const ATTRIBUTE: &[u8] = b"user.document-portal.host-path\0";
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let size = unsafe {
        libc::getxattr(
            path.as_ptr(),
            ATTRIBUTE.as_ptr().cast(),
            std::ptr::null_mut(),
            0,
        )
    };
    if size <= 0 {
        return None;
    }
    let mut value = vec![0u8; size as usize];
    let read = unsafe {
        libc::getxattr(
            path.as_ptr(),
            ATTRIBUTE.as_ptr().cast(),
            value.as_mut_ptr().cast(),
            value.len(),
        )
    };
    if read <= 0 {
        return None;
    }
    value.truncate(read as usize);
    let value = std::ffi::OsString::from_vec(value);
    Some(PathBuf::from(value))
}

/// Carpeta inicial del diálogo de carátulas: la última usada si todavía
/// existe, y si no la de la pista que se está editando.
fn cover_dialog_directory(last: Option<&Path>, track: Option<&Path>) -> Option<PathBuf> {
    let last = last.filter(|path| !is_document_portal_path(path));
    last.and_then(existing_directory)
        .or_else(|| track.and_then(Path::parent).and_then(existing_directory))
}

fn open_playlist_export_file(_window: Rid) -> Option<PathBuf> {
    let path = rfd::FileDialog::new()
        .set_title("Exportar lista")
        .add_filter("Lista M3U8 (UTF-8)", &["m3u8"])
        .add_filter("Lista M3U", &["m3u"])
        .add_filter("Lista XSPF", &["xspf"])
        .save_file()?;
    // Ensure the file has an extension; default to .m3u8 if missing
    if path.extension().is_none() {
        let mut path = path;
        path.set_extension("m3u8");
        Some(path)
    } else {
        Some(path)
    }
}

fn open_playlist_import_file(_window: Rid) -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Importar lista")
        .add_filter("Listas de reproducción", &["m3u8", "m3u", "xspf"])
        .pick_file()
}

fn export_playlist_m3u8(playlist: &Playlist, path: &Path) -> Result<(), String> {
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut content = String::from("#EXTM3U\n");
    // Save playlist name as a comment so it can be recovered on import
    content.push_str(&format!("#PLAYLIST:{}\n", playlist.name));
    for track in &playlist.tracks {
        let relative = pathdiff_diff(track, base_dir);
        let title = read_track_metadata(track).title;
        let artist = read_track_metadata(track).artist;
        let display = if artist.is_empty() {
            title.clone()
        } else {
            format!("{} - {}", artist, title)
        };
        content.push_str(&format!("#EXTINF:-1,{}\n", display));
        content.push_str(&relative);
        content.push('\n');
    }
    std::fs::write(path, content).map_err(|e| format!("No se pudo escribir el archivo: {}", e))
}

fn export_playlist_xspf(playlist: &Playlist, path: &Path) -> Result<(), String> {
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut content = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    content.push_str("<playlist version=\"1\" xmlns=\"http://xspf.org/ns/0/\">\n");
    content.push_str(&format!("  <title>{}</title>\n", xml_escape(&playlist.name)));
    content.push_str("  <trackList>\n");
    for track in &playlist.tracks {
        let relative = pathdiff_diff(track, base_dir);
        let metadata = read_track_metadata(track);
        content.push_str("    <track>\n");
        content.push_str(&format!("      <location>file://{}</location>\n", xml_escape(&relative)));
        if !metadata.title.is_empty() {
            content.push_str(&format!("      <title>{}</title>\n", xml_escape(&metadata.title)));
        }
        if !metadata.artist.is_empty() {
            content.push_str(&format!("      <creator>{}</creator>\n", xml_escape(&metadata.artist)));
        }
        if !metadata.album.is_empty() {
            content.push_str(&format!("      <album>{}</album>\n", xml_escape(&metadata.album)));
        }
        content.push_str("    </track>\n");
    }
    content.push_str("  </trackList>\n");
    content.push_str("</playlist>\n");
    std::fs::write(path, content).map_err(|e| format!("No se pudo escribir el archivo: {}", e))
}

fn import_playlist_m3u8(path: &Path) -> Result<(String, Vec<PathBuf>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("No se pudo leer el archivo: {}", e))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    // Try to read the playlist name from a #PLAYLIST: comment; fall back to file name
    let mut name = None;
    let mut tracks = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("#EXTM3U") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("#PLAYLIST:") {
            name = Some(rest.to_owned());
            continue;
        }
        if line.starts_with("#EXTINF:") {
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let track_path = if Path::new(line).is_absolute() {
            PathBuf::from(line)
        } else {
            base_dir.join(line)
        };
        tracks.push(track_path);
    }
    let name = name.unwrap_or_else(|| {
        path.file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Lista importada".to_owned())
    });
    Ok((name, tracks))
}

fn import_playlist_xspf(path: &Path) -> Result<(String, Vec<PathBuf>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("No se pudo leer el archivo: {}", e))?;
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut name = path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "Lista importada".to_owned());
    let mut tracks = Vec::new();
    let mut in_track = false;
    let mut current_tag = String::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("<title>") && !in_track {
            if let Some(start) = trimmed.find('>') {
                if let Some(end) = trimmed.rfind('<') {
                    if end > start {
                        name = xml_unescape(&trimmed[start + 1..end]);
                    }
                }
            }
        }
        if trimmed.starts_with("<track>") {
            in_track = true;
            current_tag.clear();
        }
        if trimmed.starts_with("</track>") {
            in_track = false;
        }
        if in_track && trimmed.starts_with("<location>") {
            if let Some(start) = trimmed.find('>') {
                if let Some(end) = trimmed.rfind('<') {
                    if end > start {
                        let loc = xml_unescape(&trimmed[start + 1..end]);
                        let loc = loc.strip_prefix("file://").unwrap_or(&loc);
                        let track_path = if Path::new(loc).is_absolute() {
                            PathBuf::from(loc)
                        } else {
                            base_dir.join(loc)
                        };
                        tracks.push(track_path);
                    }
                }
            }
        }
    }
    Ok((name, tracks))
}

fn pathdiff_diff(path: &Path, base: &Path) -> String {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let base = base.canonicalize().unwrap_or_else(|_| base.to_path_buf());
    let path_components: Vec<_> = path.components().collect();
    let base_components: Vec<_> = base.components().collect();
    let common = path_components.iter()
        .zip(base_components.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut result = PathBuf::new();
    for _ in common..base_components.len() {
        result.push("..");
    }
    for component in &path_components[common..] {
        result.push(component);
    }
    result.to_string_lossy().into_owned()
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

/// Contexto compartido por los dos menüs contextuales de la lista. Raven no
/// permite ocultar items, así que hay un menú para listas normales y otro para
/// resultados de YouTube; ambos usan los mismos datos.
struct ContextMenuContext {
    state: Rc<RefCell<PlayerState>>,
    ui: Ui,
}

unsafe extern "C" fn open_metadata_editor_from_context_menu(_item: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const ContextMenuContext);
    let selected = Rlist_view_get_selected(context.ui.track_list);
    if selected < 0 || context.state.borrow().youtube_mode {
        return;
    }

    let mut state = context.state.borrow_mut();
    let playlist_index = state.active_playlist;
    let track_index = selected as usize;
    let Some(playlist) = state.playlists.get_mut(playlist_index) else {
        return;
    };
    if track_index >= playlist.tracks.len() || playlist.stream_at(track_index).is_some() {
        return;
    }
    playlist.selected = Some(track_index);
    state.pending_cover = None;
    state.editor_loaded_for = None;
    state.save_playlists();
    Rswitcher_set_index(context.ui.right_panel, 1);
    update_metadata_editor(&mut state, context.ui);
}

unsafe extern "C" fn remove_from_list_from_context_menu(_item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const RefCell<PlayerState>);
    if let Ok(mut state) = state.try_borrow_mut() {
        state.remove_selected_track();
    }
}

/// Descarga explícita del resultado elegido, en Opus y en la carpeta de
/// descargas configurada. No ocurre nada en silencio: solo lo pide el usuario.
unsafe extern "C" fn download_opus_from_context_menu(_item: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const ContextMenuContext);
    let selected = Rlist_view_get_selected(context.ui.track_list);
    let Ok(mut state) = context.state.try_borrow_mut() else {
        return;
    };
    if !state.youtube_mode {
        return;
    }
    let index = (selected >= 0).then_some(selected as usize);
    state.download_youtube_track(index);
}

/// Añade el resultado elegido a la lista activa guardando solo la URL, para
/// poder reproducirlo después en streaming sin descargar el archivo.
unsafe extern "C" fn add_to_playlist_from_context_menu(_item: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const ContextMenuContext);
    let selected = Rlist_view_get_selected(context.ui.track_list);
    let Ok(mut state) = context.state.try_borrow_mut() else {
        return;
    };
    if !state.youtube_mode {
        return;
    }
    let index = (selected >= 0).then_some(selected as usize);
    state.add_youtube_to_playlist(index);
}

/// Sincroniza la vista con el modo del buscador: menú contextual, caja de
/// búsqueda y aspecto del logo. Raven no puede ocultar items, así que las
/// pistas FLAC tienen su propio menú con la entrada del conversor. `None`
/// fuerza la primera aplicación al arrancar.
struct ContextMenus {
    normal: Rid,
    youtube: Rid,
    flac: Rid,
    showing: Option<(bool, bool)>,
}

impl ContextMenus {
    fn new(normal: Rid, youtube: Rid, flac: Rid) -> Self {
        Self {
            normal,
            youtube,
            flac,
            showing: None,
        }
    }

    fn sync(&mut self, ui: Ui, youtube_mode: bool, flac_mode: bool) {
        if self.showing == Some((youtube_mode, flac_mode)) {
            return;
        }
        self.showing = Some((youtube_mode, flac_mode));
        let menu = if youtube_mode {
            self.youtube
        } else if flac_mode {
            self.flac
        } else {
            self.normal
        };
        unsafe {
            Rwidget_set_context_menu(ui.track_list, menu);
            // El modo YouTube es la «lista» activa: se atenúan las pestañas para
            // que la anterior no parezca la seleccionada (siguen siendo clicables).
            Rwidget_set_opacity(ui.playlist_tabs, if youtube_mode { 0.6 } else { 1.0 });
            // El buscador solo existe mientras está activo el modo YouTube.
            Rwidget_set_visibility(
                ui.youtube_search,
                if youtube_mode { R_VISIBLE } else { R_COLLAPSED },
            );
            if youtube_mode {
                // El logo se tiñe con el acento mientras el modo está activo.
                Rimage_set_tint_text(ui.youtube_icon, R_FALSE);
                Rimage_set_tint_color(ui.youtube_icon, 0.725, 0.639, 0.961, 1.0);
                // Al aparecer, el campo ya está listo para escribir.
                Rwidget_focus(ui.youtube_search);
            } else {
                Rimage_set_tint_text(ui.youtube_icon, R_TRUE);
            }
        }
    }

    fn destroy(&self, widget: Rid) {
        unsafe {
            Rwidget_set_context_menu(widget, R_INVALID_ID);
            Rwidget_destroy(self.normal);
            Rwidget_destroy(self.youtube);
            Rwidget_destroy(self.flac);
        }
    }
}

unsafe extern "C" fn check_updates_from_app_menu(_item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const RefCell<PlayerState>);
    if let Ok(mut state) = state.try_borrow_mut() {
        state.start_update_check();
    }
}

/// Lo que el usuario elige en el diálogo del conversor. Los callbacks de Raven
/// actualizan esta estructura en sitio; `Rdialog_run` la deja lista al volver.
struct ConverterDialogData {
    quality_index: usize,
    custom_bitrate: u32,
    files: Vec<PathBuf>,
    replace: bool,
    delete: bool,
}

/// Punteros a los widgets que deben cambiar cuando cambia otro. Vive en la
/// pila mientras `Rdialog_run` está ejecutando los callbacks.
struct ConverterCallbacks {
    data: *mut ConverterDialogData,
    quality: [Rid; 5],
    replace: Rid,
    files_view: Rid,
    status: Rid,
    listed: *const HashSet<PathBuf>,
    replace_touched: bool,
}

const CONVERTER_QUALITY_LABELS: [&str; 5] = [
    "96 kbps",
    "128 kbps",
    "160 kbps (recomendada)",
    "192 kbps",
    "Personalizada",
];

/// Índice del preset recomendado en el diálogo del conversor.
fn default_quality_index() -> usize {
    converter::PRESET_BITRATES
        .iter()
        .position(|&bitrate| bitrate == converter::RECOMMENDED_BITRATE)
        .expect("el preset recomendado debe estar en PRESET_BITRATES")
}

unsafe extern "C" fn converter_quality_changed(
    checkbox: Rid,
    checked: c_int,
    user_data: *mut c_void,
) {
    if checked == R_FALSE {
        return;
    }
    let callbacks = &*(user_data as *const ConverterCallbacks);
    for (index, radio) in callbacks.quality.iter().enumerate() {
        if *radio == checkbox {
            (*callbacks.data).quality_index = index;
        }
    }
}

unsafe extern "C" fn converter_custom_bitrate_changed(
    _spin_box: Rid,
    value: f64,
    user_data: *mut c_void,
) {
    let callbacks = &*(user_data as *const ConverterCallbacks);
    (*callbacks.data).custom_bitrate = converter::clamp_bitrate(value.round() as u32);
    (*callbacks.data).quality_index = CONVERTER_QUALITY_LABELS.len() - 1;
    Rcheckbox_set_checked(callbacks.quality[CONVERTER_QUALITY_LABELS.len() - 1], R_TRUE);
}

unsafe extern "C" fn converter_replace_changed(
    _checkbox: Rid,
    checked: c_int,
    user_data: *mut c_void,
) {
    let callbacks = &mut *(user_data as *mut ConverterCallbacks);
    callbacks.replace_touched = true;
    (*callbacks.data).replace = checked != R_FALSE;
}

unsafe extern "C" fn converter_delete_changed(
    _checkbox: Rid,
    checked: c_int,
    user_data: *mut c_void,
) {
    let callbacks = &*(user_data as *const ConverterCallbacks);
    (*callbacks.data).delete = checked != R_FALSE;
}

unsafe fn converter_refresh_files(callbacks: &ConverterCallbacks) {
    Rlist_view_clear_items(callbacks.files_view);
    for path in &(*callbacks.data).files {
        Rlist_view_add_item(callbacks.files_view, cstr(&path.to_string_lossy()).as_ptr());
    }
    let count = (*callbacks.data).files.len();
    let text = format!("{count} archivo{} seleccionado{}", if count == 1 { "" } else { "s" }, if count == 1 { "" } else { "s" });
    Rlabel_set_text(callbacks.status, cstr(&text).as_ptr());
}

unsafe extern "C" fn converter_add_files(_button: Rid, user_data: *mut c_void) {
    let callbacks = &mut *(user_data as *mut ConverterCallbacks);
    let paths = rfd::FileDialog::new()
        .set_title("Seleccionar archivos FLAC")
        .add_filter("Archivos FLAC", &["flac"])
        .pick_files()
        .unwrap_or_default();
    let mut added = false;
    for path in paths {
        if converter::is_flac(&path) && !(*callbacks.data).files.contains(&path) {
            (*callbacks.data).files.push(path);
            added = true;
        }
    }
    if !added {
        return;
    }
    // En el modo Herramientas el usuario todavía no ha elegido archivos: si
    // alguno está en una lista, proponemos reemplazarlo. Si ya tocó la casilla,
    // respetamos su decisión.
    if !callbacks.replace_touched
        && (*callbacks.data)
            .files
            .iter()
            .any(|path| (*callbacks.listed).contains(path))
    {
        (*callbacks.data).replace = true;
        Rcheckbox_set_checked(callbacks.replace, R_TRUE);
    }
    converter_refresh_files(callbacks);
}

unsafe extern "C" fn converter_remove_file(_button: Rid, user_data: *mut c_void) {
    let callbacks = &mut *(user_data as *mut ConverterCallbacks);
    let selected = Rlist_view_get_selected(callbacks.files_view);
    if selected < 0 {
        return;
    }
    let index = selected as usize;
    if index < (*callbacks.data).files.len() {
        (*callbacks.data).files.remove(index);
        converter_refresh_files(callbacks);
    }
}

unsafe extern "C" fn converter_accept(button: Rid, user_data: *mut c_void) {
    let callbacks = &mut *(user_data as *mut ConverterCallbacks);
    let valid: Vec<PathBuf> = (*callbacks.data)
        .files
        .iter()
        .filter(|path| converter::is_flac(path) && path.is_file())
        .cloned()
        .collect();
    if valid.is_empty() {
        Rlabel_set_text(
            callbacks.status,
            cstr("Selecciona al menos un archivo FLAC existente").as_ptr(),
        );
        return;
    }
    (*callbacks.data).files = valid;
    Rdialog_close(button, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn converter_cancel(button: Rid, _user_data: *mut c_void) {
    Rdialog_close(button, R_MESSAGE_RESULT_CANCEL);
}

/// Botones del diálogo de colisión: cada uno guarda su decisión y cierra.
struct CollisionButton {
    choice: *mut c_int,
    value: c_int,
}

unsafe extern "C" fn collision_button_clicked(button: Rid, user_data: *mut c_void) {
    let option = &*(user_data as *const CollisionButton);
    *option.choice = option.value;
    Rdialog_close(button, option.value);
}

unsafe extern "C" fn collision_apply_changed(
    _checkbox: Rid,
    checked: c_int,
    user_data: *mut c_void,
) {
    *(user_data as *mut c_int) = checked;
}

/// Pregunta qué hacer con un `.opus` ya existente. La casilla devuelve si la
/// respuesta debe fijarse para el resto de colisiones del lote.
fn ask_collision(window: Rid, source: &Path, desired: &Path) -> (c_int, bool) {
    unsafe {
        let content = Rwidget_create(R_WIDGET_VBOX);
        Rwidget_set_size(content, 430.0, -1.0);
        Rpanel_set_spacing(content, 12.0);
        Rslot_set_padding(content, 22.0, 18.0, 22.0, 18.0);

        let title = Rlabel_create(cstr("El archivo Opus ya existe").as_ptr());
        Rlabel_set_text_size(title, 17.0);
        Rwidget_add_child(content, title);

        let name = desired
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let source_name = source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let text = format!(
            "Ya existe «{name}» junto a «{source_name}». ¿Qué quieres hacer con este archivo?"
        );
        let message = Rlabel_create(cstr(&text).as_ptr());
        Rlabel_set_text_size(message, 12.0);
        Rlabel_set_wrap(message, R_TRUE);
        Rwidget_set_max_size(message, 390.0, -1.0);
        Rwidget_add_child(content, message);

        let mut choice: c_int = 3; // Omitir, también si se cierra con Escape.
        let buttons = [
            CollisionButton { choice: &mut choice, value: 1 },
            CollisionButton { choice: &mut choice, value: 2 },
            CollisionButton { choice: &mut choice, value: 3 },
        ];
        let row = Rwidget_create(R_WIDGET_HBOX);
        Rpanel_set_spacing(row, 8.0);
        Rslot_set_halign(row, R_HALIGN_RIGHT);
        for (label, style, option) in [
            ("Sobrescribir", R_BUTTON_PRIMARY, &buttons[0]),
            ("Cambiar nombre", R_BUTTON_FLAT, &buttons[1]),
            ("Omitir", R_BUTTON_FLAT, &buttons[2]),
        ] {
            let button = Rbutton_create(cstr(label).as_ptr());
            Rbutton_set_style(button, style);
            Rbutton_set_clicked_callback(
                button,
                Some(collision_button_clicked),
                option as *const CollisionButton as *mut c_void,
            );
            Rwidget_add_child(row, button);
        }
        Rwidget_add_child(content, row);

        let mut apply: c_int = R_FALSE;
        let apply_check = Rcheckbox_create(cstr("Aplicar la misma opción al resto").as_ptr());
        Rcheckbox_set_changed_callback(
            apply_check,
            Some(collision_apply_changed),
            &mut apply as *mut c_int as *mut c_void,
        );
        Rwidget_add_child(content, apply_check);

        Rdialog_run(window, content);
        (choice, apply != R_FALSE)
    }
}

/// Prepara el lote y lo lanza. Las colisiones se resuelven antes de crear los
/// temporales, así que el hilo de conversión no necesita diálogos.
fn launch_conversion(
    window: Rid,
    state: &Rc<RefCell<PlayerState>>,
    data: ConverterDialogData,
) {
    use converter::CollisionPolicy;

    let quality = converter::quality_from_option(data.quality_index, data.custom_bitrate)
        .unwrap_or_default();
    let inputs: Vec<PathBuf> = data
        .files
        .into_iter()
        .filter(|path| converter::is_flac(path) && path.is_file())
        .collect();
    if inputs.is_empty() {
        state.borrow_mut().status = "Selecciona al menos un archivo FLAC".to_owned();
        return;
    }

    let generation = state.borrow().converter_generation + 1;
    let mut occupied = converter::existing_outputs_in(&inputs);
    let mut fixed_policy: Option<CollisionPolicy> = None;
    let mut tasks = Vec::new();
    let mut skipped = 0;
    for source in &inputs {
        let desired = converter::output_path(source);
        let policy = if occupied.contains(&desired) {
            match fixed_policy {
                Some(policy) => policy,
                None => {
                    let (choice, apply_to_rest) = ask_collision(window, source, &desired);
                    let policy =
                        converter::collision_policy_from_choice(choice).unwrap_or(CollisionPolicy::Skip);
                    if apply_to_rest {
                        fixed_policy = Some(policy);
                    }
                    policy
                }
            }
        } else {
            CollisionPolicy::Overwrite
        };
        let Some(output) = converter::resolve_output_path(source, &occupied, policy) else {
            skipped += 1;
            continue;
        };
        occupied.push(output.clone());
        tasks.push(converter::ConversionTask::new(
            source.clone(),
            output,
            quality,
            policy == CollisionPolicy::Overwrite,
            generation,
            tasks.len(),
        ));
    }

    if tasks.is_empty() {
        state.borrow_mut().status = "No quedó ningún archivo para convertir".to_owned();
        return;
    }
    let mut state = state.borrow_mut();
    if let Err(error) = state.start_conversion_job(
        tasks,
        generation,
        data.replace,
        data.delete,
        skipped,
    ) {
        state.status = error;
    }
}

fn open_converter_dialog(
    window: Rid,
    state: Rc<RefCell<PlayerState>>,
    preselected: Option<PathBuf>,
) {
    let listed: HashSet<PathBuf> = state
        .borrow()
        .playlists
        .iter()
        .flat_map(|playlist| playlist.tracks.iter().cloned())
        .collect();
    let mut data = ConverterDialogData {
        quality_index: default_quality_index(),
        custom_bitrate: converter::RECOMMENDED_BITRATE,
        files: preselected.into_iter().collect(),
        replace: false,
        delete: false,
    };
    data.replace = data.files.iter().any(|path| listed.contains(path));

    unsafe {
        let content = Rwidget_create(R_WIDGET_VBOX);
        Rwidget_set_size(content, 480.0, -1.0);
        Rpanel_set_spacing(content, 8.0);
        Rslot_set_padding(content, 18.0, 14.0, 18.0, 14.0);

        let title = Rlabel_create(cstr("Conversor FLAC a OPUS").as_ptr());
        Rlabel_set_text_size(title, 18.0);
        Rwidget_add_child(content, title);
        Rwidget_add_child(
            content,
            settings_description("Convierte FLAC a Opus en la misma carpeta y conserva las etiquetas de texto."),
        );

        let mut callbacks = ConverterCallbacks {
            data: &mut data,
            quality: [R_INVALID_ID; 5],
            replace: R_INVALID_ID,
            files_view: R_INVALID_ID,
            status: R_INVALID_ID,
            listed: &listed,
            replace_touched: false,
        };

        Rwidget_add_child(content, settings_heading("Calidad"));
        let quality_panel = Rwidget_create(R_WIDGET_VBOX);
        Rpanel_set_spacing(quality_panel, 2.0);
        for (index, label) in CONVERTER_QUALITY_LABELS.iter().enumerate() {
            let radio = Rcheckbox_create(cstr(label).as_ptr());
            Rcheckbox_set_style(radio, R_CHECKBOX_RADIO);
            Rcheckbox_set_checked(radio, if index == data.quality_index { R_TRUE } else { R_FALSE });
            Rwidget_add_child(quality_panel, radio);
            callbacks.quality[index] = radio;
        }
        let custom_row = Rwidget_create(R_WIDGET_HBOX);
        Rpanel_set_spacing(custom_row, 4.0);
        let spin = Rwidget_create(R_WIDGET_SPIN_BOX);
        Rspin_box_set_range(spin, converter::CUSTOM_MIN as f64, converter::CUSTOM_MAX as f64);
        Rspin_box_set_step(spin, 1.0);
        Rspin_box_set_decimals(spin, 0);
        Rspin_box_set_value(spin, data.custom_bitrate as f64);
        Rwidget_set_min_size(spin, 90.0, 0.0);
        Rwidget_add_child(custom_row, spin);
        Rwidget_add_child(custom_row, Rlabel_create(cstr("kbps (6–510)").as_ptr()));
        Rwidget_add_child(quality_panel, custom_row);
        Rwidget_add_child(content, quality_panel);

        Rwidget_add_child(content, settings_heading("Archivos"));
        let files_view = Rwidget_create(R_WIDGET_LIST_VIEW);
        Rwidget_set_min_size(files_view, 400.0, 70.0);
        Rwidget_set_max_size(files_view, -1.0, 70.0);
        Rslot_set_fill(files_view, 0.0);
        callbacks.files_view = files_view;
        Rwidget_add_child(content, files_view);

        let files_row = Rwidget_create(R_WIDGET_HBOX);
        Rpanel_set_spacing(files_row, 6.0);
        let add_button = Rbutton_create(cstr("Añadir FLAC…").as_ptr());
        Rbutton_set_clicked_callback(
            add_button,
            Some(converter_add_files),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );
        Rwidget_add_child(files_row, add_button);
        let remove_button = Rbutton_create(cstr("Quitar seleccionado").as_ptr());
        Rbutton_set_clicked_callback(
            remove_button,
            Some(converter_remove_file),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );
        Rwidget_add_child(files_row, remove_button);
        let status = Rlabel_create(cstr("").as_ptr());
        Rlabel_set_text_size(status, 11.0);
        Rslot_set_fill(status, 1.0);
        Rslot_set_halign(status, R_HALIGN_RIGHT);
        callbacks.status = status;
        Rwidget_add_child(files_row, status);
        Rwidget_add_child(content, files_row);
        converter_refresh_files(&callbacks);

        Rwidget_add_child(content, settings_heading("Opciones"));
        let replace = Rcheckbox_create(cstr("Remplazar cancion en las listas").as_ptr());
        Rcheckbox_set_checked(replace, if data.replace { R_TRUE } else { R_FALSE });
        callbacks.replace = replace;
        Rwidget_add_child(content, replace);
        let delete = Rcheckbox_create(cstr("Borrar FLAC despues de conversion").as_ptr());
        Rcheckbox_set_checked(delete, R_FALSE);
        Rwidget_add_child(content, delete);

        for radio in callbacks.quality {
            Rcheckbox_set_changed_callback(
                radio,
                Some(converter_quality_changed),
                &mut callbacks as *mut ConverterCallbacks as *mut c_void,
            );
        }
        Rspin_box_set_changed_callback(
            spin,
            Some(converter_custom_bitrate_changed),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );
        Rcheckbox_set_changed_callback(
            replace,
            Some(converter_replace_changed),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );
        Rcheckbox_set_changed_callback(
            delete,
            Some(converter_delete_changed),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );

        let buttons = Rwidget_create(R_WIDGET_HBOX);
        Rpanel_set_spacing(buttons, 8.0);
        Rslot_set_halign(buttons, R_HALIGN_RIGHT);
        let cancel = Rbutton_create(cstr("Cancelar").as_ptr());
        Rbutton_set_clicked_callback(cancel, Some(converter_cancel), std::ptr::null_mut());
        Rwidget_add_child(buttons, cancel);
        let convert = Rbutton_create(cstr("Convertir").as_ptr());
        Rbutton_set_style(convert, R_BUTTON_PRIMARY);
        Rbutton_set_clicked_callback(
            convert,
            Some(converter_accept),
            &mut callbacks as *mut ConverterCallbacks as *mut c_void,
        );
        Rwidget_add_child(buttons, convert);
        Rwidget_add_child(content, buttons);

        if Rdialog_run(window, content) != R_MESSAGE_RESULT_OK {
            return;
        }
        launch_conversion(window, &state, data);
    }
}

unsafe extern "C" fn open_converter_from_app_menu(item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const Rc<RefCell<PlayerState>>);
    let window = Rwidget_get_window(item);
    open_converter_dialog(window, Rc::clone(state), None);
}

unsafe extern "C" fn open_converter_from_context_menu(item: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const ContextMenuContext);
    let path = {
        let state = context.state.borrow();
        if state.youtube_mode {
            return;
        }
        state
            .selected_location()
            .and_then(|(playlist, track)| {
                state.playlists.get(playlist)?.tracks.get(track).cloned()
            })
            .filter(|path| converter::is_flac(path))
    };
    let Some(path) = path else {
        return;
    };
    let window = Rwidget_get_window(item);
    open_converter_dialog(window, Rc::clone(&context.state), Some(path));
}

const SETTINGS_SECTIONS: [&str; 4] = ["General", "Listas", "YouTube", "Descargas"];

/// Datos que necesita el diálogo de ajustes mientras está abierto. `Aplicar`
/// conserva el contexto porque el diálogo sigue en pie; solo al cerrarlo se
/// libera y, si fue con `Aceptar`, se aplican los cambios pendientes.
struct SettingsDialogContext {
    state: Rc<RefCell<PlayerState>>,
    open_options: [Rid; 3],
    add_options: [Rid; 2],
    youtube_options: [Rid; 2],
    download_dir: Rc<RefCell<Option<PathBuf>>>,
    download_dir_label: Rid,
    menu: Rid,
    pages: Rid,
}

unsafe fn settings_heading(text: &str) -> Rid {
    let label = Rlabel_create(cstr(text).as_ptr());
    Rlabel_set_text_size(label, 14.0);
    Rlabel_set_color(label, 0.725, 0.639, 0.961, 1.0);
    label
}

unsafe fn settings_description(text: &str) -> Rid {
    let label = Rlabel_create(cstr(text).as_ptr());
    Rlabel_set_text_size(label, 12.0);
    Rlabel_set_wrap(label, R_TRUE);
    // Sin un ancho máximo Raven mide el texto de una sola línea y el diálogo
    // se ensancha al cambiar de sección.
    Rwidget_set_max_size(label, 400.0, -1.0);
    label
}

unsafe extern "C" fn open_settings_dialog(item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const Rc<RefCell<PlayerState>>);
    let window = Rwidget_get_window(item);
    let (current_open, current_add, current_youtube, current_download) = {
        let state = state.borrow();
        (
            state.open_behavior,
            state.add_behavior,
            state.youtube_playback,
            state.download_dir.clone(),
        )
    };
    let current_download = Rc::new(RefCell::new(current_download));

    let content = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(content, 12.0);
    Rslot_set_padding(content, 24.0, 20.0, 24.0, 20.0);

    let title = Rlabel_create(cstr("Ajustes").as_ptr());
    Rlabel_set_text_size(title, 19.0);
    Rwidget_add_child(content, title);

    let body = Rwidget_create(R_WIDGET_HBOX);
    Rpanel_set_spacing(body, 16.0);

    let menu = Rwidget_create(R_WIDGET_LIST_VIEW);
    Rwidget_set_min_size(menu, 160.0, 0.0);
    for section in SETTINGS_SECTIONS {
        Rlist_view_add_item(menu, cstr(section).as_ptr());
    }
    Rlist_view_set_selected(menu, 0);
    Rwidget_add_child(body, menu);

    let pages = Rwidget_create(R_WIDGET_SWITCHER);
    Rwidget_set_min_size(pages, 430.0, 250.0);
    Rslot_set_fill(pages, 1.0);

    // General: comportamiento al abrir archivos desde fuera de la app.
    let general = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(general, 10.0);
    Rwidget_add_child(general, settings_heading("Comportamiento al abrir archivos"));
    Rwidget_add_child(
        general,
        settings_description("Elige qué ocurre cuando abres un archivo con Freeway Player ya en marcha."),
    );
    let options_panel = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(options_panel, 6.0);
    let mut open_options = [R_INVALID_ID; 3];
    for behavior in OpenBehavior::ALL {
        // Las casillas radio son hermanas: Raven desmarca las demás del mismo
        // padre al marcar una.
        let option = Rcheckbox_create(cstr(behavior.label()).as_ptr());
        Rcheckbox_set_style(option, R_CHECKBOX_RADIO);
        Rcheckbox_set_checked(option, if behavior == current_open { R_TRUE } else { R_FALSE });
        Rwidget_add_child(options_panel, option);
        open_options[behavior.index()] = option;
    }
    Rwidget_add_child(general, options_panel);
    Rwidget_add_child(pages, general);

    // Listas: comportamiento al añadir pistas desde la propia app.
    let lists = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(lists, 10.0);
    Rwidget_add_child(lists, settings_heading("Comportamiento al añadir a la lista"));
    Rwidget_add_child(
        lists,
        settings_description("Elige qué ocurre al añadir pistas desde la app, tanto si las arrastras a la ventana como si usas «+ Añadir pistas». No afecta a los archivos abiertos desde el gestor."),
    );
    let add_options_panel = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(add_options_panel, 6.0);
    let mut add_options = [R_INVALID_ID; 2];
    for behavior in AddBehavior::ALL {
        let option = Rcheckbox_create(cstr(behavior.label()).as_ptr());
        Rcheckbox_set_style(option, R_CHECKBOX_RADIO);
        Rcheckbox_set_checked(option, if behavior == current_add { R_TRUE } else { R_FALSE });
        Rwidget_add_child(add_options_panel, option);
        add_options[behavior.index()] = option;
    }
    Rwidget_add_child(lists, add_options_panel);
    Rwidget_add_child(pages, lists);

    // YouTube: streaming directo o descarga Opus completa antes de sonar.
    let youtube = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(youtube, 10.0);
    Rwidget_add_child(youtube, settings_heading("Reproducción de YouTube"));
    Rwidget_add_child(
        youtube,
        settings_description("Streaming empieza a sonar en cuanto llega el audio; la descarga completa espera a bajarlo y convertirlo a Opus, y permite avanzar dentro de la pista. El cambio afecta a la siguiente reproducción."),
    );
    let youtube_options_panel = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(youtube_options_panel, 6.0);
    let mut youtube_options = [R_INVALID_ID; 2];
    for mode in YoutubePlayback::ALL {
        let option = Rcheckbox_create(cstr(mode.label()).as_ptr());
        Rcheckbox_set_style(option, R_CHECKBOX_RADIO);
        Rcheckbox_set_checked(option, if mode == current_youtube { R_TRUE } else { R_FALSE });
        Rwidget_add_child(youtube_options_panel, option);
        youtube_options[mode.index()] = option;
    }
    Rwidget_add_child(youtube, youtube_options_panel);
    Rwidget_add_child(pages, youtube);

    // Descargas: carpeta para las descargas explícitas en Opus.
    let downloads = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(downloads, 10.0);
    Rwidget_add_child(downloads, settings_heading("Descargas"));
    Rwidget_add_child(
        downloads,
        settings_description("Carpeta donde se guardarán las pistas de YouTube que decidas descargar. Buscar y escuchar en streaming no escribe ningún archivo."),
    );
    let downloads_row = Rwidget_create(R_WIDGET_HBOX);
    Rpanel_set_spacing(downloads_row, 8.0);
    let download_dir_label = Rlabel_create(
        cstr(&download_dir_text(current_download.borrow().as_deref())).as_ptr(),
    );
    Rlabel_set_text_size(download_dir_label, 12.0);
    Rslot_set_fill(download_dir_label, 1.0);
    Rwidget_add_child(downloads_row, download_dir_label);
    let choose_button = Rbutton_create(cstr("Elegir carpeta…").as_ptr());
    Rwidget_add_child(downloads_row, choose_button);
    Rwidget_add_child(downloads, downloads_row);
    Rwidget_add_child(pages, downloads);

    Rwidget_add_child(body, pages);
    Rwidget_add_child(content, body);

    let separator = Rwidget_create(R_WIDGET_SEPARATOR);
    Rwidget_add_child(content, separator);

    let buttons = Rwidget_create(R_WIDGET_HBOX);
    Rpanel_set_spacing(buttons, 8.0);
    Rslot_set_halign(buttons, R_HALIGN_RIGHT);
    let cancel = Rbutton_create(cstr("Cancelar").as_ptr());
    Rbutton_set_style(cancel, R_BUTTON_FLAT);
    let apply = Rbutton_create(cstr("Aplicar").as_ptr());
    Rbutton_set_style(apply, R_BUTTON_DEFAULT);
    let accept = Rbutton_create(cstr("Aceptar").as_ptr());
    Rbutton_set_style(accept, R_BUTTON_PRIMARY);
    Rwidget_add_child(buttons, cancel);
    Rwidget_add_child(buttons, apply);
    Rwidget_add_child(buttons, accept);
    Rwidget_add_child(content, buttons);

    let context = Box::into_raw(Box::new(SettingsDialogContext {
        state: Rc::clone(state),
        open_options,
        add_options,
        youtube_options,
        download_dir: current_download,
        download_dir_label,
        menu,
        pages,
    })) as *mut c_void;
    Rbutton_set_clicked_callback(cancel, Some(settings_dialog_cancel), std::ptr::null_mut());
    Rbutton_set_clicked_callback(apply, Some(settings_dialog_apply), context);
    Rbutton_set_clicked_callback(accept, Some(settings_dialog_accept), std::ptr::null_mut());
    Rbutton_set_clicked_callback(choose_button, Some(choose_download_dir), context);
    Rlist_view_set_selection_changed_callback(menu, Some(settings_dialog_section_changed), context);
    Rdialog_open(window, content, Some(settings_dialog_closed), context);
}

unsafe extern "C" fn settings_dialog_section_changed(
    _menu: Rid,
    index: c_int,
    user_data: *mut c_void,
) {
    if index < 0 || index as usize >= SETTINGS_SECTIONS.len() {
        return;
    }
    let context = &*(user_data as *const SettingsDialogContext);
    Rswitcher_set_index(context.pages, index);
}

/// Copia al estado las opciones visibles en el diálogo. Solo escribe en disco
/// lo que haya cambiado; `Aplicar` llama a esto sin cerrar la ventana.
unsafe fn apply_settings(context: &SettingsDialogContext) {
    fn record(result: Result<(), String>, state: &mut PlayerState, failed: &mut bool) {
        if let Err(error) = result {
            state.status = format!("No se pudo guardar el ajuste: {error}");
            *failed = true;
        }
    }

    let open_behavior = context
        .open_options
        .iter()
        .position(|option| Rcheckbox_is_checked(*option) != R_FALSE)
        .and_then(behavior_from_option_index);
    let add_behavior = context
        .add_options
        .iter()
        .position(|option| Rcheckbox_is_checked(*option) != R_FALSE)
        .and_then(add_behavior_from_option_index);
    let youtube_playback = context
        .youtube_options
        .iter()
        .position(|option| Rcheckbox_is_checked(*option) != R_FALSE)
        .and_then(youtube_playback_from_option_index);
    let download_dir = context.download_dir.borrow().clone();

    let Ok(mut state) = context.state.try_borrow_mut() else {
        return;
    };
    let mut changed = false;
    let mut failed = false;
    if let Some(behavior) = open_behavior {
        if state.open_behavior != behavior {
            changed = true;
            record(state.set_open_behavior(behavior), &mut state, &mut failed);
        }
    }
    if let Some(behavior) = add_behavior {
        if state.add_behavior != behavior {
            changed = true;
            record(state.set_add_behavior(behavior), &mut state, &mut failed);
        }
    }
    if let Some(mode) = youtube_playback {
        if state.youtube_playback != mode {
            changed = true;
            record(state.set_youtube_playback(mode), &mut state, &mut failed);
        }
    }
    if state.download_dir != download_dir {
        changed = true;
        record(state.set_download_dir(download_dir), &mut state, &mut failed);
    }
    if changed && !failed {
        state.status = "Ajustes aplicados".to_owned();
    }
}

unsafe extern "C" fn choose_download_dir(_button: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const SettingsDialogContext);
    let Some(path) = rfd::FileDialog::new()
        .set_title("Carpeta de descargas")
        .pick_folder()
    else {
        return;
    };
    *context.download_dir.borrow_mut() = Some(path.clone());
    set_label(context.download_dir_label, &download_dir_text(Some(&path)));
}

unsafe extern "C" fn settings_dialog_accept(button: Rid, _user_data: *mut c_void) {
    Rdialog_close(button, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn settings_dialog_apply(_button: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const SettingsDialogContext);
    apply_settings(context);
}

unsafe extern "C" fn settings_dialog_cancel(button: Rid, _user_data: *mut c_void) {
    Rdialog_close(button, R_MESSAGE_RESULT_CANCEL);
}

unsafe extern "C" fn settings_dialog_closed(_content: Rid, result: c_int, user_data: *mut c_void) {
    let context = Box::from_raw(user_data as *mut SettingsDialogContext);
    // Raven va a destruir el contenido: sellamos la selección para que no
    // quede una llamada pendiente con este contexto ya liberado.
    Rlist_view_set_selection_changed_callback(context.menu, None, std::ptr::null_mut());
    if result == R_MESSAGE_RESULT_OK {
        apply_settings(&context);
    }
}

unsafe extern "C" fn open_about_dialog(_item: Rid, user_data: *mut c_void) {
    let window = user_data as Rid;

    // Build a VBox container for the dialog content
    let container = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(container, 12.0);

    // App icon image
    let icon_image = Rwidget_create(R_WIDGET_IMAGE);
    Rslot_set_size(icon_image, 64.0, 64.0);
    Rslot_set_halign(icon_image, R_HALIGN_CENTER);
    if APP_ICON_PNG.len() > 0 {
        let texture = Rtexture_create(APP_ICON_PNG.as_ptr().cast(), APP_ICON_PNG.len());
        if texture != R_INVALID_ID {
            Rimage_set_texture(icon_image, texture);
            Rimage_set_stretch(icon_image, R_STRETCH_FIT);
        }
    }
    Rwidget_add_child(container, icon_image);

    // Title: "Freeway Player"
    let title = Rlabel_create(cstr("Freeway Player").as_ptr());
    Rlabel_set_text_size(title, 20.0);
    Rlabel_set_text_align(title, R_TEXT_ALIGN_CENTER);
    Rwidget_add_child(container, title);

    // Version
    let version_text = format!("Versión {}", env!("CARGO_PKG_VERSION"));
    let version_label = Rlabel_create(cstr(&version_text).as_ptr());
    Rlabel_set_text_size(version_label, 13.0);
    Rlabel_set_text_align(version_label, R_TEXT_ALIGN_CENTER);
    Rwidget_add_child(container, version_label);

    // Separator
    let sep = Rwidget_create(R_WIDGET_SEPARATOR);
    Rslot_set_padding(sep, 20.0, 0.0, 20.0, 0.0);
    Rwidget_add_child(container, sep);

    // Author
    let author = Rlabel_create(cstr("Creado por Daniel Álvarez").as_ptr());
    Rlabel_set_text_size(author, 13.0);
    Rlabel_set_text_align(author, R_TEXT_ALIGN_CENTER);
    Rwidget_add_child(container, author);

    // GitHub link
    let github = Rlabel_create(cstr("github.com/Daniel-A-G/Freeway-Player-R").as_ptr());
    Rlabel_set_text_size(github, 12.0);
    Rlabel_set_text_align(github, R_TEXT_ALIGN_CENTER);
    Rwidget_add_child(container, github);

    // Technology
    let tech = Rlabel_create(cstr("Escrito en Rust con Raven y rodio").as_ptr());
    Rlabel_set_text_size(tech, 12.0);
    Rlabel_set_text_align(tech, R_TEXT_ALIGN_CENTER);
    Rwidget_add_child(container, tech);

    // Separator
    let sep2 = Rwidget_create(R_WIDGET_SEPARATOR);
    Rslot_set_padding(sep2, 20.0, 0.0, 20.0, 0.0);
    Rwidget_add_child(container, sep2);

    // License info
    let license = Rlabel_create(
        cstr("Licencia GNU GPL v3.0\nCopyright (C) 2026 Daniel Álvarez\n\nEste programa es software libre: puede redistribuirlo y/o modificarlo bajo los términos de la GNU General Public License versión 3.").as_ptr()
    );
    Rlabel_set_text_size(license, 11.0);
    Rlabel_set_text_align(license, R_TEXT_ALIGN_CENTER);
    Rlabel_set_wrap(license, R_TRUE);
    Rwidget_add_child(container, license);

    // Spacer
    let spacer = Rwidget_create(R_WIDGET_SPACER);
    Rslot_set_size(spacer, 0.0, 4.0);
    Rwidget_add_child(container, spacer);

    // OK button
    let ok_button = Rbutton_create(cstr("Aceptar").as_ptr());
    Rbutton_set_style(ok_button, R_BUTTON_PRIMARY);
    Rslot_set_halign(ok_button, R_HALIGN_CENTER);
    Rwidget_add_child(container, ok_button);

    // Clicking OK closes the dialog
    Rbutton_set_clicked_callback(ok_button, Some(about_dialog_close), std::ptr::null_mut());

    // Open the dialog (modal, no closed callback needed)
    Rdialog_open(window, container, None, std::ptr::null_mut());
}

unsafe extern "C" fn about_dialog_close(widget: Rid, _user_data: *mut c_void) {
    Rdialog_close(widget, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn manual_update_dialog_close(widget: Rid, _user_data: *mut c_void) {
    Rdialog_close(widget, R_MESSAGE_RESULT_CANCEL);
}

unsafe extern "C" fn open_store_and_close(widget: Rid, _user_data: *mut c_void) {
    updates::open_external_uri(updates::APPSTREAM_URI);
    Rdialog_close(widget, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn open_github_and_close(widget: Rid, _user_data: *mut c_void) {
    updates::open_external_uri(updates::GITHUB_RELEASES_URL);
    Rdialog_close(widget, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn open_website_and_close(widget: Rid, _user_data: *mut c_void) {
    updates::open_external_uri(updates::PROJECT_WEBSITE_URL);
    Rdialog_close(widget, R_MESSAGE_RESULT_OK);
}

fn open_manual_update_dialog(window: Rid, tag: &str, has_remote: bool) {
    unsafe {
        let container = Rwidget_create(R_WIDGET_VBOX);
        Rpanel_set_spacing(container, 12.0);

        // App icon
        let icon_image = Rwidget_create(R_WIDGET_IMAGE);
        Rslot_set_size(icon_image, 64.0, 64.0);
        Rslot_set_halign(icon_image, R_HALIGN_CENTER);
        if !APP_ICON_PNG.is_empty() {
            let texture = Rtexture_create(APP_ICON_PNG.as_ptr().cast(), APP_ICON_PNG.len());
            if texture != R_INVALID_ID {
                Rimage_set_texture(icon_image, texture);
                Rimage_set_stretch(icon_image, R_STRETCH_FIT);
            }
        }
        Rwidget_add_child(container, icon_image);

        // Title
        let title = Rlabel_create(cstr("Actualización disponible").as_ptr());
        Rlabel_set_text_size(title, 19.0);
        Rlabel_set_text_align(title, R_TEXT_ALIGN_CENTER);
        Rwidget_add_child(container, title);

        // Version tag
        let version_text = format!("Nueva versión {tag}");
        let version_label = Rlabel_create(cstr(&version_text).as_ptr());
        Rlabel_set_text_size(version_label, 13.0);
        Rlabel_set_text_align(version_label, R_TEXT_ALIGN_CENTER);
        Rwidget_add_child(container, version_label);

        // Separator
        let sep = Rwidget_create(R_WIDGET_SEPARATOR);
        Rslot_set_padding(sep, 20.0, 0.0, 20.0, 0.0);
        Rwidget_add_child(container, sep);

        // Explanation text
        let explanation_text = if has_remote {
            "Esta versión introduce cambios en los permisos de la aplicación (o configuración del sistema) y requiere autorización manual.\n\nTienes el repositorio Flatpak configurado. Puedes actualizarla fácilmente abriendo la tienda de aplicaciones de tu sistema (Discover / Software) para revisar y autorizar los nuevos permisos."
        } else {
            "Esta versión requiere actualización manual.\n\nNo se ha detectado el repositorio Flatpak añadido en tu sistema. Puedes descargar el nuevo paquete .flatpak desde GitHub o añadir el repositorio oficial (.flatpakref) para recibir futuras actualizaciones directamente."
        };
        let label = Rlabel_create(cstr(explanation_text).as_ptr());
        Rlabel_set_text_size(label, 12.0);
        Rlabel_set_text_align(label, R_TEXT_ALIGN_CENTER);
        Rlabel_set_wrap(label, R_TRUE);
        Rwidget_add_child(container, label);

        // Spacer
        let spacer = Rwidget_create(R_WIDGET_SPACER);
        Rslot_set_size(spacer, 0.0, 6.0);
        Rwidget_add_child(container, spacer);

        // Buttons in HBox
        let button_row = Rwidget_create(R_WIDGET_HBOX);
        Rpanel_set_spacing(button_row, 10.0);
        Rslot_set_halign(button_row, R_HALIGN_CENTER);

        if has_remote {
            let store_button = Rbutton_create(cstr("Abrir tienda de software").as_ptr());
            Rbutton_set_style(store_button, R_BUTTON_PRIMARY);
            Rbutton_set_clicked_callback(store_button, Some(open_store_and_close), std::ptr::null_mut());
            Rwidget_add_child(button_row, store_button);

            let github_button = Rbutton_create(cstr("Ver en GitHub").as_ptr());
            Rbutton_set_style(github_button, R_BUTTON_FLAT);
            Rbutton_set_clicked_callback(github_button, Some(open_github_and_close), std::ptr::null_mut());
            Rwidget_add_child(button_row, github_button);
        } else {
            let github_button = Rbutton_create(cstr("Descargar desde GitHub").as_ptr());
            Rbutton_set_style(github_button, R_BUTTON_PRIMARY);
            Rbutton_set_clicked_callback(github_button, Some(open_github_and_close), std::ptr::null_mut());
            Rwidget_add_child(button_row, github_button);

            let web_button = Rbutton_create(cstr("Web del proyecto (.flatpakref)").as_ptr());
            Rbutton_set_style(web_button, R_BUTTON_FLAT);
            Rbutton_set_clicked_callback(web_button, Some(open_website_and_close), std::ptr::null_mut());
            Rwidget_add_child(button_row, web_button);
        }

        let close_button = Rbutton_create(cstr("Cerrar").as_ptr());
        Rbutton_set_style(close_button, R_BUTTON_FLAT);
        Rbutton_set_clicked_callback(close_button, Some(manual_update_dialog_close), std::ptr::null_mut());
        Rwidget_add_child(button_row, close_button);

        Rwidget_add_child(container, button_row);

        Rdialog_open(window, container, None, std::ptr::null_mut());
    }
}

// Percent-decoder that handles multi-byte UTF-8 (e.g., %C3%A1 -> á)
fn decode_uri_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                result.push(byte);
                i += 3;
                continue;
            }
        }
        result.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&result).into_owned()
}

/// Guarda las preferencias y el estado de las listas justo antes de cerrar.
/// Así el volumen y la posición de la cola aleatoria sobreviven al reinicio.
unsafe extern "C" fn window_close_callback(_window: Rid, user_data: *mut c_void) -> c_int {
    let state = &*(user_data as *const RefCell<PlayerState>);
    if let Ok(mut state) = state.try_borrow_mut() {
        if let Err(error) = state.persist_settings() {
            eprintln!("No se pudieron guardar los ajustes al cerrar: {error}");
        }
        state.save_playlists();
    }
    R_TRUE
}

/// Índice de inserción al soltar sobre el `ListView`, a partir de la posición
/// vertical dentro del widget. La mitad superior de una fila inserta antes de
/// ella y la inferior, después; se limita a `[0, len]`.
fn drop_insert_index(local_y: f32, row_height: f32, len: usize) -> usize {
    if len == 0 || row_height <= 0.0 {
        return 0;
    }
    let row = ((local_y / row_height).round() as isize).clamp(0, len as isize);
    row as usize
}

/// Extrae las rutas de audio de un drop del gestor de archivos.
///
/// Se usa tanto en el callback de la ventana como en el del `ListView`. La
/// ruta se toma tal cual la entrega el gestor: canonicalizarla puede resolver
/// un symlink hacia un destino al que el sandbox no tiene acceso.
unsafe fn audio_paths_from_drop(event: *const RDropEvent) -> Vec<PathBuf> {
    let path_count = (*event).path_count;
    let paths_ptr = (*event).paths;
    if path_count <= 0 || paths_ptr.is_null() {
        return Vec::new();
    }
    let audio_extensions = ["mp3", "flac", "wav", "ogg", "opus", "m4a"];
    let mut tracks = Vec::new();
    for i in 0..path_count {
        let path_c = *paths_ptr.add(i as usize);
        if path_c.is_null() {
            continue;
        }
        let raw = String::from_utf8_lossy(CStr::from_ptr(path_c).to_bytes()).into_owned();
        let raw = raw.strip_prefix("file://").unwrap_or(&raw);
        let path = PathBuf::from(decode_uri_path(raw));
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if audio_extensions.iter().any(|&e| e == ext_lower) {
                tracks.push(path);
            }
        }
    }
    tracks
}

/// Drop sobre cualquier zona de la ventana que no sea la lista: se mantiene
/// el comportamiento de siempre, añadir al final.
unsafe extern "C" fn window_drop_callback(
    _target: Rid,
    event: *const RDropEvent,
    user_data: *mut c_void,
) -> c_int {
    if (*event).phase != R_DROP_DROPPED {
        return R_TRUE;
    }
    let tracks = audio_paths_from_drop(event);
    if !tracks.is_empty() {
        let state = &*(user_data as *const RefCell<PlayerState>);
        if let Ok(mut state) = state.try_borrow_mut() {
            state.add_tracks(tracks);
        }
    }
    R_TRUE
}

/// Drop directamente sobre la lista de pistas.
///
/// Raven da `local_y` relativo al `ListView`, así que podemos calcular la fila
/// sobre la que se suelta. Solo se usa si la lista no se ha desplazado: el
/// `ListView` no expone su offset y una estimación podría insertar en el sitio
/// equivocado. Con scroll (o en el buscador de YouTube) se cae al final.
unsafe extern "C" fn track_list_drop_callback(
    _target: Rid,
    event: *const RDropEvent,
    user_data: *mut c_void,
) -> c_int {
    let state = &*(user_data as *const RefCell<PlayerState>);
    let phase = (*event).phase;
    let local_y = (*event).local_y;
    let window = (*event).window;

    if matches!(phase, R_DROP_ENTER | R_DROP_MOVE) {
        if let Ok(mut st) = state.try_borrow_mut() {
            let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
            let len = st
                .playlists
                .get(st.active_playlist)
                .map_or(0, |playlist| playlist.tracks.len());
            st.external_drop_index = if st.youtube_mode || st.track_list_scrolled || len == 0 {
                None
            } else {
                Some(drop_insert_index(local_y, row_height, len))
            };
        }
        Rwindow_request_redraw(window);
        return R_TRUE;
    }
    if phase == R_DROP_LEAVE {
        if let Ok(mut st) = state.try_borrow_mut() {
            st.external_drop_index = None;
        }
        Rwindow_request_redraw(window);
        return R_TRUE;
    }
    if phase == R_DROP_DROPPED {
        let tracks = audio_paths_from_drop(event);
        if let Ok(mut st) = state.try_borrow_mut() {
            let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
            let len = st
                .playlists
                .get(st.active_playlist)
                .map_or(0, |playlist| playlist.tracks.len());
            let insert_at = if st.youtube_mode || st.track_list_scrolled || len == 0 {
                None
            } else {
                Some(drop_insert_index(local_y, row_height, len))
            };
            st.external_drop_index = None;
            if !tracks.is_empty() {
                st.add_tracks_at(tracks, insert_at);
            }
        }
        Rwindow_request_redraw(window);
    }
    R_TRUE
}

// --- Geometría de las pestañas de listas ---
//
// Raven no expone el rect de cada pestaña, así que reproducimos su reparto
// (`rvn_tab_natural_width` y `rvn_tab_width` en tab_view.c): ancho natural a
// partir del texto, margen del modo Documents, reserva del botón de cerrar y
// encogido proporcional cuando no caben. El índice de origen del arrastre se
// lee de `Rtab_view_get_active`; esto solo decide el destino y la guía.

/// Margen izquierdo de las pestañas en modo Documents.
const TAB_INSET: f32 = 4.0;
/// Ancho natural mínimo y máximo de una pestaña.
const TAB_MIN_WIDTH: f32 = 96.0;
const TAB_MAX_WIDTH: f32 = 280.0;
/// Ancho mínimo cuando las pestañas tienen que encogerse.
const TAB_MIN_SHRUNK: f32 = 48.0;
/// Botón de cerrar (18 px) y su separación del texto (4 px).
const TAB_CLOSE_WIDTH: f32 = 18.0;
const TAB_CLOSE_GAP: f32 = 4.0;
/// Aire del modo Documents: 4 px antes del texto y 8 px de holgura final.
const TAB_DOCUMENTS_EXTRA: f32 = 12.0;

/// Nuevo índice de un elemento al mover otro de `from` a `to`.
///
/// Sirve para remapear `active_playlist` y la lista de la pista actual cuando
/// se reordenan las listas, igual que hace `move_track` con las pistas.
fn remap_index(index: usize, from: usize, to: usize) -> usize {
    if index == from {
        to
    } else if from < to && index > from && index <= to {
        index - 1
    } else if to < from && index >= to && index < from {
        index + 1
    } else {
        index
    }
}

/// Medidas que Raven usa para calcular el ancho de una pestaña.
#[derive(Clone, Copy, Debug)]
struct TabMetrics {
    text_width: f32,
    closable: bool,
}

/// Ancho natural de una pestaña, antes de repartir el espacio disponible.
fn tab_natural_width(tab: TabMetrics, padding_x: f32) -> f32 {
    let mut width = tab.text_width + padding_x * 2.0 + TAB_DOCUMENTS_EXTRA;
    if tab.closable {
        width += TAB_CLOSE_WIDTH + TAB_CLOSE_GAP;
    }
    width.clamp(TAB_MIN_WIDTH, TAB_MAX_WIDTH)
}

/// Anchos de todas las pestañas para el ancho disponible del TabView.
///
/// Si el total no cabe, Raven encoge todas proporcionalmente hasta un mínimo
/// de 48 px. Las pestañas nunca se ocultan ni se desplazan: siempre caben.
fn tab_widths(tabs: &[TabMetrics], padding_x: f32, view_width: f32) -> Vec<f32> {
    let mut widths: Vec<f32> = tabs
        .iter()
        .map(|tab| tab_natural_width(*tab, padding_x))
        .collect();
    let total: f32 = widths.iter().sum();
    if view_width > 0.0 && total > view_width - 8.0 {
        let scale = (view_width - 8.0) / total;
        for width in &mut widths {
            *width = (*width * scale).max(TAB_MIN_SHRUNK);
        }
    }
    widths
}

/// Pestaña que hay bajo un punto local del TabView, si hay alguna.
fn tab_at(x: f32, y: f32, widths: &[f32], bar_height: f32) -> Option<usize> {
    if y < 0.0 || y >= bar_height || x < TAB_INSET {
        return None;
    }
    let mut left = TAB_INSET;
    for (index, width) in widths.iter().enumerate() {
        if x >= left && x < left + width {
            return Some(index);
        }
        left += width;
    }
    None
}

/// Rectángulo de una pestaña en modo Documents, relativo al TabView.
///
/// Raven empieza en `TAB_INSET`, deja 2 px a la derecha de cada pestaña y las
/// dibuja desde `y = 4` hasta el borde inferior de la barra. El resultado se
/// usa para pintar el resaltado de la sesión sobre la pestaña real.
fn tab_rect(index: usize, widths: &[f32], bar_height: f32) -> Option<(f32, f32, f32, f32)> {
    let width = *widths.get(index)?;
    if width <= 2.0 || bar_height <= 4.0 {
        return None;
    }
    let x = TAB_INSET + widths[..index].iter().sum::<f32>();
    Some((x, 4.0, width - 2.0, bar_height - 4.0))
}

/// ¿El punto cae sobre el botón de cerrar de una pestaña?
///
/// Raven solo pinta el botón en la pestaña activa o bajo el cursor, pero el
/// clic se ignora igual: pulsar ahí cierra la pestaña, no inicia un arrastre.
fn tab_close_hit(x: f32, y: f32, widths: &[f32], bar_height: f32) -> bool {
    let Some(index) = tab_at(x, y, widths, bar_height) else {
        return false;
    };
    let left = TAB_INSET + widths[..index].iter().sum::<f32>();
    let close_left = left + widths[index] - TAB_CLOSE_WIDTH - 6.0;
    let close_top = (bar_height - TAB_CLOSE_WIDTH) * 0.5 + 1.0;
    x >= close_left
        && x < close_left + TAB_CLOSE_WIDTH
        && y >= close_top
        && y < close_top + TAB_CLOSE_WIDTH
}

/// Posición final de la pestaña arrastrada.
///
/// Se cuentan los centros de las *demás* pestañas que quedan a la izquierda
/// del cursor: así la pestaña agarrada no se mueve sola al cruzar su propio
/// centro, solo cuando el cursor rebasa el centro de una vecina.
fn tab_drop_target(widths: &[f32], x: f32, from: usize) -> usize {
    let last = widths.len().saturating_sub(1);
    if widths.is_empty() {
        return 0;
    }
    let mut left = TAB_INSET;
    let mut target = 0;
    for (index, width) in widths.iter().enumerate() {
        if index != from && x >= left + width * 0.5 {
            // Cuenta cuántas vecinas quedan a la izquierda del cursor: ese es
            // el índice final en la lista sin la pestaña arrastrada.
            target += 1;
        }
        left += width;
    }
    target.min(last)
}

/// Hueco donde quedará la pestaña, relativo al TabView.
///
/// Al mover hacia la derecha la pestaña termina después de la de `target`;
/// hacia la izquierda, antes. Con `from == target` no se dibuja guía.
fn tab_guide_x(widths: &[f32], from: usize, target: usize) -> f32 {
    if target >= widths.len() {
        return TAB_INSET;
    }
    let mut left = TAB_INSET;
    for width in widths.iter().take(target) {
        left += width;
    }
    if target > from {
        left + widths[target]
    } else {
        left
    }
}

/// Índice de destino a partir del desplazamiento del ratón desde el clic.
///
/// Se usa siempre `start_y` como ancla. La rueda está congelada durante el
/// gesto; si el auto-scroll desplaza la vista, `start_y` se reancla para que
/// píxeles e índices del modelo sigan alineados.
fn drag_target(from: usize, start_y: f32, y: f32, row_height: f32, len: usize) -> usize {
    let last = len.saturating_sub(1);
    if len == 0 || row_height <= 0.0 {
        return from.min(last);
    }
    let delta_rows = ((y - start_y) / row_height).round() as isize;
    (from as isize + delta_rows).clamp(0, last as isize) as usize
}

/// Paso de auto-scroll cuando el cursor toca un borde de la lista.
///
/// Devuelve el nuevo ancla, el destino recalculado y el índice que debe hacer
/// visible `Rlist_view_scroll_to_item`. Al desplazarse una fila hacia abajo,
/// el contenido sube `row_height` píxeles y el ancla baja la misma cantidad;
/// hacia arriba ocurre al revés. Así la pista agarrada sigue bajo el cursor.
fn auto_scroll_step(
    from: usize,
    start_y: f32,
    local_y: f32,
    list_h: f32,
    row_height: f32,
    target: usize,
    len: usize,
    edge: f32,
) -> Option<(f32, usize, i32)> {
    if len == 0 || row_height <= 0.0 || list_h <= 0.0 {
        return None;
    }
    let (new_start_y, scroll_to) = if local_y >= list_h - edge && target + 1 < len {
        (start_y - row_height, target + 1)
    } else if local_y <= edge && target > 0 {
        (start_y + row_height, target - 1)
    } else {
        return None;
    };
    let new_target = drag_target(from, new_start_y, local_y, row_height, len);
    Some((new_start_y, new_target, scroll_to as i32))
}

/// Hueco entre filas donde quedará la pista, relativo al rect de la lista.
///
/// `move_track` quita la pista y la inserta en `target`: al arrastrar hacia
/// abajo el hueco es después de la fila `target`; hacia arriba, antes de ella.
/// El límite de la fila agarrada se estima media fila por encima del clic, así
/// que la guía salta de un hueco a otro en lugar de seguir al cursor.
fn drag_guide_y(from: usize, start_y: f32, target: usize, row_height: f32) -> f32 {
    let delta = target as isize - from as isize;
    if delta == 0 {
        return start_y;
    }
    start_y + (delta as f32 + delta.signum() as f32 * 0.5) * row_height
}

/// Línea horizontal en el `ListView`, con `offset_y` relativo al widget.
unsafe fn paint_track_list_guide(painter: *mut RPainter, list: Rid, offset_y: f32) {
    if list == R_INVALID_ID {
        return;
    }
    let (mut list_x, mut list_y, mut list_w, mut list_h) = (0.0, 0.0, 0.0, 0.0);
    Rwidget_get_rect(list, &mut list_x, &mut list_y, &mut list_w, &mut list_h);
    if list_w <= 0.0 || list_h <= 0.0 {
        return;
    }
    let line_y = list_y + offset_y.clamp(0.0, list_h);
    Rdraw_line(
        painter,
        list_x,
        line_y,
        list_x + list_w,
        line_y,
        2.0,
        0.725,
        0.639,
        0.961,
        0.9,
    );
}

/// Dibuja la guía del gesto de arrastre activo.
///
/// El widget de guía cubre toda la ventana, así que sus coordenadas locales son
/// las de la ventana. En las pistas se pinta una línea horizontal en el hueco
/// entre filas; en las pestañas, una vertical en el hueco de destino. Se usan
/// los rects de los widgets más la posición calculada durante el gesto.
unsafe extern "C" fn drop_guide_paint(
    _widget: Rid,
    painter: *mut RPainter,
    _width: f32,
    _height: f32,
    user_data: *mut c_void,
) {
    if user_data.is_null() {
        return;
    }
    let state = &*(user_data as *const RefCell<PlayerState>);
    let Ok(st) = state.try_borrow() else {
        return;
    };

    // La pestaña de la sesión se distingue con un fondo rojo suave, esté o no
    // activa. Se pinta primero para que las guías de arrastre queden encima.
    if let Some(index) = st
        .playlists
        .iter()
        .position(|playlist| playlist.is_temporal)
    {
        let tabs = st.playlist_tabs_widget;
        if tabs != R_INVALID_ID {
            let (mut tabs_x, mut tabs_y, mut tabs_w, mut tabs_h) = (0.0, 0.0, 0.0, 0.0);
            Rwidget_get_rect(
                tabs,
                &mut tabs_x,
                &mut tabs_y,
                &mut tabs_w,
                &mut tabs_h,
            );
            if tabs_w > 0.0 && tabs_h > 0.0 {
                let widths = playlist_tab_widths(&st, tabs);
                if let Some((x, y, width, height)) =
                    tab_rect(index, &widths, playlist_tab_bar_height())
                {
                    let radius =
                        6.0_f32.min(Rtheme_get_metric(R_METRIC_CORNER_RADIUS) + 2.0);
                    let (x, y) = (tabs_x + x, tabs_y + y);
                    // Un rojo apagado sobre la paleta morada: se nota sin
                    // tapar el texto ni parecer una alerta.
                    Rdraw_rect(painter, x, y, width, height, radius, 0.50, 0.24, 0.28, 0.20);
                    Rdraw_rect_outline(
                        painter, x, y, width, height, radius, 1.0, 0.82, 0.51, 0.56, 0.55,
                    );
                }
            }
        }
    }

    if let TabDragState::Dragging {
        from,
        target,
        guide_x,
        ..
    } = st.tab_drag
    {
        if from == target {
            // Sin movimiento no hay hueco que indicar.
            return;
        }
        let tabs = st.playlist_tabs_widget;
        if tabs == R_INVALID_ID {
            return;
        }
        let (mut tabs_x, mut tabs_y, mut tabs_w, mut tabs_h) = (0.0, 0.0, 0.0, 0.0);
        Rwidget_get_rect(
            tabs,
            &mut tabs_x,
            &mut tabs_y,
            &mut tabs_w,
            &mut tabs_h,
        );
        if tabs_w <= 0.0 || tabs_h <= 0.0 {
            return;
        }
        let line_x = (tabs_x + guide_x).clamp(tabs_x + TAB_INSET, tabs_x + tabs_w);
        Rdraw_line(
            painter,
            line_x,
            tabs_y + 5.0,
            line_x,
            tabs_y + tabs_h - 5.0,
            2.0,
            0.725,
            0.639,
            0.961,
            0.9,
        );
        return;
    }

    if let Some(index) = st.external_drop_index {
        let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
        if row_height > 0.0 {
            paint_track_list_guide(painter, st.track_list_widget, index as f32 * row_height);
        }
        return;
    }

    let DragState::Dragging {
        from,
        start_y,
        target,
        ..
    } = st.drag
    else {
        return;
    };
    if from == target {
        // Sin movimiento no hay hueco que indicar.
        return;
    }
    let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
    if row_height <= 0.0 {
        return;
    }
    let guide_y = drag_guide_y(from, start_y, target, row_height);
    paint_track_list_guide(painter, st.track_list_widget, guide_y);
}

/// Crea un overlay sobre la ventana con el widget de dibujo de la guía.
///
/// El contenido XML se reparénta una sola vez dentro del overlay, que sí
/// respeta `padding`, a diferencia de un Canvas. La guía se añade después para
/// pintar encima y no robar eventos.
fn create_drop_guide(window: Rid, state: &Rc<RefCell<PlayerState>>) -> Rid {
    unsafe {
        let content = Rwindow_get_content(window);
        if content == R_INVALID_ID {
            return R_INVALID_ID;
        }
        let overlay = Rwidget_create(R_WIDGET_OVERLAY);
        if overlay == R_INVALID_ID {
            return R_INVALID_ID;
        }
        let guide = Rwidget_create(R_WIDGET_CUSTOM);
        if guide == R_INVALID_ID {
            Rwidget_destroy(overlay);
            return R_INVALID_ID;
        }

        // El slot pertenece al nuevo padre: restauramos el `Slot.Padding="20"`
        // que el VBox raíz tenía en ui/main.xml. Un Canvas lo ignoraba y dejaba
        // la interfaz pegada a los bordes de la ventana.
        Rwidget_remove_child(window, content);
        Rwidget_add_child(overlay, content);
        Rslot_set_padding(content, 20.0, 20.0, 20.0, 20.0);
        Rwidget_add_child(overlay, guide);
        Rslot_set_padding(guide, 0.0, 0.0, 0.0, 0.0);
        // La guía no debe robar eventos: el clic y el arrastre siguen en la lista.
        Rwidget_set_hit_testable(guide, R_FALSE);
        Rcustom_set_paint_callback(
            guide,
            Some(drop_guide_paint),
            Rc::as_ptr(state) as *mut c_void,
        );
        // Siempre visible: pinta la guía de arrastre y el fondo de la sesión.
        Rwidget_set_visibility(guide, R_VISIBLE);
        Rwindow_set_content(window, overlay);
        guide
    }
}

/// Anchos actuales de las pestañas de listas, medidos con la fuente del tema.
///
/// `Rfont_measure_text` con `R_INVALID_ID` y tamaño 0 resuelve la misma fuente
/// y el mismo tamaño que usa Raven para dibujar el título de la pestaña.
unsafe fn playlist_tab_widths(state: &PlayerState, tabs: Rid) -> Vec<f32> {
    let (mut x, mut y, mut view_width, mut height) = (0.0, 0.0, 0.0, 0.0);
    Rwidget_get_rect(tabs, &mut x, &mut y, &mut view_width, &mut height);
    let padding_x = Rtheme_get_metric(R_METRIC_PADDING_X);
    let metrics: Vec<TabMetrics> = state
        .playlists
        .iter()
        .map(|playlist| {
            let title = cstr(&playlist.name);
            let (mut text_width, mut text_height) = (0.0, 0.0);
            Rfont_measure_text(
                R_INVALID_ID,
                0.0,
                title.as_ptr(),
                &mut text_width,
                &mut text_height,
            );
            TabMetrics {
                text_width,
                closable: true,
            }
        })
        .collect();
    tab_widths(&metrics, padding_x, view_width)
}

/// Altura de la barra de pestañas en modo Documents (Raven: ControlHeight + 10).
unsafe fn playlist_tab_bar_height() -> f32 {
    Rtheme_get_metric(R_METRIC_CONTROL_HEIGHT) + 10.0
}

// Event callback de las pestañas de listas: reordenarlas arrastrando.
//
// El clic se deja pasar a Raven para que active la pestaña; a partir del
// umbral se consume el movimiento y, al soltar, se reordena una sola vez.
unsafe extern "C" fn playlist_tabs_event_callback(
    widget: Rid,
    event: *const REvent,
    user_data: *mut c_void,
) -> c_int {
    let state = &*(user_data as *const RefCell<PlayerState>);
    let event_type = (*event).type_;
    let local_x = (*event).local_x;
    let local_y = (*event).local_y;
    const DRAG_SLOP: f32 = 10.0;

    match event_type {
        R_EVENT_MOUSE_DOWN => {
            if (*event).button != R_MOUSE_LEFT {
                return R_FALSE;
            }
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            st.tab_drag = TabDragState::Idle;
            let widths = playlist_tab_widths(&st, widget);
            let bar_height = playlist_tab_bar_height();
            let Some(index) = tab_at(local_x, local_y, &widths, bar_height) else {
                return R_FALSE;
            };
            // El botón de cerrar no es un asa de arrastre: Raven lo gestiona.
            if tab_close_hit(local_x, local_y, &widths, bar_height) {
                return R_FALSE;
            }
            st.tab_drag = TabDragState::Pressed { start_x: local_x };
            // Raven no dispara `playlist_changed` al pulsar la pestaña ya activa;
            // sin esto, ese clic no sacaría del modo YouTube. `set_active_playlist`
            // sale pronto cuando no hay cambio de lista ni buscador activo.
            st.set_active_playlist(index);
            // Sin consumir: Raven activa la pestaña pulsada y actualiza la lista activa.
            R_FALSE
        }
        R_EVENT_MOUSE_MOVE => {
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            let start_x = match st.tab_drag {
                TabDragState::Pressed { start_x } => start_x,
                TabDragState::Dragging { start_x, .. } => start_x,
                TabDragState::Idle => return R_FALSE,
            };
            if (local_x - start_x).abs() < DRAG_SLOP {
                return R_FALSE;
            }
            let widths = playlist_tab_widths(&st, widget);
            if widths.len() != st.playlists.len() {
                st.tab_drag = TabDragState::Idle;
                return R_FALSE;
            }
            let from = match st.tab_drag {
                TabDragState::Pressed { .. } => {
                    // Raven ya procesó el clic: este es el índice real pulsado.
                    let active = Rtab_view_get_active(widget);
                    if active < 0 || active as usize >= widths.len() {
                        st.tab_drag = TabDragState::Idle;
                        return R_FALSE;
                    }
                    active as usize
                }
                TabDragState::Dragging { from, .. } => from,
                TabDragState::Idle => return R_FALSE,
            };
            let target = tab_drop_target(&widths, local_x, from);
            let guide_x = tab_guide_x(&widths, from, target);
            st.tab_drag = TabDragState::Dragging {
                from,
                start_x,
                target,
                guide_x,
            };
            // Congela el hover de Raven durante el gesto.
            R_TRUE
        }
        R_EVENT_MOUSE_UP => {
            if (*event).button != R_MOUSE_LEFT {
                return R_FALSE;
            }
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            let TabDragState::Dragging { from, target, .. } = st.tab_drag else {
                st.tab_drag = TabDragState::Idle;
                return R_FALSE;
            };
            st.tab_drag = TabDragState::Idle;
            if from == target
                || from >= st.playlists.len()
                || target >= st.playlists.len()
            {
                return R_FALSE;
            }
            let page = Rtab_view_get_tab(widget, from as i32);
            if page == R_INVALID_ID {
                return R_FALSE;
            }
            // Mover el hijo dispara `children_changed`; después se reordena el
            // modelo. Raven reasigna la página activa a la primera al separar
            // el hijo, así que se reactiva la pestaña movida al final.
            if Rwidget_insert_child(widget, page, target as i32) == R_INVALID_ID {
                return R_FALSE;
            }
            st.move_playlist(from, target);
            Rtab_view_set_active(widget, target as i32);
            R_FALSE
        }
        R_EVENT_MOUSE_LEAVE => {
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            // Un arrastre iniciado sigue vivo aunque el cursor salga un momento.
            if matches!(st.tab_drag, TabDragState::Pressed { .. }) {
                st.tab_drag = TabDragState::Idle;
            }
            R_FALSE
        }
        R_EVENT_FOCUS_LOST => {
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            st.tab_drag = TabDragState::Idle;
            R_FALSE
        }
        _ => R_FALSE,
    }
}

// Event callback for track list: handle drag-to-reorder
unsafe extern "C" fn track_list_event_callback(
    widget: Rid,
    event: *const REvent,
    user_data: *mut c_void,
) -> c_int {
    let state = &*(user_data as *const RefCell<PlayerState>);
    let event_type = (*event).type_;
    let local_y = (*event).local_y;
    const DRAG_SLOP: f32 = 10.0;
    const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(90);

    match event_type {
        R_EVENT_MOUSE_DOWN => {
            if (*event).button != R_MOUSE_LEFT {
                return R_FALSE;
            }
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            // Los resultados no se pueden arrastrar: la fila no corresponde a
            // una pista de la lista activa.
            if st.youtube_mode {
                st.drag = DragState::Idle;
                return R_FALSE;
            }
            // Clic en la franja de la barra de desplazamiento: Raven la gestiona
            // internamente y no avisa del cambio de offset. Marcamos la lista
            // como desplazada para no insertar en una fila equivocada.
            let (mut list_x, mut list_y, mut list_w, mut list_h) = (0.0, 0.0, 0.0, 0.0);
            Rwidget_get_rect(widget, &mut list_x, &mut list_y, &mut list_w, &mut list_h);
            if list_w > 0.0 && (*event).local_x >= list_w - 12.0 {
                st.track_list_scrolled = true;
            }
            // Un clic nuevo cancela cualquier gesto anterior. Dejamos pasar el
            // evento (R_FALSE) para que Raven seleccione la fila pulsada: nuestro
            // callback se ejecuta ANTES del widget, así que leer la selección aquí
            // daría la fila anterior, no la que el usuario ha pulsado.
            st.drag = DragState::Pressed { y: local_y };
            R_FALSE
        }
        R_EVENT_MOUSE_WHEEL => {
            // Congelar el viewport mientras haya un clic o arrastre en curso.
            // Si la lista scrollea a mitad del gesto, `local_y` deja de corresponder
            // al índice capturado y el destino salta filas.
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            if matches!(st.drag, DragState::Idle) {
                if (*event).wheel_x != 0.0 || (*event).wheel_y != 0.0 {
                    st.track_list_scrolled = true;
                }
                R_FALSE
            } else {
                R_TRUE
            }
        }
        R_EVENT_MOUSE_MOVE => {
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            let start_y = match st.drag {
                DragState::Pressed { y } => y,
                DragState::Dragging { start_y, .. } => start_y,
                DragState::Idle => return R_FALSE,
            };
            if (local_y - start_y).abs() < DRAG_SLOP {
                return R_FALSE;
            }
            let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
            if row_height <= 0.0 {
                return R_FALSE;
            }
            let playlist_index = st.active_playlist;
            let len = st
                .playlists
                .get(playlist_index)
                .map_or(0, |playlist| playlist.tracks.len());
            if len == 0 {
                st.drag = DragState::Idle;
                return R_FALSE;
            }

            let from = match st.drag {
                DragState::Pressed { .. } => {
                    // Raven ya ha procesado MOUSE_DOWN: ahora la selección sí es
                    // la fila real sobre la que se pulsó.
                    let selected = Rlist_view_get_selected(st.track_list_widget);
                    if selected < 0 || selected as usize >= len {
                        st.drag = DragState::Idle;
                        return R_FALSE;
                    }
                    selected as usize
                }
                DragState::Dragging { from, .. } => from,
                DragState::Idle => unreachable!("ya descartado al leer start_y"),
            };
            let mut start_y = start_y;
            let mut target = drag_target(from, start_y, local_y, row_height, len);

            // Auto-scroll en los bordes: Raven no expone el offset del ListView,
            // así que pedimos una fila más con `scroll_to_item` y reanclamos
            // `start_y` para que el destino siga correspondiéndose con el cursor.
            let (mut list_x, mut list_y, mut list_w, mut list_h) = (0.0, 0.0, 0.0, 0.0);
            Rwidget_get_rect(
                st.track_list_widget,
                &mut list_x,
                &mut list_y,
                &mut list_w,
                &mut list_h,
            );
            let now = Instant::now();
            let mut last_scroll = match st.drag {
                DragState::Dragging { last_scroll, .. } => last_scroll,
                DragState::Pressed { .. } | DragState::Idle => None,
            };
            let mut scroll_to = None;
            if last_scroll.map_or(true, |instant| {
                now.duration_since(instant) >= AUTO_SCROLL_INTERVAL
            }) {
                let edge = row_height.max(24.0);
                if let Some((new_start_y, new_target, index)) = auto_scroll_step(
                    from, start_y, local_y, list_h, row_height, target, len, edge,
                ) {
                    start_y = new_start_y;
                    target = new_target;
                    scroll_to = Some(index);
                    last_scroll = Some(now);
                    // El auto-scroll también desplaza la vista; el drop externo
                    // posterior no podrá mapear `local_y` a una fila real.
                    st.track_list_scrolled = true;
                }
            }
            st.drag = DragState::Dragging {
                from,
                start_y,
                target,
                last_scroll,
            };

            // El resaltado de Raven acompaña a la fila de destino y la línea
            // dibujada salta al hueco entre filas. Solo escribimos la selección
            // de Raven; `playlist.selected` no se toca hasta soltar el ratón,
            // para evitar bucles de realimentación.
            let list = st.track_list_widget;
            drop(st);
            if let Some(index) = scroll_to {
                Rlist_view_scroll_to_item(list, index);
            }
            Rlist_view_set_selected(list, target as i32);
            R_TRUE
        }
        R_EVENT_MOUSE_UP => {
            if (*event).button != R_MOUSE_LEFT {
                return R_FALSE;
            }
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            match st.drag {
                DragState::Dragging { from, start_y, .. } => {
                    let row_height = Rtheme_get_metric(R_METRIC_ROW_HEIGHT);
                    let len = st
                        .playlists
                        .get(st.active_playlist)
                        .map_or(0, |playlist| playlist.tracks.len());
                    let target = drag_target(from, start_y, local_y, row_height, len);
                    st.drag = DragState::Idle;
                    if from != target {
                        st.move_track(from, target);
                    }
                    // Dejamos que Raven cierre el clic: consumir el MOUSE_UP
                    // podría dejar su estado de pulsación colgado.
                    R_FALSE
                }
                DragState::Pressed { .. } => {
                    // Clic sin arrastre: que Raven lo termine con normalidad.
                    st.drag = DragState::Idle;
                    R_FALSE
                }
                DragState::Idle => R_FALSE,
            }
        }
        R_EVENT_FOCUS_LOST => {
            // Evita dejar un arrastre colgado si la ventana pierde el foco.
            let Ok(mut st) = state.try_borrow_mut() else {
                return R_FALSE;
            };
            st.drag = DragState::Idle;
            R_FALSE
        }
        R_EVENT_KEY_DOWN => {
            if (*event).key == R_KEY_DELETE {
                let Ok(mut st) = state.try_borrow_mut() else {
                    return R_FALSE;
                };
                if st.youtube_mode {
                    return R_FALSE;
                }
                st.remove_selected_track();
                return R_TRUE;
            }
            R_FALSE
        }
        _ => R_FALSE,
    }
}

fn ui_xml_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FREEWAY_UI_FILE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let local = PathBuf::from("ui/main.xml");
    local.is_file().then_some(local)
}

fn theme_xml_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FREEWAY_THEME_FILE") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let local = PathBuf::from("ui/theme.xml");
    local.is_file().then_some(local)
}

/// Raven busca los glifos que le faltan en `$XDG_DATA_HOME/fonts`, pero
/// Flatpak monta las fuentes del host en `/run/host/fonts`. Enlazar ese
/// directorio desde la carpeta de fuentes de la app permite que Raven use las
/// fuentes ya instaladas en el sistema, sin incrustar ninguna en el bundle.
#[cfg(unix)]
fn expose_host_fonts_to_raven() {
    let host = Path::new("/run/host/fonts");
    if !host.is_dir() {
        return; // ejecución fuera de Flatpak: Raven ya ve /usr/share/fonts
    }
    let data_dir = std::env::var_os("XDG_DATA_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".local/share"))
        });
    let Some(data_dir) = data_dir else { return };
    if let Err(err) = link_host_fonts(&data_dir.join("fonts"), host) {
        eprintln!("No se pudo exponer /run/host/fonts a Raven: {err}");
    }
}

#[cfg(unix)]
fn link_host_fonts(fonts_dir: &Path, host: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::symlink;

    fs::create_dir_all(fonts_dir)?;
    let link = fonts_dir.join("host");
    if fs::symlink_metadata(&link).is_err() {
        symlink(host, &link)?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn expose_host_fonts_to_raven() {}

fn main() {
    expose_host_fonts_to_raven();

    // Los archivos se leen antes de crear la ventana: si ya hay una instancia
    // abierta, esta se limita a enviárselos y termina sin abrir otra.
    let audio_args: Vec<PathBuf> = audio_paths_from_args(std::env::args().skip(1))
        .into_iter()
        .filter(|path| path.is_file())
        .collect();
    let instance = match instance::start(&audio_args) {
        instance::Role::Primary(instance) => Some(instance),
        instance::Role::Forwarded => return,
        instance::Role::Unavailable(error) => {
            eprintln!("Instancia única desactivada: {error}");
            None
        }
    };

    unsafe {
        if Rinit(R_BACKEND_VULKAN) == 0 {
            eprintln!("No se pudo iniciar Raven con Vulkan");
            std::process::exit(1);
        }
        Rlook_set(R_LOOK_RAVEN_DARK);
        let theme_name = if let Some(path) = theme_xml_path() {
            let path = cstr(&path.to_string_lossy());
            Rtheme_load(path.as_ptr())
        } else {
            Rtheme_load_from_memory(EMBEDDED_THEME.as_ptr().cast(), EMBEDDED_THEME.len())
        };
        if theme_name.is_null() || Rtheme_use(theme_name) == R_FALSE {
            eprintln!("No se pudo cargar el tema oscuro de Freeway Player");
        }
    }

    let window = if let Some(path) = ui_xml_path() {
        let path = cstr(&path.to_string_lossy());
        unsafe { Rwindow_create_from_xml_file(path.as_ptr()) }
    } else {
        window_from_xml(EMBEDDED_UI)
    };
    if window == R_INVALID_ID {
        eprintln!("No se pudo cargar la interfaz de ui/main.xml");
        unsafe { Rshutdown() };
        std::process::exit(1);
    }

    let ui = Ui::from_window(window);
    let (symbols_2_font, symbols_font) = load_symbol_fallbacks();
    let app_icon_texture = load_app_icon(window, ui);
    let action_icons_texture = load_action_icons(ui);
    let track_list_rows = Box::new(RefCell::new(TrackListRows::default()));
    let state = Rc::new(RefCell::new(PlayerState::new()));
    unsafe { Rslider_set_value(ui.volume, state.borrow().volume) };
    {
        let (mpris, mpris_commands) = mpris::MprisHandle::start();
        let mut st = state.borrow_mut();
        st.track_list_widget = ui.track_list;
        st.playlist_tabs_widget = ui.playlist_tabs;
        st.mpris = mpris;
        st.mpris_commands = Some(mpris_commands);
    }
    if let Some(instance) = &instance {
        let (sender, receiver) = std::sync::mpsc::channel();
        match instance.spawn_receiver(sender) {
            Ok(()) => state.borrow_mut().instance_paths = Some(receiver),
            Err(error) => {
                state.borrow_mut().status =
                    format!("No se pudo escuchar otras aperturas: {error}");
            }
        }
    }
    create_drop_guide(window, &state);
    // Raven no puede ocultar items de un menú, así que se crean tres: el de
    // siempre para las listas locales, uno para el buscador y otro para FLAC.
    let context_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let youtube_context_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let flac_context_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let mut menus = ContextMenus::new(context_menu, youtube_context_menu, flac_context_menu);
    let app_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let edit_metadata_text = cstr("Editar metadatos");
    let edit_metadata_item = unsafe { Rmenu_add_item(context_menu, edit_metadata_text.as_ptr()) };
    let remove_from_list_text = cstr("Eliminar de la lista");
    let remove_from_list_item = unsafe { Rmenu_add_item(context_menu, remove_from_list_text.as_ptr()) };
    // Menú exclusivo de FLAC: mismas acciones y, como segunda entrada, el
    // conversor con la ruta de la pista ya puesta en el diálogo.
    let flac_metadata_text = cstr("Editar metadatos");
    let flac_metadata_item = unsafe { Rmenu_add_item(flac_context_menu, flac_metadata_text.as_ptr()) };
    let convert_flac_text = cstr("Convertir a OPUS…");
    let convert_flac_item = unsafe { Rmenu_add_item(flac_context_menu, convert_flac_text.as_ptr()) };
    let flac_remove_text = cstr("Eliminar de la lista");
    let flac_remove_item = unsafe { Rmenu_add_item(flac_context_menu, flac_remove_text.as_ptr()) };
    let download_opus_text = cstr("Descargar en Opus");
    let download_opus_item = unsafe {
        Rmenu_add_item(youtube_context_menu, download_opus_text.as_ptr())
    };
    let add_stream_text = cstr("Añadir a lista de reproducción (Streaming)");
    let add_stream_item = unsafe { Rmenu_add_item(youtube_context_menu, add_stream_text.as_ptr()) };

    // «Ajustes» va primero para que el comportamiento al abrir archivos esté
    // a mano: es lo que más se toca al usar la app como reproductor del sistema.
    let settings_text = cstr("Ajustes");
    let settings_item = if app_menu != R_INVALID_ID {
        unsafe { Rmenu_add_item(app_menu, settings_text.as_ptr()) }
    } else {
        R_INVALID_ID
    };
    // «Herramientas» queda justo debajo de «Ajustes» y agrupa el conversor.
    let tools_menu = if app_menu != R_INVALID_ID {
        unsafe { Rmenu_add_menu(app_menu, cstr("Herramientas").as_ptr()) }
    } else {
        R_INVALID_ID
    };
    let converter_text = cstr("Conversor FLAC a OPUS");
    let converter_item = if tools_menu != R_INVALID_ID {
        unsafe { Rmenu_add_item(tools_menu, converter_text.as_ptr()) }
    } else {
        R_INVALID_ID
    };
    if settings_item != R_INVALID_ID {
        unsafe { Rmenu_add_separator(app_menu) };
    }
    let check_updates_text = cstr("Buscar actualizaciones");
    let check_updates_item = if app_menu != R_INVALID_ID {
        unsafe { Rmenu_add_item(app_menu, check_updates_text.as_ptr()) }
    } else {
        R_INVALID_ID
    };
    let about_text = cstr("Acerca de");
    let about_item = if app_menu != R_INVALID_ID {
        unsafe { Rmenu_add_item(app_menu, about_text.as_ptr()) }
    } else {
        R_INVALID_ID
    };
    let context_data = Box::into_raw(Box::new(ContextMenuContext {
        state: Rc::clone(&state),
        ui,
    })) as *mut c_void;
    unsafe {
        Rswitcher_set_index(ui.right_panel, 0);
        Rswitcher_set_index(ui.cover_switcher, 0);
        Rimage_set_stretch(ui.cover_image, R_STRETCH_FIT);
        Rwidget_set_context_menu(ui.track_list, context_menu);
        Rmenu_item_set_clicked_callback(
            edit_metadata_item,
            Some(open_metadata_editor_from_context_menu),
            context_data,
        );
        Rmenu_item_set_clicked_callback(
            remove_from_list_item,
            Some(remove_from_list_from_context_menu),
            Rc::as_ptr(&state) as *mut c_void,
        );
        Rmenu_item_set_clicked_callback(
            flac_metadata_item,
            Some(open_metadata_editor_from_context_menu),
            context_data,
        );
        Rmenu_item_set_clicked_callback(
            convert_flac_item,
            Some(open_converter_from_context_menu),
            context_data,
        );
        Rmenu_item_set_clicked_callback(
            flac_remove_item,
            Some(remove_from_list_from_context_menu),
            Rc::as_ptr(&state) as *mut c_void,
        );
        Rmenu_item_set_clicked_callback(
            download_opus_item,
            Some(download_opus_from_context_menu),
            context_data,
        );
        Rmenu_item_set_clicked_callback(
            add_stream_item,
            Some(add_to_playlist_from_context_menu),
            context_data,
        );
        if settings_item != R_INVALID_ID {
            Rmenu_item_set_clicked_callback(
                settings_item,
                Some(open_settings_dialog),
                &state as *const Rc<RefCell<PlayerState>> as *mut c_void,
            );
        }
        if converter_item != R_INVALID_ID {
            Rmenu_item_set_clicked_callback(
                converter_item,
                Some(open_converter_from_app_menu),
                &state as *const Rc<RefCell<PlayerState>> as *mut c_void,
            );
        }
        if check_updates_item != R_INVALID_ID {
            Rmenu_item_set_clicked_callback(
                check_updates_item,
                Some(check_updates_from_app_menu),
                Rc::as_ptr(&state) as *mut c_void,
            );
        }
        if about_item != R_INVALID_ID {
            Rmenu_item_set_clicked_callback(
                about_item,
                Some(open_about_dialog),
                window as *mut c_void,
            );
        }
        Rtab_view_set_documents(ui.playlist_tabs, R_TRUE);
        Rtab_view_set_close_callback(
            ui.playlist_tabs,
            Some(close_playlist_tab),
            Rc::as_ptr(&state) as *mut c_void,
        );
        // Reordenar las pestañas de listas arrastrándolas en horizontal
        Rwidget_set_event_callback(
            ui.playlist_tabs,
            Some(playlist_tabs_event_callback),
            Rc::as_ptr(&state) as *mut c_void,
        );
        Rwindow_set_close_callback(
            window,
            Some(window_close_callback),
            Rc::as_ptr(&state) as *mut c_void,
        );
        // Allow dropping audio files from the file manager into the window
        Rwindow_set_drop_callback(
            window,
            Some(window_drop_callback),
            Rc::as_ptr(&state) as *mut c_void,
        );
        // Allow dragging tracks to reorder them in the list
        Rwidget_set_event_callback(
            ui.track_list,
            Some(track_list_event_callback),
            Rc::as_ptr(&state) as *mut c_void,
        );
        // Los drops sobre la lista se aceptan aquí para saber dónde suelta el
        // ratón; el callback de la ventana sigue cubriendo el resto.
        Rwidget_set_drop_callback(
            ui.track_list,
            Some(track_list_drop_callback),
            Rc::as_ptr(&state) as *mut c_void,
        );
    }
    let (tabs, active_playlist) = {
        let state = state.borrow();
        (
            state
                .playlists
                .iter()
                .map(|playlist| playlist.name.clone())
                .collect::<Vec<_>>(),
            state.active_playlist,
        )
    };
    for (index, playlist) in state.borrow().playlists.iter().enumerate() {
        add_playlist_tab(
            ui.playlist_tabs,
            &playlist.name,
            index == active_playlist,
            playlist.is_temporal,
        );
    }
    if !tabs.is_empty() {
        unsafe { Rtab_view_set_active(ui.playlist_tabs, active_playlist as i32) };
    }

    // Abrir los archivos del lanzador (por ejemplo, "Abrir con Freeway
    // Player"). Ya se validaron antes de intentar la instancia única.
    if !audio_args.is_empty() {
        state
            .borrow_mut()
            .open_in_session(audio_args.clone(), OpenBehavior::AddAndPlay);
    }

    {
        action_bind("player.open_app_menu", move |sender, _| {
            if app_menu == R_INVALID_ID {
                return;
            }
            let (mut x, mut y, mut width, mut height) = (0.0, 0.0, 0.0, 0.0);
            unsafe {
                Rwidget_get_rect(sender, &mut x, &mut y, &mut width, &mut height);
                Rmenu_open(app_menu, window, x, y + height);
            }
        });
    }

    {
        let state = Rc::clone(&state);
        action_bind("player.open", move |_, _| {
            let paths = open_audio_files(window);
            if !paths.is_empty() {
                state.borrow_mut().add_tracks(paths);
            }
        });
    }
    {
        let state = Rc::clone(&state);
        let tabs = ui.playlist_tabs;
        action_bind("player.add_playlist", move |_, _| {
            let (_, name) = state.borrow_mut().create_playlist();
            add_playlist_tab(tabs, &name, true, false);
        });
    }
    {
        let state = Rc::clone(&state);
        let tabs = ui.playlist_tabs;
        action_bind("player.import_playlist", move |_, _| {
            let Some(path) = open_playlist_import_file(window) else {
                return;
            };
            let mut state = state.borrow_mut();
            let previous_count = state.playlists.len();
            state.import_playlist(&path);
            if state.playlists.len() > previous_count {
                let new_index = state.playlists.len() - 1;
                let name = state.playlists[new_index].name.clone();
                add_playlist_tab(tabs, &name, true, false);
            }
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.export_playlist", move |_, _| {
            let Some(path) = open_playlist_export_file(window) else {
                return;
            };
            state.borrow_mut().export_playlist(&path);
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.playlist_changed", move |sender, _| {
            let index = unsafe { Rtab_view_get_active(sender) };
            if index >= 0 {
                state.borrow_mut().set_active_playlist(index as usize);
            }
        });
    }
    {
        let state = Rc::clone(&state);
        let tabs = ui.playlist_tabs;
        action_bind("player.rename_playlist", move |sender, _| {
            let name = input_value(sender);
            let renamed = state.borrow_mut().rename_active_playlist(name);
            if let Some((index, name)) = renamed {
                let page = unsafe { Rtab_view_get_tab(tabs, index as i32) };
                let name = cstr(&name);
                unsafe { Rtab_view_set_tab_title(tabs, page, name.as_ptr()) };
            }
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.remove_track", move |_, _| {
            state.borrow_mut().remove_selected_track();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.play_random", move |_, _| {
            state.borrow_mut().play_random();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.youtube_toggle", move |_, _| {
            state.borrow_mut().toggle_youtube_mode();
        });
    }
    {
        let state = Rc::clone(&state);
        let ui = ui;
        action_bind("player.youtube_search", move |sender, _| {
            let query = input_value(sender);
            state.borrow_mut().start_youtube_search(query);
            set_input(ui.youtube_search, "");
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.play_selected", move |sender, _| {
            let selected = unsafe { Rlist_view_get_selected(sender) };
            state
                .borrow_mut()
                .play_selected((selected >= 0).then_some(selected as usize));
        });
    }
    {
        let state = Rc::clone(&state);
        let track_list = ui.track_list;
        action_bind("player.play", move |_, _| {
            let selected = unsafe { Rlist_view_get_selected(track_list) };
            let mut state = state.borrow_mut();
            state.sync_youtube_selection(selected);
            state.play();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.pause", move |_, _| state.borrow_mut().pause());
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.stop", move |_, _| state.borrow_mut().stop());
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.next", move |_, _| state.borrow_mut().next());
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.previous", move |_, _| state.borrow_mut().previous());
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.shuffle", move |_, _| {
            state.borrow_mut().toggle_shuffle();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.repeat", move |_, _| {
            state.borrow_mut().toggle_repeat();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.seek", move |sender, _| {
            let seconds = unsafe { Rslider_get_value(sender) };
            let mut st = state.borrow_mut();
            st.seek(seconds);
            st.notify_mpris_seek();
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.volume", move |sender, _| {
            let volume = unsafe { Rslider_get_value(sender) };
            state.borrow_mut().set_volume(volume);
        });
    }
    {
        let state = Rc::clone(&state);
        let ui = ui;
        action_bind("player.save_metadata", move |_, _| {
            let location = state
                .borrow()
                .editor_loaded_for
                .or_else(|| state.borrow().selected_location());
            let Some(location) = location else {
                state.borrow_mut().status = "Selecciona una pista antes de guardar".to_owned();
                return;
            };
            let metadata = TrackMetadata {
                title: input_value(ui.meta_title),
                artist: input_value(ui.meta_artist),
                album: input_value(ui.meta_album),
                genre: input_value(ui.meta_genre),
                track: input_value(ui.meta_track),
                duration: None,
            };
            state.borrow_mut().save_metadata(window, location, metadata);
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.restore_file", move |_, _| {
            let location = state
                .borrow()
                .editor_loaded_for
                .or_else(|| state.borrow().selected_location());
            let Some(location) = location else {
                state.borrow_mut().status =
                    "Selecciona una pista antes de restaurar".to_owned();
                return;
            };
            state.borrow_mut().restore_repair(location);
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.confirm_repair", move |_, _| {
            let location = state
                .borrow()
                .editor_loaded_for
                .or_else(|| state.borrow().selected_location());
            let Some(location) = location else {
                state.borrow_mut().status =
                    "Selecciona una pista antes de confirmar".to_owned();
                return;
            };
            state.borrow_mut().confirm_repair(location);
        });
    }
    {
        let state = Rc::clone(&state);
        let ui = ui;
        action_bind("player.close_metadata", move |_, _| {
            let mut state = state.borrow_mut();
            state.pending_cover = None;
            state.editor_loaded_for = None;
            unsafe { Rswitcher_set_index(ui.right_panel, 0) };
        });
    }
    {
        let state = Rc::clone(&state);
        let ui = ui;
        action_bind("player.choose_cover", move |_, _| {
            let (location, start_dir) = {
                let state_ref = state.borrow();
                let Some(location) = state_ref.editor_loaded_for else {
                    drop(state_ref);
                    state.borrow_mut().status =
                        "Abre el editor desde una pista para cambiar la carátula".to_owned();
                    return;
                };
                let start_dir = cover_dialog_directory(
                    state_ref.last_cover_dir.as_deref(),
                    state_ref.path_at(location),
                );
                (location, start_dir)
            };

            let Some(path) = open_cover_file(window, start_dir.as_deref()) else {
                return;
            };
            let mut state = state.borrow_mut();
            state.remember_cover_directory(&path);
            state.pending_cover = Some((location, CoverChange::Replace(path)));
            state.cover_hint = cover_hint_for_location(&state, Some(location));
            set_label(ui.cover_status, &state.cover_hint);
        });
    }
    {
        let state = Rc::clone(&state);
        let ui = ui;
        action_bind("player.remove_cover", move |_, _| {
            let mut state = state.borrow_mut();
            let Some(location) = state.editor_loaded_for else {
                state.status = "Abre el editor desde una pista para cambiar la carátula".to_owned();
                return;
            };
            state.pending_cover = Some((location, CoverChange::Remove));
            state.cover_hint = cover_hint_for_location(&state, Some(location));
            set_label(ui.cover_status, &state.cover_hint);
        });
    }

    refresh_ui(
        &mut state.borrow_mut(),
        ui,
        track_list_rows.as_ref(),
        &mut menus,
        window,
    );
    unsafe {
        while Rwait_frame(0.2) != 0 {
            refresh_ui(
                &mut state.borrow_mut(),
                ui,
                track_list_rows.as_ref(),
                &mut menus,
                window,
            );
            if state.borrow().restart_requested {
                break;
            }
        }
        Rwindow_close_popups(window);
        menus.destroy(ui.track_list);
        if app_menu != R_INVALID_ID {
            Rwidget_destroy(app_menu);
        }
        drop(Box::from_raw(context_data as *mut ContextMenuContext));
        let cover_texture = state.borrow().cover_texture;
        Rwindow_destroy(window);
        if cover_texture != R_INVALID_ID {
            Rtexture_destroy(cover_texture);
        }
        if app_icon_texture != R_INVALID_ID {
            Rtexture_destroy(app_icon_texture);
        }
        if action_icons_texture != R_INVALID_ID {
            Rtexture_destroy(action_icons_texture);
        }
        destroy_symbol_fallbacks(symbols_2_font, symbols_font);
        Rshutdown();
    }
    // Cierra el socket para que la próxima apertura cree una instancia nueva.
    drop(instance);
}

#[cfg(test)]
mod tests {
    use super::{
        add_behavior_from_option_index, audio_paths_from_args, auto_scroll_step,
        behavior_from_option_index, converter, cover_dialog_directory, cstr, default_quality_index,
        displayed_location_for, document_portal_host_path, download_dir_text, drag_guide_y,
        drag_target, drop_insert_index, existing_directory, format_time, is_document_portal_path_in,
        next_track_index,
        play_starts_selected, playlist_summary_text, previous_track_index, random_track_index,
        remap_index, repair_audio_file, repair_backup_path, shuffle_progress, tab_at, tab_close_hit,
        tab_drop_target, tab_guide_x, tab_natural_width, tab_rect, tab_widths, track_list_row_text,
        track_row, youtube_playback_from_option_index, youtube_result_row, AddBehavior,
        MetadataWriteError, OpenBehavior, Playlist,
        PlaylistStore, SavedPlaylist, StreamingTrack, YoutubePlayback, SavedSettings,
        TabMetrics, TrackListRows, TrackMetadata, CONVERTER_QUALITY_LABELS, SETTINGS_SECTIONS,
        INITIAL_VOLUME, R_INVALID_ID, SESSION_PLAYLIST_NAME,
    };
    use rodio::{Decoder, Source};
    use std::cell::RefCell;
    use std::collections::HashSet;
    use std::ffi::c_void;
    use std::io::Cursor;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    #[test]
    fn repair_backups_separate_same_name_in_different_folders() {
        let first = repair_backup_path(Path::new("/music/a/song.mp3"));
        let second = repair_backup_path(Path::new("/music/b/song.mp3"));
        assert_ne!(first, second);
        assert_eq!(first, repair_backup_path(Path::new("/music/a/song.mp3")));
        assert!(first
            .file_name()
            .is_some_and(|name| name.to_string_lossy().ends_with("-song.mp3")));
    }

    #[test]
    fn converter_defaults_to_recommended_quality() {
        let index = default_quality_index();
        assert_eq!(
            CONVERTER_QUALITY_LABELS[index],
            "160 kbps (recomendada)"
        );
        assert_eq!(
            converter::quality_from_option(index, 0),
            Some(converter::Quality::Preset(160))
        );
    }

    #[test]
    fn cover_directory_uses_closest_existing_ancestor() {
        let base = std::env::temp_dir().join(format!(
            "freeway-cover-{}-{}",
            std::process::id(),
            "ancestor"
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("last")).unwrap();

        let missing = base.join("last").join("gone").join("deeper");
        assert_eq!(existing_directory(&missing), Some(base.join("last")));
        assert_eq!(
            existing_directory(&base.join("last")),
            Some(base.join("last"))
        );
        assert_eq!(existing_directory(Path::new("/")), None);

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cover_dialog_prefers_last_directory_then_track() {
        let base =
            std::env::temp_dir().join(format!("freeway-cover-{}-{}", std::process::id(), "dialog"));
        let _ = std::fs::remove_dir_all(&base);
        let last = base.join("caratulas");
        let music = base.join("musica");
        std::fs::create_dir_all(&last).unwrap();
        std::fs::create_dir_all(&music).unwrap();
        let track = music.join("cancion.mp3");

        assert_eq!(
            cover_dialog_directory(Some(&last), Some(&track)),
            Some(last.clone())
        );
        assert_eq!(
            cover_dialog_directory(None, Some(&track)),
            Some(music.clone())
        );
        // Una carpeta recordada que ya no existe sube a su ancestro existente.
        let gone = base.join("desmontada").join("sub");
        assert_eq!(
            cover_dialog_directory(Some(&gone), Some(&track)),
            Some(base.clone())
        );
        // Una ruta efímera del portal se ignora y se usa la de la pista.
        assert_eq!(
            cover_dialog_directory(Some(Path::new("/run/user/1000/doc/abc123")), Some(&track)),
            Some(music.clone())
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn document_portal_paths_are_recognized() {
        let runtime = Path::new("/run/user/1000");
        assert!(is_document_portal_path_in(
            Path::new("/run/user/1000/doc/abc123/foto.png"),
            Some(runtime)
        ));
        assert!(!is_document_portal_path_in(
            Path::new("/run/user/1000/Música/foto.png"),
            Some(runtime)
        ));
        // Sin XDG_RUNTIME_DIR también se reconoce el patrón del portal.
        assert!(is_document_portal_path_in(
            Path::new("/run/user/1000/doc/abc123/foto.png"),
            None
        ));
        assert!(!is_document_portal_path_in(
            Path::new("/run/user/1000/Música/foto.png"),
            None
        ));
    }

    #[test]
    fn reads_document_portal_host_path_attribute() {
        use std::os::unix::ffi::OsStrExt;

        let dir =
            std::env::temp_dir().join(format!("freeway-cover-{}-{}", std::process::id(), "xattr"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("foto.png");
        std::fs::write(&file, b"png").unwrap();
        assert_eq!(document_portal_host_path(&file), None);

        let host = dir.join("real").join("foto.png");
        let path = std::ffi::CString::new(file.as_os_str().as_bytes()).unwrap();
        let value = host.as_os_str().as_bytes();
        let stored = unsafe {
            libc::setxattr(
                path.as_ptr(),
                b"user.document-portal.host-path\0".as_ptr().cast(),
                value.as_ptr().cast(),
                value.len(),
                0,
            )
        };
        // Algunos sistemas de archivos no admiten atributos de usuario.
        if stored == 0 {
            assert_eq!(document_portal_host_path(&file), Some(host));
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_round_trip_keeps_last_cover_directory() {
        let settings = SavedSettings {
            last_cover_dir: Some(PathBuf::from("/home/persona/Imágenes/caratulas")),
            open_behavior: OpenBehavior::AddOnly,
            add_behavior: AddBehavior::AddOnly,
            youtube_playback: YoutubePlayback::FullDownload,
            download_dir: Some(PathBuf::from("/home/persona/Descargas")),
            volume: 0.37,
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: SavedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_cover_dir, settings.last_cover_dir);
        assert_eq!(back.open_behavior, OpenBehavior::AddOnly);
        assert_eq!(back.add_behavior, AddBehavior::AddOnly);
        assert_eq!(back.youtube_playback, YoutubePlayback::FullDownload);
        assert_eq!(back.download_dir, settings.download_dir);
        assert_eq!(back.volume, 0.37);

        // Un archivo antiguo o vacío no impide arrancar.
        let empty: SavedSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.last_cover_dir.is_none());
        assert!(empty.download_dir.is_none());
        assert_eq!(empty.youtube_playback, YoutubePlayback::Streaming);
    }

    #[test]
    fn missing_youtube_playback_defaults_to_streaming() {
        let old: SavedSettings =
            serde_json::from_str(r#"{"open_behavior":"add_only","add_behavior":"add_only"}"#)
                .unwrap();
        assert_eq!(old.open_behavior, OpenBehavior::AddOnly);
        assert_eq!(old.add_behavior, AddBehavior::AddOnly);
        assert_eq!(old.youtube_playback, YoutubePlayback::Streaming);

        let explicit: SavedSettings =
            serde_json::from_str(r#"{"youtube_playback":"full_download"}"#).unwrap();
        assert_eq!(explicit.youtube_playback, YoutubePlayback::FullDownload);
    }

    #[test]
    fn missing_download_dir_defaults_to_none() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert!(settings.download_dir.is_none());
        assert!(download_dir_text(None).starts_with("Carpeta:"));
        assert!(download_dir_text(Some(Path::new("/tmp/descargas")))
            .contains("/tmp/descargas"));
    }

    #[test]
    fn youtube_placeholder_row_does_not_read_a_playlist_track() {
        let rows = RefCell::new(TrackListRows {
            tracks: Vec::new(),
            streams: Vec::new(),
            text: Vec::new(),
            pending_repairs: HashSet::new(),
            placeholder: Some(cstr("Busca en YouTube…")),
            youtube: true,
            youtube_results: Vec::new(),
        });
        let pointer = unsafe {
            track_list_row_text(
                R_INVALID_ID,
                0,
                &rows as *const RefCell<TrackListRows> as *mut c_void,
            )
        };
        let value = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        assert_eq!(value, "Busca en YouTube…");
    }

    #[test]
    fn youtube_results_use_their_own_row_text() {
        let track = crate::youtube::YoutubeTrack {
            id: "abc123".to_owned(),
            title: "Canción de prueba".to_owned(),
            channel: "Canal de prueba".to_owned(),
            duration: Some(Duration::from_secs(215)),
            thumbnail: None,
            thumbnail_data: None,
            url: "https://www.youtube.com/watch?v=abc123".to_owned(),
            live: false,
        };
        let rows = RefCell::new(TrackListRows {
            tracks: vec![PathBuf::from("/una/pista/local.mp3")],
            streams: vec![None],
            text: vec![None],
            pending_repairs: HashSet::new(),
            placeholder: None,
            youtube: true,
            youtube_results: vec![track.clone()],
        });
        let pointer = unsafe {
            track_list_row_text(
                R_INVALID_ID,
                0,
                &rows as *const RefCell<TrackListRows> as *mut c_void,
            )
        };
        let value = unsafe { std::ffi::CStr::from_ptr(pointer) }
            .to_string_lossy()
            .into_owned();
        assert!(value.contains("Canción de prueba"));
        assert!(value.contains("Canal de prueba"));
        assert!(value.contains("03:35"));
        assert!(!value.contains("local.mp3"));
        assert_eq!(youtube_result_row(&track, 0), value);

        let live = crate::youtube::YoutubeTrack {
            live: true,
            duration: None,
            ..track
        };
        assert!(youtube_result_row(&live, 0).contains("EN DIRECTO"));
    }

    #[test]
    fn youtube_step_index_wraps_and_handles_empty_results() {
        assert_eq!(crate::PlayerState::youtube_step_index(3, 0, true), Some(1));
        assert_eq!(crate::PlayerState::youtube_step_index(3, 2, true), Some(0));
        assert_eq!(crate::PlayerState::youtube_step_index(3, 0, false), Some(2));
        assert_eq!(crate::PlayerState::youtube_step_index(0, 0, true), None);
    }

    #[test]
    fn missing_open_behavior_defaults_to_add_and_play() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert_eq!(settings.open_behavior, OpenBehavior::AddAndPlay);
        assert_eq!(OpenBehavior::default(), OpenBehavior::AddAndPlay);
    }

    #[test]
    fn missing_add_behavior_defaults_to_play_first() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert_eq!(settings.add_behavior, AddBehavior::PlayFirst);
        assert_eq!(AddBehavior::default(), AddBehavior::PlayFirst);
    }

    #[test]
    fn missing_volume_defaults_to_initial_volume() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert_eq!(settings.volume, INITIAL_VOLUME);
        assert_eq!(SavedSettings::default().volume, INITIAL_VOLUME);
    }

    #[test]
    fn old_playlists_without_streams_still_load() {
        let json = r#"{"playlists":[{"name":"Antigua","tracks":["a.flac","b.flac"],"selected":null,"shuffle":false,"repeat":false,"is_temporal":false,"order":[0,1],"order_position":null}],"active_playlist":0}"#;
        let store: PlaylistStore = serde_json::from_str(json).unwrap();
        let playlist = Playlist::from_saved(store.playlists.into_iter().next().unwrap());
        assert_eq!(playlist.tracks.len(), 2);
        assert_eq!(playlist.streams.len(), 2);
        assert!(playlist.streams.iter().all(Option::is_none));
    }

    #[test]
    fn streaming_tracks_survive_a_playlist_round_trip() {
        let stream = StreamingTrack {
            url: "https://www.youtube.com/watch?v=abc123".to_owned(),
            title: "Canción en streaming".to_owned(),
            artist: "Canal".to_owned(),
            thumbnail: None,
            duration_secs: Some(215),
        };
        let mut playlist = Playlist::new("Lista".to_owned());
        playlist.tracks.push(PathBuf::from(&stream.url));
        playlist.streams.push(Some(stream.clone()));
        let json = serde_json::to_string(&playlist.saved()).unwrap();
        let restored = Playlist::from_saved(serde_json::from_str(&json).unwrap());
        let restored_stream = restored.stream_at(0).expect("la pista en streaming se conserva");
        assert_eq!(restored_stream.url, stream.url);
        assert_eq!(restored_stream.title, stream.title);
        assert_eq!(restored_stream.metadata().title, "Canción en streaming");
    }

    #[test]
    fn saved_shuffle_order_survives_a_reload() {
        let saved = SavedPlaylist {
            name: "Mezclada".to_owned(),
            tracks: vec![
                PathBuf::from("a.flac"),
                PathBuf::from("b.flac"),
                PathBuf::from("c.flac"),
            ],
            streams: Vec::new(),
            selected: Some(2),
            shuffle: true,
            repeat: false,
            is_temporal: false,
            order: vec![2, 0, 1],
            order_position: Some(1),
        };
        let playlist = Playlist::from_saved(saved.clone());
        assert_eq!(playlist.order, vec![2, 0, 1]);
        assert_eq!(playlist.order_position, Some(1));
        assert_eq!(playlist.selected, Some(0));
        assert_eq!(playlist.saved().order, saved.order);
        assert_eq!(playlist.saved().order_position, saved.order_position);
    }

    fn shuffled_playlist() -> Playlist {
        Playlist::from_saved(SavedPlaylist {
            name: "Mezclada".to_owned(),
            tracks: vec![
                PathBuf::from("a.flac"),
                PathBuf::from("b.flac"),
                PathBuf::from("c.flac"),
            ],
            streams: Vec::new(),
            selected: Some(0),
            shuffle: true,
            repeat: false,
            is_temporal: false,
            order: vec![2, 0, 1],
            order_position: Some(1),
        })
    }

    fn simple_playlist(name: &str, shuffle: bool, is_temporal: bool) -> Playlist {
        Playlist::from_saved(SavedPlaylist {
            name: name.to_owned(),
            tracks: vec![PathBuf::from("a.flac")],
            streams: Vec::new(),
            selected: None,
            shuffle,
            repeat: false,
            is_temporal,
            order: vec![0],
            order_position: Some(0),
        })
    }

    #[test]
    fn shuffle_cycle_restarts_with_a_fresh_order() {
        let mut playlist = Playlist::from_saved(SavedPlaylist {
            name: "Mezclada".to_owned(),
            tracks: (0..5)
                .map(|index| PathBuf::from(format!("{index}.flac")))
                .collect(),
            streams: Vec::new(),
            selected: Some(4),
            shuffle: true,
            repeat: false,
            is_temporal: false,
            order: vec![0, 1, 2, 3, 4],
            order_position: Some(4),
        });
        for _ in 0..20 {
            let previous = playlist.order.clone();
            let current = *previous.last().unwrap();
            playlist.reshuffle_cycle_after(current);
            // Cada reinicio baraja de nuevo y deja la última pista al final.
            assert_ne!(playlist.order, previous);
            assert_eq!(playlist.order.last(), Some(&current));
            assert_eq!(playlist.order.len(), 5);
            let mut sorted = playlist.order.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, vec![0, 1, 2, 3, 4]);
            assert_eq!(playlist.order_position, Some(4));
        }
    }

    #[test]
    fn random_track_handles_empty_single_and_exclusion() {
        assert_eq!(random_track_index(0, None), None);
        assert_eq!(random_track_index(1, Some(0)), Some(0));
        let chosen = random_track_index(4, Some(2));
        assert!(chosen.is_some_and(|index| (0..4).contains(&index) && index != 2));
    }

    #[test]
    fn random_track_eventually_reaches_every_index() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            seen.insert(random_track_index(6, None).unwrap());
        }
        assert_eq!(seen, (0..6).collect());
    }

    #[test]
    fn shuffle_progress_only_shows_for_the_playing_playlist() {
        let playlist = shuffled_playlist();
        assert_eq!(
            shuffle_progress(&playlist, Some((0, 0)), 0).as_deref(),
            Some("2/3")
        );
        // Si suena otra lista, ésta no muestra su contador.
        assert_eq!(shuffle_progress(&playlist, Some((1, 0)), 0), None);
        assert_eq!(shuffle_progress(&playlist, None, 0), None);
        // Sin aleatorio el número no aporta información.
        let sin_aleatorio = simple_playlist("Normal", false, false);
        assert_eq!(shuffle_progress(&sin_aleatorio, Some((0, 0)), 0), None);
    }

    #[test]
    fn playlist_summary_appends_shuffle_position() {
        let playlist = shuffled_playlist();
        assert_eq!(
            playlist_summary_text(&playlist, Some((0, 0)), 0),
            "2/3 · 3 pistas · Mezclada"
        );
        assert_eq!(
            playlist_summary_text(&playlist, Some((1, 0)), 0),
            "3 pistas · Mezclada"
        );
        let temporal = simple_playlist("Sesión", false, true);
        assert_eq!(
            playlist_summary_text(&temporal, None, 0),
            "1 pistas · no se guarda; renómbrala para conservarla"
        );
    }

    #[test]
    fn invalid_saved_shuffle_order_is_rebuilt() {
        let saved = SavedPlaylist {
            name: "Mezclada".to_owned(),
            tracks: vec![PathBuf::from("a.flac"), PathBuf::from("b.flac")],
            streams: Vec::new(),
            selected: Some(1),
            shuffle: true,
            repeat: false,
            is_temporal: false,
            order: vec![1, 1],
            order_position: Some(0),
        };
        let playlist = Playlist::from_saved(saved);
        assert_eq!(playlist.order.len(), 2);
        assert_ne!(playlist.order[0], playlist.order[1]);
        assert_eq!(playlist.order_position, Some(0));
    }

    #[test]
    fn open_behavior_values_have_distinct_indices() {
        for (position, behavior) in OpenBehavior::ALL.iter().copied().enumerate() {
            assert_eq!(behavior.index(), position);
            assert!(!behavior.label().is_empty());
        }
    }

    #[test]
    fn settings_dialog_options_map_to_behaviors() {
        for (position, behavior) in OpenBehavior::ALL.iter().copied().enumerate() {
            assert_eq!(behavior_from_option_index(position), Some(behavior));
        }
        assert_eq!(behavior_from_option_index(OpenBehavior::ALL.len()), None);
    }

    #[test]
    fn add_behavior_values_have_distinct_indices() {
        for (position, behavior) in AddBehavior::ALL.iter().copied().enumerate() {
            assert_eq!(behavior.index(), position);
            assert!(!behavior.label().is_empty());
        }
    }

    #[test]
    fn settings_dialog_add_options_map_to_behaviors() {
        for (position, behavior) in AddBehavior::ALL.iter().copied().enumerate() {
            assert_eq!(add_behavior_from_option_index(position), Some(behavior));
        }
        assert_eq!(add_behavior_from_option_index(AddBehavior::ALL.len()), None);
    }

    #[test]
    fn youtube_playback_values_have_distinct_indices() {
        for (position, mode) in YoutubePlayback::ALL.iter().copied().enumerate() {
            assert_eq!(mode.index(), position);
            assert!(!mode.label().is_empty());
        }
    }

    #[test]
    fn settings_dialog_youtube_options_map_to_playback() {
        for (position, mode) in YoutubePlayback::ALL.iter().copied().enumerate() {
            assert_eq!(youtube_playback_from_option_index(position), Some(mode));
        }
        assert_eq!(
            youtube_playback_from_option_index(YoutubePlayback::ALL.len()),
            None
        );
    }

    #[test]
    fn settings_sections_match_the_dialog_pages() {
        assert_eq!(SETTINGS_SECTIONS.len(), 4);
        assert_eq!(SETTINGS_SECTIONS[0], "General");
        assert_eq!(SETTINGS_SECTIONS[1], "Listas");
        assert_eq!(SETTINGS_SECTIONS[2], "YouTube");
        assert_eq!(SETTINGS_SECTIONS[3], "Descargas");
    }

    #[test]
    fn appending_tracks_without_playback_keeps_the_queue_position() {
        let mut playlist = Playlist::new("Mezclada".to_owned());
        playlist.tracks = vec![
            PathBuf::from("a.flac"),
            PathBuf::from("b.flac"),
            PathBuf::from("c.flac"),
        ];
        playlist.shuffle = true;
        playlist.rebuild_order(Some(1));
        let previous_order = playlist.order.clone();
        let previous_position = playlist.order_position;

        playlist.tracks.push(PathBuf::from("d.flac"));
        playlist.append_new_to_order(3);

        assert_eq!(playlist.order_position, previous_position);
        assert_eq!(&playlist.order[..previous_order.len()], &previous_order[..]);
        assert_eq!(playlist.order.last(), Some(&3));
    }

    #[test]
    fn track_row_marks_pending_repairs() {
        let metadata = TrackMetadata::default();
        assert!(track_row(&metadata, 0, true, false).contains('⚠'));
        assert!(!track_row(&metadata, 0, false, false).contains('⚠'));
    }

    #[test]
    fn track_row_marks_streaming_tracks() {
        let metadata = TrackMetadata {
            title: "Canción en streaming".to_owned(),
            artist: "Canal".to_owned(),
            ..TrackMetadata::default()
        };
        let local = track_row(&metadata, 0, false, false);
        let stream = track_row(&metadata, 0, false, true);
        assert!(!local.contains('▶'));
        assert!(!local.contains("(YouTube)"));
        assert!(stream.contains('▶'));
        assert!(stream.contains("(YouTube)"));
        assert!(stream.contains("Canción en streaming"));
    }

    #[test]
    fn only_audio_io_errors_offer_repair() {
        assert!(MetadataWriteError::Read("lectura".to_owned()).can_repair());
        assert!(MetadataWriteError::Write("escritura".to_owned()).can_repair());
        assert!(!MetadataWriteError::Validation("validación".to_owned()).can_repair());
    }

    #[test]
    fn ffmpeg_repairs_a_corrupted_mp3() {
        use lofty::file::AudioFile;
        use std::process::Command;

        if Command::new("ffmpeg").arg("-version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("freeway-repair-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let original = dir.join("corrupt.mp3");
        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-y",
                "-f",
                "lavfi",
                "-i",
            ])
            .arg("sine=frequency=440:duration=0.2")
            .args(["-c:a", "libmp3lame", "-b:a", "64k"])
            .arg(&original)
            .status()
            .unwrap();
        assert!(status.success());

        // Un tamaño de etiqueta ID3 a cero reproduce el fallo de lofty al
        // guardar ("failed to write Mpeg file") sin romper la lectura.
        let mut bytes = std::fs::read(&original).unwrap();
        assert_eq!(&bytes[..3], b"ID3");
        bytes[6..10].copy_from_slice(&[0, 0, 0, 0]);
        std::fs::write(&original, &bytes).unwrap();

        let repaired = repair_audio_file(&original).expect("ffmpeg debe reparar el MP3");
        assert!(std::fs::metadata(&repaired).unwrap().len() > 0);
        let file =
            lofty::read_from_path(&repaired).expect("el archivo reparado debe leerse");
        file.save_to_path(&repaired, lofty::config::WriteOptions::default())
            .expect("el archivo reparado debe admitir escritura");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn host_fonts_link_is_created_once() {
        use super::link_host_fonts;

        let base = std::env::temp_dir().join(format!("freeway-fonts-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let fonts_dir = base.join("fonts");
        let host = base.join("host-fonts");
        std::fs::create_dir_all(&host).unwrap();

        link_host_fonts(&fonts_dir, &host).unwrap();
        link_host_fonts(&fonts_dir, &host).unwrap();

        let link = fonts_dir.join("host");
        assert_eq!(std::fs::read_link(&link).unwrap(), host);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn drag_target_moves_rows_and_clamps_to_list() {
        // Con el viewport congelado, el destino es la fila de partida más las
        // filas que se ha movido el ratón, limitadas a la lista.
        assert_eq!(drag_target(5, 100.0, 100.0, 40.0, 10), 5);
        assert_eq!(drag_target(5, 100.0, 185.0, 40.0, 10), 7);
        assert_eq!(drag_target(5, 100.0, 20.0, 40.0, 10), 3);
        assert_eq!(drag_target(5, 100.0, 1000.0, 40.0, 10), 9);
        assert_eq!(drag_target(5, 100.0, -1000.0, 40.0, 10), 0);
        assert_eq!(drag_target(5, 100.0, 185.0, 0.0, 10), 5);
    }

    #[test]
    fn drop_insert_index_uses_the_nearest_row_edge() {
        // La mitad superior de una fila inserta antes; la inferior, después.
        assert_eq!(drop_insert_index(0.0, 40.0, 5), 0);
        assert_eq!(drop_insert_index(19.0, 40.0, 5), 0);
        assert_eq!(drop_insert_index(20.0, 40.0, 5), 1);
        assert_eq!(drop_insert_index(60.0, 40.0, 5), 2);
        assert_eq!(drop_insert_index(1000.0, 40.0, 5), 5);
        assert_eq!(drop_insert_index(-100.0, 40.0, 5), 0);
        assert_eq!(drop_insert_index(100.0, 0.0, 5), 0);
        assert_eq!(drop_insert_index(100.0, 40.0, 0), 0);
    }

    #[test]
    fn insert_new_into_order_keeps_the_playing_track() {
        let mut playlist = Playlist::new("Prueba".to_owned());
        playlist.tracks = (0..6).map(|i| PathBuf::from(format!("pista-{i}.flac"))).collect();
        playlist.order = vec![2, 0, 3, 1];
        playlist.order_position = Some(1); // la pista 0 suena en ese momento

        playlist.insert_new_into_order(2, 2);

        assert_eq!(playlist.tracks.len(), 6);
        assert_eq!(playlist.order, vec![2, 3, 4, 0, 5, 1]);
        assert_eq!(playlist.order[playlist.order_position.unwrap()], 0);
    }

    #[test]
    fn insert_new_into_order_appends_when_nothing_moves() {
        let mut playlist = Playlist::new("Prueba".to_owned());
        playlist.tracks = (0..4).map(|i| PathBuf::from(format!("pista-{i}.flac"))).collect();
        playlist.order = vec![0, 1, 2];
        playlist.order_position = Some(2);

        playlist.insert_new_into_order(3, 1);

        assert_eq!(playlist.order, vec![0, 1, 2, 3]);
        assert_eq!(playlist.order_position, Some(2));
    }

    #[test]
    fn drag_guide_snaps_between_rows() {
        // Con clic en el centro de la fila 2 (start_y = 100, filas de 40 px),
        // bajar una fila deja la pista después de la fila 3: hueco en 160.
        assert_eq!(drag_guide_y(2, 100.0, 3, 40.0), 160.0);
        // Subir una fila la deja antes de la fila 1: hueco en 40.
        assert_eq!(drag_guide_y(2, 100.0, 1, 40.0), 40.0);
        // Dos filas hacia abajo: hueco en 200.
        assert_eq!(drag_guide_y(2, 100.0, 4, 40.0), 200.0);
        // Sin cambio de destino no se dibuja; el valor no importa.
        assert_eq!(drag_guide_y(2, 100.0, 2, 40.0), 100.0);
    }

    #[test]
    fn tab_widths_clamp_and_shrink_to_fit() {
        let tab = |text_width| TabMetrics {
            text_width,
            closable: true,
        };
        // 50 + 22 (padding) + 12 (Documents) + 22 (cerrar) = 106.
        assert_eq!(tab_natural_width(tab(50.0), 11.0), 106.0);
        assert_eq!(tab_natural_width(tab(0.0), 11.0), 96.0);
        assert_eq!(tab_natural_width(tab(400.0), 11.0), 280.0);

        // Si caben, conservan su ancho natural.
        let widths = tab_widths(&[tab(50.0), tab(50.0), tab(50.0)], 11.0, 1000.0);
        assert_eq!(widths, vec![106.0, 106.0, 106.0]);

        // En 200 px se reparten los 192 disponibles entre las tres.
        let widths = tab_widths(&[tab(50.0), tab(50.0), tab(50.0)], 11.0, 200.0);
        assert!((widths.iter().sum::<f32>() - 192.0).abs() < 0.01);
        assert!((widths[0] - 64.0).abs() < 0.01);

        // Por debajo del mínimo de Raven (48 px) no se encogen más.
        let widths = tab_widths(&[tab(50.0), tab(50.0), tab(50.0)], 11.0, 20.0);
        assert_eq!(widths, vec![48.0, 48.0, 48.0]);
    }

    #[test]
    fn session_tab_rect_matches_documents_geometry() {
        let widths = [100.0, 80.0];
        // Raven dibuja desde x = 4, con 2 px de aire a la derecha y desde y = 4.
        assert_eq!(tab_rect(0, &widths, 44.0), Some((4.0, 4.0, 98.0, 40.0)));
        assert_eq!(tab_rect(1, &widths, 44.0), Some((104.0, 4.0, 78.0, 40.0)));
        assert_eq!(tab_rect(2, &widths, 44.0), None);
        assert_eq!(tab_rect(0, &[2.0], 44.0), None);
        assert_eq!(tab_rect(0, &widths, 4.0), None);
    }

    #[test]
    fn renaming_the_session_makes_it_permanent() {
        let mut session = Playlist::new_temporal();
        assert!(session.is_temporal);
        assert_eq!(session.name, SESSION_PLAYLIST_NAME);

        // Conservar el nombre de sesión la deja como temporal.
        assert!(!session.rename(SESSION_PLAYLIST_NAME.to_owned()));
        assert!(session.is_temporal);

        // Cualquier otro nombre la convierte en una lista normal.
        assert!(session.rename("Favoritas".to_owned()));
        assert!(!session.is_temporal);
        assert_eq!(session.name, "Favoritas");

        // Una lista normal llamada "Sesión" no se vuelve temporal por el nombre.
        let mut regular = Playlist::new("Mi música".to_owned());
        assert!(!regular.is_temporal);
        assert!(!regular.rename(SESSION_PLAYLIST_NAME.to_owned()));
        assert!(!regular.is_temporal);
        assert_eq!(regular.name, SESSION_PLAYLIST_NAME);
    }

    #[test]
    fn tab_at_stays_inside_bar_and_inset() {
        let widths = [100.0, 100.0, 100.0];
        // Las pestañas empiezan en x = 4 y son contiguas.
        assert_eq!(tab_at(4.0, 10.0, &widths, 44.0), Some(0));
        assert_eq!(tab_at(103.9, 10.0, &widths, 44.0), Some(0));
        assert_eq!(tab_at(104.0, 10.0, &widths, 44.0), Some(1));
        assert_eq!(tab_at(303.9, 10.0, &widths, 44.0), Some(2));
        // Hueco a la izquierda, después de la última y fuera de la barra.
        assert_eq!(tab_at(3.9, 10.0, &widths, 44.0), None);
        assert_eq!(tab_at(304.0, 10.0, &widths, 44.0), None);
        assert_eq!(tab_at(50.0, 43.9, &widths, 44.0), Some(0));
        assert_eq!(tab_at(50.0, 44.0, &widths, 44.0), None);
        assert_eq!(tab_at(50.0, -1.0, &widths, 44.0), None);
    }

    #[test]
    fn tab_drop_target_crosses_neighbour_centers() {
        let widths = [100.0, 100.0, 100.0];
        // Centros en 54, 154 y 254. El centro de la pestaña agarrada no cuenta.
        assert_eq!(tab_drop_target(&widths, 154.0, 1), 1);
        assert_eq!(tab_drop_target(&widths, 155.0, 1), 1);
        assert_eq!(tab_drop_target(&widths, 255.0, 1), 2);
        assert_eq!(tab_drop_target(&widths, 10.0, 1), 0);
        // La primera pestaña solo avanza una posición al rebasar la segunda.
        assert_eq!(tab_drop_target(&widths, 155.0, 0), 1);
        assert_eq!(tab_drop_target(&widths, 200.0, 0), 1);
        assert_eq!(tab_drop_target(&widths, 255.0, 0), 2);
        assert_eq!(tab_drop_target(&widths, 400.0, 0), 2);
        // La última retrocede una posición al rebasar el centro de la segunda.
        assert_eq!(tab_drop_target(&widths, 155.0, 2), 2);
        assert_eq!(tab_drop_target(&widths, 100.0, 2), 1);
        assert_eq!(tab_drop_target(&[], 100.0, 0), 0);
        assert_eq!(tab_drop_target(&[100.0], 500.0, 0), 0);
    }

    #[test]
    fn tab_guide_sits_at_destination_edge() {
        let widths = [100.0, 100.0, 100.0];
        // Hacia la derecha, después de la pestaña de destino.
        assert_eq!(tab_guide_x(&widths, 1, 2), 304.0);
        assert_eq!(tab_guide_x(&widths, 0, 1), 204.0);
        // Hacia la izquierda, antes de la pestaña de destino.
        assert_eq!(tab_guide_x(&widths, 1, 0), 4.0);
        assert_eq!(tab_guide_x(&widths, 2, 0), 4.0);
    }

    #[test]
    fn tab_close_button_is_not_a_drag_handle() {
        let widths = [100.0, 100.0];
        // Con barra de 44 px, el botón de la primera pestaña va de x=80 a 98
        // y de y=14 a 32.
        assert!(tab_close_hit(80.0, 14.0, &widths, 44.0));
        assert!(tab_close_hit(97.9, 31.9, &widths, 44.0));
        assert!(tab_close_hit(190.0, 20.0, &widths, 44.0));
        assert!(!tab_close_hit(98.0, 14.0, &widths, 44.0));
        assert!(!tab_close_hit(79.9, 14.0, &widths, 44.0));
        assert!(!tab_close_hit(80.0, 13.9, &widths, 44.0));
        assert!(!tab_close_hit(80.0, 32.0, &widths, 44.0));
        assert!(!tab_close_hit(50.0, 20.0, &widths, 44.0));
        assert!(!tab_close_hit(80.0, 44.0, &widths, 44.0));
    }

    #[test]
    fn remap_index_follows_moved_item() {
        // 1 -> 4: la lista movida acaba en 4 y las intermedias suben uno.
        assert_eq!(remap_index(1, 1, 4), 4);
        assert_eq!(remap_index(3, 1, 4), 2);
        assert_eq!(remap_index(5, 1, 4), 5);
        // 4 -> 1: la movida acaba en 1 y las intermedias bajan uno.
        assert_eq!(remap_index(4, 4, 1), 1);
        assert_eq!(remap_index(2, 4, 1), 3);
        assert_eq!(remap_index(0, 4, 1), 0);
    }

    #[test]
    fn auto_scroll_steps_one_row_and_reanchors() {
        let (height, row_height, len) = (400.0, 40.0, 20);

        // Borde inferior: se pide la fila siguiente y el ancla baja una fila.
        let target = drag_target(2, 100.0, 390.0, row_height, len);
        let (start_y, new_target, scroll_to) =
            auto_scroll_step(2, 100.0, 390.0, height, row_height, target, len, row_height)
                .unwrap();
        assert_eq!(target, 9);
        assert_eq!(start_y, 60.0);
        assert_eq!(new_target, 10);
        assert_eq!(scroll_to, 10);

        // Borde superior: se pide la fila anterior y el ancla sube una fila.
        let target = drag_target(10, 100.0, 10.0, row_height, len);
        let (start_y, new_target, scroll_to) =
            auto_scroll_step(10, 100.0, 10.0, height, row_height, target, len, row_height)
                .unwrap();
        assert_eq!(target, 8);
        assert_eq!(start_y, 140.0);
        assert_eq!(new_target, 7);
        assert_eq!(scroll_to, 7);

        // En el centro no hay scroll, y tampoco en los extremos del modelo.
        assert!(auto_scroll_step(
            2, 100.0, 200.0, height, row_height, 5, len, row_height
        )
        .is_none());
        assert!(auto_scroll_step(
            2, 100.0, 390.0, height, row_height, 19, len, row_height
        )
        .is_none());
        assert!(auto_scroll_step(
            2, 100.0, 10.0, height, row_height, 0, len, row_height
        )
        .is_none());
    }

    #[test]
    fn formats_short_and_long_times() {
        assert_eq!(format_time(Duration::ZERO), "00:00");
        assert_eq!(format_time(Duration::from_secs(65)), "01:05");
        assert_eq!(format_time(Duration::from_secs(3_661)), "1:01:01");
    }

    #[test]
    fn moves_forward_and_wraps_playlist() {
        assert_eq!(next_track_index(None, 3), Some(0));
        assert_eq!(next_track_index(Some(0), 3), Some(1));
        assert_eq!(next_track_index(Some(2), 3), Some(0));
        assert_eq!(next_track_index(Some(0), 1), Some(0));
        assert_eq!(next_track_index(None, 0), None);
    }

    #[test]
    fn moves_backward_and_wraps_playlist() {
        assert_eq!(previous_track_index(None, 3), Some(0));
        assert_eq!(previous_track_index(Some(2), 3), Some(1));
        assert_eq!(previous_track_index(Some(0), 3), Some(2));
        assert_eq!(previous_track_index(Some(0), 1), Some(0));
        assert_eq!(previous_track_index(None, 0), None);
    }

    #[test]
    fn decodes_a_short_pcm_wav_without_an_audio_device() {
        let source = Decoder::try_from(Cursor::new(silent_wav(800)))
            .expect("el decodificador debe aceptar WAV PCM");

        assert_eq!(source.total_duration(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn rejects_non_audio_data() {
        assert!(Decoder::try_from(Cursor::new(b"esto no es audio".to_vec())).is_err());
    }

    #[test]
    fn accepts_plain_paths_and_uris_from_the_launcher() {
        assert_eq!(
            audio_paths_from_args([
                "/tmp/a.flac".to_owned(),
                "file:///tmp/Mi%20m%C3%BAsica/b.MP3".to_owned(),
                "--file-forwarding".to_owned(),
                "@@u".to_owned(),
                "@@".to_owned(),
            ]),
            vec![
                PathBuf::from("/tmp/a.flac"),
                PathBuf::from("/tmp/Mi música/b.MP3"),
            ]
        );
    }

    #[test]
    fn ignores_non_audio_and_non_file_arguments() {
        assert!(audio_paths_from_args([
            "/tmp/nota.txt".to_owned(),
            "%U".to_owned(),
            "https://example.com/a.mp3".to_owned(),
            "file://servidor/a.mp3".to_owned(),
            "--open".to_owned(),
        ])
        .is_empty());
    }

    #[test]
    fn removes_duplicate_audio_arguments() {
        assert_eq!(
            audio_paths_from_args([
                "/tmp/a.flac".to_owned(),
                "file:///tmp/a.flac".to_owned(),
                "/tmp/b.ogg".to_owned(),
            ]),
            vec![PathBuf::from("/tmp/a.flac"), PathBuf::from("/tmp/b.ogg")]
        );
    }

    #[test]
    fn stopped_state_shows_the_selected_track() {
        assert_eq!(
            displayed_location_for(true, Some((0, 4)), Some((0, 1))),
            Some((0, 1))
        );
        assert_eq!(displayed_location_for(true, Some((0, 4)), None), None);
    }

    #[test]
    fn playing_state_shows_the_loaded_track() {
        assert_eq!(
            displayed_location_for(false, Some((0, 4)), Some((0, 1))),
            Some((0, 4))
        );
        assert_eq!(displayed_location_for(false, None, Some((0, 1))), Some((0, 1)));
    }

    #[test]
    fn play_starts_selection_only_when_it_differs_from_the_loaded_track() {
        assert!(play_starts_selected(Some((0, 2)), Some((0, 5))));
        assert!(!play_starts_selected(Some((0, 2)), Some((0, 2))));
        assert!(!play_starts_selected(Some((0, 2)), None));
        assert!(!play_starts_selected(None, None));
    }

    fn silent_wav(sample_count: u32) -> Vec<u8> {
        const SAMPLE_RATE: u32 = 8_000;
        const CHANNELS: u16 = 1;
        const BITS_PER_SAMPLE: u16 = 16;
        let bytes_per_sample = u32::from(BITS_PER_SAMPLE / 8);
        let data_len = sample_count * u32::from(CHANNELS) * bytes_per_sample;
        let byte_rate = SAMPLE_RATE * u32::from(CHANNELS) * bytes_per_sample;
        let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);

        let mut wav = Vec::with_capacity((44 + data_len) as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36 + data_len).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&CHANNELS.to_le_bytes());
        wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
        wav.extend_from_slice(&byte_rate.to_le_bytes());
        wav.extend_from_slice(&block_align.to_le_bytes());
        wav.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_len.to_le_bytes());
        wav.resize(44 + data_len as usize, 0);
        wav
    }
}
