//! VENUS Auto Normalization — single-view application.
//!
//! Workflow, top to bottom:
//! 1. Select the IPTS (dropdown of accessible IPTS-* folders, or manual
//!    entry). Everything below is disabled until an IPTS is chosen.
//! 2. Select the normalization configuration file
//!    (`<IPTS>/shared/autoreduce/*.h5` or its `configs/` subfolder, created with the marimo
//!    "Normalization TOF at VENUS" notebook — a button launches that
//!    notebook directly in the selected IPTS).
//! 3. Either turn auto-normalization ON (every upcoming run gets
//!    normalized — writes the shared `autoreduction.cfg`), or type a list
//!    of runs to normalize.
//! 4. When a run list is given, a table shows for each run whether its
//!    NeXus / raw / corrected / normalized files exist yet (hover an icon
//!    for the full path). With auto-normalization ON, every run landing
//!    from then on is normalized by this app on its own (configuration's
//!    sample replaced by the run) as soon as its corrected folder exists;
//!    the row shows the progress, then view / open-folder icons. Older
//!    runs show the same icons when their result already sits in the
//!    configuration's output folder, or a button to normalize them now.
//!    A Config drop-down on every row lets a run be normalized with
//!    another configuration file than the section 2 selection.
//!    Each row also names the open beam(s) its normalization uses. An
//!    open-beam run (filed under `images/…/ob` by the DAQ) is never
//!    normalized; once its corrected data is there, a banner offers to
//!    make it (them, when several landed) the open beam(s) of the runs to
//!    come — a copy of the configuration with the new open beams is
//!    written next to it and selected.

mod config;
mod files;
mod h5;
mod norm;
mod notebook;
mod theme;
mod zoom;

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
/// How many per-run normalizations may run side by side (the queue and
/// the automatic pass respect it). Each NeuNorm job is a python process
/// of its own: a few in parallel is cheap on the analysis machines (100+
/// cores, >1 TB), the shared filesystem is the limit.
const DEFAULT_PARALLEL_JOBS: usize = 4;
/// Upper bound of the "parallel jobs" setting.
const MAX_PARALLEL_JOBS: usize = 16;
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
/// Output lines kept per normalization job for the output panel.
const OUTPUT_LINES_KEPT: usize = 600;

/// Sample-environment PVs that can be overlaid on the acquisition timeline:
/// (dataset name under `/entry/DASlogs` of the NeXus files, annotation
/// appended in the selector). The `SPSet`/`RateSet` names are what the DAS
/// records for the Loop1/Loop2 setpoint and setpoint-rate PVs.
const TIMELINE_PVS: &[(&str, &str)] = &[
    ("BL10:SE:ND2:Loop1:SP:RateSet", ""),
    ("BL10:SE:ND2:Loop2:SP:RateSet", ""),
    ("BL10:SE:ND2:Loop1:SPSet", ""),
    ("BL10:SE:ND2:Loop2:SPSet", ""),
    ("BL10:SE:ND2:CH1:PV", " (zone 1)"),
    ("BL10:SE:ND2:CH2:PV", " (zone 2)"),
    ("BL10:SE:ND2:CH4:PV", " (sample)"),
];

/// The (absolute time, value) points of one DASlogs entry.
type PvPoints = Vec<(chrono::DateTime<chrono::FixedOffset>, f64)>;

/// The two views of the "Rolling combine" (4) and "Live reduction" (5)
/// sections: their table, or the acquisition timeline plot.
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

/// What one run's normalization was launched with (recorded when the job
/// starts here; read back from the job log for a result found on disk).
#[derive(Clone, Debug, Default)]
struct RunMeta {
    /// The open-beam folders it divides by.
    obs: Vec<PathBuf>,
    /// The configuration file (settings) it runs with.
    config: Option<PathBuf>,
    /// Its result folder (`…/Run_<run>/normalization`).
    output: Option<PathBuf>,
}

/// The user's per-run settings (✏ in the table): what replaces the
/// configuration's open beams / output folder for that run only.
#[derive(Clone, Debug, Default)]
struct RunOverride {
    obs: Option<Vec<PathBuf>>,
    /// Output base folder: the result goes to `<here>/Run_<run>/normalization`.
    output: Option<PathBuf>,
    /// Configuration file (the Config drop-down): this run is normalized
    /// with it instead of the section 2 selection — its open beams,
    /// settings and output folder, unless the two overrides above say
    /// otherwise.
    config: Option<PathBuf>,
}

impl RunOverride {
    /// Nothing overridden — the entry can be dropped.
    fn is_empty(&self) -> bool {
        self.obs.is_none() && self.output.is_none() && self.config.is_none()
    }
}

/// One selectable open beam in the run editor: a corrected open-beam
/// folder of the IPTS (or one the configuration names elsewhere).
struct ObChoice {
    run: Option<u64>,
    folder: PathBuf,
    /// Complete corrected folder (usable as input)?
    complete: bool,
    frames: usize,
}

/// Which per-run setting a [`RunEditor`] window edits.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorMode {
    /// "⇄ replace by…": the open beams the run divides by.
    OpenBeams,
    /// "✏" next to the Output column: the run's output folder.
    Output,
}

/// The per-run settings window (open beams, or output folder) being
/// edited.
struct RunEditor {
    run: u64,
    mode: EditorMode,
    /// Frames of the run's corrected data (open beams must match).
    frames: usize,
    choices: Vec<ObChoice>,
    /// The open-beam folders currently ticked.
    selected: Vec<PathBuf>,
    output_text: String,
}

/// What the run editor's buttons ask for.
enum EditorAction {
    /// Keep the setting for this run (normalized later, by the automatic
    /// pass or ▶ normalize).
    Apply,
    /// Keep the setting and normalize the run now (re-run when done).
    ApplyAndRun,
    /// Open beams: this run now, and every upcoming run (the ticked open
    /// beams become the configuration's — new file).
    ApplyUpcomingAndRun,
    Cancel,
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
    /// Where the DAQ filed each run's images (its NeXus
    /// `BL10:Exp:IM:ImageFilePath` log, read once): a path under an
    /// `alignment` folder marks an alignment run — never corrected by the
    /// autoreduction, no normalization needed; a path under an `ob`
    /// folder marks an open-beam run — corrected, but a normalization
    /// input, never normalized. Such runs stay listed but are neither
    /// waited for nor put in the windows.
    image_path_cache: HashMap<u64, String>,
    /// The open-beam folders of the selected configuration file (read
    /// when the selection changes): what every upcoming normalization
    /// divides by, shown in the "Open beam" column of the table.
    config_obs: Vec<PathBuf>,
    /// What each run's normalization was launched with (open beams,
    /// configuration, output) — recorded when a job is launched here,
    /// read back from the job log for a result found on disk.
    run_meta: HashMap<u64, RunMeta>,
    /// Per-run overrides of the open beams / output folder (✏ in the
    /// table), kept for the session.
    run_overrides: HashMap<u64, RunOverride>,
    /// What the configuration files picked in the Config column contain
    /// (read when picked, refreshed at every pass): the open beams and
    /// output folder the rows using them show and start from.
    config_infos: HashMap<PathBuf, h5::ConfigInfo>,
    /// The per-run settings window, when open.
    run_editor: Option<RunEditor>,
    /// Error from the last "use as open beam" attempt (writing the
    /// derived configuration), shown until the next successful one.
    ob_error: Option<String>,
    /// Which view of section 5 is open: the table or the acquisition
    /// timeline plot.
    runs_view: RunsView,
    /// Which view of section 4 is open: the windows grid or the same
    /// acquisition timeline.
    windows_view: RunsView,
    /// Sample-environment PV overlaid on the timeline (index into
    /// [`TIMELINE_PVS`]), if any.
    timeline_pv: Option<usize>,
    /// DASlogs points already read: (run, PV dataset name) → (time, value).
    /// An empty vector marks a NeXus without that log (not re-read).
    pv_cache: HashMap<(u64, &'static str), PvPoints>,
    /// Latest (anchor) run the live mode already reacted to: a job volley
    /// fires only when a newer NeXus shows up.
    last_live_anchor: Option<u64>,
    /// Per-run normalizations: state of each run's own job, keyed by run
    /// number — Running while NeuNorm works, Done (output folder) or Failed
    /// afterwards. Done also marks a result found on disk (workflow runner,
    /// previous session).
    run_jobs: HashMap<u64, norm::JobState>,
    /// The configuration the Done-on-disk entries of `run_jobs` were
    /// looked up with — its output folder decides where results live.
    run_jobs_config: Option<PathBuf>,
    /// Where the per-run normalized data lands (`<here>/Run_<run>/
    /// normalization`): the selected configuration's output folder, or the
    /// IPTS default. Shown in the footer.
    output_base: Option<PathBuf>,
    /// Detector name handed to the TIFF viewer (`--detector`, e.g. `tpx1`)
    /// so every stack opens in the right orientation — normalized folders
    /// (`Run_<run>/normalization`) carry no detector in their path for the
    /// viewer to guess from. From the configuration, else the IPTS layout.
    detector: Option<String>,
    /// Live mode: every run the table has shown this session. Rows are
    /// never dropped when a new run lands (the windows slide, the table
    /// does not — a normalization in progress must stay visible).
    table_runs: std::collections::BTreeSet<u64>,
    /// Output lines of each job (script output + runner steps), newest
    /// last, capped — shown in the output panel.
    job_output: HashMap<norm::JobTarget, std::collections::VecDeque<String>>,
    /// The job whose output panel is open, if any.
    output_view: Option<norm::JobTarget>,
    /// Runs waiting for a free slot — "▶ normalize all missing", and every
    /// manual start (▶ normalize, ↻ re-run, the run editor's "normalize
    /// now") asked for while `parallel_jobs` are already running: started
    /// in order as slots free up, never more than `parallel_jobs` at once
    /// (each job holds whole image stacks in memory; too many at once
    /// get the python processes killed by the system).
    run_queue: std::collections::VecDeque<u64>,
    /// Runs whose normalization was killed by the system (SIGKILL: memory
    /// pressure) and already retried once — no second automatic retry.
    killed_retried: HashSet<u64>,
    /// How many per-run normalizations may run at the same time (queue
    /// and automatic pass alike). Each job is heavy, but the machine has
    /// the cores and memory for several.
    parallel_jobs: usize,
    /// Newest run whose corrected data already existed when auto
    /// normalization was seen active: only runs AFTER it get their own
    /// normalization automatically (the app must not chew through the
    /// whole IPTS on startup) — a run still in flight at that moment (NeXus
    /// there, corrected data not yet) counts as new. `None` while auto
    /// normalization is OFF — re-armed when it resumes.
    run_jobs_from: Option<u64>,
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
            image_path_cache: HashMap::new(),
            config_obs: Vec::new(),
            run_meta: HashMap::new(),
            run_overrides: HashMap::new(),
            config_infos: HashMap::new(),
            run_editor: None,
            ob_error: None,
            runs_view: RunsView::Table,
            windows_view: RunsView::Table,
            timeline_pv: None,
            pv_cache: HashMap::new(),
            last_live_anchor: None,
            run_jobs: HashMap::new(),
            run_jobs_config: None,
            output_base: None,
            detector: None,
            table_runs: std::collections::BTreeSet::new(),
            job_output: HashMap::new(),
            output_view: None,
            run_queue: std::collections::VecDeque::new(),
            killed_retried: HashSet::new(),
            parallel_jobs: DEFAULT_PARALLEL_JOBS,
            run_jobs_from: None,
            norm_tx,
            norm_rx,
            viewer_error: None,
            last_refresh: Instant::now(),
            auto_refresh: true,
            refresh_secs: DEFAULT_REFRESH_SECS,
        };
        app.refresh();
        // Convenience: pre-select the IPTS the shared configuration points
        // at. The configuration file defaults to the newest one of the
        // IPTS (select_ipts) — the file the notebook saved last is the one
        // to use; the registered file only fills in when the IPTS has none.
        if let Ok(cfg) = &app.cfg {
            if let Some(ipts) = cfg.get("ipts") {
                let ipts = ipts.to_owned();
                let registered = cfg
                    .get("user_autoreduction_config_file")
                    .map(PathBuf::from);
                app.select_ipts(ipts);
                if app.selected_config.is_none() {
                    if let Some(file) = registered {
                        app.selected_config = Some(file);
                        app.keep_selected_config();
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
        self.image_path_cache.clear();
        self.pv_cache.clear();
        self.rejected.clear();
        self.last_live_anchor = None;
        self.run_jobs.clear();
        self.run_jobs_config = None;
        self.config_obs.clear();
        self.run_meta.clear();
        self.run_overrides.clear();
        self.config_infos.clear();
        self.run_editor = None;
        self.ob_error = None;
        self.output_base = None;
        self.detector = None;
        self.run_jobs_from = None;
        self.table_runs.clear();
        self.job_output.clear();
        self.output_view = None;
        self.run_queue.clear();
        self.killed_retried.clear();
        for w in &mut self.windows {
            w.runs.clear();
            w.state = norm::JobState::Idle;
        }
        self.rescan_configs();
        self.select_newest_config();
        self.check_runs();
    }

    /// Default configuration: the most recent `.h5` of the IPTS (the list
    /// is sorted newest first). Auto normalization ON re-registers it in
    /// autoreduction.cfg when it differs from the registered file.
    fn select_newest_config(&mut self) {
        if let Some(newest) = self.configs.first() {
            self.selected_config = Some(newest.path.clone());
            self.preview_error = None;
        }
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

    /// List the configuration files of the IPTS (`shared/autoreduce/*.h5`
    /// and `shared/autoreduce/configs/*.h5`), newest first.
    fn rescan_configs(&mut self) {
        self.configs.clear();
        self.configs_error = None;
        let Some(ipts_path) = self.ipts_path() else {
            return;
        };
        // The notebook saves either straight into shared/autoreduce or into
        // its configs/ subfolder, depending on the version: scan both.
        let autoreduce = ipts_path.join("shared/autoreduce");
        let dirs = [autoreduce.join("configs"), autoreduce.clone()];
        let mut readable = false;
        for dir in &dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            readable = true;
            for entry in entries.flatten() {
                let path = entry.path();
                let is_h5 = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.eq_ignore_ascii_case("h5"));
                if !is_h5 || !path.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let mtime = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                self.configs.push(ConfigFile { path, name, mtime });
            }
        }
        if !readable {
            self.configs_error = Some(format!(
                "cannot read {} (nor its configs/ subfolder)",
                autoreduce.display()
            ));
            return;
        }
        self.configs.sort_by(|a, b| b.mtime.cmp(&a.mtime));
        self.keep_selected_config();
    }

    /// Reconcile the selection with the list: a selected file that lives
    /// outside the configs folder (picked with "Browse…", or registered by
    /// hand in autoreduction.cfg) is appended to the list so the dropdown
    /// can show it; a selection that no longer exists on disk is dropped.
    fn keep_selected_config(&mut self) {
        let Some(selected) = self.selected_config.clone() else {
            return;
        };
        if self.configs.iter().any(|c| c.path == selected) {
            return;
        }
        match std::fs::metadata(&selected) {
            Ok(meta) if meta.is_file() => {
                let name = selected
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| selected.display().to_string());
                let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                self.configs.push(ConfigFile {
                    path: selected,
                    name,
                    mtime,
                });
            }
            _ => self.selected_config = None,
        }
    }

    /// "Browse…": native file dialog to pick any `.h5` configuration file,
    /// opened in the selected IPTS `shared` folder.
    fn browse_config(&mut self) {
        let Some(ipts_path) = self.ipts_path() else {
            return;
        };
        let start_dir = ipts_path.join("shared");
        let mut dialog = rfd::FileDialog::new()
            .add_filter("Normalization configuration (HDF5)", &["h5", "hdf5"])
            .add_filter("All files", &["*"])
            .set_title("Select a normalization configuration file");
        if start_dir.is_dir() {
            dialog = dialog.set_directory(&start_dir);
        } else {
            dialog = dialog.set_directory(&ipts_path);
        }
        if let Some(path) = dialog.pick_file() {
            self.selected_config = Some(path);
            self.preview_error = None;
            self.keep_selected_config();
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
            // The span only grows during the session: a run stays listed
            // once shown, even after the windows slid past it.
            self.table_runs.extend(self.window_span.iter().copied());
            self.table_runs.iter().copied().collect()
        };
        self.run_files = match self.ipts_path() {
            Some(ipts_path) if !table_runs.is_empty() => {
                files::check_runs(&ipts_path, &table_runs)
            }
            _ => Vec::new(),
        };
        // Once a run's NeXus is there, find out what kind of run it is: an
        // alignment run is shown as such and never waited for.
        if let Some(ipts_path) = self.ipts_path() {
            let present: Vec<u64> = self
                .run_files
                .iter()
                .filter(|rf| matches!(rf.nexus, files::FileStatus::Present(_)))
                .map(|rf| rf.run)
                .collect();
            for run in present {
                self.classify_run(&ipts_path, run);
            }
        }
        self.launch_run_jobs();
        // The next NeXus that will land in the IPTS (latest one + 1): what
        // auto-normalization will process next.
        self.next_run = if self.is_active() && self.live_enabled() {
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
            self.classify_run(&ipts_path, run);
            // The anchor is the newest run, rejected or not: rejecting the
            // latest run(s) must not slide the span back onto older runs —
            // the table just keeps waiting for the next run to show up.
            if anchor.is_none() {
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
        // Rejected runs never enter the windows (the windows anchor on the
        // newest kept run inside the span), but stay in the table span so
        // they can be restored; alignment and open-beam runs likewise
        // (nothing to normalize: no corrected data would ever come for an
        // alignment run, and an open beam is an input). When every run of
        // the span is out the windows are simply empty until a new run
        // lands.
        let kept: Vec<(u64, chrono::DateTime<chrono::FixedOffset>)> = end_times
            .iter()
            .filter(|(run, _)| !self.rejected.contains(run) && !self.skips_normalization(*run))
            .copied()
            .collect();
        norm::assign_windows(&mut self.windows, &kept);
        let mut span: Vec<u64> = end_times.iter().map(|(run, _)| *run).collect();
        span.sort_unstable();
        self.window_span = span;

        // Live and hybrid modes: a new anchor run (a NeXus that just
        // showed up — in hybrid mode, just joined the list) fires the
        // window normalizations — only when the user opted the windows
        // into the auto normalization (shared `rolling_combine` flag);
        // otherwise the auto normalization does the plain per-run work
        // alone. The first anchor seen only arms the trigger — the app
        // should not fire for a run that landed before it was even
        // watching.
        if self.is_active() && self.rolling_enabled() {
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
            // Re-armed when auto normalization (or the opt-in) resumes.
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
                    w.state = norm::JobState::Failed { message: e.clone(), log: None };
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
                        stages: Vec::new(),
                    };
                    self.job_output.remove(&spec.target);
                    norm::launch(spec, self.norm_tx.clone());
                }
                Err(message) => self.windows[i].state = norm::JobState::Failed { message, log: None },
            }
        }
    }

    /// Keep the per-run normalizations in step with the table.
    ///
    /// 1. A different configuration file (its output folder decides where
    ///    results live) drops what was found on disk with the previous one.
    /// 2. Every listed run whose result already exists in the
    ///    configuration's output folder (workflow runner, previous session)
    ///    is marked Done — never redone.
    /// 3. Auto normalization ON: every run that landed after the app
    ///    started watching is normalized on its own as soon as its
    ///    corrected folder is there (the "Corrected" column turned green).
    ///    Older runs get a "▶ normalize" button in the table instead.
    fn launch_run_jobs(&mut self) {
        let Some(ipts_path) = self.ipts_path() else {
            return;
        };
        if self.run_jobs_config != self.selected_config {
            self.run_jobs
                .retain(|_, state| matches!(state, norm::JobState::Running { .. }));
            let running: Vec<u64> = self.run_jobs.keys().copied().collect();
            self.run_meta.retain(|run, _| running.contains(run));
            self.run_jobs_config = self.selected_config.clone();
            // Auto normalization ON and a different file selected: the
            // shared autoreduction.cfg must follow, or the autoreduction
            // keeps normalizing with the previous configuration.
            if let (Ok(cfg), Some(ipts), Some(selected)) =
                (&self.cfg, self.ipts.clone(), self.selected_config.clone())
            {
                let registered = cfg.get("user_autoreduction_config_file").map(Path::new);
                if cfg.activate && registered != Some(selected.as_path()) {
                    let (rolling, live) = (cfg.rolling_combine, cfg.live_reduction);
                    match config::write_full(
                        &self.cfg_path,
                        &ipts,
                        &selected.display().to_string(),
                        true,
                        rolling,
                        live,
                    ) {
                        Ok(()) => {
                            self.write_error = None;
                            self.cfg = config::read(&self.cfg_path);
                        }
                        Err(e) => self.write_error = Some(e),
                    }
                }
            }
            // Where the results go (footer) and which detector took the
            // data (viewer orientation): from the configuration, with the
            // IPTS layout as fallback.
            let info = self
                .selected_config
                .as_ref()
                .and_then(|config| h5::read_config_info(config).ok());
            self.output_base = self.selected_config.as_ref().map(|_| {
                info.as_ref()
                    .and_then(|i| i.output_folder.clone())
                    .unwrap_or_else(|| ipts_path.join("shared/autoreduce/normalized"))
            });
            self.detector = info
                .as_ref()
                .and_then(|i| i.detector.clone())
                .or_else(|| Self::detector_from_layout(&ipts_path));
            self.config_obs = info.map(|i| i.ob_folders).unwrap_or_default();
        }
        // The per-run automatic pass needs auto normalization ON and the
        // live reduction opted in (shared `live_reduction` flag).
        match self.cfg.as_ref().map(|c| c.activate && c.live_reduction) {
            Ok(true) => {
                if self.run_jobs_from.is_none() {
                    // First look while active: everything already
                    // normalizable is old news — the "▶ normalize" button
                    // covers it. A run whose corrected data is not there
                    // yet is still in flight: it will be normalized.
                    self.run_jobs_from = Some(Self::newest_corrected_run(&ipts_path));
                }
            }
            // Auto normalization OFF (or live reduction opted out):
            // re-armed when it resumes.
            Ok(false) => self.run_jobs_from = None,
            // Unreadable shared configuration (transient on the shared
            // filesystem): keep the current arming, do not lose track of
            // the runs that landed meanwhile.
            Err(_) => {}
        }
        // Alignment and open-beam runs are left alone: nothing to look up
        // or normalize. A run without any configuration (none selected in
        // section 2, none picked in its Config drop-down) has nothing to
        // look up or normalize with — retried next refresh.
        let pending: Vec<(u64, PathBuf)> = self
            .run_files
            .iter()
            .map(|rf| rf.run)
            .filter(|run| !self.run_jobs.contains_key(run) && !self.skips_normalization(*run))
            .filter_map(|run| self.config_for_run(run).map(|config| (run, config)))
            .collect();
        if pending.is_empty() {
            return;
        }
        // Each distinct configuration file is read once per pass (most
        // runs share the section 2 file; the Config column may point a
        // few of them elsewhere — those readings also refresh what their
        // rows show).
        let mut infos: HashMap<PathBuf, Result<h5::ConfigInfo, String>> = HashMap::new();
        for (run, config) in pending {
            let info = infos
                .entry(config.clone())
                .or_insert_with(|| h5::read_config_info(&config));
            let info = match info {
                Ok(info) => info.clone(),
                Err(e) => {
                    // Surface the problem on the run that needs it.
                    self.run_jobs
                        .insert(run, norm::JobState::Failed { message: e.clone(), log: None });
                    continue;
                }
            };
            if Some(&config) != self.selected_config.as_ref() {
                self.config_infos.insert(config.clone(), info.clone());
            }
            // The row's own settings (✏) decide where its result lives.
            let output = norm::run_output_dir(&ipts_path, run, &self.effective_info(run, &info));
            if norm::output_is_done(&output) {
                let finished = std::fs::metadata(&output)
                    .and_then(|m| m.modified())
                    .map(chrono::DateTime::<chrono::Local>::from)
                    .unwrap_or_else(|_| chrono::Local::now());
                // What that result was launched with: its job log says
                // (this tool's and the workflow runner's alike).
                let log = output
                    .parent()
                    .map(|row| row.join("logs").join("normalization.log"));
                let launch = log.and_then(|log| norm::launch_from_log(&log));
                self.run_meta.insert(
                    run,
                    RunMeta {
                        obs: launch.as_ref().map(|l| l.obs.clone()).unwrap_or_default(),
                        config: launch.and_then(|l| l.config),
                        output: Some(output.clone()),
                    },
                );
                self.run_jobs.insert(
                    run,
                    norm::JobState::Done {
                        output,
                        finished,
                        runs: vec![run],
                    },
                );
                continue;
            }
            let watched = self.run_jobs_from.is_some_and(|from| run > from);
            // At the cap the run is simply not registered: the next pass
            // picks it up once a slot frees up.
            if watched && !self.rejected.contains(&run) && self.has_free_job_slot() {
                self.start_run_job(run, &ipts_path, &config, &info);
            }
        }
    }

    /// Number of per-run normalizations currently running.
    fn running_run_jobs(&self) -> usize {
        self.run_jobs
            .values()
            .filter(|s| matches!(s, norm::JobState::Running { .. }))
            .count()
    }

    /// May one more per-run normalization start now?
    fn has_free_job_slot(&self) -> bool {
        self.running_run_jobs() < self.parallel_jobs.max(1)
    }

    /// The newest run of the IPTS whose corrected folder already exists,
    /// looking at the newest runs only (older ones are irrelevant for the
    /// arming). 0 when none of them has corrected data yet — then every
    /// run is new.
    fn newest_corrected_run(ipts_path: &Path) -> u64 {
        const LOOKBACK: usize = 30;
        let runs = files::list_nexus_runs(ipts_path);
        let newest: Vec<u64> = runs.iter().rev().take(LOOKBACK).copied().collect();
        if newest.is_empty() {
            return 0;
        }
        files::check_runs(ipts_path, &newest)
            .iter()
            .filter(|rf| matches!(rf.corrected, files::FileStatus::Present(_)))
            .map(|rf| rf.run)
            .max()
            .unwrap_or(0)
    }

    /// Normalize one run on its own (configuration's sample replaced by the
    /// run) if its corrected folder is there; the outcome lands in
    /// `run_jobs[run]`. Without corrected data nothing happens — the next
    /// pass retries.
    fn start_run_job(
        &mut self,
        run: u64,
        ipts_path: &Path,
        config: &Path,
        info: &h5::ConfigInfo,
    ) {
        let corrected = match self.run_files.iter().find(|rf| rf.run == run) {
            Some(rf) => match &rf.corrected {
                files::FileStatus::Present(folder) => Ok(folder.clone()),
                files::FileStatus::Writing(folder) => Err(format!(
                    "the corrected data of run {run} is still being written — \
                     retried automatically once the folder is complete\n{}",
                    folder.display()
                )),
                files::FileStatus::Missing(expected) => Err(format!(
                    "no corrected data for run {run} yet (autoreduction) — expected \
                     under {}",
                    expected.display()
                )),
            },
            None => Err(format!("run {run} is not listed in the table")),
        };
        let corrected = match corrected {
            Ok(folder) => folder,
            Err(message) => {
                // Only a run someone asked for explicitly gets the message
                // (the automatic pass simply retries next refresh).
                if self.run_jobs_from.is_none_or(|from| run <= from) {
                    self.run_jobs
                        .insert(run, norm::JobState::Failed { message, log: None });
                }
                return;
            }
        };
        // The row's own settings (✏) replace the configuration's open
        // beams / output folder for this run.
        let info = self.effective_info(run, info);
        let state = match norm::prepare_run_job(run, &corrected, ipts_path, config, &info) {
            Ok(spec) => {
                self.job_output.remove(&spec.target);
                self.run_meta.insert(
                    run,
                    RunMeta {
                        obs: spec.ob_folders(),
                        config: Some(spec.config().to_path_buf()),
                        output: Some(spec.output().to_path_buf()),
                    },
                );
                norm::launch(spec, self.norm_tx.clone());
                norm::JobState::Running {
                    runs: vec![run],
                    stage: "starting…".to_owned(),
                    fraction: None,
                    stages: Vec::new(),
                }
            }
            Err(message) => {
                self.run_meta.remove(&run);
                norm::JobState::Failed { message, log: None }
            }
        };
        self.run_jobs.insert(run, state);
    }

    /// The configuration file a run is normalized with: the one picked in
    /// its Config drop-down, else the section 2 selection.
    fn config_for_run(&self, run: u64) -> Option<PathBuf> {
        self.run_overrides
            .get(&run)
            .and_then(|o| o.config.clone())
            .or_else(|| self.selected_config.clone())
    }

    /// The contents of the configuration picked for a run in its Config
    /// drop-down (`None` when the run follows the section 2 selection, or
    /// the file could not be read).
    fn own_config_info(&self, run: u64) -> Option<&h5::ConfigInfo> {
        self.run_overrides
            .get(&run)
            .and_then(|o| o.config.as_ref())
            .and_then(|config| self.config_infos.get(config))
    }

    /// The open beams a run's configuration names (its own file, else the
    /// section 2 selection's) — what the run divides by unless ⇄ says
    /// otherwise.
    fn config_obs_for_run(&self, run: u64) -> Vec<PathBuf> {
        match self.own_config_info(run) {
            Some(info) => info.ob_folders.clone(),
            None => self.config_obs.clone(),
        }
    }

    /// The output base folder of a run's configuration (its own file,
    /// else the section 2 selection's) — where its result goes unless ✏
    /// says otherwise.
    fn output_base_for_run(&self, run: u64) -> Option<PathBuf> {
        match self.own_config_info(run) {
            Some(info) => Some(
                info.output_folder
                    .clone()
                    .unwrap_or_else(|| self.ipts_path().unwrap_or_default().join("shared/autoreduce/normalized")),
            ),
            None => self.output_base.clone(),
        }
    }

    /// Read a configuration file picked in the Config column into
    /// `config_infos` (once; an unreadable file is simply not cached —
    /// the pass reports the error on the row).
    fn cache_config_info(&mut self, config: &Path) {
        if self.config_infos.contains_key(config) {
            return;
        }
        if let Ok(info) = h5::read_config_info(config) {
            self.config_infos.insert(config.to_path_buf(), info);
        }
    }

    /// The Config drop-down of a row: normalize that run with `config`
    /// (`None` = back to the section 2 selection). A result found on disk
    /// with the previous file is looked up again — another configuration
    /// may keep its results elsewhere — and the automatic pass / the
    /// ▶ normalize button use the new file; a normalization already
    /// running on the run is left alone (the new file applies to its
    /// next run).
    fn set_run_config(&mut self, run: u64, config: Option<PathBuf>) {
        let before = self.config_for_run(run);
        let config = config.filter(|c| Some(c) != self.selected_config.as_ref());
        if let Some(config) = &config {
            self.cache_config_info(config);
        }
        let mut over = self.run_overrides.get(&run).cloned().unwrap_or_default();
        over.config = config;
        if over.is_empty() {
            self.run_overrides.remove(&run);
        } else {
            self.run_overrides.insert(run, over);
        }
        if self.config_for_run(run) == before {
            return;
        }
        if matches!(self.run_jobs.get(&run), Some(norm::JobState::Running { .. })) {
            return;
        }
        self.run_jobs.remove(&run);
        self.run_meta.remove(&run);
        self.launch_run_jobs();
    }

    /// "▶ normalize" / "↻" in the table: normalize one run now, whatever
    /// its age.
    fn normalize_run_now(&mut self, run: u64) {
        let (Some(ipts_path), Some(config)) = (self.ipts_path(), self.config_for_run(run))
        else {
            return;
        };
        self.run_jobs.remove(&run);
        match h5::read_config_info(&config) {
            Ok(info) => self.start_run_job(run, &ipts_path, &config, &info),
            Err(message) => {
                self.run_jobs.insert(run, norm::JobState::Failed { message, log: None });
            }
        }
    }

    /// The configuration as it applies to one run: the file's values with
    /// the row's overrides (✏) — open beams and/or output folder — on top.
    fn effective_info(&self, run: u64, base: &h5::ConfigInfo) -> h5::ConfigInfo {
        let mut info = base.clone();
        if let Some(over) = self.run_overrides.get(&run) {
            if let Some(obs) = &over.obs {
                info.ob_folders = obs.clone();
            }
            if let Some(output) = &over.output {
                info.output_folder = Some(output.clone());
            }
        }
        info
    }

    /// Where a run's result goes with the current settings (configuration
    /// + the row's overrides), if a configuration is selected.
    fn planned_output(&self, run: u64) -> Option<PathBuf> {
        let ipts_path = self.ipts_path()?;
        let config = self.config_for_run(run)?;
        let info = h5::read_config_info(&config).ok()?;
        Some(norm::run_output_dir(&ipts_path, run, &self.effective_info(run, &info)))
    }

    /// "↻ re-run" on a normalized row (or "Apply & re-run" in the run
    /// editor): normalize the run again with the row's current settings.
    /// A result sitting where the new one will go is moved aside first as
    /// `normalization.previous` (an older `.previous` is replaced) — the
    /// job never runs twice into the same folder. The job log keeps the
    /// history (append-only).
    fn rerun_run(&mut self, run: u64) {
        if matches!(self.run_jobs.get(&run), Some(norm::JobState::Running { .. })) {
            return; // already running (started meanwhile): never twice at once
        }
        if let Some(norm::JobState::Done { output, .. }) = self.run_jobs.get(&run) {
            let output = output.clone();
            if self.planned_output(run).as_ref() == Some(&output) {
                let previous = output.with_extension("previous");
                let _ = std::fs::remove_dir_all(&previous);
                if let Err(e) = std::fs::rename(&output, &previous) {
                    self.run_jobs.insert(
                        run,
                        norm::JobState::Failed {
                            message: format!(
                                "cannot move the previous result aside ({} → {}): {e}",
                                output.display(),
                                previous.display()
                            ),
                            log: None,
                        },
                    );
                    return;
                }
            }
        }
        self.normalize_run_now(run);
    }

    /// A manual start (▶ normalize, ↻ re-run, "normalize now" in the run
    /// editor): run now when a slot is free, else wait in the queue —
    /// `parallel_jobs` is a hard cap, manual starts included. Fourteen
    /// re-runs clicked in a row used to start fourteen python processes
    /// at once, and the machine's memory ran out (SIGKILL).
    fn request_run(&mut self, run: u64) {
        if matches!(self.run_jobs.get(&run), Some(norm::JobState::Running { .. })) {
            return;
        }
        if self.has_free_job_slot() {
            self.rerun_run(run);
        } else if !self.run_queue.contains(&run) {
            self.run_queue.push_back(run);
        }
    }

    /// The corrected open-beam folders of the IPTS
    /// (`shared/autoreduce/images/<detector>/ob/…`, run folders up to two
    /// levels below `ob`), plus `extra` folders (the configuration's, which
    /// may live in another IPTS), newest run first — the choices of the
    /// run editor, each with its frame count and whether it is complete.
    fn scan_ob_choices(ipts_path: &Path, extra: &[PathBuf]) -> Vec<ObChoice> {
        fn collect(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                if files::run_number_in_name(&name).is_some() {
                    found.push(entry.path());
                } else if depth < 2 {
                    collect(&entry.path(), depth + 1, found);
                }
            }
        }
        let mut found: Vec<PathBuf> = Vec::new();
        if let Ok(detectors) = std::fs::read_dir(ipts_path.join("shared/autoreduce/images")) {
            for detector in detectors.flatten() {
                collect(&detector.path().join("ob"), 0, &mut found);
            }
        }
        for folder in extra {
            if !found.contains(folder) {
                found.push(folder.clone());
            }
        }
        let mut choices: Vec<ObChoice> = found
            .into_iter()
            .map(|folder| {
                let name = folder
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ObChoice {
                    run: files::run_number_in_name(&name),
                    complete: files::folder_complete(&folder, files::FolderKind::Corrected),
                    frames: files::tiff_count(&folder),
                    folder,
                }
            })
            .collect();
        choices.sort_by(|a, b| b.run.cmp(&a.run).then_with(|| a.folder.cmp(&b.folder)));
        choices
    }

    /// "⇄ replace by…" / "✏" in the table: open the settings window of
    /// one run — its open beams (every corrected open-beam folder of the
    /// IPTS listed, the ones the run currently uses ticked) or its output
    /// folder (pre-filled with what the run currently uses).
    fn open_run_editor(&mut self, run: u64, mode: EditorMode) {
        let Some(ipts_path) = self.ipts_path() else { return };
        let over = self.run_overrides.get(&run).cloned().unwrap_or_default();
        let selected = over.obs.clone().unwrap_or_else(|| self.config_obs_for_run(run));
        let output = over.output.clone().or_else(|| self.output_base_for_run(run));
        let frames = self
            .run_files
            .iter()
            .find(|rf| rf.run == run)
            .and_then(|rf| match &rf.corrected {
                files::FileStatus::Present(folder) => Some(files::tiff_count(folder)),
                _ => None,
            })
            .unwrap_or(0);
        let choices = if mode == EditorMode::OpenBeams {
            let mut extra = self.config_obs_for_run(run);
            extra.extend(selected.iter().cloned());
            Self::scan_ob_choices(&ipts_path, &extra)
        } else {
            Vec::new()
        };
        self.run_editor = Some(RunEditor {
            run,
            mode,
            frames,
            choices,
            selected,
            output_text: output.map(|p| p.display().to_string()).unwrap_or_default(),
        });
    }

    /// Apply the run editor: store the row's override of the edited
    /// setting (nothing stored when it equals the configuration's; the
    /// other setting's override is kept), for the open beams optionally
    /// make them the configuration's for the upcoming runs, then either
    /// normalize the run now (`run_now` — moving a previous result aside;
    /// only once its corrected data is complete) or let the automatic
    /// pass / the ▶ normalize button use the new settings. A result found
    /// on disk with the old settings is looked up again.
    fn apply_run_editor(&mut self, editor: RunEditor, run_now: bool, upcoming: bool) {
        let run = editor.run;
        let mut over = self.run_overrides.get(&run).cloned().unwrap_or_default();
        let mut selected = editor.selected;
        match editor.mode {
            EditorMode::OpenBeams => {
                selected.sort_by_key(|f| {
                    f.file_name()
                        .and_then(|n| files::run_number_in_name(&n.to_string_lossy()))
                        .unwrap_or(u64::MAX)
                });
                let same_obs = {
                    let mut a = selected.clone();
                    let mut b = self.config_obs_for_run(run);
                    a.sort();
                    b.sort();
                    a == b
                };
                over.obs = (!selected.is_empty() && !same_obs).then(|| selected.clone());
            }
            EditorMode::Output => {
                let output_text = editor.output_text.trim();
                over.output = (!output_text.is_empty()
                    && self.output_base_for_run(run).as_deref() != Some(Path::new(output_text)))
                .then(|| PathBuf::from(output_text));
            }
        }
        if over.is_empty() {
            self.run_overrides.remove(&run);
        } else {
            self.run_overrides.insert(run, over);
        }
        if upcoming && !selected.is_empty() {
            self.use_as_open_beams(selected.clone());
            // The configuration now names them: no override needed for
            // this run either.
            if let Some(over) = self.run_overrides.get_mut(&run) {
                if over.obs.as_ref().is_some_and(|obs| {
                    let mut a = obs.clone();
                    let mut b = self.config_obs.clone();
                    a.sort();
                    b.sort();
                    a == b
                }) {
                    over.obs = None;
                }
            }
            self.run_overrides.retain(|_, over| !over.is_empty());
        }
        if matches!(self.run_jobs.get(&run), Some(norm::JobState::Running { .. })) {
            return; // the settings apply to the next run of this row
        }
        let corrected_ready = self
            .run_files
            .iter()
            .find(|rf| rf.run == run)
            .is_some_and(|rf| matches!(rf.corrected, files::FileStatus::Present(_)));
        if run_now && corrected_ready {
            self.request_run(run);
        } else {
            self.run_jobs.remove(&run);
            self.run_meta.remove(&run);
            self.check_runs();
        }
    }

    /// The run editor window (open beams, or output folder, of one run),
    /// drawn while `run_editor` is set.
    fn run_editor_window(&mut self, ui: &mut egui::Ui) {
        let Some(mut editor) = self.run_editor.take() else { return };
        let run = editor.run;
        let config_obs = self.config_obs_for_run(run);
        let output_base = self.output_base_for_run(run);
        let state = self.run_jobs.get(&run);
        let done = matches!(state, Some(norm::JobState::Done { .. }));
        let running = matches!(state, Some(norm::JobState::Running { .. }));
        let corrected_ready = self
            .run_files
            .iter()
            .find(|rf| rf.run == run)
            .is_some_and(|rf| matches!(rf.corrected, files::FileStatus::Present(_)));
        let has_config = self.config_for_run(run).is_some();
        let mut action: Option<EditorAction> = None;
        let mut open = true;
        let title = match editor.mode {
            EditorMode::OpenBeams => format!("Run {run} — replace the open beams by…"),
            EditorMode::Output => format!("Run {run} — output folder"),
        };
        egui::Window::new(title)
            .id(egui::Id::new("run_editor"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.set_min_width(560.0);
                let dim = theme::text_emphasis(ui.visuals());
                match editor.mode {
                    EditorMode::OpenBeams => {
                        Self::open_beams_editor(ui, &mut editor, &config_obs, dim);
                    }
                    EditorMode::Output => {
                        Self::output_editor(ui, &mut editor, output_base.as_deref(), dim);
                    }
                }
                ui.add_space(theme::SPACE_SM);
                ui.horizontal(|ui| {
                    let now = if done { "re-run now" } else { "normalize now" };
                    let run_hover = |what: &str| {
                        if running {
                            "This run is being normalized — the setting applies to its \
                             next normalization"
                                .to_owned()
                        } else if !corrected_ready {
                            format!(
                                "{what} — normalized as soon as the corrected data of run \
                                 {run} is complete (automatically when auto normalization \
                                 covers it, else with ▶ normalize)"
                            )
                        } else if done {
                            format!(
                                "{what} and normalize run {run} again at once — the \
                                 result already in the target folder is kept as \
                                 normalization.previous"
                            )
                        } else {
                            format!("{what} and normalize run {run} at once")
                        }
                    };
                    match editor.mode {
                        EditorMode::OpenBeams => {
                            let none = editor.selected.is_empty();
                            let button = ui.add_enabled(
                                !none && !running,
                                theme::primary_button(&format!("Replace for this run & {now}")),
                            );
                            let button = if none {
                                button.on_disabled_hover_text("Tick at least one open beam")
                            } else {
                                button.on_hover_text(run_hover(
                                    "Use the ticked open beams for this run only",
                                ))
                            };
                            if button.clicked() {
                                action = Some(EditorAction::ApplyAndRun);
                            }
                            let button = ui.add_enabled(
                                !none && !running && has_config,
                                egui::Button::new(format!(
                                    "Replace for this run and every upcoming run & {now}"
                                )),
                            );
                            let button = if none {
                                button.on_disabled_hover_text("Tick at least one open beam")
                            } else if !has_config {
                                button.on_disabled_hover_text(
                                    "Select a configuration file first (section 2)",
                                )
                            } else {
                                button.on_hover_text(format!(
                                    "{}\nA copy of the selected configuration with the \
                                     ticked open beams is written next to it and selected: \
                                     every run normalized from now on divides by them (with \
                                     auto normalization ON the new file is registered in \
                                     the shared autoreduction.cfg at once). Runs already \
                                     normalized are not redone.",
                                    run_hover("Use the ticked open beams for this run")
                                ))
                            };
                            if button.clicked() {
                                action = Some(EditorAction::ApplyUpcomingAndRun);
                            }
                        }
                        EditorMode::Output => {
                            if ui
                                .add(theme::primary_button("Apply"))
                                .on_hover_text(
                                    "Keep this output folder for this run: used by the \
                                     automatic normalization / the ▶ normalize button. A \
                                     result already there is looked up in the new folder.",
                                )
                                .clicked()
                            {
                                action = Some(EditorAction::Apply);
                            }
                            let button = ui.add_enabled(
                                !running,
                                egui::Button::new(format!("Apply & {now}")),
                            );
                            if button.on_hover_text(run_hover("Keep this output folder")).clicked() {
                                action = Some(EditorAction::ApplyAndRun);
                            }
                        }
                    }
                    if ui.button("Cancel").clicked() {
                        action = Some(EditorAction::Cancel);
                    }
                    if running {
                        ui.label(
                            egui::RichText::new("normalization running").color(theme::INFO),
                        );
                    }
                });
            });
        if !open {
            action = Some(EditorAction::Cancel);
        }
        match action {
            None => self.run_editor = Some(editor),
            Some(EditorAction::Cancel) => {}
            Some(EditorAction::Apply) => self.apply_run_editor(editor, false, false),
            Some(EditorAction::ApplyAndRun) => self.apply_run_editor(editor, true, false),
            Some(EditorAction::ApplyUpcomingAndRun) => self.apply_run_editor(editor, true, true),
        }
    }

    /// Body of the open-beams editor: every corrected open-beam folder of
    /// the IPTS as a checkbox (newest first, frame count, flags), the
    /// ticked ones summarized.
    fn open_beams_editor(
        ui: &mut egui::Ui,
        editor: &mut RunEditor,
        config_obs: &[PathBuf],
        dim: egui::Color32,
    ) {
        let run = editor.run;
        ui.label(
            egui::RichText::new(if editor.frames > 0 {
                format!(
                    "The corrected data of run {run} has {} frames — tick open beams \
                     with the same number of frames (1 or more).",
                    editor.frames
                )
            } else {
                format!(
                    "Run {run} has no complete corrected data yet — tick the open beams \
                     (1 or more) it will divide by."
                )
            })
            .color(dim),
        );
        ui.add_space(theme::SPACE_XS);
        let mut toggled: Vec<(PathBuf, bool)> = Vec::new();
        egui::ScrollArea::vertical()
            .id_salt("ob_choices")
            .max_height(260.0)
            .show(ui, |ui| {
                for choice in &editor.choices {
                    let mut checked = editor.selected.contains(&choice.folder);
                    let name = choice
                        .folder
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let mut label = match choice.run {
                        Some(r) => format!("run {r} — {} frames", choice.frames),
                        None => format!("{name} — {} frames", choice.frames),
                    };
                    if !choice.complete {
                        label.push_str("  (⏳ corrected data not complete)");
                    } else if editor.frames > 0 && choice.frames != editor.frames {
                        label.push_str("  (⚠ frame count differs)");
                    }
                    if config_obs.contains(&choice.folder) {
                        label.push_str("  [configuration's]");
                    }
                    let response = ui
                        .add_enabled(choice.complete, egui::Checkbox::new(&mut checked, label))
                        .on_hover_text(choice.folder.display().to_string());
                    if response.changed() {
                        toggled.push((choice.folder.clone(), checked));
                    }
                }
                if editor.choices.is_empty() {
                    ui.label(
                        egui::RichText::new(
                            "No corrected open-beam folder found in this IPTS \
                             (shared/autoreduce/images/<detector>/ob)",
                        )
                        .color(dim),
                    );
                }
            });
        for (folder, checked) in toggled {
            if checked {
                if !editor.selected.contains(&folder) {
                    editor.selected.push(folder);
                }
            } else {
                editor.selected.retain(|f| f != &folder);
            }
        }
        ui.horizontal(|ui| {
            if ui
                .button("↺ configuration's")
                .on_hover_text("Tick the configuration file's open beams again")
                .clicked()
            {
                editor.selected = config_obs.to_vec();
            }
            if ui.button("none").clicked() {
                editor.selected.clear();
            }
            ui.label(
                egui::RichText::new(if editor.selected.is_empty() {
                    "none ticked".to_owned()
                } else {
                    format!("ticked: {}", Self::ob_runs_text(&editor.selected))
                })
                .color(dim),
            );
        });
    }

    /// Body of the output-folder editor: the folder (typed or browsed)
    /// the run's result goes to.
    fn output_editor(
        ui: &mut egui::Ui,
        editor: &mut RunEditor,
        output_base: Option<&Path>,
        dim: egui::Color32,
    ) {
        let run = editor.run;
        ui.label(egui::RichText::new("Output folder for this run").strong());
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut editor.output_text)
                    .desired_width(400.0)
                    .hint_text("the configuration's output folder"),
            );
            if ui.button("📂 Browse…").clicked() {
                let mut dialog = rfd::FileDialog::new().set_title(format!(
                    "Output folder for run {run} (the result goes to \
                     Run_{run}/normalization inside it)"
                ));
                let start = Path::new(editor.output_text.trim());
                if start.is_dir() {
                    dialog = dialog.set_directory(start);
                } else if let Some(base) = output_base.filter(|b| b.is_dir()) {
                    dialog = dialog.set_directory(base);
                }
                if let Some(folder) = dialog.pick_folder() {
                    editor.output_text = folder.display().to_string();
                }
            }
            if ui
                .button("↺ configuration's")
                .on_hover_text("Back to the configuration file's output folder")
                .clicked()
            {
                editor.output_text = output_base
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
            }
        });
        ui.label(
            egui::RichText::new(format!("The result goes to <folder>/Run_{run}/normalization"))
                .color(dim)
                .small(),
        );
    }

    /// Open a folder in the desktop file manager, or a file (e.g. a job
    /// log) in its default application — detached, via xdg-open.
    fn open_folder(&mut self, folder: &Path) {
        self.viewer_error = if !folder.exists() {
            Some(format!("not found: {}", folder.display()))
        } else {
            std::process::Command::new("xdg-open")
                .arg(folder)
                .spawn()
                .map(|_| ())
                .err()
                .map(|e| format!("cannot launch xdg-open: {e}"))
        };
    }

    /// The detector folder of the IPTS reduction layout
    /// (`shared/autoreduce/images/<detector>`, e.g. `tpx1`) when there is
    /// exactly one — the fallback when the configuration names none.
    fn detector_from_layout(ipts_path: &Path) -> Option<String> {
        let dir = ipts_path.join("shared/autoreduce/images");
        let names: Vec<String> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| !n.starts_with('.'))
            .collect();
        match names.as_slice() {
            [one] => Some(one.clone()),
            _ => None,
        }
    }

    /// Open normalized-data folders in ONE TIFF viewer session (detached):
    /// the first folder is the main stack, the others are `--compare`
    /// stacks shown side by side (shared colorscale, mirrored regions).
    /// The detector is passed along so the frames come up in the right
    /// orientation right away.
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
            if let Some(detector) = &self.detector {
                cmd.arg("--detector").arg(detector);
            }
            cmd.spawn()
                .map(|_| ())
                .err()
                .map(|e| format!("cannot launch {TIFF_VIEWER_CMD}: {e}"))
        };
    }

    /// The state slot a job message refers to (a window's or a run's).
    fn job_state_mut(&mut self, target: norm::JobTarget) -> Option<&mut norm::JobState> {
        match target {
            norm::JobTarget::Window(i) => self.windows.get_mut(i).map(|w| &mut w.state),
            norm::JobTarget::Run(run) => self.run_jobs.get_mut(&run),
        }
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

    /// What a run is (sample, open beam, alignment), per the image folder
    /// its NeXus records. A run whose NeXus could not be read yet counts
    /// as a sample run (classified on a later refresh).
    fn run_kind(&self, run: u64) -> h5::RunKind {
        self.image_path_cache
            .get(&run)
            .map(|path| h5::image_path_kind(path))
            .unwrap_or(h5::RunKind::Sample)
    }

    /// Is a run an open-beam run?
    fn is_open_beam(&self, run: u64) -> bool {
        self.run_kind(run) == h5::RunKind::OpenBeam
    }

    /// Is a run never normalized (alignment or open beam)? Such a run is
    /// neither waited for nor put in the windows.
    fn skips_normalization(&self, run: u64) -> bool {
        self.run_kind(run) != h5::RunKind::Sample
    }

    /// Why a run is out of the windows and the normalizations, for the
    /// timeline / hovers: "alignment" or "open beam".
    fn kind_label(&self, run: u64) -> Option<&'static str> {
        match self.run_kind(run) {
            h5::RunKind::Sample => None,
            h5::RunKind::OpenBeam => Some("open beam"),
            h5::RunKind::Alignment => Some("alignment"),
        }
    }

    /// The run numbers of open-beam folders (from the `Run_<n>` token of
    /// each folder name), in order; a folder without one is skipped.
    fn ob_run_numbers(folders: &[PathBuf]) -> Vec<u64> {
        folders
            .iter()
            .filter_map(|f| f.file_name())
            .filter_map(|n| files::run_number_in_name(&n.to_string_lossy()))
            .collect()
    }

    /// `29905, 29906, 29907` — the open beams as a short run list.
    fn ob_runs_text(folders: &[PathBuf]) -> String {
        let runs = Self::ob_run_numbers(folders);
        if runs.is_empty() {
            return folders
                .iter()
                .filter_map(|f| f.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(", ");
        }
        runs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ")
    }

    /// The folders of a list of open beams, one per line (for hovers).
    fn ob_folders_text(folders: &[PathBuf]) -> String {
        folders
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Is a listed open-beam run one of the configuration's open beams
    /// (by run number — the configuration names corrected folders, the
    /// same the table finds)?
    fn ob_in_use(&self, run: u64) -> bool {
        Self::ob_run_numbers(&self.config_obs).contains(&run)
    }

    /// The open-beam runs of the table that landed AFTER the
    /// configuration's open beams (newest run number among them; every
    /// open-beam run when the configuration names none) and are not
    /// rejected — candidates to replace the configuration's. Ascending,
    /// with the corrected folder of each (complete or not).
    fn new_open_beams(&self) -> Vec<(u64, files::FileStatus)> {
        let newest_in_use = Self::ob_run_numbers(&self.config_obs)
            .into_iter()
            .max()
            .unwrap_or(0);
        self.run_files
            .iter()
            .filter(|rf| rf.run > newest_in_use)
            .filter(|rf| self.is_open_beam(rf.run) && !self.rejected.contains(&rf.run))
            .map(|rf| (rf.run, rf.corrected.clone()))
            .collect()
    }

    /// Make `folders` (complete corrected open-beam folders) the open
    /// beams of every normalization to come: a copy of the selected
    /// configuration with those folders as its open beams is written next
    /// to it (`<name>_ob_<run>_<run>.h5`, everything else kept) and
    /// selected — auto normalization ON re-registers it in the shared
    /// autoreduction.cfg on the next pass, so the autoreduction follows
    /// too. Results already there are never redone.
    fn use_as_open_beams(&mut self, folders: Vec<PathBuf>) {
        let Some(config) = self.selected_config.clone() else {
            self.ob_error = Some("no configuration file selected (section 2)".to_owned());
            return;
        };
        if folders.is_empty() {
            self.ob_error = Some("no open-beam run to use".to_owned());
            return;
        }
        for folder in &folders {
            if !files::folder_complete(folder, files::FolderKind::Corrected) {
                self.ob_error = Some(format!(
                    "the corrected open-beam folder is not complete: {}",
                    folder.display()
                ));
                return;
            }
        }
        let runs = Self::ob_run_numbers(&folders);
        if runs.is_empty() {
            self.ob_error = Some("no run number in the open-beam folder names".to_owned());
            return;
        }
        let run_spec = runs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
        // `<stem>_ob_<runs>.h5` next to the selected file; a previous
        // `_ob_…` suffix (the selected file being itself derived) is
        // dropped rather than chained.
        let stem = config
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "normalization_config".to_owned());
        let base = Self::strip_ob_suffix(&stem);
        let suffix = runs.iter().map(u64::to_string).collect::<Vec<_>>().join("_");
        let dst = config.with_file_name(format!("{base}_ob_{suffix}.h5"));
        match h5::write_config_with_obs(&config, &dst, &folders, &run_spec) {
            Ok(()) => {
                self.ob_error = None;
                self.rescan_configs();
                self.selected_config = Some(dst);
                self.keep_selected_config();
                self.preview_error = None;
                // The new selection takes effect at once (open beams read,
                // registration when ON, table refreshed).
                self.check_runs();
            }
            Err(e) => self.ob_error = Some(e),
        }
    }

    /// `name_ob_29962_29963` → `name`: drop a trailing `_ob_<run>[_<run>…]`.
    fn strip_ob_suffix(stem: &str) -> String {
        let mut base = stem;
        let mut numbers = 0;
        while let Some((head, tail)) = base.rsplit_once('_') {
            if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
                base = head;
                numbers += 1;
            } else if tail == "ob" && numbers > 0 {
                return head.to_owned();
            } else {
                break;
            }
        }
        stem.to_owned()
    }

    /// Read (once) where the DAQ filed a run's images, from its NeXus —
    /// nothing is stored when the file is missing or still being written,
    /// so the next refresh retries.
    fn classify_run(&mut self, ipts_path: &Path, run: u64) {
        if self.image_path_cache.contains_key(&run) {
            return;
        }
        if let Some(path) = h5::nexus_image_path(&files::nexus_path(ipts_path, run)) {
            self.image_path_cache.insert(run, path);
        }
    }

    /// Did the user opt the rolling combine & compare windows into the
    /// auto normalization (shared `rolling_combine` flag, false when the
    /// key is absent)?
    fn rolling_enabled(&self) -> bool {
        self.cfg.as_ref().map(|c| c.rolling_combine).unwrap_or(false)
    }

    /// Did the user opt the live reduction — every upcoming run normalized
    /// on its own from here — into the auto normalization (shared
    /// `live_reduction` flag, false when the key is absent)?
    fn live_enabled(&self) -> bool {
        self.cfg.as_ref().map(|c| c.live_reduction).unwrap_or(false)
    }

    /// Opt the rolling windows in/out of the auto normalization: written to
    /// the shared config so it survives restarts and is visible to every
    /// user (the line is added when the notebook-written file lacks it).
    fn set_rolling_enabled(&mut self, enabled: bool) {
        let live = self.live_enabled();
        self.set_opt_in(enabled, live, |path| config::set_rolling_combine(path, enabled));
    }

    /// Opt the live reduction in/out of the auto normalization (same
    /// shared-config mechanics as the rolling windows).
    fn set_live_enabled(&mut self, enabled: bool) {
        let rolling = self.rolling_enabled();
        self.set_opt_in(rolling, enabled, |path| config::set_live_reduction(path, enabled));
    }

    /// Write one opt-in flag: `set` toggles its line in the existing shared
    /// file; when there is no shared file yet (auto normalization never
    /// turned ON) the file is created, OFF, with the selected
    /// IPTS/configuration if any and both flags as they should now read
    /// (`rolling`, `live`).
    fn set_opt_in(
        &mut self,
        rolling: bool,
        live: bool,
        set: impl FnOnce(&Path) -> Result<(), String>,
    ) {
        let result = if self.cfg_path.is_file() {
            set(&self.cfg_path)
        } else {
            config::write_full(
                &self.cfg_path,
                self.ipts.as_deref().unwrap_or(""),
                &self
                    .selected_config
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default(),
                false,
                rolling,
                live,
            )
        };
        match result {
            Ok(()) => self.write_error = None,
            Err(e) => self.write_error = Some(e),
        }
        self.refresh();
    }

    /// Turn auto-normalization ON: register the selected IPTS +
    /// configuration file in the shared config and set the flag.
    fn turn_on(&mut self) {
        let (Some(ipts), Some(config_file)) = (self.ipts.clone(), self.selected_config.clone())
        else {
            return;
        };
        // The opt-ins are the user's own choices: keep them as they are.
        let (rolling, live) = (self.rolling_enabled(), self.live_enabled());
        match config::write_full(
            &self.cfg_path,
            &ipts,
            &config_file.display().to_string(),
            true,
            rolling,
            live,
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
                    .on_hover_text("Rescan shared/autoreduce (and its configs/ subfolder)")
                    .clicked()
                {
                    self.rescan_configs();
                }
                if ui
                    .button("📂 Browse…")
                    .on_hover_text(format!(
                        "Pick a configuration file anywhere on disk\n(opens in {})",
                        self.ipts_path()
                            .map(|p| p.join("shared").display().to_string())
                            .unwrap_or_default()
                    ))
                    .clicked()
                {
                    self.browse_config();
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
            // The folder-scan warning only matters while nothing is selected:
            // a file picked with Browse… (or registered in autoreduction.cfg)
            // works fine without the configs folder.
            match (
                &self.configs_error,
                self.configs.is_empty(),
                self.selected_config.is_some(),
            ) {
                (Some(_), _, true) => {}
                (Some(e), _, false) => {
                    ui.label(egui::RichText::new(e.as_str()).color(theme::WARNING));
                }
                (None, true, _) => {
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
                    let windows = match (cfg.live_reduction, cfg.rolling_combine) {
                        (true, true) => " — live reduction + rolling combine & compare windows",
                        (true, false) => " — live reduction only",
                        (false, true) => " — rolling combine & compare windows only",
                        (false, false) => {
                            " — nothing fires from here: check 4. and/or 5. below"
                        }
                    };
                    ui.label(
                        egui::RichText::new(format!(
                            "Active on {reg_ipts} with {}{windows}",
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

    /// Section 4 — the rolling combine-normalization windows. The heading
    /// line carries the opt-in box (shared `rolling_combine` flag) that
    /// makes them part of the auto normalization; unchecked, the whole
    /// section is folded away. Checked, the frame shows the editable
    /// durations, the runs currently inside each window, job status, and
    /// view/compare buttons, laid out as an aligned grid; the jobs fire on
    /// every new NeXus (live / hybrid mode) or by hand.
    fn windows_section(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let mut opted_in = self.rolling_enabled();
            let response = ui
                .checkbox(
                    &mut opted_in,
                    theme::section_heading("4. Rolling combine & compare (NeuNorm)"),
                )
                .on_hover_text(
                    "Opt-in, saved in the shared configuration: when checked, the \
                     auto normalization also combines the runs of each window \
                     through NeuNorm every time a new NeXus shows up. Unchecked \
                     (the default for everybody), the auto normalization only \
                     normalizes each run on its own and this section is hidden.",
                );
            if response.changed() {
                self.set_rolling_enabled(opted_in);
            }
            if !self.rolling_enabled() {
                return;
            }
            // Same view switch as section 5: the windows grid, or the
            // acquisition timeline with the window coverage bands.
            ui.add_space(theme::SPACE_MD);
            for (view, label) in [
                (RunsView::Table, "☰ Windows"),
                (RunsView::Timeline, "📈 Timeline"),
            ] {
                if ui.selectable_label(self.windows_view == view, label).clicked() {
                    self.windows_view = view;
                }
            }
        });
        if !self.rolling_enabled() {
            return;
        }
        ui.add_space(theme::SPACE_XS);
        if self.windows_view == RunsView::Timeline {
            self.runs_timeline(ui, "windows");
            return;
        }
        theme::section_frame(ui, |ui| {
            let live = self.runs.is_empty();
            let active = self.is_active();
            ui.label(
                egui::RichText::new(match (live, active) {
                    (true, _) => {
                        "Live: the windows follow the latest run of the IPTS, and \
                         the normalizations fire when a new NeXus shows up (auto \
                         normalization ON + configuration selected)."
                    }
                    (false, true) => {
                        "Hybrid: the windows look at the listed runs, new runs \
                         join the list as they land, and the normalizations fire \
                         on each new NeXus."
                    }
                    (false, false) => {
                        "The windows look at the listed runs only — launch by \
                         hand (turn auto normalization ON to have new runs join \
                         the list)."
                    }
                })
                .color(theme::text_emphasis(ui.visuals())),
            );
            ui.add_space(theme::SPACE_SM);

            let mut minutes_changed = false;
            let mut view_folder: Option<PathBuf> = None;
            let mut open_log: Option<PathBuf> = None;
            let mut output_toggle: Option<norm::JobTarget> = None;
            egui::Grid::new("windows_grid")
                .num_columns(4)
                .spacing([theme::SPACE_LG * 2.0, theme::SPACE_SM])
                .show(ui, |ui| {
                    ui.label(theme::section_heading("Window"));
                    ui.label(theme::section_heading("Runs"));
                    ui.label(theme::section_heading("Status"));
                    ui.label(theme::section_heading("Result"));
                    ui.end_row();
                    for (i, w) in self.windows.iter_mut().enumerate() {
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
                            norm::JobState::Running { runs, stage, fraction, stages } => {
                                ui.horizontal(|ui| {
                                    Self::progress_cell(
                                        ui,
                                        &format!("normalizing {} run(s)", runs.len()),
                                        stage,
                                        *fraction,
                                        stages,
                                        140.0,
                                    );
                                });
                                Self::output_toggle(
                                    ui,
                                    norm::JobTarget::Window(i),
                                    &mut output_toggle,
                                );
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
                                ui.horizontal(|ui| {
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
                                    Self::output_toggle(
                                        ui,
                                        norm::JobTarget::Window(i),
                                        &mut output_toggle,
                                    );
                                });
                            }
                            norm::JobState::Failed { message, log } => {
                                ui.label(
                                    egui::RichText::new("✖ failed")
                                        .color(theme::DANGER)
                                        .strong(),
                                )
                                .on_hover_text(match log {
                                    Some(log) => {
                                        format!("{message}\n\nfull log: {}", log.display())
                                    }
                                    None => message.clone(),
                                });
                                ui.horizontal(|ui| {
                                    if let Some(log) = log {
                                        if ui
                                            .button("📄 log")
                                            .on_hover_text(format!(
                                                "Open the job log\n{}",
                                                log.display()
                                            ))
                                            .clicked()
                                        {
                                            open_log = Some(log.clone());
                                        }
                                    }
                                    Self::output_toggle(
                                        ui,
                                        norm::JobTarget::Window(i),
                                        &mut output_toggle,
                                    );
                                });
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
            if let Some(log) = open_log {
                self.viewer_error = None;
                self.open_folder(&log);
            }
            if let Some(target) = output_toggle {
                self.toggle_output_view(target);
            }
            if matches!(self.output_view, Some(norm::JobTarget::Window(_))) {
                self.output_panel(ui);
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

    /// One ✔/✖ cell of the runs table, with the full path on hover.
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
            files::FileStatus::Writing(path) => {
                ui.label(
                    egui::RichText::new("⏳")
                        .color(theme::WARNING)
                        .size(16.0),
                )
                .on_hover_text(format!(
                    "being written — the folder is there but not complete yet: the \
                     Spectra.txt rows must match the frames (raw: .fits minus the \
                     SummedImg; corrected: .tif, plus summary.json)\n{}",
                    path.display()
                ));
            }
            files::FileStatus::Missing(path) => {
                ui.label(
                    egui::RichText::new("✖")
                        .color(theme::text_emphasis(ui.visuals()))
                        .size(16.0),
                )
                .on_hover_text(format!("not there yet — expected at\n{}", path.display()));
            }
        }
    }

    /// Timeline view of sections 4 and 5 (`id` keeps the two apart when
    /// both are open): one horizontal bar per run (acquisition
    /// start → end, from the NeXus times) and, on top, the coverage of the
    /// three rolling windows — all on a shared time axis in minutes
    /// relative to the anchor (the newest non-rejected run).
    /// egui_plot draws hover text left-anchored at the pointer tip, where
    /// the arrow cursor covers the first characters — indent every line so
    /// the text starts clear of the cursor.
    fn hover_indent(text: impl AsRef<str>) -> String {
        text.as_ref()
            .lines()
            .map(|l| format!("      {l}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn runs_timeline(&mut self, ui: &mut egui::Ui, id: &str) {
        use egui_plot::{Bar, BarChart, Plot, PlotPoint, Text, VLine};
        theme::section_frame(ui, |ui| {
            // (run, start, end, out) in table order (ascending runs); `out`
            // names why a run is not in the windows (rejected, alignment,
            // open beam).
            let bars_data: Vec<_> = self
                .run_files
                .iter()
                .filter_map(|rf| {
                    self.time_cache.get(&rf.run).map(|(start, end)| {
                        let out = if self.rejected.contains(&rf.run) {
                            Some("rejected")
                        } else {
                            self.kind_label(rf.run)
                        };
                        (rf.run, *start, *end, out)
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
            // Which sample-environment PV (NeXus DASlogs) to overlay on a
            // second y-axis, on the right of the plot.
            ui.horizontal(|ui| {
                ui.label("Metadata overlay:");
                let current = self.timeline_pv.map_or_else(
                    || "none".to_owned(),
                    |i| format!("{}{}", TIMELINE_PVS[i].0, TIMELINE_PVS[i].1),
                );
                egui::ComboBox::from_id_salt(("timeline_pv", id))
                    .selected_text(current)
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.timeline_pv, None, "none");
                        for (i, (name, extra)) in TIMELINE_PVS.iter().enumerate() {
                            ui.selectable_value(
                                &mut self.timeline_pv,
                                Some(i),
                                format!("{name}{extra}"),
                            );
                        }
                    });
            });
            ui.add_space(theme::SPACE_XS);
            // Same anchor as the windows: newest end among non-rejected runs.
            let anchor = bars_data
                .iter()
                .filter(|(_, _, _, out)| out.is_none())
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
            for (i, (run, start, end, out)) in bars_data.iter().enumerate() {
                let (s, e) = (to_min(start), to_min(end));
                // A visible sliver even for very short acquisitions.
                let value = (e - s).max(-span_min * 0.004);
                run_bars.push(
                    Bar::new(i as f64, value)
                        .base_offset(s)
                        .width(0.6)
                        .fill(if out.is_some() {
                            egui::Color32::from_gray(if dark { 110 } else { 150 })
                        } else {
                            theme::PRIMARY
                        })
                        .name(format!(
                            // En dash, not "→": the arrow glyph is missing
                            // from egui's default font (renders as a box).
                            "run {run}{}\n{} – {}  ({:.1} min)",
                            out.map(|why| format!(" ({why})")).unwrap_or_default(),
                            start.format("%H:%M:%S"),
                            end.format("%H:%M:%S"),
                            e - s
                        )),
                );
                let mut label = egui::RichText::new(run.to_string()).size(11.0);
                if *out == Some("rejected") {
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

            // Selected PV: (time, value) points of every run's DASlog, as
            // one polyline per run (no line across the gaps between runs).
            // The values live on their own scale, drawn inside the run-bar
            // band [-0.5, n-0.5] and read back on the right-hand axis.
            let band = (-0.5_f64, n as f64 - 0.5);
            let mut pv_series: Vec<Vec<[f64; 2]>> = Vec::new();
            let mut pv_scale: Option<(f64, f64)> = None;
            let mut pv_label = String::new();
            if let Some(idx) = self.timeline_pv {
                let (pv_name, extra) = TIMELINE_PVS[idx];
                pv_label = format!(
                    "{}{extra}",
                    pv_name.trim_start_matches("BL10:SE:ND2:")
                );
                if let Some(ipts_path) = self.ipts_path() {
                    for (run, ..) in &bars_data {
                        let points = self
                            .pv_cache
                            .entry((*run, pv_name))
                            .or_insert_with(|| {
                                h5::daslog(&files::nexus_path(&ipts_path, *run), pv_name)
                                    .unwrap_or_default()
                            });
                        let series: Vec<[f64; 2]> = points
                            .iter()
                            .filter(|(_, v)| v.is_finite())
                            .map(|(t, v)| [to_min(t), *v])
                            .collect();
                        if !series.is_empty() {
                            pv_series.push(series);
                        }
                    }
                }
                let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
                for p in pv_series.iter().flatten() {
                    lo = lo.min(p[1]);
                    hi = hi.max(p[1]);
                }
                if lo.is_finite() {
                    if hi - lo < 1e-9 {
                        // Constant log — pad so the flat line sits mid-band.
                        lo -= 1.0;
                        hi += 1.0;
                    }
                    pv_scale = Some((lo, hi));
                }
            }
            let mut y_axes =
                vec![egui_plot::AxisHints::new_y().formatter(|_, _| String::new())];
            if let Some((lo, hi)) = pv_scale {
                let (b0, b1) = band;
                y_axes.push(
                    egui_plot::AxisHints::new_y()
                        .placement(egui_plot::HPlacement::Right)
                        .label(pv_label.clone())
                        .formatter(move |mark, _| {
                            if (b0..=b1).contains(&mark.value) {
                                let v = lo + (mark.value - b0) / (b1 - b0) * (hi - lo);
                                egui::emath::format_with_decimals_in_range(v, 0..=2)
                            } else {
                                String::new()
                            }
                        }),
                );
            }

            let height = ((n + 5) as f32 * 24.0).clamp(200.0, 440.0);
            let anchor_for_cursor = anchor;
            let hover_pv = pv_scale.map(|scale| (pv_label.clone(), scale, band));
            let response = Plot::new(("acq_timeline", id))
                .height(height)
                .allow_scroll(false)
                .custom_y_axes(y_axes)
                .x_axis_label("time [minutes] relative to the latest run")
                .include_x(span_min * 1.12)
                .include_x(-span_min * 0.03)
                .include_y(-0.7)
                .include_y(n as f64 + 3.9)
                .label_formatter(move |name, point| {
                    // Hovering the PV curve: map the plot y back to the
                    // PV's own scale so the real value is shown.
                    if let Some((label, (lo, hi), (b0, b1))) =
                        hover_pv.as_ref().filter(|(label, ..)| name == label)
                    {
                        let v = lo + (point.y - b0) / (b1 - b0) * (hi - lo);
                        let at = anchor_for_cursor
                            + chrono::Duration::milliseconds((point.x * 60_000.0) as i64);
                        return Self::hover_indent(format!(
                            "{label}: {v:.2}\nat {:.1} min  ({})",
                            point.x,
                            at.format("%H:%M:%S")
                        ));
                    }
                    if name.is_empty() {
                        let at = anchor_for_cursor
                            + chrono::Duration::milliseconds((point.x * 60_000.0) as i64);
                        Self::hover_indent(format!(
                            "{:.1} min  ({})",
                            point.x,
                            at.format("%H:%M:%S")
                        ))
                    } else {
                        Self::hover_indent(name)
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
                            .element_formatter(Box::new(|bar, _| {
                                Self::hover_indent(&bar.name)
                            })),
                    );
                    plot_ui.bar_chart(
                        BarChart::new(run_bars)
                            .horizontal()
                            .element_formatter(Box::new(|bar, _| {
                                Self::hover_indent(&bar.name)
                            })),
                    );
                    for text in texts {
                        plot_ui.text(text);
                    }
                    // The PV curves, values mapped into the run-bar band
                    // (their true scale is the right-hand axis). Markers on
                    // top so single-point logs of short runs stay visible.
                    if let Some((lo, hi)) = pv_scale {
                        let (b0, b1) = band;
                        let to_y =
                            move |v: f64| b0 + (v - lo) / (hi - lo) * (b1 - b0);
                        for series in &pv_series {
                            plot_ui.line(
                                egui_plot::Line::new(
                                    series
                                        .iter()
                                        .map(|p| [p[0], to_y(p[1])])
                                        .collect::<Vec<_>>(),
                                )
                                .color(theme::DANGER)
                                .width(1.8)
                                .name(&pv_label),
                            );
                            plot_ui.points(
                                egui_plot::Points::new(
                                    series
                                        .iter()
                                        .map(|p| [p[0], to_y(p[1])])
                                        .collect::<Vec<_>>(),
                                )
                                .radius(2.5)
                                .color(theme::DANGER)
                                .name(&pv_label),
                            );
                        }
                    }
                });
            // A thin crosshair instead of the arrow, which sat right on top
            // of the hover text next to the data points.
            if response.response.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
            }
            if self.timeline_pv.is_some() && pv_scale.is_none() {
                ui.label(
                    egui::RichText::new(format!(
                        "No {pv_label} values in the NeXus files of these runs"
                    ))
                    .color(theme::WARNING)
                    .small(),
                );
            }
            ui.label(
                egui::RichText::new(
                    "X axis: minutes relative to the latest (non-rejected) run — \
                     hover a bar for the run's start/end and duration. The red \
                     curve (if a metadata PV is selected) reads on the right-hand \
                     axis.",
                )
                .color(theme::text_emphasis(ui.visuals()))
                .small(),
            );
        });
    }

    /// Section 5 — the live reduction: the runs in use (the list, or the
    /// widest window in live mode), file status per run, a preview of the
    /// corrected data in the TIFF viewer, and a reject/restore toggle —
    /// rejected runs stay listed, crossed out, but leave the windows and
    /// their normalizations. The upcoming run tops the table when
    /// auto-normalization is ON. Like section 4, the heading line carries
    /// the opt-in box (shared `live_reduction` flag) that makes the per-run
    /// normalization of every upcoming run part of the auto normalization;
    /// unchecked, the whole section is folded away.
    fn runs_table(&mut self, ui: &mut egui::Ui) {
        let rejected_count = self
            .run_files
            .iter()
            .filter(|r| self.rejected.contains(&r.run))
            .count();
        // Nothing to show under the heading (opted out, or no run yet).
        let folded =
            !self.live_enabled() || (self.run_files.is_empty() && self.next_run.is_none());
        let heading = if folded {
            "5. Live reduction".to_owned()
        } else if self.run_files.is_empty() {
            "5. Live reduction — next auto-normalized run".to_owned()
        } else if rejected_count > 0 {
            format!(
                "5. Live reduction ({} runs, {rejected_count} rejected)",
                self.run_files.len() - rejected_count
            )
        } else {
            format!("5. Live reduction ({} runs)", self.run_files.len())
        };
        // Heading (opt-in box) + view switch (table / acquisition timeline).
        ui.horizontal(|ui| {
            let mut opted_in = self.live_enabled();
            let response = ui
                .checkbox(&mut opted_in, theme::section_heading(&heading))
                .on_hover_text(
                    "Opt-in, saved in the shared configuration: when checked, the \
                     auto normalization normalizes every upcoming run on its own \
                     from here, as soon as its corrected data is complete (auto \
                     normalization ON + configuration selected). Unchecked (the \
                     default for everybody), no run is normalized one by one from \
                     this app and this section is hidden.",
                );
            if response.changed() {
                self.set_live_enabled(opted_in);
            }
            if folded {
                return;
            }
            ui.add_space(theme::SPACE_MD);
            for (view, label) in [
                (RunsView::Table, "☰ Table"),
                (RunsView::Timeline, "📈 Timeline"),
            ] {
                if ui.selectable_label(self.runs_view == view, label).clicked() {
                    self.runs_view = view;
                }
            }
            // Every listed run with complete corrected data and no result
            // yet, normalized `parallel_jobs` at a time.
            let missing: Vec<u64> = self
                .run_files
                .iter()
                .filter(|rf| matches!(rf.corrected, files::FileStatus::Present(_)))
                .filter(|rf| !self.rejected.contains(&rf.run) && !self.skips_normalization(rf.run))
                .filter(|rf| {
                    !matches!(
                        self.run_jobs.get(&rf.run),
                        Some(norm::JobState::Done { .. } | norm::JobState::Running { .. })
                    )
                })
                .map(|rf| rf.run)
                .rev()
                .collect();
            ui.add_space(theme::SPACE_MD);
            if !self.run_queue.is_empty() {
                ui.label(
                    egui::RichText::new(format!("{} run(s) queued", self.run_queue.len()))
                        .color(theme::INFO),
                );
                if ui.button("✖ clear queue").clicked() {
                    self.run_queue.clear();
                }
            } else {
                let enabled = missing
                    .iter()
                    .any(|run| self.config_for_run(*run).is_some());
                let button = ui.add_enabled(
                    enabled,
                    egui::Button::new(format!("▶ normalize all missing ({})", missing.len())),
                );
                let button = if enabled {
                    button.on_hover_text(format!(
                        "Normalize every listed run that has its corrected data and no \
                         result yet — newest first, up to {} at a time",
                        self.parallel_jobs
                    ))
                } else {
                    button.on_disabled_hover_text(
                        "Nothing to do: every listed run is normalized, running, or \
                         waiting for its corrected data (or no configuration is selected)",
                    )
                };
                if button.clicked() {
                    self.run_queue.extend(missing);
                }
            }
            ui.add_space(theme::SPACE_MD);
            ui.add(
                egui::DragValue::new(&mut self.parallel_jobs)
                    .range(1..=MAX_PARALLEL_JOBS)
                    .speed(0.1),
            )
            .on_hover_text(
                "How many per-run normalizations may run at the same time — a hard \
                 cap: automatic normalization, ▶ normalize all missing, and manual \
                 starts (▶ normalize, ↻ re-run) alike; the rest waits in the queue. \
                 Each one is a NeuNorm python process holding the sample and \
                 open-beam image stacks in memory (tens of GB); too many at once \
                 and the system kills them (SIGKILL). 4 is a safe default on the \
                 analysis machines.",
            );
            let running = self.running_run_jobs();
            ui.label(if running == 0 {
                "parallel jobs".to_owned()
            } else {
                format!("parallel jobs ({running} running)")
            });
        });
        if folded {
            return;
        }
        ui.add_space(theme::SPACE_XS);
        self.new_open_beams_banner(ui);
        if self.runs_view == RunsView::Timeline {
            self.runs_timeline(ui, "runs");
            return;
        }
        let mut toggle_run: Option<u64> = None;
        let mut preview_folder: Option<PathBuf> = None;
        let mut view_normalized: Option<PathBuf> = None;
        let mut open_normalized: Option<PathBuf> = None;
        let mut retry_run: Option<u64> = None;
        let mut rerun: Option<u64> = None;
        let running_jobs = self.running_run_jobs();
        let mut edit_run: Option<(u64, EditorMode)> = None;
        let mut output_toggle: Option<norm::JobTarget> = None;
        let active = self.is_active();
        let watching_from = self.run_jobs_from;
        // (run, file picked in its Config drop-down — None = section 2's)
        let mut change_config: Option<(u64, Option<PathBuf>)> = None;
        theme::section_frame(ui, |ui| {
            egui::ScrollArea::both()
                .id_salt("runs_table")
                .show(ui, |ui| {
                    egui::Grid::new("runs_grid")
                        .num_columns(11)
                        .striped(true)
                        .spacing([theme::SPACE_LG * 2.5, theme::SPACE_XS])
                        .show(ui, |ui| {
                            ui.label(theme::section_heading("Run"));
                            ui.label(theme::section_heading("NeXus"));
                            ui.label(theme::section_heading("Raw"));
                            ui.label(theme::section_heading("Corrected"));
                            ui.label(theme::section_heading("Preview"));
                            ui.label(theme::section_heading("Normalized"))
                                .on_hover_text(
                                    "Auto normalization ON: each new run is normalized on \
                                     its own (configuration's sample replaced by the run) \
                                     once its corrected data is there",
                                );
                            ui.label(theme::section_heading("Open beam")).on_hover_text(
                                "The open-beam run(s) each normalization divides by: \
                                 what the result used (from its job log) for a normalized \
                                 run, the configuration's open beams (dimmed) for a run \
                                 still to come. An open-beam run reads \"in use\" when it \
                                 is one of the configuration's, \"new\" when it landed \
                                 after them. ⇄ replace by… (next column) switches to \
                                 other open beams, for that run or for every upcoming run",
                            );
                            ui.label("");
                            ui.label(theme::section_heading("Config")).on_hover_text(
                                "The configuration file (open beams, settings, output \
                                 folder) each run is normalized with: the section 2 \
                                 selection unless another file of the IPTS is picked in \
                                 the row's drop-down (blue) — the run is then normalized, \
                                 and its result looked up, with that file. Hover a cell \
                                 for the file a normalized run actually ran with",
                            );
                            ui.label(theme::section_heading("Output")).on_hover_text(
                                "The output folder each result lands in \
                                 (<folder>/Run_<run>/normalization): the result's own \
                                 for a normalized run, the configuration's (dimmed) for \
                                 a run still to come. ✏ overrides it for one run",
                            );
                            ui.label(theme::section_heading("Use"))
                                .on_hover_text("✖ reject / ↩ restore for the windows");
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
                                self.planned_obs_cell(ui, Some(next.run));
                                ui.label("");
                                // A file picked here applies once the run lands.
                                self.run_config_cell(ui, next.run, &mut change_config);
                                self.planned_output_cell(ui, Some(next.run));
                                ui.label("");
                                ui.end_row();
                            }
                            // Newest first, right under the upcoming run.
                            for run in self.run_files.iter().rev() {
                                let rejected = self.rejected.contains(&run.run);
                                let kind = self.run_kind(run.run);
                                let alignment = kind == h5::RunKind::Alignment;
                                let open_beam = kind == h5::RunKind::OpenBeam;
                                let dim = theme::text_emphasis(ui.visuals());
                                let mut run_text =
                                    egui::RichText::new(run.run.to_string()).strong();
                                if rejected {
                                    run_text = run_text.strikethrough().color(dim);
                                }
                                let label = ui.label(run_text);
                                if rejected {
                                    label.on_hover_text(
                                        "Rejected — excluded from the windows",
                                    );
                                } else if alignment {
                                    label.on_hover_text(
                                        "Alignment run — no normalization needed",
                                    );
                                } else if open_beam {
                                    label.on_hover_text(
                                        "Open-beam run — a normalization input, not \
                                         normalized itself",
                                    );
                                }
                                Self::status_cell(ui, &run.nexus);
                                Self::status_cell(ui, &run.raw);
                                // Corrected: an alignment run is never
                                // corrected by the autoreduction — not
                                // checked (unless a folder is there anyway).
                                if alignment
                                    && !matches!(run.corrected, files::FileStatus::Present(_))
                                {
                                    ui.label(egui::RichText::new("—").color(dim)).on_hover_text(
                                        "Alignment run — the autoreduction does not correct \
                                         alignment runs: nothing to wait for",
                                    );
                                } else {
                                    Self::status_cell(ui, &run.corrected);
                                }
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
                                        ui.label(egui::RichText::new("—").color(dim))
                                            .on_hover_text(if alignment {
                                                "Alignment run — no corrected data to preview"
                                            } else {
                                                "No corrected data yet"
                                            });
                                    }
                                }
                                // Normalized: this run's own normalization.
                                let watched = active
                                    && watching_from.is_some_and(|from| run.run > from);
                                let no_config = self.config_for_run(run.run).is_none();
                                Self::normalized_cell(
                                    ui,
                                    run,
                                    self.run_jobs.get(&run.run),
                                    self.image_path_cache
                                        .get(&run.run)
                                        .filter(|_| alignment || open_beam)
                                        .map(|folder| (kind, folder)),
                                    watched && !rejected,
                                    no_config,
                                    self.run_queue
                                        .iter()
                                        .position(|r| *r == run.run)
                                        .map(|ahead| (ahead, running_jobs)),
                                    &mut view_normalized,
                                    &mut open_normalized,
                                    &mut retry_run,
                                    &mut rerun,
                                    &mut output_toggle,
                                );
                                // Open beam / Config / Output: what this
                                // run's normalization uses (or will), or
                                // the status of an open-beam run itself.
                                self.obs_cell(ui, run.run, kind);
                                if kind == h5::RunKind::Sample {
                                    let overridden = self
                                        .run_overrides
                                        .get(&run.run)
                                        .is_some_and(|o| o.obs.is_some());
                                    let text = if overridden {
                                        egui::RichText::new("⇄ replace by…").color(theme::INFO)
                                    } else {
                                        egui::RichText::new("⇄ replace by…")
                                    };
                                    if ui
                                        .button(text)
                                        .on_hover_text(
                                            "Replace the open beams of this run's \
                                             normalization by other open-beam run(s) of the \
                                             IPTS (1 or more) — for this run only, or for \
                                             this run and every upcoming run",
                                        )
                                        .clicked()
                                    {
                                        edit_run = Some((run.run, EditorMode::OpenBeams));
                                    }
                                } else {
                                    ui.label("");
                                }
                                if kind == h5::RunKind::Sample {
                                    self.run_config_cell(ui, run.run, &mut change_config);
                                } else {
                                    ui.label(egui::RichText::new("—").color(dim));
                                }
                                ui.horizontal(|ui| {
                                    self.output_cell(ui, run.run, kind);
                                    if kind == h5::RunKind::Sample {
                                        let overridden = self
                                            .run_overrides
                                            .get(&run.run)
                                            .is_some_and(|o| o.output.is_some());
                                        let pencil = if overridden {
                                            egui::RichText::new("✏").color(theme::INFO).strong()
                                        } else {
                                            egui::RichText::new("✏")
                                        };
                                        if ui
                                            .button(pencil)
                                            .on_hover_text(if overridden {
                                                "This run has its own output folder — edit it"
                                            } else {
                                                "Choose another output folder for this run's \
                                                 result (instead of the configuration's)"
                                            })
                                            .clicked()
                                        {
                                            edit_run = Some((run.run, EditorMode::Output));
                                        }
                                    }
                                });
                                // Reject / restore toggle — moot for an
                                // alignment run, never in the windows. An
                                // open-beam run is never in the windows
                                // either, but rejecting it keeps it out
                                // of the "new open beams" banner.
                                if alignment {
                                    ui.label(egui::RichText::new("—").color(dim)).on_hover_text(
                                        "Alignment run — never part of the windows",
                                    );
                                    ui.end_row();
                                    continue;
                                }
                                if open_beam {
                                    let (text, hover) = if rejected {
                                        ("↩ restore", "Offer this open beam again")
                                    } else {
                                        (
                                            "✖ reject",
                                            "Never offer this open-beam run as a \
                                             replacement open beam",
                                        )
                                    };
                                    if ui.button(text).on_hover_text(hover).clicked() {
                                        toggle_run = Some(run.run);
                                    }
                                    ui.end_row();
                                    continue;
                                }
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
        if let Some(folder) = view_normalized {
            self.viewer_error = None;
            self.open_in_viewer(std::slice::from_ref(&folder));
        }
        if let Some(folder) = open_normalized {
            self.viewer_error = None;
            self.open_folder(&folder);
        }
        if let Some(run) = retry_run {
            self.request_run(run);
        }
        if let Some(run) = rerun {
            self.request_run(run);
        }
        if let Some((run, config)) = change_config {
            self.set_run_config(run, config);
        }
        if let Some((run, mode)) = edit_run {
            self.open_run_editor(run, mode);
        }
        if let Some(target) = output_toggle {
            self.toggle_output_view(target);
        }
        self.run_editor_window(ui);
        if let Some(err) = &self.viewer_error {
            ui.label(
                egui::RichText::new(format!("Cannot open: {err}")).color(theme::DANGER),
            );
        }
        if matches!(self.output_view, Some(norm::JobTarget::Run(_))) {
            self.output_panel(ui);
        }
    }

    /// Banner above the table when open-beam run(s) landed after the
    /// configuration's open beams: names them and offers to make them the
    /// open beam(s) of every normalization to come (the ones whose
    /// corrected data is complete; the others are named as waiting).
    fn new_open_beams_banner(&mut self, ui: &mut egui::Ui) {
        let candidates = self.new_open_beams();
        if candidates.is_empty() && self.ob_error.is_none() {
            return;
        }
        let list = |runs: &[u64]| runs.iter().map(u64::to_string).collect::<Vec<_>>().join(", ");
        if !candidates.is_empty() {
            let ready: Vec<u64> = candidates
                .iter()
                .filter(|(_, status)| matches!(status, files::FileStatus::Present(_)))
                .map(|(run, _)| *run)
                .collect();
            let waiting: Vec<u64> = candidates
                .iter()
                .filter(|(_, status)| !matches!(status, files::FileStatus::Present(_)))
                .map(|(run, _)| *run)
                .collect();
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    egui::RichText::new(format!(
                        "🔆 New open beam run{}: {}",
                        if candidates.len() > 1 { "s" } else { "" },
                        list(&candidates.iter().map(|(r, _)| *r).collect::<Vec<_>>())
                    ))
                    .color(theme::WARNING)
                    .strong(),
                )
                .on_hover_text(format!(
                    "Open-beam run(s) acquired after the configuration's open beams \
                     ({}). The normalizations keep using the configuration's until \
                     you switch.",
                    if self.config_obs.is_empty() {
                        "the configuration names none".to_owned()
                    } else {
                        Self::ob_runs_text(&self.config_obs)
                    }
                ));
                ui.label(
                    egui::RichText::new(if ready.is_empty() {
                        format!(
                            "— ⏳ waiting for the corrected data of {} (autoreduction)",
                            list(&waiting)
                        )
                    } else if waiting.is_empty() {
                        "— ⇄ replace by… on a row switches to them, for that run or for \
                         every upcoming run"
                            .to_owned()
                    } else {
                        format!(
                            "— ⇄ replace by… on a row switches to them, for that run or for \
                             every upcoming run (⏳ {} still waiting for corrected data)",
                            list(&waiting)
                        )
                    })
                    .color(theme::text_emphasis(ui.visuals())),
                );
            });
        }
        if let Some(err) = &self.ob_error {
            ui.label(
                egui::RichText::new(format!("Cannot switch the open beams: {err}"))
                    .color(theme::DANGER),
            );
        }
        ui.add_space(theme::SPACE_XS);
    }

    /// "Open beam" cell of the upcoming run: the configuration's open
    /// beams, dimmed (what the run will divide by).
    fn planned_obs_cell(&self, ui: &mut egui::Ui, run: Option<u64>) {
        let dim = theme::text_emphasis(ui.visuals());
        let obs = match run {
            Some(run) => self.config_obs_for_run(run),
            None => self.config_obs.clone(),
        };
        if obs.is_empty() {
            ui.label(egui::RichText::new("—").color(dim)).on_hover_text(
                "The configuration names no open beam (or none is selected)",
            );
        } else {
            ui.label(egui::RichText::new(Self::ob_runs_text(&obs)).color(dim))
                .on_hover_text(format!(
                    "Will divide by the configuration's open beam(s)\n{}",
                    Self::ob_folders_text(&obs)
                ));
        }
    }

    /// "Open beam" cell of one row. Sample run: the open beams its
    /// normalization used (recorded at launch, or read from the job log
    /// of a result found on disk), else the configuration's, dimmed, as
    /// what it will use. Open-beam run: whether it is in use, new (landed
    /// after the configuration's open beams) or older. Alignment run: —.
    fn obs_cell(&self, ui: &mut egui::Ui, run: u64, kind: h5::RunKind) {
        let dim = theme::text_emphasis(ui.visuals());
        match kind {
            h5::RunKind::Alignment => {
                ui.label(egui::RichText::new("—").color(dim))
                    .on_hover_text("Alignment run — not normalized, no open beam involved");
            }
            h5::RunKind::OpenBeam => {
                if self.ob_in_use(run) {
                    ui.label(egui::RichText::new("✔ in use").color(theme::SUCCESS))
                        .on_hover_text(
                            "One of the configuration's open beams — every upcoming \
                             normalization divides by it",
                        );
                } else if self.rejected.contains(&run) {
                    ui.label(egui::RichText::new("rejected").color(dim))
                        .on_hover_text("Never offered as a replacement open beam (↩ restore to offer it again)");
                } else if self.new_open_beams().iter().any(|(r, _)| *r == run) {
                    ui.label(egui::RichText::new("new").color(theme::WARNING).strong())
                        .on_hover_text(
                            "Landed after the configuration's open beams — the banner \
                             above offers to switch to it",
                        );
                } else {
                    ui.label(egui::RichText::new("not in use").color(dim)).on_hover_text(
                        "An older open beam the configuration does not name",
                    );
                }
            }
            h5::RunKind::Sample => match self.run_meta.get(&run).map(|m| &m.obs) {
                Some(obs) if !obs.is_empty() => {
                    ui.label(Self::ob_runs_text(obs)).on_hover_text(format!(
                        "Open beam(s) this run's normalization divides by\n{}",
                        Self::ob_folders_text(obs)
                    ));
                }
                _ => {
                    let done = matches!(self.run_jobs.get(&run), Some(norm::JobState::Done { .. }));
                    if done {
                        ui.label(egui::RichText::new("?").color(dim)).on_hover_text(
                            "Normalized, but its job log names no open beam (result \
                             produced by another tool, or log missing)",
                        );
                    } else if let Some(obs) = self.run_overrides.get(&run).and_then(|o| o.obs.as_ref())
                    {
                        ui.label(egui::RichText::new(Self::ob_runs_text(obs)).color(theme::INFO))
                            .on_hover_text(format!(
                                "Will divide by the open beam(s) chosen for this run (⇄)\n{}",
                                Self::ob_folders_text(obs)
                            ));
                    } else {
                        self.planned_obs_cell(ui, Some(run));
                    }
                }
            },
        }
    }

    /// The file name of a path (the whole path when it has none).
    fn short_name(path: &Path) -> String {
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string())
    }

    /// "Output" cell of a run still to come: the base folder its result
    /// will land in — the row's own (✏, highlighted) or the
    /// configuration's (dimmed).
    fn planned_output_cell(&self, ui: &mut egui::Ui, run: Option<u64>) {
        let dim = theme::text_emphasis(ui.visuals());
        let over = run
            .and_then(|run| self.run_overrides.get(&run))
            .and_then(|o| o.output.clone());
        let base = match run {
            Some(run) => self.output_base_for_run(run),
            None => self.output_base.clone(),
        };
        match (over, &base) {
            (Some(folder), _) => {
                ui.label(egui::RichText::new(Self::short_name(&folder)).color(theme::INFO))
                    .on_hover_text(format!(
                        "Output folder chosen for this run (✏) — the result goes to \
                         Run_<run>/normalization in there\n{}",
                        folder.display()
                    ));
            }
            (None, Some(base)) => {
                ui.label(egui::RichText::new(Self::short_name(base)).color(dim))
                    .on_hover_text(format!(
                        "The configuration's output folder — the result goes to \
                         Run_<run>/normalization in there\n{}",
                        base.display()
                    ));
            }
            (None, None) => {
                ui.label(egui::RichText::new("—").color(dim))
                    .on_hover_text("Select a configuration file (section 2)");
            }
        }
    }

    /// "Config" cell of one sample row (and of the upcoming run): a
    /// drop-down of the configuration files of the IPTS (section 2's
    /// list), whose first entry follows the section 2 selection. The
    /// run's normalization runs with the file shown — a pick lands in
    /// `change_config` (applied once the table is drawn). A file picked
    /// for the row reads in blue; the hover also names the file a
    /// normalized run actually ran with (recorded at launch, or read from
    /// the job log of a result found on disk) when it differs.
    fn run_config_cell(
        &self,
        ui: &mut egui::Ui,
        run: u64,
        change_config: &mut Option<(u64, Option<PathBuf>)>,
    ) {
        let own = self.run_overrides.get(&run).and_then(|o| o.config.as_ref());
        let effective = self.config_for_run(run);
        let ran_with = self.run_meta.get(&run).and_then(|m| m.config.as_ref());
        let mut text = egui::RichText::new(match &effective {
            Some(path) => Self::short_name(path),
            None => "— none —".to_owned(),
        });
        if own.is_some() {
            text = text.color(theme::INFO);
        } else if effective.is_none() {
            text = text.color(theme::WARNING);
        } else if ran_with.is_none() {
            text = text.color(theme::text_emphasis(ui.visuals()));
        }
        let combo = egui::ComboBox::from_id_salt(("run_config", run))
            .selected_text(text)
            .width(280.0);
        let response = combo.show_ui(ui, |ui| {
            let default_name = match &self.selected_config {
                Some(path) => format!("(section 2) {}", Self::short_name(path)),
                None => "(section 2) — none selected —".to_owned(),
            };
            if ui
                .selectable_label(own.is_none(), default_name)
                .on_hover_text("Follow the configuration selected in section 2")
                .clicked()
            {
                *change_config = Some((run, None));
            }
            ui.separator();
            for cfg_file in &self.configs {
                let active = own == Some(&cfg_file.path);
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
                    *change_config = Some((run, Some(cfg_file.path.clone())));
                }
            }
        });
        let mut hover = match (&own, &effective) {
            (Some(path), _) => format!(
                "Configuration picked for this run instead of the section 2 \
                 selection — its open beams, settings and output folder (unless \
                 ⇄ / ✏ say otherwise)\n{}",
                path.display()
            ),
            (None, Some(path)) => format!("The section 2 selection\n{}", path.display()),
            (None, None) => "No configuration file — select one in section 2 or pick \
                             one here for this run"
                .to_owned(),
        };
        match ran_with {
            Some(path) if effective.as_ref() != Some(path) => {
                hover.push_str(&format!(
                    "\n\nThis run's normalization ran with another file (↻ re-run to \
                     use the one shown):\n{}",
                    path.display()
                ));
            }
            Some(_) => hover.push_str("\n\nThis run's normalization ran with this file"),
            None if matches!(self.run_jobs.get(&run), Some(norm::JobState::Done { .. })) => {
                hover.push_str(
                    "\n\nNormalized, but its job log names no configuration file \
                     (result produced by another tool, or log missing)",
                );
            }
            None => {}
        }
        response.response.on_hover_text(hover);
    }

    /// "Output" cell of one row: the base folder the run's result sits
    /// in (`<here>/Run_<run>/normalization`), else where it will go.
    fn output_cell(&self, ui: &mut egui::Ui, run: u64, kind: h5::RunKind) {
        let dim = theme::text_emphasis(ui.visuals());
        if kind != h5::RunKind::Sample {
            ui.label(egui::RichText::new("—").color(dim));
            return;
        }
        match self.run_meta.get(&run).and_then(|m| m.output.as_ref()) {
            Some(output) => {
                // `<base>/Run_<run>/normalization` → show `<base>`.
                let base = output
                    .parent()
                    .and_then(|row| row.parent())
                    .unwrap_or(output);
                let overridden = self
                    .run_overrides
                    .get(&run)
                    .is_some_and(|o| o.output.is_some());
                let text = egui::RichText::new(Self::short_name(base));
                let text = if overridden { text.color(theme::INFO) } else { text };
                ui.label(text).on_hover_text(format!(
                    "Result folder of this run's normalization{}\n{}",
                    if overridden { " (output folder chosen for this run, ✏)" } else { "" },
                    output.display()
                ));
            }
            None => self.planned_output_cell(ui, Some(run)),
        }
    }

    /// Footer: where the normalized data lands, with a shortcut to the
    /// folder. The per-run results go to `<output folder>/Run_<run>/
    /// normalization`, the output folder coming from the configuration.
    fn output_footer(&mut self, ui: &mut egui::Ui) {
        if self.ipts.is_none() {
            return;
        }
        let mut open: Option<PathBuf> = None;
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("💾 Normalized data lands in").strong());
            match &self.output_base {
                Some(base) => {
                    ui.label(
                        egui::RichText::new(base.display().to_string())
                            .monospace()
                            .color(theme::primary_text(ui.visuals())),
                    )
                    .on_hover_text(
                        "The configuration file's output folder — each run goes to \
                         Run_<run>/normalization in there (rolling windows: \
                         <IPTS>/shared/autoreduce/normalized/rolling)",
                    );
                    let exists = base.is_dir();
                    let button = ui.add_enabled(exists, egui::Button::new("📂 open folder"));
                    let button = if exists {
                        button.on_hover_text(format!(
                            "Jump to the output folder in the file manager\n{}",
                            base.display()
                        ))
                    } else {
                        button.on_disabled_hover_text(
                            "The folder does not exist yet — created by the first \
                             normalization",
                        )
                    };
                    if button.clicked() {
                        open = Some(base.clone());
                    }
                }
                None => {
                    ui.label(
                        egui::RichText::new(
                            "the configuration file's output folder — select a \
                             configuration (section 2) to see it",
                        )
                        .color(theme::text_emphasis(ui.visuals())),
                    );
                }
            }
        });
        if let Some(folder) = open {
            self.viewer_error = None;
            self.open_folder(&folder);
        }
    }

    fn toggle_output_view(&mut self, target: norm::JobTarget) {
        self.output_view = if self.output_view == Some(target) {
            None
        } else {
            Some(target)
        };
    }

    /// Overall progress of a job: the bar spans the whole workflow ("stage
    /// k/n", stages weighted equally) once the script announced its stages,
    /// the current stage alone before that. Hover: every stage, the done
    /// ones ticked.
    fn progress_cell(
        ui: &mut egui::Ui,
        what: &str,
        stage: &str,
        fraction: Option<f32>,
        stages: &[String],
        width: f32,
    ) {
        let overall = norm::overall_progress(stages, stage, fraction);
        let bar = match (overall, fraction) {
            (Some((_, f)), _) => egui::ProgressBar::new(f)
                .desired_width(width)
                .desired_height(14.0)
                .corner_radius(3.0)
                .show_percentage(),
            (None, Some(f)) => egui::ProgressBar::new(f)
                .desired_width(width)
                .desired_height(14.0)
                .corner_radius(3.0)
                .show_percentage(),
            (None, None) => egui::ProgressBar::new(0.99)
                .desired_width(width)
                .desired_height(14.0)
                .corner_radius(3.0)
                .animate(true),
        };
        let mut hover = what.to_owned();
        if let Some(((k, n), _)) = overall {
            hover.push_str(&format!("\nstage {k}/{n}"));
            for (i, s) in stages.iter().enumerate() {
                let mark = if i + 1 < k {
                    "✔"
                } else if i + 1 == k {
                    "▶"
                } else {
                    "·"
                };
                hover.push_str(&format!("\n {mark} {s}"));
            }
        } else {
            hover.push_str(&format!("\n{stage}"));
        }
        ui.add(bar).on_hover_text(hover);
        let text = match overall {
            Some(((k, n), _)) => format!("{k}/{n} {stage}"),
            None => stage.to_owned(),
        };
        ui.label(egui::RichText::new(text).color(theme::INFO).small());
    }

    /// "📋" button toggling the output panel of one job.
    fn output_toggle(
        ui: &mut egui::Ui,
        target: norm::JobTarget,
        output_toggle: &mut Option<norm::JobTarget>,
    ) {
        if ui
            .button("📋")
            .on_hover_text("Show / hide the output of this normalization")
            .clicked()
        {
            *output_toggle = Some(target);
        }
    }

    /// The output of the job in `output_view`: its lines (script output and
    /// runner steps), newest at the bottom, in a scrolling monospace box.
    fn output_panel(&mut self, ui: &mut egui::Ui) {
        let Some(target) = self.output_view else {
            return;
        };
        let title = match target {
            norm::JobTarget::Run(run) => format!("Output — run {run}"),
            norm::JobTarget::Window(i) => match self.windows.get(i) {
                Some(w) => format!("Output — window last {} min", w.minutes),
                None => "Output".to_owned(),
            },
        };
        let state = match target {
            norm::JobTarget::Run(run) => self.run_jobs.get(&run),
            norm::JobTarget::Window(i) => self.windows.get(i).map(|w| &w.state),
        };
        let running = matches!(state, Some(norm::JobState::Running { .. }));
        // What to say when no line was streamed: a job refused before the
        // script even started only has its failure message; a result found
        // on disk was produced elsewhere.
        let (empty_note, empty_color) = match state {
            Some(norm::JobState::Running { .. }) => ("no output yet…".to_owned(), theme::INFO),
            Some(norm::JobState::Failed { message, log: None }) => (
                format!(
                    "The normalization was refused before NeuNorm started — nothing to \
                     stream. Reason:\n\n{message}"
                ),
                theme::DANGER,
            ),
            Some(norm::JobState::Failed { message, log: Some(log) }) => (
                format!("{message}\n\nfull log: {}", log.display()),
                theme::DANGER,
            ),
            Some(norm::JobState::Done { output, .. }) => (
                format!(
                    "Result found on disk — normalized in another session or by the \
                     workflow runner, so there is no output to show here.\n{}",
                    output.display()
                ),
                theme::text_emphasis(ui.visuals()),
            ),
            _ => (
                "no output kept for this job".to_owned(),
                theme::text_emphasis(ui.visuals()),
            ),
        };
        ui.add_space(theme::SPACE_XS);
        let mut close = false;
        theme::section_frame(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(theme::section_heading(&title));
                if running {
                    ui.spinner();
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("✖ close").clicked() {
                        close = true;
                    }
                });
            });
            let lines = self.job_output.get(&target);
            egui::ScrollArea::vertical()
                .id_salt(("job_output", format!("{target:?}")))
                .max_height(220.0)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.set_min_width(ui.available_width());
                    match lines {
                        Some(lines) if !lines.is_empty() => {
                            let text: String =
                                lines.iter().map(|l| l.as_str()).collect::<Vec<_>>().join("\n");
                            ui.add(
                                egui::Label::new(egui::RichText::new(text).monospace().small())
                                    .wrap(),
                            );
                        }
                        _ => {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&empty_note).color(empty_color),
                                )
                                .wrap(),
                            );
                        }
                    }
                });
        });
        if close {
            self.output_view = None;
        }
    }

    /// The "Normalized" cell of one table row: waiting marker (or a
    /// "▶ normalize" button for runs auto normalization does not cover),
    /// progress bar while NeuNorm runs, view/open icons when done, error +
    /// retry when failed. `watched` = auto normalization will (or did)
    /// pick this run. `special` = the kind and recorded image folder of a
    /// run that is never normalized (alignment, open beam). `queued` =
    /// (runs ahead of it, jobs running) when the run waits for a slot.
    fn normalized_cell(
        ui: &mut egui::Ui,
        run: &files::RunFiles,
        state: Option<&norm::JobState>,
        special: Option<(h5::RunKind, &String)>,
        watched: bool,
        no_config: bool,
        queued: Option<(usize, usize)>,
        view_normalized: &mut Option<PathBuf>,
        open_normalized: &mut Option<PathBuf>,
        retry_run: &mut Option<u64>,
        rerun: &mut Option<u64>,
        output_toggle: &mut Option<norm::JobTarget>,
    ) {
        let target = norm::JobTarget::Run(run.run);
        let dim = theme::text_emphasis(ui.visuals());
        // An alignment run (its NeXus files it under images/…/alignment)
        // or an open-beam run (images/…/ob): nothing to normalize — said
        // once its NeXus is read, unless a result exists anyway (then the
        // result is shown as usual).
        if let (Some((kind, folder)), None | Some(norm::JobState::Idle)) = (special, state) {
            let (text, hover) = match kind {
                h5::RunKind::OpenBeam => (
                    "open beam — no normalization needed",
                    "The NeXus records this run's images under an ob folder \
                     (BL10:Exp:IM:ImageFilePath log): an open-beam run is a \
                     normalization input, not something to normalize. Once its \
                     corrected data is complete, the banner above the table offers \
                     to make it the open beam of the upcoming runs",
                ),
                _ => (
                    "alignment run — no normalization needed",
                    "The NeXus records this run's images under an alignment folder \
                     (BL10:Exp:IM:ImageFilePath log): an alignment run is not corrected \
                     by the autoreduction and is not normalized — nothing to check or \
                     wait for",
                ),
            };
            ui.label(egui::RichText::new(text).color(dim))
                .on_hover_text(format!("{hover}\n{folder}"));
            return;
        }
        // Waiting for a free slot (every slot busy when it was asked for):
        // whatever its last state, the row says so — the job starts by
        // itself, in order.
        if let Some((ahead, running)) = queued {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("⏳ queued").color(theme::INFO))
                    .on_hover_text(format!(
                        "Waiting for a free slot: {running} normalization(s) running \
                         (the parallel jobs cap), {ahead} queued ahead of this run. \
                         It starts by itself; ✖ clear queue (above the table) \
                         cancels the wait"
                    ));
                Self::output_toggle(ui, target, output_toggle);
            });
            return;
        }
        match state {
            None | Some(norm::JobState::Idle) => {
                if watched {
                    let (text, hover, color) = match &run.corrected {
                        files::FileStatus::Present(_) if no_config => (
                            "⚠ no configuration",
                            "Corrected data found, but no configuration file is \
                             selected (section 2) — select one to normalize this run",
                            theme::WARNING,
                        ),
                        files::FileStatus::Present(_) => (
                            "⏳ starting…",
                            "Corrected data found — launching NeuNorm",
                            theme::INFO,
                        ),
                        files::FileStatus::Writing(_) => (
                            "⏳ waiting",
                            "The corrected data is still being written — normalizing \
                             this run as soon as the folder is complete",
                            theme::INFO,
                        ),
                        files::FileStatus::Missing(_) => (
                            "⏳ waiting",
                            "Waiting for the corrected data (autoreduction) before \
                             normalizing this run",
                            theme::INFO,
                        ),
                    };
                    ui.label(egui::RichText::new(text).color(color))
                        .on_hover_text(hover);
                } else {
                    // Not covered by auto normalization (older run, or
                    // auto normalization OFF): offer to do it now.
                    match &run.corrected {
                        files::FileStatus::Present(_) if no_config => {
                            ui.label(egui::RichText::new("—").color(dim)).on_hover_text(
                                "No normalized data found — select a configuration \
                                 file (section 2) to normalize this run",
                            );
                        }
                        files::FileStatus::Present(_) => {
                            if ui
                                .button("▶ normalize")
                                .on_hover_text(
                                    "No normalized data found in the configuration's \
                                     output folder — normalize this run now \
                                     (configuration's sample replaced by the run)",
                                )
                                .clicked()
                            {
                                *retry_run = Some(run.run);
                            }
                        }
                        files::FileStatus::Writing(_) => {
                            ui.label(egui::RichText::new("—").color(dim)).on_hover_text(
                                "Corrected data still being written — nothing to \
                                 normalize yet",
                            );
                        }
                        files::FileStatus::Missing(_) => {
                            ui.label(egui::RichText::new("—").color(dim))
                                .on_hover_text("No corrected data yet — nothing to normalize");
                        }
                    }
                }
            }
            Some(norm::JobState::Running { stage, fraction, stages, .. }) => {
                ui.horizontal(|ui| {
                    Self::progress_cell(
                        ui,
                        &format!("normalizing run {}", run.run),
                        stage,
                        *fraction,
                        stages,
                        110.0,
                    );
                    Self::output_toggle(ui, target, output_toggle);
                });
            }
            Some(norm::JobState::Done { output, finished, .. }) => {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("✔ normalization done")
                            .color(theme::SUCCESS)
                            .strong(),
                    )
                    .on_hover_text(format!(
                        "finished {}\n{}",
                        finished.format("%Y-%m-%d %H:%M:%S"),
                        output.display()
                    ));
                    if ui
                        .button("👁 view")
                        .on_hover_text(format!(
                            "Visualize the normalized data in the TIFF viewer\n{}",
                            output.display()
                        ))
                        .clicked()
                    {
                        *view_normalized = Some(output.clone());
                    }
                    if ui
                        .button("📂 folder")
                        .on_hover_text(format!(
                            "Jump to the normalized folder in the file manager\n{}",
                            output.display()
                        ))
                        .clicked()
                    {
                        *open_normalized = Some(output.clone());
                    }
                    if ui
                        .button("↻")
                        .on_hover_text(
                            "Re-run this normalization with the row's current settings \
                             (open beams: ⇄ replace by…; output folder: ✏). The \
                             previous result is kept as normalization.previous",
                        )
                        .clicked()
                    {
                        *rerun = Some(run.run);
                    }
                    Self::output_toggle(ui, target, output_toggle);
                });
            }
            Some(norm::JobState::Failed { message, log }) => {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new("✖ failed")
                            .color(theme::DANGER)
                            .strong(),
                    )
                    .on_hover_text(match log {
                        Some(log) => format!("{message}\n\nfull log: {}", log.display()),
                        None => message.clone(),
                    });
                    if let Some(log) = log {
                        if ui
                            .button("📄")
                            .on_hover_text(format!("Open the job log\n{}", log.display()))
                            .clicked()
                        {
                            *open_normalized = Some(log.clone());
                        }
                    }
                    if ui
                        .button("↻")
                        .on_hover_text("Retry the normalization of this run")
                        .clicked()
                    {
                        *retry_run = Some(run.run);
                    }
                    Self::output_toggle(ui, target, output_toggle);
                });
            }
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
                norm::JobMessage::Stages { target, stages } => {
                    if let Some(norm::JobState::Running { stages: s, .. }) =
                        self.job_state_mut(target)
                    {
                        *s = stages;
                    }
                }
                norm::JobMessage::Output { target, line } => {
                    let lines = self.job_output.entry(target).or_default();
                    lines.push_back(line);
                    while lines.len() > OUTPUT_LINES_KEPT {
                        lines.pop_front();
                    }
                }
                norm::JobMessage::Progress { target, stage, fraction } => {
                    if let Some(norm::JobState::Running {
                        stage: s,
                        fraction: f,
                        ..
                    }) = self.job_state_mut(target)
                    {
                        *s = stage;
                        *f = fraction;
                    }
                }
                norm::JobMessage::Finished { target, runs, result, log } => {
                    // A process killed by the system (memory pressure) is
                    // retried once, through the queue (it waits for a slot).
                    let retry = match (&target, &result) {
                        (norm::JobTarget::Run(run), Err(message))
                            if norm::killed_by_system(message)
                                && self.killed_retried.insert(*run) =>
                        {
                            Some(*run)
                        }
                        _ => None,
                    };
                    let succeeded = result.is_ok();
                    if let Some(state) = self.job_state_mut(target) {
                        *state = match result {
                            Ok(output) => norm::JobState::Done {
                                output,
                                finished: chrono::Local::now(),
                                runs,
                            },
                            Err(message) => norm::JobState::Failed {
                                message: if retry.is_some() {
                                    format!("{message}\n\n→ queued for one automatic retry")
                                } else {
                                    message
                                },
                                log: Some(log),
                            },
                        };
                    }
                    if let Some(run) = retry.filter(|run| !self.run_queue.contains(run)) {
                        self.run_queue.push_back(run);
                    } else if let (norm::JobTarget::Run(run), true) = (&target, succeeded) {
                        // A success clears the one-retry mark: a later
                        // re-run of this row gets its own retry again.
                        self.killed_retried.remove(run);
                    }
                }
            }
        }

        // Queued runs ("normalize all missing", manual starts asked for
        // while every slot was busy, automatic retries): fill the free
        // slots, up to `parallel_jobs` running at once. A queued run that
        // got started meanwhile (automatic pass) is simply dropped.
        while self.has_free_job_slot() {
            let Some(run) = self.run_queue.pop_front() else {
                break;
            };
            self.rerun_run(run);
        }

        // While jobs run, keep frames coming so the spinner moves and the
        // finished jobs are collected promptly.
        let windows_running = self
            .windows
            .iter()
            .any(|w| matches!(w.state, norm::JobState::Running { .. }));
        let runs_running = self
            .run_jobs
            .values()
            .any(|s| matches!(s, norm::JobState::Running { .. }));
        if windows_running || runs_running {
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
                    zoom::toggle_button(ui);
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
                    ui.add_space(theme::SPACE_LG);
                    self.output_footer(ui);
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
            install_fonts(&cc.egui_ctx);
            // Saved light/dark preference, shared by all the VENUS rust
            // tools (dark when none is saved); the controls bar has a toggle.
            cc.egui_ctx.set_theme(theme::load());
            cc.egui_ctx.set_zoom_factor(zoom::load());
            theme::apply(&cc.egui_ctx);
            Ok(Box::new(MonitorApp::new()))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ob_suffix_is_stripped_not_chained() {
        let strip = MonitorApp::strip_ob_suffix;
        assert_eq!(strip("normalization_config_20260910_102311_last"), "normalization_config_20260910_102311_last");
        assert_eq!(strip("normalization_config_20260910_102311_last_ob_29962"), "normalization_config_20260910_102311_last");
        assert_eq!(strip("normalization_config_20260910_102311_ob_29962_29963"), "normalization_config_20260910_102311");
        // A trailing number that is not an _ob_ suffix stays.
        assert_eq!(strip("normalization_config_20260910_102311"), "normalization_config_20260910_102311");
        assert_eq!(strip("my_ob"), "my_ob");
        assert_eq!(strip("config_ob_"), "config_ob_");
    }

    #[test]
    fn scans_the_corrected_open_beams_of_a_real_ipts() {
        let ipts = Path::new("/SNS/VENUS/IPTS-37705");
        if !ipts.join("shared/autoreduce/images/tpx1/ob").is_dir() {
            return;
        }
        let extra = vec![PathBuf::from("/SNS/VENUS/IPTS-1/shared/autoreduce/images/tpx1/ob/x/Run_5_ob_0")];
        let choices = MonitorApp::scan_ob_choices(ipts, &extra);
        // Newest first; 30051 is one of the open beams of that IPTS.
        let runs: Vec<Option<u64>> = choices.iter().map(|c| c.run).collect();
        assert!(runs.contains(&Some(30051)), "{runs:?}");
        assert!(runs.windows(2).all(|w| w[0] >= w[1]), "{runs:?}");
        let ob = choices.iter().find(|c| c.run == Some(30051)).unwrap();
        assert!(ob.complete);
        assert!(ob.frames > 0);
        assert!(ob.folder.starts_with("/SNS/VENUS/IPTS-37705/shared/autoreduce/images/tpx1/ob"));
        // A folder the configuration names elsewhere is offered too
        // (incomplete: it does not exist), and never twice.
        let missing = choices.iter().filter(|c| c.folder == extra[0]).count();
        assert_eq!(missing, 1);
        assert!(!choices.iter().find(|c| c.folder == extra[0]).unwrap().complete);
        // The previous-result folder name used by re-run.
        assert_eq!(
            Path::new("/x/Run_1/normalization").with_extension("previous"),
            PathBuf::from("/x/Run_1/normalization.previous")
        );
    }

    #[test]
    fn ob_run_numbers_come_from_the_folder_names() {
        let folders = vec![
            PathBuf::from("/x/ob/20260909_Run_29905_virgin_c_ob_1_185C_1_000AngsMin_ob_0"),
            PathBuf::from("/x/ob/20260909_Run_29906_virgin_c_ob_1_185C_1_000AngsMin_ob_1"),
            PathBuf::from("/x/ob/no_run_here"),
        ];
        assert_eq!(MonitorApp::ob_run_numbers(&folders), vec![29905, 29906]);
        assert_eq!(MonitorApp::ob_runs_text(&folders), "29905, 29906");
        assert_eq!(MonitorApp::ob_runs_text(&folders[2..]), "no_run_here");
        assert_eq!(MonitorApp::ob_runs_text(&[]), "");
    }
}

/// egui's proportional family (Ubuntu-Light + the emoji fonts) has no glyph
/// for the arrows (→ ← ↑ ↓), bullets and similar symbols used in the labels,
/// which then show up as squares; the bundled monospace font Hack has them,
/// so it is appended as the last fallback of the proportional family.
fn install_fonts(ctx: &eframe::egui::Context) {
    let mut fonts = eframe::egui::FontDefinitions::default();
    if let Some(family) = fonts.families.get_mut(&eframe::egui::FontFamily::Proportional) {
        if !family.iter().any(|f| f == "Hack") {
            family.push("Hack".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}
