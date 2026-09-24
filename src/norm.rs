//! NeuNorm normalizations run through the VENUS workflow-runner script
//! (`normalize_tof.py`), in background threads:
//!
//! - **Per-run**: a run normalized on its own, with the configuration
//!   file's sample replaced by that run — automatically for every new run
//!   when auto normalization is ON (as soon as its detector-corrected
//!   folder shows up), on demand for older runs. Output goes under the
//!   configuration's output folder, in a folder named after the run's
//!   corrected (input) folder: `<config output folder>/<corrected folder
//!   name>/normalization` (`<IPTS>/shared/autoreduce/normalized/…` when
//!   the configuration names no output folder). A result the workflow
//!   runner or an older version of this tool left in the legacy layout
//!   (`Run_<run>/normalization`) is found there and not redone.
//! - **Rolling windows**: the runs acquired within the last N minutes
//!   (acquisition time from the NeXus `end_time`) normalized together, one
//!   job per time window. Output:
//!   `<IPTS>/shared/autoreduce/normalized/rolling/anchor_<run>/last_<N>min`.
//!
//! Sample inputs are the runs' detector-corrected folders; open-beam
//! folders come from the normalization configuration file. Every job runs
//! the workflow runner's process: a result already there is never redone,
//! the inputs are pre-cropped on disk when the configuration has a crop
//! region, the script's output is streamed into `logs/<name>.log` next to
//! the result, the job stages into a `.partial` folder and is promoted on
//! success. Every job also appends a human-readable summary — sample,
//! open beams, configuration and its settings, output folder, start and
//! end times — to one `autoreduction.log` at the top of the output folder
//! (see [`SUMMARY_LOG`]), written live: the start block when the job
//! launches, the end line when it finishes.

use crate::{files, h5};
use chrono::{DateTime, FixedOffset};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

/// The headless NeuNorm normalization of the VENUS workflow runner.
pub const NORMALIZE_SCRIPT: &str =
    "/SNS/VENUS/shared/software/git/rust_workflow_runner/scripts/normalize_tof.py";
/// Python of the marimo_notebooks pixi environment (has neunorm/scipp/h5py).
pub const PYTHON_BIN: &str =
    "/SNS/VENUS/shared/software/git/marimo_notebooks/.pixi/envs/default/bin/python";

/// One rolling time window ("the last N minutes of acquisition").
pub struct Window {
    pub minutes: u32,
    /// Runs whose acquisition ended within the window (newest anchor run
    /// included), ascending.
    pub runs: Vec<u64>,
    pub state: JobState,
}

impl Window {
    pub fn new(minutes: u32) -> Self {
        Self {
            minutes,
            runs: Vec::new(),
            state: JobState::Idle,
        }
    }
}

/// What a job normalizes: one rolling window (by index) or one run.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JobTarget {
    Window(usize),
    Run(u64),
}

/// Lifecycle of one normalization job (a window's or a run's).
pub enum JobState {
    Idle,
    Running {
        /// Runs the job was launched with (the window may drift meanwhile).
        runs: Vec<u64>,
        /// Current NeuNorm stage (from the script's PROGRESS lines).
        stage: String,
        /// Progress within the stage, 0..=1 when the total is known.
        fraction: Option<f32>,
        /// The whole workflow, in order (from the script's STAGES line);
        /// empty until announced.
        stages: Vec<String>,
    },
    Done {
        output: PathBuf,
        finished: DateTime<chrono::Local>,
        runs: Vec<u64>,
    },
    Failed {
        message: String,
        /// The job's log file, when the job got far enough to open one.
        log: Option<PathBuf>,
    },
}

/// Messages a job thread sends while it runs and when it ends.
pub enum JobMessage {
    /// The script's STAGES line: the workflow's stages, in order.
    Stages {
        target: JobTarget,
        stages: Vec<String>,
    },
    /// A PROGRESS line of the normalization script.
    Progress {
        target: JobTarget,
        stage: String,
        fraction: Option<f32>,
    },
    /// Any other output line of the job (script output, runner steps).
    Output {
        target: JobTarget,
        line: String,
    },
    Finished {
        target: JobTarget,
        runs: Vec<u64>,
        result: Result<PathBuf, String>,
        /// The job's log file (what happened, in full).
        log: PathBuf,
    },
}

/// Parse one `PROGRESS <done>/<total> <label>` line of the normalization
/// script (`total` is `-` when unknown) into `(label, fraction)`.
pub fn parse_progress(line: &str) -> Option<(String, Option<f32>)> {
    let rest = line.strip_prefix("PROGRESS ")?;
    let (counts, label) = rest.split_once(' ')?;
    let (done, total) = counts.split_once('/')?;
    let fraction = match (done.parse::<f64>(), total.parse::<f64>()) {
        (Ok(done), Ok(total)) if total > 0.0 => Some((done / total).clamp(0.0, 1.0) as f32),
        _ => None, // total "-" or unparsable: indeterminate
    };
    Some((label.trim().to_owned(), fraction))
}

/// Parse the script's `STAGES <label>|<label>|...` line.
pub fn parse_stages(line: &str) -> Option<Vec<String>> {
    let rest = line.strip_prefix("STAGES ")?;
    let stages: Vec<String> = rest
        .split('|')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    (!stages.is_empty()).then_some(stages)
}

/// Overall progress of a job: `(index of the current stage, count)` and the
/// 0..=1 fraction of the whole workflow (stages weighted equally, the
/// current one by its own fraction). `None` before the stages are known or
/// for a stage outside the announced list.
pub fn overall_progress(
    stages: &[String],
    stage: &str,
    fraction: Option<f32>,
) -> Option<((usize, usize), f32)> {
    let n = stages.len();
    let i = stages.iter().position(|s| s == stage)?;
    let within = fraction.unwrap_or(0.0).clamp(0.0, 1.0);
    Some(((i + 1, n), ((i as f32) + within) / (n as f32)))
}

/// Fill each window with the runs whose acquisition ended within its last
/// N minutes, anchored at the most recent end time of `end_times`
/// (`(run, end_time)` pairs). Runs without a readable end time are ignored.
pub fn assign_windows(windows: &mut [Window], end_times: &[(u64, DateTime<FixedOffset>)]) {
    let Some(anchor) = end_times.iter().map(|(_, t)| *t).max() else {
        for w in windows {
            w.runs.clear();
        }
        return;
    };
    for w in windows.iter_mut() {
        let cutoff = anchor - chrono::Duration::minutes(i64::from(w.minutes));
        w.runs = end_times
            .iter()
            .filter(|(_, t)| *t >= cutoff)
            .map(|(run, _)| *run)
            .collect();
        w.runs.sort_unstable();
    }
}

/// Output folder of one window's normalization.
pub fn output_dir(ipts_path: &Path, anchor_run: u64, minutes: u32) -> PathBuf {
    ipts_path.join(format!(
        "shared/autoreduce/normalized/rolling/anchor_{anchor_run}/last_{minutes}min"
    ))
}

/// Name of the summary log every job appends to, at the top of the
/// output folder (`<output folder>/autoreduction.log` for the per-run
/// normalizations, `<IPTS>/shared/autoreduce/normalized/rolling/
/// autoreduction.log` for the rolling windows).
pub const SUMMARY_LOG: &str = "autoreduction.log";

/// Base folder the per-run results go under: the configuration's output
/// folder, else `<IPTS>/shared/autoreduce/normalized`.
pub fn output_base(ipts_path: &Path, config_info: &h5::ConfigInfo) -> PathBuf {
    config_info
        .output_folder
        .clone()
        .unwrap_or_else(|| ipts_path.join("shared/autoreduce/normalized"))
}

/// Name of a run's row folder under the output base: the name of its
/// corrected (input) folder, e.g. `20260921_Run_30340_sample_3_000AngsMin_0`
/// — `Run_<run>` when the corrected folder is not known (yet).
pub fn run_row_name(run: u64, corrected: Option<&Path>) -> String {
    corrected
        .and_then(|f| f.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| format!("Run_{run}"))
}

/// Output folder of one run's own normalization: `<output base>/<corrected
/// folder name>/normalization` (see [`run_row_name`]). A complete result
/// left in the legacy layout — `<output base>/Run_<run>/normalization`, the
/// workflow runner's and this tool's before 2026-09-24 — is returned
/// instead when nothing sits at the new place, so it is found and never
/// redone.
pub fn run_output_dir(
    ipts_path: &Path,
    run: u64,
    corrected: Option<&Path>,
    config_info: &h5::ConfigInfo,
) -> PathBuf {
    let base = output_base(ipts_path, config_info);
    let row = run_row_name(run, corrected);
    let output = base.join(&row).join("normalization");
    let legacy_row = format!("Run_{run}");
    if row != legacy_row && !output_is_done(&output) {
        let legacy = base.join(legacy_row).join("normalization");
        if output_is_done(&legacy) {
            return legacy;
        }
    }
    output
}

/// Is a normalization output folder complete (holds at least one TIFF)?
/// A folder still being written is a `.partial` sibling, so any TIFF in
/// the final folder means the job finished.
pub fn output_is_done(output: &Path) -> bool {
    std::fs::read_dir(output)
        .map(|entries| {
            entries.flatten().any(|e| {
                let name = e.file_name().to_string_lossy().to_ascii_lowercase();
                name.ends_with(".tif") || name.ends_with(".tiff")
            })
        })
        .unwrap_or(false)
}

/// Everything a normalization job needs, resolved up-front so problems are
/// reported before any thread is spawned.
pub struct JobSpec {
    pub target: JobTarget,
    pub runs: Vec<u64>,
    /// (corrected folder, NeXus file) per sample run.
    samples: Vec<(PathBuf, PathBuf)>,
    /// (corrected folder, NeXus file) per open-beam run.
    obs: Vec<(PathBuf, PathBuf)>,
    config: PathBuf,
    /// Final result folder (`<row>/normalization`, or a window's folder).
    output: PathBuf,
    /// The job's log file (`<row>/logs/<name>.log`).
    pub log: PathBuf,
    /// The summary log the job appends to (`<base>/autoreduction.log`).
    pub summary: PathBuf,
    /// What the job normalizes, for the summary log: `Run 30340` or
    /// `Runs 30338, 30339, 30340 (last 5 min window)`.
    what: String,
    /// The configuration's settings, as text, for the summary log.
    parameters: Vec<(String, String)>,
    /// Pre-crop region from the configuration, and where the cropped
    /// copies of the inputs go (`<base>/cropped_x0…_y1…/<folder name>`).
    crop: Option<((usize, usize, usize, usize), PathBuf)>,
}

impl JobSpec {
    /// The open-beam folders the job normalizes with.
    pub fn ob_folders(&self) -> Vec<PathBuf> {
        self.obs.iter().map(|(folder, _)| folder.clone()).collect()
    }

    /// The configuration file the job normalizes with.
    pub fn config(&self) -> &Path {
        &self.config
    }

    /// The final result folder of the job.
    pub fn output(&self) -> &Path {
        &self.output
    }
}

/// What a finished normalization was launched with, read back from its
/// job log (`<row>/logs/normalization.log`, written by this tool and by
/// the workflow runner alike): the `running: "python" "script" "--config"
/// "<file>" … "--ob" "<folder>" …` line — its last occurrence, the one of
/// the launch that produced the result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchInfo {
    /// The `--config` file, when the line names one.
    pub config: Option<PathBuf>,
    /// The `--ob` folders, in order.
    pub obs: Vec<PathBuf>,
}

/// The launch a job log records (see [`LaunchInfo`]). `None` when the log
/// is missing or has no `running:` line naming an open beam or a
/// configuration.
pub fn launch_from_log(log: &Path) -> Option<LaunchInfo> {
    let text = std::fs::read_to_string(log).ok()?;
    text.lines()
        .rev()
        .filter(|line| line.contains("running:"))
        .filter_map(|line| {
            let info = launch_from_command_line(line);
            (!info.obs.is_empty() || info.config.is_some()).then_some(info)
        })
        .next()
}

/// The `--config` / `--ob` arguments of a `running: {cmd:?}` log line:
/// the Debug form of a `Command` quotes every argument (`"--ob"
/// "/path/x"`), so the tokens are the quoted strings in order, and a
/// value is the token after its flag.
fn launch_from_command_line(line: &str) -> LaunchInfo {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (in_quotes, c) {
            (false, '"') => in_quotes = true,
            (true, '"') => {
                in_quotes = false;
                tokens.push(std::mem::take(&mut current));
            }
            (true, '\\') => {
                if let Some(escaped) = chars.next() {
                    current.push(escaped);
                }
            }
            (true, c) => current.push(c),
            _ => {}
        }
    }
    LaunchInfo {
        config: tokens
            .windows(2)
            .find(|pair| pair[0] == "--config")
            .map(|pair| PathBuf::from(&pair[1])),
        obs: tokens
            .windows(2)
            .filter(|pair| pair[0] == "--ob")
            .map(|pair| PathBuf::from(&pair[1]))
            .collect(),
    }
}

/// The workflow runner's crop-copy parent folder for a region.
fn crop_parent(base: &Path, (x0, y0, x1, y1): (usize, usize, usize, usize)) -> PathBuf {
    base.join(format!("cropped_x0{x0}_y0{y0}_x1{x1}_y1{y1}"))
}

/// Open beams: folder from the config; NeXus derived from the folder (run
/// number in its name, IPTS from its path — OBs may live in another IPTS
/// than the samples).
fn resolve_obs(
    ipts_path: &Path,
    config_info: &h5::ConfigInfo,
) -> Result<Vec<(PathBuf, PathBuf)>, String> {
    if config_info.ob_folders.is_empty() {
        return Err("the configuration file names no open-beam folder".to_owned());
    }
    let mut obs = Vec::new();
    for folder in &config_info.ob_folders {
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let run = files::run_number_in_name(&name)
            .ok_or_else(|| format!("no run number in OB folder name '{name}'"))?;
        let ob_ipts = folder
            .iter()
            .find(|part| part.to_string_lossy().starts_with("IPTS-"))
            .map(|p| Path::new("/SNS/VENUS").join(p))
            .unwrap_or_else(|| ipts_path.to_path_buf());
        obs.push((folder.clone(), files::nexus_path(&ob_ipts, run)));
    }
    Ok(obs)
}

/// NeuNorm divides the sample stack by the open-beam stack frame by frame:
/// every sample and OB folder must hold the same number of images, or the
/// run fails deep inside the pipeline ("Mismatch in coordinate 'tof'").
/// The notebook refuses to launch on that (its Data Quality Check); so
/// does this, with the counts spelled out.
fn check_frame_counts(
    samples: &[(PathBuf, PathBuf)],
    obs: &[(PathBuf, PathBuf)],
) -> Result<(), String> {
    let describe = |(folder, _): &(PathBuf, PathBuf)| {
        let name = folder
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let run = files::run_number_in_name(&name)
            .map(|r| format!("run {r}"))
            .unwrap_or(name);
        (run, files::tiff_count(folder))
    };
    let samples: Vec<(String, usize)> = samples.iter().map(describe).collect();
    let obs: Vec<(String, usize)> = obs.iter().map(describe).collect();
    let first = samples.first().map(|(_, n)| *n).unwrap_or(0);
    let same = samples.iter().chain(obs.iter()).all(|(_, n)| *n == first);
    if same {
        return Ok(());
    }
    let list = |items: &[(String, usize)]| {
        items
            .iter()
            .map(|(what, n)| format!("{what}: {n} images"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(format!(
        "sample and open beam do not have the same number of images — \
         sample {}; open beam {}. NeuNorm needs matching stacks: pick open \
         beams acquired with the same settings (and re-save the \
         configuration from the notebook so it names them)",
        list(&samples),
        list(&obs)
    ))
}

/// Resolve one window into a launchable job. Errors name what is missing
/// (no runs, runs without corrected data, config problems, …).
pub fn prepare_job(
    window_index: usize,
    window: &Window,
    ipts_path: &Path,
    config_path: &Path,
    config_info: &h5::ConfigInfo,
) -> Result<JobSpec, String> {
    if window.runs.is_empty() {
        return Err("no run in the window".to_owned());
    }
    let obs = resolve_obs(ipts_path, config_info)?;

    // Corrected folders of the window's runs; every run must be there (a
    // missing folder means the autoreduction has not caught up yet).
    let status = files::check_runs(ipts_path, &window.runs);
    let mut samples = Vec::new();
    let mut not_ready = Vec::new();
    for run in &status {
        match &run.corrected {
            files::FileStatus::Present(folder) => {
                samples.push((folder.clone(), files::nexus_path(ipts_path, run.run)));
            }
            _ => not_ready.push(run.run.to_string()),
        }
    }
    if samples.is_empty() {
        return Err(format!(
            "no corrected data yet for run(s) {}",
            not_ready.join(", ")
        ));
    }

    check_frame_counts(&samples, &obs)?;

    let anchor_run = *window.runs.iter().max().expect("runs not empty");
    let output = output_dir(ipts_path, anchor_run, window.minutes);
    let anchor_dir = output.parent().expect("window output has a parent").to_path_buf();
    let rolling = ipts_path.join("shared/autoreduce/normalized/rolling");
    let runs_text: Vec<String> = window.runs.iter().map(|r| r.to_string()).collect();
    Ok(JobSpec {
        target: JobTarget::Window(window_index),
        runs: window.runs.clone(),
        samples,
        obs,
        config: config_path.to_path_buf(),
        log: anchor_dir.join("logs").join(format!("last_{}min.log", window.minutes)),
        summary: rolling.join(SUMMARY_LOG),
        what: format!(
            "Runs {} (last {} min window)",
            runs_text.join(", "),
            window.minutes
        ),
        parameters: config_info.parameters.clone(),
        crop: config_info
            .crop_region
            .map(|region| (region, crop_parent(&rolling, region))),
        output,
    })
}

/// Resolve one run's own normalization: the configuration's sample is
/// replaced by that run's corrected folder, everything else (open beams,
/// settings) comes from the configuration file. The result goes to
/// `<output base>/<corrected folder name>/normalization`.
pub fn prepare_run_job(
    run: u64,
    corrected_folder: &Path,
    ipts_path: &Path,
    config_path: &Path,
    config_info: &h5::ConfigInfo,
) -> Result<JobSpec, String> {
    let obs = resolve_obs(ipts_path, config_info)?;
    let samples = vec![(
        corrected_folder.to_path_buf(),
        files::nexus_path(ipts_path, run),
    )];
    check_frame_counts(&samples, &obs)?;
    let output = run_output_dir(ipts_path, run, Some(corrected_folder), config_info);
    let row_dir = output.parent().expect("run output has a parent").to_path_buf();
    let base = row_dir.parent().expect("row dir has a parent").to_path_buf();
    Ok(JobSpec {
        target: JobTarget::Run(run),
        runs: vec![run],
        samples,
        obs,
        config: config_path.to_path_buf(),
        log: row_dir.join("logs").join("normalization.log"),
        summary: base.join(SUMMARY_LOG),
        what: format!("Run {run}"),
        parameters: config_info.parameters.clone(),
        crop: config_info
            .crop_region
            .map(|region| (region, crop_parent(&base, region))),
        output,
    })
}

/// Start of the failure message of a job whose python process was killed
/// by a signal (the OOM killer, in practice) — see [`killed_by_system`].
pub const KILLED_PREFIX: &str = "normalization process killed by the system";

/// Was this job's failure a kill by the system (SIGKILL & co.) rather
/// than an error of the script? Such a failure is transient (memory
/// pressure) and worth one automatic retry.
pub fn killed_by_system(message: &str) -> bool {
    message.starts_with(KILLED_PREFIX)
}

/// Launch a prepared job in a background thread; progress and the outcome
/// arrive on `tx` as [`JobMessage`]s. The job stages into
/// `<output>.partial` and promotes to `<output>` on success.
pub fn launch(spec: JobSpec, tx: Sender<JobMessage>) {
    std::thread::spawn(move || {
        let result = run_job(&spec, &tx);
        let _ = tx.send(JobMessage::Finished {
            target: spec.target,
            runs: spec.runs.clone(),
            result,
            log: spec.log.clone(),
        });
    });
}

/// Append-only job log (`<row>/logs/<name>.log`), the workflow runner's
/// format: timestamped lines for the runner's own steps, `  | ` lines for
/// the streamed script output.
struct Log {
    path: PathBuf,
}

impl Log {
    fn open(path: &Path) -> Result<Log, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        Ok(Log {
            path: path.to_path_buf(),
        })
    }

    fn line(&self, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "[{}] {msg}", chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ"));
        }
    }

    fn raw_line(&self, msg: &str) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(f, "  | {msg}");
        }
    }
}

/// Local wall-clock time, as the summary log prints it.
fn clock_text(t: DateTime<chrono::Local>) -> String {
    t.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// A duration in seconds as `41 s`, `8 min 41 s`, `1 h 02 min 05 s`.
pub fn duration_text(seconds: i64) -> String {
    let seconds = seconds.max(0);
    let (h, m, s) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    match (h, m) {
        (0, 0) => format!("{s} s"),
        (0, m) => format!("{m} min {s:02} s"),
        (h, m) => format!("{h} h {m:02} min {s:02} s"),
    }
}

/// The summary log (`autoreduction.log`) block a job appends when it
/// starts: what it normalizes, every input, the configuration and its
/// settings, where the result goes. One block, so parallel jobs never
/// interleave their lines.
fn summary_start_block(spec: &JobSpec, started: DateTime<chrono::Local>) -> String {
    let mut text = String::new();
    let line = |text: &mut String, label: &str, value: &str| {
        text.push_str(&format!("  {label:<14}: {value}\n"));
    };
    text.push_str(&format!(
        "{}\n[{}] {} — normalization STARTED\n",
        "-".repeat(78),
        clock_text(started),
        spec.what
    ));
    for (i, (folder, nexus)) in spec.samples.iter().enumerate() {
        let label = if spec.samples.len() == 1 {
            "sample".to_owned()
        } else {
            format!("sample {}", i + 1)
        };
        line(&mut text, &label, &folder.display().to_string());
        line(&mut text, "  nexus", &nexus.display().to_string());
    }
    for (i, (folder, nexus)) in spec.obs.iter().enumerate() {
        let label = if spec.obs.len() == 1 {
            "open beam".to_owned()
        } else {
            format!("open beam {}", i + 1)
        };
        line(&mut text, &label, &folder.display().to_string());
        line(&mut text, "  nexus", &nexus.display().to_string());
    }
    line(&mut text, "configuration", &spec.config.display().to_string());
    for (name, value) in &spec.parameters {
        text.push_str(&format!("      {name} = {value}\n"));
    }
    if let Some(((x0, y0, x1, y1), parent)) = &spec.crop {
        line(
            &mut text,
            "pre-crop",
            &format!("(x0, y0, x1, y1) = ({x0}, {y0}, {x1}, {y1}) — cropped copies in {}", parent.display()),
        );
    }
    line(&mut text, "output", &spec.output.display().to_string());
    line(&mut text, "job log", &spec.log.display().to_string());
    text
}

/// The summary log line a job appends when it ends.
fn summary_end_line(
    spec: &JobSpec,
    started: DateTime<chrono::Local>,
    ended: DateTime<chrono::Local>,
    outcome: &str,
) -> String {
    format!(
        "[{}] {} — normalization {outcome} (started {}, took {})\n",
        clock_text(ended),
        spec.what,
        clock_text(started),
        duration_text((ended - started).num_seconds())
    )
}

/// Append one block to the summary log (created with its folder on first
/// use). A summary that cannot be written never fails the job.
fn append_summary(path: &Path, text: &str) {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Outcome of [`run_job_inner`]: the result folder, and whether the job
/// found it already there (skipped) rather than produced it.
struct Outcome {
    output: PathBuf,
    skipped: bool,
}

/// Run one job and keep the summary log (`autoreduction.log`) up to date:
/// the start block when the job launches, one end line when it finishes
/// (DONE with the result folder, FAILED with the first line of the error,
/// SKIPPED when the result was already there).
fn run_job(spec: &JobSpec, tx: &Sender<JobMessage>) -> Result<PathBuf, String> {
    let started = chrono::Local::now();
    let result = run_job_inner(spec, tx, started);
    let ended = chrono::Local::now();
    match &result {
        Ok(Outcome { skipped: true, .. }) => {} // its own one-liner is written
        Ok(Outcome { output, .. }) => append_summary(
            &spec.summary,
            &summary_end_line(spec, started, ended, &format!("DONE → {}", output.display())),
        ),
        Err(message) => append_summary(
            &spec.summary,
            &summary_end_line(
                spec,
                started,
                ended,
                &format!("FAILED: {}", message.lines().next().unwrap_or("").trim()),
            ),
        ),
    }
    result.map(|o| o.output)
}

fn run_job_inner(
    spec: &JobSpec,
    tx: &Sender<JobMessage>,
    started: DateTime<chrono::Local>,
) -> Result<Outcome, String> {
    let log = Log::open(&spec.log)?;
    let progress = |stage: String, fraction: Option<f32>| {
        let _ = tx.send(JobMessage::Progress {
            target: spec.target,
            stage,
            fraction,
        });
    };
    let say = |msg: &str| {
        log.line(msg);
        let _ = tx.send(JobMessage::Output {
            target: spec.target,
            line: msg.to_owned(),
        });
    };

    // Never run twice: a complete result is reused as is.
    if output_is_done(&spec.output) {
        say("normalization output already there — skipped (never run twice)");
        append_summary(
            &spec.summary,
            &format!(
                "[{}] {} — normalization SKIPPED, result already there: {}\n",
                clock_text(started),
                spec.what,
                spec.output.display()
            ),
        );
        return Ok(Outcome {
            output: spec.output.clone(),
            skipped: true,
        });
    }
    append_summary(&spec.summary, &summary_start_block(spec, started));

    // Pre-crop every sample/OB folder when the configuration asks for it
    // (notebook convention), and normalize the cropped copies.
    let (mut samples, mut obs) = (spec.samples.clone(), spec.obs.clone());
    if let Some((region, parent)) = &spec.crop {
        let (x0, y0, x1, y1) = *region;
        say(&format!(
            "pre-crop (x0, y0, x1, y1) = ({x0}, {y0}, {x1}, {y1}) into {}",
            parent.display()
        ));
        for (folder, _) in samples.iter_mut().chain(obs.iter_mut()) {
            let name = folder
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            progress(format!("pre-cropping {name}…"), None);
            *folder = precrop_folder(folder, *region, parent, &log)?;
        }
    }

    progress("running NeuNorm…".to_owned(), None);
    let partial = spec.output.with_extension("partial");
    // A leftover staging folder from a crashed run would confuse the
    // promote step: start clean.
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;

    // Images are named after the final folder, not the .partial staging one.
    let basename = spec
        .output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "normalization".to_owned());
    let mut cmd = std::process::Command::new(PYTHON_BIN);
    cmd.arg(NORMALIZE_SCRIPT)
        .arg("--config")
        .arg(&spec.config)
        .arg("--output")
        .arg(&partial)
        .arg("--basename")
        .arg(&basename);
    for (folder, nexus) in &samples {
        cmd.arg("--sample").arg(folder).arg("--sample-nexus").arg(nexus);
    }
    for (folder, nexus) in &obs {
        cmd.arg("--ob").arg(folder).arg("--ob-nexus").arg(nexus);
    }
    say(&format!("running: {cmd:?}"));

    // Stream the script's output: both pipes drained concurrently (a full
    // one would block the child) into one channel; PROGRESS lines feed the
    // progress bar, everything else goes to the log (and the error tail).
    use std::io::BufRead;
    use std::process::Stdio;
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot launch {PYTHON_BIN}: {e}"))?;
    let (ltx, lrx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    let pipes: [Option<Box<dyn std::io::Read + Send>>; 2] = [
        child.stdout.take().map(|p| Box::new(p) as _),
        child.stderr.take().map(|p| Box::new(p) as _),
    ];
    for pipe in pipes.into_iter().flatten() {
        let ltx = ltx.clone();
        readers.push(std::thread::spawn(move || {
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                if ltx.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(ltx);
    let mut tail: Vec<String> = Vec::new();
    for line in lrx {
        if let Some((stage, fraction)) = parse_progress(&line) {
            progress(stage, fraction);
            continue;
        }
        if let Some(stages) = parse_stages(&line) {
            let _ = tx.send(JobMessage::Stages {
                target: spec.target,
                stages,
            });
            continue;
        }
        log.raw_line(&line);
        if !line.trim().is_empty() {
            let _ = tx.send(JobMessage::Output {
                target: spec.target,
                line: line.clone(),
            });
            tail.push(line);
            if tail.len() > 8 {
                tail.remove(0);
            }
        }
    }
    for reader in readers {
        let _ = reader.join();
    }
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for the normalization: {e}"))?;
    if !status.success() {
        use std::os::unix::process::ExitStatusExt;
        // A signal (SIGKILL above all) does not come from the script: it is
        // the kernel's OOM killer when the analysis machine runs out of
        // memory — every NeuNorm job holds the sample and open-beam stacks
        // in memory, and too many at once (from this tool, other users or
        // other tools) exhaust it.
        let message = if status.signal().is_some() {
            format!(
                "{KILLED_PREFIX} ({status}) — the analysis machine most likely ran out of \
                 memory: every normalization holds the sample and open-beam image stacks \
                 in memory, and too many at once (this tool's parallel jobs, other users, \
                 other tools on the same machine) exhaust it. The run is retried once \
                 automatically when a slot frees up; fewer parallel jobs help.\n{}",
                tail.join("\n")
            )
        } else {
            format!(
                "normalization script failed ({status})\n{}",
                tail.join("\n")
            )
        };
        say(&format!("ERROR: {message}"));
        return Err(message);
    }

    // Promote the staging folder to the final name (a leftover empty
    // destination would make the rename fail).
    if spec.output.exists() && !output_is_done(&spec.output) {
        let _ = std::fs::remove_dir_all(&spec.output);
    }
    std::fs::rename(&partial, &spec.output).map_err(|e| {
        let message = format!(
            "normalized, but cannot rename {} -> {}: {e}",
            partial.display(),
            spec.output.display()
        );
        log.line(&format!("ERROR: {message}"));
        message
    })?;
    say(&format!("normalized data written to {}", spec.output.display()));
    Ok(Outcome {
        output: spec.output.clone(),
        skipped: false,
    })
}

/// Crop one input folder into `<crop_parent>/<folder name>/` (exclusive-stop
/// bounds in the on-disk frame of the files), workflow-runner conventions:
/// a destination already holding the same number of TIFFs is reused as is,
/// and the `*_Spectra.txt` sidecars are copied along either way. Returns
/// the destination folder.
fn precrop_folder(
    src: &Path,
    (x0, y0, x1, y1): (usize, usize, usize, usize),
    crop_parent: &Path,
    log: &Log,
) -> Result<PathBuf, String> {
    use rayon::prelude::*;
    let name = src
        .file_name()
        .ok_or_else(|| format!("input folder {} has no name", src.display()))?;
    let dest = crop_parent.join(name);
    let src_tiffs = list_tiff_in_dir(src)?;
    let existing = list_tiff_in_dir(&dest).map(|v| v.len()).unwrap_or(0);
    if existing == src_tiffs.len() {
        log.line(&format!(
            "pre-crop — {} already cropped ({existing} TIFF(s) in {}), reused",
            name.to_string_lossy(),
            dest.display()
        ));
    } else {
        std::fs::create_dir_all(&dest)
            .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
        src_tiffs.par_iter().try_for_each(|path| -> Result<(), String> {
            let (values, w, h) = load_tiff_frame(path)?;
            if x1 > w || y1 > h || x0 >= x1 || y0 >= y1 {
                return Err(format!(
                    "crop region ({x0}, {y0}, {x1}, {y1}) does not fit the {w}×{h} image {}",
                    path.display()
                ));
            }
            let cropped: Vec<f32> = (y0..y1)
                .flat_map(|row| values[row * w + x0..row * w + x1].to_vec())
                .collect();
            let file_name = path
                .file_name()
                .ok_or_else(|| format!("{} has no name", path.display()))?;
            write_tiff_f32(&dest.join(file_name), x1 - x0, y1 - y0, &cropped)
        })?;
        log.line(&format!(
            "pre-crop — {}: {} TIFF(s) cropped into {}",
            name.to_string_lossy(),
            src_tiffs.len(),
            dest.display()
        ));
    }
    let entries =
        std::fs::read_dir(src).map_err(|e| format!("cannot read {}: {e}", src.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        let lower = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase();
        if path.is_file() && lower.ends_with("_spectra.txt") {
            std::fs::copy(&path, dest.join(path.file_name().unwrap_or_default()))
                .map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
        }
    }
    Ok(dest)
}

fn list_tiff_in_dir(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries =
        std::fs::read_dir(dir).map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            p.is_file() && (ext == "tif" || ext == "tiff")
        })
        .collect();
    if out.is_empty() {
        return Err(format!("no TIFF files found in {}", dir.display()));
    }
    out.sort();
    Ok(out)
}

/// Read the first page of a TIFF as a flat row-major f32 buffer. Extra
/// samples per pixel (e.g. RGB) keep only the first sample.
fn load_tiff_frame(path: &Path) -> Result<(Vec<f32>, usize, usize), String> {
    use tiff::decoder::{Decoder, DecodingResult};
    let err = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
    let file = std::fs::File::open(path).map_err(|e| err(&e))?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file)).map_err(|e| err(&e))?;
    let (w, h) = decoder.dimensions().map_err(|e| err(&e))?;
    let (w, h) = (w as usize, h as usize);
    let values: Vec<f32> = match decoder.read_image().map_err(|e| err(&e))? {
        DecodingResult::U8(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U32(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::U64(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I8(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I16(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I32(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::I64(v) => v.into_iter().map(|x| x as f32).collect(),
        DecodingResult::F16(v) => v.into_iter().map(|x| x.to_f32()).collect(),
        DecodingResult::F32(v) => v,
        DecodingResult::F64(v) => v.into_iter().map(|x| x as f32).collect(),
    };
    let expected = w * h;
    if values.len() == expected {
        return Ok((values, w, h));
    }
    if expected > 0 && values.len() % expected == 0 {
        let spp = values.len() / expected;
        return Ok(((0..expected).map(|i| values[i * spp]).collect(), w, h));
    }
    Err(format!(
        "pixel count {} not compatible with {w}×{h} in {}",
        values.len(),
        path.display()
    ))
}

fn write_tiff_f32(path: &Path, w: usize, h: usize, values: &[f32]) -> Result<(), String> {
    let err = |e: &dyn std::fmt::Display| format!("cannot write {}: {e}", path.display());
    let file = std::fs::File::create(path).map_err(|e| err(&e))?;
    let mut enc =
        tiff::encoder::TiffEncoder::new(std::io::BufWriter::new(file)).map_err(|e| err(&e))?;
    enc.write_image::<tiff::encoder::colortype::Gray32Float>(w as u32, h as u32, values)
        .map_err(|e| err(&e))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(s: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(s).unwrap()
    }

    #[test]
    fn assigns_runs_to_windows_by_acquisition_time() {
        let mut windows = [Window::new(5), Window::new(15), Window::new(30)];
        let end_times = vec![
            (100, t("2026-08-22T10:00:00-04:00")), // anchor
            (99, t("2026-08-22T09:57:00-04:00")),  // 3 min before
            (98, t("2026-08-22T09:48:00-04:00")),  // 12 min before
            (97, t("2026-08-22T09:35:00-04:00")),  // 25 min before
            (96, t("2026-08-22T09:20:00-04:00")),  // 40 min before
        ];
        assign_windows(&mut windows, &end_times);
        assert_eq!(windows[0].runs, vec![99, 100]);
        assert_eq!(windows[1].runs, vec![98, 99, 100]);
        assert_eq!(windows[2].runs, vec![97, 98, 99, 100]);
        // No end time at all → empty windows.
        assign_windows(&mut windows, &[]);
        assert!(windows.iter().all(|w| w.runs.is_empty()));
    }

    #[test]
    fn parses_progress_lines() {
        assert_eq!(
            parse_progress("PROGRESS 50/100 normalizing"),
            Some(("normalizing".to_owned(), Some(0.5)))
        );
        assert_eq!(
            parse_progress("PROGRESS 10/- loading sample"),
            Some(("loading sample".to_owned(), None))
        );
        assert_eq!(parse_progress("Writing data to /tmp"), None);
        assert_eq!(parse_progress("PROGRESS garbage"), None);
    }

    fn config_info(output_folder: Option<&str>) -> h5::ConfigInfo {
        h5::ConfigInfo {
            ob_folders: vec![],
            crop_region: None,
            output_folder: output_folder.map(PathBuf::from),
            detector: None,
            parameters: vec![],
        }
    }

    #[test]
    fn run_output_dir_is_named_after_the_corrected_folder() {
        let ipts = Path::new("/SNS/VENUS/IPTS-1");
        let corrected = Path::new(
            "/SNS/VENUS/IPTS-1/shared/autoreduce/images/tpx1/raw/x/20260921_Run_23642_x_3_000AngsMin_0",
        );
        let with_folder = config_info(Some("/SNS/VENUS/IPTS-1/shared/jean"));
        assert_eq!(
            run_output_dir(ipts, 23642, Some(corrected), &with_folder),
            PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/jean/20260921_Run_23642_x_3_000AngsMin_0/normalization"
            )
        );
        // No corrected folder known (yet): the run number stands in.
        assert_eq!(
            run_output_dir(ipts, 23642, None, &with_folder),
            PathBuf::from("/SNS/VENUS/IPTS-1/shared/jean/Run_23642/normalization")
        );
        // No output folder in the configuration: the IPTS default.
        assert_eq!(
            run_output_dir(ipts, 23642, Some(corrected), &config_info(None)),
            PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/autoreduce/normalized/\
                 20260921_Run_23642_x_3_000AngsMin_0/normalization"
            )
        );
        assert_eq!(run_row_name(7, None), "Run_7");
        assert_eq!(run_row_name(7, Some(Path::new("/a/b/20260101_Run_7_s_0"))), "20260101_Run_7_s_0");
    }

    #[test]
    fn run_output_dir_finds_a_legacy_result() {
        let base = std::env::temp_dir().join("anm_test_legacy_output");
        let _ = std::fs::remove_dir_all(&base);
        let info = config_info(base.to_str());
        let ipts = Path::new("/SNS/VENUS/IPTS-1");
        let corrected = Path::new("/x/20260101_Run_9_s_0");
        let fresh = base.join("20260101_Run_9_s_0/normalization");
        let legacy = base.join("Run_9/normalization");
        // Nothing on disk: the new layout.
        assert_eq!(run_output_dir(ipts, 9, Some(corrected), &info), fresh);
        // A complete result in the legacy layout is found there.
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("img_0000.tiff"), b"").unwrap();
        assert_eq!(run_output_dir(ipts, 9, Some(corrected), &info), legacy);
        // An empty legacy folder does not count.
        std::fs::remove_file(legacy.join("img_0000.tiff")).unwrap();
        assert_eq!(run_output_dir(ipts, 9, Some(corrected), &info), fresh);
        // A complete result at the new place wins over a legacy one.
        std::fs::write(legacy.join("img_0000.tiff"), b"").unwrap();
        std::fs::create_dir_all(&fresh).unwrap();
        std::fs::write(fresh.join("img_0000.tiff"), b"").unwrap();
        assert_eq!(run_output_dir(ipts, 9, Some(corrected), &info), fresh);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(duration_text(41), "41 s");
        assert_eq!(duration_text(521), "8 min 41 s");
        assert_eq!(duration_text(3725), "1 h 02 min 05 s");
        assert_eq!(duration_text(-3), "0 s");
    }

    #[test]
    fn summary_log_follows_a_job() {
        let root = std::env::temp_dir().join("anm_test_summary_log");
        let _ = std::fs::remove_dir_all(&root);
        let spec = JobSpec {
            target: JobTarget::Run(30340),
            runs: vec![30340],
            samples: vec![(
                PathBuf::from("/c/20260921_Run_30340_s_0"),
                PathBuf::from("/n/VENUS_30340.nxs.h5"),
            )],
            obs: vec![
                (PathBuf::from("/c/ob/Run_30338_ob_0"), PathBuf::from("/n/VENUS_30338.nxs.h5")),
                (PathBuf::from("/c/ob/Run_30339_ob_0"), PathBuf::from("/n/VENUS_30339.nxs.h5")),
            ],
            config: PathBuf::from("/cfg/normalization_config_x.h5"),
            output: root.join("20260921_Run_30340_s_0/normalization"),
            log: root.join("20260921_Run_30340_s_0/logs/normalization.log"),
            summary: root.join(SUMMARY_LOG),
            what: "Run 30340".to_owned(),
            parameters: vec![
                ("distance_source_detector_m".to_owned(), "25".to_owned()),
                ("proton_charge".to_owned(), "true".to_owned()),
            ],
            crop: Some(((1, 2, 3, 4), root.join("cropped_x01_y02_x13_y14"))),
        };
        let started = chrono::Local.with_ymd_and_hms(2026, 9, 24, 10, 12, 3).unwrap();
        let ended = started + chrono::Duration::seconds(521);
        append_summary(&spec.summary, &summary_start_block(&spec, started));
        append_summary(
            &spec.summary,
            &summary_end_line(&spec, started, ended, "DONE → /out"),
        );
        let text = std::fs::read_to_string(&spec.summary).unwrap();
        let expect = |needle: &str| assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        expect("[2026-09-24 10:12:03] Run 30340 — normalization STARTED");
        expect("sample        : /c/20260921_Run_30340_s_0");
        expect("  nexus       : /n/VENUS_30340.nxs.h5");
        expect("open beam 1   : /c/ob/Run_30338_ob_0");
        expect("open beam 2   : /c/ob/Run_30339_ob_0");
        expect("configuration : /cfg/normalization_config_x.h5");
        expect("      distance_source_detector_m = 25");
        expect("      proton_charge = true");
        expect("pre-crop      : (x0, y0, x1, y1) = (1, 2, 3, 4)");
        expect("output        : ");
        expect("job log       : ");
        expect("[2026-09-24 10:20:44] Run 30340 — normalization DONE → /out (started 2026-09-24 10:12:03, took 8 min 41 s)");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn precrops_with_the_runner_reuse_rule() {
        let root = std::env::temp_dir().join("anm_test_precrop");
        let _ = std::fs::remove_dir_all(&root);
        let src = root.join("20260613_Run_1_x_0");
        std::fs::create_dir_all(&src).unwrap();
        // 10×6 frames holding their row index; two frames + a spectra file.
        let frame: Vec<f32> = (0..6).flat_map(|r| vec![r as f32; 10]).collect();
        write_tiff_f32(&src.join("img_0000.tiff"), 10, 6, &frame).unwrap();
        write_tiff_f32(&src.join("img_0001.tiff"), 10, 6, &frame).unwrap();
        std::fs::write(src.join("img_Spectra.txt"), "tof").unwrap();
        let parent = crop_parent(&root, (2, 1, 8, 5));
        let log = Log::open(&root.join("logs/test.log")).unwrap();
        let dest = precrop_folder(&src, (2, 1, 8, 5), &parent, &log).unwrap();
        assert_eq!(dest, parent.join("20260613_Run_1_x_0"));
        let (values, w, h) = load_tiff_frame(&dest.join("img_0001.tiff")).unwrap();
        assert_eq!((w, h), (6, 4));
        assert_eq!(values[0], 1.0);
        assert_eq!(values[values.len() - 1], 4.0);
        assert!(dest.join("img_Spectra.txt").is_file());
        // Same count already there: reused, not rewritten.
        let before = std::fs::metadata(dest.join("img_0000.tiff")).unwrap().modified().unwrap();
        precrop_folder(&src, (2, 1, 8, 5), &parent, &log).unwrap();
        let after = std::fs::metadata(dest.join("img_0000.tiff")).unwrap().modified().unwrap();
        assert_eq!(before, after);
        // Region outside the frame is refused.
        let err = precrop_folder(&src, (0, 0, 11, 6), &root.join("bad"), &log).unwrap_err();
        assert!(err.contains("does not fit"), "{err}");
    }

    /// End-to-end check of the per-run job on real VENUS data (NeuNorm
    /// takes about a minute): `cargo test --release -- --ignored`. Output
    /// goes under `$ANM_TEST_OUTPUT` (default: the temp dir).
    #[test]
    #[ignore]
    fn normalizes_a_real_run_end_to_end() {
        let ipts_path = Path::new("/SNS/VENUS/IPTS-36967");
        let config = Path::new(
            "/SNS/VENUS/IPTS-36967/shared/autoreduce/configs/normalization_config_20260718_084258.h5",
        );
        if !config.is_file() {
            return;
        }
        let base = std::env::var_os("ANM_TEST_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("anm_test_e2e"));
        let _ = std::fs::remove_dir_all(&base);
        let mut info = h5::read_config_info(config).unwrap();
        info.output_folder = Some(base.clone());
        let run = 23642;
        let corrected = match files::check_runs(ipts_path, &[run]).remove(0).corrected {
            files::FileStatus::Present(folder) => folder,
            other => panic!("no corrected data for run {run}: {other:?}"),
        };
        let spec = prepare_run_job(run, &corrected, ipts_path, config, &info).unwrap();
        let row = corrected.file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(spec.output, base.join(&row).join("normalization"));
        assert_eq!(spec.summary, base.join(SUMMARY_LOG));
        assert_eq!(spec.log, base.join(&row).join("logs/normalization.log"));
        let (tx, rx) = std::sync::mpsc::channel();
        let output = run_job(&spec, &tx).unwrap_or_else(|e| {
            panic!(
                "job failed: {e}\n--- log ---\n{}",
                std::fs::read_to_string(&spec.log).unwrap_or_default()
            )
        });
        assert!(output_is_done(&output));
        assert!(!spec.output.with_extension("partial").exists());
        let progress = rx.try_iter().count();
        assert!(progress > 0, "no PROGRESS message reached the UI channel");
        let log = std::fs::read_to_string(&spec.log).unwrap();
        assert!(log.contains("running:"), "{log}");
        assert!(log.contains("normalized data written to"), "{log}");
        // The summary log followed the job: a start block, an end line.
        let summary = std::fs::read_to_string(&spec.summary).unwrap();
        assert!(summary.contains("Run 23642 — normalization STARTED"), "{summary}");
        assert!(summary.contains(&format!("sample        : {}", corrected.display())), "{summary}");
        assert!(summary.contains("open beam"), "{summary}");
        assert!(summary.contains("      proton_charge = "), "{summary}");
        assert!(summary.contains("Run 23642 — normalization DONE → "), "{summary}");
        // Second call: reused, never run twice.
        assert_eq!(run_job(&spec, &tx).unwrap(), output);
        assert!(std::fs::read_to_string(&spec.log).unwrap().contains("skipped"));
        let summary = std::fs::read_to_string(&spec.summary).unwrap();
        assert!(summary.contains("Run 23642 — normalization SKIPPED"), "{summary}");
        println!("--- {} ---\n{summary}", spec.summary.display());
    }

    #[test]
    fn parses_stages_and_overall_progress() {
        let stages = parse_stages("STAGES loading sample|combining runs|normalizing|exporting")
            .unwrap();
        assert_eq!(stages.len(), 4);
        assert_eq!(parse_stages("STAGES "), None);
        assert_eq!(parse_stages("PROGRESS 1/2 x"), None);
        let ((k, n), f) = overall_progress(&stages, "combining runs", Some(0.5)).unwrap();
        assert_eq!((k, n), (2, 4));
        assert!((f - 0.375).abs() < 1e-6);
        let ((k, _), f) = overall_progress(&stages, "exporting", None).unwrap();
        assert_eq!(k, 4);
        assert!((f - 0.75).abs() < 1e-6);
        assert!(overall_progress(&stages, "unknown", None).is_none());
        assert!(overall_progress(&[], "exporting", None).is_none());
    }

    #[test]
    fn frame_counts_must_match() {
        let root = std::env::temp_dir().join("anm_test_frame_counts");
        let _ = std::fs::remove_dir_all(&root);
        let sample = root.join("20260909_Run_29930_x_23");
        let ob = root.join("20260909_Run_29902_x_ob_0");
        std::fs::create_dir_all(&sample).unwrap();
        std::fs::create_dir_all(&ob).unwrap();
        for i in 0..3 {
            std::fs::write(sample.join(format!("s_{i}.tif")), "x").unwrap();
        }
        for i in 0..2 {
            std::fs::write(ob.join(format!("o_{i}.tif")), "x").unwrap();
        }
        let nexus = root.join("n.h5");
        let samples = vec![(sample.clone(), nexus.clone())];
        let obs = vec![(ob.clone(), nexus.clone())];
        let err = check_frame_counts(&samples, &obs).unwrap_err();
        assert!(err.contains("run 29930: 3 images"), "{err}");
        assert!(err.contains("run 29902: 2 images"), "{err}");
        std::fs::write(ob.join("o_2.tif"), "x").unwrap();
        assert!(check_frame_counts(&samples, &obs).is_ok());
    }

    #[test]
    fn open_beams_are_read_back_from_a_job_log() {
        let dir = std::env::temp_dir().join("anm_test_obs_from_log");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("normalization.log");
        // Two launches: the first with one OB, the second (the one that
        // produced the result) with two — the last one counts.
        std::fs::write(
            &log,
            "[2026-09-10T18:06:22Z] running: \"/py\" \"/s.py\" \"--config\" \"/c.h5\" \
             \"--sample\" \"/x/Run_1_s\" \"--sample-nexus\" \"/n/1.h5\" \"--ob\" \"/ob/Run_5_ob_0\" \
             \"--ob-nexus\" \"/n/5.h5\"\n  | some output\n\
             [2026-09-10T18:09:00Z] running: \"/py\" \"/s.py\" \"--ob\" \"/ob/Run_7_ob_0\" \
             \"--ob-nexus\" \"/n/7.h5\" \"--ob\" \"/ob/with space/Run_8_ob_1\" \"--ob-nexus\" \"/n/8.h5\"\n\
             [2026-09-10T18:10:00Z] done\n",
        )
        .unwrap();
        assert_eq!(
            launch_from_log(&log),
            Some(LaunchInfo {
                config: None,
                obs: vec![
                    PathBuf::from("/ob/Run_7_ob_0"),
                    PathBuf::from("/ob/with space/Run_8_ob_1"),
                ],
            })
        );
        // No running line / missing file → None.
        std::fs::write(&log, "[t] nothing here\n").unwrap();
        assert_eq!(launch_from_log(&log), None);
        assert_eq!(launch_from_log(&dir.join("missing.log")), None);
    }

    #[test]
    fn open_beams_of_a_real_job_log() {
        let log = Path::new(
            "/SNS/VENUS/IPTS-37705/shared/first_ct_attempt_v2/Run_29908/logs/normalization.log",
        );
        if !log.is_file() {
            return;
        }
        let info = launch_from_log(log).expect("the log names open beams");
        assert_eq!(info.obs.len(), 3);
        assert!(info.obs[0].ends_with("20260909_Run_29905_virgin_c_ob_1_185C_1_000AngsMin_ob_0"));
        assert!(info
            .config
            .is_some_and(|c| c.ends_with("normalization_config_20260910_102311_last.h5")));
    }

    #[test]
    fn output_is_done_needs_a_tiff() {
        let dir = std::env::temp_dir().join("anm_test_output_done");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(!output_is_done(&dir));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!output_is_done(&dir));
        std::fs::write(dir.join("logs.txt"), "x").unwrap();
        assert!(!output_is_done(&dir));
        std::fs::write(dir.join("Run_23642_0001.tiff"), "x").unwrap();
        assert!(output_is_done(&dir));
    }

    #[test]
    fn output_dir_follows_the_layout() {
        assert_eq!(
            output_dir(Path::new("/SNS/VENUS/IPTS-1"), 23644, 5),
            PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/autoreduce/normalized/rolling/anchor_23644/last_5min"
            )
        );
    }
}
