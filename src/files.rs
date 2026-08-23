//! Run-list parsing and discovery of a run's files on disk.
//!
//! For a VENUS run `N` inside an IPTS folder (`/SNS/VENUS/IPTS-x`):
//! - NeXus:      `nexus/VENUS_N.nxs.h5` (a file, exact path)
//! - Raw:        a folder named `*_Run_N_*` somewhere under `images/`
//!               (e.g. `images/tpx1/raw/radiography/<title>/<date>_Run_N_<title>_<i>`)
//! - Corrected:  same folder-name pattern under `shared/autoreduce/images/`
//!               (the autoreduction mirrors the raw tree there)

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// How deep below the scan root a run folder can sit
/// (`tpx1/raw/radiography/<title>/<run folder>` = 5 levels).
const MAX_SCAN_DEPTH: usize = 6;

/// Parse a user-typed run list: comma/space separated numbers and `a-b`
/// ranges, e.g. `"23615-23618, 23642"`. Returns the sorted, de-duplicated
/// run numbers, or a message describing the first token that failed.
pub fn parse_run_list(text: &str) -> Result<Vec<u64>, String> {
    let mut runs: Vec<u64> = Vec::new();
    for token in text.split(|c: char| c == ',' || c.is_whitespace()) {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if let Some((a, b)) = token.split_once('-') {
            let (a, b) = (
                a.trim().parse::<u64>().map_err(|_| bad(token))?,
                b.trim().parse::<u64>().map_err(|_| bad(token))?,
            );
            if a > b || b - a > 10_000 {
                return Err(format!("invalid range '{token}'"));
            }
            runs.extend(a..=b);
        } else {
            runs.push(token.parse::<u64>().map_err(|_| bad(token))?);
        }
    }
    runs.sort_unstable();
    runs.dedup();
    Ok(runs)
}

fn bad(token: &str) -> String {
    format!("cannot parse '{token}' — expected run numbers like 23642 or 23615-23620")
}

/// Extract the run number out of a folder name like
/// `20260613_Run_23642_LF99D_Rnd2_..._0` (`Run_<digits>` token; the digits
/// must end at a non-digit so `Run_2364` never matches run 23642).
pub fn run_number_in_name(name: &str) -> Option<u64> {
    let idx = name.find("Run_")?;
    let digits: String = name[idx + 4..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Walk `root` (depth-limited) and map each wanted run number to the first
/// folder found whose name carries it. Matched folders are not descended
/// into. A missing/unreadable root simply yields an empty map.
pub fn scan_run_dirs(root: &Path, wanted: &HashSet<u64>) -> HashMap<u64, PathBuf> {
    let mut found = HashMap::new();
    if !wanted.is_empty() {
        walk(root, 0, wanted, &mut found);
    }
    found
}

fn walk(dir: &Path, depth: usize, wanted: &HashSet<u64>, found: &mut HashMap<u64, PathBuf>) {
    if depth > MAX_SCAN_DEPTH || found.len() == wanted.len() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with('.') {
            continue;
        }
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        match run_number_in_name(&name_str) {
            Some(run) if wanted.contains(&run) => {
                found.entry(run).or_insert_with(|| entry.path());
            }
            // A run folder we don't care about: no need to descend into it.
            Some(_) => {}
            None => walk(&entry.path(), depth + 1, wanted, found),
        }
    }
}

/// Path of a run's NeXus file inside an IPTS folder.
pub fn nexus_path(ipts_path: &Path, run: u64) -> PathBuf {
    ipts_path.join(format!("nexus/VENUS_{run}.nxs.h5"))
}

/// Every run number present in `<IPTS>/nexus` (`VENUS_<run>.nxs.h5` files),
/// sorted ascending. Missing/unreadable folder → empty list.
pub fn list_nexus_runs(ipts_path: &Path) -> Vec<u64> {
    let Ok(dir) = std::fs::read_dir(ipts_path.join("nexus")) else {
        return Vec::new();
    };
    let mut runs: Vec<u64> = dir
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            name_str
                .strip_prefix("VENUS_")?
                .strip_suffix(".nxs.h5")?
                .parse()
                .ok()
        })
        .collect();
    runs.sort_unstable();
    runs
}

/// Highest run number already present in `<IPTS>/nexus` — the next
/// acquired run will be this + 1. `None` when the folder is missing,
/// empty, or holds no VENUS NeXus file.
pub fn latest_nexus_run(ipts_path: &Path) -> Option<u64> {
    list_nexus_runs(ipts_path).last().copied()
}

/// Presence of one of a run's files/folders on disk.
#[derive(Clone, Debug)]
pub enum FileStatus {
    /// The file/folder exists at this path.
    Present(PathBuf),
    /// Not found; the path is where it was expected / searched for.
    Missing(PathBuf),
}

/// Status of every tracked file of one run.
#[derive(Clone, Debug)]
pub struct RunFiles {
    pub run: u64,
    pub nexus: FileStatus,
    pub raw: FileStatus,
    pub corrected: FileStatus,
}

/// Check NeXus/raw/corrected for every run in `runs`, inside the IPTS
/// folder `ipts_path` (e.g. `/SNS/VENUS/IPTS-36967`).
pub fn check_runs(ipts_path: &Path, runs: &[u64]) -> Vec<RunFiles> {
    let wanted: HashSet<u64> = runs.iter().copied().collect();
    let raw_root = ipts_path.join("images");
    let corrected_root = ipts_path.join("shared/autoreduce/images");
    let raw_dirs = scan_run_dirs(&raw_root, &wanted);
    let corrected_dirs = scan_run_dirs(&corrected_root, &wanted);
    runs.iter()
        .map(|&run| {
            let nexus = ipts_path.join(format!("nexus/VENUS_{run}.nxs.h5"));
            RunFiles {
                run,
                nexus: if nexus.is_file() {
                    FileStatus::Present(nexus)
                } else {
                    FileStatus::Missing(nexus)
                },
                raw: dir_status(&raw_dirs, run, &raw_root),
                corrected: dir_status(&corrected_dirs, run, &corrected_root),
            }
        })
        .collect()
}

fn dir_status(found: &HashMap<u64, PathBuf>, run: u64, root: &Path) -> FileStatus {
    match found.get(&run) {
        Some(path) => FileStatus::Present(path.clone()),
        None => FileStatus::Missing(root.join(format!("**/*Run_{run}*"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parses_run_lists() {
        assert_eq!(parse_run_list("23642").unwrap(), vec![23642]);
        assert_eq!(
            parse_run_list("23615-23618, 23642").unwrap(),
            vec![23615, 23616, 23617, 23618, 23642]
        );
        // Spaces as separators, duplicates removed, output sorted.
        assert_eq!(parse_run_list("5 3 4-5").unwrap(), vec![3, 4, 5]);
        assert_eq!(parse_run_list("").unwrap(), Vec::<u64>::new());
        assert!(parse_run_list("abc").is_err());
        assert!(parse_run_list("10-5").is_err());
        assert!(parse_run_list("1-999999999").is_err());
    }

    #[test]
    fn extracts_run_number_from_folder_names() {
        assert_eq!(
            run_number_in_name("20260613_Run_23642_LF99D_Rnd2_Coarsen_0"),
            Some(23642)
        );
        assert_eq!(run_number_in_name("Run_7"), Some(7));
        assert_eq!(run_number_in_name("no_run_here"), None);
        assert_eq!(run_number_in_name("Run_"), None);
    }

    #[test]
    fn scans_nested_run_folders() {
        let root = std::env::temp_dir().join("anm_test_scan");
        let _ = fs::remove_dir_all(&root);
        let deep = root.join("tpx1/raw/radiography/20260613_title");
        fs::create_dir_all(deep.join("20260613_Run_23642_title_0")).unwrap();
        fs::create_dir_all(deep.join("20260613_Run_23643_title_1")).unwrap();
        let wanted: HashSet<u64> = [23642, 99999].into_iter().collect();
        let found = scan_run_dirs(&root, &wanted);
        assert_eq!(
            found.get(&23642),
            Some(&deep.join("20260613_Run_23642_title_0"))
        );
        // 23643 exists but was not asked for; 99999 does not exist.
        assert!(!found.contains_key(&23643));
        assert!(!found.contains_key(&99999));
        // Missing root: empty map, no error.
        assert!(scan_run_dirs(&root.join("nope"), &wanted).is_empty());
    }

    #[test]
    fn finds_the_latest_nexus_run() {
        let ipts = std::env::temp_dir().join("anm_test_latest_nexus");
        let _ = fs::remove_dir_all(&ipts);
        // Missing / empty nexus folder → None.
        assert_eq!(latest_nexus_run(&ipts), None);
        fs::create_dir_all(ipts.join("nexus")).unwrap();
        assert_eq!(latest_nexus_run(&ipts), None);
        fs::write(ipts.join("nexus/VENUS_23641.nxs.h5"), "x").unwrap();
        fs::write(ipts.join("nexus/VENUS_23642.nxs.h5"), "x").unwrap();
        fs::write(ipts.join("nexus/unrelated.txt"), "x").unwrap();
        assert_eq!(latest_nexus_run(&ipts), Some(23642));
    }

    #[test]
    fn checks_runs_inside_an_ipts_layout() {
        let ipts = std::env::temp_dir().join("anm_test_ipts_layout");
        let _ = fs::remove_dir_all(&ipts);
        fs::create_dir_all(ipts.join("nexus")).unwrap();
        fs::write(ipts.join("nexus/VENUS_11.nxs.h5"), "x").unwrap();
        fs::create_dir_all(ipts.join("images/tpx1/raw/radiography/t/20260101_Run_11_t_0"))
            .unwrap();
        fs::create_dir_all(
            ipts.join("shared/autoreduce/images/tpx1/raw/radiography/t/20260101_Run_12_t_0"),
        )
        .unwrap();

        let status = check_runs(&ipts, &[11, 12]);
        assert_eq!(status.len(), 2);
        assert!(matches!(status[0].nexus, FileStatus::Present(_)));
        assert!(matches!(status[0].raw, FileStatus::Present(_)));
        assert!(matches!(status[0].corrected, FileStatus::Missing(_)));
        assert!(matches!(status[1].nexus, FileStatus::Missing(_)));
        assert!(matches!(status[1].raw, FileStatus::Missing(_)));
        assert!(matches!(status[1].corrected, FileStatus::Present(_)));
    }
}
