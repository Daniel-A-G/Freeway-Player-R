use ashpd::flatpak::{update_monitor::UpdateStatus, Flatpak, SpawnFlags, SpawnOptions};
use futures_util::StreamExt;
use semver::Version;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::env;
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use ureq::Agent;

const OWNER: &str = "Daniel-A-G";
const REPOSITORY: &str = "Freeway-Player-R";

pub const APPSTREAM_URI: &str = "appstream://io.github.freewayplayer.FreewayPlayer";
pub const GITHUB_RELEASES_URL: &str = "https://github.com/Daniel-A-G/Freeway-Player-R/releases/latest";
pub const PROJECT_WEBSITE_URL: &str = "https://daniel-a-g.github.io/Freeway-Player-R/";

#[derive(Debug)]
pub enum UpdateMessage {
    Available(String),
    Installing(String),
    Progress { tag: String, percent: u32 },
    Finished(Result<UpdateOutcome, String>),
}

#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate(String),
    InstalledIsNewer { latest_tag: String },
    Declined(String),
    ManualRequired { tag: String, has_remote: bool },
    InstalledAndRestarting(String),
}

pub struct UpdateTask {
    pub messages: Receiver<UpdateMessage>,
    pub decision: Sender<bool>,
}

#[derive(Deserialize)]
struct GithubRelease {
    tag_name: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    draft: bool,
}

/// Comprueba si el texto de la release indica que se requiere actualización manual
pub fn is_manual_update_required(body: Option<&str>) -> bool {
    body.map(|b| {
        let lower = b.to_lowercase();
        lower.contains("[manual-update]")
            || lower.contains("manual-update")
            || lower.contains("[actualizacion-manual]")
            || lower.contains("[actualización-manual]")
    })
    .unwrap_or(false)
}

/// Extrae el nombre del repositorio remoto desde el contenido de /.flatpak-info
pub fn parse_flatpak_origin(content: &str) -> Option<String> {
    for line in content.lines() {
        let line = line.trim();
        if let Some(origin) = line.strip_prefix("origin=") {
            let origin = origin.trim();
            if !origin.is_empty() {
                return Some(origin.to_owned());
            }
        }
    }
    None
}

/// Comprueba si la aplicación se instaló desde un repositorio remoto de Flatpak (p. ej. mediante .flatpakref)
pub fn has_flatpak_remote() -> bool {
    let path = Path::new("/.flatpak-info");
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| parse_flatpak_origin(&c))
        .is_some()
}

/// Abre una URL o URI en el sistema anfitrión (usando xdg-open a través del portal de Flatpak o directamente)
pub fn open_external_uri(uri: &str) -> bool {
    std::process::Command::new("xdg-open")
        .arg(uri)
        .spawn()
        .is_ok()
}

pub fn start_update_check() -> Result<UpdateTask, String> {
    let (sender, receiver) = mpsc::channel();
    let (decision_sender, decision_receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-update-check".to_owned())
        .spawn(move || {
            let result = check_and_install(&sender, &decision_receiver);
            let _ = sender.send(UpdateMessage::Finished(result));
        })
        .map_err(|error| format!("No se pudo iniciar la búsqueda de actualizaciones: {error}"))?;
    Ok(UpdateTask {
        messages: receiver,
        decision: decision_sender,
    })
}

fn check_and_install(
    sender: &Sender<UpdateMessage>,
    decision_receiver: &Receiver<bool>,
) -> Result<UpdateOutcome, String> {
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

    // Comprobar si la versión requiere actualización manual (p. ej. por cambios de permisos de Flatpak)
    if is_manual_update_required(release.body.as_deref()) {
        let has_remote = has_flatpak_remote();
        return Ok(UpdateOutcome::ManualRequired {
            tag: release.tag_name,
            has_remote,
        });
    }

    let _ = sender.send(UpdateMessage::Available(release.tag_name.clone()));
    let should_install = decision_receiver
        .recv()
        .map_err(|_| "No se recibió una respuesta sobre la actualización".to_owned())?;
    if !should_install {
        return Ok(UpdateOutcome::Declined(release.tag_name));
    }

    let _ = sender.send(UpdateMessage::Installing(release.tag_name.clone()));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("No se pudo iniciar el cliente del portal Flatpak: {error}"))?;

    let install_result = runtime.block_on(install_through_flatpak_portal(sender, &release.tag_name));
    match install_result {
        Ok(()) => Ok(UpdateOutcome::InstalledAndRestarting(release.tag_name)),
        Err(error) => {
            let is_permission_or_manual = error.contains("NotSupported")
                || error.to_lowercase().contains("permission")
                || error.to_lowercase().contains("permiso");
            if is_permission_or_manual {
                let has_remote = has_flatpak_remote();
                Ok(UpdateOutcome::ManualRequired {
                    tag: release.tag_name,
                    has_remote,
                })
            } else {
                Err(error)
            }
        }
    }
}

async fn install_through_flatpak_portal(
    sender: &Sender<UpdateMessage>,
    tag: &str,
) -> Result<(), String> {
    let flatpak = Flatpak::new()
        .await
        .map_err(|error| format!("No se pudo conectar con el portal Flatpak: {error}"))?;
    if flatpak.version() < 2 {
        return Err("El portal Flatpak de este sistema no admite instalaciones desde la aplicación".to_owned());
    }

    let monitor = flatpak
        .create_update_monitor(Default::default())
        .await
        .map_err(|error| {
            format!(
                "No se pudo iniciar el actualizador Flatpak. Comprueba que esta versión esté instalada desde el remoto de Freeway Player: {error}"
            )
        })?;
    let mut progress_stream = monitor
        .receive_progress()
        .await
        .map_err(|error| format!("No se pudo recibir el progreso de Flatpak: {error}"))?;

    monitor
        .update(None, Default::default())
        .await
        .map_err(|error| format!("Flatpak no pudo iniciar la actualización: {error}"))?;

    loop {
        let progress = tokio::time::timeout(Duration::from_secs(600), progress_stream.next())
            .await
            .map_err(|_| "La instalación Flatpak tardó demasiado y se interrumpió".to_owned())?
            .ok_or_else(|| "Flatpak cerró el canal de progreso antes de terminar".to_owned())?;

        if let Some(percent) = progress.progress() {
            let _ = sender.send(UpdateMessage::Progress {
                tag: tag.to_owned(),
                percent,
            });
        }

        match progress.status() {
            Some(UpdateStatus::Running) | None => {}
            Some(UpdateStatus::Empty) => {
                return Err(format!(
                    "GitHub publica {tag}, pero el remoto Flatpak todavía no tiene esa actualización"
                ));
            }
            Some(UpdateStatus::Done) => break,
            Some(UpdateStatus::Failed) => {
                let details = progress
                    .error_message()
                    .or_else(|| progress.error())
                    .unwrap_or("Flatpak no proporcionó más detalles");
                return Err(format!("Flatpak no pudo instalar la actualización: {details}"));
            }
        }
    }

    let _ = monitor.close().await;
    let executable = Path::new("/app/bin/freeway-player");
    let argv = [executable];
    flatpak
        .spawn(
            Path::new("/"),
            &argv,
            HashMap::<u32, OwnedFd>::new(),
            HashMap::<&str, &str>::new(),
            SpawnFlags::LATEST_VERSION,
            SpawnOptions::default(),
        )
        .await
        .map_err(|error| format!("La actualización quedó instalada, pero no se pudo reiniciar Freeway Player: {error}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_manual_update_flags() {
        assert!(is_manual_update_required(Some("This release has [manual-update] due to permissions")));
        assert!(is_manual_update_required(Some("Includes manual-update requirement")));
        assert!(is_manual_update_required(Some("Requiere [actualización-manual] por cambios de permisos")));
        assert!(is_manual_update_required(Some("[ACTUALIZACION-MANUAL]")));
        assert!(!is_manual_update_required(Some("Regular bug fix release")));
        assert!(!is_manual_update_required(None));
    }

    #[test]
    fn parses_flatpak_origin() {
        let info_with_remote = "[Application]\nname=io.github.freewayplayer.FreewayPlayer\n\n[Instance]\norigin=freeway-player\nbranch=stable\n";
        assert_eq!(parse_flatpak_origin(info_with_remote), Some("freeway-player".to_owned()));

        let info_with_empty_origin = "[Instance]\norigin=\n";
        assert_eq!(parse_flatpak_origin(info_with_empty_origin), None);

        let info_without_origin = "[Application]\nname=io.github.freewayplayer.FreewayPlayer\n";
        assert_eq!(parse_flatpak_origin(info_without_origin), None);
    }
}
