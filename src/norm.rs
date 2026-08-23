//! Rolling-window combine normalization: pick the runs acquired within the
//! last N minutes (acquisition time from the NeXus `end_time`), then run
//! them together through NeuNorm via the VENUS workflow-runner script
//! (`normalize_tof.py`), one job per time window.
//!
//! Sample inputs are the runs' detector-corrected folders; open-beam
//! folders come from the normalization configuration file. Each job writes
//! into `<IPTS>/shared/autoreduce/normalized/rolling/anchor_<run>/last_<N>min`
//! (staged in a `.partial` folder, promoted on success — workflow-runner
//! convention).

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

/// Lifecycle of one window's normalization job.
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
    },
}

/// Messages a job thread sends while it runs and when it ends.
pub enum JobMessage {
    /// A PROGRESS line of the normalization script.
    Progress {
        window_index: usize,
        stage: String,
        fraction: Option<f32>,
    },
    Finished {
        window_index: usize,
        runs: Vec<u64>,
        result: Result<PathBuf, String>,
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

/// Everything a normalization job needs, resolved up-front so problems are
/// reported before any thread is spawned.
pub struct JobSpec {
    pub window_index: usize,
    pub minutes: u32,
    pub runs: Vec<u64>,
    /// (corrected folder, NeXus file) per sample run.
    samples: Vec<(PathBuf, PathBuf)>,
    /// (corrected folder, NeXus file) per open-beam run.
    obs: Vec<(PathBuf, PathBuf)>,
    config: PathBuf,
    output: PathBuf,
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
    if config_info.has_crop {
        return Err(
            "the configuration has a crop region — not supported here yet \
             (use the workflow runner)"
                .to_owned(),
        );
    }
    if config_info.ob_folders.is_empty() {
        return Err("the configuration file names no open-beam folder".to_owned());
    }

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

    // Open beams: folder from the config; NeXus derived from the folder
    // (run number in its name, IPTS from its path — OBs may live in
    // another IPTS than the samples).
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

    let anchor_run = *window.runs.iter().max().expect("runs not empty");
    Ok(JobSpec {
        window_index,
        minutes: window.minutes,
        runs: window.runs.clone(),
        samples,
        obs,
        config: config_path.to_path_buf(),
        output: output_dir(ipts_path, anchor_run, window.minutes),
    })
}

/// Launch a prepared job in a background thread; progress and the outcome
/// arrive on `tx` as [`JobMessage`]s. The job stages into
/// `<output>.partial` and promotes to `<output>` on success.
pub fn launch(spec: JobSpec, tx: Sender<JobMessage>) {
    std::thread::spawn(move || {
        let result = run_job(&spec, &tx);
        let _ = tx.send(JobMessage::Finished {
            window_index: spec.window_index,
            runs: spec.runs.clone(),
            result,
        });
    });
}

fn run_job(spec: &JobSpec, tx: &Sender<JobMessage>) -> Result<PathBuf, String> {
    let partial = spec.output.with_extension("partial");
    // A leftover staging folder from a crashed run would confuse the
    // promote step: start clean.
    let _ = std::fs::remove_dir_all(&partial);
    std::fs::create_dir_all(&partial)
        .map_err(|e| format!("cannot create {}: {e}", partial.display()))?;

    let basename = spec
        .output
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("last_{}min", spec.minutes));
    let mut cmd = std::process::Command::new(PYTHON_BIN);
    cmd.arg(NORMALIZE_SCRIPT)
        .arg("--config")
        .arg(&spec.config)
        .arg("--output")
        .arg(&partial)
        .arg("--basename")
        .arg(&basename);
    for (folder, nexus) in &spec.samples {
        cmd.arg("--sample").arg(folder).arg("--sample-nexus").arg(nexus);
    }
    for (folder, nexus) in &spec.obs {
        cmd.arg("--ob").arg(folder).arg("--ob-nexus").arg(nexus);
    }

    // Stream the script's output: its PROGRESS lines feed the status
    // column's progress bar while everything is kept for the error tail.
    use std::io::BufRead;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot launch {PYTHON_BIN}: {e}"))?;
    let stderr_lines = child.stderr.take().map(|stderr| {
        std::thread::spawn(move || {
            std::io::BufReader::new(stderr)
                .lines()
                .map_while(Result::ok)
                .collect::<Vec<String>>()
        })
    });
    let mut stdout_lines: Vec<String> = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some((stage, fraction)) = parse_progress(&line) {
                let _ = tx.send(JobMessage::Progress {
                    window_index: spec.window_index,
                    stage,
                    fraction,
                });
            }
            stdout_lines.push(line);
        }
    }
    let status = child
        .wait()
        .map_err(|e| format!("cannot wait for the normalization: {e}"))?;
    if !status.success() {
        // The script prints its error last — keep the tail for the UI.
        let mut all = stdout_lines;
        if let Some(handle) = stderr_lines {
            all.extend(handle.join().unwrap_or_default());
        }
        let tail: Vec<&String> = all
            .iter()
            .filter(|l| !l.trim().is_empty() && !l.starts_with("PROGRESS"))
            .collect();
        let n = tail.len();
        return Err(tail[n.saturating_sub(6)..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n"));
    }

    // Promote the staging folder to the final name.
    let _ = std::fs::remove_dir_all(&spec.output);
    std::fs::rename(&partial, &spec.output).map_err(|e| {
        format!(
            "normalized, but cannot rename {} -> {}: {e}",
            partial.display(),
            spec.output.display()
        )
    })?;
    Ok(spec.output.clone())
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
    fn output_dir_follows_the_layout() {
        assert_eq!(
            output_dir(Path::new("/SNS/VENUS/IPTS-1"), 23644, 5),
            PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/autoreduce/normalized/rolling/anchor_23644/last_5min"
            )
        );
    }
}
