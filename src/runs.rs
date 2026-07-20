//! Discovery of reduced runs from the autoreduction log folder
//! (`/SNS/VENUS/<ipts>/shared/autoreduce/reduction_log`).
//!
//! Each reduced run leaves `VENUS_<run>.nxs.h5.log` (and, when something went
//! wrong, `VENUS_<run>.nxs.h5.err`) in that folder. Runs are grouped by run
//! number and ordered by the modification time of their files, newest first.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// One reduced run: its number and the log/error files found for it.
#[derive(Clone, Debug)]
pub struct RunEntry {
    pub run_number: u64,
    pub log_path: Option<PathBuf>,
    pub err_path: Option<PathBuf>,
    /// Most recent modification time among the run's files (sort key).
    pub mtime: SystemTime,
}

/// Parse `VENUS_20343.nxs.h5.err` → `(20343, "err")`.
fn parse_file_name(name: &str) -> Option<(u64, &str)> {
    let rest = name.strip_prefix("VENUS_")?;
    let (run, suffix) = rest.split_once(".nxs.h5.")?;
    Some((run.parse().ok()?, suffix))
}

/// Scan `log_dir` and return the `count` most recently modified runs,
/// newest first.
pub fn last_runs(log_dir: &Path, count: usize) -> Result<Vec<RunEntry>, String> {
    let dir = fs::read_dir(log_dir)
        .map_err(|e| format!("cannot read {}: {e}", log_dir.display()))?;
    let mut by_run: HashMap<u64, RunEntry> = HashMap::new();
    for entry in dir.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        let Some((run_number, suffix)) = parse_file_name(&name_str) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        let mtime = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        let run = by_run.entry(run_number).or_insert_with(|| RunEntry {
            run_number,
            log_path: None,
            err_path: None,
            mtime,
        });
        if mtime > run.mtime {
            run.mtime = mtime;
        }
        match suffix {
            "log" => run.log_path = Some(entry.path()),
            "err" => run.err_path = Some(entry.path()),
            _ => {}
        }
    }
    let mut runs: Vec<RunEntry> = by_run.into_values().collect();
    runs.sort_by(|a, b| b.mtime.cmp(&a.mtime).then(b.run_number.cmp(&a.run_number)));
    runs.truncate(count);
    Ok(runs)
}

/// Data folders a reduction reported in its log file.
#[derive(Clone, Debug, Default)]
pub struct LogFolders {
    /// Detector-efficiency-corrected data: `Writing data to <path>`.
    pub corrected: Option<PathBuf>,
    /// Normalized data (written by the normalization tool):
    /// `Writing normalized data to <path>`.
    pub normalized: Option<PathBuf>,
}

/// Parse the data-folder lines out of a run's log file. For each kind the
/// last matching line wins if the log ever contains several. A missing or
/// unreadable log yields empty folders.
pub fn folders_from_log(log_path: &Path) -> LogFolders {
    let Ok(content) = fs::read_to_string(log_path) else {
        return LogFolders::default();
    };
    let mut folders = LogFolders::default();
    for line in content.lines() {
        // Tolerate the historical "Writting" spelling.
        if let Some(path) = line
            .strip_prefix("Writing normalized data to ")
            .or_else(|| line.strip_prefix("Writting normalized data to "))
        {
            folders.normalized = Some(PathBuf::from(path.trim()));
        } else if let Some(path) = line
            .strip_prefix("Writing data to ")
            .or_else(|| line.strip_prefix("Writting data to "))
        {
            folders.corrected = Some(PathBuf::from(path.trim()));
        }
    }
    folders
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_log_and_err_names() {
        assert_eq!(parse_file_name("VENUS_20343.nxs.h5.err"), Some((20343, "err")));
        assert_eq!(parse_file_name("VENUS_23642.nxs.h5.log"), Some((23642, "log")));
        assert_eq!(parse_file_name("VENUS_23642.nxs.h5"), None);
        assert_eq!(parse_file_name("SNAP_1.nxs.h5.log"), None);
        assert_eq!(parse_file_name("VENUS_abc.nxs.h5.log"), None);
    }

    #[test]
    fn extracts_data_folders_from_log() {
        let dir = std::env::temp_dir().join("anm_test_datafolder");
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("VENUS_1.nxs.h5.log");
        fs::write(
            &log,
            "Parsing input\n\
             Writing data to /SNS/VENUS/IPTS-1/shared/autoreduce/images/run_1\n\
             Writing normalized data to /SNS/VENUS/IPTS-1/shared/autoreduce/normalized/run_1\n\
             run_number ='1'\n",
        )
        .unwrap();
        let folders = folders_from_log(&log);
        assert_eq!(
            folders.corrected,
            Some(PathBuf::from("/SNS/VENUS/IPTS-1/shared/autoreduce/images/run_1"))
        );
        assert_eq!(
            folders.normalized,
            Some(PathBuf::from(
                "/SNS/VENUS/IPTS-1/shared/autoreduce/normalized/run_1"
            ))
        );
        // Not-normalized run: only the corrected folder is present.
        let plain = dir.join("VENUS_2.nxs.h5.log");
        fs::write(&plain, "Writing data to /tmp/x\n").unwrap();
        let folders = folders_from_log(&plain);
        assert_eq!(folders.corrected, Some(PathBuf::from("/tmp/x")));
        assert!(folders.normalized.is_none());
        // No matching lines / unreadable log → empty folders.
        assert!(folders_from_log(&dir.join("missing.log")).corrected.is_none());
    }

    #[test]
    fn groups_files_and_keeps_most_recent_runs() {
        let dir = std::env::temp_dir().join("anm_test_runs");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 25 runs with only a .log; run 5 also has an .err file.
        for run in 1..=25u64 {
            fs::write(dir.join(format!("VENUS_{run}.nxs.h5.log")), "log").unwrap();
        }
        fs::write(dir.join("VENUS_25.nxs.h5.err"), "err").unwrap();
        fs::write(dir.join("unrelated.txt"), "x").unwrap();

        let runs = last_runs(&dir, 20).unwrap();
        assert_eq!(runs.len(), 20);
        // All files share ~the same mtime, so the run-number tiebreak applies:
        // the 20 highest run numbers survive, newest (highest) first.
        assert_eq!(runs[0].run_number, 25);
        assert!(runs.iter().all(|r| r.run_number >= 6));
        // Run 25's .log and .err were grouped into a single entry.
        assert!(runs[0].log_path.is_some());
        assert!(runs[0].err_path.is_some());
        let run6 = runs.iter().find(|r| r.run_number == 6).unwrap();
        assert!(run6.log_path.is_some());
        assert!(run6.err_path.is_none());
    }
}
