use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{Picture, PictureType};
use lofty::tag::{Accessor, Tag};
use raven::*;
use rodio::{DeviceSinkBuilder, MixerDeviceSink, Player, Source};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::hash_map::RandomState;
use std::ffi::c_void;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

#[cfg(test)]
use rodio::Decoder;

mod opus;

const EMBEDDED_UI: &str = include_str!("../ui/main.xml");
const INITIAL_VOLUME: f32 = 0.8;

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
}

#[derive(Serialize, Deserialize)]
struct PlaylistStore {
    playlists: Vec<SavedPlaylist>,
    active_playlist: usize,
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
        };
        playlist.rebuild_order(selected);
        playlist
    }

    fn saved(&self) -> SavedPlaylist {
        SavedPlaylist {
            name: self.name.clone(),
            tracks: self.tracks.clone(),
            selected: self.selected,
            shuffle: self.shuffle,
            repeat: self.repeat,
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
        self.order = shuffled_track_indices(self.tracks.len(), Some(current_track));
        if current_track < self.tracks.len() {
            self.order.push(current_track);
        }
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
    status: String,
    metadata_hint: String,
    cover_hint: String,
    ui_rendered_playlist: Option<(usize, u64)>,
    editor_loaded_for: Option<(usize, usize)>,
    ui_selection_dirty: bool,
    pending_cover: Option<((usize, usize), CoverChange)>,
    cover_rendered_for: Option<(usize, usize, u64)>,
    cover_texture: Rid,
}

impl PlayerState {
    fn new() -> Self {
        let (saved, active_playlist) = load_playlists();
        let mut playlists: Vec<_> = saved.into_iter().map(Playlist::from_saved).collect();
        if playlists.is_empty() {
            playlists.push(Playlist::new("Mi música".to_owned()));
        }
        let active_playlist = active_playlist.min(playlists.len() - 1);

        let (output, status) = match create_audio_output() {
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
            current: None,
            current_metadata: None,
            duration: None,
            volume: INITIAL_VOLUME,
            playing: false,
            status,
            metadata_hint: "Selecciona una pista para editar sus etiquetas".to_owned(),
            cover_hint: "No hay una carátula seleccionada".to_owned(),
            ui_rendered_playlist: None,
            editor_loaded_for: None,
            ui_selection_dirty: false,
            pending_cover: None,
            cover_rendered_for: None,
            cover_texture: R_INVALID_ID,
        }
    }

    fn save_playlists(&mut self) {
        let path = playlist_store_path();
        let Some(parent) = path.parent() else {
            self.status = "No se pudo guardar la ubicación de las listas".to_owned();
            return;
        };
        let store = PlaylistStore {
            playlists: self.playlists.iter().map(Playlist::saved).collect(),
            active_playlist: self.active_playlist,
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
        playlist.name = name.clone();
        self.status = format!("Lista renombrada: {name}");
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

    fn add_tracks(&mut self, paths: Vec<PathBuf>) {
        if self.playlists.is_empty() || paths.is_empty() {
            return;
        }
        let playlist_index = self.active_playlist;
        let playlist = &mut self.playlists[playlist_index];
        let first_added = playlist.tracks.len();
        for path in paths {
            if !playlist.tracks.contains(&path) {
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

        if self.current.is_none() {
            if let Err(error) = self.play_track(playlist_index, first_added, true) {
                self.status = error;
            }
        }
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
        output.player.set_volume(self.volume);
        output.player.append(source);
        if start_playing {
            output.player.play();
        }

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

    fn play(&mut self) {
        if self.output.is_none() {
            self.status = "No hay un dispositivo de salida de audio disponible".to_owned();
            return;
        }
        if let Some((playlist, track)) = self.current {
            if playlist == self.active_playlist {
                let empty = self
                    .output
                    .as_ref()
                    .map(|output| output.player.empty())
                    .unwrap_or(true);
                if empty {
                    if let Err(error) = self.play_track(playlist, track, true) {
                        self.status = error;
                    }
                } else {
                    self.output.as_ref().unwrap().player.play();
                    self.playing = true;
                    self.status = "Reproduciendo".to_owned();
                }
                return;
            }
        }
        self.play_selected(None);
    }

    fn pause(&mut self) {
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

        if forward && automatic && repeat && shuffle && position == Some(len - 1) {
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
        self.output
            .as_ref()
            .map(|output| output.player.get_pos())
            .unwrap_or_default()
    }

    fn selected_location(&self) -> Option<(usize, usize)> {
        self.playlists
            .get(self.active_playlist)?
            .selected
            .map(|track| (self.active_playlist, track))
    }

    fn save_metadata(&mut self, location: (usize, usize), metadata: TrackMetadata) {
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
        let save_result = write_track_metadata(&path, &metadata, cover_change);
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

        match (save_result, restore_result) {
            (Ok(()), Ok(())) => {
                self.playlists[location.0].revision += 1;
                if self.current == Some(location) {
                    self.current_metadata = Some(metadata);
                }
                self.pending_cover = None;
                if cover_was_changed {
                    self.cover_hint = "Carátula guardada".to_owned();
                }
                self.metadata_hint = "Etiquetas guardadas en el archivo".to_owned();
                self.status = "Metadatos guardados".to_owned();
            }
            (Err(error), _) => {
                self.metadata_hint = error.clone();
                self.status = error;
            }
            (Ok(()), Err(error)) => {
                self.metadata_hint = "Etiquetas guardadas; revisa la reproducción".to_owned();
                self.status = error;
            }
        }
    }
}

fn playlist_store_path() -> PathBuf {
    let config_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .unwrap_or_else(|| PathBuf::from("."));
    config_dir.join("freeway-player").join("playlists.json")
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

fn update_cover_art(state: &mut PlayerState, ui: Ui) {
    let location = state.current.or_else(|| state.selected_location());
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
) -> Result<(), String> {
    let track_number = if metadata.track.trim().is_empty() {
        None
    } else {
        let number = metadata
            .track
            .trim()
            .parse::<u32>()
            .map_err(|_| "El número de pista debe ser un entero positivo".to_owned())?;
        if number == 0 {
            return Err("El número de pista debe ser mayor que cero".to_owned());
        }
        Some(number)
    };
    let mut file = lofty::read_from_path(path)
        .map_err(|error| format!("No se pudieron leer los metadatos: {error}"))?;
    let tag_type = file.primary_tag_type();
    if !file.tag_support(tag_type).is_writable() {
        return Err("Este formato no admite escribir metadatos".to_owned());
    }
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(tag_type));
    }
    let tag = file
        .primary_tag_mut()
        .ok_or_else(|| "No se pudo crear la etiqueta del archivo".to_owned())?;

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
            let mut image_file = File::open(&image_path)
                .map_err(|error| format!("No se pudo abrir la carátula: {error}"))?;
            let mut picture = Picture::from_reader(&mut image_file)
                .map_err(|error| format!("La imagen no es compatible: {error}"))?;
            picture.set_pic_type(PictureType::CoverFront);
            if let Some(index) = tag
                .pictures()
                .iter()
                .position(|picture| picture.pic_type() == PictureType::CoverFront)
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
        .map_err(|error| format!("No se pudieron guardar los metadatos: {error}"))
}

fn create_audio_output() -> Result<AudioOutput, String> {
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
    player.set_volume(INITIAL_VOLUME);
    Ok(AudioOutput {
        player,
        _stream: stream,
    })
}

#[derive(Clone, Copy)]
struct Ui {
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
    shuffle: Rid,
    repeat: Rid,
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
            shuffle: widget_find(window, "shuffle"),
            repeat: widget_find(window, "repeat"),
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

fn track_row(path: &Path, index: usize) -> String {
    let metadata = read_track_metadata(path);
    let duration = metadata
        .duration
        .map(format_time)
        .unwrap_or_else(|| "--:--".to_owned());
    let artist = if metadata.artist.is_empty() {
        "Artista desconocido"
    } else {
        metadata.artist.as_str()
    };
    format!(
        "{:02}   {}  ·  {}     {}",
        index + 1,
        metadata.title,
        artist,
        duration
    )
}

fn update_track_list(state: &mut PlayerState, ui: Ui) {
    let Some(playlist) = state.playlists.get(state.active_playlist) else {
        return;
    };
    let marker = (state.active_playlist, playlist.revision);
    if state.ui_rendered_playlist == Some(marker) {
        return;
    }

    let name = playlist.name.clone();
    let tracks: Vec<_> = playlist.tracks.clone();
    let selected = playlist.selected;
    unsafe { Rlist_view_clear_items(ui.track_list) };
    for (index, path) in tracks.iter().enumerate() {
        let row = cstr(&track_row(path, index));
        unsafe { Rlist_view_add_item(ui.track_list, row.as_ptr()) };
    }
    if let Some(index) = selected {
        unsafe { Rlist_view_set_selected(ui.track_list, index as i32) };
    }
    state.ui_rendered_playlist = Some(marker);
    state.editor_loaded_for = None;
    state.ui_selection_dirty = false;
    set_input(ui.playlist_name, &name);
    set_label(
        ui.playlist_summary,
        &format!("{} pistas · {}", tracks.len(), name),
    );
}

fn update_metadata_editor(state: &mut PlayerState, ui: Ui) {
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
    let metadata = location
        .and_then(|(playlist, track)| state.playlists.get(playlist)?.tracks.get(track))
        .map(|path| read_track_metadata(path));
    if let Some(metadata) = metadata {
        set_input(ui.meta_title, &metadata.title);
        set_input(ui.meta_artist, &metadata.artist);
        set_input(ui.meta_album, &metadata.album);
        set_input(ui.meta_genre, &metadata.genre);
        set_input(ui.meta_track, &metadata.track);
        state.metadata_hint = "Los cambios se escriben en el archivo de audio".to_owned();
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

fn refresh_ui(state: &mut PlayerState, ui: Ui) {
    state.update_after_frame();
    update_track_list(state, ui);
    update_metadata_editor(state, ui);
    update_cover_art(state, ui);

    if let Some(metadata) = &state.current_metadata {
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
    set_label(
        ui.total,
        &state
            .duration
            .map(format_time)
            .unwrap_or_else(|| "--:--".to_owned()),
    );
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

    let duration_seconds = state.duration.unwrap_or_default().as_secs_f32();
    let position = state.position().as_secs_f32();
    unsafe {
        Rslider_set_range(ui.seek, 0.0, duration_seconds.max(1.0));
        Rslider_set_value(ui.seek, position.min(duration_seconds));
    }
}

fn add_playlist_tab(tabs: Rid, title: &str, make_active: bool) {
    let page = unsafe { Rwidget_create(R_WIDGET_VBOX) };
    let title = cstr(title);
    let index = unsafe { Rtab_view_add_tab(tabs, title.as_ptr(), page) };
    if index < 0 {
        return;
    }
    let page = unsafe { Rtab_view_get_tab(tabs, index) };
    unsafe {
        Rtab_view_set_tab_closable(tabs, page, R_TRUE);
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

fn open_audio_files(window: Rid) -> Vec<PathBuf> {
    let filter_name = cstr("Archivos de audio");
    let filter_patterns = cstr("*.mp3;*.flac;*.wav;*.ogg;*.opus;*.m4a");
    let title = cstr("Añadir pistas a la lista");
    let filter = RFileFilter {
        name: filter_name.as_ptr(),
        patterns: filter_patterns.as_ptr(),
    };
    let mut description: RFileDialogDesc = unsafe { std::mem::zeroed() };
    description.parent = window;
    description.title = title.as_ptr();
    description.filters = &filter;
    description.filter_count = 1;
    description.flags = (R_FILE_DIALOG_RAVEN | R_FILE_DIALOG_MULTIPLE) as Ruint;

    let first = unsafe { Rfile_dialog_open(&description) };
    if first.is_null() {
        return Vec::new();
    }

    let count = unsafe { Rfile_dialog_get_result_count() };
    if count > 0 {
        (0..count)
            .map(|index| PathBuf::from(unsafe { text(Rfile_dialog_get_result(index)) }))
            .collect()
    } else {
        vec![PathBuf::from(unsafe { text(first) })]
    }
}

fn open_cover_file(window: Rid) -> Option<PathBuf> {
    let filter_name = cstr("Imágenes de carátula");
    let filter_patterns = cstr("*.png;*.jpg;*.jpeg;*.bmp;*.gif");
    let title = cstr("Seleccionar carátula");
    let filter = RFileFilter {
        name: filter_name.as_ptr(),
        patterns: filter_patterns.as_ptr(),
    };
    let mut description: RFileDialogDesc = unsafe { std::mem::zeroed() };
    description.parent = window;
    description.title = title.as_ptr();
    description.filters = &filter;
    description.filter_count = 1;
    description.flags = R_FILE_DIALOG_RAVEN as Ruint;

    let selected = unsafe { Rfile_dialog_open(&description) };
    (!selected.is_null()).then(|| PathBuf::from(unsafe { text(selected) }))
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

fn main() {
    unsafe {
        if Rinit(R_BACKEND_VULKAN) == 0 {
            eprintln!("No se pudo iniciar Raven con Vulkan");
            std::process::exit(1);
        }
        Rlook_set(R_LOOK_RAVEN_DARK);
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
    let state = Rc::new(RefCell::new(PlayerState::new()));
    let context_menu = unsafe { Rwidget_create(R_WIDGET_MENU) };
    let edit_metadata_text = cstr("Editar metadatos");
    let edit_metadata_item = unsafe { Rmenu_add_item(context_menu, edit_metadata_text.as_ptr()) };
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
        Rtab_view_set_documents(ui.playlist_tabs, R_TRUE);
        Rtab_view_set_close_callback(
            ui.playlist_tabs,
            Some(close_playlist_tab),
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
    for (index, title) in tabs.iter().enumerate() {
        add_playlist_tab(ui.playlist_tabs, title, index == active_playlist);
    }
    if !tabs.is_empty() {
        unsafe { Rtab_view_set_active(ui.playlist_tabs, active_playlist as i32) };
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
            add_playlist_tab(tabs, &name, true);
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
            state.borrow_mut().seek(seconds);
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
            state.borrow_mut().save_metadata(location, metadata);
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
            let Some(path) = open_cover_file(window) else {
                return;
            };
            let mut state = state.borrow_mut();
            let Some(location) = state.editor_loaded_for else {
                state.status = "Abre el editor desde una pista para cambiar la carátula".to_owned();
                return;
            };
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

    refresh_ui(&mut state.borrow_mut(), ui);
    unsafe {
        while Rwait_frame(0.2) != 0 {
            refresh_ui(&mut state.borrow_mut(), ui);
        }
        Rwidget_set_context_menu(ui.track_list, R_INVALID_ID);
        Rwidget_destroy(context_menu);
        drop(Box::from_raw(context_data as *mut MetadataMenuContext));
        let cover_texture = state.borrow().cover_texture;
        if cover_texture != R_INVALID_ID {
            Rtexture_destroy(cover_texture);
        }
        Rshutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::{format_time, next_track_index, previous_track_index};
    use rodio::{Decoder, Source};
    use std::io::Cursor;
    use std::time::Duration;

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
