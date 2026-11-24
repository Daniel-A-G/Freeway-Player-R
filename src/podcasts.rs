use feed_rs::parser;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PodcastFeed {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub episodes: Vec<PodcastEpisode>,
    #[serde(default)]
    pub last_checked: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PodcastEpisode {
    pub id: String,
    pub title: String,
    pub enclosure_url: String,
    #[serde(default)]
    pub published: Option<i64>,
    #[serde(default)]
    pub duration_secs: Option<u64>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub image_url: Option<String>,
    #[serde(default)]
    pub feed_title: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EpisodeState {
    #[serde(default)]
    pub played: bool,
    #[serde(default)]
    pub position_secs: u64,
    #[serde(default)]
    pub last_played: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PodcastStore {
    #[serde(default)]
    pub subscriptions: Vec<PodcastFeed>,
    #[serde(default)]
    pub episode_state: HashMap<String, EpisodeState>,
}

impl PodcastStore {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let file = fs::File::create(path).map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(file, self).map_err(|e| e.to_string())
    }

    pub fn episode_state(&self, id: &str) -> EpisodeState {
        self.episode_state.get(id).cloned().unwrap_or_default()
    }

    pub fn mark_played(&mut self, id: &str, played: bool) {
        let state = self.episode_state.entry(id.to_owned()).or_default();
        state.played = played;
        state.last_played = Some(now_ts());
    }

    pub fn set_position(&mut self, id: &str, position_secs: u64) {
        let state = self.episode_state.entry(id.to_owned()).or_default();
        state.position_secs = position_secs;
        state.last_played = Some(now_ts());
    }
}

#[derive(Clone, Debug)]
pub struct PodcastSearchResult {
    pub title: String,
    pub author: String,
    pub feed_url: String,
    pub artwork_url: Option<String>,
}

#[derive(Debug)]
pub enum PodcastMessage {
    Searching { generation: u64 },
    SearchResults { generation: u64, results: Vec<PodcastSearchResult> },
    FeedLoading { generation: u64 },
    FeedLoaded { generation: u64, feed: PodcastFeed },
    Error { generation: u64, message: String },
}

#[derive(Debug)]
pub enum PodcastAudioMessage {
    Ready {
        generation: u64,
        episode_id: String,
        path: PathBuf,
        duration: Option<Duration>,
    },
    Error {
        generation: u64,
        message: String,
    },
}

pub fn start_search(query: String, generation: u64) -> Receiver<PodcastMessage> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-podcast-search".to_owned())
        .spawn(move || {
            let _ = tx.send(PodcastMessage::Searching { generation });
            let result = if query.starts_with("http://") || query.starts_with("https://") {
                fetch_feed(&query).map(|feed| PodcastMessage::FeedLoaded { generation, feed })
            } else {
                search_itunes(&query).map(|results| PodcastMessage::SearchResults { generation, results })
            };
            match result {
                Ok(msg) => { let _ = tx.send(msg); }
                Err(err) => { let _ = tx.send(PodcastMessage::Error { generation, message: err }); }
            }
        })
        .ok();
    rx
}

pub fn start_refresh_feed(url: String, generation: u64) -> Receiver<PodcastMessage> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-podcast-feed".to_owned())
        .spawn(move || {
            let _ = tx.send(PodcastMessage::FeedLoading { generation });
            match fetch_feed(&url) {
                Ok(feed) => { let _ = tx.send(PodcastMessage::FeedLoaded { generation, feed }); }
                Err(err) => { let _ = tx.send(PodcastMessage::Error { generation, message: err }); }
            }
        })
        .ok();
    rx
}

pub fn start_download_episode(episode: PodcastEpisode, generation: u64, cache_dir: PathBuf) -> Receiver<PodcastAudioMessage> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-podcast-audio".to_owned())
        .spawn(move || {
            let result = download_episode(&episode, &cache_dir);
            match result {
                Ok((path, duration)) => {
                    let _ = tx.send(PodcastAudioMessage::Ready {
                        generation,
                        episode_id: episode.id.clone(),
                        path,
                        duration,
                    });
                }
                Err(err) => {
                    let _ = tx.send(PodcastAudioMessage::Error { generation, message: err });
                }
            }
        })
        .ok();
    rx
}

fn search_itunes(query: &str) -> Result<Vec<PodcastSearchResult>, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(20)))
        .build()
        .into();
    let url = format!(
        "https://itunes.apple.com/search?media=podcast&limit=15&term={}",
        urlencoding(query)
    );
    let mut response = agent
        .get(&url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|e| format!("No se pudo buscar podcasts: {e}"))?;
    let body: serde_json::Value = response
        .body_mut()
        .read_json()
        .map_err(|e| format!("Respuesta de búsqueda no válida: {e}"))?;
    let mut results = Vec::new();
    if let Some(items) = body.get("results").and_then(|v| v.as_array()) {
        for item in items {
            let feed_url = item.get("feedUrl").and_then(|v| v.as_str()).unwrap_or("").trim();
            if feed_url.is_empty() { continue; }
            let title = item.get("collectionName").or_else(|| item.get("trackName"))
                .and_then(|v| v.as_str()).unwrap_or("Podcast").to_owned();
            let author = item.get("artistName").and_then(|v| v.as_str()).unwrap_or("").to_owned();
            let artwork_url = item.get("artworkUrl600").or_else(|| item.get("artworkUrl100"))
                .and_then(|v| v.as_str()).map(|s| s.to_owned());
            results.push(PodcastSearchResult { title, author, feed_url: feed_url.to_owned(), artwork_url });
        }
    }
    Ok(results)
}

fn fetch_feed(url: &str) -> Result<PodcastFeed, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .get(url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|e| format!("No se pudo descargar el feed: {e}"))?;
    let mut bytes = Vec::new();
    response.body_mut().as_reader().read_to_end(&mut bytes)
        .map_err(|e| format!("No se pudo leer el feed: {e}"))?;
    let feed = parser::parse(&bytes[..]).map_err(|e| format!("No se pudo interpretar el feed RSS/Atom: {e}"))?;

    let title = feed.title.as_ref().map(|t| t.content.clone()).filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "Podcast".to_owned());
    let author = feed.authors.first().map(|a| a.name.clone()).unwrap_or_default();
    let description = feed.description.as_ref().map(|d| d.content.clone()).unwrap_or_default();
    let artwork_url = feed.icon.as_ref().map(|i| i.uri.clone())
        .or_else(|| feed.logo.as_ref().map(|i| i.uri.clone()));

    let mut episodes = Vec::new();
    for entry in feed.entries.iter().take(200) {
        let enclosure_url = entry.media.first()
            .and_then(|m| m.content.first())
            .and_then(|c| c.url.as_ref().map(|u| u.to_string()))
            .or_else(|| entry.links.iter().find(|l| {
                l.rel.as_deref() == Some("enclosure") || l.media_type.as_deref().map(|m| m.starts_with("audio/")).unwrap_or(false)
            }).map(|l| l.href.clone()));
        let Some(enclosure_url) = enclosure_url else { continue };
        let ep_title = entry.title.as_ref().map(|t| t.content.clone()).filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "Episodio".to_owned());
        let guid = if !entry.id.trim().is_empty() { entry.id.clone() } else { enclosure_url.clone() };
        let published = entry.published.or(entry.updated).map(|d| d.timestamp());
        let duration_secs = entry.media.first()
            .and_then(|m| m.content.first())
            .and_then(|c| c.duration)
            .map(|d| d.as_secs());
        let ep_desc = entry.summary.as_ref().map(|s| s.content.clone()).unwrap_or_default();
        let image_url = entry.media.first()
            .and_then(|m| m.thumbnails.first())
            .map(|t| t.image.uri.clone());
        episodes.push(PodcastEpisode {
            id: guid,
            title: ep_title,
            enclosure_url,
            published,
            duration_secs,
            description: ep_desc,
            image_url,
            feed_title: title.clone(),
        });
    }

    Ok(PodcastFeed {
        id: url.to_owned(),
        title,
        url: url.to_owned(),
        author,
        artwork_url,
        description,
        episodes,
        last_checked: Some(now_ts()),
    })
}

fn download_episode(episode: &PodcastEpisode, cache_dir: &Path) -> Result<(PathBuf, Option<Duration>), String> {
    fs::create_dir_all(cache_dir).map_err(|e| format!("No se pudo crear la caché de podcasts: {e}"))?;
    let ext = episode.enclosure_url
        .rsplit('/')
        .next()
        .and_then(|s| s.split('?').next())
        .and_then(|s| s.rsplit('.').next())
        .filter(|e| matches!(*e, "mp3"|"m4a"|"aac"|"ogg"|"opus"|"wav"|"flac"))
        .unwrap_or("audio");
    let safe = safe_name(&episode.id);
    let path = cache_dir.join(format!("{safe}.{ext}"));
    if path.exists() && path.metadata().map(|m| m.len() > 0).unwrap_or(false) {
        return Ok((path, episode.duration_secs.map(Duration::from_secs)));
    }

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .build()
        .into();
    let mut response = agent
        .get(&episode.enclosure_url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|e| format!("No se pudo descargar el episodio: {e}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = fs::File::create(&path).map_err(|e| format!("No se pudo crear el archivo de audio: {e}"))?;
    let mut buf = [0u8; 16384];
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("Error descargando episodio: {e}"))?;
        if n == 0 { break; }
        file.write_all(&buf[..n]).map_err(|e| format!("Error escribiendo episodio: {e}"))?;
    }
    file.flush().ok();
    Ok((path, episode.duration_secs.map(Duration::from_secs)))
}

fn safe_name(input: &str) -> String {
    let mut out = String::new();
    for ch in input.chars() {
        if ch.is_ascii_alphanumeric() { out.push(ch); }
        else if !out.ends_with('-') { out.push('-'); }
        if out.len() > 80 { break; }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() { "episode".to_owned() } else { out }
}

fn urlencoding(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

fn now_ts() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

pub fn podcast_store_path() -> PathBuf {
    dirs_or_default().join("podcasts.json")
}

pub fn podcast_cache_dir() -> PathBuf {
    dirs_or_default().join("podcast-cache")
}

/// Descarga una carátula en segundo plano y la entrega como `(clave, bytes)`.
/// La clave identifica al episodio o podcast que debe mostrarla.
pub fn start_fetch_artwork(url: String, key: String, sender: Sender<(String, Vec<u8>)>) {
    thread::Builder::new()
        .name("freeway-podcast-art".to_owned())
        .spawn(move || {
            let agent: ureq::Agent = ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(20)))
                .build()
                .into();
            let mut bytes = Vec::new();
            if let Ok(mut response) = agent
                .get(&url)
                .header("User-Agent", "Freeway-Player")
                .call()
            {
                let _ = response.body_mut().as_reader().take(5 * 1024 * 1024)
                    .read_to_end(&mut bytes);
            }
            let _ = sender.send((key, bytes));
        })
        .ok();
}

fn dirs_or_default() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_DATA_HOME") {
        PathBuf::from(dir).join("freeway-player")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".local/share/freeway-player")
    } else {
        PathBuf::from("/tmp/freeway-player")
    }
}
