# Contexto del proyecto

Resumen rápido para asistentes de IA. Actualizado el 2026-11-07.

## Estado actual

- Freeway Player es un reproductor local de música para Linux escrito en Rust.
- Se rediseñó la interfaz para organizar el uso alrededor de listas con pestañas, una biblioteca seleccionable, controles con símbolos y tooltips, barra de búsqueda de posición y un editor de etiquetas.
- `cargo check` y `cargo build` terminaron correctamente después de los cambios. No se ejecutaron pruebas ni se abrió la ventana nueva para inspección visual.
- El usuario confirmó que la versión anterior ya funcionaba en su equipo.
- Raven está instalado localmente en `.local/raven`; existen enlaces simbólicos `libraven.so` y `libraven.so.0` hacia `libraven.so.0.2.1`.

## Diseño y tecnologías

- Raven es la biblioteca GUI en C; la interfaz se describe en `ui/main.xml`.
- Los bindings Rust están en `vendor/raven-gui` y se importan como `raven`. Corresponden a Raven `v0.2.1`; no reemplazarlos por un paquete homónimo de crates.io sin comprobar que sea el mismo proyecto/API.
- `rodio` decodifica/reproduce audio y CPAL proporciona salida. En Linux se intenta PulseAudio primero y ALSA después.
- Formatos de audio configurados: MP3, FLAC, WAV, Ogg/Vorbis, Opus mono/estéreo y M4A/MP4. Ogg/Vorbis y Opus usan Symphonia; Opus se integra mediante `symphonia-adapter-libopus`.
- Aleatorio y repetición se configuran y persisten por lista. El orden aleatorio recorre las pistas antes de barajar un nuevo ciclo; la repetición reanuda la lista al terminar.
- El panel lateral muestra la carátula de la pista actual o un distintivo genérico. El editor de metadatos se abre desde el menú contextual (clic derecho) de una pista y permite reemplazar o quitar la carátula incrustada con Lofty.
- Lofty lee y escribe metadatos; la UI edita título, artista, álbum, género y número de pista. Guardar modifica el archivo de audio original. Algunos formatos/etiquetas pueden ser de solo lectura.
- Raven se inicia con Vulkan. La UI busca `FREEWAY_UI_FILE`; si no existe, usa `ui/main.xml` desde el directorio actual y finalmente el XML embebido.

## Funciones

- Crear, cambiar, renombrar y cerrar pestañas de listas; la última lista no se puede cerrar.
- Añadir archivos de audio a la lista activa, quitar pistas de la lista sin borrar los archivos y reproducir la pista seleccionada.
- Las listas, sus rutas, selección y pestaña activa se guardan en `$XDG_CONFIG_HOME/freeway-player/playlists.json` o `~/.config/freeway-player/playlists.json`.
- Los controles de transporte usan símbolos Unicode con tooltips; volumen y posición se pueden ajustar.
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

- `src/main.rs`: estado de reproducción y listas, callbacks, persistencia JSON, lectura/escritura de metadatos y actualización de UI.
- `ui/main.xml`: interfaz. Si se renombran controles, actualizar `Ui::from_window` y sus acciones.
- `Cargo.toml` / `Cargo.lock`: dependencias Rust; Lofty, Serde y Serde JSON se añadieron para editor y persistencia.
- `vendor/raven-gui`: bindings locales de Raven.
- `packaging/io.github.freewayplayer.FreewayPlayer.yml`: Flatpak Freedesktop 25.08. El acceso a `home` ahora es de lectura/escritura para que el selector funcione y se puedan guardar etiquetas; evaluar una integración con portal si se quiere limitar ese permiso. Para Flathub aún hay que generar `cargo-sources.json` y retirar `--share=network`.
- `.local/raven`: instalación local de Raven para desarrollo; Cargo no la descarga.
- `README.md`: descripción, dependencias y pasos de compilación/empaquetado.

