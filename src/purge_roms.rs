use super::common::*;
use super::config::*;
use super::database::*;
use super::model::{Romfile, System};
use super::progress::*;
use super::prompt::*;
use super::util::*;
use anyhow::{Context, Result};
use clap::{Arg, ArgAction, ArgMatches, Command};
use indicatif::ProgressBar;
use sqlx::sqlite::SqliteConnection;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

pub fn subcommand() -> Command {
    Command::new("purge-roms")
        .about("Purge trashed, missing, and orphan ROM files")
        .arg(
            Arg::new("MISSING")
                .short('m')
                .long("missing")
                .help("Delete missing ROM files from the database")
                .required(false)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("ORPHAN")
                .short('o')
                .long("orphan")
                .help("Delete ROM files without an associated ROM from the database")
                .required(false)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("TRASH")
                .short('t')
                .long("trash")
                .help("Physically delete ROM files from the trash directories")
                .required(false)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("FOREIGN")
                .short('f')
                .long("foreign")
                .help("Physically delete ROM files unknown to the database")
                .required(false)
                .action(ArgAction::SetTrue),
        )
        .arg(
            Arg::new("YES")
                .short('y')
                .long("yes")
                .help("Automatically say yes to prompts")
                .required(false)
                .action(ArgAction::SetTrue),
        )
}

pub async fn main(
    connection: &mut SqliteConnection,
    matches: &ArgMatches,
    progress_bar: &ProgressBar,
) -> Result<()> {
    let answer_yes = matches.get_flag("YES");
    if matches.get_flag("MISSING") {
        purge_missing_romfiles(connection, progress_bar).await?;
    }
    if matches.get_flag("TRASH") {
        let romfiles = find_romfiles_in_trash(connection).await;
        purge_romfiles(connection, progress_bar, answer_yes, "trashed", romfiles).await?;
    }
    if matches.get_flag("ORPHAN") {
        let romfiles = find_orphan_romfiles(connection).await;
        purge_romfiles(connection, progress_bar, answer_yes, "orphan", romfiles).await?;
    }
    if matches.get_flag("FOREIGN") {
        purge_foreign_romfiles(connection, progress_bar, answer_yes).await?;
    }
    for system in find_systems(connection).await {
        compute_system_completion(connection, progress_bar, &system).await?;
    }
    Ok(())
}

/// The directories a purge may empty but never removes: the ROM directories,
/// every system directory and every Trash directory.
async fn kept_directories(connection: &mut SqliteConnection) -> Result<HashSet<PathBuf>> {
    let mut kept = HashSet::new();
    let rom_directory = get_rom_directory(connection).await;
    kept.insert(rom_directory.join("Trash"));
    kept.insert(rom_directory);
    let systems: Vec<System> = find_systems(connection).await;
    for system in &systems {
        let system_directory = get_system_directory(connection, system).await?;
        if let Some(system_rom_directory) = system_directory.parent() {
            kept.insert(system_rom_directory.to_path_buf());
        }
        kept.insert(system_directory.join("Trash"));
        kept.insert(system_directory);
    }
    Ok(kept)
}

/// Every game lives in its own folder, so purging a game's last file leaves
/// that folder behind. Removes empty directories from the file's parent
/// upwards, stopping at the first kept, non-empty or missing one, and never
/// leaving the ROM directories.
async fn remove_empty_parents(
    progress_bar: &ProgressBar,
    file: &Path,
    kept: &HashSet<PathBuf>,
) -> Result<()> {
    let mut directory = file.parent();
    while let Some(dir) = directory {
        if kept.contains(dir)
            || !kept.iter().any(|kept_dir| dir.starts_with(kept_dir))
            || !dir.is_dir()
            || dir.read_dir()?.next().is_some()
        {
            break;
        }
        remove_directory(progress_bar, &dir, true).await?;
        directory = dir.parent();
    }
    Ok(())
}

async fn purge_missing_romfiles(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
) -> Result<()> {
    print_subheader(progress_bar, "Processing missing ROM files");

    let romfiles = find_romfiles(connection).await;
    let kept = kept_directories(connection).await?;
    let mut count = 0;

    for romfile in romfiles {
        let path = romfile.as_common(connection).await?.path;
        if !path.is_file() {
            delete_romfile_by_id(connection, romfile.id).await;
            remove_empty_parents(progress_bar, &path, &kept).await?;
            count += 1;
        }
    }

    if count > 0 {
        print_success(
            progress_bar,
            &format!("Deleted {} missing ROM file(s) from the database", count),
        );
    }

    Ok(())
}

async fn purge_romfiles(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    answer_yes: bool,
    label: &str,
    romfiles: Vec<Romfile>,
) -> Result<()> {
    print_subheader(progress_bar, &format!("Processing {} ROM files", label));

    let mut count = 0;

    if !romfiles.is_empty() {
        print_subheader(progress_bar, "Summary:");
        for romfile in &romfiles {
            print_info(progress_bar, &romfile.path);
        }

        if answer_yes || confirm(true)? {
            let kept = kept_directories(connection).await?;
            let mut transaction = begin_transaction(connection).await;

            for romfile in &romfiles {
                let common_romfile = romfile.as_common(&mut transaction).await?;
                if common_romfile.path.is_file() {
                    let path = common_romfile.path.clone();
                    common_romfile.delete(progress_bar, false).await?;
                    delete_romfile_by_id(&mut transaction, romfile.id).await;
                    remove_empty_parents(progress_bar, &path, &kept).await?;
                    count += 1;
                }
            }

            commit_transaction(transaction).await;

            if count > 0 {
                print_success(
                    progress_bar,
                    &format!("Deleted {} {} ROM file(s)", count, label),
                );
            }
        }
    }

    Ok(())
}

async fn purge_foreign_romfiles(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    answer_yes: bool,
) -> Result<()> {
    print_subheader(progress_bar, "Processing foreign ROM files");
    let rom_directory = get_rom_directory(connection).await;
    let walker = WalkDir::new(&rom_directory).into_iter();
    let mut removed: Vec<PathBuf> = vec![];
    let mut count = 0;
    for entry in walker.filter_map(|e| e.ok()) {
        if entry.path().is_file() {
            let relative_path = entry
                .path()
                .strip_prefix(&rom_directory)
                .context("Failed to retrieve relative path")?;
            if find_romfile_by_path(connection, relative_path.as_os_str().to_str().unwrap())
                .await
                .is_none()
            {
                print_action(
                    progress_bar,
                    &format!(
                        "Foreign file found: \"{}\"",
                        relative_path.as_os_str().to_str().unwrap()
                    ),
                );
                if answer_yes || confirm(true)? {
                    remove_file(progress_bar, &entry.path(), false).await?;
                    removed.push(entry.path().to_path_buf());
                    count += 1;
                }
            }
        }
    }
    // after the walk, so no directory is removed while it is being read
    let kept = kept_directories(connection).await?;
    for path in &removed {
        remove_empty_parents(progress_bar, path, &kept).await?;
    }
    if count > 0 {
        print_success(
            progress_bar,
            &format!(
                "Deleted {} foreign ROM file(s) from the ROM directory",
                count
            ),
        );
    }
    Ok(())
}

#[cfg(test)]
mod test_foreign;
#[cfg(test)]
mod test_missing;
#[cfg(test)]
mod test_orphans;
#[cfg(test)]
mod test_trashed;
