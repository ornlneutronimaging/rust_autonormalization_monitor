//! Minimal HDF5 reading: acquisition times and the kind (sample, open
//! beam, alignment) of a VENUS NeXus file, and the open-beam entries of a
//! normalization session configuration — plus the one write this tool
//! does: a copy of a configuration with its open beams replaced. The tolerant
//! string reading follows `rust_nexus_viewer`'s `h5io` module (the proven
//! way to read h5py-written files with `hdf5-metno`).

use chrono::{DateTime, FixedOffset};
use hdf5_metno as h5;
use hdf5_metno::types::TypeDescriptor;
use std::path::{Path, PathBuf};

/// Fixed-length strings longer than this are not supported (HDF5 only
/// converts fixed -> fixed strings, so they go through a fixed buffer).
const MAX_FIXED_STR: usize = 4096;

/// All strings of a dataset/attribute, whatever their HDF5 string flavor.
fn read_strings(c: &h5::Container) -> Option<Vec<String>> {
    if c.size() == 0 {
        return None;
    }
    let td = c.dtype().and_then(|d| d.to_descriptor()).ok()?;
    use TypeDescriptor::*;
    match td {
        VarLenAscii => c
            .read_raw::<h5::types::VarLenAscii>()
            .ok()
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
        VarLenUnicode => c
            .read_raw::<h5::types::VarLenUnicode>()
            .ok()
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
        FixedAscii(n) if n <= MAX_FIXED_STR => c
            .read_raw::<h5::types::FixedAscii<MAX_FIXED_STR>>()
            .ok()
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
        FixedUnicode(n) if n <= MAX_FIXED_STR => c
            .read_raw::<h5::types::FixedUnicode<MAX_FIXED_STR>>()
            .ok()
            .map(|v| v.iter().map(|s| s.to_string()).collect()),
        _ => None,
    }
}

fn dataset_strings(file: &h5::File, path: &str) -> Option<Vec<String>> {
    let ds = file.dataset(path).ok()?;
    read_strings(&ds)
}

fn dataset_string(file: &h5::File, path: &str) -> Option<String> {
    dataset_strings(file, path)?.into_iter().next()
}

/// Acquisition interval of a run, from `/entry/start_time` and
/// `/entry/end_time` of its NeXus file (RFC-3339 strings, e.g.
/// `2026-06-13T13:49:50.064875667-04:00`). `None` when the file is
/// missing, still being written, or has no parsable times.
pub fn nexus_times(path: &Path) -> Option<(DateTime<FixedOffset>, DateTime<FixedOffset>)> {
    let file = h5::File::open(path).ok()?;
    let start = dataset_string(&file, "entry/start_time")?;
    let end = dataset_string(&file, "entry/end_time")?;
    Some((
        DateTime::parse_from_rfc3339(start.trim()).ok()?,
        DateTime::parse_from_rfc3339(end.trim()).ok()?,
    ))
}

/// DASlogs entry recording where the DAQ wrote the run's images,
/// relative to the IPTS folder (e.g. `images/tpx1/raw/radiography/<title>/
/// <run folder>`, or `images/tpx1/alignment/…` for an alignment run).
const IMAGE_FILE_PATH_LOG: &str = "entry/DASlogs/BL10:Exp:IM:ImageFilePath/value";

/// Where the DAQ wrote the run's images, relative to the IPTS folder,
/// from the `BL10:Exp:IM:ImageFilePath` log of its NeXus file. The log
/// is a time series: its first value is usually the PREVIOUS run's folder
/// (the value at the start of the run, before the DAQ set the new one), so
/// the last value is the run's own. Trailing padding of the fixed-length
/// string removed. `None` when the file is missing, still being written,
/// or has no such log.
pub fn nexus_image_path(path: &Path) -> Option<String> {
    let file = h5::File::open(path).ok()?;
    dataset_strings(&file, IMAGE_FILE_PATH_LOG)?
        .into_iter()
        .rev()
        .map(|s| s.trim().to_owned())
        .find(|s| !s.is_empty())
}

/// What a run is, per the image folder its NeXus records. The DAQ files
/// alignment runs under an `alignment` folder of the IPTS `images` tree
/// and open beams under an `ob` folder (samples under `raw/…`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RunKind {
    /// A regular (sample) run — the only kind that gets normalized.
    Sample,
    /// An open-beam run: the DAQ files it under an `ob` folder of the
    /// IPTS `images` tree. It is corrected by the autoreduction like any
    /// run, but never normalized: it is a normalization INPUT.
    OpenBeam,
    /// An alignment run (`alignment` folder): never corrected, never
    /// normalized.
    Alignment,
}

/// Classify the image folder recorded by [`nexus_image_path`]: an
/// `alignment` or `ob` path component (any case) anywhere in the path
/// tells; anything else is a sample run. Alignment runs are never
/// corrected by the autoreduction and need no normalization; open beams
/// are corrected but are normalization inputs, never normalized.
pub fn image_path_kind(image_path: &str) -> RunKind {
    for part in image_path.split(['/', '\\']) {
        let part = part.trim();
        if part.eq_ignore_ascii_case("alignment") {
            return RunKind::Alignment;
        }
        if part.eq_ignore_ascii_case("ob") {
            return RunKind::OpenBeam;
        }
    }
    RunKind::Sample
}

/// Numeric dataset read as f64 whatever its stored numeric flavor (the
/// HDF5 library converts integers and narrower floats on the fly).
fn read_f64s(c: &h5::Container) -> Option<Vec<f64>> {
    use TypeDescriptor::*;
    match c.dtype().and_then(|d| d.to_descriptor()).ok()? {
        Float(_) | Integer(_) | Unsigned(_) => c.read_raw::<f64>().ok(),
        _ => None,
    }
}

/// One sample-environment log of a NeXus file: the (absolute time, value)
/// points of `/entry/DASlogs/<name>`, where the `time` dataset holds
/// seconds since its own `start` attribute (RFC-3339). `None` when the
/// file or the log is missing or unreadable.
pub fn daslog(path: &Path, name: &str) -> Option<Vec<(DateTime<FixedOffset>, f64)>> {
    let file = h5::File::open(path).ok()?;
    let group = file.group(&format!("entry/DASlogs/{name}")).ok()?;
    let time_ds = group.dataset("time").ok()?;
    let start = time_ds
        .attr("start")
        .ok()
        .and_then(|a| read_strings(&a)?.into_iter().next())?;
    let start = DateTime::parse_from_rfc3339(start.trim()).ok()?;
    let times = read_f64s(&time_ds)?;
    let value_ds = group.dataset("value").ok()?;
    let values = read_f64s(&value_ds)?;
    Some(
        times
            .into_iter()
            .zip(values)
            .map(|(t, v)| {
                (
                    start + chrono::Duration::milliseconds((t * 1000.0) as i64),
                    v,
                )
            })
            .collect(),
    )
}

/// What the auto-normalization launcher needs out of a normalization
/// session configuration file (schema of the marimo notebook, version 1).
#[derive(Clone, Debug)]
pub struct ConfigInfo {
    /// Detector-corrected open-beam folders (NeuNorm `--ob` inputs).
    pub ob_folders: Vec<PathBuf>,
    /// The notebook's crop region `(x0, y0, x1, y1)` (exclusive stops, in
    /// the on-disk frame of the TIFF files), if any — the inputs are
    /// pre-cropped on disk before NeuNorm runs, as the workflow runner does.
    pub crop_region: Option<(usize, usize, usize, usize)>,
    /// The notebook's output folder (root attribute `output_folder`), where
    /// the workflow runner writes `Run_<run>/normalization`. `None` when
    /// absent or empty.
    pub output_folder: Option<PathBuf>,
    /// The detector the data came from (root attribute `detector`, e.g.
    /// `tpx1`) — decides the display orientation in the TIFF viewer.
    pub detector: Option<String>,
}

/// Read the open-beam entries, crop flag and output folder of a
/// configuration file.
pub fn read_config_info(path: &Path) -> Result<ConfigInfo, String> {
    let file = h5::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let ob_folders = dataset_strings(&file, "ob/folders")
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let crop_region = file
        .group("normalization")
        .ok()
        .and_then(|g| g.attr("crop_region").ok())
        .and_then(|a| read_f64s(&a))
        .filter(|v| v.len() == 4 && v.iter().all(|x| x.is_finite() && *x >= 0.0))
        .map(|v| (v[0] as usize, v[1] as usize, v[2] as usize, v[3] as usize));
    let root_string = |name: &str| {
        file.attr(name)
            .ok()
            .and_then(|a| read_strings(&a)?.into_iter().next())
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    let output_folder = root_string("output_folder").map(PathBuf::from);
    let detector = root_string("detector");
    Ok(ConfigInfo {
        ob_folders,
        crop_region,
        output_folder,
        detector,
    })
}

/// Write a copy of the configuration `src` at `dst` with its open beams
/// replaced by `ob_folders` (the `ob/folders` dataset, variable-length
/// UTF-8 strings as the notebook writes them, and the `ob/run_spec`
/// attribute set to `run_spec`, e.g. `29962, 29963`). Everything else —
/// sample, masks, normalization settings, output folder — is kept
/// byte-for-byte (the file is copied first). The root gets a
/// `derived_from` attribute naming `src` and a fresh `created` time so the
/// notebook and the viewers can tell where the file comes from. `dst` is
/// overwritten if it exists.
pub fn write_config_with_obs(
    src: &Path,
    dst: &Path,
    ob_folders: &[PathBuf],
    run_spec: &str,
) -> Result<(), String> {
    use hdf5_metno::types::VarLenUnicode;
    if ob_folders.is_empty() {
        return Err("no open-beam folder to write".to_owned());
    }
    let folders: Vec<VarLenUnicode> = ob_folders
        .iter()
        .map(|f| {
            f.to_string_lossy()
                .parse::<VarLenUnicode>()
                .map_err(|e| format!("open-beam path is not valid UTF-8: {e}"))
        })
        .collect::<Result<_, _>>()?;
    std::fs::copy(src, dst).map_err(|e| {
        format!("cannot copy {} to {}: {e}", src.display(), dst.display())
    })?;
    let result = (|| -> Result<(), String> {
        let file = h5::File::open_rw(dst)
            .map_err(|e| format!("cannot open {} for writing: {e}", dst.display()))?;
        let ob = match file.group("ob") {
            Ok(group) => group,
            Err(_) => file
                .create_group("ob")
                .map_err(|e| format!("cannot create the ob group: {e}"))?,
        };
        if ob.link_exists("folders") {
            ob.unlink("folders")
                .map_err(|e| format!("cannot replace ob/folders: {e}"))?;
        }
        ob.new_dataset_builder()
            .with_data(&folders)
            .create("folders")
            .map_err(|e| format!("cannot write ob/folders: {e}"))?;
        let set_string_attr = |loc: &h5::Location, name: &str, value: &str| {
            let value = value
                .parse::<VarLenUnicode>()
                .map_err(|e| format!("attribute {name} is not valid UTF-8: {e}"))?;
            if loc.attr_names().map(|names| names.iter().any(|n| n == name)).unwrap_or(false) {
                loc.delete_attr(name)
                    .map_err(|e| format!("cannot replace attribute {name}: {e}"))?;
            }
            loc.new_attr::<VarLenUnicode>()
                .create(name)
                .and_then(|a| a.write_scalar(&value))
                .map_err(|e| format!("cannot write attribute {name}: {e}"))
        };
        set_string_attr(&ob, "run_spec", run_spec)?;
        set_string_attr(&file, "derived_from", &src.display().to_string())?;
        set_string_attr(
            &file,
            "created",
            &chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        )?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(dst);
    }
    result
}

/// The notebook's default parameters, as a configuration file.
///
/// Write at `dst` a normalization configuration (the notebook's schema,
/// version 1) holding every default of the "Normalization TOF at VENUS"
/// notebook — what a user gets by opening it and saving without touching
/// a setting: Bragg mode, proton-charge normalization, no background
/// matching, no inpainting, no manual TOF binning (no TOF range), 25 m
/// source-detector distance, 700 ns spectra bins, no crop, no mask, the
/// TIFF stack + integrated TIFF + `x_axis.txt` exports — with no sample
/// (each run's own corrected folder is the sample) and no open beam (the
/// table's "⇄ replace by…" names them). `ipts` (e.g. `IPTS-36967`),
/// `detector` (e.g. `tpx1`) and `output_folder` are the root attributes
/// the notebook, the workflow runner and this app read. The file is
/// written to a temporary sibling and renamed into place, so a reader
/// never sees a half-written file; `dst` is overwritten if it exists.
pub fn write_default_config(
    dst: &Path,
    ipts: &str,
    detector: &str,
    output_folder: &Path,
) -> Result<(), String> {
    use hdf5_metno::types::VarLenUnicode;
    let parent = dst
        .parent()
        .ok_or_else(|| format!("{} has no parent folder", dst.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    let tmp = dst.with_extension("h5.tmp");
    let unicode = |name: &str, value: &str| {
        value
            .parse::<VarLenUnicode>()
            .map_err(|e| format!("{name} is not valid UTF-8: {e}"))
    };
    let string_attr = |loc: &h5::Location, name: &str, value: &str| -> Result<(), String> {
        let value = unicode(name, value)?;
        loc.new_attr::<VarLenUnicode>()
            .create(name)
            .and_then(|a| a.write_scalar(&value))
            .map_err(|e| format!("cannot write attribute {name}: {e}"))
    };
    let bool_attr = |loc: &h5::Location, name: &str, value: bool| -> Result<(), String> {
        loc.new_attr::<bool>()
            .create(name)
            .and_then(|a| a.write_scalar(&value))
            .map_err(|e| format!("cannot write attribute {name}: {e}"))
    };
    let f64_attr = |loc: &h5::Location, name: &str, value: f64| -> Result<(), String> {
        loc.new_attr::<f64>()
            .create(name)
            .and_then(|a| a.write_scalar(&value))
            .map_err(|e| format!("cannot write attribute {name}: {e}"))
    };
    let result = (|| -> Result<(), String> {
        let file = h5::File::create(&tmp)
            .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;
        let group = |name: &str| {
            file.create_group(name)
                .map_err(|e| format!("cannot create the {name} group: {e}"))
        };
        // Root: what the notebook writes, plus where this file comes from.
        file.new_attr::<i64>()
            .create("schema_version")
            .and_then(|a| a.write_scalar(&1))
            .map_err(|e| format!("cannot write attribute schema_version: {e}"))?;
        string_attr(
            &file,
            "created",
            &chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        )?;
        string_attr(&file, "notebook", "normalization_tof_at_venus_marimo")?;
        string_attr(
            &file,
            "generated_by",
            "autonormalization_monitor — the notebook's default parameters",
        )?;
        string_attr(&file, "ipts", ipts)?;
        string_attr(&file, "detector", detector)?;
        string_attr(&file, "output_folder", &output_folder.display().to_string())?;

        // Sample: none — each run's corrected folder is the sample.
        let sample = group("sample")?;
        string_attr(&sample, "run_spec", "")?;
        bool_attr(&sample, "combine_runs", false)?;
        sample
            .new_dataset::<VarLenUnicode>()
            .shape(0)
            .create("folders")
            .map_err(|e| format!("cannot write sample/folders: {e}"))?;

        // Open beams: none — named per run in the table.
        let ob = group("ob")?;
        string_attr(&ob, "run_spec", "")?;
        ob.new_dataset::<VarLenUnicode>()
            .shape(0)
            .create("folders")
            .map_err(|e| format!("cannot write ob/folders: {e}"))?;

        let spectra = group("spectra")?;
        f64_attr(&spectra, "tof_bin_size_ns", 700.0)?;

        let norm = group("normalization")?;
        string_attr(&norm, "mode", "Bragg mode")?;
        bool_attr(&norm, "proton_charge", true)?;
        bool_attr(&norm, "match_background", false)?;
        bool_attr(&norm, "inpaint", false)?;
        string_attr(&norm, "tof_binning_mode", "None")?;
        f64_attr(&norm, "distance_source_detector_m", 25.0)?;
        norm.new_dataset::<f64>()
            .shape((0, 2))
            .create("tof_ranges_us")
            .map_err(|e| format!("cannot write normalization/tof_ranges_us: {e}"))?;
        norm.new_dataset::<i64>()
            .shape(0)
            .create("tof_ranges_enabled")
            .map_err(|e| format!("cannot write normalization/tof_ranges_enabled: {e}"))?;
        // No crop_region, no roi_mask, no background_mask: the notebook
        // writes them only when selected.

        let export = group("export")?;
        bool_attr(&export, "scitiff", false)?;
        bool_attr(&export, "stack", true)?;
        bool_attr(&export, "integrated", true)?;
        bool_attr(&export, "integrated_tiff", true)?;
        bool_attr(&export, "integrated_scitiff", false)?;
        bool_attr(&export, "ascii", true)?;
        drop(file);
        std::fs::rename(&tmp, dst)
            .map_err(|e| format!("cannot move {} to {}: {e}", tmp.display(), dst.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_a_default_config_this_app_reads_back() {
        // Under $ANM_TEST_OUTPUT when set (the file is kept there for the
        // python-side checks), else the temp dir.
        let dir = std::env::var("ANM_TEST_OUTPUT")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("anm_default_config_test");
        let output = dir.join("processed_data/autoreduction");
        let path = output.join("default_normalization_config.h5");
        write_default_config(&path, "IPTS-36967", "tpx1", &output).expect("written");
        let info = read_config_info(&path).expect("readable");
        assert!(info.ob_folders.is_empty());
        assert_eq!(info.crop_region, None);
        assert_eq!(info.output_folder.as_deref(), Some(output.as_path()));
        assert_eq!(info.detector.as_deref(), Some("tpx1"));
        assert!(!path.with_extension("h5.tmp").exists());
        // Overwriting an existing file works too (a fresh copy per IPTS).
        write_default_config(&path, "IPTS-36967", "tpx3", &output).expect("rewritten");
        assert_eq!(read_config_info(&path).unwrap().detector.as_deref(), Some("tpx3"));
    }

    // These read real shared files; they pass trivially where the VENUS
    // filesystem is not mounted.

    #[test]
    fn reads_the_times_of_a_real_nexus() {
        let path = Path::new("/SNS/VENUS/IPTS-36967/nexus/VENUS_23642.nxs.h5");
        if !path.is_file() {
            return;
        }
        let (start, end) = nexus_times(path).expect("times should be readable");
        assert_eq!(end.format("%Y-%m-%d").to_string(), "2026-06-13");
        assert!(start <= end);
        // Missing file → None, no panic.
        assert!(nexus_times(Path::new("/nonexistent.nxs.h5")).is_none());
    }

    #[test]
    fn classifies_image_paths() {
        let alignment = |p: &str| image_path_kind(p) == RunKind::Alignment;
        assert!(alignment(
            "images/tpx1/alignment/20260910_x_10_000s/20260910_Run_29949_x_10_000s"
        ));
        assert!(alignment("images/tpx1/ALIGNMENT/foo   "));
        assert!(!alignment("images/tpx1/raw/radiography/20260910_t/20260910_Run_29955_t_0"));
        assert!(!alignment("images/tpx1/ob/20260910_ob/20260910_Run_29962_ob_0"));
        // A title mentioning alignment is not an alignment folder.
        assert!(!alignment("images/tpx1/raw/radiography/alignment_test/Run_1"));
        assert!(!alignment(""));
        // Open beams: the `ob` folder of the images tree, whatever the
        // case; a title mentioning ob is still a sample.
        assert_eq!(
            image_path_kind("images/tpx1/ob/20260910_ob/20260910_Run_29962_ob_0"),
            RunKind::OpenBeam
        );
        assert_eq!(image_path_kind("images/tpx1/OB/x/Run_1_ob_0"), RunKind::OpenBeam);
        assert_eq!(
            image_path_kind("images/tpx1/raw/radiography/20260910_t/20260910_Run_29955_t_0"),
            RunKind::Sample
        );
        assert_eq!(
            image_path_kind("images/tpx1/raw/ct/virgin_c_ob_1_185C/Run_29909_ob_0"),
            RunKind::Sample
        );
        assert_eq!(image_path_kind("images/tpx1/alignment/x/Run_1"), RunKind::Alignment);
        assert_eq!(image_path_kind(""), RunKind::Sample);
    }

    #[test]
    fn derives_a_config_with_other_open_beams() {
        let src = Path::new(
            "/SNS/VENUS/IPTS-37705/shared/autoreduce/normalization_config_20260910_102311_last.h5",
        );
        if !src.is_file() {
            return;
        }
        let dir = std::env::temp_dir().join("anm_test_derive_config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("derived.h5");
        let obs = vec![
            PathBuf::from("/SNS/VENUS/IPTS-37705/shared/autoreduce/images/tpx1/ob/a/Run_29962_ob_0"),
            PathBuf::from("/SNS/VENUS/IPTS-37705/shared/autoreduce/images/tpx1/ob/a/Run_29963_ob_1"),
        ];
        write_config_with_obs(src, &dst, &obs, "29962, 29963").unwrap();
        let before = read_config_info(src).unwrap();
        let after = read_config_info(&dst).unwrap();
        assert_eq!(after.ob_folders, obs);
        assert_ne!(before.ob_folders, after.ob_folders);
        // Everything else is carried over.
        assert_eq!(after.output_folder, before.output_folder);
        assert_eq!(after.detector, before.detector);
        assert_eq!(after.crop_region, before.crop_region);
        let file = h5::File::open(&dst).unwrap();
        assert_eq!(
            dataset_strings(&file, "sample/folders"),
            dataset_strings(&h5::File::open(src).unwrap(), "sample/folders")
        );
        let attr = |loc: &h5::Location, name: &str| {
            loc.attr(name).ok().and_then(|a| read_strings(&a)?.into_iter().next())
        };
        assert_eq!(attr(&file.group("ob").unwrap(), "run_spec").as_deref(), Some("29962, 29963"));
        assert_eq!(attr(&file, "derived_from").as_deref(), Some(src.to_str().unwrap()));
        assert!(attr(&file, "notebook").is_some());
        // Writing again over the same file works (it is overwritten) —
        // once nobody holds it open (the HDF5 library refuses otherwise).
        drop(file);
        write_config_with_obs(src, &dst, &obs[..1], "29962").unwrap();
        assert_eq!(read_config_info(&dst).unwrap().ob_folders, obs[..1]);
        // No open beam → error, nothing left behind.
        let bad = dir.join("bad.h5");
        assert!(write_config_with_obs(src, &bad, &[], "").is_err());
        assert!(!bad.exists());
    }

    #[test]
    fn detects_alignment_runs_of_a_real_ipts() {
        let ipts = Path::new("/SNS/VENUS/IPTS-37705/nexus");
        if !ipts.is_dir() {
            return;
        }
        // 29949 was filed under images/tpx1/alignment, 29955 under raw/
        // (the log's first value is the previous run's folder — the last
        // one must be used).
        let kind = |run: u64| {
            nexus_image_path(&ipts.join(format!("VENUS_{run}.nxs.h5")))
                .map(|p| image_path_kind(&p))
        };
        assert_eq!(kind(29949), Some(RunKind::Alignment));
        assert_eq!(kind(29955), Some(RunKind::Sample));
        // 29905 is one of the open beams of the IPTS configuration.
        assert_eq!(kind(29905), Some(RunKind::OpenBeam));
        assert!(nexus_image_path(&ipts.join("VENUS_29955.nxs.h5"))
            .is_some_and(|p| p.ends_with("Run_29955_Fe_powder_11mm_1_190C_1_000AngsMin_0")));
        // Missing file → None, no panic.
        assert_eq!(nexus_image_path(Path::new("/nonexistent.nxs.h5")), None);
    }

    #[test]
    fn reads_a_daslog_of_a_real_nexus() {
        let path = Path::new("/SNS/VENUS/IPTS-36967/nexus/VENUS_23642.nxs.h5");
        if !path.is_file() {
            return;
        }
        let points = daslog(path, "BL10:SE:ND2:CH4:PV").expect("log should be readable");
        assert!(!points.is_empty());
        let (t, v) = points[0];
        assert_eq!(t.format("%Y-%m-%d").to_string(), "2026-06-13");
        assert!(v.is_finite());
        // Missing log → None, no panic.
        assert!(daslog(path, "BL10:SE:ND2:NoSuchLog").is_none());
    }

    #[test]
    fn reads_ob_folders_of_a_real_config() {
        let path = Path::new(
            "/SNS/VENUS/IPTS-36967/shared/autoreduce/configs/normalization_config_20260718_084258.h5",
        );
        if !path.is_file() {
            return;
        }
        let info = read_config_info(path).expect("config should be readable");
        assert!(!info.ob_folders.is_empty());
        for folder in &info.ob_folders {
            assert!(folder.is_absolute());
        }
        assert_eq!(
            info.output_folder,
            Some(PathBuf::from("/SNS/VENUS/IPTS-36967/shared/jean"))
        );
        assert_eq!(info.detector.as_deref(), Some("tpx1"));
    }
}
