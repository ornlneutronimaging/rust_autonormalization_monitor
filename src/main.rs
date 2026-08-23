//! VENUS Auto Normalization — single-view application.
//!
//! Workflow, top to bottom:
//! 1. Select the IPTS (dropdown of accessible IPTS-* folders, or manual
//!    entry). Everything below is disabled until an IPTS is chosen.
//! 2. Select the normalization configuration file
//!    (`<IPTS>/shared/autoreduce/configs/*.h5`, created with the marimo
//!    "Normalization TOF at VENUS" notebook — a button launches that
//!    notebook directly in the selected IPTS).
//! 3. Either turn auto-normalization ON (every upcoming run gets
//!    normalized — writes the shared `autoreduction.cfg`), or type a list
//!    of runs to normalize.
//! 4. When a run list is given, a table shows for each run whether its
//!    NeXus / raw / corrected / normalized files exist yet (hover an icon
//!    for the full path).

mod config;
mod files;
mod h5;
mod norm;
mod notebook;
mod theme;

use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Shared configuration read/written by the normalization notebook and the
/// autoreduction. The notebook writes the first path (creating it on first
/// registration); the second is the legacy location still found on disk.
const CONFIG_PATHS: &[&str] = &[
    "/SNS/VENUS/shared/autoreduction/autoreduction.cfg",
    "/SNS/VENUS/shared/autoreduce/autoreduction.cfg",
];
/// Root scanned for the IPTS-* experiment folders.
const IPTS_ROOT: &str = "/SNS/VENUS";
const LOGO_PATH: &str = "/SNS/VENUS/shared/software/logos/logo_with_green_neutron_rays.png";
const APP_TITLE: &str = "VENUS Auto Normalization";
/// Default auto-refresh period (config file + runs table), in seconds.
const DEFAULT_REFRESH_SECS: u32 = 5;
/// Application launched to preview a normalization configuration file
/// (HDF5): the rust_nexus_viewer, called with the file as argument.
const NEXUS_VIEWER_CMD: &str =
    "/SNS/VENUS/shared/software/git/rust_nexus_viewer/launch_nexus_viewer.sh";
/// Application launched to look at a window's normalized images: the
/// rust_tiff_viewer, called with the data folder.
const TIFF_VIEWER_CMD: &str =
    "/SNS/VENUS/shared/software/git/rust_tiff_viewer/launch_rust_tiff_viewer.sh";
/// Default rolling time windows, in minutes of acquisition time.
const DEFAULT_WINDOWS_MIN: [u32; 3] = [5, 15, 30];

/// The two views of the "Runs in use" section.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RunsView {
    Table,
    Timeline,
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

/// The `autoreduction.cfg` to read/write: the first existing path, or the
/// notebook's (primary) path when none exists yet.
fn resolve_config_path() -> PathBuf {
    CONFIG_PATHS
        .iter()
        .map(PathBuf::from)
        .find(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from(CONFIG_PATHS[0]))
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

/// One selectable normalization configuration file.
#[derive(Clone)]
struct ConfigFile {
    path: PathBuf,
    name: String,
    mtime: std::time::SystemTime,
}

struct MonitorApp {
    logo: Option<Logo>,
    logo_loaded: bool,
    /// Resolved path of the shared autoreduction.cfg (re-resolved on refresh).
    cfg_path: PathBuf,
    /// Latest read of the shared configuration file (Err = not readable /
    /// not created yet — shown as OFF).
    cfg: Result<config::AutoNormConfig, String>,
    /// Error from the last write attempt, shown until the next successful one.
    write_error: Option<String>,

    /// IPTS folders the current user can access (scanned at startup).
    ipts_list: Result<Vec<String>, String>,
    ipts_filter: String,
    manual_ipts: String,
    manual_ipts_error: Option<String>,
    /// The selected experiment, e.g. "IPTS-36967". Gates the whole UI.
    ipts: Option<String>,

    /// Normalization configuration files found in the selected IPTS.
    configs: Vec<ConfigFile>,
    configs_error: Option<String>,
    selected_config: Option<PathBuf>,
    /// Status/errors from the last notebook launch.
    launch_status: Option<(String, egui::Color32)>,
    /// Error from the last attempt to preview a configuration file.
    preview_error: Option<String>,

    /// Raw text of the run-list field and its parse error, if any.
    run_list_text: String,
    run_list_error: Option<String>,
    /// Parsed run numbers of the user's list (empty = live mode).
    runs: Vec<u64>,
    /// Runs rejected by the user: still listed in the table (crossed out)
    /// but excluded from the windows and their normalizations.
    rejected: HashSet<u64>,
    /// Live mode: every run inside the widest window (rejected included),
    /// so the table can show the runs the windows use.
    window_span: Vec<u64>,
    /// File presence for each run shown in the table (rebuilt on refresh).
    run_files: Vec<files::RunFiles>,
    /// When auto-normalization is ON: the upcoming run (latest NeXus in the
    /// IPTS + 1) that will be normalized next, shown on top of the table.
    next_run: Option<files::RunFiles>,

    /// Rolling combine-normalization windows (default last 5/15/30 min).
    windows: Vec<norm::Window>,
    /// NeXus (start, end) acquisition times already read (they never
    /// change once written).
    time_cache: HashMap<
        u64,
        (
            chrono::DateTime<chrono::FixedOffset>,
            chrono::DateTime<chrono::FixedOffset>,
        ),
    >,
    /// Which view of section 5 is open: the table or the acquisition
    /// timeline plot.
    runs_view: RunsView,
    /// Latest (anchor) run the live mode already reacted to: a job volley
    /// fires only when a newer NeXus shows up.
    last_live_anchor: Option<u64>,
    /// Channel the window jobs report their progress and outcome on.
    norm_tx: mpsc::Sender<norm::JobMessage>,
    norm_rx: mpsc::Receiver<norm::JobMessage>,
    /// Error from the last attempt to open a normalized folder in the viewer.
    viewer_error: Option<String>,

    last_refresh: Instant,
    auto_refresh: bool,
    refresh_secs: u32,
}

impl MonitorApp {
    fn new() -> Self {
        let (norm_tx, norm_rx) = mpsc::channel();
        let mut app = Self {
            logo: None,
            logo_loaded: false,
            cfg_path: resolve_config_path(),
            cfg: Err("not read yet".to_owned()),
            write_error: None,
            ipts_list: list_accessible_ipts(Path::new(IPTS_ROOT)),
            ipts_filter: String::new(),
            manual_ipts: String::new(),
            manual_ipts_error: None,
            ipts: None,
            configs: Vec::new(),
            configs_error: None,
            selected_config: None,
            launch_status: None,
            preview_error: None,
            run_list_text: String::new(),
            run_list_error: None,
            runs: Vec::new(),
            rejected: HashSet::new(),
            window_span: Vec::new(),
            run_files: Vec::new(),
            next_run: None,
            windows: DEFAULT_WINDOWS_MIN
                .iter()
                .map(|&m| norm::Window::new(m))
                .collect(),
            time_cache: HashMap::new(),
            runs_view: RunsView::Table,
            last_live_anchor: None,
            norm_tx,
            norm_rx,
            viewer_error: None,
            last_refresh: Instant::now(),
            auto_refresh: true,
            refresh_secs: DEFAULT_REFRESH_SECS,
        };
        app.refresh();
        // Convenience: pre-select the IPTS (and configuration file) the
        // shared configuration currently points at.
        if let Ok(cfg) = &app.cfg {
            if let Some(ipts) = cfg.get("ipts") {
                let ipts = ipts.to_owned();
                let registered = cfg
                    .get("user_autoreduction_config_file")
                    .map(PathBuf::from);
                app.select_ipts(ipts);
                if let Some(file) = registered {
                    if app.configs.iter().any(|c| c.path == file) {
                        app.selected_config = Some(file);
                    }
                }
            }
        }
        app
    }

    fn ipts_path(&self) -> Option<PathBuf> {
        self.ipts.as_ref().map(|i| Path::new(IPTS_ROOT).join(i))
    }

    /// Make `ipts` the selected experiment and rescan what depends on it.
    fn select_ipts(&mut self, ipts: String) {
        self.ipts = Some(ipts);
        self.manual_ipts_error = None;
        self.launch_status = None;
        self.preview_error = None;
        self.viewer_error = None;
        self.selected_config = None;
        self.time_cache.clear();
        self.rejected.clear();
        self.last_live_anchor = None;
        for w in &mut self.windows {
            w.runs.clear();
            w.state = norm::JobState::Idle;
        }
        self.rescan_configs();
        self.check_runs();
    }

    /// Validate the manually typed IPTS ("36967" or "IPTS-36967") and select
    /// it if the folder exists and is accessible.
    fn apply_manual_ipts(&mut self) {
        let typed = self.manual_ipts.trim();
        let number = typed
            .trim_start_matches("IPTS-")
            .trim_start_matches("ipts-");
        if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
            self.manual_ipts_error =
                Some(format!("'{typed}' is not an IPTS number (e.g. 36967)"));
            return;
        }
        let name = format!("IPTS-{number}");
        let path = Path::new(IPTS_ROOT).join(&name);
        if !path.is_dir() {
            self.manual_ipts_error = Some(format!("{} does not exist", path.display()));
        } else if !can_access(&path) {
            self.manual_ipts_error =
                Some(format!("no permission to access {}", path.display()));
        } else {
            self.select_ipts(name);
        }
    }

    /// List `<IPTS>/shared/autoreduce/configs/*.h5`, newest first.
    fn rescan_configs(&mut self) {
        self.configs.clear();
        self.configs_error = None;
        let Some(ipts_path) = self.ipts_path() else {
            return;
        };
        let dir = ipts_path.join("shared/autoreduce/configs");
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.configs_error = Some(format!("cannot read {}: {e}", dir.display()));
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("h5") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            self.configs.push(ConfigFile { path, name, mtime });
        }
        self.configs.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        // Drop a selection that no longer exists on disk.
        if let Some(selected) = &self.selected_config {
            if !self.configs.iter().any(|c| &c.path == selected) {
                self.selected_config = None;
            }
        }
    }

    /// Recompute the windows, then the table: file presence of every run
    /// in use (the user's list, or the widest window in live mode) and of
    /// the upcoming run when auto-normalization is active.
    fn check_runs(&mut self) {
        self.update_windows();
        let table_runs = if !self.runs.is_empty() {
            self.runs.clone()
        } else {
            self.window_span.clone()
        };
        self.run_files = match self.ipts_path() {
            Some(ipts_path) if !table_runs.is_empty() => {
                files::check_runs(&ipts_path, &table_runs)
            }
            _ => Vec::new(),
        };
        // The next NeXus that will land in the IPTS (latest one + 1): what
        // auto-normalization will process next.
        self.next_run = if self.is_active() {
            self.ipts_path().and_then(|ipts_path| {
                files::latest_nexus_run(&ipts_path)
                    .map(|latest| files::check_runs(&ipts_path, &[latest + 1]).remove(0))
            })
        } else {
            None
        };
    }

    /// Recompute which runs fall in each rolling window: the user's run
    /// list when one was given, otherwise every run of the IPTS (live
    /// mode). Acquisition end times come from the NeXus files (cached).
    fn update_windows(&mut self) {
        let Some(ipts_path) = self.ipts_path() else {
            for w in &mut self.windows {
                w.runs.clear();
            }
            return;
        };
        let live = self.runs.is_empty();
        let candidates = if live {
            files::list_nexus_runs(&ipts_path)
        } else {
            self.runs.clone()
        };
        let max_minutes = self.windows.iter().map(|w| w.minutes).max().unwrap_or(0);
        let mut end_times: Vec<(u64, chrono::DateTime<chrono::FixedOffset>)> = Vec::new();
        let mut anchor: Option<chrono::DateTime<chrono::FixedOffset>> = None;
        // Newest runs first; the first readable end time is the anchor and,
        // in live mode, the scan stops at the first run older than the
        // widest window (no point opening thousands of old NeXus files).
        for &run in candidates.iter().rev() {
            let time = match self.time_cache.get(&run) {
                Some((_, end)) => *end,
                None => {
                    let Some(times) = h5::nexus_times(&files::nexus_path(&ipts_path, run))
                    else {
                        // Missing or still being written — retry next refresh.
                        continue;
                    };
                    self.time_cache.insert(run, times);
                    times.1
                }
            };
            // The anchor is the newest NON-rejected run, so rejecting the
            // latest run slides the windows back to the previous one.
            if anchor.is_none() && !self.rejected.contains(&run) {
                anchor = Some(time);
            }
            if let Some(anchor) = anchor {
                if time < anchor - chrono::Duration::minutes(i64::from(max_minutes)) {
                    if live {
                        break;
                    }
                    continue;
                }
            }
            end_times.push((run, time));
        }
        // Rejected runs never enter the windows (nor set the anchor), but
        // stay in the live table span so they can be restored.
        let kept: Vec<(u64, chrono::DateTime<chrono::FixedOffset>)> = end_times
            .iter()
            .filter(|(run, _)| !self.rejected.contains(run))
            .copied()
            .collect();
        norm::assign_windows(&mut self.windows, &kept);
        self.window_span = match kept.iter().map(|(_, t)| *t).max() {
            Some(kept_anchor) => {
                let cutoff =
                    kept_anchor - chrono::Duration::minutes(i64::from(max_minutes));
                let mut span: Vec<u64> = end_times
                    .iter()
                    .filter(|(_, t)| *t >= cutoff)
                    .map(|(run, _)| *run)
                    .collect();
                span.sort_unstable();
                span
            }
            None => Vec::new(),
        };

        // Live and hybrid modes: a new anchor run (a NeXus that just
        // showed up — in hybrid mode, just joined the list) fires the
        // window normalizations. The first anchor seen only arms the
        // trigger — the app should not fire for a run that landed before
        // it was even watching.
        if self.is_active() {
            let anchor_run = self.windows.iter().flat_map(|w| w.runs.iter()).max().copied();
            if let Some(anchor_run) = anchor_run {
                match self.last_live_anchor {
                    None => self.last_live_anchor = Some(anchor_run),
                    Some(prev) if anchor_run > prev => {
                        self.last_live_anchor = Some(anchor_run);
                        if self.selected_config.is_some() {
                            self.launch_windows();
                        }
                    }
                    _ => {}
                }
            }
        } else {
            // Re-armed when auto normalization resumes.
            self.last_live_anchor = None;
        }
    }

    /// Launch the combine normalization of every window that has runs and
    /// is not already running.
    fn launch_windows(&mut self) {
        let (Some(ipts_path), Some(config)) = (self.ipts_path(), self.selected_config.clone())
        else {
            return;
        };
        let info = match h5::read_config_info(&config) {
            Ok(info) => info,
            Err(e) => {
                for w in &mut self.windows {
                    w.state = norm::JobState::Failed { message: e.clone() };
                }
                return;
            }
        };
        for i in 0..self.windows.len() {
            if matches!(self.windows[i].state, norm::JobState::Running { .. }) {
                continue;
            }
            match norm::prepare_job(i, &self.windows[i], &ipts_path, &config, &info) {
                Ok(spec) => {
                    self.windows[i].state = norm::JobState::Running {
                        runs: spec.runs.clone(),
                        stage: "starting…".to_owned(),
                        fraction: None,
                    };
                    norm::launch(spec, self.norm_tx.clone());
                }
                Err(message) => self.windows[i].state = norm::JobState::Failed { message },
            }
        }
    }

    /// Open normalized-data folders in ONE TIFF viewer session (detached):
    /// the first folder is the main stack, the others are `--compare`
    /// stacks shown side by side (shared colorscale, mirrored regions).
    fn open_in_viewer(&mut self, folders: &[PathBuf]) {
        let Some((first, rest)) = folders.split_first() else {
            return;
        };
        self.viewer_error = if let Some(missing) = folders.iter().find(|f| !f.is_dir()) {
            Some(format!("folder not found: {}", missing.display()))
        } else {
            let mut cmd = std::process::Command::new(TIFF_VIEWER_CMD);
            cmd.arg(first);
            for folder in rest {
                cmd.arg("--compare").arg(folder);
            }
            cmd.spawn()
                .map(|_| ())
                .err()
                .map(|e| format!("cannot launch {TIFF_VIEWER_CMD}: {e}"))
        };
    }

    fn refresh(&mut self) {
        self.cfg_path = resolve_config_path();
        self.cfg = config::read(&self.cfg_path);
        if self.ipts.is_some() {
            self.rescan_configs();
        }
        self.extend_run_list();
        self.check_runs();
        self.last_refresh = Instant::now();
    }

    /// Hybrid mode: when the user gave a run list AND auto normalization is
    /// ON, every run that lands after the newest listed one joins the list
    /// (and the text field) automatically, so the windows and the table
    /// follow the acquisition.
    fn extend_run_list(&mut self) {
        if self.runs.is_empty() || !self.is_active() {
            return;
        }
        let Some(ipts_path) = self.ipts_path() else {
            return;
        };
        let newest_listed = *self.runs.last().expect("list not empty");
        // `runs` is kept sorted, so appending only-newer runs keeps it sorted.
        for run in files::list_nexus_runs(&ipts_path) {
            if run > newest_listed {
                self.runs.push(run);
                if !self.run_list_text.trim().is_empty() {
                    self.run_list_text.push_str(&format!(", {run}"));
                } else {
                    self.run_list_text = run.to_string();
                }
            }
        }
    }

    /// Is auto-normalization currently active (per the shared config file)?
    fn is_active(&self) -> bool {
        self.cfg.as_ref().map(|c| c.activate).unwrap_or(false)
    }

    /// Turn auto-normalization ON: register the selected IPTS +
    /// configuration file in the shared config and set the flag.
    fn turn_on(&mut self) {
        let (Some(ipts), Some(config_file)) = (self.ipts.clone(), self.selected_config.clone())
        else {
            return;
        };
        match config::write_full(
            &self.cfg_path,
            &ipts,
            &config_file.display().to_string(),
            true,
        ) {
            Ok(()) => self.write_error = None,
            Err(e) => self.write_error = Some(e),
        }
        self.refresh();
    }

    /// Turn auto-normalization OFF (only the flag is touched, the registered
    /// configuration file is kept).
    fn turn_off(&mut self) {
        match config::set_activate(&self.cfg_path, false) {
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

    /// Auto-refresh controls: enable/disable the periodic re-read of the
    /// config file and runs table, and pick its period.
    fn refresh_controls(&mut self, ui: &mut egui::Ui) {
        // Laid out right-to-left, so add the widgets in reverse order.
        if ui
            .button("⟳ Refresh now")
            .on_hover_text("Re-read the configuration and re-check the files")
            .clicked()
        {
            self.refresh();
        }
        ui.add_enabled(
            self.auto_refresh,
            egui::DragValue::new(&mut self.refresh_secs)
                .range(1..=3600)
                .suffix(" s")
                .speed(1),
        )
        .on_hover_text("Refresh period in seconds");
        ui.checkbox(&mut self.auto_refresh, "Auto-refresh");
    }

    /// Section 1 — pick the experiment. Dropdown of accessible IPTS with a
    /// type-to-filter box, plus a manual entry for an IPTS not listed.
    fn ipts_section(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::section_heading("1. Experiment (IPTS)"));
        ui.add_space(theme::SPACE_XS);
        theme::section_frame(ui, |ui| {
            let current = self.ipts.clone().unwrap_or_default();
            let mut selected: Option<String> = None;
            match &self.ipts_list {
                Ok(list) => {
                    let filter = self
                        .ipts_filter
                        .trim()
                        .trim_start_matches("IPTS-")
                        .trim_start_matches("ipts-")
                        .to_owned();
                    let filtered: Vec<&String> =
                        list.iter().filter(|name| name.contains(&filter)).collect();
                    ui.horizontal(|ui| {
                        ui.label("IPTS:");
                        egui::ComboBox::from_id_salt("ipts_combo")
                            .selected_text(if current.is_empty() {
                                "— select —"
                            } else {
                                &current
                            })
                            .show_ui(ui, |ui| {
                                for name in &filtered {
                                    if ui
                                        .selectable_label(**name == current, *name)
                                        .clicked()
                                        && **name != current
                                    {
                                        selected = Some((*name).clone());
                                    }
                                }
                            });
                        ui.label("Filter:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.ipts_filter)
                                .hint_text("type IPTS number…")
                                .desired_width(110.0),
                        );
                        ui.label(
                            egui::RichText::new(if filter.is_empty() {
                                format!("({} accessible)", list.len())
                            } else {
                                format!("({} of {} match)", filtered.len(), list.len())
                            })
                            .color(theme::text_emphasis(ui.visuals())),
                        );
                    });
                }
                Err(e) => {
                    ui.label(
                        egui::RichText::new(format!("Cannot list IPTS: {e}"))
                            .color(theme::DANGER),
                    );
                }
            }
            // Manual entry, for an IPTS the scan did not list.
            ui.horizontal(|ui| {
                ui.label("Manual entry:");
                let edit = egui::TextEdit::singleline(&mut self.manual_ipts)
                    .hint_text("e.g. 36967")
                    .desired_width(110.0);
                let response = ui.add(edit);
                let submitted =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Use").clicked() || submitted {
                    self.apply_manual_ipts();
                }
                if let Some(err) = &self.manual_ipts_error {
                    ui.label(egui::RichText::new(err).color(theme::DANGER));
                }
            });
            if let Some(name) = selected {
                self.select_ipts(name);
            }
        });
    }

    /// Open the selected configuration file in the NeXus viewer (detached).
    fn preview_config(&mut self) {
        let Some(path) = self.selected_config.clone() else {
            return;
        };
        self.preview_error = if !path.is_file() {
            Some(format!("configuration file not found: {}", path.display()))
        } else {
            std::process::Command::new(NEXUS_VIEWER_CMD)
                .arg(&path)
                .spawn()
                .map(|_| ())
                .err()
                .map(|e| format!("cannot launch {NEXUS_VIEWER_CMD}: {e}"))
        };
    }

    /// Section 2 — pick the normalization configuration file, or launch the
    /// marimo notebook to create one.
    fn config_section(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::section_heading("2. Normalization configuration"));
        ui.add_space(theme::SPACE_XS);
        theme::section_frame(ui, |ui| {
            let mut selected: Option<PathBuf> = None;
            ui.horizontal(|ui| {
                ui.label("Configuration file:");
                let current_name = self
                    .selected_config
                    .as_ref()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "— select —".to_owned());
                let combo = egui::ComboBox::from_id_salt("config_combo")
                    .selected_text(current_name)
                    .width(340.0);
                let response = combo.show_ui(ui, |ui| {
                    for cfg_file in &self.configs {
                        let active = self.selected_config.as_ref() == Some(&cfg_file.path);
                        let when: chrono::DateTime<chrono::Local> = cfg_file.mtime.into();
                        if ui
                            .selectable_label(active, &cfg_file.name)
                            .on_hover_text(format!(
                                "{}\nmodified {}",
                                cfg_file.path.display(),
                                when.format("%Y-%m-%d %H:%M:%S")
                            ))
                            .clicked()
                        {
                            selected = Some(cfg_file.path.clone());
                        }
                    }
                });
                if let Some(path) = &self.selected_config {
                    response.response.on_hover_text(path.display().to_string());
                }
                if ui
                    .button("⟳")
                    .on_hover_text("Rescan the configs folder")
                    .clicked()
                {
                    self.rescan_configs();
                }
                // Preview the selected configuration in the NeXus viewer
                // (the config is a plain HDF5 file).
                let preview = ui.add_enabled(
                    self.selected_config.is_some(),
                    egui::Button::new("👁 Preview"),
                );
                let preview = match &self.selected_config {
                    Some(path) => preview.on_hover_text(format!(
                        "Open the configuration in the NeXus viewer\n{}",
                        path.display()
                    )),
                    None => preview
                        .on_disabled_hover_text("Select a configuration file first"),
                };
                if preview.clicked() {
                    self.preview_config();
                }
            });
            if let Some(err) = &self.preview_error {
                ui.label(
                    egui::RichText::new(format!("Cannot preview the configuration: {err}"))
                        .color(theme::DANGER),
                );
            }
            match (&self.configs_error, self.configs.is_empty()) {
                (Some(e), _) => {
                    ui.label(egui::RichText::new(e.as_str()).color(theme::WARNING));
                }
                (None, true) => {
                    ui.label(
                        egui::RichText::new(
                            "No configuration file found — create one with the notebook below",
                        )
                        .color(theme::text_emphasis(ui.visuals())),
                    );
                }
                _ => {}
            }
            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                if ui
                    .add(theme::primary_button("🚀 Create new configuration (normalization notebook)"))
                    .on_hover_text(format!(
                        "Launch the marimo \"Normalization TOF at VENUS\" notebook\n\
                         directly in the selected IPTS\n{}",
                        notebook::NOTEBOOK_PATH
                    ))
                    .clicked()
                {
                    if let Some(ipts_path) = self.ipts_path() {
                        self.launch_status = Some(match notebook::launch(&ipts_path) {
                            Ok(msg) => (msg, theme::SUCCESS),
                            Err(e) => (e, theme::DANGER),
                        });
                    }
                }
                if let Some((msg, color)) = &self.launch_status {
                    ui.label(egui::RichText::new(msg).color(*color));
                }
            });
            if let Some(path) = selected {
                self.selected_config = Some(path);
                self.preview_error = None;
            }
        });
    }

    /// Section 3 — auto-normalization ON/OFF, or a manual list of runs.
    fn mode_section(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::section_heading("3. What to normalize"));
        ui.add_space(theme::SPACE_XS);
        theme::section_frame(ui, |ui| {
            let active = self.is_active();
            ui.horizontal(|ui| {
                let (label, fill) = if active {
                    ("Auto normalization: ON", theme::SUCCESS)
                } else {
                    ("Auto normalization: OFF", theme::DANGER)
                };
                let text = egui::RichText::new(label)
                    .color(theme::TEXT_WHITE)
                    .strong()
                    .size(18.0);
                let button = egui::Button::new(text)
                    .fill(fill)
                    .corner_radius(8.0)
                    .min_size(egui::vec2(260.0, 40.0));
                let can_turn_on = self.selected_config.is_some();
                let response = ui.add_enabled(active || can_turn_on, button);
                let response = if active {
                    response.on_hover_text(
                        "Every upcoming run is normalized automatically — click to turn OFF",
                    )
                } else if can_turn_on {
                    response.on_hover_text(
                        "Click to normalize every upcoming run with the selected configuration",
                    )
                } else {
                    response.on_disabled_hover_text(
                        "Select a normalization configuration file first",
                    )
                };
                if response.clicked() {
                    if active {
                        self.turn_off();
                    } else {
                        self.turn_on();
                    }
                }
                ui.label(
                    egui::RichText::new(format!("({})", self.cfg_path.display()))
                        .color(theme::text_emphasis(ui.visuals()))
                        .small(),
                );
            });
            if let Some(err) = &self.write_error {
                ui.label(
                    egui::RichText::new(format!("Failed to update the configuration: {err}"))
                        .color(theme::DANGER),
                );
            }
            // The shared config may point at another IPTS/config than the
            // one selected here — make that visible.
            if active {
                if let Ok(cfg) = &self.cfg {
                    let reg_ipts = cfg.get("ipts").unwrap_or("?");
                    let reg_file = cfg.get("user_autoreduction_config_file").unwrap_or("?");
                    ui.label(
                        egui::RichText::new(format!(
                            "Active on {reg_ipts} with {}",
                            Path::new(reg_file)
                                .file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_else(|| reg_file.to_owned())
                        ))
                        .color(theme::text_emphasis(ui.visuals())),
                    )
                    .on_hover_text(reg_file);
                }
            }

            ui.add_space(theme::SPACE_SM);
            ui.separator();
            ui.add_space(theme::SPACE_XS);
            ui.horizontal(|ui| {
                ui.label("…or normalize a list of runs:");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.run_list_text)
                        .hint_text("e.g. 23615-23620, 23642")
                        .desired_width(260.0),
                );
                let submitted =
                    response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Show").clicked() || submitted {
                    match files::parse_run_list(&self.run_list_text) {
                        Ok(runs) => {
                            self.run_list_error = None;
                            self.runs = runs;
                            self.check_runs();
                        }
                        Err(e) => {
                            self.run_list_error = Some(e);
                            self.runs.clear();
                            self.run_files.clear();
                        }
                    }
                }
                if !self.runs.is_empty() && ui.button("Clear").clicked() {
                    self.runs.clear();
                    self.run_files.clear();
                    self.run_list_text.clear();
                    self.run_list_error = None;
                    // Back to live mode: windows follow the whole IPTS again.
                    self.check_runs();
                }
            });
            if let Some(err) = &self.run_list_error {
                ui.label(egui::RichText::new(err).color(theme::DANGER));
            }
        });
    }

    /// Section 4 — the rolling combine-normalization windows: editable
    /// durations, the runs currently inside each window, job status, and
    /// view/compare buttons, laid out as an aligned grid. In live mode the
    /// jobs fire on every new NeXus; with a run list they are launched by
    /// hand.
    fn windows_section(&mut self, ui: &mut egui::Ui) {
        ui.label(theme::section_heading(
            "4. Rolling combine & compare (NeuNorm)",
        ));
        ui.add_space(theme::SPACE_XS);
        theme::section_frame(ui, |ui| {
            let live = self.runs.is_empty();
            ui.label(
                egui::RichText::new(if live {
                    "Live: the windows follow the latest run of the IPTS, and the \
                     normalizations fire when a new NeXus shows up (auto \
                     normalization ON + configuration selected)."
                } else if self.is_active() {
                    "Hybrid: the windows look at the listed runs, new runs join \
                     the list as they land, and the normalizations fire on each \
                     new NeXus."
                } else {
                    "The windows look at the listed runs only — launch by hand \
                     (turn auto normalization ON to have new runs join the list)."
                })
                .color(theme::text_emphasis(ui.visuals())),
            );
            ui.add_space(theme::SPACE_SM);

            let mut minutes_changed = false;
            let mut view_folder: Option<PathBuf> = None;
            egui::Grid::new("windows_grid")
                .num_columns(4)
                .spacing([theme::SPACE_LG * 2.0, theme::SPACE_SM])
                .show(ui, |ui| {
                    ui.label(theme::section_heading("Window"));
                    ui.label(theme::section_heading("Runs"));
                    ui.label(theme::section_heading("Status"));
                    ui.label(theme::section_heading("Result"));
                    ui.end_row();
                    for w in &mut self.windows {
                        ui.horizontal(|ui| {
                            ui.label("last");
                            if ui
                                .add(
                                    egui::DragValue::new(&mut w.minutes)
                                        .range(1..=1440)
                                        .suffix(" min")
                                        .speed(1),
                                )
                                .on_hover_text(
                                    "Acquisition-time window, ending at the newest run",
                                )
                                .changed()
                            {
                                minutes_changed = true;
                            }
                        });
                        // Which runs the window currently holds. A range
                        // ("23640–23642") only when truly contiguous —
                        // with a hole (e.g. a rejected run) the actual
                        // numbers are spelled out.
                        let summary = match (w.runs.first(), w.runs.last()) {
                            (Some(first), Some(last)) if first == last => {
                                format!("1 run ({first})")
                            }
                            (Some(first), Some(last)) => {
                                let n = w.runs.len();
                                let contiguous = last - first + 1 == n as u64;
                                if contiguous {
                                    format!("{n} runs ({first}–{last})")
                                } else if n <= 8 {
                                    format!(
                                        "{n} runs ({})",
                                        w.runs
                                            .iter()
                                            .map(|r| r.to_string())
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    )
                                } else {
                                    format!("{n} runs ({first}, …, {last})")
                                }
                            }
                            _ => "no run in window".to_owned(),
                        };
                        ui.label(
                            egui::RichText::new(summary)
                                .color(theme::text_emphasis(ui.visuals())),
                        )
                        .on_hover_text(
                            w.runs
                                .iter()
                                .map(|r| r.to_string())
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                        // Status column, then the Result (view) column.
                        match &w.state {
                            norm::JobState::Idle => {
                                ui.label(
                                    egui::RichText::new("—")
                                        .color(theme::text_emphasis(ui.visuals())),
                                );
                                ui.label("");
                            }
                            norm::JobState::Running { runs, stage, fraction } => {
                                ui.horizontal(|ui| {
                                    // NeuNorm's own stage progress: a filling
                                    // bar when the total is known, a moving
                                    // one while it is not.
                                    let bar = match fraction {
                                        Some(f) => egui::ProgressBar::new(*f)
                                            .desired_width(140.0)
                                            .desired_height(14.0)
                                            .corner_radius(3.0)
                                            .show_percentage(),
                                        None => egui::ProgressBar::new(0.99)
                                            .desired_width(140.0)
                                            .desired_height(14.0)
                                            .corner_radius(3.0)
                                            .animate(true),
                                    };
                                    ui.add(bar).on_hover_text(format!(
                                        "normalizing {} run(s)",
                                        runs.len()
                                    ));
                                    ui.label(
                                        egui::RichText::new(stage.as_str())
                                            .color(theme::INFO),
                                    );
                                });
                                ui.label("");
                            }
                            norm::JobState::Done { output, finished, runs } => {
                                ui.label(
                                    egui::RichText::new(format!(
                                        "✔ {} ({} run(s))",
                                        finished.format("%H:%M:%S"),
                                        runs.len()
                                    ))
                                    .color(theme::SUCCESS),
                                )
                                .on_hover_text(output.display().to_string());
                                if ui
                                    .button("👁 view")
                                    .on_hover_text(format!(
                                        "Open in the TIFF viewer\n{}",
                                        output.display()
                                    ))
                                    .clicked()
                                {
                                    view_folder = Some(output.clone());
                                }
                            }
                            norm::JobState::Failed { message } => {
                                ui.label(
                                    egui::RichText::new("✖ failed")
                                        .color(theme::DANGER)
                                        .strong(),
                                )
                                .on_hover_text(message);
                                ui.label("");
                            }
                        }
                        ui.end_row();
                    }
                });
            if minutes_changed {
                self.check_runs();
            }
            if let Some(folder) = view_folder {
                self.viewer_error = None;
                self.open_in_viewer(std::slice::from_ref(&folder));
            }

            ui.add_space(theme::SPACE_SM);
            ui.horizontal(|ui| {
                let any_runs = self.windows.iter().any(|w| !w.runs.is_empty());
                let launchable = self.selected_config.is_some() && any_runs;
                let launch = ui.add_enabled(
                    launchable,
                    theme::primary_button("▶ Normalize windows now"),
                );
                let launch = if launchable {
                    launch.on_hover_text("Run the combine normalization of every window")
                } else {
                    launch.on_disabled_hover_text(
                        "Needs a selected configuration file and at least one run in a window",
                    )
                };
                if launch.clicked() {
                    self.launch_windows();
                }
                // Compare needs every window normalized: one TIFF viewer
                // session with the 5/15/30 min stacks side by side.
                let done: Vec<PathBuf> = self
                    .windows
                    .iter()
                    .filter_map(|w| match &w.state {
                        norm::JobState::Done { output, .. } => Some(output.clone()),
                        _ => None,
                    })
                    .collect();
                let total = self.windows.len();
                let all_ready = done.len() == total;
                let compare = ui.add_enabled(
                    all_ready,
                    egui::Button::new(if all_ready {
                        format!("👁 Compare all {total} (ready)")
                    } else {
                        format!("👁 Compare all {total}")
                    }),
                );
                let compare = if all_ready {
                    compare.on_hover_text(
                        "Open ONE TIFF viewer with the windows side by side \
                         (shared colorscale, regions mirrored)",
                    )
                } else {
                    compare.on_disabled_hover_text(format!(
                        "All windows must be normalized first ({} of {total} ready)",
                        done.len()
                    ))
                };
                if compare.clicked() {
                    self.viewer_error = None;
                    self.open_in_viewer(&done);
                }
            });
            if let Some(err) = &self.viewer_error {
                ui.label(
                    egui::RichText::new(format!("Cannot open the viewer: {err}"))
                        .color(theme::DANGER),
                );
            }
        });
    }

    /// One ✔/✘ cell of the runs table, with the full path on hover.
    fn status_cell(ui: &mut egui::Ui, status: &files::FileStatus) {
        match status {
            files::FileStatus::Present(path) => {
                ui.label(
                    egui::RichText::new("✔")
                        .color(theme::SUCCESS)
                        .strong()
                        .size(16.0),
                )
                .on_hover_text(path.display().to_string());
            }
            files::FileStatus::Missing(path) => {
                ui.label(
                    egui::RichText::new("✘")
                        .color(theme::text_emphasis(ui.visuals()))
                        .size(16.0),
                )
                .on_hover_text(format!("not there yet — expected at\n{}", path.display()));
            }
        }
    }

    /// Timeline view of section 5: one horizontal bar per run (acquisition
    /// start → end, from the NeXus times) and, on top, the coverage of the
    /// three rolling windows — all on a shared time axis in minutes
    /// relative to the anchor (the newest non-rejected run).
    fn runs_timeline(&mut self, ui: &mut egui::Ui) {
        use egui_plot::{Bar, BarChart, Plot, PlotPoint, Text, VLine};
        theme::section_frame(ui, |ui| {
            // (run, start, end, rejected) in table order (ascending runs).
            let bars_data: Vec<_> = self
                .run_files
                .iter()
                .filter_map(|rf| {
                    self.time_cache.get(&rf.run).map(|(start, end)| {
                        (rf.run, *start, *end, self.rejected.contains(&rf.run))
                    })
                })
                .collect();
            if bars_data.is_empty() {
                ui.label(
                    egui::RichText::new(
                        "No acquisition times yet — they are read from the NeXus files",
                    )
                    .color(theme::text_emphasis(ui.visuals())),
                );
                return;
            }
            // Same anchor as the windows: newest end among non-rejected runs.
            let anchor = bars_data
                .iter()
                .filter(|(_, _, _, rejected)| !rejected)
                .map(|(_, _, end, _)| *end)
                .max()
                .unwrap_or_else(|| {
                    bars_data.iter().map(|(_, _, end, _)| *end).max().expect("not empty")
                });
            let to_min = |t: &chrono::DateTime<chrono::FixedOffset>| {
                t.signed_duration_since(anchor).num_milliseconds() as f64 / 60_000.0
            };

            let n = bars_data.len();
            let dark = ui.visuals().dark_mode;
            let text_color = if dark { theme::TEXT_STRONG } else { theme::TEXT_STRONG_LIGHT };
            let mut run_bars = Vec::new();
            let mut texts = Vec::new();
            // Leftmost extent, for the run-number label offset.
            let span_min = bars_data
                .iter()
                .map(|(_, start, _, _)| to_min(start))
                .fold(0.0_f64, f64::min)
                .min(-f64::from(self.windows.iter().map(|w| w.minutes).max().unwrap_or(0)));
            for (i, (run, start, end, rejected)) in bars_data.iter().enumerate() {
                let (s, e) = (to_min(start), to_min(end));
                // A visible sliver even for very short acquisitions.
                let value = (e - s).max(-span_min * 0.004);
                run_bars.push(
                    Bar::new(i as f64, value)
                        .base_offset(s)
                        .width(0.6)
                        .fill(if *rejected {
                            egui::Color32::from_gray(if dark { 110 } else { 150 })
                        } else {
                            theme::PRIMARY
                        })
                        .name(format!(
                            "run {run}{}\n{} → {}  ({:.1} min)",
                            if *rejected { " (rejected)" } else { "" },
                            start.format("%H:%M:%S"),
                            end.format("%H:%M:%S"),
                            e - s
                        )),
                );
                let mut label = egui::RichText::new(run.to_string()).size(11.0);
                if *rejected {
                    label = label.strikethrough();
                }
                texts.push(
                    Text::new(PlotPoint::new(s + span_min * 0.01, i as f64), label)
                        .anchor(egui::Align2::RIGHT_CENTER)
                        .color(text_color),
                );
            }
            // Window coverage bands above the runs.
            let band_colors = [
                theme::INFO,
                theme::WARNING,
                egui::Color32::from_rgb(160, 110, 220),
            ];
            let mut band_bars = Vec::new();
            for (j, w) in self.windows.iter().enumerate() {
                let y = n as f64 + 0.9 + j as f64;
                let minutes = f64::from(w.minutes);
                let color = band_colors[j % band_colors.len()];
                band_bars.push(
                    Bar::new(y, minutes)
                        .base_offset(-minutes)
                        .width(0.7)
                        .fill(color.gamma_multiply(0.35))
                        .name(format!(
                            "last {} min — {} run(s) in the window",
                            w.minutes,
                            w.runs.len()
                        )),
                );
                texts.push(
                    Text::new(
                        PlotPoint::new(-minutes - span_min * 0.01, y),
                        egui::RichText::new(format!("last {} min", w.minutes)).size(11.0),
                    )
                    .anchor(egui::Align2::LEFT_CENTER)
                    .color(color),
                );
            }

            let height = ((n + 5) as f32 * 24.0).clamp(200.0, 440.0);
            let anchor_for_cursor = anchor;
            Plot::new("acq_timeline")
                .height(height)
                .allow_scroll(false)
                .y_axis_formatter(|_, _| String::new())
                .include_x(span_min * 1.12)
                .include_x(-span_min * 0.03)
                .include_y(-0.7)
                .include_y(n as f64 + 3.9)
                .label_formatter(move |name, point| {
                    if name.is_empty() {
                        let at = anchor_for_cursor
                            + chrono::Duration::milliseconds((point.x * 60_000.0) as i64);
                        format!("{:.1} min  ({})", point.x, at.format("%H:%M:%S"))
                    } else {
                        name.to_owned()
                    }
                })
                .show(ui, |plot_ui| {
                    plot_ui.vline(
                        VLine::new(0.0)
                            .color(egui::Color32::from_gray(if dark { 150 } else { 110 }))
                            .style(egui_plot::LineStyle::dashed_loose())
                            .name(format!("latest run ({})", anchor.format("%H:%M:%S"))),
                    );
                    plot_ui.bar_chart(
                        BarChart::new(band_bars)
                            .horizontal()
                            .element_formatter(Box::new(|bar, _| bar.name.clone())),
                    );
                    plot_ui.bar_chart(
                        BarChart::new(run_bars)
                            .horizontal()
                            .element_formatter(Box::new(|bar, _| bar.name.clone())),
                    );
                    for text in texts {
                        plot_ui.text(text);
                    }
                });
            ui.label(
                egui::RichText::new(
                    "Time axis: minutes relative to the latest (non-rejected) run — \
                     hover a bar for the run's start/end and duration",
                )
                .color(theme::text_emphasis(ui.visuals()))
                .small(),
            );
        });
    }

    /// Section 5 — the runs in use (the list, or the widest window in live
    /// mode): file status per run, a preview of the corrected data in the
    /// TIFF viewer, and a reject/restore toggle — rejected runs stay
    /// listed, crossed out, but leave the windows and their
    /// normalizations. The upcoming run tops the table when
    /// auto-normalization is ON.
    fn runs_table(&mut self, ui: &mut egui::Ui) {
        if self.run_files.is_empty() && self.next_run.is_none() {
            return;
        }
        let rejected_count = self
            .run_files
            .iter()
            .filter(|r| self.rejected.contains(&r.run))
            .count();
        let heading = if self.run_files.is_empty() {
            "5. Runs in use — next auto-normalized run".to_owned()
        } else if rejected_count > 0 {
            format!(
                "5. Runs in use ({}, {rejected_count} rejected)",
                self.run_files.len() - rejected_count
            )
        } else {
            format!("5. Runs in use ({})", self.run_files.len())
        };
        // Heading + view switch (table / acquisition timeline).
        ui.horizontal(|ui| {
            ui.label(theme::section_heading(&heading));
            ui.add_space(theme::SPACE_MD);
            for (view, label) in [
                (RunsView::Table, "☰ Table"),
                (RunsView::Timeline, "📈 Timeline"),
            ] {
                if ui.selectable_label(self.runs_view == view, label).clicked() {
                    self.runs_view = view;
                }
            }
        });
        ui.add_space(theme::SPACE_XS);
        if self.runs_view == RunsView::Timeline {
            self.runs_timeline(ui);
            return;
        }
        let mut toggle_run: Option<u64> = None;
        let mut preview_folder: Option<PathBuf> = None;
        theme::section_frame(ui, |ui| {
            egui::ScrollArea::vertical()
                .id_salt("runs_table")
                .show(ui, |ui| {
                    egui::Grid::new("runs_grid")
                        .num_columns(6)
                        .striped(true)
                        .spacing([theme::SPACE_LG * 2.5, theme::SPACE_XS])
                        .show(ui, |ui| {
                            ui.label(theme::section_heading("Run"));
                            ui.label(theme::section_heading("NeXus"));
                            ui.label(theme::section_heading("Raw"));
                            ui.label(theme::section_heading("Corrected"));
                            ui.label(theme::section_heading("Preview"));
                            ui.label(theme::section_heading("Use"));
                            ui.end_row();
                            // Upcoming run first: not acquired yet, its
                            // NeXus is what auto-normalization waits for.
                            if let Some(next) = &self.next_run {
                                ui.label(
                                    egui::RichText::new(format!("{} (next)", next.run))
                                        .color(theme::INFO)
                                        .strong(),
                                )
                                .on_hover_text(
                                    "Next run that will be auto-normalized — its NeXus \
                                     is not in the IPTS yet",
                                );
                                Self::status_cell(ui, &next.nexus);
                                Self::status_cell(ui, &next.raw);
                                Self::status_cell(ui, &next.corrected);
                                ui.label("");
                                ui.label("");
                                ui.end_row();
                            }
                            for run in &self.run_files {
                                let rejected = self.rejected.contains(&run.run);
                                let mut run_text =
                                    egui::RichText::new(run.run.to_string()).strong();
                                if rejected {
                                    run_text = run_text
                                        .strikethrough()
                                        .color(theme::text_emphasis(ui.visuals()));
                                }
                                let label = ui.label(run_text);
                                if rejected {
                                    label.on_hover_text(
                                        "Rejected — excluded from the windows",
                                    );
                                }
                                Self::status_cell(ui, &run.nexus);
                                Self::status_cell(ui, &run.raw);
                                Self::status_cell(ui, &run.corrected);
                                // Preview: the corrected data in the viewer.
                                match &run.corrected {
                                    files::FileStatus::Present(folder) => {
                                        if ui
                                            .button("👁")
                                            .on_hover_text(format!(
                                                "Open the corrected data in the TIFF \
                                                 viewer\n{}",
                                                folder.display()
                                            ))
                                            .clicked()
                                        {
                                            preview_folder = Some(folder.clone());
                                        }
                                    }
                                    _ => {
                                        ui.label(
                                            egui::RichText::new("—").color(
                                                theme::text_emphasis(ui.visuals()),
                                            ),
                                        )
                                        .on_hover_text("No corrected data yet");
                                    }
                                }
                                // Reject / restore toggle.
                                let (text, hover) = if rejected {
                                    ("↩ restore", "Put this run back in the windows")
                                } else {
                                    (
                                        "✖ reject",
                                        "Exclude this run from the windows and their \
                                         normalizations",
                                    )
                                };
                                if ui.button(text).on_hover_text(hover).clicked() {
                                    toggle_run = Some(run.run);
                                }
                                ui.end_row();
                            }
                        });
                });
        });
        if let Some(run) = toggle_run {
            if !self.rejected.remove(&run) {
                self.rejected.insert(run);
            }
            // Windows (and the table span) follow the new selection.
            self.check_runs();
        }
        if let Some(folder) = preview_folder {
            self.viewer_error = None;
            self.open_in_viewer(std::slice::from_ref(&folder));
        }
    }
}

impl eframe::App for MonitorApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.logo_loaded {
            self.logo = Logo::load(ctx, LOGO_PATH);
            self.logo_loaded = true;
        }

        // Collect progress and outcomes of the normalization jobs.
        while let Ok(message) = self.norm_rx.try_recv() {
            match message {
                norm::JobMessage::Progress { window_index, stage, fraction } => {
                    if let Some(w) = self.windows.get_mut(window_index) {
                        if let norm::JobState::Running {
                            stage: s,
                            fraction: f,
                            ..
                        } = &mut w.state
                        {
                            *s = stage;
                            *f = fraction;
                        }
                    }
                }
                norm::JobMessage::Finished { window_index, runs, result } => {
                    if let Some(w) = self.windows.get_mut(window_index) {
                        w.state = match result {
                            Ok(output) => norm::JobState::Done {
                                output,
                                finished: chrono::Local::now(),
                                runs,
                            },
                            Err(message) => norm::JobState::Failed { message },
                        };
                    }
                }
            }
        }

        // While jobs run, keep frames coming so the spinner moves and the
        // finished jobs are collected promptly.
        if self
            .windows
            .iter()
            .any(|w| matches!(w.state, norm::JobState::Running { .. }))
        {
            ctx.request_repaint_after(Duration::from_millis(500));
        }

        // Poll the disk so changes made elsewhere show up without user action;
        // request_repaint keeps frames coming while the window is idle.
        if self.auto_refresh {
            let period = Duration::from_secs(self.refresh_secs.max(1) as u64);
            if self.last_refresh.elapsed() >= period {
                self.refresh();
            }
            ctx.request_repaint_after(period);
        }

        self.header(ctx);

        // Slim strip under the header: theme toggle + refresh controls.
        egui::TopBottomPanel::top("controls_bar")
            .frame(
                egui::Frame::new()
                    .fill(theme::surface_weak(&ctx.style().visuals))
                    .inner_margin(egui::Margin {
                        left: 16,
                        right: 16,
                        top: 8,
                        bottom: 8,
                    }),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    theme::toggle_button(ui);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.refresh_controls(ui);
                    });
                });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(theme::SPACE_LG);
                self.ipts_section(ui);
                ui.add_space(theme::SPACE_LG);
                // Everything below needs an IPTS.
                ui.add_enabled_ui(self.ipts.is_some(), |ui| {
                    self.config_section(ui);
                    ui.add_space(theme::SPACE_LG);
                    self.mode_section(ui);
                    ui.add_space(theme::SPACE_LG);
                    self.windows_section(ui);
                    ui.add_space(theme::SPACE_LG);
                    self.runs_table(ui);
                });
                ui.add_space(theme::SPACE_LG);
            });
        });
    }
}

fn main() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([880.0, 820.0])
            .with_title(APP_TITLE),
        ..Default::default()
    };
    eframe::run_native(
        APP_TITLE,
        native_options,
        Box::new(|cc| {
            // Saved light/dark preference, shared by all the VENUS rust
            // tools (dark when none is saved); the controls bar has a toggle.
            cc.egui_ctx.set_theme(theme::load());
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(MonitorApp::new()))
        }),
    )
}
