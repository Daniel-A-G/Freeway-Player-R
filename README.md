# Freeway Player

Reproductor local de música para Linux escrito en Rust. La interfaz está descrita en `ui/main.xml` con Raven; reproduce MP3, FLAC, WAV, Ogg/Vorbis, Opus mono/estéreo y M4A. Opus y Vorbis en contenedor Ogg se decodifican en streaming. Permite organizar pistas en listas con pestañas y editar sus etiquetas.

## Por qué Raven

Raven es la biblioteca C de la documentación indicada, con bindings oficiales para Rust (`raven-gui`, importado como `raven`). Sus helpers `window_from_xml`, `action_bind` y `widget_find` permiten conservar el XML de Raven y mantener la lógica del reproductor en Rust.

Los bindings se incluyen en `vendor/raven-gui` desde la etiqueta `v0.2.1` de Raven. El paquete homónimo publicado en crates.io pertenece a otro proyecto, así que la dependencia usa la copia local para mantenerla alineada con la biblioteca C.

RavenRS es otro proyecto: una GUI nativa implementada en Rust con vistas declarativas y renderizado `wgpu`. Es una buena opción si se busca una interfaz completamente Rust, pero no carga el XML descrito en este proyecto. Para este requisito Raven encaja mejor; RavenRS no es una versión Rust del mismo API.

## Funciones

- Abrir una o varias pistas con el selector de archivos de Raven.
- Mantener varias listas en pestañas, añadir/quitar pistas y conservar las listas entre sesiones en `$XDG_CONFIG_HOME/freeway-player/playlists.json` (o `~/.config/freeway-player/playlists.json`).
- Reproducir, pausar, detener, avanzar y retroceder por la lista activa con controles de iconos.
- Activar aleatorio y repetición por lista; aleatorio recorre las pistas en un orden mezclado antes de iniciar un nuevo ciclo. Las opciones se guardan junto a cada lista.
- Cambiar volumen y posición desde la barra de progreso, con tiempo transcurrido y duración.
- Mostrar la carátula de la pista en reproducción, con un distintivo genérico si no hay imagen. El menú contextual de cada pista abre el editor de título, artista, álbum, género, número de pista y carátula; los cambios se incrustan en el archivo.
- Reproducción automática de la siguiente pista.
- Interfaz XML recargable mientras se desarrolla: `FREEWAY_UI_FILE` permite elegir el fichero; si no existe, se usa `ui/main.xml` desde el directorio actual. El XML también se incluye dentro del ejecutable como respaldo.

## Compilar y ejecutar en Linux

Además de Rust/Cargo, Raven requiere CMake, Ninja, un compilador C11, cabeceras X11, cabeceras Vulkan y una biblioteca Vulkan disponible al ejecutar. Rodio usa CPAL para la salida; en Linux la compilación requiere cabeceras ALSA (`libasound2-dev` en Debian/Ubuntu). Esta app también habilita el backend PulseAudio para usar el socket estándar de Flatpak, que requiere `libpulse-dev` al compilar.

Compila e instala Raven como biblioteca compartida:

```sh
git clone --depth 1 --branch v0.2.1 https://github.com/oscar-soirey/Raven.git /tmp/Raven
cmake -S /tmp/Raven -B /tmp/Raven/build -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_INSTALL_PREFIX="$PWD/.local/raven" \
  -DRAVEN_BUILD_SHARED=ON \
  -DRAVEN_BUILD_EXAMPLES=OFF \
  -DRAVEN_BUILD_TESTS=OFF \
  -DRAVEN_BUILD_TOOLS=OFF
cmake --build /tmp/Raven/build --parallel
cmake --install /tmp/Raven/build
```

Luego ejecuta desde la raíz del proyecto:

```sh
RAVEN_LIB_DIR="$PWD/.local/raven/lib" \
LD_LIBRARY_PATH="$PWD/.local/raven/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}" \
cargo run
```

Si el sistema no instala Vulkan headers en una ruta que CMake detecte, indica `-DRAVEN_VULKAN_INCLUDE_DIR=/ruta/a/include` al configurar Raven; ese directorio debe contener `vulkan/vulkan.h`.

## Flatpak

El manifiesto está en `packaging/io.github.freewayplayer.FreewayPlayer.yml`. Usa el runtime Freedesktop 25.08, compila Raven y luego el binario Rust. Se puede instalar localmente con:

```sh
flatpak-builder --user --install --install-deps-from=flathub --force-clean build-dir \
  packaging/io.github.freewayplayer.FreewayPlayer.yml
```

El manifiesto requiere el runtime/SDK Freedesktop 25.08 y la extensión Rust estable de esa misma rama; `--install-deps-from=flathub` hace que Flatpak Builder los instale si faltan.

El manifiesto concede acceso a `home` para que el selector integrado de Raven explore música local y el editor pueda guardar etiquetas en los archivos, además de Vulkan/DRI y el socket de audio. Esto permite leer y modificar archivos de la carpeta personal; una unidad montada fuera de ella requerirá conceder acceso a esa ruta o integrar un selector basado en el portal de archivos. `Cargo.lock` ya está incluido; para publicar en Flathub aún habrá que generar `cargo-sources.json` con Flatpak Builder Tools y quitar `--share=network` del módulo de Cargo, porque las compilaciones de Flathub deben resolver dependencias desde fuentes declaradas y fijadas.

## RavenRS

RavenRS no sustituye a `raven-gui`: es un proyecto independiente. Sus componentes se construyen con el API Rust y no se conectan a `ui/main.xml`. Esta app usa Raven para cumplir el requisito XML. Rodio tampoco elimina todos los requisitos de sistema: su decodificación es Rust, pero la salida Linux pasa por CPAL. La app intenta usar PulseAudio (incluido el servidor compatible de PipeWire) y luego ALSA; para compilar necesita las cabeceras ALSA y PulseAudio.
