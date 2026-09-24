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

/// Extract the starting wavelength (Å) the DAQ bakes into a run's folder
/// name, e.g. `..._2_900C_3_000AngsMin_...` → `3.000` (the `AngsMin`
/// token; the underscore between its two digit groups stands for the
/// decimal point — not to be confused with an unrelated digit group
/// earlier in the name, like a temperature such as `2_900C` here).
/// `None` when the name carries no such token.
/// The acquisition name of a run folder — what the DAQ was told to call
/// the measurement — without the `<date>_Run_<run>_` prefix and the
/// trailing `_<index>` (the run's rank among those of the same name):
/// `20260921_Run_30340_reptRib_PFV468_2_900C_3_000AngsMin_0` →
/// `reptRib_PFV468_2_900C_3_000AngsMin`. Consecutive runs of one series
/// share it (title, sample environment setpoint, chopper setting); a
/// different sample or setpoint changes it. `None` when the name does not
/// follow the pattern.
pub fn acquisition_name(name: &str) -> Option<String> {
    let idx = name.find("_Run_")?;
    let after = &name[idx + "_Run_".len()..];
    let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return None;
    }
    let rest = after[digits..].strip_prefix('_')?;
    let trimmed = match rest.rfind('_') {
        Some(i) if !rest[i + 1..].is_empty() && rest[i + 1..].chars().all(|c| c.is_ascii_digit()) => {
            &rest[..i]
        }
        _ => rest,
    };
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

pub fn starting_wavelength_in_name(name: &str) -> Option<f64> {
    let idx = name.find("AngsMin")?;
    let prefix = &name[..idx];
    let frac_start = prefix.rfind(|c: char| !c.is_ascii_digit())?;
    if prefix.as_bytes()[frac_start] != b'_' {
        return None;
    }
    let frac = &prefix[frac_start + 1..];
    if frac.is_empty() {
        return None;
    }
    let before = &prefix[..frac_start];
    let int_start = before.rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
    let int_part = &before[int_start..];
    if int_part.is_empty() {
        return None;
    }
    format!("{int_part}.{frac}").parse().ok()
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
    /// The file exists / the folder is complete at this path.
    Present(PathBuf),
    /// The folder exists but is still being written: no image yet, or the
    /// end-of-run sidecar (`*_Spectra.txt` for a raw folder, `summary.json`
    /// for a corrected one — written last) is not there yet. Not usable as
    /// an input.
    Writing(PathBuf),
    /// Not found; the path is where it was expected / searched for.
    Missing(PathBuf),
}

/// Which run folder a completeness check looks at — each has its own
/// image format and end-of-run files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderKind {
    /// DAQ output (`<IPTS>/images`): one `.fits` per frame plus the
    /// integrated `*_SummedImg.fits`; the `*_Spectra.txt` (no header, one
    /// row per frame) lands last.
    Raw,
    /// Reduction output (`shared/autoreduce/images`): one `.tif` per frame;
    /// its `*_Spectra.txt` has a header line, then one row per frame;
    /// `summary.json` lands last.
    Corrected,
}

/// Is a run folder complete? The DAQ / the reduction create the folder
/// first, write the frames, then the sidecars. Complete = the Spectra file
/// is there and its number of data rows equals the number of frames (raw:
/// `.fits` files minus the SummedImg one; corrected: `.tif` files) — and,
/// for a corrected folder, `summary.json` (written last) is there too.
pub fn folder_complete(folder: &Path, kind: FolderKind) -> bool {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return false;
    };
    let mut frames = 0usize;
    let mut spectra: Option<PathBuf> = None;
    let mut summary = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        match kind {
            FolderKind::Raw => {
                if name.ends_with(".fits") && !name.contains("summedimg") {
                    frames += 1;
                }
            }
            FolderKind::Corrected => {
                if name.ends_with(".tif") || name.ends_with(".tiff") {
                    frames += 1;
                } else if name == "summary.json" {
                    summary = true;
                }
            }
        }
        if name.ends_with("_spectra.txt") && spectra.is_none() {
            spectra = Some(entry.path());
        }
    }
    if frames == 0 || (kind == FolderKind::Corrected && !summary) {
        return false;
    }
    let Some(spectra) = spectra else {
        return false;
    };
    spectra_rows(&spectra) == Some(frames)
}

/// Number of TIFF images in a folder (0 when unreadable).
pub fn tiff_count(folder: &Path) -> usize {
    std::fs::read_dir(folder)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_lowercase();
                    name.ends_with(".tif") || name.ends_with(".tiff")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Number of data rows of a `*_Spectra.txt` file (header lines — anything
/// not starting with a digit, sign or dot — and blank lines excluded).
pub fn spectra_rows(path: &Path) -> Option<usize> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(
        text.lines()
            .filter(|l| {
                l.trim_start()
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit() || c == '-' || c == '.')
            })
            .count(),
    )
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
                raw: dir_status(&raw_dirs, run, &raw_root, FolderKind::Raw),
                corrected: dir_status(&corrected_dirs, run, &corrected_root, FolderKind::Corrected),
            }
        })
        .collect()
}

fn dir_status(
    found: &HashMap<u64, PathBuf>,
    run: u64,
    root: &Path,
    kind: FolderKind,
) -> FileStatus {
    match found.get(&run) {
        Some(path) if folder_complete(path, kind) => FileStatus::Present(path.clone()),
        Some(path) => FileStatus::Writing(path.clone()),
        None => FileStatus::Missing(root.join(format!("**/*Run_{run}*"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquisition_name_drops_the_run_prefix_and_the_index() {
        assert_eq!(
            acquisition_name("20260921_Run_30340_reptRib_PFV468_2_900C_3_000AngsMin_0").as_deref(),
            Some("reptRib_PFV468_2_900C_3_000AngsMin")
        );
        assert_eq!(
            acquisition_name("20260613_Run_23642_LF99D_Rnd2_Coarsen_0_416C_0_000AngsMin_12").as_deref(),
            Some("LF99D_Rnd2_Coarsen_0_416C_0_000AngsMin")
        );
        // Open beams have the `_ob_<i>` tail: the `_<i>` goes, `ob` stays.
        assert_eq!(
            acquisition_name("20260921_Run_30339_ob__2_900C_3_000AngsMin_ob_0").as_deref(),
            Some("ob__2_900C_3_000AngsMin_ob")
        );
        assert_eq!(acquisition_name("Run_7"), None);
        assert_eq!(acquisition_name("something_else"), None);
    }
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
    fn extracts_starting_wavelength_from_folder_names() {
        // Real IPTS-38902 names: an unrelated digit group (the "2_900C"
        // temperature) sits right before the wavelength token and must
        // not be picked up instead.
        assert_eq!(
            starting_wavelength_in_name("20260921_Run_30338_ob__2_900C_0_700AngsMin_ob_0"),
            Some(0.7)
        );
        assert_eq!(
            starting_wavelength_in_name("20260921_Run_30339_ob__2_900C_3_000AngsMin_ob_0"),
            Some(3.0)
        );
        assert_eq!(
            starting_wavelength_in_name(
                "20260921_Run_30340_reptVertMx_PFV089_2_900C_3_000AngsMin_0"
            ),
            Some(3.0)
        );
        // Multi-digit integer part, no leading token.
        assert_eq!(starting_wavelength_in_name("12_500AngsMin"), Some(12.5));
        // No token at all.
        assert_eq!(starting_wavelength_in_name("20260430_OB_RT_1_393C_no_token_here"), None);
        assert_eq!(starting_wavelength_in_name("AngsMin"), None);
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

    // Reads real shared folders; passes trivially where VENUS is not mounted.
    #[test]
    fn real_run_folders_are_complete() {
        let ipts = Path::new("/SNS/VENUS/IPTS-37705");
        if !ipts.join("nexus/VENUS_29909.nxs.h5").is_file() {
            return;
        }
        let status = check_runs(ipts, &[29909]).remove(0);
        assert!(matches!(status.raw, FileStatus::Present(_)), "{:?}", status.raw);
        assert!(
            matches!(status.corrected, FileStatus::Present(_)),
            "{:?}",
            status.corrected
        );
    }

    #[test]
    fn checks_runs_inside_an_ipts_layout() {
        let ipts = std::env::temp_dir().join("anm_test_ipts_layout");
        let _ = fs::remove_dir_all(&ipts);
        fs::create_dir_all(ipts.join("nexus")).unwrap();
        fs::write(ipts.join("nexus/VENUS_11.nxs.h5"), "x").unwrap();
        let raw11 = ipts.join("images/tpx1/raw/radiography/t/20260101_Run_11_t_0");
        fs::create_dir_all(&raw11).unwrap();
        // Complete raw folder: 2 frames + the SummedImg, a headerless
        // Spectra file with 2 rows.
        fs::write(raw11.join("20260101_Run_11_t_0_00000.fits"), "x").unwrap();
        fs::write(raw11.join("20260101_Run_11_t_0_00001.fits"), "x").unwrap();
        fs::write(raw11.join("20260101_Run_11_t_0_SummedImg.fits"), "x").unwrap();
        fs::write(raw11.join("20260101_Run_11_t_0_Spectra.txt"), "0.0003\t0\r\n0.0004\t5\r\n")
            .unwrap();
        let corr12 =
            ipts.join("shared/autoreduce/images/tpx1/raw/radiography/t/20260101_Run_12_t_0");
        fs::create_dir_all(&corr12).unwrap();
        // Still being written: one of two images and the Spectra file
        // (header + 2 rows), no summary.json yet.
        fs::write(corr12.join("20260101_Run_12_t_0_00000.tif"), "x").unwrap();
        fs::write(
            corr12.join("20260101_Run_12_t_0_Spectra.txt"),
            "shutter_time,counts\n3.0e-4,0\n4.0e-4,5\n",
        )
        .unwrap();

        let status = check_runs(&ipts, &[11, 12]);
        assert_eq!(status.len(), 2);
        assert!(matches!(status[0].nexus, FileStatus::Present(_)));
        assert!(matches!(status[0].raw, FileStatus::Present(_)));
        assert!(matches!(status[0].corrected, FileStatus::Missing(_)));
        assert!(matches!(status[1].nexus, FileStatus::Missing(_)));
        assert!(matches!(status[1].raw, FileStatus::Missing(_)));
        assert!(matches!(status[1].corrected, FileStatus::Writing(_)));
        // The reduction finishes: summary.json lands, but one image is
        // still missing versus the Spectra rows → not complete.
        fs::write(corr12.join("summary.json"), "{}").unwrap();
        assert!(!folder_complete(&corr12, FolderKind::Corrected));
        fs::write(corr12.join("20260101_Run_12_t_0_00001.tif"), "x").unwrap();
        let status = check_runs(&ipts, &[12]);
        assert!(matches!(status[0].corrected, FileStatus::Present(_)));
        // An empty folder is being written too, not complete.
        let empty = ipts.join("shared/autoreduce/images/tpx1/raw/radiography/t/20260101_Run_13_t_0");
        fs::create_dir_all(&empty).unwrap();
        assert!(!folder_complete(&empty, FolderKind::Corrected));
        // Raw: frames (SummedImg excluded) must match the rows; one frame
        // short → still writing.
        assert!(folder_complete(&raw11, FolderKind::Raw));
        fs::remove_file(raw11.join("20260101_Run_11_t_0_00001.fits")).unwrap();
        assert!(!folder_complete(&raw11, FolderKind::Raw));
        assert_eq!(spectra_rows(&raw11.join("20260101_Run_11_t_0_Spectra.txt")), Some(2));
        assert_eq!(spectra_rows(&corr12.join("20260101_Run_12_t_0_Spectra.txt")), Some(2));
    }
}
