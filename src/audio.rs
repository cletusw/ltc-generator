use crate::ltc::{
    LtcBiphaseMarkGenerator, LtcFrameClock, SelectedFps, duplicate_mono_samples,
    generate_ltc_frames,
};
use crate::status::{StatusText, set_audio_status};
use chrono_tz::TZ_VARIANTS;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct AudioWorkerGuard {
    worker_running: Arc<AtomicBool>,
    enabled: Arc<AtomicBool>,
}

impl AudioWorkerGuard {
    pub(crate) fn new(worker_running: Arc<AtomicBool>, enabled: Arc<AtomicBool>) -> Self {
        Self {
            worker_running,
            enabled,
        }
    }
}

impl Drop for AudioWorkerGuard {
    fn drop(&mut self) {
        self.worker_running.store(false, Ordering::Relaxed);
        self.enabled.store(false, Ordering::Relaxed);
    }
}

pub(crate) struct SharedAudioState {
    pub(crate) fps: Arc<AtomicU8>,
    pub(crate) timezone: Arc<AtomicUsize>,
    pub(crate) enabled: Arc<AtomicBool>,
    pub(crate) volume_dbfs: Arc<AtomicI32>,
    pub(crate) offset_ms: Arc<AtomicI64>,
    pub(crate) local_base: Instant,
    pub(crate) status_text: Arc<Mutex<StatusText>>,
    pub(crate) stream_error: Arc<AtomicBool>,
    pub(crate) available: Arc<AtomicBool>,
    pub(crate) worker_running: Arc<AtomicBool>,
}

pub(crate) fn start_audio_worker(shared: SharedAudioState) {
    std::thread::spawn(move || {
        let host = match std::panic::catch_unwind(cpal::default_host) {
            Ok(host) => host,
            Err(_) => {
                set_audio_status(&shared.status_text, "Audio host initialization panicked");
                return;
            }
        };
        let mut stream = None;
        let mut active_device_id = None;

        while shared.worker_running.load(Ordering::Relaxed) {
            let default_device = current_default_output_device_id(&host);
            let stream_failed = shared.stream_error.swap(false, Ordering::Relaxed);

            match &default_device {
                Ok(device_id) if device_id == &active_device_id && !stream_failed => {}
                _ => {
                    stream = None;
                    active_device_id = None;
                    shared.enabled.store(false, Ordering::Relaxed);
                    shared.available.store(false, Ordering::Relaxed);
                }
            }

            if stream.is_none() {
                match default_device {
                    Ok(Some(device_id)) => {
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            initialize_audio_stream(&host, &shared, &device_id)
                        })) {
                            Err(_) => {
                                set_audio_status(
                                    &shared.status_text,
                                    "Audio output initialization panicked",
                                );
                            }
                            Ok(Err(error)) => {
                                set_audio_status(
                                    &shared.status_text,
                                    &format!("Audio output unavailable: {error}"),
                                );
                            }
                            Ok(Ok((new_stream, device_id))) => {
                                active_device_id = Some(device_id);
                                stream = Some(new_stream);
                                shared.available.store(true, Ordering::Relaxed);
                                let status = if shared.enabled.load(Ordering::Relaxed) {
                                    "Audio output stream is playing"
                                } else {
                                    "Audio output ready (muted)"
                                };
                                set_audio_status(&shared.status_text, status);
                            }
                        }
                    }
                    Ok(None) => set_audio_status(
                        &shared.status_text,
                        "No default audio output device is available",
                    ),
                    Err(error) => set_audio_status(
                        &shared.status_text,
                        &format!("Audio device check failed: {error}"),
                    ),
                }
            }

            std::thread::sleep(Duration::from_millis(500));
        }

        drop(stream);
        shared.available.store(false, Ordering::Relaxed);
    });
}

fn initialize_audio_stream(
    host: &cpal::Host,
    shared: &SharedAudioState,
    default_device_id: &str,
) -> Result<(cpal::Stream, String), String> {
    let mut device = None;
    for candidate in host
        .output_devices()
        .map_err(|error| format!("Could not enumerate audio output devices: {error}"))?
    {
        let candidate_id = candidate
            .id()
            .map_err(|error| format!("Could not identify audio output device: {error}"))?
            .to_string();
        if candidate_id == default_device_id {
            device = Some((candidate, candidate_id));
            break;
        }
    }
    let (device, device_id) = device
        .ok_or_else(|| "The default audio output device is no longer available".to_owned())?;
    let default_config = device
        .default_output_config()
        .map_err(|error| format!("Could not read audio output config: {error}"))?;
    let config = device
        .supported_output_configs()
        .ok()
        .and_then(|configs| {
            configs
                .filter(|config| config.sample_format() == cpal::SampleFormat::F32)
                .min_by_key(|config| {
                    (
                        config.channels() != default_config.channels(),
                        !(config.min_sample_rate() <= default_config.sample_rate()
                            && default_config.sample_rate() <= config.max_sample_rate()),
                    )
                })
                .map(|config| {
                    config
                        .try_with_sample_rate(default_config.sample_rate())
                        .unwrap_or_else(|| config.with_max_sample_rate())
                })
        })
        .unwrap_or(default_config);
    let sample_rate = config.sample_rate();
    let channels = usize::from(config.channels());
    if channels == 0 {
        return Err("Audio output config has zero channels".to_owned());
    }
    let stream_config = config.config();
    let sample_format = config.sample_format();
    let shared_fps = shared.fps.clone();
    let shared_timezone = shared.timezone.clone();
    let shared_enabled_flag = shared.enabled.clone();
    let offset_ms = shared.offset_ms.clone();
    let base_instant = shared.local_base;
    let stream_error = shared.stream_error.clone();
    let shared_volume_dbfs = shared.volume_dbfs.clone();
    macro_rules! build_stream {
        ($sample_type:ty) => {
            build_typed_output_stream::<$sample_type>(
                &device,
                stream_config,
                sample_rate,
                channels,
                shared_fps.clone(),
                shared_timezone.clone(),
                shared_enabled_flag.clone(),
                shared_volume_dbfs.clone(),
                offset_ms.clone(),
                base_instant,
                stream_error.clone(),
            )
        };
    }
    let stream = match sample_format {
        cpal::SampleFormat::F32 => build_stream!(f32),
        cpal::SampleFormat::F64 => build_stream!(f64),
        cpal::SampleFormat::I8 => build_stream!(i8),
        cpal::SampleFormat::I16 => build_stream!(i16),
        cpal::SampleFormat::I24 => build_stream!(cpal::I24),
        cpal::SampleFormat::I32 => build_stream!(i32),
        cpal::SampleFormat::I64 => build_stream!(i64),
        cpal::SampleFormat::U8 => build_stream!(u8),
        cpal::SampleFormat::U16 => build_stream!(u16),
        cpal::SampleFormat::U24 => build_stream!(cpal::U24),
        cpal::SampleFormat::U32 => build_stream!(u32),
        cpal::SampleFormat::U64 => build_stream!(u64),
        unsupported => {
            return Err(format!(
                "Unsupported audio output sample format: {unsupported}"
            ));
        }
    };
    stream
        .map(|stream| (stream, device_id))
        .map_err(|error| format!("Could not build audio output stream: {error}"))
        .and_then(|(stream, device_id)| {
            stream
                .play()
                .map(|()| (stream, device_id))
                .map_err(|error| format!("Could not start audio output stream: {error}"))
        })
}

#[allow(clippy::too_many_arguments)]
fn build_typed_output_stream<T>(
    device: &cpal::Device,
    stream_config: cpal::StreamConfig,
    sample_rate: u32,
    channels: usize,
    shared_fps: Arc<AtomicU8>,
    shared_timezone: Arc<AtomicUsize>,
    shared_enabled_flag: Arc<AtomicBool>,
    shared_volume_dbfs: Arc<AtomicI32>,
    offset_ms: Arc<AtomicI64>,
    base_instant: Instant,
    stream_error: Arc<AtomicBool>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let mut generator_fps = SelectedFps::from_u8(shared_fps.load(Ordering::Relaxed));
    let mut ltc_gen = LtcBiphaseMarkGenerator::new(sample_rate, generator_fps);
    let mut frame_clock = LtcFrameClock::new(sample_rate, generator_fps);
    let mut samples_until_frame_boundary = 0;
    let mut mono_scratch = Vec::new();
    let mut sample_scratch = Vec::new();

    device.build_output_stream(
        stream_config,
        move |data: &mut [T], _| {
            sample_scratch.resize(data.len(), 0.0);
            if !shared_enabled_flag.load(Ordering::Relaxed) {
                data.fill(T::from_sample(0.0));
                samples_until_frame_boundary = 0;
                return;
            }

            let fps = SelectedFps::from_u8(shared_fps.load(Ordering::Relaxed));
            if fps != generator_fps {
                ltc_gen = LtcBiphaseMarkGenerator::new(sample_rate, fps);
                frame_clock = LtcFrameClock::new(sample_rate, fps);
                generator_fps = fps;
                samples_until_frame_boundary = 0;
            }

            let offset = offset_ms.load(Ordering::Relaxed);
            let callback_elapsed = base_instant.elapsed();
            let timezone_index = shared_timezone.load(Ordering::Relaxed) % TZ_VARIANTS.len();
            let audio_frames = sample_scratch.len() / channels;

            if channels == 1 {
                generate_ltc_frames(
                    &mut sample_scratch[..audio_frames],
                    &mut ltc_gen,
                    &mut frame_clock,
                    &mut samples_until_frame_boundary,
                    sample_rate,
                    callback_elapsed,
                    offset,
                    timezone_index,
                    fps,
                );
            } else {
                mono_scratch.resize(audio_frames, 0.0);
                generate_ltc_frames(
                    &mut mono_scratch,
                    &mut ltc_gen,
                    &mut frame_clock,
                    &mut samples_until_frame_boundary,
                    sample_rate,
                    callback_elapsed,
                    offset,
                    timezone_index,
                    fps,
                );
                duplicate_mono_samples(
                    &mono_scratch,
                    &mut sample_scratch[..audio_frames * channels],
                    channels,
                );
                sample_scratch[audio_frames * channels..].fill(0.0);
            }

            let gain = 10.0_f32.powf(shared_volume_dbfs.load(Ordering::Relaxed) as f32 / 20.0);
            for (output, sample) in data.iter_mut().zip(sample_scratch.iter()) {
                *output = T::from_sample(*sample * gain);
            }
        },
        move |_| {
            stream_error.store(true, Ordering::Relaxed);
        },
        None,
    )
}

fn current_default_output_device_id(host: &cpal::Host) -> Result<Option<String>, String> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.default_output_device()
            .map(|device| device.id().map(|id| id.to_string()))
            .transpose()
    }))
    .map_err(|_| "Audio host device polling panicked".to_owned())?
    .map_err(|error| format!("Could not identify default audio output device: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_app_stops_audio_output() {
        let worker_running = Arc::new(AtomicBool::new(true));
        let audio_enabled = Arc::new(AtomicBool::new(true));
        let app_audio_worker = AudioWorkerGuard::new(worker_running.clone(), audio_enabled.clone());

        drop(app_audio_worker);

        assert!(!worker_running.load(Ordering::Relaxed));
        assert!(!audio_enabled.load(Ordering::Relaxed));
    }
}
