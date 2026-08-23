//! Minimal HDF5 reading: acquisition times from a VENUS NeXus file and the
//! open-beam entries of a normalization session configuration. The tolerant
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

/// What the auto-normalization launcher needs out of a normalization
/// session configuration file (schema of the marimo notebook, version 1).
pub struct ConfigInfo {
    /// Detector-corrected open-beam folders (NeuNorm `--ob` inputs).
    pub ob_folders: Vec<PathBuf>,
    /// The notebook's crop region, if any — the workflow-runner pre-crops
    /// inputs when set, which this application does not implement yet.
    pub has_crop: bool,
}

/// Read the open-beam entries (and crop flag) of a configuration file.
pub fn read_config_info(path: &Path) -> Result<ConfigInfo, String> {
    let file = h5::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let ob_folders = dataset_strings(&file, "ob/folders")
        .unwrap_or_default()
        .into_iter()
        .map(PathBuf::from)
        .collect();
    let has_crop = file
        .group("normalization")
        .ok()
        .and_then(|g| g.attr("crop_region").ok())
        .is_some();
    Ok(ConfigInfo { ob_folders, has_crop })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
