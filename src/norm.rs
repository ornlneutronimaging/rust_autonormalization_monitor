//! NeuNorm normalizations run through the VENUS workflow-runner script
//! (`normalize_tof.py`), in background threads:
//!
//! - **Per-run**: a run normalized on its own, with the configuration
//!   file's sample replaced by that run — automatically for every new run
//!   when auto normalization is ON (as soon as its detector-corrected
//!   folder shows up), on demand for older runs. Output follows the
//!   workflow runner: `<config output folder>/Run_<run>/normalization`
//!   (`<IPTS>/shared/autoreduce/normalized/Run_<run>/normalization` when
//!   the configuration names no output folder), so a run already
//!   normalized by either tool is found and not redone.
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
//! success.

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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// A PROGRESS line of the normalization script.
    Progress {
        target: JobTarget,
        stage: String,
        fraction: Option<f32>,
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

/// Output folder of one run's own normalization — the workflow runner's
/// layout under the configuration's output folder, so results are shared
/// between the tools: `<output folder>/Run_<run>/normalization`. Without
/// an output folder in the configuration, the same layout under
/// `<IPTS>/shared/autoreduce/normalized`.
pub fn run_output_dir(ipts_path: &Path, run: u64, config_info: &h5::ConfigInfo) -> PathBuf {
    let base = config_info
        .output_folder
        .clone()
        .unwrap_or_else(|| ipts_path.join("shared/autoreduce/normalized"));
    base.join(format!("Run_{run}")).join("normalization")
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
    /// Pre-crop region from the configuration, and where the cropped
    /// copies of the inputs go (`<base>/cropped_x0…_y1…/<folder name>`).
    crop: Option<((usize, usize, usize, usize), PathBuf)>,
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

    let anchor_run = *window.runs.iter().max().expect("runs not empty");
    let output = output_dir(ipts_path, anchor_run, window.minutes);
    let anchor_dir = output.parent().expect("window output has a parent").to_path_buf();
    Ok(JobSpec {
        target: JobTarget::Window(window_index),
        runs: window.runs.clone(),
        samples,
        obs,
        config: config_path.to_path_buf(),
        log: anchor_dir.join("logs").join(format!("last_{}min.log", window.minutes)),
        crop: config_info.crop_region.map(|region| {
            (
                region,
                crop_parent(&ipts_path.join("shared/autoreduce/normalized/rolling"), region),
            )
        }),
        output,
    })
}

/// Resolve one run's own normalization: the configuration's sample is
/// replaced by that run's corrected folder, everything else (open beams,
/// settings) comes from the configuration file.
pub fn prepare_run_job(
    run: u64,
    corrected_folder: &Path,
    ipts_path: &Path,
    config_path: &Path,
    config_info: &h5::ConfigInfo,
) -> Result<JobSpec, String> {
    let obs = resolve_obs(ipts_path, config_info)?;
    let output = run_output_dir(ipts_path, run, config_info);
    let row_dir = output.parent().expect("run output has a parent").to_path_buf();
    let base = row_dir.parent().expect("row dir has a parent").to_path_buf();
    Ok(JobSpec {
        target: JobTarget::Run(run),
        runs: vec![run],
        samples: vec![(
            corrected_folder.to_path_buf(),
            files::nexus_path(ipts_path, run),
        )],
        obs,
        config: config_path.to_path_buf(),
        log: row_dir.join("logs").join("normalization.log"),
        crop: config_info
            .crop_region
            .map(|region| (region, crop_parent(&base, region))),
        output,
    })
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

fn run_job(spec: &JobSpec, tx: &Sender<JobMessage>) -> Result<PathBuf, String> {
    let log = Log::open(&spec.log)?;
    let progress = |stage: String, fraction: Option<f32>| {
        let _ = tx.send(JobMessage::Progress {
            target: spec.target,
            stage,
            fraction,
        });
    };

    // Never run twice: a complete result is reused as is.
    if output_is_done(&spec.output) {
        log.line("normalization output already there — skipped (never run twice)");
        return Ok(spec.output.clone());
    }

    // Pre-crop every sample/OB folder when the configuration asks for it
    // (notebook convention), and normalize the cropped copies.
    let (mut samples, mut obs) = (spec.samples.clone(), spec.obs.clone());
    if let Some((region, parent)) = &spec.crop {
        let (x0, y0, x1, y1) = *region;
        log.line(&format!(
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
    log.line(&format!("running: {cmd:?}"));

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
        log.raw_line(&line);
        if !line.trim().is_empty() {
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
        let message = format!(
            "normalization script failed ({status})\n{}",
            tail.join("\n")
        );
        log.line(&format!("ERROR: {message}"));
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
    log.line(&format!("normalized data written to {}", spec.output.display()));
    Ok(spec.output.clone())
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

    #[test]
    fn run_output_dir_follows_the_workflow_runner_layout() {
        let with_folder = h5::ConfigInfo {
            ob_folders: vec![],
            crop_region: None,
            output_folder: Some(PathBuf::from("/SNS/VENUS/IPTS-1/shared/jean")),
        };
        assert_eq!(
            run_output_dir(Path::new("/SNS/VENUS/IPTS-1"), 23642, &with_folder),
            PathBuf::from("/SNS/VENUS/IPTS-1/shared/jean/Run_23642/normalization")
        );
        let without = h5::ConfigInfo {
            ob_folders: vec![],
            crop_region: None,
            output_folder: None,
        };
        assert_eq!(
            run_output_dir(Path::new("/SNS/VENUS/IPTS-1"), 23642, &without),
            PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/autoreduce/normalized/Run_23642/normalization"
            )
        );
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
        assert_eq!(spec.output, base.join("Run_23642/normalization"));
        assert_eq!(spec.log, base.join("Run_23642/logs/normalization.log"));
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
        // Second call: reused, never run twice.
        assert_eq!(run_job(&spec, &tx).unwrap(), output);
        assert!(std::fs::read_to_string(&spec.log).unwrap().contains("skipped"));
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
