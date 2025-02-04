use super::progress::*;
use super::SimpleResult;
use async_std::path::{Path, PathBuf};
use indicatif::ProgressBar;
use std::process::Command;
use std::str::FromStr;
use std::time::Duration;

#[derive(PartialEq, Eq)]
pub enum ArchiveType {
    Sevenzip,
    Zip,
}

pub struct ArchiveInfo {
    pub path: String,
    pub size: u64,
    pub crc: String,
}

pub fn parse_archive<P: AsRef<Path>>(
    progress_bar: &ProgressBar,
    archive_path: &P,
) -> SimpleResult<Vec<ArchiveInfo>> {
    progress_bar.set_message("Parsing archive");
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    let output = Command::new("7z")
        .arg("l")
        .arg("-slt")
        .arg(archive_path.as_ref())
        .output()
        .expect("Failed to parse archive");

    if !output.status.success() {
        bail!(String::from_utf8(output.stderr).unwrap().as_str());
    }

    let stdout = String::from_utf8(output.stdout).unwrap();
    let lines: Vec<&str> = stdout
        .lines()
        .filter(|&line| {
            line.starts_with("Path =") || line.starts_with("Size =") || line.starts_with("CRC =")
        })
        .skip(1) // the first line is the archive itself
        .map(|line| line.split('=').last().unwrap().trim()) // keep only the rhs
        .collect();

    // each chunk will have the path, size and crc respectively
    let mut sevenzip_infos: Vec<ArchiveInfo> = Vec::new();
    for info in lines.chunks(3) {
        let sevenzip_info = ArchiveInfo {
            path: String::from(info.get(0).unwrap().to_owned()),
            size: FromStr::from_str(info.get(1).unwrap()).unwrap(),
            crc: info.get(2).unwrap().to_owned().to_lowercase(),
        };
        sevenzip_infos.push(sevenzip_info);
    }

    progress_bar.set_message("");
    progress_bar.disable_steady_tick();

    Ok(sevenzip_infos)
}

pub fn rename_file_in_archive<P: AsRef<Path>>(
    progress_bar: &ProgressBar,
    archive_path: &P,
    file_name: &str,
    new_file_name: &str,
) -> SimpleResult<()> {
    progress_bar.set_message("Renaming file in archive");
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));
    progress_bar.println(format!(
        "Renaming \"{}\" to \"{}\"",
        file_name, new_file_name
    ));

    let output = Command::new("7z")
        .arg("rn")
        .arg(archive_path.as_ref())
        .arg(file_name)
        .arg(new_file_name)
        .output()
        .expect("Failed to rename file in archive");

    if !output.status.success() {
        bail!(String::from_utf8(output.stderr).unwrap().as_str());
    }

    progress_bar.set_message("");
    progress_bar.disable_steady_tick();

    Ok(())
}

pub fn extract_files_from_archive<P: AsRef<Path>, Q: AsRef<Path>>(
    progress_bar: &ProgressBar,
    archive_path: &P,
    file_names: &[&str],
    directory: &Q,
) -> SimpleResult<Vec<PathBuf>> {
    progress_bar.set_message("Extracting files");
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    for &file_name in file_names {
        progress_bar.println(format!("Extracting \"{}\"", file_name));
    }

    let output = Command::new("7z")
        .arg("x")
        .arg(archive_path.as_ref())
        .args(file_names)
        .current_dir(directory.as_ref())
        .output()
        .expect("Failed to extract archive");

    if !output.status.success() {
        bail!(String::from_utf8(output.stderr).unwrap().as_str())
    }

    progress_bar.set_message("");
    progress_bar.disable_steady_tick();

    Ok(file_names
        .iter()
        .map(|file_name| directory.as_ref().join(file_name))
        .collect())
}

pub fn add_files_to_archive<P: AsRef<Path>, Q: AsRef<Path>>(
    progress_bar: &ProgressBar,
    archive_path: &P,
    file_names: &[&str],
    directory: &Q,
    solid: bool,
) -> SimpleResult<()> {
    progress_bar.set_message("Compressing files");
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    for &file_name in file_names {
        progress_bar.println(format!("Compressing \"{}\"", file_name));
    }

    let mut args = vec!["-mx=9"];
    if solid {
        args.push("-ms=on")
    }
    let output = Command::new("7z")
        .arg("a")
        .arg(archive_path.as_ref())
        .args(file_names)
        .args(args)
        .current_dir(directory.as_ref())
        .output()
        .expect("Failed to add files to archive");

    if !output.status.success() {
        bail!(String::from_utf8(output.stderr).unwrap().as_str())
    }

    progress_bar.set_message("");
    progress_bar.disable_steady_tick();

    Ok(())
}

pub fn remove_files_from_archive<P: AsRef<Path>>(
    progress_bar: &ProgressBar,
    archive_path: &P,
    file_names: &[&str],
) -> SimpleResult<()> {
    progress_bar.set_message("Deleting files");
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));

    for &file_name in file_names {
        progress_bar.println(format!("Deleting \"{}\"", file_name));
    }

    let output = Command::new("7z")
        .arg("d")
        .arg(archive_path.as_ref())
        .args(file_names)
        .output()
        .expect("Failed to remove files from archive");

    if !output.status.success() {
        bail!(String::from_utf8(output.stderr).unwrap().as_str())
    }

    progress_bar.set_message("");
    progress_bar.disable_steady_tick();

    Ok(())
}
