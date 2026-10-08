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
    let config = device
        .default_output_config()
        .map_err(|error| format!("Could not read audio output config: {error}"))?;
    let sample_rate = config.sample_rate();
    let channels = usize::from(config.channels());
    if channels == 0 {
        return Err("Audio output config has zero channels".to_owned());
    }
    let stream_config = config.into();
    let shared_fps = shared.fps.clone();
    let shared_timezone = shared.timezone.clone();
    let shared_enabled_flag = shared.enabled.clone();
    let shared_volume_dbfs = shared.volume_dbfs.clone();
    let offset_ms = shared.offset_ms.clone();
    let base_instant = shared.local_base;
    let stream_error = shared.stream_error.clone();
    let mut generator_fps = SelectedFps::from_u8(shared_fps.load(Ordering::Relaxed));
    let mut ltc_gen = LtcBiphaseMarkGenerator::new(sample_rate, generator_fps);
    let mut frame_clock = LtcFrameClock::new(sample_rate, generator_fps);
    let mut samples_until_frame_boundary = 0;
    let mut mono_scratch = Vec::new();

    device
        .build_output_stream(
            stream_config,
            move |data: &mut [f32], _| {
                if !shared_enabled_flag.load(Ordering::Relaxed) {
                    data.fill(0.0);
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
                let audio_frames = data.len() / channels;

                if channels == 1 {
                    generate_ltc_frames(
                        &mut data[..audio_frames],
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
                        &mut data[..audio_frames * channels],
                        channels,
                    );
                    data[audio_frames * channels..].fill(0.0);
                }

                let gain = 10.0_f32.powf(shared_volume_dbfs.load(Ordering::Relaxed) as f32 / 20.0);
                for sample in data.iter_mut() {
                    *sample *= gain;
                }
            },
            move |_| {
                stream_error.store(true, Ordering::Relaxed);
            },
            None,
        )
        .map(|stream| (stream, device_id))
        .map_err(|error| format!("Could not build audio output stream: {error}"))
        .and_then(|(stream, device_id)| {
            stream
                .play()
                .map(|()| (stream, device_id))
                .map_err(|error| format!("Could not start audio output stream: {error}"))
        })
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
