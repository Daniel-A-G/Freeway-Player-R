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
use std::ffi::{c_char, c_int, c_void, CString};
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
mod instance;
mod mpris;
mod opus;
mod updates;

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

#[derive(Clone, Serialize, Deserialize)]
struct SavedPlaylist {
    name: String,
    tracks: Vec<PathBuf>,
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

/// Preferencias que sobreviven al cierre del programa. Se guardan aparte de
/// las listas para que un fallo al escribir una no arrastre a las otras.
#[derive(Serialize, Deserialize)]
struct SavedSettings {
    #[serde(default)]
    last_cover_dir: Option<PathBuf>,
    #[serde(default)]
    open_behavior: OpenBehavior,
    #[serde(default = "default_volume")]
    volume: f32,
}

fn default_volume() -> f32 {
    INITIAL_VOLUME
}

impl Default for SavedSettings {
    fn default() -> Self {
        Self {
            last_cover_dir: None,
            open_behavior: OpenBehavior::default(),
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
    text: Vec<Option<CString>>,
    pending_repairs: HashSet<PathBuf>,
}

impl Playlist {
    fn new(name: String) -> Self {
        Self {
            name,
            tracks: Vec::new(),
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
        let mut playlist = Self {
            name: saved.name,
            tracks: saved.tracks,
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
            selected: self.selected,
            shuffle: self.shuffle,
            repeat: self.repeat,
            is_temporal: self.is_temporal,
            order: if self.shuffle { self.order.clone() } else { Vec::new() },
            order_position: if self.shuffle { self.order_position } else { None },
        }
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

    fn reshuffle_cycle_after(&mut self, current_track: usize) {
        let previous = std::mem::take(&mut self.order);
        self.order = reshuffled_cycle_order(self.tracks.len(), Some(current_track), &previous);
        self.order_position = self.order.len().checked_sub(1);
    }
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
    cover_rendered_for: Option<(usize, usize, u64)>,
    cover_texture: Rid,
    track_list_widget: Rid,
    playlist_tabs_widget: Rid,
    last_cover_dir: Option<PathBuf>,
    open_behavior: OpenBehavior,
    /// Rutas que envían las siguientes instancias de la app.
    instance_paths: Option<Receiver<Vec<PathBuf>>>,
    /// Publicador MPRIS y canal de órdenes entrantes (el hilo D-Bus no toca
    /// `PlayerState`; solo envía comandos aquí).
    mpris: Option<mpris::MprisHandle>,
    mpris_commands: Option<Receiver<mpris::MprisCommand>>,
    /// Carátula ya exportada: (lista, pista, revisión) -> URI del archivo.
    mpris_art: Option<((usize, usize, u64), String)>,
    pending_repairs: Vec<PendingRepair>,
    drag: DragState,
    tab_drag: TabDragState,
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
            instance_paths: None,
            mpris: None,
            mpris_commands: None,
            mpris_art: None,
            pending_repairs: load_pending_repairs(),
            drag: DragState::Idle,
            tab_drag: TabDragState::Idle,
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
        if index >= self.playlists.len() || index == self.active_playlist {
            return;
        }
        self.active_playlist = index;
        self.ui_rendered_playlist = None;
        self.editor_loaded_for = None;
        self.save_playlists();
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
        if self.playlists.is_empty() || paths.is_empty() {
            return;
        }
        let playlist_index = self.active_playlist;
        let playlist = &mut self.playlists[playlist_index];
        let first_added = playlist.tracks.len();
        let mut existing: HashSet<_> = playlist.tracks.iter().cloned().collect();
        for path in paths {
            if existing.insert(path.clone()) {
                playlist.tracks.push(path);
            }
        }
        if playlist.tracks.len() == first_added {
            self.status = "Esos archivos ya están en la lista".to_owned();
            return;
        }

        playlist.revision += 1;
        playlist.selected = Some(first_added);
        let order_anchor = self
            .current
            .filter(|(current_playlist, _)| *current_playlist == playlist_index)
            .map(|(_, current_track)| current_track)
            .or(Some(first_added));
        playlist.rebuild_order(order_anchor);
        self.editor_loaded_for = None;
        self.status = format!("{} pistas en la lista", playlist.tracks.len());
        self.save_playlists();

        if self.is_stopped() {
            if let Err(error) = self.play_track(playlist_index, first_added, true) {
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
                playlist.selected = Some(0);
                playlist.order.clear();
                playlist.order_position = None;
                playlist.revision += 1;
                first_new = Some(0);
            }
            OpenBehavior::AddAndPlay | OpenBehavior::AddOnly => {
                let playlist = &mut self.playlists[playlist_index];
                let previous_len = playlist.tracks.len();
                let mut existing: HashSet<_> = playlist.tracks.iter().cloned().collect();
                for path in &paths {
                    if existing.insert(path.clone()) {
                        playlist.tracks.push(path.clone());
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
            if let Some(output) = &self.output {
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
        if let Some(output) = &self.output {
            output.player.stop();
        }
        self.playing = false;
        self.stopped = true;
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
                match self.repair_track(&path, &metadata, cover_change) {
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

        let warning = write_track_metadata(path, metadata, cover_change)
            .err()
            .map(|error| {
                format!(
                    "El archivo se reparó, pero no se pudieron reaplicar los metadatos: {}",
                    error.message()
                )
            });

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

    fn set_open_behavior(&mut self, behavior: OpenBehavior) {
        self.open_behavior = behavior;
        self.status = format!("Ajustes: {}", behavior.label());
        if let Err(error) = self.persist_settings() {
            self.status = format!("No se pudo guardar el ajuste: {error}");
        }
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
    let temporary = parent.join(format!(".{stem}.freeway-repair.{extension}"));
    let _ = fs::remove_file(&temporary);

    let output = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-i")
        .arg(path)
        .arg("-c")
        .arg("copy")
        .arg("-map_metadata")
        .arg("0")
        .arg(&temporary)
        .output()
        .map_err(|error| {
            format!("No se pudo ejecutar ffmpeg: {error}. Comprueba que está instalado.")
        })?;
    if !output.status.success() {
        let _ = fs::remove_file(&temporary);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        return Err(if detail.is_empty() {
            "ffmpeg no pudo reparar el archivo".to_owned()
        } else {
            format!("ffmpeg no pudo reparar el archivo: {detail}")
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
    let location = state.displayed_location();
    let marker = location.and_then(|(playlist, track)| {
        state
            .playlists
            .get(playlist)
            .map(|entry| (playlist, track, entry.revision))
    });
    if marker == state.cover_rendered_for {
        return;
    }

    let image_data = marker
        .and_then(|(playlist, track, _)| state.playlists.get(playlist)?.tracks.get(track))
        .and_then(|path| read_cover_art(path));
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
    track_list: Rid,
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
            track_list: widget_find(window, "track-list"),
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
            (ui.shuffle_icon, 0.0, 1.0 / 7.0),
            (ui.repeat_icon, 1.0 / 7.0, 2.0 / 7.0),
            (ui.import_icon, 2.0 / 7.0, 3.0 / 7.0),
            (ui.export_icon, 3.0 / 7.0, 4.0 / 7.0),
            (ui.restore_icon, 4.0 / 7.0, 5.0 / 7.0),
            (ui.confirm_icon, 5.0 / 7.0, 6.0 / 7.0),
            (ui.random_icon, 6.0 / 7.0, 1.0),
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

fn track_row(metadata: &TrackMetadata, index: usize, pending_repair: bool) -> String {
    let duration = metadata
        .duration
        .map(format_time)
        .unwrap_or_else(|| "--:--".to_owned());
    let artist = if metadata.artist.is_empty() {
        "Artista desconocido"
    } else {
        metadata.artist.as_str()
    };
    let marker = if pending_repair { "⚠" } else { " " };
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
    let index = index as usize;
    if index >= rows.tracks.len() {
        return EMPTY.as_ptr().cast();
    }

    if rows.text[index].is_none() {
        let metadata = read_track_metadata(&rows.tracks[index]);
        let pending_repair = rows.pending_repairs.contains(&rows.tracks[index]);
        rows.text[index] = Some(cstr(&track_row(&metadata, index, pending_repair)));
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
    let Some(playlist) = state.playlists.get(state.active_playlist) else {
        return;
    };
    let marker = (state.active_playlist, playlist.revision);
    if state.ui_rendered_playlist == Some(marker) {
        return;
    }

    let name = playlist.name.clone();
    let tracks = playlist.tracks.clone();
    let selected = playlist.selected;
    let track_count = tracks.len();
    {
        let mut rows = row_cache.borrow_mut();
        rows.tracks = tracks;
        rows.text.clear();
        rows.text.resize_with(track_count, || None);
        rows.pending_repairs = pending_repairs;
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
    let path = location
        .and_then(|(playlist, track)| state.playlists.get(playlist)?.tracks.get(track))
        .cloned();
    let metadata = path.as_deref().map(read_track_metadata);
    if let Some(metadata) = metadata {
        set_input(ui.meta_title, &metadata.title);
        set_input(ui.meta_artist, &metadata.artist);
        set_input(ui.meta_album, &metadata.album);
        set_input(ui.meta_genre, &metadata.genre);
        set_input(ui.meta_track, &metadata.track);
        state.metadata_hint = if path
            .as_deref()
            .is_some_and(|path| state.is_pending_repair(path))
        {
            PENDING_REPAIR_HINT.to_owned()
        } else {
            "Los cambios se escriben en el archivo de audio".to_owned()
        };
        state.cover_hint = cover_hint_for_location(state, location);
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
    window: Rid,
) {
    state.poll_update_check(window);
    state.poll_instance_messages(window);
    state.update_after_frame();
    state.poll_mpris_commands();
    update_track_list(state, ui, row_cache);
    if let Some(playlist) = state.playlists.get(state.active_playlist) {
        let summary = playlist_summary_text(playlist, state.current, state.active_playlist);
        set_label(ui.playlist_summary, &summary);
    }
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

struct MetadataMenuContext {
    state: Rc<RefCell<PlayerState>>,
    ui: Ui,
}

unsafe extern "C" fn open_metadata_editor_from_context_menu(_item: Rid, user_data: *mut c_void) {
    let context = &*(user_data as *const MetadataMenuContext);
    let selected = Rlist_view_get_selected(context.ui.track_list);
    if selected < 0 {
        return;
    }

    let mut state = context.state.borrow_mut();
    let playlist_index = state.active_playlist;
    let track_index = selected as usize;
    let Some(playlist) = state.playlists.get_mut(playlist_index) else {
        return;
    };
    if track_index >= playlist.tracks.len() {
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

unsafe extern "C" fn check_updates_from_app_menu(_item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const RefCell<PlayerState>);
    if let Ok(mut state) = state.try_borrow_mut() {
        state.start_update_check();
    }
}

/// Datos que necesita el diálogo de ajustes mientras está abierto. Raven
/// borra el contenido al cerrarlo, así que las opciones se leen en la
/// devolución de llamada de cierre.
struct SettingsDialogContext {
    state: Rc<RefCell<PlayerState>>,
    options: [Rid; 3],
}

unsafe extern "C" fn open_settings_dialog(item: Rid, user_data: *mut c_void) {
    let state = &*(user_data as *const Rc<RefCell<PlayerState>>);
    let window = Rwidget_get_window(item);
    let current = state.borrow().open_behavior;

    let content = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(content, 12.0);
    Rslot_set_padding(content, 24.0, 20.0, 24.0, 20.0);

    let title = Rlabel_create(cstr("Ajustes").as_ptr());
    Rlabel_set_text_size(title, 19.0);
    Rwidget_add_child(content, title);

    let category = Rlabel_create(cstr("Comportamiento al abrir archivos").as_ptr());
    Rlabel_set_text_size(category, 14.0);
    Rlabel_set_color(category, 0.725, 0.639, 0.961, 1.0);
    Rwidget_add_child(content, category);

    let description = Rlabel_create(
        cstr("Elige qué ocurre cuando abres un archivo con Freeway Player ya en marcha.").as_ptr(),
    );
    Rlabel_set_text_size(description, 12.0);
    Rlabel_set_wrap(description, R_TRUE);
    Rwidget_add_child(content, description);

    // Las tres casillas radio son hermanas: Raven desmarca las demás del
    // mismo padre al marcar una.
    let options_panel = Rwidget_create(R_WIDGET_VBOX);
    Rpanel_set_spacing(options_panel, 6.0);
    let mut options = [R_INVALID_ID; 3];
    for behavior in OpenBehavior::ALL {
        let option = Rcheckbox_create(cstr(behavior.label()).as_ptr());
        Rcheckbox_set_style(option, R_CHECKBOX_RADIO);
        Rcheckbox_set_checked(option, if behavior == current { R_TRUE } else { R_FALSE });
        Rwidget_add_child(options_panel, option);
        options[behavior.index()] = option;
    }
    Rwidget_add_child(content, options_panel);

    let separator = Rwidget_create(R_WIDGET_SEPARATOR);
    Rwidget_add_child(content, separator);

    let buttons = Rwidget_create(R_WIDGET_HBOX);
    Rpanel_set_spacing(buttons, 8.0);
    Rslot_set_halign(buttons, R_HALIGN_RIGHT);
    let accept = Rbutton_create(cstr("Aceptar").as_ptr());
    Rbutton_set_style(accept, R_BUTTON_PRIMARY);
    Rbutton_set_clicked_callback(accept, Some(settings_dialog_accept), std::ptr::null_mut());
    let cancel = Rbutton_create(cstr("Cancelar").as_ptr());
    Rbutton_set_style(cancel, R_BUTTON_FLAT);
    Rbutton_set_clicked_callback(cancel, Some(settings_dialog_cancel), std::ptr::null_mut());
    Rwidget_add_child(buttons, accept);
    Rwidget_add_child(buttons, cancel);
    Rwidget_add_child(content, buttons);

    let context = Box::into_raw(Box::new(SettingsDialogContext {
        state: Rc::clone(state),
        options,
    })) as *mut c_void;
    Rdialog_open(window, content, Some(settings_dialog_closed), context);
}

unsafe extern "C" fn settings_dialog_accept(button: Rid, _user_data: *mut c_void) {
    Rdialog_close(button, R_MESSAGE_RESULT_OK);
}

unsafe extern "C" fn settings_dialog_cancel(button: Rid, _user_data: *mut c_void) {
    Rdialog_close(button, R_MESSAGE_RESULT_CANCEL);
}

unsafe extern "C" fn settings_dialog_closed(_content: Rid, result: c_int, user_data: *mut c_void) {
    let context = Box::from_raw(user_data as *mut SettingsDialogContext);
    if result != R_MESSAGE_RESULT_OK {
        return;
    }
    let selected = context
        .options
        .iter()
        .position(|option| Rcheckbox_is_checked(*option) != R_FALSE)
        .and_then(behavior_from_option_index);
    let Some(behavior) = selected else {
        return;
    };
    let state = Rc::clone(&context.state);
    if let Ok(mut state) = state.try_borrow_mut() {
        state.set_open_behavior(behavior);
    };
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

// Drop callback: receive files dragged from the file manager into the window
unsafe extern "C" fn window_drop_callback(
    _target: Rid,
    event: *const RDropEvent,
    user_data: *mut c_void,
) -> c_int {
    if (*event).phase != R_DROP_DROPPED {
        return R_TRUE;
    }
    let state = &*(user_data as *const RefCell<PlayerState>);
    let path_count = (*event).path_count;
    let paths_ptr = (*event).paths;
    if path_count <= 0 || paths_ptr.is_null() {
        return R_TRUE;
    }

    let audio_extensions = ["mp3", "flac", "wav", "ogg", "opus", "m4a"];
    let mut tracks = Vec::new();
    for i in 0..path_count {
        let path_c = *paths_ptr.add(i as usize);
        if path_c.is_null() {
            continue;
        }
        // Print raw bytes for debugging
        let raw_bytes = std::ffi::CStr::from_ptr(path_c).to_bytes();
        eprintln!("[DROP DEBUG] raw path bytes: {:?}", raw_bytes);
        let raw = String::from_utf8_lossy(raw_bytes).into_owned();
        eprintln!("[DROP DEBUG] raw string: {:?}", raw);
        // Strip file:// URI scheme if present (file managers may drop URIs)
        let raw = raw.strip_prefix("file://").unwrap_or(&raw);
        // Decode percent-encoded characters (e.g., %20 -> space, %C3%A1 -> á)
        let decoded = decode_uri_path(raw);
        eprintln!("[DROP DEBUG] decoded: {:?}", decoded);
        let path = PathBuf::from(&decoded);
        eprintln!("[DROP DEBUG] path: {:?}", path);
        // Use the path as provided by the file manager; do not canonicalize
        // because symlinks may point outside the sandbox or to inaccessible locations.
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            let ext_lower = ext.to_lowercase();
            if audio_extensions.iter().any(|&e| e == ext_lower) {
                tracks.push(path);
            }
        }
    }
    if !tracks.is_empty() {
        if let Ok(mut state) = state.try_borrow_mut() {
            state.add_tracks(tracks);
        }
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
    let list = st.track_list_widget;
    if list == R_INVALID_ID {
        return;
    }

    let (mut list_x, mut list_y, mut list_w, mut list_h) = (0.0, 0.0, 0.0, 0.0);
    Rwidget_get_rect(list, &mut list_x, &mut list_y, &mut list_w, &mut list_h);
    if list_w <= 0.0 || list_h <= 0.0 {
        return;
    }
    let guide_y = drag_guide_y(from, start_y, target, row_height);
    let line_y = list_y + guide_y.clamp(0.0, list_h);
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
            if tab_at(local_x, local_y, &widths, bar_height).is_none() {
                return R_FALSE;
            }
            // El botón de cerrar no es un asa de arrastre: Raven lo gestiona.
            if tab_close_hit(local_x, local_y, &widths, bar_height) {
                return R_FALSE;
            }
            st.tab_drag = TabDragState::Pressed { start_x: local_x };
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
    _widget: Rid,
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
            let Ok(st) = state.try_borrow() else {
                return R_FALSE;
            };
            if matches!(st.drag, DragState::Idle) {
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
    let context_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let app_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let edit_metadata_text = cstr("Editar metadatos");
    let edit_metadata_item = unsafe { Rmenu_add_item(context_menu, edit_metadata_text.as_ptr()) };
    let remove_from_list_text = cstr("Eliminar de la lista");
    let remove_from_list_item = unsafe { Rmenu_add_item(context_menu, remove_from_list_text.as_ptr()) };

    // «Ajustes» va primero para que el comportamiento al abrir archivos esté
    // a mano: es lo que más se toca al usar la app como reproductor del sistema.
    let settings_text = cstr("Ajustes");
    let settings_item = if app_menu != R_INVALID_ID {
        unsafe { Rmenu_add_item(app_menu, settings_text.as_ptr()) }
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
    let context_data = Box::into_raw(Box::new(MetadataMenuContext {
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
        if settings_item != R_INVALID_ID {
            Rmenu_item_set_clicked_callback(
                settings_item,
                Some(open_settings_dialog),
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
        action_bind("player.play_selected", move |sender, _| {
            let selected = unsafe { Rlist_view_get_selected(sender) };
            state
                .borrow_mut()
                .play_selected((selected >= 0).then_some(selected as usize));
        });
    }
    {
        let state = Rc::clone(&state);
        action_bind("player.play", move |_, _| state.borrow_mut().play());
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
        window,
    );
    unsafe {
        while Rwait_frame(0.2) != 0 {
            refresh_ui(
                &mut state.borrow_mut(),
                ui,
                track_list_rows.as_ref(),
                window,
            );
            if state.borrow().restart_requested {
                break;
            }
        }
        Rwindow_close_popups(window);
        Rwidget_set_context_menu(ui.track_list, R_INVALID_ID);
        Rwidget_destroy(context_menu);
        if app_menu != R_INVALID_ID {
            Rwidget_destroy(app_menu);
        }
        drop(Box::from_raw(context_data as *mut MetadataMenuContext));
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
        audio_paths_from_args, auto_scroll_step, cover_dialog_directory, displayed_location_for,
        document_portal_host_path, drag_guide_y, drag_target, existing_directory, format_time,
        is_document_portal_path_in, next_track_index, play_starts_selected, previous_track_index,
        random_track_index,
        remap_index, repair_audio_file, repair_backup_path, tab_at, tab_close_hit, tab_drop_target,
        tab_guide_x, tab_natural_width, tab_rect, tab_widths, track_row, behavior_from_option_index,
        playlist_summary_text, shuffle_progress, MetadataWriteError, OpenBehavior, Playlist,
        SavedPlaylist, SavedSettings, TabMetrics, TrackMetadata, INITIAL_VOLUME,
        SESSION_PLAYLIST_NAME,
    };
    use rodio::{Decoder, Source};
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
            volume: 0.37,
        };
        let json = serde_json::to_string(&settings).unwrap();
        let back: SavedSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_cover_dir, settings.last_cover_dir);
        assert_eq!(back.open_behavior, OpenBehavior::AddOnly);
        assert_eq!(back.volume, 0.37);

        // Un archivo antiguo o vacío no impide arrancar.
        let empty: SavedSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.last_cover_dir.is_none());
    }

    #[test]
    fn missing_open_behavior_defaults_to_add_and_play() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert_eq!(settings.open_behavior, OpenBehavior::AddAndPlay);
        assert_eq!(OpenBehavior::default(), OpenBehavior::AddAndPlay);
    }

    #[test]
    fn missing_volume_defaults_to_initial_volume() {
        let settings: SavedSettings = serde_json::from_str(r#"{"last_cover_dir":null}"#).unwrap();
        assert_eq!(settings.volume, INITIAL_VOLUME);
        assert_eq!(SavedSettings::default().volume, INITIAL_VOLUME);
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
    fn track_row_marks_pending_repairs() {
        let metadata = TrackMetadata::default();
        assert!(track_row(&metadata, 0, true).contains('⚠'));
        assert!(!track_row(&metadata, 0, false).contains('⚠'));
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
        let mut file =
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
