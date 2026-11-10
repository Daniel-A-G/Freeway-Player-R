use std::{fs::File, io, path::Path, sync::Arc, time::Duration};

use rodio::{source::SeekError, ChannelCount, Sample, SampleRate, Source};
use symphonia::{
    core::{
        audio::{AudioBufferRef, SampleBuffer, SignalSpec},
        codecs::{CodecRegistry, Decoder, DecoderOptions, CODEC_TYPE_NULL},
        errors::Error,
        formats::{FormatOptions, FormatReader, SeekMode, SeekTo},
        io::MediaSourceStream,
        meta::MetadataOptions,
        probe::Hint,
        units,
    },
    default::{
        codecs::{FlacDecoder, VorbisDecoder},
        get_probe,
    },
};

use symphonia_adapter_libopus::OpusDecoder;

pub(crate) fn source_for_path(path: &Path) -> Result<Box<dyn Source + Send>, String> {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let file = File::open(path)
        .map_err(|error| format!("No se pudo abrir {}: {error}", path.display()))?;

    if matches!(extension.as_str(), "opus" | "ogg") {
        OggSource::new(file, &extension)
            .map(|source| Box::new(source) as Box<dyn Source + Send>)
            .map_err(|error| format!("Formato Ogg/Opus no compatible: {error}"))
    } else {
        rodio::Decoder::try_from(file)
            .map(|source| Box::new(source) as Box<dyn Source + Send>)
            .map_err(|error| format!("Formato de audio no compatible: {error}"))
    }
}

struct OggSource {
    decoder: Box<dyn Decoder>,
    format: Box<dyn FormatReader>,
    track_id: u32,
    total_duration: Option<Duration>,
    buffer: SampleBuffer<Sample>,
    spec: SignalSpec,
    current_offset: usize,
}

impl OggSource {
    fn new(file: File, extension: &str) -> Result<Self, Error> {
        let mut hint = Hint::new();
        hint.with_extension(extension);

        let stream = MediaSourceStream::new(Box::new(file), Default::default());
        let mut probed = get_probe().format(
            &hint,
            stream,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        let track = probed
            .format
            .default_track()
            .ok_or(Error::Unsupported("Ogg file has no default track"))?;
        let track_id = track.id;
        let codec_params = track.codec_params.clone();
        if codec_params.codec == CODEC_TYPE_NULL {
            return Err(Error::Unsupported("Ogg file has no supported audio track"));
        }

        let mut codecs = CodecRegistry::new();
        codecs.register_all::<OpusDecoder>();
        codecs.register_all::<VorbisDecoder>();
        codecs.register_all::<FlacDecoder>();
        let mut decoder = codecs.make(&codec_params, &DecoderOptions::default())?;
        let total_duration = codec_params
            .time_base
            .zip(codec_params.n_frames)
            .map(|(time_base, frames)| Duration::from(time_base.calc_time(frames)))
            .filter(|duration| !duration.is_zero());

        let decoded = loop {
            let packet = probed.format.next_packet()?;
            if packet.track_id() != track_id {
                continue;
            }
            match decoder.decode(&packet) {
                Ok(decoded) if decoded.frames() > 0 => break decoded,
                Ok(_) | Err(Error::DecodeError(_)) => continue,
                Err(error) => return Err(error),
            }
        };

        let spec = decoded.spec().to_owned();
        let buffer = Self::make_buffer(decoded, &spec);

        Ok(Self {
            decoder,
            format: probed.format,
            track_id,
            total_duration,
            buffer,
            spec,
            current_offset: 0,
        })
    }

    fn make_buffer(decoded: AudioBufferRef<'_>, spec: &SignalSpec) -> SampleBuffer<Sample> {
        let duration = units::Duration::from(decoded.capacity() as u64);
        let mut buffer = SampleBuffer::<Sample>::new(duration, *spec);
        buffer.copy_interleaved_ref(decoded);
        buffer
    }

    fn seek_error(error: impl std::fmt::Display) -> SeekError {
        SeekError::Other(Arc::new(io::Error::other(error.to_string())))
    }
}

impl Iterator for OggSource {
    type Item = Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_offset >= self.buffer.len() {
            let decoded = loop {
                let packet = match self.format.next_packet() {
                    Ok(packet) => packet,
                    Err(_) => return None,
                };
                if packet.track_id() != self.track_id {
                    continue;
                }

                match self.decoder.decode(&packet) {
                    Ok(decoded) if decoded.frames() > 0 => break decoded,
                    Ok(_) | Err(Error::DecodeError(_)) => continue,
                    Err(_) => return None,
                }
            };

            decoded.spec().clone_into(&mut self.spec);
            self.buffer = Self::make_buffer(decoded, &self.spec);
            self.current_offset = 0;
        }

        let sample = *self.buffer.samples().get(self.current_offset)?;
        self.current_offset += 1;
        Some(sample)
    }
}

impl Source for OggSource {
    fn current_span_len(&self) -> Option<usize> {
        Some(self.buffer.len())
    }

    fn channels(&self) -> ChannelCount {
        ChannelCount::new(
            self.spec
                .channels
                .count()
                .try_into()
                .expect("rodio only supports up to u16::MAX channels"),
        )
        .expect("decoded audio must have at least one channel")
    }

    fn sample_rate(&self) -> SampleRate {
        SampleRate::new(self.spec.rate).expect("decoded audio must have a non-zero sample rate")
    }

    fn total_duration(&self) -> Option<Duration> {
        self.total_duration
    }

    fn try_seek(&mut self, position: Duration) -> Result<(), SeekError> {
        let time_base =
            self.decoder
                .codec_params()
                .time_base
                .ok_or_else(|| SeekError::NotSupported {
                    underlying_source: std::any::type_name::<Self>(),
                })?;

        let target = self
            .total_duration
            .map(|duration| position.min(duration))
            .unwrap_or(position);
        let active_channel = self.current_offset % self.channels().get() as usize;
        let seeked = self
            .format
            .seek(
                SeekMode::Accurate,
                SeekTo::Time {
                    time: target.into(),
                    track_id: Some(self.track_id),
                },
            )
            .map_err(Self::seek_error)?;

        self.decoder.reset();
        self.current_offset = usize::MAX;

        let mut samples_to_skip = (Duration::from(
            time_base.calc_time(seeked.required_ts.saturating_sub(seeked.actual_ts)),
        )
        .as_secs_f64()
            * f64::from(self.sample_rate().get())
            * f64::from(self.channels().get()))
        .ceil() as usize;
        samples_to_skip -= samples_to_skip % self.channels().get() as usize;

        for _ in 0..samples_to_skip {
            if self.next().is_none() {
                break;
            }
        }
        for _ in 0..active_channel {
            self.next();
        }

        Ok(())
    }
}
