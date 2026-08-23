//! Provision and launch the marimo "Normalization TOF at VENUS" notebook
//! inside a chosen IPTS, exactly like the marimo general-tools portal does:
//! the notebook (plus its `utilities/` package) is copied into
//! `<IPTS>/shared/notebooks/imaging_marimo_<user>/` and marimo is started
//! with that folder as working directory — the notebook detects the IPTS
//! from its cwd and pre-selects it, so the user lands directly in the right
//! experiment.

use std::fs;
use std::io::BufRead;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The notebook used to create/edit normalization configuration files.
pub const NOTEBOOK_PATH: &str =
    "/SNS/VENUS/shared/software/git/marimo_notebooks/notebooks/normalization_tof_at_venus_marimo.py";
/// The marimo binary of the shared notebooks environment.
pub const MARIMO_BIN: &str =
    "/SNS/VENUS/shared/software/git/marimo_notebooks/.pixi/envs/default/bin/marimo";

fn user_id() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".to_string())
}

/// Copy a directory tree, skipping python cache folders.
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let name = entry.file_name();
        if matches!(name.to_string_lossy().as_ref(), "__pycache__" | "__marimo__") {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst.join(&name);
        if ft.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            if dst_path.exists() {
                let _ = fs::remove_file(&dst_path);
            }
            fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Recursively set rwx-for-all. Entries owned by other users cannot be
/// chmod'ed and are left as-is.
fn open_permissions_recursive(path: &Path) {
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o777));
    if path.is_dir() {
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                open_permissions_recursive(&entry.path());
            }
        }
    }
}

/// Copy the notebook + its `utilities/` package into the per-user notebooks
/// folder of `ipts_path`. Returns (destination folder, notebook file name).
fn provision(ipts_path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let notebook = Path::new(NOTEBOOK_PATH);
    let file_name = notebook
        .file_name()
        .ok_or_else(|| format!("invalid notebook path: {NOTEBOOK_PATH}"))?
        .to_owned();
    let src_dir = notebook
        .parent()
        .ok_or_else(|| format!("cannot determine source folder of {NOTEBOOK_PATH}"))?;
    let dest = ipts_path
        .join("shared/notebooks")
        .join(format!("imaging_marimo_{}", user_id()));

    fs::create_dir_all(&dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    // Everybody must be able to read/write/traverse the notebooks folders.
    // The parent `notebooks` dir may pre-exist and belong to another user,
    // in which case chmod fails and we leave it as-is.
    if let Some(notebooks_dir) = dest.parent() {
        let _ = fs::set_permissions(notebooks_dir, fs::Permissions::from_mode(0o777));
    }

    let dst_notebook = dest.join(&file_name);
    if dst_notebook.exists() {
        let _ = fs::remove_file(&dst_notebook);
    }
    fs::copy(notebook, &dst_notebook).map_err(|e| {
        format!("copy {} -> {}: {e}", notebook.display(), dst_notebook.display())
    })?;

    let utilities_src = src_dir.join("utilities");
    if utilities_src.is_dir() {
        copy_dir_recursive(&utilities_src, &dest.join("utilities"))
            .map_err(|e| format!("copy utilities: {e}"))?;
    }
    open_permissions_recursive(&dest);
    Ok((dest, PathBuf::from(file_name)))
}

/// Provision + launch the notebook for `ipts_path`, opening the served URL
/// in firefox as soon as marimo prints it. The marimo process is detached
/// and keeps running after this application exits.
pub fn launch(ipts_path: &Path) -> Result<String, String> {
    let (dest, notebook_name) = provision(ipts_path)?;
    let mut child = Command::new(MARIMO_BIN)
        .arg("run")
        .arg(&notebook_name)
        .arg("--headless")
        // Provisioned IPTS folders have no pyproject.toml/.marimo.toml in
        // their ancestry, so marimo would fall back to its 8 MB default;
        // the branding cell alone trips the truncation banner at startup.
        .env("MARIMO_OUTPUT_MAX_BYTES", "20000000")
        .current_dir(&dest)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch {MARIMO_BIN}: {e}"))?;

    for stream in [
        child.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        std::thread::spawn(move || {
            let reader = std::io::BufReader::new(stream);
            let mut launched = false;
            for line in reader.lines().map_while(Result::ok) {
                println!("{line}");
                if !launched {
                    if let Some(start) = line.find("http://") {
                        let url: String = line[start..]
                            .chars()
                            .take_while(|c| !c.is_whitespace())
                            .collect();
                        println!("Opening {url} in firefox");
                        let _ = Command::new("firefox").arg(&url).spawn();
                        launched = true;
                    }
                }
            }
        });
    }
    // Detach: keep marimo running after we drop the handle.
    std::mem::forget(child);
    Ok(format!("Notebook launched from {}", dest.display()))
}
