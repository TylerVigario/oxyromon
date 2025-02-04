#[cfg(feature = "chd")]
use super::chdman;
use super::checksum::*;
use super::config::*;
use super::database::*;
#[cfg(feature = "rvz")]
use super::dolphin;
#[cfg(feature = "cso")]
use super::maxcso;
use super::model::*;
use super::progress::*;
use super::prompt::*;
use super::sevenzip;
use super::util::*;
use super::SimpleResult;
use async_std::path::Path;
use cfg_if::cfg_if;
use clap::builder::PossibleValuesParser;
use clap::{Arg, ArgMatches, Command};
use indicatif::ProgressBar;
use rayon::prelude::*;
use sqlx::sqlite::SqliteConnection;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
#[cfg(feature = "ird")]
use walkdir::WalkDir;

pub fn subcommand() -> Command {
    Command::new("import-roms")
        .about("Validate and import ROM files or directories into oxyromon")
        .arg(
            Arg::new("ROMS")
                .help("Set the ROM files or directories to import")
                .required(true)
                .num_args(1..)
                .index(1)
                .value_parser(value_parser!(PathBuf)),
        )
        .arg(
            Arg::new("SYSTEM")
                .short('s')
                .long("system")
                .help("Set the system number to use")
                .required(false)
                .num_args(1),
        )
        .arg(
            Arg::new("HASH")
                .short('a')
                .long("hash")
                .help("Set the hash algorithm")
                .required(false)
                .num_args(1)
                .value_parser(PossibleValuesParser::new(HASH_ALGORITHMS)),
        )
}

pub async fn main(
    connection: &mut SqliteConnection,
    matches: &ArgMatches,
    progress_bar: &ProgressBar,
) -> SimpleResult<()> {
    let romfile_paths: Vec<&PathBuf> = matches.get_many::<PathBuf>("ROMS").unwrap().collect();
    let system = prompt_for_system(
        connection,
        matches
            .get_one::<String>("SYSTEM")
            .map(|s| FromStr::from_str(s).expect("Failed to parse number")),
    )
    .await?;
    let header = find_header_by_system_id(connection, system.id).await;
    let hash_algorithm = match matches.get_one::<String>("HASH").map(String::as_str) {
        Some("CRC") => HashAlgorithm::Crc,
        Some("MD5") => HashAlgorithm::Md5,
        Some(&_) | None => {
            match find_setting_by_key(connection, "HASH_ALGORITHM")
                .await
                .unwrap()
                .value
                .as_deref()
            {
                Some("CRC") => HashAlgorithm::Crc,
                Some("MD5") => HashAlgorithm::Md5,
                Some(&_) | None => bail!("Not possible"),
            }
        }
    };

    for romfile_path in romfile_paths {
        progress_bar.println(&format!("Processing \"{:?}\"", &romfile_path));
        let romfile_path = get_canonicalized_path(&romfile_path).await?;
        if romfile_path.is_dir().await {
            cfg_if! {
                if #[cfg(feature = "ird")] {
                    if romfile_path.join(PS3_DISC_SFB).is_file().await {
                        import_jbfolder(connection, progress_bar, &system, &romfile_path).await?;
                    } else {
                        let walker = WalkDir::new(&romfile_path).into_iter();
                        for entry in walker.filter_map(|e| e.ok()) {
                            if entry.path().is_file() {
                                import_rom(
                                    connection,
                                    progress_bar,
                                    &system,
                                    &header,
                                    &entry.path(),
                                    &hash_algorithm,
                                )
                                .await?;
                            }
                        }
                    }
                } else {
                    let walker = WalkDir::new(&romfile_path).into_iter();
                    for entry in walker.filter_map(|e| e.ok()) {
                        if entry.path().is_file() {
                            import_rom(
                                connection,
                                progress_bar,
                                &system,
                                &header,
                                &entry.path(),
                                &hash_algorithm,
                            )
                            .await?;
                        }
                    }
                }
            }
        } else {
            import_rom(
                connection,
                progress_bar,
                &system,
                &header,
                &romfile_path,
                &hash_algorithm,
            )
            .await?;
        }
        progress_bar.println("");
    }

    // mark games and system as complete if they are
    progress_bar.set_style(get_none_progress_style());
    progress_bar.enable_steady_tick(Duration::from_millis(100));
    progress_bar.set_message("Computing system completion");
    update_games_by_system_id_mark_complete(connection, system.id).await;
    cfg_if! {
        if #[cfg(feature = "ird")] {
            update_jbfolder_games_by_system_id_mark_complete(connection, system.id).await;
        }
    }
    update_system_mark_complete(connection, system.id).await;

    Ok(())
}

pub async fn import_rom<P: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let mut transaction = begin_transaction(connection).await;

    // abort if the romfile is already in the database
    if find_romfile_by_path(
        &mut transaction,
        romfile_path.as_ref().as_os_str().to_str().unwrap(),
    )
    .await
    .is_some()
    {
        progress_bar.println("Already in database");
        return Ok(());
    }

    let romfile_extension = romfile_path
        .as_ref()
        .extension()
        .unwrap_or(&OsString::new())
        .to_str()
        .unwrap()
        .to_lowercase();

    let system_directory = get_system_directory(&mut transaction, progress_bar, system).await?;

    if ARCHIVE_EXTENSIONS.contains(&romfile_extension.as_str()) {
        import_archive(
            &mut transaction,
            progress_bar,
            &system_directory,
            system,
            header,
            &romfile_path,
            &romfile_extension,
            hash_algorithm,
        )
        .await?;
    } else if CHD_EXTENSION == romfile_extension {
        cfg_if! {
            if #[cfg(feature = "chd")] {
                import_chd(
                    &mut transaction,
                    progress_bar,
                    &system_directory,
                    system,
                    header,
                    &romfile_path,
                    hash_algorithm,
                )
                .await?;
            } else {
                progress_bar.println("Please rebuild with the CHD feature enabled");
            }
        }
    } else if CSO_EXTENSION == romfile_extension {
        cfg_if! {
            if #[cfg(feature = "cso")] {
                import_cso(
                    &mut transaction,
                    progress_bar,
                    &system_directory,
                    system,
                    header,
                    &romfile_path,
                    hash_algorithm,
                )
                .await?;
            } else {
                progress_bar.println("Please rebuild with the CSO feature enabled");
            }
        }
    } else if RVZ_EXTENSION == romfile_extension {
        cfg_if! {
            if #[cfg(feature = "rvz")] {
                import_rvz(
                    &mut transaction,
                    progress_bar,
                    &system_directory,
                    system,
                    header,
                    &romfile_path,
                    hash_algorithm,
                )
                .await?;
            } else {
                progress_bar.println("Please rebuild with the RVZ feature enabled");
            }
        }
    } else {
        import_other(
            &mut transaction,
            progress_bar,
            &system_directory,
            system,
            header,
            &romfile_path,
            &romfile_extension,
            hash_algorithm,
        )
        .await?;
    }

    commit_transaction(transaction).await;

    Ok(())
}

#[cfg(feature = "ird")]
async fn import_jbfolder<P: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system: &System,
    folder_path: &P,
) -> SimpleResult<()> {
    let sfb_romfile_path = folder_path.as_ref().join(PS3_DISC_SFB);

    // abort if the romfile is already in the database
    if find_romfile_by_path(connection, sfb_romfile_path.as_os_str().to_str().unwrap())
        .await
        .is_some()
    {
        progress_bar.println("Already in database");
        return Ok(());
    }

    let mut transaction = begin_transaction(connection).await;

    // find the correct game
    let (size, md5) = get_size_and_hash(
        &mut transaction,
        progress_bar,
        &sfb_romfile_path,
        &None,
        1,
        1,
        &HashAlgorithm::Md5,
    )
    .await?;
    let sfb_rom =
        match find_sfb_rom_by_md5(&mut transaction, size, &md5, system, progress_bar).await? {
            Some(rom) => rom,
            None => {
                move_to_trash(&mut transaction, progress_bar, system, &folder_path).await?;
                return Ok(());
            }
        };
    let game = find_game_by_id(&mut transaction, sfb_rom.game_id).await;

    let system_directory = get_system_directory(&mut transaction, progress_bar, system).await?;

    let walker = WalkDir::new(folder_path.as_ref()).into_iter();
    for entry in walker.filter_map(|e| e.ok()) {
        if entry.path().is_file() {
            progress_bar.println(&format!(
                "Processing \"{}\"",
                &entry.path().as_os_str().to_str().unwrap()
            ));
            // force MD5 as IRD files only provide those
            let (size, md5) = get_size_and_hash(
                &mut transaction,
                progress_bar,
                &entry.path(),
                &None,
                1,
                1,
                &HashAlgorithm::Md5,
            )
            .await?;

            let rom: Option<Rom>;
            let mut roms = find_roms_without_romfile_by_size_and_md5_and_parent_id(
                &mut transaction,
                size,
                &md5,
                sfb_rom.parent_id.unwrap(),
            )
            .await;

            // abort if no match
            if roms.is_empty() {
                progress_bar.println("No match");
                continue;
            }

            // select the first rom if there is only one
            if roms.len() == 1 {
                rom = Some(roms.remove(0));
                progress_bar.println(&format!("Matches \"{}\"", rom.as_ref().unwrap().name));
            } else {
                // select the first rom that matches the file name if there multiple matches
                if let Some(rom_index) = roms.iter().position(|rom| {
                    entry
                        .path()
                        .as_os_str()
                        .to_str()
                        .unwrap()
                        .ends_with(&rom.name)
                }) {
                    rom = Some(roms.remove(rom_index));
                } else {
                    // let the user select the rom if all else fails
                    rom = prompt_for_rom(&mut roms, None)?;
                }
            }

            if let Some(rom) = rom {
                // abort if rom already has a file
                if rom.romfile_id.is_some() {
                    let romfile =
                        find_romfile_by_id(&mut transaction, rom.romfile_id.unwrap()).await;
                    progress_bar.println(&format!("Duplicate of \"{}\"", romfile.path));
                    continue;
                }

                // put arcade roms in subdirectories as their names aren't unique
                let new_path = system_directory.join(&game.name).join(&rom.name);

                // move file if needed
                rename_file(progress_bar, &entry.path(), &new_path, false).await?;

                // persist in database
                create_or_update_romfile(&mut transaction, &new_path, &[rom]).await;

                // remove directories if empty
                let mut directory = entry.path().parent().unwrap();
                while directory.read_dir().unwrap().next().is_none() {
                    remove_directory(progress_bar, &directory, false).await?;
                    if directory == entry.path() {
                        break;
                    }
                    directory = directory.parent().unwrap();
                }
            }
        }
    }

    commit_transaction(transaction).await;

    Ok(())
}

async fn import_archive<P: AsRef<Path>, Q: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system_directory: &Q,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    romfile_extension: &str,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let tmp_directory = create_tmp_directory(connection).await?;
    let sevenzip_infos = sevenzip::parse_archive(progress_bar, romfile_path)?;

    let mut roms_sevenzip_infos: Vec<(Rom, &sevenzip::ArchiveInfo)> = Vec::new();
    let mut game_ids: HashSet<i64> = HashSet::new();

    for sevenzip_info in &sevenzip_infos {
        progress_bar.println(&format!("Processing \"{}\"", &sevenzip_info.path));

        let size: u64;
        let hash: String;

        // system has a header, crc is absent, or selected checksum is not crc
        if header.is_some() || sevenzip_info.crc.is_empty() || hash_algorithm != &HashAlgorithm::Crc
        {
            let extracted_path = sevenzip::extract_files_from_archive(
                progress_bar,
                romfile_path,
                &[&sevenzip_info.path],
                &tmp_directory.path(),
            )?
            .remove(0);
            let size_hash = get_size_and_hash(
                connection,
                progress_bar,
                &extracted_path,
                header,
                1,
                1,
                hash_algorithm,
            )
            .await?;
            remove_file(progress_bar, &extracted_path, true).await?;
            size = size_hash.0;
            hash = size_hash.1;
        } else {
            size = sevenzip_info.size;
            hash = sevenzip_info.crc.clone();
        }

        match find_rom_by_hash(
            connection,
            progress_bar,
            size,
            &hash,
            system,
            hash_algorithm,
        )
        .await?
        {
            Some(rom) => {
                game_ids.insert(rom.game_id);
                roms_sevenzip_infos.push((rom, sevenzip_info));
            }
            None => {
                if sevenzip_infos.len() == 1 {
                    move_to_trash(connection, progress_bar, system, romfile_path).await?;
                }
            }
        }
    }

    // archive contains a single full game with no invalid file
    if roms_sevenzip_infos.len() == sevenzip_infos.len() && game_ids.len() == 1 {
        let game_id = game_ids.drain().last().unwrap();
        let rom_ids: HashSet<i64> = find_roms_by_game_id_no_parents(connection, game_id)
            .await
            .into_par_iter()
            .map(|rom| rom.id)
            .collect();
        if rom_ids
            .difference(
                &roms_sevenzip_infos
                    .par_iter()
                    .map(|(rom, _)| rom.id)
                    .collect(),
            )
            .count()
            == 0
        {
            let game = find_game_by_id(connection, game_id).await;
            for (rom, sevenzip_info) in &roms_sevenzip_infos {
                if sevenzip_info.path != rom.name {
                    sevenzip::rename_file_in_archive(
                        progress_bar,
                        romfile_path,
                        &sevenzip_info.path,
                        &rom.name,
                    )?;
                }
            }

            let new_path = match roms_sevenzip_infos.len() {
                1 => {
                    let rom = &roms_sevenzip_infos.get(0).unwrap().0;
                    let rom_extension = Path::new(&rom.name)
                        .extension()
                        .unwrap_or(&OsString::new())
                        .to_str()
                        .unwrap()
                        .to_lowercase();
                    let mut archive_path;
                    if system.arcade || PS3_EXTENSIONS.contains(&rom_extension.as_str()) {
                        archive_path = system_directory
                            .as_ref()
                            .join(format!("{}.{}", &game.name, &romfile_extension));
                    } else {
                        archive_path = system_directory.as_ref().join(&rom.name);
                        archive_path.set_extension(romfile_extension);
                    }
                    archive_path
                }
                _ => system_directory
                    .as_ref()
                    .join(format!("{}.{}", &game.name, &romfile_extension)),
            };

            // move file
            rename_file(progress_bar, romfile_path, &new_path, false).await?;

            // persist in database
            create_or_update_romfile(
                connection,
                &new_path,
                &roms_sevenzip_infos
                    .into_iter()
                    .map(|(rom, _)| rom)
                    .collect::<Vec<Rom>>(),
            )
            .await;

            return Ok(());
        }
    }

    // all other cases
    for (rom, sevenzip_info) in roms_sevenzip_infos {
        let extracted_path = sevenzip::extract_files_from_archive(
            progress_bar,
            romfile_path,
            &[&sevenzip_info.path],
            &tmp_directory.path(),
        )?
        .remove(0);

        let game = find_game_by_id(connection, rom.game_id).await;

        let new_path;
        // put arcade roms and JB folders in subdirectories
        if system.arcade || game.jbfolder {
            let game = find_game_by_id(connection, rom.game_id).await;
            new_path = system_directory.as_ref().join(&game.name).join(&rom.name)
        // use game name for PS3 updates and DLCs because rom name is usually gibberish
        } else if PS3_EXTENSIONS.contains(&romfile_extension) {
            new_path = system_directory
                .as_ref()
                .join(format!("{}.{}", &game.name, romfile_extension));
        } else {
            new_path = system_directory.as_ref().join(&rom.name);
        }

        // move file
        copy_file(progress_bar, &extracted_path, &new_path, false).await?;

        // persist in database
        create_or_update_romfile(connection, &new_path, &[rom]).await;
    }

    Ok(())
}

#[cfg(feature = "chd")]
async fn import_chd<P: AsRef<Path>, Q: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system_directory: &Q,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let tmp_directory = create_tmp_directory(connection).await?;

    let mut cue_path = romfile_path.as_ref().to_path_buf();
    cue_path.set_extension(CUE_EXTENSION);

    if cue_path.is_file().await {
        progress_bar.println("CUE file found, using multiple tracks mode");
        let (size, hash) = get_size_and_hash(
            connection,
            progress_bar,
            &cue_path,
            header,
            1,
            1,
            hash_algorithm,
        )
        .await?;
        let cue_rom = match find_rom_by_hash(
            connection,
            progress_bar,
            size,
            &hash,
            system,
            hash_algorithm,
        )
        .await?
        {
            Some(rom) => rom,
            None => {
                move_to_trash(connection, progress_bar, system, &cue_path).await?;
                return Ok(());
            }
        };

        let roms: Vec<Rom> = find_roms_by_game_id_no_parents(connection, cue_rom.game_id)
            .await
            .into_iter()
            .filter(|rom| rom.id != cue_rom.id)
            .collect();

        let names_sizes: Vec<(&str, u64)> = roms
            .iter()
            .map(|rom| (rom.name.as_str(), rom.size as u64))
            .collect();
        let bin_paths = chdman::extract_chd_to_multiple_tracks(
            progress_bar,
            romfile_path,
            &tmp_directory.path(),
            &names_sizes,
            true,
        )
        .await?;
        let mut hashes: Vec<String> = Vec::new();
        for (i, bin_path) in bin_paths.iter().enumerate() {
            let (_, hash) = get_size_and_hash(
                connection,
                progress_bar,
                &bin_path,
                header,
                i,
                bin_paths.len(),
                hash_algorithm,
            )
            .await?;
            hashes.push(hash);
            remove_file(progress_bar, &bin_path, true).await?;
        }

        if roms
            .iter()
            .enumerate()
            .any(|(i, rom)| &hashes[i] != rom.crc.as_ref().unwrap())
        {
            progress_bar.println("CRC mismatch");
            move_to_trash(connection, progress_bar, system, romfile_path).await?;
            return Ok(());
        }

        let new_cue_path = system_directory.as_ref().join(&cue_rom.name);
        let mut new_chd_path = new_cue_path.clone();
        new_chd_path.set_extension(CHD_EXTENSION);

        // move cue and chd if needed
        rename_file(progress_bar, &cue_path, &new_cue_path, false).await?;
        rename_file(progress_bar, romfile_path, &new_chd_path, false).await?;

        // persist in database
        create_or_update_romfile(connection, &new_cue_path, &[cue_rom]).await;
        create_or_update_romfile(connection, &new_chd_path, &roms).await;

        Ok(())
    } else {
        progress_bar.println("CUE file not found, using single track mode");
        let bin_path =
            chdman::extract_chd_to_single_track(progress_bar, romfile_path, &tmp_directory.path())
                .await?;
        let (size, hash) = get_size_and_hash(
            connection,
            progress_bar,
            &bin_path,
            header,
            1,
            1,
            hash_algorithm,
        )
        .await?;
        remove_file(progress_bar, &bin_path, true).await?;
        let rom = match find_rom_by_hash(
            connection,
            progress_bar,
            size,
            &hash,
            system,
            hash_algorithm,
        )
        .await?
        {
            Some(rom) => rom,
            None => {
                move_to_trash(connection, progress_bar, system, romfile_path).await?;
                return Ok(());
            }
        };

        let mut new_chd_path = system_directory.as_ref().join(&rom.name);
        new_chd_path.set_extension(CHD_EXTENSION);

        // move CHD if needed
        rename_file(progress_bar, romfile_path, &new_chd_path, false).await?;

        // persist in database
        create_or_update_romfile(connection, &new_chd_path, &[rom]).await;

        Ok(())
    }
}

#[cfg(feature = "cso")]
async fn import_cso<P: AsRef<Path>, Q: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system_directory: &Q,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let tmp_directory = create_tmp_directory(connection).await?;
    let iso_path = maxcso::extract_cso(progress_bar, romfile_path, &tmp_directory.path())?;
    let (size, hash) = get_size_and_hash(
        connection,
        progress_bar,
        &iso_path,
        header,
        1,
        1,
        hash_algorithm,
    )
    .await?;
    remove_file(progress_bar, &iso_path, true).await?;
    let rom = match find_rom_by_hash(
        connection,
        progress_bar,
        size,
        &hash,
        system,
        hash_algorithm,
    )
    .await?
    {
        Some(rom) => rom,
        None => {
            move_to_trash(connection, progress_bar, system, romfile_path).await?;
            return Ok(());
        }
    };

    let mut new_cso_path = system_directory.as_ref().join(&rom.name);
    new_cso_path.set_extension(CSO_EXTENSION);

    // move CSO if needed
    rename_file(progress_bar, romfile_path, &new_cso_path, false).await?;

    // persist in database
    create_or_update_romfile(connection, &new_cso_path, &[rom]).await;

    Ok(())
}

#[cfg(feature = "rvz")]
async fn import_rvz<P: AsRef<Path>, Q: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system_directory: &Q,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let tmp_directory = create_tmp_directory(connection).await?;
    let iso_path = dolphin::extract_rvz(progress_bar, romfile_path, &tmp_directory.path())?;
    let (size, hash) = get_size_and_hash(
        connection,
        progress_bar,
        &iso_path,
        header,
        1,
        1,
        hash_algorithm,
    )
    .await?;
    remove_file(progress_bar, &iso_path, true).await?;
    let rom = match find_rom_by_hash(
        connection,
        progress_bar,
        size,
        &hash,
        system,
        hash_algorithm,
    )
    .await?
    {
        Some(rom) => rom,
        None => {
            move_to_trash(connection, progress_bar, system, romfile_path).await?;
            return Ok(());
        }
    };

    let mut new_rvz_path = system_directory.as_ref().join(&rom.name);
    new_rvz_path.set_extension(RVZ_EXTENSION);

    // move RVZ if needed
    rename_file(progress_bar, romfile_path, &new_rvz_path, false).await?;

    // persist in database
    create_or_update_romfile(connection, &new_rvz_path, &[rom]).await;

    Ok(())
}

async fn import_other<P: AsRef<Path>, Q: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system_directory: &Q,
    system: &System,
    header: &Option<Header>,
    romfile_path: &P,
    romfile_extension: &str,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<()> {
    let (size, hash) = get_size_and_hash(
        connection,
        progress_bar,
        romfile_path,
        header,
        1,
        1,
        hash_algorithm,
    )
    .await?;
    let rom = match find_rom_by_hash(
        connection,
        progress_bar,
        size,
        &hash,
        system,
        hash_algorithm,
    )
    .await?
    {
        Some(rom) => rom,
        None => {
            move_to_trash(connection, progress_bar, system, romfile_path).await?;
            return Ok(());
        }
    };
    let game = find_game_by_id(connection, rom.game_id).await;

    let new_path;
    // put arcade roms and JB folders in subdirectories
    if system.arcade || game.jbfolder {
        let game = find_game_by_id(connection, rom.game_id).await;
        new_path = system_directory.as_ref().join(&game.name).join(&rom.name)
    // use game name for PS3 updates and DLCs because rom name is usually gibberish
    } else if PS3_EXTENSIONS.contains(&romfile_extension) {
        new_path = system_directory
            .as_ref()
            .join(format!("{}.{}", &game.name, romfile_extension));
    } else {
        new_path = system_directory.as_ref().join(&rom.name);
    }

    // move file if needed
    rename_file(progress_bar, romfile_path, &new_path, false).await?;

    // persist in database
    create_or_update_romfile(connection, &new_path, &[rom]).await;

    Ok(())
}

async fn find_rom_by_hash(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    size: u64,
    hash: &str,
    system: &System,
    hash_algorithm: &HashAlgorithm,
) -> SimpleResult<Option<Rom>> {
    let rom: Option<Rom>;
    let mut roms = match hash_algorithm {
        HashAlgorithm::Crc => {
            find_roms_without_romfile_by_size_and_crc_and_system_id(
                connection, size, hash, system.id,
            )
            .await
        }
        HashAlgorithm::Md5 => {
            find_roms_without_romfile_by_size_and_md5_and_system_id(
                connection, size, hash, system.id,
            )
            .await
        }
        HashAlgorithm::Sha1 => {
            find_roms_without_romfile_by_size_and_sha1_and_system_id(
                connection, size, hash, system.id,
            )
            .await
        }
    };

    // abort if no match
    if roms.is_empty() {
        progress_bar.println("No match");
        return Ok(None);
    }

    // let user choose the rom if there are multiple matches
    if roms.len() == 1 {
        rom = Some(roms.remove(0));
        progress_bar.println(&format!("Matches \"{}\"", rom.as_ref().unwrap().name));
    } else {
        let mut roms_games: Vec<(Rom, Game)> = vec![];
        for rom in roms {
            let game = find_game_by_id(connection, rom.game_id).await;
            roms_games.push((rom, game));
        }
        rom = prompt_for_rom_game(&mut roms_games)?;
    }

    // abort if rom already has a file
    if rom.is_some() && rom.as_ref().unwrap().romfile_id.is_some() {
        let romfile =
            find_romfile_by_id(connection, rom.as_ref().unwrap().romfile_id.unwrap()).await;
        progress_bar.println(&format!("Duplicate of \"{}\"", romfile.path));
        return Ok(None);
    }

    Ok(rom)
}

#[cfg(feature = "ird")]
async fn find_sfb_rom_by_md5(
    connection: &mut SqliteConnection,
    size: u64,
    md5: &str,
    system: &System,
    progress_bar: &ProgressBar,
) -> SimpleResult<Option<Rom>> {
    let rom: Option<Rom>;
    let mut roms = find_roms_without_romfile_by_name_and_size_and_md5_and_system_id(
        connection,
        PS3_DISC_SFB,
        size,
        md5,
        system.id,
    )
    .await;

    // abort if no match
    if roms.is_empty() {
        progress_bar.println("No match");
        return Ok(None);
    }

    // let user choose the rom if there are multiple matches
    if roms.len() == 1 {
        rom = Some(roms.remove(0));
        progress_bar.println(&format!("Matches \"{}\"", rom.as_ref().unwrap().name));
    } else {
        let mut roms_games: Vec<(Rom, Game)> = vec![];
        for rom in roms {
            let game = find_game_by_id(connection, rom.game_id).await;
            roms_games.push((rom, game));
        }
        rom = prompt_for_rom_game(&mut roms_games)?;
    }

    // abort if rom already has a file
    if rom.is_some() && rom.as_ref().unwrap().romfile_id.is_some() {
        let romfile =
            find_romfile_by_id(connection, rom.as_ref().unwrap().romfile_id.unwrap()).await;
        progress_bar.println(&format!("Duplicate of \"{}\"", romfile.path));
        return Ok(None);
    }

    Ok(rom)
}

async fn create_or_update_romfile<P: AsRef<Path>>(
    connection: &mut SqliteConnection,
    romfile_path: &P,
    roms: &[Rom],
) {
    let romfile = find_romfile_by_path(
        connection,
        romfile_path.as_ref().as_os_str().to_str().unwrap(),
    )
    .await;
    let romfile_id = match romfile {
        Some(romfile) => {
            update_romfile(
                connection,
                romfile.id,
                &romfile.path,
                romfile_path.as_ref().metadata().await.unwrap().len(),
            )
            .await;
            romfile.id
        }
        None => {
            create_romfile(
                connection,
                romfile_path.as_ref().as_os_str().to_str().unwrap(),
                romfile_path.as_ref().metadata().await.unwrap().len(),
            )
            .await
        }
    };
    for rom in roms {
        update_rom_romfile(connection, rom.id, Some(romfile_id)).await;
    }
}

async fn move_to_trash<P: AsRef<Path>>(
    connection: &mut SqliteConnection,
    progress_bar: &ProgressBar,
    system: &System,
    romfile_path: &P,
) -> SimpleResult<()> {
    let new_path = get_trash_directory(connection, progress_bar, system)
        .await?
        .join(romfile_path.as_ref().file_name().unwrap());
    rename_file(progress_bar, romfile_path, &new_path, false).await?;
    match find_romfile_by_path(connection, &new_path.as_os_str().to_str().unwrap()).await {
        Some(romfile) => {
            update_romfile(
                connection,
                romfile.id,
                new_path.as_os_str().to_str().unwrap(),
                new_path.metadata().await.unwrap().len(),
            )
            .await;
        }
        None => {
            create_romfile(
                connection,
                new_path.as_os_str().to_str().unwrap(),
                new_path.metadata().await.unwrap().len(),
            )
            .await;
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "chd"))]
mod test_chd_multiple_tracks;
#[cfg(all(test, feature = "chd"))]
mod test_chd_multiple_tracks_without_cue_should_fail;
#[cfg(all(test, feature = "chd"))]
mod test_chd_single_track;
#[cfg(all(test, feature = "cso"))]
mod test_cso;
#[cfg(test)]
mod test_original;
#[cfg(test)]
mod test_original_headered;
#[cfg(test)]
mod test_sevenzip_multiple_files_full_game;
#[cfg(test)]
mod test_sevenzip_multiple_files_headered_mixed_games;
#[cfg(test)]
mod test_sevenzip_multiple_files_mixed_games;
#[cfg(test)]
mod test_sevenzip_multiple_files_partial_game;
#[cfg(test)]
mod test_sevenzip_single_file;
#[cfg(test)]
mod test_sevenzip_single_file_headered;
#[cfg(test)]
mod test_zip_single_file;
