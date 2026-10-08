use crate::audio::{AudioWorkerGuard, SharedAudioState, start_audio_worker};
use crate::ltc::{SelectedFps, current_utc_time, system_timezone_name, timezone_index};
use crate::ntp::{self, store_time_at_base};
use crate::status::{StatusText, set_audio_status};
use chrono::Timelike;
use chrono::Utc;
use chrono_tz::TZ_VARIANTS;
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const APP_NAME: &str = "LTC Generator w/ NTP";

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct LtcApp {
    selected_fps: SelectedFps,
    selected_timezone: String,
    #[serde(skip)]
    timezone_search: String,
    #[serde(skip)]
    timezone_picker_open: bool,
    #[serde(skip)]
    timezone_search_needs_focus: bool,
    #[serde(skip)]
    timezone_scroll_to_selection: bool,
    #[serde(skip)]
    audio_enabled: bool,
    volume_dbfs: i32,

    #[serde(skip)]
    shared_fps: Arc<AtomicU8>,
    #[serde(skip)]
    shared_timezone: Arc<AtomicUsize>,
    #[serde(skip)]
    shared_enabled: Arc<AtomicBool>,
    #[serde(skip)]
    shared_volume_dbfs: Arc<AtomicI32>,
    #[serde(skip)]
    offset_ms: Arc<AtomicI64>,
    #[serde(skip)]
    local_base: Instant,
    #[serde(skip)]
    status_text: Arc<Mutex<StatusText>>,
    #[serde(skip)]
    audio_available: Arc<AtomicBool>,
    #[serde(skip)]
    audio_stream_error: Arc<AtomicBool>,
    #[serde(skip)]
    audio_worker_running: Arc<AtomicBool>,
    #[serde(skip)]
    audio_worker_guard: AudioWorkerGuard,
}

impl Default for LtcApp {
    fn default() -> Self {
        let local_base = Instant::now();
        let offset_ms = Arc::new(AtomicI64::new(0));
        let shared_enabled = Arc::new(AtomicBool::new(false));
        let audio_worker_running = Arc::new(AtomicBool::new(true));
        store_time_at_base(&offset_ms, Utc::now(), local_base);

        Self {
            selected_fps: SelectedFps::Fps2997Df,
            selected_timezone: system_timezone_name(),
            timezone_search: String::new(),
            timezone_picker_open: false,
            timezone_search_needs_focus: false,
            timezone_scroll_to_selection: false,
            audio_enabled: false,
            volume_dbfs: -6,
            shared_fps: Arc::new(AtomicU8::new(SelectedFps::Fps2997Df as u8)),
            shared_timezone: Arc::new(AtomicUsize::new(
                timezone_index(&system_timezone_name()).unwrap_or(0),
            )),
            shared_enabled: shared_enabled.clone(),
            shared_volume_dbfs: Arc::new(AtomicI32::new(-6)),
            offset_ms,
            local_base,
            status_text: Arc::new(Mutex::new(StatusText::new(
                "NTP: Syncing via OxiMedia NTP...",
            ))),
            audio_available: Arc::new(AtomicBool::new(false)),
            audio_stream_error: Arc::new(AtomicBool::new(false)),
            audio_worker_running: audio_worker_running.clone(),
            audio_worker_guard: AudioWorkerGuard::new(audio_worker_running, shared_enabled),
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
        store_time_at_base(&app.offset_ms, Utc::now(), app.local_base);
        app.shared_fps
            .store(app.selected_fps as u8, Ordering::Relaxed);
        if timezone_index(&app.selected_timezone).is_none() {
            app.selected_timezone = system_timezone_name();
        }
        app.shared_timezone.store(
            timezone_index(&app.selected_timezone).unwrap_or(0),
            Ordering::Relaxed,
        );
        app.audio_enabled = false;
        app.shared_enabled.store(false, Ordering::Relaxed);
        app.volume_dbfs = app.volume_dbfs.clamp(-60, 0);
        app.shared_volume_dbfs
            .store(app.volume_dbfs, Ordering::Relaxed);

        ntp::start_ntp_sync(
            app.offset_ms.clone(),
            app.status_text.clone(),
            app.local_base,
        );

        start_audio_worker(app.audio_shared_state());

        app
    }

    fn audio_shared_state(&self) -> SharedAudioState {
        SharedAudioState {
            fps: self.shared_fps.clone(),
            timezone: self.shared_timezone.clone(),
            enabled: self.shared_enabled.clone(),
            volume_dbfs: self.shared_volume_dbfs.clone(),
            offset_ms: self.offset_ms.clone(),
            local_base: self.local_base,
            status_text: self.status_text.clone(),
            stream_error: self.audio_stream_error.clone(),
            available: self.audio_available.clone(),
            worker_running: self.audio_worker_running.clone(),
        }
    }
}

impl eframe::App for LtcApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, self);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // UI refresh rate lowered since we only display seconds now
        let ctx = ui.ctx().clone();
        ctx.request_repaint_after(Duration::from_millis(100));
        self.audio_enabled = self.shared_enabled.load(Ordering::Relaxed);
        let audio_available = self.audio_available.load(Ordering::Relaxed);
        if !audio_available && self.audio_enabled {
            self.audio_enabled = false;
            self.shared_enabled.store(false, Ordering::Relaxed);
        }

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(APP_NAME);

            // Standard Mutex safe-lock for UI
            if let Ok(status) = self.status_text.lock() {
                ui.label(egui::RichText::new(status.combined()).small());
            }

            ui.separator();
            ui.add_space(8.0);

            ui.horizontal(|ui| {
                ui.label("Frame Rate:");
                let prev = self.selected_fps;
                egui::ComboBox::from_id_salt("fps_selector")
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

                if prev != self.selected_fps {
                    self.shared_fps
                        .store(self.selected_fps as u8, Ordering::Relaxed);
                }
            });

            let mut timezone_picker_pos = None;
            let mut timezone_control_rect = None;
            ui.horizontal(|ui| {
                ui.label("Timezone:");
                let previous_timezone = self.selected_timezone.clone();
                if self.timezone_picker_open {
                    let search_field = ui.add_sized(
                        [220.0, ui.spacing().interact_size.y],
                        egui::TextEdit::singleline(&mut self.timezone_search)
                            .id_source("timezone_search"),
                    );
                    timezone_picker_pos = Some(search_field.rect.left_bottom());
                    timezone_control_rect = Some(search_field.rect);
                    if self.timezone_search_needs_focus || search_field.clicked() {
                        search_field.request_focus();
                        self.timezone_search_needs_focus = false;
                    }
                } else {
                    let selector = ui.add_sized(
                        [220.0, ui.spacing().interact_size.y],
                        egui::Button::new(&self.selected_timezone),
                    );
                    timezone_control_rect = Some(selector.rect);
                    if selector.clicked() {
                        self.timezone_search.clear();
                        self.timezone_picker_open = true;
                        self.timezone_search_needs_focus = true;
                        self.timezone_scroll_to_selection = true;
                    }
                }

                if ui.button("System default").clicked() {
                    self.selected_timezone = system_timezone_name();
                    self.timezone_picker_open = false;
                }

                if previous_timezone != self.selected_timezone {
                    self.timezone_search.clear();
                }
            });

            let mut timezone_popup_rect = None;
            if self.timezone_picker_open {
                if let Some(position) = timezone_picker_pos {
                    let search = self.timezone_search.to_lowercase();
                    let popup = egui::Area::new(egui::Id::new("timezone_picker_popup"))
                        .order(egui::Order::Foreground)
                        .fixed_pos(position + egui::vec2(0.0, 4.0))
                        .show(&ctx, |ui| {
                            egui::Frame::popup(ui.style()).show(ui, |ui| {
                                ui.set_min_width(260.0);
                                let mut selected = None;
                                egui::ScrollArea::vertical()
                                    .id_salt("timezone_results")
                                    .min_scrolled_height(240.0)
                                    .max_height(240.0)
                                    .auto_shrink([false, false])
                                    .show(ui, |ui| {
                                        for timezone in TZ_VARIANTS {
                                            let timezone_name = timezone.to_string();
                                            if !timezone_name.to_lowercase().contains(&search) {
                                                continue;
                                            }
                                            let response = ui.selectable_label(
                                                self.selected_timezone == timezone_name,
                                                &timezone_name,
                                            );
                                            if self.timezone_scroll_to_selection
                                                && self.selected_timezone == timezone_name
                                            {
                                                response.scroll_to_me(Some(egui::Align::Center));
                                                self.timezone_scroll_to_selection = false;
                                            }
                                            if response.clicked() {
                                                selected = Some(timezone_name);
                                            }
                                        }
                                    });
                                if let Some(timezone) = selected {
                                    self.selected_timezone = timezone;
                                    self.timezone_picker_open = false;
                                    self.timezone_search.clear();
                                }
                            })
                        });
                    timezone_popup_rect =
                        Some(popup.response.rect.union(popup.inner.response.rect));
                }
            }
            if self.timezone_picker_open
                && ctx.input(|input| {
                    input.key_pressed(egui::Key::Enter) || input.key_pressed(egui::Key::Escape)
                })
            {
                self.timezone_picker_open = false;
            }
            if self.timezone_picker_open {
                let click_outside = ctx.input(|input| {
                    input.pointer.button_clicked(egui::PointerButton::Primary)
                        && input.pointer.interact_pos().is_some_and(|position| {
                            !timezone_control_rect.is_some_and(|rect| rect.contains(position))
                                && !timezone_popup_rect.is_some_and(|rect| rect.contains(position))
                        })
                });
                if click_outside {
                    self.timezone_picker_open = false;
                }
            }
            if let Some(index) = timezone_index(&self.selected_timezone) {
                self.shared_timezone.store(index, Ordering::Relaxed);
            }

            ui.horizontal(|ui| {
                ui.label("LTC Output Level:");
                let previous_volume = self.volume_dbfs;
                ui.add(egui::Slider::new(&mut self.volume_dbfs, -60..=0).suffix(" dBFS"));
                if previous_volume != self.volume_dbfs {
                    self.shared_volume_dbfs
                        .store(self.volume_dbfs, Ordering::Relaxed);
                }
            });

            ui.add_space(12.0);

            audio_output_control(
                ui,
                &mut self.audio_enabled,
                audio_available,
                &self.shared_enabled,
                &self.status_text,
            );

            let timezone_index = self.shared_timezone.load(Ordering::Relaxed) % TZ_VARIANTS.len();
            let clock_text = current_utc_time(
                self.offset_ms.load(Ordering::Relaxed),
                self.local_base.elapsed(),
            )
            .map(|time| time.with_timezone(&TZ_VARIANTS[timezone_index]))
            .map(|time| {
                format!(
                    "{:02}:{:02}:{:02}.{}",
                    time.hour(),
                    time.minute(),
                    time.second(),
                    time.timestamp_subsec_millis() / 100
                )
            })
            .unwrap_or_else(|| "Invalid time".to_owned());

            // Display tenths of a second; the repaint interval is 100 ms.
            ui.group(|ui| {
                ui.vertical_centered(|ui| {
                    let (audio_status, clock_color) = if self.audio_enabled {
                        ("AUDIO ENABLED", egui::Color32::RED)
                    } else {
                        ("AUDIO DISABLED", ui.visuals().weak_text_color())
                    };
                    ui.label(
                        egui::RichText::new(audio_status)
                            .small()
                            .strong()
                            .color(clock_color),
                    );
                    ui.label(
                        egui::RichText::new(clock_text)
                            .size(42.0)
                            .monospace()
                            .strong()
                            .color(clock_color),
                    );
                });
            });
        });
    }
}

fn audio_output_control(
    ui: &mut egui::Ui,
    audio_enabled: &mut bool,
    audio_available: bool,
    shared_enabled: &AtomicBool,
    status_text: &Mutex<StatusText>,
) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.add_space(ui.available_width() * 0.1);
            if ui
                .add_enabled(
                    audio_available,
                    egui::Checkbox::new(
                        audio_enabled,
                        egui::RichText::new("Enable LTC Audio Output")
                            .size(18.0)
                            .strong(),
                    ),
                )
                .changed()
            {
                shared_enabled.store(*audio_enabled, Ordering::Relaxed);
                let status = if *audio_enabled {
                    "Audio output stream is playing"
                } else {
                    "Audio output ready (muted)"
                };
                set_audio_status(status_text, status);
            }
        });
        if !audio_available {
            ui.label(
                egui::RichText::new(
                    "Audio output unavailable. Connect an output device to enable this control.",
                )
                .small()
                .color(ui.visuals().error_fg_color),
            );
        }
    });
}

pub(crate) fn run() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([360.0, 500.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(LtcApp::new(cc)))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::accesskit::Toggled;
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };

    #[test]
    fn enabling_audio_output_updates_the_app_and_status() {
        let mut audio_enabled = false;
        let shared_enabled = AtomicBool::new(false);
        let status_text = Mutex::new(StatusText::new("NTP: Locked"));
        let mut harness = Harness::new_ui(|ui| {
            audio_output_control(ui, &mut audio_enabled, true, &shared_enabled, &status_text);
        });

        let checkbox = harness.get_by_label("Enable LTC Audio Output");
        assert_eq!(checkbox.accesskit_node().toggled(), Some(Toggled::False));
        checkbox.click();
        harness.run();

        let checkbox = harness.get_by_label("Enable LTC Audio Output");
        assert_eq!(checkbox.accesskit_node().toggled(), Some(Toggled::True));
        drop(harness);
        assert!(audio_enabled);
        assert!(shared_enabled.load(Ordering::Relaxed));
        assert_eq!(
            status_text.lock().unwrap().combined(),
            "NTP: Locked | Audio output: Audio output stream is playing"
        );
    }
}
