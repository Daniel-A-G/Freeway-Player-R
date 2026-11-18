//! Instancia única.
//!
//! El primer proceso abre un socket Unix en el directorio de ejecución del
//! usuario y se queda escuchando. Los siguientes le envían las rutas que
//! quieren abrir y terminan, para que la ventana existente las añada a la
//! lista de sesión. Un bloqueo `flock` evita que dos procesos se declaren
//! primarios a la vez cuando se abren varios archivos de golpe.

use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;
use std::time::{Duration, Instant};

/// Mensaje que envía una segunda instancia a la que ya está abierta.
#[derive(Serialize, Deserialize)]
struct Message {
    paths: Vec<PathBuf>,
}

pub enum Role {
    /// Este proceso es el principal y escucha en el socket.
    Primary(Instance),
    /// Ya había una instancia abierta: se le enviaron las rutas.
    Forwarded,
    /// No se pudo preparar la instancia única; la app arranca igualmente.
    Unavailable(String),
}

/// Socket y bloqueo de la instancia principal.
pub struct Instance {
    listener: UnixListener,
    socket_path: PathBuf,
    _lock: File,
}

impl Instance {
    /// Lanza el hilo que recibe las rutas de las siguientes instancias.
    pub fn spawn_receiver(&self, sender: Sender<Vec<PathBuf>>) -> io::Result<()> {
        let listener = self.listener.try_clone()?;
        thread::Builder::new()
            .name("freeway-instance".to_owned())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut payload = String::new();
                    let _ = stream
                        .set_read_timeout(Some(Duration::from_secs(2)));
                    if stream.take(64 * 1024).read_to_string(&mut payload).is_err() {
                        continue;
                    }
                    if let Ok(Message { paths }) = serde_json::from_str(&payload) {
                        if sender.send(paths).is_err() {
                            break;
                        }
                    }
                }
            })?;
        Ok(())
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket_path);
    }
}

/// Intenta entregar las rutas a una instancia abierta o convertirse en la
/// principal. `Unavailable` no impide usar la app: solo pierde la instancia
/// única.
pub fn start(paths: &[PathBuf]) -> Role {
    let Some(directory) = runtime_directory() else {
        return Role::Unavailable(
            "no hay un directorio de ejecución para el socket de instancia única".to_owned(),
        );
    };
    start_in(&directory, paths)
}

fn start_in(directory: &Path, paths: &[PathBuf]) -> Role {
    if let Err(error) = prepare_directory(directory) {
        return Role::Unavailable(error.to_string());
    }
    let socket_path = directory.join("freeway-player.sock");
    let lock_path = directory.join("freeway-player.lock");
    if socket_path.as_os_str().len() > 100 {
        return Role::Unavailable("la ruta del socket es demasiado larga".to_owned());
    }

    let lock = match OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
    {
        Ok(lock) => lock,
        Err(error) => return Role::Unavailable(error.to_string()),
    };

    match lock_primary(&lock) {
        Ok(true) => {}
        Ok(false) => {
            return if forward(&socket_path, paths) {
                Role::Forwarded
            } else {
                Role::Unavailable(
                    "otra instancia tiene el bloqueo pero no responde".to_owned(),
                )
            };
        }
        Err(error) => return Role::Unavailable(error.to_string()),
    }

    // Si quedó un socket de un cierre brusco, conexión rechazada y a borrarlo.
    let _ = fs::remove_file(&socket_path);
    match UnixListener::bind(&socket_path) {
        Ok(listener) => Role::Primary(Instance {
            listener,
            socket_path,
            _lock: lock,
        }),
        Err(error) => Role::Unavailable(error.to_string()),
    }
}

/// `true` si este proceso consiguió el bloqueo de instancia principal.
fn lock_primary(file: &File) -> io::Result<bool> {
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        return Ok(true);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(error)
    }
}

/// Envía las rutas y espera un momento a que la instancia principal escuche.
///
/// Al arrancar varias aperturas a la vez, la principal puede tener el bloqueo
/// pero no haber llegado todavía a crear el socket; por eso se reintenta.
fn forward(socket_path: &Path, paths: &[PathBuf]) -> bool {
    let payload = match serde_json::to_vec(&Message {
        paths: paths.to_vec(),
    }) {
        Ok(payload) => payload,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match UnixStream::connect(socket_path) {
            Ok(mut stream) => {
                if stream.write_all(&payload).is_ok() {
                    let _ = stream.shutdown(std::net::Shutdown::Write);
                }
                return true;
            }
            Err(_) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return false,
        }
    }
}

fn runtime_directory() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)?;
    Some(runtime_directory_in(&runtime, std::env::var_os("FLATPAK_ID")))
}

/// Dentro de Flatpak, `$XDG_RUNTIME_DIR/app/<id>` es el directorio compartido
/// entre instancias de la app. Fuera, basta con un subdirectorio propio.
fn runtime_directory_in(runtime: &Path, flatpak_id: Option<std::ffi::OsString>) -> PathBuf {
    match flatpak_id.filter(|id| !id.is_empty()) {
        Some(id) => runtime.join("app").join(id),
        None => runtime.join("freeway-player"),
    }
}

fn prepare_directory(directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_directory(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "freeway-instance-{}-{}",
            std::process::id(),
            name
        ));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn forwards_paths_to_the_primary_instance() {
        let directory = test_directory("forward");
        let paths = vec![
            PathBuf::from("/música/a.flac"),
            PathBuf::from("/música/mi canción.flac"),
        ];
        let Role::Primary(primary) = start_in(&directory, &[]) else {
            panic!("el primer proceso debe ser el principal");
        };
        let (sender, receiver) = mpsc::channel();
        primary.spawn_receiver(sender).unwrap();

        match start_in(&directory, &paths) {
            Role::Forwarded => {}
            _ => panic!("el segundo proceso debe reenviar las rutas"),
        }
        let received = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("la instancia principal debe recibir las rutas");
        assert_eq!(received, paths);

        drop(primary);
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn stale_socket_does_not_block_a_new_primary() {
        let directory = test_directory("stale");
        prepare_directory(&directory).unwrap();
        let socket = directory.join("freeway-player.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());

        match start_in(&directory, &[]) {
            Role::Primary(primary) => drop(primary),
            _ => panic!("un socket huérfano debe poder reemplazarse"),
        }
        assert!(!socket.exists());
        let _ = fs::remove_dir_all(&directory);
    }

    #[test]
    fn flatpak_uses_the_shared_app_runtime_directory() {
        let runtime = Path::new("/run/user/1000");
        assert_eq!(
            runtime_directory_in(runtime, Some("io.github.freewayplayer.FreewayPlayer".into())),
            runtime
                .join("app")
                .join("io.github.freewayplayer.FreewayPlayer")
        );
        assert_eq!(
            runtime_directory_in(runtime, None),
            runtime.join("freeway-player")
        );
    }
}
