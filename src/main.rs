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
mod runs;
mod theme;

use eframe::egui;
use sha2::{Digest, Sha256};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, Instant};

const CONFIG_PATH: &str = "/SNS/VENUS/shared/autoreduction/autoreduction.cfg";
/// Root scanned for the IPTS-* experiment folders offered in the admin
/// "Autoreduction IPTS" selector.
const IPTS_ROOT: &str = "/SNS/VENUS";
const LOGO_PATH: &str = "/SNS/VENUS/shared/software/logos/logo_with_green_neutron_rays.png";
const APP_TITLE: &str = "VENUS Auto Normalization Monitor";
/// SHA-256 of the admin password — the password itself never appears in the
/// source or the compiled binary, only this digest.
const ADMIN_PASSWORD_SHA256: &str =
    "b8b22aedc372aa891df895be9a7626e6d9ddc6d39ba85d202ca68de8c52ad782";
/// How often the configuration file is re-read.
const REFRESH_EVERY: Duration = Duration::from_secs(2);
/// Number of most recently reduced runs shown in the Monitor table.
const MONITOR_RUN_COUNT: usize = 20;
/// Application launched to visualize a run's corrected / normalized data:
/// the rust_tiff_viewer, called with the data folder and, when the folder's
/// config.json / summary.json names one, the detector offset (µs, --offset).
const DATA_VISUALIZER_CMD: &str =
    "/SNS/VENUS/shared/software/git/rust_tiff_viewer/launch_rust_tiff_viewer.sh";

/// Which of a run's files is open in the viewer below the Monitor table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LogKind {
    Log,
    Err,
}

impl LogKind {
    fn label(self) -> &'static str {
        match self {
            LogKind::Log => "log",
            LogKind::Err => "error log",
        }
    }
}

/// POSIX `access(2)` check: can the current user read + enter this directory?
fn can_access(path: &Path) -> bool {
    let Ok(cstr) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(cstr.as_ptr(), libc::R_OK | libc::X_OK) == 0 }
}

/// List the IPTS-* folders under `root` the current user can access, sorted
/// by IPTS number (same pattern as the marimo portal template).
fn list_accessible_ipts(root: &Path) -> Result<Vec<String>, String> {
    let dir = std::fs::read_dir(root)
        .map_err(|e| format!("cannot read {}: {e}", root.display()))?;
    let mut ipts: Vec<(u64, String)> = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some(suffix) = name_str.strip_prefix("IPTS-") else {
            continue;
        };
        if !can_access(&entry.path()) {
            continue;
        }
        let num: u64 = suffix.parse().unwrap_or(u64::MAX);
        ipts.push((num, name_str.into_owned()));
    }
    ipts.sort_by_key(|(n, _)| *n);
    Ok(ipts.into_iter().map(|(_, name)| name).collect())
}

fn password_matches(candidate: &str) -> bool {
    let digest = Sha256::digest(candidate.as_bytes());
    // Constant-length hex compare against the stored digest.
    format!("{digest:x}") == ADMIN_PASSWORD_SHA256
}

/// Top-level tabs: Admin (status + admin-gated toggle) and Monitor (live view
/// of the normalization state, to be implemented).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Admin,
    Monitor,
}

const TABS: &[(Tab, &str)] = &[(Tab::Admin, "Admin"), (Tab::Monitor, "Monitor")];

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
    tab: Tab,
    /// Latest read of the configuration file (Err = message shown in the UI).
    cfg: Result<config::AutoNormConfig, String>,
    last_refresh: Instant,
    /// Admin mode: unlocked by password, allows toggling the flag.
    admin_unlocked: bool,
    password_input: String,
    password_error: bool,
    /// Error from the last write attempt, shown until the next successful one.
    write_error: Option<String>,
    /// IPTS folders the current user can access (scanned on admin unlock).
    ipts_list: Result<Vec<String>, String>,
    /// Text typed by the admin to narrow the IPTS list (matched on the number).
    ipts_filter: String,
    /// Last-reduced runs shown in the Monitor tab (refreshed with the config).
    runs: Result<Vec<runs::RunEntry>, String>,
    /// File open in the viewer below the Monitor table: (run number, kind).
    viewer: Option<(u64, LogKind)>,
    /// Content of the viewed file (re-read on every refresh so a run that is
    /// still reducing streams into the viewer).
    viewer_content: String,
    /// Error from the last attempt to launch the data visualizer.
    launch_error: Option<String>,
    /// Corrected/normalized data folders parsed from each run's log
    /// (rebuilt on every refresh).
    run_folders: std::collections::HashMap<u64, runs::LogFolders>,
}

impl MonitorApp {
    fn new() -> Self {
        let mut app = Self {
            logo: None,
            logo_loaded: false,
            tab: Tab::Admin,
            cfg: Ok(config::AutoNormConfig::default()),
            last_refresh: Instant::now(),
            admin_unlocked: false,
            password_input: String::new(),
            password_error: false,
            write_error: None,
            ipts_list: Ok(Vec::new()),
            ipts_filter: String::new(),
            runs: Ok(Vec::new()),
            viewer: None,
            viewer_content: String::new(),
            launch_error: None,
            run_folders: std::collections::HashMap::new(),
        };
        app.refresh();
        app
    }

    /// `/SNS/VENUS/<ipts>/shared/autoreduce/reduction_log` for the IPTS
    /// currently named in the configuration file.
    fn reduction_log_dir(&self) -> Option<std::path::PathBuf> {
        let ipts = self.cfg.as_ref().ok()?.get("ipts")?;
        Some(
            Path::new(IPTS_ROOT)
                .join(ipts)
                .join("shared/autoreduce/reduction_log"),
        )
    }

    fn refresh(&mut self) {
        self.cfg = config::read(Path::new(CONFIG_PATH));
        self.runs = match self.reduction_log_dir() {
            Some(dir) => runs::last_runs(&dir, MONITOR_RUN_COUNT),
            None => Err("no IPTS defined in the configuration file".to_owned()),
        };
        self.run_folders.clear();
        if let Ok(run_list) = &self.runs {
            for run in run_list {
                if let Some(log_path) = &run.log_path {
                    self.run_folders
                        .insert(run.run_number, runs::folders_from_log(log_path));
                }
            }
        }
        self.reload_viewer();
        self.last_refresh = Instant::now();
    }

    /// (Re)read the file selected in the Monitor viewer. Clears the selection
    /// if its run dropped out of the table.
    fn reload_viewer(&mut self) {
        let Some((run_number, kind)) = self.viewer else {
            return;
        };
        let path = self
            .runs
            .as_ref()
            .ok()
            .and_then(|runs| runs.iter().find(|r| r.run_number == run_number))
            .and_then(|r| match kind {
                LogKind::Log => r.log_path.clone(),
                LogKind::Err => r.err_path.clone(),
            });
        match path {
            Some(path) => {
                self.viewer_content = std::fs::read_to_string(&path)
                    .unwrap_or_else(|e| format!("cannot read {}: {e}", path.display()));
            }
            None => {
                self.viewer = None;
                self.viewer_content.clear();
            }
        }
    }

    fn try_unlock(&mut self) {
        if password_matches(self.password_input.trim()) {
            self.admin_unlocked = true;
            self.password_error = false;
            // Fresh scan on every unlock so newly granted IPTS show up.
            self.ipts_list = list_accessible_ipts(Path::new(IPTS_ROOT));
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

    /// Admin-only: choose which IPTS the autoreduction should use, among the
    /// IPTS-* folders the current user can access. Selecting one writes the
    /// `ipts` field of the configuration file.
    fn ipts_section(&mut self, ui: &mut egui::Ui, current: &str) {
        ui.label(theme::section_heading("Autoreduction IPTS"));
        ui.add_space(theme::SPACE_XS);
        theme::container_frame().show(ui, |ui| {
            match &self.ipts_list {
                Ok(list) if list.is_empty() => {
                    ui.label(
                        egui::RichText::new(format!(
                            "No accessible IPTS found under {IPTS_ROOT}"
                        ))
                        .color(theme::WARNING),
                    );
                }
                Ok(list) => {
                    // Type-to-filter: keep the entries whose IPTS number
                    // contains the typed text (e.g. "369" → IPTS-36967).
                    let filter = self
                        .ipts_filter
                        .trim()
                        .trim_start_matches("IPTS-")
                        .trim_start_matches("ipts-")
                        .to_owned();
                    let filtered: Vec<&String> =
                        list.iter().filter(|name| name.contains(&filter)).collect();
                    let mut selected: Option<String> = None;
                    ui.horizontal(|ui| {
                        ui.label("IPTS to use:");
                        egui::ComboBox::from_id_salt("ipts_combo")
                            .selected_text(if current.is_empty() {
                                "— select —"
                            } else {
                                current
                            })
                            .show_ui(ui, |ui| {
                                for name in &filtered {
                                    if ui
                                        .selectable_label(*name == current, *name)
                                        .clicked()
                                        && *name != current
                                    {
                                        selected = Some((*name).clone());
                                    }
                                }
                            });
                        ui.label("Filter:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ipts_filter)
                                .hint_text("type IPTS number…")
                                .desired_width(130.0),
                        );
                        ui.label(
                            egui::RichText::new(if filter.is_empty() {
                                format!("({} accessible)", list.len())
                            } else {
                                format!("({} of {} match)", filtered.len(), list.len())
                            })
                            .color(theme::TEXT_EMPHASIS),
                        );
                    });
                    if !filter.is_empty() && filtered.is_empty() {
                        ui.label(
                            egui::RichText::new("No accessible IPTS matches the filter")
                                .color(theme::WARNING),
                        );
                    }
                    if let Some(name) = selected {
                        match config::set_value(Path::new(CONFIG_PATH), "ipts", &name) {
                            Ok(()) => self.write_error = None,
                            Err(e) => self.write_error = Some(e),
                        }
                        self.refresh();
                    }
                }
                Err(e) => {
                    ui.label(
                        egui::RichText::new(format!("Cannot list IPTS: {e}"))
                            .color(theme::DANGER),
                    );
                }
            }
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

    /// Launch the rust_tiff_viewer on a data folder (detached), passing the
    /// detector offset found in the folder's config.json / summary.json.
    fn launch_visualizer(&mut self, folder: &Path) {
        let result = if folder.is_dir() {
            let mut cmd = std::process::Command::new(DATA_VISUALIZER_CMD);
            cmd.arg(folder);
            if let Some(offset_us) = runs::detector_offset_us(folder) {
                cmd.arg("--offset").arg(offset_us.to_string());
            }
            cmd.spawn()
                .map(|_| ())
                .map_err(|e| format!("cannot launch {DATA_VISUALIZER_CMD}: {e}"))
        } else {
            Err(format!("data folder not found: {}", folder.display()))
        };
        self.launch_error = result.err();
    }

    /// Admin tab: ON/OFF status button, admin unlock, and (once unlocked) the
    /// raw configuration content.
    fn admin_tab(&mut self, ui: &mut egui::Ui) {
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
                // The raw configuration content and the IPTS selector are
                // admin-only.
                if self.admin_unlocked {
                    ui.add_space(theme::SPACE_LG);
                    self.ipts_section(ui, cfg.get("ipts").unwrap_or(""));
                    ui.add_space(theme::SPACE_LG);
                    self.details(ui, &cfg);
                }
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
    }

    /// Monitor tab: master table of the last reduced runs, with switches to
    /// open each run's reduction log / error log in a viewer below the table.
    fn monitor_tab(&mut self, ui: &mut egui::Ui) {
        let ipts = self
            .cfg
            .as_ref()
            .ok()
            .and_then(|cfg| cfg.get("ipts"))
            .unwrap_or("?")
            .to_owned();
        ui.label(theme::section_heading(&format!(
            "Last {MONITOR_RUN_COUNT} reduced runs — {ipts}"
        )));
        ui.add_space(theme::SPACE_XS);

        let run_list = match self.runs.clone() {
            Ok(list) => list,
            Err(e) => {
                ui.label(
                    egui::RichText::new(format!("Cannot list reduced runs: {e}"))
                        .color(theme::DANGER),
                );
                return;
            }
        };
        if run_list.is_empty() {
            ui.label(
                egui::RichText::new("No reduced runs found in the reduction_log folder")
                    .color(theme::TEXT_EMPHASIS),
            );
            return;
        }

        // Master table: run number | log switch | error-log switch. A switch
        // opens that file in the viewer below; only one file is open at a
        // time, and clicking the active switch closes the viewer.
        let mut toggled: Option<(u64, LogKind)> = None;
        theme::container_frame().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("runs_table")
                .max_height(ui.available_height() * 0.45)
                .show(ui, |ui| {
                    egui::Grid::new("runs_grid")
                        .num_columns(7)
                        .striped(true)
                        .spacing([theme::SPACE_LG * 2.0, theme::SPACE_XS])
                        .show(ui, |ui| {
                            ui.label(theme::section_heading("Run"));
                            ui.label(theme::section_heading("Date/time"));
                            ui.label(theme::section_heading("Status"));
                            ui.label(theme::section_heading("Log"));
                            ui.label(theme::section_heading("Error log"));
                            ui.label(theme::section_heading("Corrected"));
                            ui.label(theme::section_heading("Normalized"));
                            ui.end_row();
                            for run in &run_list {
                                let failed = run.err_path.is_some();
                                let mut run_text =
                                    egui::RichText::new(run.run_number.to_string()).strong();
                                if failed {
                                    run_text = run_text.color(theme::DANGER);
                                }
                                ui.label(run_text);
                                let when: chrono::DateTime<chrono::Local> = run.mtime.into();
                                ui.label(when.format("%Y-%m-%d %H:%M:%S").to_string());
                                // An error log means the reduction failed.
                                let status = if failed {
                                    egui::RichText::new("✖ failed")
                                        .color(theme::DANGER)
                                        .strong()
                                } else {
                                    egui::RichText::new("✔ success")
                                        .color(theme::SUCCESS)
                                        .strong()
                                };
                                ui.label(status);
                                for (kind, path) in [
                                    (LogKind::Log, &run.log_path),
                                    (LogKind::Err, &run.err_path),
                                ] {
                                    if path.is_some() {
                                        let active =
                                            self.viewer == Some((run.run_number, kind));
                                        if ui.selectable_label(active, "view").clicked() {
                                            toggled = Some((run.run_number, kind));
                                        }
                                    } else {
                                        ui.label(
                                            egui::RichText::new("—")
                                                .color(theme::TEXT_EMPHASIS),
                                        );
                                    }
                                }
                                // Corrected / Normalized data: launch the
                                // external visualizer on the folder named in
                                // the run's log. A dash means the log does
                                // not name that folder (e.g. the run was
                                // never normalized).
                                let folders =
                                    self.run_folders.get(&run.run_number).cloned();
                                for (folder, what) in [
                                    (
                                        folders.as_ref().and_then(|f| f.corrected.clone()),
                                        "detector efficiency corrected",
                                    ),
                                    (
                                        folders.as_ref().and_then(|f| f.normalized.clone()),
                                        "normalized",
                                    ),
                                ] {
                                    match folder {
                                        Some(folder) => {
                                            let clicked = ui
                                                .button("▶ visualize")
                                                .on_hover_text(format!(
                                                    "Open the {what} data in the visualizer\n{}",
                                                    folder.display()
                                                ))
                                                .clicked();
                                            if clicked {
                                                self.launch_error = None;
                                                self.launch_visualizer(&folder);
                                            }
                                        }
                                        None => {
                                            ui.label(
                                                egui::RichText::new("—")
                                                    .color(theme::TEXT_EMPHASIS),
                                            );
                                        }
                                    }
                                }
                                ui.end_row();
                            }
                        });
                });
        });
        if let Some(err) = &self.launch_error {
            ui.add_space(theme::SPACE_XS);
            ui.label(
                egui::RichText::new(format!("Cannot visualize data: {err}"))
                    .color(theme::DANGER),
            );
        }
        if let Some(selection) = toggled {
            // Same switch again → off; otherwise switch the viewer over.
            self.viewer = if self.viewer == Some(selection) {
                None
            } else {
                Some(selection)
            };
            self.viewer_content.clear();
            self.reload_viewer();
        }

        // Viewer: content of the selected file, below the table.
        if let Some((run_number, kind)) = self.viewer {
            ui.add_space(theme::SPACE_MD);
            ui.label(theme::section_heading(&format!(
                "Run {run_number} — {} (VENUS_{run_number}.nxs.h5.{})",
                kind.label(),
                match kind {
                    LogKind::Log => "log",
                    LogKind::Err => "err",
                }
            )));
            ui.add_space(theme::SPACE_XS);
            theme::container_frame().show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("log_viewer")
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut self.viewer_content.as_str())
                                .font(egui::TextStyle::Monospace)
                                .desired_width(f32::INFINITY),
                        );
                    });
            });
        }
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

        // Tab bar directly under the header (same pattern as the template's
        // selector bar).
        egui::TopBottomPanel::top("tab_bar")
            .frame(
                egui::Frame::new()
                    .fill(theme::SURFACE_WEAK)
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in TABS {
                        if ui.selectable_label(self.tab == *tab, *label).clicked() {
                            self.tab = *tab;
                        }
                    }
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(theme::SPACE_LG);
            match self.tab {
                Tab::Admin => self.admin_tab(ui),
                Tab::Monitor => self.monitor_tab(ui),
            }
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
