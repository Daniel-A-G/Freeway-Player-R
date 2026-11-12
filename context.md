# Contexto del proyecto

Resumen rápido para asistentes de IA. Actualizado el 2026-11-12.

## Estado actual

- Freeway Player es un reproductor local de música para Linux escrito en Rust.
- La interfaz usa un tema oscuro opaco con acento lila suave, superficies planas y radios discretos. Biblioteca, carátula/editor y reproductor forman una composición continua separada por líneas sutiles; no se usan transparencias, desenfoques ni tarjetas independientes.
- El rediseño conserva los nombres de widgets y las acciones existentes. Los cambios visuales no alteran la reproducción ni el comportamiento de listas, metadatos o controles.
- `ui/theme.xml` define colores, métricas y estilos sobre RavenDark con material sólido. `src/main.rs` carga primero `FREEWAY_THEME_FILE`, luego `ui/theme.xml` desde el directorio de trabajo y finalmente usa el tema incrustado mediante `include_str!`.
- Se mantienen las listas con pestañas, biblioteca seleccionable, controles con símbolos y tooltips, barra de búsqueda de posición y editor de etiquetas.
- `icon.png` (256×256 RGBA) se incrusta en el binario y se usa en la ventana y el encabezado; el manifiesto Flatpak lo instala como icono de la aplicación.
- Los símbolos Unicode usan Noto Sans Symbols 2 y Noto Sans Symbols como fuentes fallback incrustadas; los botones de aleatorio y repetición usan una pequeña textura de iconos propia. Esto evita depender de fuentes o temas de iconos del host, incluido Flatpak. El manifiesto instala también la licencia OFL de las fuentes.
- El selector de pistas y carátulas usa `rfd` con backend XDG Desktop Portal (con Zenity como fallback si el portal no está disponible). El escritorio anfitrión presenta el diálogo; puede reflejar ubicaciones fijadas por el usuario y recorrer enlaces a directorios, incluidos destinos fuera de `home`, con acceso concedido por el portal. La compilación del bundle está verificada, pero falta comprobar visualmente el diálogo en CachyOS.
- La lista de pistas usa el modo virtual de Raven: las etiquetas se leen de forma diferida para las filas visibles y se cachean mientras se muestra esa lista, evitando bloquear el hilo de UI al abrir listas grandes. Al añadir archivos, la deduplicación usa `HashSet`. `cargo check` pasó tras este cambio; falta que el usuario lo compruebe con su lista de más de 400 canciones.
- La compilación de Rust en modo release y la compilación Flatpak terminaron correctamente con los cambios de fuentes e iconos.
- El bundle anterior x86_64 (5.6 MB) se instaló en el perfil Flatpak de usuario y arrancó en sandbox durante 8 segundos sin errores en stderr; esa comprobación visual no detectaba el fallo de glifos en CachyOS.
- El bundle actualizado x86_64 `freeway-player.flatpak` (6.0 MB) incorpora las fuentes fallback, los iconos de aleatorio/repetición, la licencia OFL y el selector XDG Portal. SHA-256: `9518e5bcb236cbf518d6958fca8c4b8ef997fcce6056669af9b32ad2f8c14413`. La compilación y exportación terminaron; queda comprobar visualmente el selector en CachyOS.
- El bundle depende de Freedesktop Platform 25.08. El otro equipo debe tener Flatpak y acceso a Flathub para obtener ese runtime si aún no lo tiene. El bundle actual está construido para x86_64.
- El manifiesto excluye `.git`, `.local`, `.flatpak-builder`, `target`, `build-dir` y `repo` de la fuente local para evitar copiar artefactos. Raven se instala en `/app/lib` para coincidir con `RAVEN_LIB_DIR` y `LD_LIBRARY_PATH`.
- El usuario confirmó que la versión anterior ya funcionaba en su equipo.
- Raven está instalado localmente en `.local/raven`; existen enlaces simbólicos `libraven.so` y `libraven.so.0` hacia `libraven.so.0.2.1`.

### Sistema de actualizaciones automáticas (v0.1.4+)

- `src/updates.rs` implementa comprobación de actualizaciones consultando la API de GitHub Releases (`/releases/latest`). Compara la versión instalada (`CARGO_PKG_VERSION`) con la última release usando `semver`. Si hay una versión nueva, envía `UpdateMessage::Available` y espera la decisión del usuario a través de un canal `mpsc::Sender<bool>`.
- Al aceptar, usa el **portal Flatpak de ashpd** (`Flatpak::create_update_monitor`) para descargar e instalar la actualización directamente desde el repositorio remoto firmado. Muestra el progreso con `UpdateMessage::Progress { percent }`.
- Tras instalar, reinicia la app mediante `flatpak.spawn()` con la bandera `LATEST_VERSION`. Si el usuario instaló desde un bundle manual (no desde el remoto), el portal no puede actualizar y muestra un error indicando que debe instalarse desde el remoto de Freeway Player.
- `src/main.rs` integra el sistema: `start_update_check()` inicia el hilo de verificación, `poll_update_check()` procesa los mensajes en el loop principal. Un diálogo de confirmación (`Rmessage_box` con `R_MESSAGE_YES_NO`) pregunta al usuario antes de instalar. La bandera `restart_requested` sale del loop principal cuando la actualización completa.
- El menú del botón del logo (`update_menu`) contiene "Buscar actualizaciones" y "Acerca de".
- Diálogo "Acerca de" (`open_about_dialog`): ventana modal con icono de la app (64×64), título "Freeway Player", versión desde `CARGO_PKG_VERSION`, autor "Creado por Daniel Álvarez", enlace a GitHub, tecnología "Escrito en Rust con Raven y rodio", licencia GNU GPL v3.0 con copyright y nota de software libre, y botón "Aceptar" para cerrar.
- Dependencias añadidas: `ashpd` (portal Flatpak), `futures-util` (streams async), `tokio` (runtime async), `ureq` (HTTP), `semver` (comparación de versiones). Se eliminó `sha2` (ya no se verifica checksum manualmente).

### Distribución y GitHub Actions

- `.github/workflows/flatpak-release.yml`: workflow que se dispara al crear un tag `v*`. Verifica que el tag coincide con la versión de `Cargo.toml`, construye el repositorio Flatpak con `flatpak-builder`, firma con GPG (secretos `FLATPAK_GPG_PRIVATE_KEY` y `FLATPAK_GPG_KEY_ID`), genera archivos `.flatpakref` y `.flatpakrepo`, crea un bundle `.flatpak` firmado, despliega el repositorio en **GitHub Pages** (`https://daniel-a-g.github.io/Freeway-Player-R/`) y publica una GitHub Release con los assets.
- El repositorio Flatpak remoto se sirve desde GitHub Pages. Los usuarios pueden añadirlo con `flatpak remote-add` o hacer clic en el enlace de instalación desde la página web.
- La clave GPG para firmar usa el ID `E11C8395A1DDE608` (configurada como secreto en GitHub).
- Para la próxima versión, solo hay que actualizar `Cargo.toml`, hacer commit y push, y crear y publicar el tag `vX.Y.Z`. El workflow se encarga de todo automáticamente.
- El environment `github-pages` en GitHub tiene las restricciones de branch policy desactivadas para permitir deployments desde tags.

## Diseño y tecnologías

- Raven es la biblioteca GUI en C; la interfaz se describe en `ui/main.xml`.
- Los bindings Rust están en `vendor/raven-gui` y se importan como `raven`. Corresponden a Raven `v0.2.1`; no reemplazarlos por un paquete homónimo de crates.io sin comprobar que sea el mismo proyecto/API.
- `rodio` decodifica/reproduce audio y CPAL proporciona salida. En Linux se intenta PulseAudio primero y ALSA después.
- Formatos de audio configurados: MP3, FLAC, WAV, Ogg/Vorbis, Opus mono/estéreo y M4A/MP4. Ogg/Vorbis y Opus usan Symphonia; Opus se integra mediante `symphonia-adapter-libopus`.
- Aleatorio y repetición se configuran y persisten por lista. El orden aleatorio recorre las pistas antes de barajar un nuevo ciclo; la repetición reanuda la lista al terminar.
- El panel lateral muestra la carátula de la pista actual o un distintivo genérico. El editor de metadatos se abre desde el menú contextual (clic derecho) de una pista y permite reemplazar o quitar la carátula incrustada con Lofty.
- Lofty lee y escribe metadatos; la UI edita título, artista, álbum, género y número de pista. Guardar modifica el archivo de audio original. Algunos formatos/etiquetas pueden ser de solo lectura.
- Raven se inicia con Vulkan. La UI busca `FREEWAY_UI_FILE`; si no existe, usa `ui/main.xml` desde el directorio actual y finalmente el XML embebido.
- El tema no requiere imágenes ni efectos de composición adicionales; el acento, fondos y controles se dibujan con colores sólidos de Raven.

## Funciones

- Crear, cambiar, renombrar y cerrar pestañas de listas; la última lista no se puede cerrar.
- Añadir archivos de audio a la lista activa, quitar pistas de la lista sin borrar los archivos y reproducir la pista seleccionada.
- Las listas, sus rutas, selección y pestaña activa se guardan en `$XDG_CONFIG_HOME/freeway-player/playlists.json` o `~/.config/freeway-player/playlists.json`.
- Los controles de transporte usan símbolos Unicode con tooltips; volumen y posición se pueden ajustar.
- `rfd` abre el selector del portal para pistas y carátulas; el diálogo es el del escritorio y las rutas elegidas quedan disponibles al Flatpak mediante el permiso del portal.
- `data/fonts/` contiene las fuentes fallback Noto y su licencia OFL; `data/action-icons.png` contiene los iconos monocromos de aleatorio y repetición.
- Si una pista no se puede leer, el avance manual y automático intenta saltar a otra pista reproducible.

## Compilar y ejecutar en el entorno actual

Desde `/home/pruebas/Documentos/Freeway Player`:

```sh
RAVEN_LIB_DIR="$PWD/.local/raven/lib" \
LD_LIBRARY_PATH="$PWD/.local/raven/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
cargo run
```

Hay que pasar esas variables en cada invocación si no están exportadas en el shell. Dependencias de compilación del host: Rust/Cargo, `pkg-config`, cabeceras ALSA y PulseAudio, CMake/Ninja y toolchain C para Raven; también cabeceras X11/Vulkan y Vulkan disponible al ejecutar. En Debian/Ubuntu, instalar `pkg-config`, `libasound2-dev` y `libpulse-dev`.

## Archivos y mantenimiento

- `src/main.rs`: estado de reproducción y listas, callbacks, persistencia JSON, lectura/escritura de metadatos, actualización de UI, sistema de actualizaciones (integración con `updates.rs`) y diálogo "Acerca de".
- `src/updates.rs`: módulo de comprobación e instalación de actualizaciones. Consulta GitHub Releases, compara versiones con `semver`, usa el portal Flatpak de `ashpd` para instalar desde el remoto firmado y reinicia la app tras actualizar.
- `ui/main.xml`: estructura visual. Si se renombran controles, actualizar `Ui::from_window` y sus acciones en `src/main.rs`.
- `ui/theme.xml`: paleta oscura lila y estilos opacos; se puede sustituir en desarrollo con `FREEWAY_THEME_FILE`.
- `Cargo.toml` / `Cargo.lock`: dependencias Rust; Lofty, Serde y Serde JSON para editor y persistencia; `rfd` para el selector XDG Portal; `ashpd`, `futures-util`, `tokio`, `ureq` y `semver` para actualizaciones automáticas.
- `vendor/raven-gui`: bindings locales de Raven.
- `icon.png`: icono de la aplicación, incluido en el ejecutable y en el lanzador Flatpak (`hicolor/256x256`).
- `packaging/io.github.freewayplayer.FreewayPlayer.yml`: manifiesto Flatpak Freedesktop 25.08. El acceso a `home` es de lectura/escritura para reproducir rutas guardadas y editar etiquetas; el selector usa el portal para abrir también ubicaciones externas seleccionadas por el usuario. Para publicar en Flathub aún hay que generar `cargo-sources.json` y retirar `--share=network`.
- `.github/workflows/flatpak-release.yml`: workflow de GitHub Actions para construir, firmar con GPG, desplegar en GitHub Pages y publicar releases. Se dispara al crear un tag `v*`.
- `freeway-player.flatpak`: bundle local x86_64 actualizado desde el repositorio Flatpak `repo/`; se puede instalar o actualizar con `flatpak install --user --bundle --or-update ./freeway-player.flatpak`. El bundle y `repo/` son artefactos locales ignorados por Git.
- `.local/raven`: instalación local de Raven para desarrollo; Cargo no la descarga.
- `README.md`: descripción, dependencias y pasos de compilación/empaquetado.
- `LICENSE`: licencia GNU GPL v3.0 completa.
- `context.md`: este archivo, resumen del proyecto para asistentes de IA.

