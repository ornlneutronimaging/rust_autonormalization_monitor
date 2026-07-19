//! VENUS Auto Normalization Monitor — shows whether the auto-normalization
//! pipeline is active (the `activate` flag in the shared
//! `autoreduction.cfg`) as a big ON/OFF button at the top of the window.
//!
//! The state is read-only for regular users: the flag can only be changed
//! after unlocking admin mode with the admin password (stored as a SHA-256
//! hash, never in clear text). Once unlocked, clicking the ON/OFF button
//! flips the flag and writes it back to the configuration file.
//!
//! The file is re-read every couple of seconds so the display always reflects
//! changes made by other tools (e.g. the marimo normalization notebook).

mod config;
mod theme;

use eframe::egui;
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};

const CONFIG_PATH: &str = "/SNS/VENUS/shared/autoreduction/autoreduction.cfg";
const LOGO_PATH: &str = "/SNS/VENUS/shared/software/logos/logo_with_green_neutron_rays.png";
const APP_TITLE: &str = "VENUS Auto Normalization Monitor";
/// SHA-256 of the admin password — the password itself never appears in the
/// source or the compiled binary, only this digest.
const ADMIN_PASSWORD_SHA256: &str =
    "b8b22aedc372aa891df895be9a7626e6d9ddc6d39ba85d202ca68de8c52ad782";
/// How often the configuration file is re-read.
const REFRESH_EVERY: Duration = Duration::from_secs(2);

fn password_matches(candidate: &str) -> bool {
    let digest = Sha256::digest(candidate.as_bytes());
    // Constant-length hex compare against the stored digest.
    format!("{digest:x}") == ADMIN_PASSWORD_SHA256
}

/// A static logo image loaded into a texture, plus its aspect ratio for sizing.
struct Logo {
    texture: egui::TextureHandle,
    aspect: f32, // width / height
}

impl Logo {
    /// Load the image at `path` into a GPU texture. Returns `None` if the file
    /// is missing or cannot be decoded.
    fn load(ctx: &egui::Context, path: &str) -> Option<Self> {
        let img = image::open(path).ok()?.to_rgba8();
        let (w, h) = (img.width(), img.height());
        let color_image =
            egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw());
        let texture = ctx.load_texture("logo", color_image, egui::TextureOptions::LINEAR);
        let aspect = if h > 0 { w as f32 / h as f32 } else { 1.0 };
        Some(Self { texture, aspect })
    }
}

struct MonitorApp {
    logo: Option<Logo>,
    logo_loaded: bool,
    /// Latest read of the configuration file (Err = message shown in the UI).
    cfg: Result<config::AutoNormConfig, String>,
    last_refresh: Instant,
    /// Admin mode: unlocked by password, allows toggling the flag.
    admin_unlocked: bool,
    password_input: String,
    password_error: bool,
    /// Error from the last write attempt, shown until the next successful one.
    write_error: Option<String>,
}

impl MonitorApp {
    fn new() -> Self {
        Self {
            logo: None,
            logo_loaded: false,
            cfg: config::read(Path::new(CONFIG_PATH)),
            last_refresh: Instant::now(),
            admin_unlocked: false,
            password_input: String::new(),
            password_error: false,
            write_error: None,
        }
    }

    fn refresh(&mut self) {
        self.cfg = config::read(Path::new(CONFIG_PATH));
        self.last_refresh = Instant::now();
    }

    fn try_unlock(&mut self) {
        if password_matches(self.password_input.trim()) {
            self.admin_unlocked = true;
            self.password_error = false;
        } else {
            self.password_error = true;
        }
        self.password_input.clear();
    }

    /// Flip the `activate` flag on disk, then re-read the file so the button
    /// shows what is actually stored.
    fn toggle_activate(&mut self, current: bool) {
        match config::set_activate(Path::new(CONFIG_PATH), !current) {
            Ok(()) => self.write_error = None,
            Err(e) => self.write_error = Some(e),
        }
        self.refresh();
    }

    /// Branded header: full-width ORNL Green banner, white title with a soft
    /// drop shadow, neutron imaging logo in the top-right corner (template
    /// shared by the VENUS rust applications).
    fn header(&self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("header")
            .frame(
                egui::Frame::new()
                    .fill(theme::PRIMARY_RICH)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // Title with a soft drop shadow: egui has no text shadow, so
                    // paint the text twice — a dark offset copy behind the white.
                    let font = egui::FontId::proportional(28.0);
                    let shadow_offset = egui::vec2(2.0, 2.0);
                    let galley = ui.painter().layout_no_wrap(
                        APP_TITLE.to_string(),
                        font.clone(),
                        theme::TEXT_WHITE,
                    );
                    let (rect, _) =
                        ui.allocate_exact_size(galley.size() + shadow_offset, egui::Sense::hover());
                    let pos = rect.min;
                    ui.painter().text(
                        pos + shadow_offset,
                        egui::Align2::LEFT_TOP,
                        APP_TITLE,
                        font.clone(),
                        egui::Color32::from_black_alpha(140),
                    );
                    ui.painter()
                        .text(pos, egui::Align2::LEFT_TOP, APP_TITLE, font, theme::TEXT_WHITE);
                    if let Some(logo) = &self.logo {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let height = 44.0;
                            let size = egui::vec2(height * logo.aspect, height);
                            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
                            let uv = egui::Rect::from_min_max(
                                egui::pos2(0.0, 0.0),
                                egui::pos2(1.0, 1.0),
                            );
                            let shadow_offset = egui::vec2(2.0, 2.0);
                            // Drop shadow: the texture tinted black draws its
                            // alpha as a dark silhouette behind the logo.
                            ui.painter().image(
                                logo.texture.id(),
                                rect.translate(shadow_offset),
                                uv,
                                egui::Color32::from_black_alpha(140),
                            );
                            ui.painter()
                                .image(logo.texture.id(), rect, uv, egui::Color32::WHITE);
                        });
                    }
                });
            });
    }

    /// The big ON/OFF status button. Read-only unless admin mode is unlocked;
    /// when unlocked, clicking it toggles the flag in the configuration file.
    fn status_button(&mut self, ui: &mut egui::Ui, activate: bool) {
        let (label, fill) = if activate {
            ("ON", theme::SUCCESS)
        } else {
            ("OFF", theme::DANGER)
        };
        let text = egui::RichText::new(label)
            .color(theme::TEXT_WHITE)
            .strong()
            .size(34.0);
        let button = egui::Button::new(text)
            .fill(fill)
            .corner_radius(10.0)
            .min_size(egui::vec2(220.0, 64.0));
        ui.vertical_centered(|ui| {
            ui.label(theme::section_heading("Auto normalization status"));
            ui.add_space(theme::SPACE_SM);
            let response = ui.add_enabled(self.admin_unlocked, button);
            let response = if self.admin_unlocked {
                response.on_hover_text("Click to turn auto normalization ".to_owned()
                    + if activate { "OFF" } else { "ON" })
            } else {
                response.on_disabled_hover_text("Admin unlock required to change the state")
            };
            if response.clicked() {
                self.toggle_activate(activate);
            }
            ui.add_space(theme::SPACE_XS);
            ui.label(
                egui::RichText::new(if self.admin_unlocked {
                    "Admin mode: click the button to change the state"
                } else {
                    "Read-only — unlock admin mode below to change the state"
                })
                .color(theme::TEXT_EMPHASIS),
            );
        });
    }

    /// Read-only view of every `key: value` pair of the configuration file.
    fn details(&self, ui: &mut egui::Ui, cfg: &config::AutoNormConfig) {
        ui.label(theme::section_heading("Configuration"));
        ui.add_space(theme::SPACE_XS);
        theme::container_frame().show(ui, |ui| {
            egui::Grid::new("cfg_grid")
                .num_columns(2)
                .spacing([theme::SPACE_LG, theme::SPACE_XS])
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("file").color(theme::TEXT_EMPHASIS));
                    ui.label(CONFIG_PATH);
                    ui.end_row();
                    for (key, value) in &cfg.entries {
                        ui.label(egui::RichText::new(key).color(theme::TEXT_EMPHASIS));
                        ui.label(value);
                        ui.end_row();
                    }
                });
        });
    }

    /// Admin unlock (password prompt) / lock control.
    fn admin_section(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::section_heading("Admin"));
        ui.add_space(theme::SPACE_XS);
        theme::container_frame().show(ui, |ui| {
            if self.admin_unlocked {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("Admin mode unlocked")
                            .color(theme::PRIMARY_STRONG)
                            .strong(),
                    );
                    if ui.button("Lock").clicked() {
                        self.admin_unlocked = false;
                    }
                });
            } else {
                ui.horizontal(|ui| {
                    ui.label("Password:");
                    let edit = egui::TextEdit::singleline(&mut self.password_input)
                        .password(true)
                        .desired_width(180.0);
                    let response = ui.add(edit);
                    let submitted =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    if ui.button("Unlock").clicked() || submitted {
                        self.try_unlock();
                    }
                });
                if self.password_error {
                    ui.label(
                        egui::RichText::new("Incorrect password").color(theme::DANGER),
                    );
                }
            }
        });
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.logo_loaded {
            self.logo = Logo::load(ctx, LOGO_PATH);
            self.logo_loaded = true;
        }

        // Poll the file so changes made elsewhere show up without user action;
        // request_repaint keeps frames coming while the window is idle.
        if self.last_refresh.elapsed() >= REFRESH_EVERY {
            self.refresh();
        }
        ctx.request_repaint_after(REFRESH_EVERY);

        self.header(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(theme::SPACE_LG);
            match self.cfg.clone() {
                Ok(cfg) => {
                    self.status_button(ui, cfg.activate);
                    if let Some(err) = &self.write_error {
                        ui.add_space(theme::SPACE_SM);
                        ui.vertical_centered(|ui| {
                            ui.label(
                                egui::RichText::new(format!("Failed to update flag: {err}"))
                                    .color(theme::DANGER),
                            );
                        });
                    }
                    ui.add_space(theme::SPACE_LG);
                    self.details(ui, &cfg);
                }
                Err(e) => {
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new(format!("Cannot read configuration: {e}"))
                                .color(theme::DANGER),
                        );
                    });
                }
            }
            ui.add_space(theme::SPACE_LG);
            self.admin_section(ui);
        });
    }
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([760.0, 560.0])
            .with_title(APP_TITLE),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(|cc| {
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(MonitorApp::new()))
        }),
    )
}
