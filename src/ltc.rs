use chrono::{DateTime, Timelike, Utc};
use chrono_tz::TZ_VARIANTS;
use oximedia_timecode::ltc_encoder::LtcBitEncoder;
use oximedia_timecode::{FrameRate as OxiFrameRate, Timecode};
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub(crate) enum SelectedFps {
    Fps24 = 0,
    Fps25 = 1,
    Fps2997Ndf = 2,
    Fps2997Df = 3,
    Fps30 = 4,
}

impl SelectedFps {
    pub(crate) fn from_u8(val: u8) -> Self {
        match val {
            0 => SelectedFps::Fps24,
            1 => SelectedFps::Fps25,
            2 => SelectedFps::Fps2997Ndf,
            3 => SelectedFps::Fps2997Df,
            _ => SelectedFps::Fps30,
        }
    }

    pub(crate) fn to_oximedia_fps(self) -> OxiFrameRate {
        match self {
            SelectedFps::Fps24 => OxiFrameRate::Fps24,
            SelectedFps::Fps25 => OxiFrameRate::Fps25,
            SelectedFps::Fps2997Ndf => OxiFrameRate::Fps2997NDF,
            SelectedFps::Fps2997Df => OxiFrameRate::Fps2997DF,
            SelectedFps::Fps30 => OxiFrameRate::Fps30,
        }
    }

    pub(crate) fn ratio(self) -> (u64, u64) {
        match self {
            SelectedFps::Fps24 => (24, 1),
            SelectedFps::Fps25 => (25, 1),
            SelectedFps::Fps2997Ndf | SelectedFps::Fps2997Df => (30_000, 1_001),
            SelectedFps::Fps30 => (30, 1),
        }
    }

    pub(crate) fn label(&self) -> &'static str {
        match self {
            SelectedFps::Fps24 => "24 fps (Film)",
            SelectedFps::Fps25 => "25 fps (PAL)",
            SelectedFps::Fps2997Ndf => "29.97 fps (NDF)",
            SelectedFps::Fps2997Df => "29.97 fps (Drop Frame)",
            SelectedFps::Fps30 => "30 fps",
        }
    }
}

impl Default for SelectedFps {
    fn default() -> Self {
        SelectedFps::Fps2997Df
    }
}

pub(crate) fn generate_ltc_frames(
    data: &mut [f32],
    ltc_gen: &mut LtcBiphaseMarkGenerator,
    frame_clock: &mut LtcFrameClock,
    samples_until_frame_boundary: &mut usize,
    sample_rate: u32,
    callback_elapsed: Duration,
    offset: i64,
    timezone_index: usize,
    fps: SelectedFps,
) {
    let mut sample_offset = 0;
    while sample_offset < data.len() {
        if *samples_until_frame_boundary == 0 {
            let sample_time = callback_elapsed
                + Duration::from_secs_f64(sample_offset as f64 / f64::from(sample_rate));
            let Some(tc) = timecode_at(offset, sample_time, timezone_index, fps) else {
                data[sample_offset..].fill(0.0);
                break;
            };
            ltc_gen.begin_frame(&tc);
            *samples_until_frame_boundary = frame_clock.next_frame_samples();
        }

        let chunk_len = (*samples_until_frame_boundary).min(data.len() - sample_offset);
        ltc_gen.generate(&mut data[sample_offset..sample_offset + chunk_len]);
        sample_offset += chunk_len;
        *samples_until_frame_boundary -= chunk_len;
    }
}

pub(crate) fn duplicate_mono_samples(mono: &[f32], interleaved: &mut [f32], channels: usize) {
    if channels == 0 {
        interleaved.fill(0.0);
        return;
    }

    let frames = mono.len().min(interleaved.len() / channels);
    for (sample, frame) in mono
        .iter()
        .take(frames)
        .zip(interleaved.chunks_exact_mut(channels))
    {
        frame.fill(*sample);
    }
    interleaved[frames * channels..].fill(0.0);
}

pub(crate) struct LtcBiphaseMarkGenerator {
    sample_rate: u32,
    fps: SelectedFps,
    bits: [u8; 80],
    sample_in_frame: usize,
    bit_position: usize,
    polarity: f32,
    mid_transition_done: bool,
}

impl LtcBiphaseMarkGenerator {
    pub(crate) fn new(sample_rate: u32, fps: SelectedFps) -> Self {
        Self {
            sample_rate,
            fps,
            bits: [0; 80],
            sample_in_frame: 0,
            bit_position: 0,
            polarity: 1.0,
            mid_transition_done: false,
        }
    }

    pub(crate) fn begin_frame(&mut self, timecode: &Timecode) {
        self.bits = LtcBitEncoder::encode(timecode);
        self.sample_in_frame = 0;
        self.bit_position = 0;
        self.mid_transition_done = false;
    }

    pub(crate) fn generate(&mut self, samples: &mut [f32]) {
        let samples_per_bit =
            f64::from(self.sample_rate) / (80.0 * self.fps.to_oximedia_fps().as_float());

        for sample in samples {
            let current_bit = (self.sample_in_frame as f64 / samples_per_bit) as usize;
            while self.bit_position <= current_bit && self.bit_position < self.bits.len() {
                self.polarity = -self.polarity;
                self.bit_position += 1;
                self.mid_transition_done = false;
            }

            if current_bit < self.bits.len()
                && self.bits[current_bit] == 1
                && !self.mid_transition_done
                && self.sample_in_frame as f64 >= (current_bit as f64 + 0.5) * samples_per_bit
            {
                self.polarity = -self.polarity;
                self.mid_transition_done = true;
            }

            *sample = self.polarity;
            self.sample_in_frame += 1;
        }
    }
}

pub(crate) struct LtcFrameClock {
    samples_per_frame_numerator: u64,
    frames_per_second_numerator: u64,
    rounding_error: u64,
}

impl LtcFrameClock {
    pub(crate) fn new(sample_rate: u32, fps: SelectedFps) -> Self {
        let (fps_numerator, fps_denominator) = fps.ratio();
        Self {
            samples_per_frame_numerator: u64::from(sample_rate) * fps_denominator,
            frames_per_second_numerator: fps_numerator,
            rounding_error: 0,
        }
    }

    pub(crate) fn next_frame_samples(&mut self) -> usize {
        let adjusted_numerator = self.samples_per_frame_numerator - self.rounding_error;
        let samples = adjusted_numerator.div_ceil(self.frames_per_second_numerator);
        self.rounding_error = samples * self.frames_per_second_numerator - adjusted_numerator;
        samples as usize
    }
}

pub(crate) fn current_utc_time(offset_ms: i64, elapsed: Duration) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(offset_ms)?
        .checked_add_signed(chrono::Duration::from_std(elapsed).ok()?)
}

fn timecode_at(
    offset_ms: i64,
    elapsed: Duration,
    timezone_index: usize,
    fps: SelectedFps,
) -> Option<Timecode> {
    let local_time = current_utc_time(offset_ms, elapsed)?
        .with_timezone(&TZ_VARIANTS[timezone_index % TZ_VARIANTS.len()]);
    let elapsed_ns = (i128::from(local_time.num_seconds_from_midnight()) * 1_000_000_000)
        + i128::from(local_time.timestamp_subsec_nanos());
    let (fps_numerator, fps_denominator) = fps.ratio();
    let frame_number = (elapsed_ns * i128::from(fps_numerator)
        / (1_000_000_000 * i128::from(fps_denominator))) as u64;
    Timecode::from_frames(frame_number, fps.to_oximedia_fps()).ok()
}

pub(crate) fn system_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_owned())
}

pub(crate) fn timezone_index(name: &str) -> Option<usize> {
    TZ_VARIANTS
        .iter()
        .position(|timezone| timezone.to_string() == name)
}
