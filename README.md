# Freeway Player

> **Aviso:** este proyecto tiene un alto uso de código generado con ayuda de IA. Revisa el código y las pruebas antes de confiar en él o redistribuirlo.

Reproductor local de música para Linux escrito en Rust. La interfaz está descrita en `ui/main.xml` con Raven; reproduce MP3, FLAC, WAV, Ogg/Vorbis, Opus mono/estéreo y M4A. Opus y Vorbis en contenedor Ogg se decodifican en streaming. Permite organizar pistas en listas con pestañas y editar sus etiquetas.

## Por qué Raven

Raven es la biblioteca C de la documentación indicada, con bindings oficiales para Rust (`raven-gui`, importado como `raven`). Sus helpers `window_from_xml`, `action_bind` y `widget_find` permiten conservar el XML de Raven y mantener la lógica del reproductor en Rust.

Los bindings se incluyen en `vendor/raven-gui` desde la etiqueta `v0.2.1` de Raven. El paquete homónimo publicado en crates.io pertenece a otro proyecto, así que la dependencia usa la copia local para mantenerla alineada con la biblioteca C.

## Funciones

- Interfaz oscura y opaca con acento lila, superficies planas y separadores sutiles; biblioteca, carátula/editor y controles forman una sola composición sin paneles tipo tarjeta.
- El icono `icon.png` se incrusta en el binario para la ventana y el encabezado, y se instala para el lanzador Flatpak.
- El logo abre el menú de la aplicación. La primera opción, **Ajustes**, abre una ventana con un menú vertical a la izquierda: **General** (comportamiento al abrir archivos), **Listas** (comportamiento al añadir pistas), **YouTube** (streaming o descarga completa) y **Descargas** (carpeta de destino). En **General**, al abrir un archivo con la ventana ya abierta puedes elegir **Añadir a Sesión y reproducir** (predeterminado), **Añadir a Sesión sin cambiar la reproducción** o **Reemplazar Sesión y reproducir**. En **Listas**, al añadir pistas desde la propia app, arrastrándolas a la ventana o con «+ Añadir pistas», puedes elegir **Añadir y reproducir la primera pista si no hay nada sonando** (predeterminado) o **Añadir solo a la lista, sin iniciar ni cambiar la reproducción**; esta última deja intactas la reproducción, la selección y la cola, y añade las pistas nuevas al final. **Aceptar** aplica los cambios y cierra la ventana, **Aplicar** los guarda sin cerrarla y **Cancelar** (o Escape) descarta solo los cambios que aún no se han aplicado. Todo se guarda en `$XDG_CONFIG_HOME/freeway-player/settings.json`. El menú también permite buscar una release nueva; tras confirmarlo, el portal Flatpak instala la actualización desde el remoto de Freeway Player y relanza la aplicación.
- La categoría **Descargas** de Ajustes permite elegir la carpeta donde se guardarán las pistas que decidas descargar desde YouTube. Mientras no se elija ninguna se usa `XDG_DOWNLOAD_DIR` o `~/Downloads`. Buscar no escribe ningún archivo y, con **Streaming** (el ajuste predeterminado en **YouTube**), escuchar un resultado tampoco: el audio AAC se lee directamente del pipe de `yt-dlp`. Solo con **Descarga completa** se crea un archivo Opus temporal en la caché de la app, que se sustituye al cambiar de resultado. La opción **Descargar en Opus** del menú contextual guarda el audio en esa carpeta (sin sobrescribir: añade `(1)`, `(2)`, … si ya existe).
- Los símbolos de controles y las notas musicales usan fuentes Noto incluidas en el programa; aleatorio, repetición, el botón de pista al azar y el botón de YouTube usan iconos monocromos propios. Así no dependen de las fuentes o temas de iconos instalados en cada distribución.
- Abrir una o varias pistas con el selector del escritorio. En Linux usa XDG Desktop Portal, de modo que el diálogo puede mostrar las ubicaciones fijadas en GTK/KDE y dar acceso a archivos elegidos fuera del espacio concedido directamente al Flatpak.
- El lanzador declara los tipos de audio compatibles (`MimeType`) y acepta rutas o URIs `%U`, así que puede elegirse como reproductor predeterminado. La aplicación usa una sola instancia: si ya está abierta, los archivos se envían a la ventana existente, que se trae al frente, en lugar de abrir otra. Los archivos abiertos desde el gestor o con «Abrir con» van a la lista **Sesión**, que se reproduce al momento según el ajuste elegido y se distingue con un fondo rojo suave en su pestaña. No se guarda al cerrar: renómbrala para conservarla como lista normal.
- Mantener varias listas en pestañas, añadir/quitar pistas y conservar las listas entre sesiones en `$XDG_CONFIG_HOME/freeway-player/playlists.json` (o `~/.config/freeway-player/playlists.json`).
- Reordenar pistas arrastrándolas dentro de la lista: una línea guía salta entre huecos para mostrar el destino, la lista se desplaza sola al arrastrar contra los bordes y la pista queda en el hueco indicado al soltar.
- Reordenar las listas arrastrando sus pestañas: una guía vertical marca el hueco de destino y el nuevo orden se conserva entre sesiones.
- Reproducir, pausar, detener, avanzar y retroceder por la lista activa con controles de iconos. Play arranca la pista seleccionada (también si suena otra) y detener reinicia la barra de tiempo a cero, dejando la vista como recién abierta.
- Activar aleatorio y repetición por lista (icono de infinito); aleatorio recorre las pistas en un orden mezclado antes de iniciar un nuevo ciclo. Cada vez que la cola vuelve a empezar se baraja de nuevo, con un historial limpio, en lugar de repetir siempre el mismo orden. Las opciones, el orden mezclado y la posición de la cola se guardan junto a cada lista, así que al volver a abrir la app la reproducción aleatoria continúa donde se dejó en lugar de empezar de cero. El resumen bajo «Biblioteca» muestra la posición de la cola (`15/30 · 30 pistas · …`) mientras suene esa lista con el aleatorio activo.
- El botón de dados a la derecha de «Biblioteca» y del recuento de pistas, antes de «+ Añadir pistas», reproduce una pista al azar de la lista activa. Funciona tanto con aleatorio activo como sin él (no tiene que ver con el modo aleatorio): si ya suena una pista, elige otra distinta.
- El botón con el logo de YouTube en la esquina inferior izquierda abre el buscador de YouTube: la lista queda desactivada mientras se busca y la cabecera pasa a «YouTube». Elegir cualquier pestaña de listas vuelve a la biblioteca, incluso si es la que ya estaba activa antes de entrar. El campo de búsqueda solo está visible mientras el modo YouTube está activo: aparece enfocado, el logo se tiñe con el acento lila y las pestañas se atenúan para que la lista anterior no parezca la seleccionada. Este modo es solo un buscador: los resultados no se guardan ni se añaden a las listas; descargar una pista o añadirla a una lista en streaming (guardando solo la URL) serán acciones explícitas posteriores. La búsqueda se hace en un hilo aparte con `yt-dlp` (hasta 10 resultados, con título, canal y duración) y los resultados se descartan al volver a la biblioteca o al cambiar de lista. Doble clic o Play reproducen solo el audio del resultado seleccionado, sin tocar la lista activa: por defecto `yt-dlp` entrega el m4a/AAC original por stdout y `rodio` empieza a decodificarlo en cuanto llegan los primeros bytes; si el vídeo no ofrece ese formato o falla la red, se cae automáticamente a la descarga completa en Opus. La categoría **YouTube** de Ajustes permite elegir **Streaming** (predeterminado, empieza a sonar antes) o **Descarga completa** (espera a bajar y convertir el Opus, y permite avanzar dentro de la pista). La carátula del panel muestra la miniatura del resultado seleccionado o, si no hay selección, del que está sonando. Siguiente y Anterior recorren los resultados, y al terminar uno se pasa al siguiente. El menú contextual de los resultados cambia según el modo: en el buscador ofrece **Descargar en Opus** (guarda el audio en la carpeta de descargas de Ajustes) y **Añadir a lista de reproducción (Streaming)** (guarda solo la URL y los metadatos básicos en la lista activa; al reproducirlo respeta el ajuste de **YouTube**: pipe AAC en streaming u Opus en caché con la descarga completa); en las listas normales sigue ofreciendo **Editar metadatos** y **Eliminar de la lista**. Buscar nunca escribe nada: descargar o añadir a la lista son acciones explícitas del usuario. Las pistas añadidas en streaming se distinguen en la lista con `▶` antes del título y `(YouTube)` junto al artista, para no confundirlas con archivos locales.
- Cambiar volumen y posición desde la barra de progreso, con tiempo transcurrido y duración. El volumen se recuerda entre sesiones en `$XDG_CONFIG_HOME/freeway-player/settings.json`. En el streaming de YouTube la barra sigue informando de la posición, pero no admite arrastre: el aviso «El streaming de YouTube no permite cambiar la posición» aparece al intentarlo; con **Descarga completa** el seek funciona con normalidad.
- Aparecer como reproductor del sistema mediante MPRIS2: el área de notificaciones, KDE Connect y otros clientes pueden consultar la pista actual y su carátula, y controlar reproducción, volumen, posición, aleatorio y repetición. No publica la lista de pistas por D-Bus. Si no hay bus de sesión, la reproducción local sigue funcionando igual.
- Mostrar la carátula de la pista en reproducción, con un distintivo genérico si no hay imagen. Al detener, el panel vuelve al estado inicial y muestra la carátula de la pista seleccionada, de modo que se puede explorar la lista con sus imágenes sin reproducirlas. El menú contextual de cada pista abre el editor de título, artista, álbum, género, número de pista y carátula; los cambios se incrustan en el archivo. El selector de carátula recuerda la última carpeta usada entre sesiones en `$XDG_CONFIG_HOME/freeway-player/settings.json`; si el portal de documentos entrega una ruta efímera (`/run/user/<uid>/doc/…`), se guarda la carpeta real del archivo.
- Reparar archivos de audio con etiquetas corruptas: si guardar los metadatos falla, la aplicación ofrece repararlos con `ffmpeg` (remux con `-c copy`, sin recodificar) en la misma ruta. Antes guarda una copia del original en `$XDG_CACHE_HOME/freeway-player/repairs/` y deja la pista pendiente, marcada con `⚠` en todas las listas. El editor muestra entonces un aviso con dos acciones: restaurar el original o confirmar la versión reparada como definitiva. Las listas conservan el orden porque el archivo no cambia de ruta.
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

## Control MPRIS2

Al iniciarse, la aplicación publica en el bus de sesión el nombre `org.mpris.MediaPlayer2.io.github.freewayplayer.FreewayPlayer` y las interfaces `org.mpris.MediaPlayer2` y `org.mpris.MediaPlayer2.Player` en `/org/mpris/MediaPlayer2`. Expone `PlaybackStatus`, `Metadata` (título, artista, álbum, género, número de pista, URL, duración y carátula), `Volume`, `Position`, `Shuffle` y `LoopStatus`, además de las señales `PropertiesChanged` y `Seeked`. Acepta `Play`, `Pause`, `Stop`, `Next`, `Previous`, `Seek` y `SetPosition`, y permite escribir `Volume`, `Shuffle` y `LoopStatus`. `LoopStatus` solo distingue `None` de `Playlist` porque la app no tiene repetición de pista.

La carátula incrustada se extrae a `$XDG_CACHE_HOME/freeway-player/mpris/cover.<ext>` y se anuncia como `mpris:artUrl`; solo se reescribe al cambiar de pista o de carátula. En Flatpak ese directorio equivale a `~/.var/app/io.github.freewayplayer.FreewayPlayer/cache/freeway-player/mpris/`, legible por clientes del host gracias a `--filesystem=home`.

El servicio corre en su propio hilo y solo recibe un snapshot; las órdenes se aplican en el hilo de la interfaz, igual que los botones. Si D-Bus no está disponible, se imprime un aviso por stderr y el reproductor continúa sin control externo.

En Flatpak no hace falta permiso adicional: la política por defecto permite a una app poseer `org.mpris.MediaPlayer2.$FLATPAK_ID`. Eso sí, un bundle instalado antes de esta función debe recompilarse y reinstalarse para incluirla.

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

El workflow `.github/workflows/flatpak-release.yml` compila un bundle x86_64 y crea un GitHub Release al publicar un tag `v*`. El tag debe coincidir con la versión de `Cargo.toml`; la versión actual es `0.4.1`, así que el siguiente tag a publicar es `v0.4.1`, por ejemplo:

```sh
git tag -a v0.4.1 -m "Freeway Player v0.4.1"
git push origin main v0.4.1
```

Conviene subir `main` junto con el tag para que la rama remota quede alineada con el Release.

El Release incluye el `.flatpak`, su checksum SHA-256 y `BUILD-INFO.txt` con el tag y el commit exacto del código fuente. El bundle se construye en GitHub Actions y se adjunta al Release automáticamente; no hace falta configurar un token personal, porque el workflow usa `GITHUB_TOKEN` con permiso de contenido para publicar el Release.

El manifiesto requiere el runtime/SDK Freedesktop 25.08 y la extensión Rust estable de esa misma rama; `--install-deps-from=flathub` hace que Flatpak Builder los instale si faltan. El buscador de YouTube usará `yt-dlp`: el manifiesto instala su zipapp (`2026.10.26`, ~2,9 MiB) en `/app/bin/yt-dlp`, fijado por SHA-256, junto con sus licencias. No hace falta instalar nada en el host: dentro del sandbox el `PATH` es `/app/bin:/usr/bin` y el runtime ya trae Python 3.13 y ffmpeg. Para compilaciones nativas basta con tener `yt-dlp` en el `PATH` (o señalar uno concreto con `FREEWAY_YTDLP`). Con el ajuste **Streaming**, la reproducción usa `yt-dlp -f bestaudio[ext=m4a] -o -` y `rodio` decodifica el AAC/m4a directamente desde el pipe (ya hay soporte MP4/AAC en las dependencias); no se escribe ningún archivo. Con **Descarga completa**, o si el streaming falla, se usa `yt-dlp --extract-audio --audio-format opus` y el archivo va a `$XDG_CACHE_HOME/freeway-player/youtube-audio/`, donde `rodio` lo lee con el decodificador Ogg/Opus de siempre. No hace falta iniciar sesión ni configurar una cuenta para buscar ni para reproducir vídeos públicos. Si YouTube cambia y el extractor deja de funcionar, habrá que actualizar la versión fijada y publicar una nueva build.

El manifiesto concede acceso de lectura/escritura a `home` para reproducir rutas guardadas y editar etiquetas, además de acceso de red para consultar releases, instalar actualizaciones desde el remoto, Vulkan/DRI y el socket de audio. Los selectores de pistas y carátulas usan XDG Desktop Portal: el diálogo del escritorio puede mostrar los marcadores del usuario y navegar enlaces a carpetas, y permite seleccionar archivos fuera de `home` sin añadir permisos globales de acceso. `Cargo.lock` ya está incluido; para publicar en Flathub aún habrá que generar `cargo-sources.json` con Flatpak Builder Tools y quitar `--share=network` del módulo de Cargo, porque las compilaciones de Flathub deben resolver dependencias desde fuentes declaradas y fijadas.
