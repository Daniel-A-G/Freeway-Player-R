//! Lógica pura del conversor FLAC → Opus.
//!
//! Esta capa no llama a Raven ni a `ffmpeg`: decide la calidad, el nombre de
//! salida, qué hacer cuando el `.opus` ya existe y cómo se refleja el cambio en
//! las listas. Así se puede probar sin equipo de audio ni archivos reales.

use std::collections::HashSet;
use std::ffi::{c_int, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

/// Preset mínimo 96 kbps; 160 kbps sigue siendo el recomendado.
pub const PRESET_BITRATES: [u32; 4] = [96, 128, 160, 192];
pub const RECOMMENDED_BITRATE: u32 = 160;
pub const CUSTOM_MIN: u32 = 6;
pub const CUSTOM_MAX: u32 = 510;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    Preset(u32),
    Custom(u32),
}

impl Default for Quality {
    fn default() -> Self {
        Quality::Preset(RECOMMENDED_BITRATE)
    }
}

/// Acción elegida cuando ya existe el `.opus` de destino.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CollisionPolicy {
    Overwrite,
    Rename,
    /// «Cancelar» en la UI: se omite ese archivo y el lote sigue con los demás.
    Skip,
}

pub fn quality_from_option(index: usize, custom_bitrate: u32) -> Option<Quality> {
    PRESET_BITRATES
        .get(index)
        .copied()
        .map(Quality::Preset)
        .or_else(|| (index == PRESET_BITRATES.len()).then_some(Quality::Custom(custom_bitrate)))
}

pub fn clamp_bitrate(bitrate: u32) -> u32 {
    bitrate.clamp(CUSTOM_MIN, CUSTOM_MAX)
}

impl Quality {
    pub fn bitrate(self) -> u32 {
        match self {
            Quality::Preset(bitrate) => bitrate,
            Quality::Custom(bitrate) => clamp_bitrate(bitrate),
        }
    }

    /// `libopus` usa VBR de forma predeterminada cuando se le da un `-b:a`.
    /// `-map_metadata 0` conserva las etiquetas de texto del FLAC.
    pub fn ffmpeg_args(self) -> Vec<String> {
        vec![
            "-c:a".to_owned(),
            "libopus".to_owned(),
            "-b:a".to_owned(),
            format!("{}k", self.bitrate()),
            "-map_metadata".to_owned(),
            "0".to_owned(),
        ]
    }
}

/// `cancion.FLAC` → `cancion.opus`, en la misma carpeta.
pub fn output_path(input: &Path) -> PathBuf {
    let mut output = input.to_path_buf();
    output.set_extension("opus");
    output
}

/// `cancion.opus` → `cancion-1.opus`, `cancion-2.opus`, …
pub fn renamed_output_path(desired: &Path, occupied: &[PathBuf]) -> PathBuf {
    let parent = desired.parent().unwrap_or_else(|| Path::new(""));
    let stem = desired.file_stem().unwrap_or_default().to_os_string();
    let extension = desired.extension().unwrap_or_default().to_os_string();

    for number in 1.. {
        let mut name = OsString::from(&stem);
        name.push(format!("-{number}"));
        let mut candidate = parent.join(name);
        candidate.set_extension(&extension);
        if !occupied.iter().any(|path| path == &candidate) {
            return candidate;
        }
    }

    unreachable!("la numeración nunca se agota")
}

pub fn is_flac(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("flac"))
}

/// Todos los `.opus` ya presentes en las carpetas de los FLAC seleccionados.
/// Se usan como ocupados para que «cambiar nombre» no repita un `-1` existente.
pub fn existing_outputs_in(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let directories: HashSet<PathBuf> = inputs
        .iter()
        .filter_map(|input| input.parent().map(Path::to_path_buf))
        .collect();
    let mut outputs = Vec::new();
    for directory in directories {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path
                .extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension.eq_ignore_ascii_case("opus"))
            {
                outputs.push(path);
            }
        }
    }
    outputs
}

/// Resultados del diálogo de colisión: sobrescribir, cambiar nombre u omitir.
pub fn collision_policy_from_choice(choice: c_int) -> Option<CollisionPolicy> {
    match choice {
        1 => Some(CollisionPolicy::Overwrite),
        2 => Some(CollisionPolicy::Rename),
        0 | 3 => Some(CollisionPolicy::Skip),
        _ => None,
    }
}

/// Resuelve la ruta final aplicando la política elegida ante una colisión.
pub fn resolve_output_path(
    input: &Path,
    occupied: &[PathBuf],
    policy: CollisionPolicy,
) -> Option<PathBuf> {
    let desired = output_path(input);
    if !occupied.iter().any(|path| path == &desired) {
        return Some(desired);
    }

    match policy {
        CollisionPolicy::Overwrite => Some(desired),
        CollisionPolicy::Rename => Some(renamed_output_path(&desired, occupied)),
        CollisionPolicy::Skip => None,
    }
}

/// Número de archivos del selector que aparecen en alguna lista.
/// Se cuentan archivos únicos, no repeticiones en listas distintas.
#[cfg(test)]
pub fn list_match_count
(inputs: &[PathBuf], lists: &[Vec<PathBuf>]) -> usize {
    let selected: HashSet<PathBuf> = inputs.iter().cloned().collect();
    let mut matched = HashSet::new();
    for list in lists {
        for path in list {
            if selected.contains(path) {
                matched.insert(path.clone());
            }
        }
    }
    matched.len()
}

/// Versión con datos auxiliares paralelos (en la app, la entrada de streaming).
/// Sustituye la ruta y conserva el payload de la entrada original.
pub fn replace_entries_in_lists<T: Clone>(
    lists: &mut [Vec<(PathBuf, T)>],
    replacements: &[(PathBuf, PathBuf)],
) -> usize {
    let mut changed = 0;
    for list in lists.iter_mut() {
        let previous = list.clone();
        let mut next = Vec::with_capacity(previous.len());
        for (path, payload) in &previous {
            if let Some((_, output)) = replacements.iter().find(|(source, _)| source == path) {
                if !previous.iter().any(|(existing, _)| existing == output)
                    && !next.iter().any(|(existing, _)| existing == output)
                {
                    next.push((output.clone(), payload.clone()));
                }
                changed += 1;
            } else {
                next.push((path.clone(), payload.clone()));
            }
        }
        *list = next;
    }
    changed
}

/// Sustituye `FLAC` por `OPUS` solo en las listas que contenían ese FLAC.
/// Devuelve cuántas entradas se han reemplazado.
#[cfg(test)]
pub fn replace_in_lists
(lists: &mut [Vec<PathBuf>], replacements: &[(PathBuf, PathBuf)]) -> usize {
    let mut entries: Vec<Vec<(PathBuf, ())>> = lists
        .iter()
        .map(|list| list.iter().map(|path| (path.clone(), ())).collect())
        .collect();
    let changed = replace_entries_in_lists(&mut entries, replacements);
    for (list, converted) in lists.iter_mut().zip(entries) {
        *list = converted.into_iter().map(|(path, _)| path).collect();
    }
    changed
}

/// El borrado del FLAC es el único paso irreversible: exige éxito total y
/// que la pista no esté sonando en ese momento.
pub fn can_delete_source(
    conversion_ok: bool,
    output_exists: bool,
    replacement_ok: bool,
    playing: bool,
) -> bool {
    conversion_ok && output_exists && replacement_ok && !playing
}

/// Un archivo ya resuelto: destino final y temporal único donde escribe ffmpeg.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConversionTask {
    pub source: PathBuf,
    pub output: PathBuf,
    pub temporary: PathBuf,
    pub quality: Quality,
    /// `false` añade `-n`: nunca se sobrescribe un `.opus` sin permiso explícito.
    pub overwrite: bool,
}

impl ConversionTask {
    pub fn new(
        source: PathBuf,
        output: PathBuf,
        quality: Quality,
        overwrite: bool,
        generation: u64,
        index: usize,
    ) -> Self {
        Self {
            source,
            temporary: temporary_output_path(&output, generation, index),
            output,
            quality,
            overwrite,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConversionMessage {
    Started {
        generation: u64,
        index: usize,
        source: PathBuf,
        output: PathBuf,
    },
    Done {
        generation: u64,
        index: usize,
        source: PathBuf,
        output: PathBuf,
    },
    Error {
        generation: u64,
        index: usize,
        source: PathBuf,
        message: String,
    },
}

/// Temporal oculto en la misma carpeta: el renombrado final es atómico y un
/// fallo de ffmpeg no puede dejar un `.opus` a medias con el nombre definitivo.
pub fn temporary_output_path(output: &Path, generation: u64, index: usize) -> PathBuf {
    let parent = output.parent().unwrap_or_else(|| Path::new(""));
    let stem = output.file_stem().unwrap_or_default().to_os_string();
    let mut name = OsString::from(".");
    name.push(stem);
    name.push(format!(".freeway-convert-{generation}-{index}.opus"));
    parent.join(name)
}

/// `FREEWAY_FFMPEG` permite probar la compilación nativa con otro ejecutable.
fn ffmpeg_program() -> OsString {
    std::env::var_os("FREEWAY_FFMPEG").unwrap_or_else(|| OsString::from("ffmpeg"))
}

pub fn ffmpeg_command(task: &ConversionTask) -> Command {
    let mut command = Command::new(ffmpeg_program());
    command
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg(if task.overwrite { "-y" } else { "-n" })
        .arg("-i")
        .arg(&task.source);
    command.args(task.quality.ffmpeg_args());
    command.arg(&task.temporary);
    command
}

/// Mueve el temporal al destino. En Linux `rename` reemplaza al destino cuando
/// existe, así que no hay que borrarlo antes de convertir.
pub fn finalize_output(temporary: &Path, output: &Path, overwrite: bool) -> Result<(), String> {
    if output.exists() && !overwrite {
        return Err(format!("Ya existe {}", output.display()));
    }
    fs::rename(temporary, output)
        .map_err(|error| format!("No se pudo colocar el Opus final: {error}"))
}

fn last_stderr_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

/// Convierte un FLAC y devuelve la ruta final. Nunca toca el destino hasta que
/// ffmpeg ha terminado correctamente y el temporal no está vacío.
pub fn run_conversion(task: &ConversionTask) -> Result<PathBuf, String> {
    if !is_flac(&task.source) {
        return Err("Solo se pueden convertir archivos FLAC".to_owned());
    }
    if !task.source.is_file() {
        return Err(format!(
            "No se encontró el archivo FLAC: {}",
            task.source.display()
        ));
    }
    if let Some(parent) = task.temporary.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("No se pudo preparar la carpeta de destino: {error}"))?;
    }
    if task.temporary.exists() {
        fs::remove_file(&task.temporary)
            .map_err(|error| format!("No se pudo limpiar el archivo temporal: {error}"))?;
    }

    let output = ffmpeg_command(task)
        .output()
        .map_err(|error| format!("No se pudo ejecutar ffmpeg: {error}"))?;
    if !output.status.success() {
        let _ = fs::remove_file(&task.temporary);
        let detail = last_stderr_line(&output.stderr)
            .unwrap_or_else(|| "ffmpeg no dio más detalles".to_owned());
        let name = task
            .source
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        return Err(format!("No se pudo convertir «{name}»: {detail}"));
    }
    if !task.temporary.is_file() {
        return Err("ffmpeg no creó el archivo Opus".to_owned());
    }
    let bytes = fs::metadata(&task.temporary)
        .map(|metadata| metadata.len())
        .map_err(|error| format!("No se pudo verificar el Opus: {error}"))?;
    if bytes == 0 {
        let _ = fs::remove_file(&task.temporary);
        return Err("ffmpeg creó un archivo Opus vacío".to_owned());
    }

    finalize_output(&task.temporary, &task.output, task.overwrite)?;
    Ok(task.output.clone())
}

/// Bucle del hilo, separado para poder probarlo con un convertidor falso.
pub fn run_batch<F>(
    tasks: Vec<ConversionTask>,
    generation: u64,
    sender: Sender<ConversionMessage>,
    mut run: F,
) where
    F: FnMut(&ConversionTask) -> Result<PathBuf, String>,
{
    for (index, task) in tasks.iter().enumerate() {
        let _ = sender.send(ConversionMessage::Started {
            generation,
            index,
            source: task.source.clone(),
            output: task.output.clone(),
        });
        match run(task) {
            Ok(output) => {
                let _ = sender.send(ConversionMessage::Done {
                    generation,
                    index,
                    source: task.source.clone(),
                    output,
                });
            }
            Err(message) => {
                let _ = sender.send(ConversionMessage::Error {
                    generation,
                    index,
                    source: task.source.clone(),
                    message,
                });
            }
        }
    }
}

/// Arranca el lote en su hilo. La generación evita mensajes de lotes anteriores.
pub fn start_conversion(
    tasks: Vec<ConversionTask>,
    generation: u64,
) -> Result<Receiver<ConversionMessage>, String> {
    if tasks.is_empty() {
        return Err("Selecciona al menos un archivo FLAC".to_owned());
    }
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("freeway-converter".to_owned())
        .spawn(move || {
            run_batch(tasks, generation, sender, run_conversion);
        })
        .map_err(|error| format!("No se pudo iniciar la conversión: {error}"))?;
    Ok(receiver)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn p(value: &str) -> PathBuf {
        PathBuf::from(value)
    }

    fn task(source: &str, output: &str, generation: u64, index: usize) -> ConversionTask {
        ConversionTask::new(
            p(source),
            p(output),
            Quality::Preset(96),
            false,
            generation,
            index,
        )
    }

    fn test_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("freeway-converter-{label}-{nanos}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn args_of(command: &Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn presets_start_at_96_and_keep_160_as_recommended() {
        assert_eq!(PRESET_BITRATES, [96, 128, 160, 192]);
        assert_eq!(Quality::default(), Quality::Preset(160));
        assert_eq!(quality_from_option(0, 96), Some(Quality::Preset(96)));
        assert_eq!(quality_from_option(2, 96), Some(Quality::Preset(160)));
        assert_eq!(quality_from_option(3, 96), Some(Quality::Preset(192)));
        assert_eq!(quality_from_option(4, 96), Some(Quality::Custom(96)));
        assert_eq!(quality_from_option(5, 96), None);
    }

    #[test]
    fn custom_bitrate_is_clamped_to_opus_range() {
        assert_eq!(clamp_bitrate(5), 6);
        assert_eq!(clamp_bitrate(96), 96);
        assert_eq!(clamp_bitrate(511), 510);
        assert_eq!(Quality::Custom(999).bitrate(), 510);
    }

    #[test]
    fn ffmpeg_args_keep_vbr_bitrate_and_text_metadata() {
        assert_eq!(
            Quality::Preset(160).ffmpeg_args(),
            vec!["-c:a", "libopus", "-b:a", "160k", "-map_metadata", "0"]
        );
        assert_eq!(Quality::Custom(96).ffmpeg_args()[3], "96k");
    }

    #[test]
    fn output_path_only_changes_extension() {
        assert_eq!(
            output_path(&p("/musica/Canción.FLAC")),
            p("/musica/Canción.opus")
        );
        assert_eq!(
            output_path(&p("/musica/song.tar.flac")),
            p("/musica/song.tar.opus")
        );
        assert_eq!(
            output_path(&p("/musica/sin_extension")),
            p("/musica/sin_extension.opus")
        );
    }

    #[test]
    fn renamed_output_skips_existing_candidates() {
        let occupied = vec![p("/musica/song.opus"), p("/musica/song-1.opus")];
        assert_eq!(
            renamed_output_path(&p("/musica/song.opus"), &occupied),
            p("/musica/song-2.opus")
        );
    }

    #[test]
    fn resolve_output_path_applies_collision_policy() {
        let input = p("/musica/song.flac");
        assert_eq!(
            resolve_output_path(&input, &[], CollisionPolicy::Overwrite),
            Some(p("/musica/song.opus"))
        );
        assert_eq!(
            resolve_output_path(
                &input,
                &[p("/musica/song.opus")],
                CollisionPolicy::Overwrite
            ),
            Some(p("/musica/song.opus"))
        );
        assert_eq!(
            resolve_output_path(&input, &[p("/musica/song.opus")], CollisionPolicy::Rename),
            Some(p("/musica/song-1.opus"))
        );
        assert_eq!(
            resolve_output_path(&input, &[p("/musica/song.opus")], CollisionPolicy::Skip),
            None
        );
    }

    #[test]
    fn list_match_count_counts_files_not_occurrences() {
        let inputs = vec![
            p("/musica/a.flac"),
            p("/musica/b.flac"),
            p("/musica/c.flac"),
        ];
        let lists = vec![
            vec![p("/musica/a.flac"), p("/musica/x.opus")],
            vec![p("/musica/a.flac"), p("/musica/b.flac")],
        ];
        assert_eq!(list_match_count(&inputs, &lists), 2);
    }

    #[test]
    fn replace_in_lists_updates_matching_paths_without_duplicates() {
        let mut lists = vec![
            vec![
                p("/musica/a.flac"),
                p("/musica/keep.opus"),
                p("/musica/b.flac"),
            ],
            vec![p("/musica/c.flac")],
        ];
        let replacements = vec![
            (p("/musica/a.flac"), p("/musica/a.opus")),
            (p("/musica/b.flac"), p("/musica/keep.opus")),
        ];

        let changed = replace_in_lists(&mut lists, &replacements);

        assert_eq!(changed, 2);
        assert_eq!(lists[0], vec![p("/musica/a.opus"), p("/musica/keep.opus")]);
        assert_eq!(lists[1], vec![p("/musica/c.flac")]);
    }

    #[test]
    fn source_is_deleted_only_after_a_clean_success() {
        assert!(can_delete_source(true, true, true, false));
        assert!(!can_delete_source(false, true, true, false));
        assert!(!can_delete_source(true, false, true, false));
        assert!(!can_delete_source(true, true, false, false));
        assert!(!can_delete_source(true, true, true, true));
    }

    #[test]
    fn temporary_output_path_is_hidden_and_unique() {
        assert_eq!(
            temporary_output_path(&p("/musica/a.opus"), 7, 2),
            p("/musica/.a.freeway-convert-7-2.opus")
        );
        assert_ne!(
            temporary_output_path(&p("/musica/a.opus"), 7, 2),
            temporary_output_path(&p("/musica/a.opus"), 7, 3)
        );
    }

    #[test]
    fn ffmpeg_command_writes_to_temporary_and_honours_quality_and_overwrite() {
        let mut convert = task("/musica/a.flac", "/musica/a.opus", 9, 0);
        convert.quality = Quality::Preset(128);
        convert.overwrite = true;

        let command = ffmpeg_command(&convert);
        let expected_program =
            std::env::var_os("FREEWAY_FFMPEG").unwrap_or_else(|| OsString::from("ffmpeg"));
        assert_eq!(command.get_program(), expected_program);

        let args = args_of(&command);
        assert!(args.contains(&"-nostdin".to_owned()));
        assert!(args.contains(&"-hide_banner".to_owned()));
        assert!(args.contains(&"-y".to_owned()));
        assert!(!args.contains(&"-n".to_owned()));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-i" && pair[1] == "/musica/a.flac"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-b:a" && pair[1] == "128k"));
        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "-map_metadata" && pair[1] == "0"));
        assert_eq!(
            args.last().unwrap(),
            &convert.temporary.to_string_lossy().into_owned()
        );

        let keep = ConversionTask {
            overwrite: false,
            ..convert.clone()
        };
        let keep_args = args_of(&ffmpeg_command(&keep));
        assert!(keep_args.contains(&"-n".to_owned()));
        assert!(!keep_args.contains(&"-y".to_owned()));
    }

    #[test]
    fn last_stderr_line_takes_the_last_non_empty_line() {
        assert_eq!(
            last_stderr_line(b"aviso\n\nerror final\n"),
            Some("error final".to_owned())
        );
        assert_eq!(last_stderr_line(b"\n\n"), None);
    }

    #[test]
    fn finalize_output_replaces_existing_opus_when_allowed() {
        let dir = test_dir("overwrite");
        let temporary = dir.join("nuevo.opus");
        let output = dir.join("viejo.opus");
        fs::write(&temporary, b"nuevo").unwrap();
        fs::write(&output, b"viejo").unwrap();

        finalize_output(&temporary, &output, true).unwrap();

        assert_eq!(fs::read(&output).unwrap(), b"nuevo");
        assert!(!temporary.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn finalize_output_refuses_to_overwrite_without_permission() {
        let dir = test_dir("keep");
        let temporary = dir.join("nuevo.opus");
        let output = dir.join("viejo.opus");
        fs::write(&temporary, b"nuevo").unwrap();
        fs::write(&output, b"viejo").unwrap();

        assert!(finalize_output(&temporary, &output, false).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"viejo");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_conversion_rejects_non_flac_sources() {
        let convert = task("/musica/a.mp3", "/musica/a.opus", 1, 0);
        assert!(run_conversion(&convert).unwrap_err().contains("FLAC"));
    }

    #[test]
    fn start_conversion_rejects_an_empty_batch() {
        assert!(start_conversion(Vec::new(), 1).is_err());
    }

    #[test]
    fn is_flac_accepts_uppercase_extension() {
        assert!(is_flac(&p("/musica/a.flac")));
        assert!(is_flac(&p("/musica/a.FLAC")));
        assert!(!is_flac(&p("/musica/a.opus")));
        assert!(!is_flac(&p("/musica/a.flac.bak")));
    }

    #[test]
    fn existing_outputs_in_finds_opus_in_source_folders() {
        let dir = test_dir("occupied");
        fs::write(dir.join("a.flac"), b"flac").unwrap();
        fs::write(dir.join("a.opus"), b"opus").unwrap();
        fs::write(dir.join("a-1.OPUS"), b"opus").unwrap();
        fs::write(dir.join("a.txt"), b"texto").unwrap();

        let mut found = existing_outputs_in(&[dir.join("a.flac")]);
        found.sort();
        let mut expected = vec![dir.join("a.opus"), dir.join("a-1.OPUS")];
        expected.sort();

        assert_eq!(found, expected);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn collision_policy_from_choice_maps_dialog_results() {
        assert_eq!(
            collision_policy_from_choice(1),
            Some(CollisionPolicy::Overwrite)
        );
        assert_eq!(
            collision_policy_from_choice(2),
            Some(CollisionPolicy::Rename)
        );
        assert_eq!(collision_policy_from_choice(3), Some(CollisionPolicy::Skip));
        assert_eq!(collision_policy_from_choice(0), Some(CollisionPolicy::Skip));
        assert_eq!(collision_policy_from_choice(9), None);
    }

    #[test]
    fn replace_entries_in_lists_keeps_the_stream_payload_aligned() {
        let mut lists = vec![vec![
            (p("/musica/a.flac"), Some("stream-a")),
            (p("/musica/keep.opus"), None),
            (p("/musica/b.flac"), Some("stream-b")),
        ]];
        let replacements = vec![
            (p("/musica/a.flac"), p("/musica/a.opus")),
            (p("/musica/b.flac"), p("/musica/keep.opus")),
        ];

        let changed = replace_entries_in_lists(&mut lists, &replacements);

        assert_eq!(changed, 2);
        assert_eq!(
            lists[0],
            vec![
                (p("/musica/a.opus"), Some("stream-a")),
                (p("/musica/keep.opus"), None),
            ]
        );
    }

    #[test]
    #[ignore = "requiere ffmpeg"]
    fn converts_a_real_flac_with_ffmpeg() {
        let dir = test_dir("real");
        let source = dir.join("tono.flac");
        let generated = Command::new("ffmpeg")
            .arg("-nostdin")
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-y")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("sine=frequency=440:duration=1")
            .arg("-c:a")
            .arg("flac")
            .arg(&source)
            .status()
            .expect("ffmpeg debe estar instalado");
        assert!(
            generated.success(),
            "ffmpeg no pudo generar el FLAC de prueba"
        );

        let output = dir.join("tono.opus");
        let convert = ConversionTask::new(source, output.clone(), Quality::Preset(96), false, 3, 0);

        let final_path = run_conversion(&convert).expect("la conversión debe terminar bien");
        assert_eq!(final_path, output);
        assert!(output.is_file());
        assert!(fs::metadata(&output).unwrap().len() > 0);
        assert!(!convert.temporary.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn run_batch_reports_each_file_and_keeps_going_after_errors() {
        let tasks = vec![
            task("/musica/a.flac", "/musica/a.opus", 9, 0),
            task("/musica/bad.flac", "/musica/bad.opus", 9, 1),
            task("/musica/c.flac", "/musica/c.opus", 9, 2),
        ];
        let (sender, receiver) = mpsc::channel();

        run_batch(tasks, 9, sender, |convert| {
            if convert.source.ends_with("bad.flac") {
                Err("falló".to_owned())
            } else {
                Ok(convert.output.clone())
            }
        });

        let messages: Vec<_> = receiver.iter().collect();
        assert_eq!(messages.len(), 6);
        assert_eq!(
            messages[0],
            ConversionMessage::Started {
                generation: 9,
                index: 0,
                source: p("/musica/a.flac"),
                output: p("/musica/a.opus"),
            }
        );
        assert_eq!(
            messages[1],
            ConversionMessage::Done {
                generation: 9,
                index: 0,
                source: p("/musica/a.flac"),
                output: p("/musica/a.opus"),
            }
        );
        assert_eq!(
            messages[3],
            ConversionMessage::Error {
                generation: 9,
                index: 1,
                source: p("/musica/bad.flac"),
                message: "falló".to_owned(),
            }
        );
        assert_eq!(
            messages[5],
            ConversionMessage::Done {
                generation: 9,
                index: 2,
                source: p("/musica/c.flac"),
                output: p("/musica/c.opus"),
            }
        );
    }
}
