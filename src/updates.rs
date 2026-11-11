use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use ureq::Agent;

const OWNER: &str = "Daniel-A-G";
const REPOSITORY: &str = "Freeway-Player-R";
const MAX_DOWNLOAD_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug)]
pub enum UpdateMessage {
    Downloading(String),
    Finished(Result<UpdateOutcome, String>),
}

#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate(String),
    InstalledIsNewer { latest_tag: String },
    Downloaded { tag: String, path: PathBuf },
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    assets: Vec<ReleaseAsset>,
}

#[derive(Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
    size: u64,
    digest: Option<String>,
}

pub fn start_update_check() -> Result<Receiver<UpdateMessage>, String> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-update-check".to_owned())
        .spawn(move || {
            let result = check_and_download(&sender);
            let _ = sender.send(UpdateMessage::Finished(result));
        })
        .map_err(|error| format!("No se pudo iniciar la búsqueda de actualizaciones: {error}"))?;
    Ok(receiver)
}

fn check_and_download(sender: &Sender<UpdateMessage>) -> Result<UpdateOutcome, String> {
    let agent: Agent = Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(300)))
        .build()
        .into();
    let api_url = format!("https://api.github.com/repos/{OWNER}/{REPOSITORY}/releases/latest");
    let mut response = agent
        .get(&api_url)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|error| format!("No se pudo consultar GitHub: {error}"))?;
    let release: GithubRelease = response
        .body_mut()
        .read_json()
        .map_err(|error| format!("GitHub devolvió una respuesta no válida: {error}"))?;

    if release.draft || release.prerelease {
        return Err("GitHub no devolvió una release estable publicada".to_owned());
    }

    let latest_version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let latest = Version::parse(latest_version).map_err(|_| {
        format!(
            "La etiqueta {} no tiene una versión válida",
            release.tag_name
        )
    })?;
    let installed = Version::parse(env!("CARGO_PKG_VERSION"))
        .expect("CARGO_PKG_VERSION debe ser una versión SemVer válida");

    match latest.cmp_precedence(&installed) {
        Ordering::Equal => return Ok(UpdateOutcome::UpToDate(release.tag_name)),
        Ordering::Less => {
            return Ok(UpdateOutcome::InstalledIsNewer {
                latest_tag: release.tag_name,
            });
        }
        Ordering::Greater => {}
    }

    let asset_name = format!("freeway-player-{}-x86_64.flatpak", release.tag_name);
    let asset = release
        .assets
        .iter()
        .find(|asset| asset.name == asset_name)
        .ok_or_else(|| format!("La release {} no contiene {asset_name}", release.tag_name))?;
    if asset.size == 0 || asset.size > MAX_DOWNLOAD_BYTES {
        return Err("El tamaño del bundle Flatpak no es válido".to_owned());
    }
    validate_download_url(&asset.browser_download_url)?;
    let expected_digest = expected_sha256(&agent, &release, asset)?;

    let download_dir = download_directory()?;
    fs::create_dir_all(&download_dir)
        .map_err(|error| format!("No se pudo preparar la carpeta de descargas: {error}"))?;
    let destination = download_dir.join(&asset_name);
    if destination.is_file()
        && file_sha256(&destination)
            .map(|digest| digest == expected_digest)
            .unwrap_or(false)
    {
        return Ok(UpdateOutcome::Downloaded {
            tag: release.tag_name,
            path: destination,
        });
    }

    let _ = sender.send(UpdateMessage::Downloading(release.tag_name.clone()));
    let temporary = download_dir.join(format!(".{asset_name}.part-{}", std::process::id()));
    let _ = fs::remove_file(&temporary);

    let download_result = download_asset(&agent, asset, &temporary, &expected_digest);
    if let Err(error) = download_result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    fs::rename(&temporary, &destination)
        .map_err(|error| format!("No se pudo guardar el bundle descargado: {error}"))?;

    Ok(UpdateOutcome::Downloaded {
        tag: release.tag_name,
        path: destination,
    })
}

fn expected_sha256(
    agent: &Agent,
    release: &GithubRelease,
    asset: &ReleaseAsset,
) -> Result<String, String> {
    if let Some(digest) = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
        .filter(|digest| is_sha256(digest))
    {
        return Ok(digest.to_ascii_lowercase());
    }

    let checksum_name = format!("{}.sha256", asset.name);
    let checksum_asset = release
        .assets
        .iter()
        .find(|candidate| candidate.name == checksum_name)
        .ok_or_else(|| "La release no incluye el checksum SHA-256 del Flatpak".to_owned())?;
    validate_download_url(&checksum_asset.browser_download_url)?;
    let mut response = agent
        .get(&checksum_asset.browser_download_url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|error| format!("No se pudo descargar el checksum: {error}"))?;
    let checksum = response
        .body_mut()
        .read_to_string()
        .map_err(|error| format!("No se pudo leer el checksum: {error}"))?;
    let digest = checksum
        .split_whitespace()
        .next()
        .filter(|digest| is_sha256(digest))
        .ok_or_else(|| "El checksum de la release no tiene un formato válido".to_owned())?;
    Ok(digest.to_ascii_lowercase())
}

fn download_asset(
    agent: &Agent,
    asset: &ReleaseAsset,
    temporary: &Path,
    expected_digest: &str,
) -> Result<(), String> {
    let mut response = agent
        .get(&asset.browser_download_url)
        .header("User-Agent", "Freeway-Player")
        .call()
        .map_err(|error| format!("No se pudo descargar el Flatpak: {error}"))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(temporary)
        .map_err(|error| format!("No se pudo crear el archivo temporal: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;

    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| format!("La descarga del Flatpak quedó incompleta: {error}"))?;
        if read == 0 {
            break;
        }
        total = total.saturating_add(read as u64);
        if total > MAX_DOWNLOAD_BYTES || total > asset.size {
            return Err("La descarga supera el tamaño esperado del bundle".to_owned());
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| format!("No se pudo guardar el Flatpak: {error}"))?;
    }
    if total != asset.size {
        return Err("El tamaño del Flatpak descargado no coincide con el publicado".to_owned());
    }
    file.sync_all()
        .map_err(|error| format!("No se pudo finalizar el archivo descargado: {error}"))?;

    let actual_digest = format!("{:x}", hasher.finalize());
    if actual_digest != expected_digest {
        return Err("El SHA-256 descargado no coincide; se descartó el archivo".to_owned());
    }
    Ok(())
}

fn file_sha256(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn validate_download_url(url: &str) -> Result<(), String> {
    let expected_prefix = format!("https://github.com/{OWNER}/{REPOSITORY}/releases/download/");
    if url.starts_with(&expected_prefix) {
        Ok(())
    } else {
        Err("GitHub devolvió una URL de descarga inesperada".to_owned())
    }
}

fn is_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn download_directory() -> Result<PathBuf, String> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| "No se encontró la carpeta personal del usuario".to_owned())?;

    let config_dir = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let user_dirs = config_dir.join("user-dirs.dirs");
    if let Ok(contents) = fs::read_to_string(user_dirs) {
        for line in contents.lines() {
            let Some(value) = line.strip_prefix("XDG_DOWNLOAD_DIR=") else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            let home_text = home.to_string_lossy();
            let value = value
                .replace("${HOME}", &home_text)
                .replace("$HOME", &home_text);
            let path = PathBuf::from(value);
            if path.is_absolute() {
                return Ok(path);
            }
        }
    }

    let localized = home.join("Descargas");
    if localized.is_dir() {
        return Ok(localized);
    }
    Ok(home.join("Downloads"))
}
