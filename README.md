# Freeway Player

Reproductor local de música para Linux escrito en Rust. La interfaz está descrita en `ui/main.xml` con Raven; reproduce MP3, FLAC, WAV, Ogg/Vorbis, Opus mono/estéreo y M4A. Opus y Vorbis en contenedor Ogg se decodifican en streaming. Permite organizar pistas en listas con pestañas y editar sus etiquetas.

## Por qué Raven

Raven es la biblioteca C de la documentación indicada, con bindings oficiales para Rust (`raven-gui`, importado como `raven`). Sus helpers `window_from_xml`, `action_bind` y `widget_find` permiten conservar el XML de Raven y mantener la lógica del reproductor en Rust.

Los bindings se incluyen en `vendor/raven-gui` desde la etiqueta `v0.2.1` de Raven. El paquete homónimo publicado en crates.io pertenece a otro proyecto, así que la dependencia usa la copia local para mantenerla alineada con la biblioteca C.

## Funciones

- Interfaz oscura y opaca con acento lila, superficies planas y separadores sutiles; biblioteca, carátula/editor y controles forman una sola composición sin paneles tipo tarjeta.
- El icono `icon.png` se incrusta en el binario para la ventana y el encabezado, y se instala para el lanzador Flatpak.
- El logo abre un menú para buscar una release nueva. Tras confirmarlo, el portal Flatpak instala la actualización desde el remoto de Freeway Player y relanza la aplicación.
- Los símbolos de controles y las notas musicales usan fuentes Noto incluidas en el programa; aleatorio y repetición usan iconos monocromos propios. Así no dependen de las fuentes o temas de iconos instalados en cada distribución.
- Abrir una o varias pistas con el selector del escritorio. En Linux usa XDG Desktop Portal, de modo que el diálogo puede mostrar las ubicaciones fijadas en GTK/KDE y dar acceso a archivos elegidos fuera del espacio concedido directamente al Flatpak.
- Mantener varias listas en pestañas, añadir/quitar pistas y conservar las listas entre sesiones en `$XDG_CONFIG_HOME/freeway-player/playlists.json` (o `~/.config/freeway-player/playlists.json`).
- Reproducir, pausar, detener, avanzar y retroceder por la lista activa con controles de iconos.
- Activar aleatorio y repetición por lista; aleatorio recorre las pistas en un orden mezclado antes de iniciar un nuevo ciclo. Las opciones se guardan junto a cada lista.
- Cambiar volumen y posición desde la barra de progreso, con tiempo transcurrido y duración.
- Mostrar la carátula de la pista en reproducción, con un distintivo genérico si no hay imagen. El menú contextual de cada pista abre el editor de título, artista, álbum, género, número de pista y carátula; los cambios se incrustan en el archivo.
- Reproducción automática de la siguiente pista.
- Interfaz XML recargable mientras se desarrolla: `FREEWAY_UI_FILE` permite elegir el fichero; si no existe, se usa `ui/main.xml` desde el directorio actual. El XML también se incluye dentro del ejecutable como respaldo.
- El tema se configura en `ui/theme.xml`; `FREEWAY_THEME_FILE` permite cargar otro tema durante el desarrollo. Si no se encuentra un fichero, se usa el tema incrustado en el ejecutable.

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

Para generar un archivo `.flatpak` que puedas compartir con otros equipos Linux, primero asegúrate de tener Flathub configurado y exporta la compilación a un repositorio local:

```sh
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
flatpak-builder --user --install-deps-from=flathub --force-clean --delete-build-dirs --repo=repo --default-branch=stable \
  build-dir packaging/io.github.freewayplayer.FreewayPlayer.yml
flatpak build-bundle repo freeway-player.flatpak io.github.freewayplayer.FreewayPlayer stable \
  --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo
```

En el equipo de destino se instala o actualiza con `flatpak install --user --bundle --or-update ./freeway-player.flatpak`. El bundle contiene la aplicación, no el runtime: el equipo necesita Flatpak y conexión a Flathub para descargar Freedesktop 25.08 si aún no está instalado. El paquete se compila para la arquitectura indicada por Flatpak Builder (por defecto, la del equipo que construye).

### Releases automáticos en GitHub

El workflow `.github/workflows/flatpak-release.yml` compila un bundle x86_64 y crea un GitHub Release al publicar un tag `v*`. El tag debe coincidir con la versión de `Cargo.toml`; por ejemplo, para la versión `0.1.0`:

```sh
git tag -a v0.1.0 -m "Freeway Player v0.1.0"
git push origin v0.1.0
```

El Release incluye el `.flatpak`, su checksum SHA-256 y `BUILD-INFO.txt` con el tag y el commit exacto del código fuente. El bundle se construye en GitHub Actions y se adjunta al Release automáticamente; no hace falta configurar un token personal, porque el workflow usa `GITHUB_TOKEN` con permiso de contenido para publicar el Release.

El manifiesto requiere el runtime/SDK Freedesktop 25.08 y la extensión Rust estable de esa misma rama; `--install-deps-from=flathub` hace que Flatpak Builder los instale si faltan.

El manifiesto concede acceso de lectura/escritura a `home` para reproducir rutas guardadas y editar etiquetas, además de acceso de red para consultar releases, instalar actualizaciones desde el remoto, Vulkan/DRI y el socket de audio. Los selectores de pistas y carátulas usan XDG Desktop Portal: el diálogo del escritorio puede mostrar los marcadores del usuario y navegar enlaces a carpetas, y permite seleccionar archivos fuera de `home` sin añadir permisos globales de acceso. `Cargo.lock` ya está incluido; para publicar en Flathub aún habrá que generar `cargo-sources.json` con Flatpak Builder Tools y quitar `--share=network` del módulo de Cargo, porque las compilaciones de Flathub deben resolver dependencias desde fuentes declaradas y fijadas.
