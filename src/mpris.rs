//! Servidor MPRIS2 de Freeway Player.
//!
//! Este módulo no conoce `PlayerState`: publica un snapshot inmutable y envía
//! comandos por canal. El hilo de UI sigue siendo el único que toca el
//! reproductor real, igual que ocurre con los action_bind de Raven.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};
use zbus::{fdo, interface, object_server::SignalEmitter, Array, ObjectPath, OwnedVariant, Str};

/// La política por defecto de Flatpak permite a la app poseer
/// `org.mpris.MediaPlayer2.$FLATPAK_ID`, así que este nombre no necesita
/// permisos adicionales en el manifiesto.
pub const BUS_NAME: &str = "org.mpris.MediaPlayer2.io.github.freewayplayer.FreewayPlayer";
const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
/// Track ID que exige MPRIS2 cuando no hay ninguna pista cargada.
pub const NO_TRACK_ID: &str = "/org/mpris/MediaPlayer2/Track/NoTrack";

/// Órdenes que el servicio D-Bus envía al hilo de UI.
#[derive(Clone, Debug, PartialEq)]
pub enum MprisCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    /// Desplazamiento relativo en microsegundos.
    Seek(i64),
    /// Salto absoluto, solo si el ID de pista sigue siendo el actual.
    SetPosition { track_id: String, position: i64 },
    SetVolume(f64),
    SetShuffle(bool),
    SetLoopStatus(String),
}

/// Datos de la pista que se convierten en el diccionario `a{sv}` de MPRIS.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MprisTrack {
    pub id: String,
    pub title: String,
    pub artist: Vec<String>,
    pub album: String,
    pub genre: Vec<String>,
    pub track_number: Option<i32>,
    pub url: String,
    pub length_micros: Option<i64>,
    /// URI de un archivo de imagen ya extraído de la pista.
    pub art_uri: String,
}

/// Estado que leen las propiedades del objeto D-Bus.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MprisSnapshot {
    pub playback_status: String,
    pub metadata: HashMap<String, OwnedVariant>,
    pub position_micros: i64,
    pub volume: f64,
    pub shuffle: bool,
    pub loop_status: String,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
    pub can_control: bool,
    /// La UI lo marca cuando se produce un salto; el hilo D-Bus lo consume y
    /// emite la señal `Seeked`.
    pub seeked_position: Option<i64>,
}

/// Identificador estable de una pista dentro del object path MPRIS.
pub fn track_id(playlist_index: usize, track_index: usize) -> String {
    format!("{OBJECT_PATH}/Track/{playlist_index}_{track_index}")
}

/// Construye el diccionario `Metadata` con los tipos que exige MPRIS2.
pub fn metadata_for(track: &MprisTrack) -> HashMap<String, OwnedVariant> {
    let mut metadata = HashMap::new();

    if let Ok(id) = ObjectPath::try_from(track.id.as_str()) {
        metadata.insert("mpris:trackid".to_owned(), OwnedVariant::from(id));
    }
    if !track.title.is_empty() {
        metadata.insert(
            "xesam:title".to_owned(),
            OwnedVariant::from(Str::from(track.title.as_str())),
        );
    }
    if !track.artist.is_empty() {
        let artists: Vec<_> = track
            .artist
            .iter()
            .map(|artist| Str::from(artist.as_str()))
            .collect();
        if let Ok(value) = OwnedVariant::try_from(Array::from(artists)) {
            metadata.insert("xesam:artist".to_owned(), value);
        }
    }
    if !track.album.is_empty() {
        metadata.insert(
            "xesam:album".to_owned(),
            OwnedVariant::from(Str::from(track.album.as_str())),
        );
    }
    if !track.genre.is_empty() {
        let genres: Vec<_> = track
            .genre
            .iter()
            .map(|genre| Str::from(genre.as_str()))
            .collect();
        if let Ok(value) = OwnedVariant::try_from(Array::from(genres)) {
            metadata.insert("xesam:genre".to_owned(), value);
        }
    }
    if let Some(number) = track.track_number {
        metadata.insert("xesam:trackNumber".to_owned(), OwnedVariant::from(number));
    }
    if !track.url.is_empty() {
        metadata.insert(
            "xesam:url".to_owned(),
            OwnedVariant::from(Str::from(track.url.as_str())),
        );
    }
    if let Some(length) = track.length_micros.filter(|length| *length > 0) {
        metadata.insert("mpris:length".to_owned(), OwnedVariant::from(length));
    }
    if !track.art_uri.is_empty() {
        metadata.insert(
            "mpris:artUrl".to_owned(),
            OwnedVariant::from(Str::from(track.art_uri.as_str())),
        );
    }

    metadata
}

/// Extensión de una carátula incrustada según sus bytes mágicos. Los clientes
/// MPRIS abren la URI como un archivo, así que conviene usar una extensión real.
pub fn art_extension(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("gif")
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("webp")
    } else if data.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// Escribe la carátula en la caché y devuelve la ruta del archivo. Se limpian
/// las versiones con otra extensión para no dejar copias obsoletas.
pub fn write_art_cache(dir: &Path, data: &[u8]) -> Option<PathBuf> {
    let extension = art_extension(data)?;
    fs::create_dir_all(dir).ok()?;
    let target = dir.join(format!("cover.{extension}"));
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_cover = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("cover."));
            if is_cover && path != target {
                let _ = fs::remove_file(path);
            }
        }
    }
    let temporary = dir.join("cover.tmp");
    fs::write(&temporary, data).ok()?;
    fs::rename(&temporary, &target).ok()?;
    Some(target)
}

/// Convierte una duración a microsegundos, el tipo que usa MPRIS2.
pub fn to_micros(duration: Duration) -> i64 {
    duration.as_micros().min(i64::MAX as u128) as i64
}

/// Posición resultante de un `Seek(offset)` sin salir del rango de `i64`.
pub fn seek_target(current_micros: i64, offset: i64) -> i64 {
    (current_micros as i128 + offset as i128).clamp(0, i64::MAX as i128) as i64
}

/// Traduce `LoopStatus` a la única opción que tiene la app: repetir lista o no.
pub fn loop_status_to_repeat(status: &str) -> Option<bool> {
    match status {
        "None" => Some(false),
        "Track" | "Playlist" => Some(true),
        _ => None,
    }
}

/// Parte del servicio que vive en el hilo de UI: publica estado y marca saltos.
#[derive(Clone)]
pub struct MprisHandle {
    snapshot: Arc<Mutex<MprisSnapshot>>,
}

impl MprisHandle {
    /// Arranca el hilo del servicio. Devuelve también el receptor de comandos
    /// para que `refresh_ui` los drene como hace con los demás canales.
    pub fn start() -> (Option<Self>, Receiver<MprisCommand>) {
        let (commands, receiver) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(MprisSnapshot::default()));
        let worker_snapshot = Arc::clone(&snapshot);

        let thread = thread::Builder::new()
            .name("freeway-mpris".to_owned())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        eprintln!("MPRIS: no se pudo crear el runtime: {error}");
                        return;
                    }
                };
                if let Err(error) = runtime.block_on(serve(worker_snapshot, commands)) {
                    eprintln!("MPRIS: servicio no disponible: {error}");
                }
            });

        match thread {
            Ok(_) => (Some(Self { snapshot }), receiver),
            Err(error) => {
                eprintln!("MPRIS: no se pudo lanzar el hilo: {error}");
                let (_, closed) = mpsc::channel();
                (None, closed)
            }
        }
    }

    /// Sustituye el snapshot manteniendo un `Seeked` pendiente que aún no haya
    /// emitido el hilo D-Bus.
    pub fn publish(&self, snapshot: MprisSnapshot) {
        let Ok(mut current) = self.snapshot.lock() else {
            return;
        };
        let mut snapshot = snapshot;
        if current.seeked_position.is_some() {
            snapshot.seeked_position = current.seeked_position;
        }
        *current = snapshot;
    }

    /// Pide al hilo D-Bus que emita `Seeked` para esta posición.
    pub fn mark_seeked(&self, position_micros: i64) {
        let Ok(mut snapshot) = self.snapshot.lock() else {
            return;
        };
        snapshot.seeked_position = Some(position_micros);
    }
}

struct MprisRoot;

#[interface(name = "org.mpris.MediaPlayer2")]
impl MprisRoot {
    #[zbus(property)]
    fn identity(&self) -> &'static str {
        "Freeway Player"
    }

    #[zbus(property)]
    fn desktop_entry(&self) -> &'static str {
        "io.github.freewayplayer.FreewayPlayer"
    }

    #[zbus(property)]
    fn can_quit(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_raise(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn can_set_fullscreen(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }

    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<&'static str> {
        vec!["file"]
    }

    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<&'static str> {
        Vec::new()
    }

    fn raise(&self) -> Result<(), fdo::Error> {
        Err(fdo::Error::NotSupported(
            "Freeway Player no puede levantar la ventana desde MPRIS".to_owned(),
        ))
    }

    fn quit(&self) -> Result<(), fdo::Error> {
        Err(fdo::Error::NotSupported(
            "Freeway Player no puede cerrarse desde MPRIS".to_owned(),
        ))
    }
}

struct MprisPlayer {
    snapshot: Arc<Mutex<MprisSnapshot>>,
    commands: Sender<MprisCommand>,
}

impl MprisPlayer {
    fn snapshot(&self) -> MprisSnapshot {
        match self.snapshot.lock() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn send(&self, command: MprisCommand) -> Result<(), fdo::Error> {
        self.commands.send(command).map_err(|_| {
            fdo::Error::Failed("Freeway Player ya no está escuchando MPRIS".to_owned())
        })
    }

    /// Actualiza el snapshot antes de `Properties.Set` emita el cambio: así el
    /// valor anunciado es el nuevo y no el último que publicó la UI.
    fn update_snapshot(&self, update: impl FnOnce(&mut MprisSnapshot)) {
        let mut snapshot = match self.snapshot.lock() {
            Ok(snapshot) => snapshot,
            Err(poisoned) => poisoned.into_inner(),
        };
        update(&mut snapshot);
    }
}

#[interface(name = "org.mpris.MediaPlayer2.Player")]
impl MprisPlayer {
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.snapshot().playback_status
    }

    #[zbus(property)]
    fn loop_status(&self) -> String {
        self.snapshot().loop_status
    }

    #[zbus(property)]
    fn set_loop_status(&self, status: String) -> Result<(), fdo::Error> {
        self.update_snapshot(|snapshot| snapshot.loop_status = status.clone());
        self.send(MprisCommand::SetLoopStatus(status))
    }

    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.0
    }

    #[zbus(property)]
    fn shuffle(&self) -> bool {
        self.snapshot().shuffle
    }

    #[zbus(property)]
    fn set_shuffle(&self, shuffle: bool) -> Result<(), fdo::Error> {
        self.update_snapshot(|snapshot| snapshot.shuffle = shuffle);
        self.send(MprisCommand::SetShuffle(shuffle))
    }

    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedVariant> {
        self.snapshot().metadata
    }

    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.snapshot().position_micros
    }

    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.snapshot().volume
    }

    #[zbus(property)]
    fn set_volume(&self, volume: f64) -> Result<(), fdo::Error> {
        if !volume.is_finite() {
            return Err(fdo::Error::InvalidArgs("Volumen no válido".to_owned()));
        }
        let volume = volume.clamp(0.0, 1.0);
        self.update_snapshot(|snapshot| snapshot.volume = volume);
        self.send(MprisCommand::SetVolume(volume))
    }

    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.snapshot().can_go_next
    }

    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.snapshot().can_go_previous
    }

    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.snapshot().can_play
    }

    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.snapshot().can_pause
    }

    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.snapshot().can_seek
    }

    #[zbus(property)]
    fn can_control(&self) -> bool {
        self.snapshot().can_control
    }

    fn play(&self) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Play)
    }

    fn pause(&self) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Pause)
    }

    fn stop(&self) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Stop)
    }

    fn next(&self) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Next)
    }

    fn previous(&self) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Previous)
    }

    fn seek(&self, offset: i64) -> Result<(), fdo::Error> {
        self.send(MprisCommand::Seek(offset))
    }

    fn set_position(&self, track_id: ObjectPath<'_>, position: i64) -> Result<(), fdo::Error> {
        self.send(MprisCommand::SetPosition {
            track_id: track_id.as_str().to_owned(),
            position,
        })
    }

    #[zbus(signal)]
    async fn seeked(signal_emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
}

async fn serve(snapshot: Arc<Mutex<MprisSnapshot>>, commands: Sender<MprisCommand>) -> zbus::Result<()> {
    let connection = zbus::connection::Builder::session()
        .name(BUS_NAME)
        .serve_at(OBJECT_PATH, MprisRoot)
        .serve_at(
            OBJECT_PATH,
            MprisPlayer {
                snapshot: Arc::clone(&snapshot),
                commands,
            },
        )
        .build()
        .await?;

    let iface_ref = connection
        .object_server()
        .interface::<_, MprisPlayer>(OBJECT_PATH)
        .await?;
    let mut previous = MprisSnapshot::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(50));

    loop {
        ticker.tick().await;
        let (current, seeked) = {
            let mut guard = match snapshot.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let current = guard.clone();
            let seeked = guard.seeked_position.take();
            (current, seeked)
        };

        {
            let iface = iface_ref.get_mut().await;
            if previous.playback_status != current.playback_status {
                iface
                    .playback_status_changed(iface_ref.signal_emitter())
                    .await?;
            }
            if previous.loop_status != current.loop_status {
                iface.loop_status_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.shuffle != current.shuffle {
                iface.shuffle_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.metadata != current.metadata {
                iface.metadata_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.volume != current.volume {
                iface.volume_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.can_play != current.can_play {
                iface.can_play_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.can_pause != current.can_pause {
                iface.can_pause_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.can_go_next != current.can_go_next {
                iface.can_go_next_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.can_go_previous != current.can_go_previous {
                iface
                    .can_go_previous_changed(iface_ref.signal_emitter())
                    .await?;
            }
            if previous.can_seek != current.can_seek {
                iface.can_seek_changed(iface_ref.signal_emitter()).await?;
            }
            if previous.can_control != current.can_control {
                iface
                    .can_control_changed(iface_ref.signal_emitter())
                    .await?;
            }
        }

        // El guard de `get_mut()` ya se soltó: emitir `Seeked` mientras vive
        // bloquea la conexión D-Bus.
        if let Some(position) = seeked {
            iface_ref.seeked(position).await?;
        }
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_stable_track_ids() {
        assert_eq!(track_id(0, 0), "/org/mpris/MediaPlayer2/Track/0_0");
        assert_eq!(track_id(2, 17), "/org/mpris/MediaPlayer2/Track/2_17");
    }

    #[test]
    fn builds_metadata_with_dbus_types() {
        let track = MprisTrack {
            id: track_id(1, 2),
            title: "Canción".to_owned(),
            artist: vec!["A".to_owned(), "B".to_owned()],
            album: "Álbum".to_owned(),
            genre: vec!["Rock".to_owned()],
            track_number: Some(4),
            url: "file:///tmp/song.opus".to_owned(),
            length_micros: Some(42_000_000),
            art_uri: "file:///tmp/cover.png".to_owned(),
        };
        let metadata = metadata_for(&track);
        let title: &str = metadata["xesam:title"].downcast_ref().unwrap();
        let album: &str = metadata["xesam:album"].downcast_ref().unwrap();
        let url: &str = metadata["xesam:url"].downcast_ref().unwrap();
        let number: i32 = metadata["xesam:trackNumber"].downcast_ref().unwrap();
        let length: i64 = metadata["mpris:length"].downcast_ref().unwrap();
        let art: &str = metadata["mpris:artUrl"].downcast_ref().unwrap();
        assert_eq!(title, "Canción");
        assert_eq!(album, "Álbum");
        assert_eq!(url, "file:///tmp/song.opus");
        assert_eq!(number, 4);
        assert_eq!(length, 42_000_000);
        assert_eq!(art, "file:///tmp/cover.png");
        assert!(metadata.contains_key("mpris:trackid"));
        assert!(metadata.contains_key("xesam:artist"));
        assert!(metadata.contains_key("xesam:genre"));
    }

    #[test]
    fn clamps_relative_seeks() {
        assert_eq!(seek_target(10_000_000, -4_000_000), 6_000_000);
        assert_eq!(seek_target(1_000, -9_000), 0);
        assert_eq!(seek_target(i64::MAX - 1, 10), i64::MAX);
    }

    #[test]
    fn maps_loop_status_to_repeat() {
        assert_eq!(loop_status_to_repeat("None"), Some(false));
        assert_eq!(loop_status_to_repeat("Track"), Some(true));
        assert_eq!(loop_status_to_repeat("Playlist"), Some(true));
        assert_eq!(loop_status_to_repeat("otro"), None);
    }

    #[test]
    fn detects_cover_formats_from_magic_bytes() {
        assert_eq!(art_extension(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]), Some("png"));
        assert_eq!(art_extension(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(art_extension(b"GIF89a...."), Some("gif"));
        assert_eq!(art_extension(b"RIFF....WEBP"), Some("webp"));
        assert_eq!(art_extension(b"BM...."), Some("bmp"));
        assert_eq!(art_extension(b"no imagen"), None);
        assert_eq!(art_extension(&[]), None);
    }

    #[test]
    fn writes_art_and_removes_the_old_extension() {
        let dir = std::env::temp_dir().join(format!("freeway-mpris-art-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let png = [0x89, b'P', b'N', b'G', 13, 10, 26, 10, 1, 2, 3];
        let jpg = [0xFF, 0xD8, 0xFF, 0xE0, 4, 5];

        let first = write_art_cache(&dir, &png).unwrap();
        assert_eq!(first, dir.join("cover.png"));
        assert_eq!(fs::read(&first).unwrap(), png);

        let second = write_art_cache(&dir, &jpg).unwrap();
        assert_eq!(second, dir.join("cover.jpg"));
        assert_eq!(fs::read(&second).unwrap(), jpg);
        assert!(!first.exists());
        assert!(!dir.join("cover.tmp").exists());

        fs::remove_dir_all(&dir).unwrap();
    }
}
