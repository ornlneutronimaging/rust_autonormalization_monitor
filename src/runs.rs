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

/// Folder the reduction wrote the detector-efficiency-corrected data to,
/// parsed from the run's log file: the line `Writing data to <path>`.
/// The last such line wins if the log ever contains several.
pub fn data_folder_from_log(log_path: &Path) -> Result<PathBuf, String> {
    let content = fs::read_to_string(log_path)
        .map_err(|e| format!("cannot read {}: {e}", log_path.display()))?;
    content
        .lines()
        .filter_map(|line| {
            // Tolerate the historical "Writting" spelling.
            line.strip_prefix("Writing data to ")
                .or_else(|| line.strip_prefix("Writting data to "))
        })
        .last()
        .map(|path| PathBuf::from(path.trim()))
        .ok_or_else(|| {
            format!(
                "no 'Writing data to' line found in {}",
                log_path.display()
            )
        })
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
    fn extracts_data_folder_from_log() {
        let dir = std::env::temp_dir().join("anm_test_datafolder");
        fs::create_dir_all(&dir).unwrap();
        let log = dir.join("VENUS_1.nxs.h5.log");
        fs::write(
            &log,
            "Parsing input\nWriting data to /SNS/VENUS/IPTS-1/shared/autoreduce/images/run_1\nrun_number ='1'\n",
        )
        .unwrap();
        assert_eq!(
            data_folder_from_log(&log).unwrap(),
            PathBuf::from("/SNS/VENUS/IPTS-1/shared/autoreduce/images/run_1")
        );
        // No matching line → error.
        let empty = dir.join("VENUS_2.nxs.h5.log");
        fs::write(&empty, "Parsing input\n").unwrap();
        assert!(data_folder_from_log(&empty).is_err());
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
