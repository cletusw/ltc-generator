use chrono::{DateTime, Timelike, Utc};
use chrono_tz::TZ_VARIANTS;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use eframe::egui;
use oximedia_timecode::{FrameRate as OxiFrameRate, Timecode};
use oximedia_timesync::NtpClient;
use oximedia_timesync::ntp::client::NtpClientConfig;
use oximedia_timesync::timecode::ltc::LtcGenerator;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum SelectedFps {
    Fps24 = 0,
    Fps25 = 1,
    Fps2997Ndf = 2,
    Fps2997Df = 3,
    Fps30 = 4,
}

impl SelectedFps {
    fn from_u8(val: u8) -> Self {
        match val {
            0 => SelectedFps::Fps24,
            1 => SelectedFps::Fps25,
            2 => SelectedFps::Fps2997Ndf,
            3 => SelectedFps::Fps2997Df,
            _ => SelectedFps::Fps30,
        }
    }

    fn to_oximedia_fps(self) -> OxiFrameRate {
        match self {
            SelectedFps::Fps24 => OxiFrameRate::Fps24,
            SelectedFps::Fps25 => OxiFrameRate::Fps25,
            SelectedFps::Fps2997Ndf => OxiFrameRate::Fps2997NDF,
            SelectedFps::Fps2997Df => OxiFrameRate::Fps2997DF,
            SelectedFps::Fps30 => OxiFrameRate::Fps30,
        }
    }

    fn label(&self) -> &'static str {
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

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct LtcApp {
    selected_fps: SelectedFps,
    selected_timezone: String,
    audio_enabled: bool,

    #[serde(skip)]
    shared_fps: Arc<AtomicU8>,
    #[serde(skip)]
    shared_timezone: Arc<AtomicUsize>,
    #[serde(skip)]
    shared_enabled: Arc<AtomicBool>,
    #[serde(skip)]
    offset_ms: Arc<AtomicI64>,
    #[serde(skip)]
    local_base: Instant,
    #[serde(skip)]
    status_text: Arc<Mutex<String>>,
    #[serde(skip)]
    _stream: Option<cpal::Stream>,
}

impl Default for LtcApp {
    fn default() -> Self {
        Self {
            selected_fps: SelectedFps::Fps2997Df,
            selected_timezone: system_timezone_name(),
            audio_enabled: true,
            shared_fps: Arc::new(AtomicU8::new(SelectedFps::Fps2997Df as u8)),
            shared_timezone: Arc::new(AtomicUsize::new(
                timezone_index(&system_timezone_name()).unwrap_or(0),
            )),
            shared_enabled: Arc::new(AtomicBool::new(true)),
            offset_ms: Arc::new(AtomicI64::new(0)),
            local_base: Instant::now(),
            status_text: Arc::new(Mutex::new("Syncing via OxiMedia NTP...".into())),
            _stream: None,
        }
    }
}

impl LtcApp {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let mut app: Self = if let Some(storage) = cc.storage {
            eframe::get_value(storage, eframe::APP_KEY).unwrap_or_default()
        } else {
            Self::default()
        };

        app.local_base = Instant::now();
        app.shared_fps
            .store(app.selected_fps as u8, Ordering::Relaxed);
        if timezone_index(&app.selected_timezone).is_none() {
            app.selected_timezone = system_timezone_name();
        }
        app.shared_timezone.store(
            timezone_index(&app.selected_timezone).unwrap_or(0),
            Ordering::Relaxed,
        );
        app.shared_enabled
            .store(app.audio_enabled, Ordering::Relaxed);

        let offset_clone = app.offset_ms.clone();
        let status_clone = app.status_text.clone();

        // Background NTP Sync
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new();
            match rt {
                Ok(runtime) => {
                    let result = runtime.block_on(async {
                        let mut config = NtpClientConfig::default();
                        config.timeout = Duration::from_secs(3);
                        config.max_retries = 1;
                        let mut ntp = NtpClient::with_config(config);
                        let mut dns_errors = Vec::new();
                        let mut resolved_server_count = 0;

                        for server in [
                            "time.google.com:123",
                            "time.cloudflare.com:123",
                            "pool.ntp.org:123",
                        ] {
                            match tokio::net::lookup_host(server).await {
                                Ok(addresses) => {
                                    let addresses: Vec<_> = addresses.collect();
                                    if addresses.is_empty() {
                                        dns_errors.push(format!("{server}: no addresses returned"));
                                    }
                                    for address in addresses {
                                        ntp.add_server(address);
                                        resolved_server_count += 1;
                                    }
                                }
                                Err(error) => {
                                    dns_errors.push(format!("{server}: {error}"));
                                }
                            }
                        }

                        if resolved_server_count == 0 {
                            return Err(format!(
                                "Could not resolve any NTP servers ({})",
                                dns_errors.join("; ")
                            ));
                        }

                        ntp.synchronize().await.map_err(|error| {
                            if dns_errors.is_empty() {
                                error.to_string()
                            } else {
                                format!("{error}; DNS lookup failures: {}", dns_errors.join("; "))
                            }
                        })
                    });

                    match result {
                        Ok(sync) => {
                            let ntp_sys = if sync.offset.is_finite() {
                                let now = SystemTime::now();
                                let adjustment = Duration::from_secs_f64(sync.offset.abs());
                                if sync.offset < 0.0 {
                                    now.checked_sub(adjustment)
                                } else {
                                    now.checked_add(adjustment)
                                }
                            } else {
                                None
                            };

                            if let Some(time) = ntp_sys.map(DateTime::<Utc>::from) {
                                offset_clone.store(time.timestamp_millis(), Ordering::Relaxed);
                                if let Ok(mut status) = status_clone.lock() {
                                    *status = "NTP Locked (pool.ntp.org)".into();
                                }
                            } else {
                                use_system_time_fallback(
                                    &offset_clone,
                                    &status_clone,
                                    "invalid NTP time adjustment",
                                );
                            }
                        }
                        Err(e) => {
                            use_system_time_fallback(&offset_clone, &status_clone, &e);
                        }
                    }
                }
                Err(e) => {
                    use_system_time_fallback(&offset_clone, &status_clone, &e);
                }
            }
        });

        // Audio Stream Initialization
        if let Ok(host) = std::panic::catch_unwind(cpal::default_host) {
            if let Some(device) = host.default_output_device() {
                if let Ok(config) = device.default_output_config() {
                    let sample_rate = config.sample_rate().0;
                    let shared_fps = app.shared_fps.clone();
                    let shared_timezone = app.shared_timezone.clone();
                    let shared_enabled_flag = app.shared_enabled.clone();
                    let offset_ms = app.offset_ms.clone();
                    let base_instant = app.local_base;

                    let stream = device.build_output_stream(
                        &config.into(),
                        move |data: &mut [f32], _| {
                            if !shared_enabled_flag.load(Ordering::Relaxed) {
                                for sample in data.iter_mut() {
                                    *sample = 0.0;
                                }
                                return;
                            }

                            let fps = SelectedFps::from_u8(shared_fps.load(Ordering::Relaxed));
                            let timezone_index =
                                shared_timezone.load(Ordering::Relaxed) % TZ_VARIANTS.len();
                            let Some(local_time) = current_utc_time(
                                offset_ms.load(Ordering::Relaxed),
                                base_instant.elapsed(),
                            )
                            .map(|time| time.with_timezone(&TZ_VARIANTS[timezone_index])) else {
                                data.fill(0.0);
                                return;
                            };
                            let h = local_time.hour() as u8;
                            let m = local_time.minute() as u8;
                            let s = local_time.second() as u8;
                            let ms = local_time.timestamp_subsec_millis();

                            let frame_rate_enum = fps.to_oximedia_fps();
                            let frames_per_sec = match fps {
                                SelectedFps::Fps24 => 24.0,
                                SelectedFps::Fps25 => 25.0,
                                SelectedFps::Fps2997Ndf | SelectedFps::Fps2997Df => 29.97,
                                SelectedFps::Fps30 => 30.0,
                            };

                            let f = ((ms as f32 / 1000.0) * frames_per_sec) as u8;

                            if let Ok(tc) = Timecode::new(h, m, s, f, frame_rate_enum) {
                                let mut ltc_gen = LtcGenerator::new(sample_rate, frame_rate_enum);
                                let _ = ltc_gen.generate(&tc, data);
                            }
                        },
                        |_| {},
                        None,
                    );

                    if let Ok(s) = stream {
                        let _ = s.play();
                        app._stream = Some(s);
                    }
                }
            }
        }

        app
    }
}

fn use_system_time_fallback(
    offset_ms: &AtomicI64,
    status_text: &Mutex<String>,
    error: impl std::fmt::Display,
) {
    offset_ms.store(Utc::now().timestamp_millis(), Ordering::Relaxed);
    let status = format!("NTP Sync Error: {}; falling back to system time", error);

    if let Ok(mut current_status) = status_text.lock() {
        *current_status = status;
    }
}

impl eframe::App for LtcApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // UI refresh rate lowered since we only display seconds now
        ctx.request_repaint_after(Duration::from_millis(100));

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("OxiMedia NTP LTC Generator");

            // Standard Mutex safe-lock for UI
            if let Ok(status) = self.status_text.lock() {
                ui.label(egui::RichText::new(status.as_str()).small());
            }

            ui.separator();
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Frame Rate:");
                let prev = self.selected_fps;
                egui::ComboBox::from_id_source("fps_selector")
                    .selected_text(self.selected_fps.label())
                    .show_ui(ui, |ui| {
                        for fps in [
                            SelectedFps::Fps24,
                            SelectedFps::Fps25,
                            SelectedFps::Fps2997Ndf,
                            SelectedFps::Fps2997Df,
                            SelectedFps::Fps30,
                        ] {
                            ui.selectable_value(&mut self.selected_fps, fps, fps.label());
                        }
                    });

                ui.horizontal(|ui| {
                    ui.label("Timezone:");
                    let previous_timezone = self.selected_timezone.clone();
                    egui::ComboBox::from_id_source("timezone_selector")
                        .selected_text(&self.selected_timezone)
                        .show_ui(ui, |ui| {
                            for timezone in TZ_VARIANTS {
                                let timezone_name = timezone.to_string();
                                ui.selectable_value(
                                    &mut self.selected_timezone,
                                    timezone_name.clone(),
                                    timezone_name,
                                );
                            }
                        });
                    if previous_timezone != self.selected_timezone {
                        if let Some(index) = timezone_index(&self.selected_timezone) {
                            self.shared_timezone.store(index, Ordering::Relaxed);
                        }
                    }
                });

                if prev != self.selected_fps {
                    self.shared_fps
                        .store(self.selected_fps as u8, Ordering::Relaxed);
                }
            });

            if ui
                .checkbox(&mut self.audio_enabled, "Output LTC to Audio Jack")
                .changed()
            {
                self.shared_enabled
                    .store(self.audio_enabled, Ordering::Relaxed);
            }

            ui.add_space(12.0);

            let timezone_index = self.shared_timezone.load(Ordering::Relaxed) % TZ_VARIANTS.len();
            let clock_text = current_utc_time(
                self.offset_ms.load(Ordering::Relaxed),
                self.local_base.elapsed(),
            )
            .map(|time| time.with_timezone(&TZ_VARIANTS[timezone_index]))
            .map(|time| {
                format!(
                    "{:02}:{:02}:{:02}",
                    time.hour(),
                    time.minute(),
                    time.second()
                )
            })
            .unwrap_or_else(|| "Invalid time".to_owned());

            // Simplified display: HH:MM:SS
            ui.group(|ui| {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        egui::RichText::new(clock_text)
                            .size(42.0)
                            .monospace()
                            .strong(),
                    );
                });
            });
        });
    }
}

fn current_utc_time(offset_ms: i64, elapsed: Duration) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp_millis(offset_ms)?
        .checked_add_signed(chrono::Duration::from_std(elapsed).ok()?)
}

fn system_timezone_name() -> String {
    iana_time_zone::get_timezone().unwrap_or_else(|_| "UTC".to_owned())
}

fn timezone_index(name: &str) -> Option<usize> {
    TZ_VARIANTS
        .iter()
        .position(|timezone| timezone.to_string() == name)
}

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([360.0, 220.0]),
        ..Default::default()
    };
    eframe::run_native(
        "NTP LTC Generator",
        options,
        Box::new(|cc| Ok(Box::new(LtcApp::new(cc)))),
    )
}
