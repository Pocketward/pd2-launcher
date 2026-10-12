//! Where the Diablo II game folder is, and Diablo II installations the user
//! already has, which may be 1.14b or 1.14d.
//!
//! The game normally lives in the Managed prefix at
//! `prefix/drive_c/Program Files (x86)/Diablo II`. An existing installation
//! can instead be copied there, or used where it is. A folder
//! used in place is recorded by `prefix/.pd2launcher-game-folder.json` with
//! its path and identity (filesystem id and inode); it is opened by that path with
//! no symbolic link in any component and must still be the same directory,
//! or it is unavailable and nothing runs until it is back. Because the
//! record lives in the Managed prefix, Start over forgets it together with
//! the prefix and never touches the folder itself.
//!
//! Wine reaches a folder used in place through its default `Z:` drive (the
//! host root), so the Launcher creates no link of its own; the Launcher
//! itself only ever opens it by descriptor, below the proven directory.

use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    operation::{OperationControl, WorkPhase, WorkProgress},
    safe_fs::Directory,
};

/// The Managed game folder below the managed root.
pub(crate) const GAME_RELATIVE: &str = "prefix/drive_c/Program Files (x86)/Diablo II";
/// The same folder as Windows programs in the Managed prefix see it.
pub(crate) const MANAGED_WINDOWS_PATH: &str = r"C:\Program Files (x86)\Diablo II";
/// The record of a game folder used in place, in the Managed prefix.
pub(crate) const POINTER_NAME: &str = ".pd2launcher-game-folder.json";
/// The Launcher's own evidence that an existing installation was imported,
/// in the Managed prefix. It stands in for Blizzard's install logs.
pub(crate) const IMPORT_RECEIPT_NAME: &str = ".pd2launcher-imported-game.json";
const SCHEMA: u32 = 1;
const RECORD_LIMIT: u64 = 8 * 1024;
/// A game folder with more entries than this is not a Diablo II folder.
const MAX_TREE_ENTRIES: usize = 65_536;
const MAX_TREE_DEPTH: usize = 16;

/// A Diablo II version the Launcher accepts as the base game:
/// the size and SHA-256 of its `Game.exe`, and of its `Patch_D2.mpq` where
/// that file is the same in every language. The game is never patched:
/// Project Diablo 2 brings its own game files in `ProjectD2`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BaseGameVersion {
    pub(crate) game: (u64, &'static str),
    pub(crate) archive: Option<(u64, &'static str)>,
}

/// What Blizzard's current Diablo II and Lord of Destruction installers
/// write, which the Launcher installs: 1.14b. `Game.exe` (FileVersion
/// 1.14.1.68) is one binary for every language. `Patch_D2.mpq` ships inside
/// each language's installer, so only its presence is required.
pub(crate) const BASE_GAME_1_14B: BaseGameVersion = BaseGameVersion {
    game: (
        3_590_120,
        "0680e266b1fbbc8e7815b0a7c4f67267de38fd9063fceeae2da0306389a52178",
    ),
    archive: None,
};

/// Blizzard's 1.14d patch output, which installations made by older
/// releases (and many existing ones) have. The one patch served every
/// language, so both files are pinned.
pub(crate) const BASE_GAME_1_14D: BaseGameVersion = BaseGameVersion {
    game: (
        3_618_792,
        "631066c1649c4ea9ffe48bf97e24c00bca1f7a6759c21150f1a79982589adaaf",
    ),
    archive: Some((
        5_727_413,
        "531bad76d520eea8cb0601bea5a40e1ec52f51fe7b94a6892dc483d18170c607",
    )),
};

/// Every base game the Launcher and the Auditor accept.
pub(crate) const ACCEPTED_BASE_GAMES: [BaseGameVersion; 2] = [BASE_GAME_1_14B, BASE_GAME_1_14D];

/// Files a complete Diablo II installation needs, by canonical name. The
/// Auditor checks the same names for Diablo II and Lord of Destruction.
const BASE_FILES: [&str; 9] = [
    "Game.exe",
    "Patch_D2.mpq",
    "Diablo II.exe",
    "d2char.mpq",
    "d2data.mpq",
    "d2music.mpq",
    "d2sfx.mpq",
    "d2speech.mpq",
    "d2video.mpq",
];
const EXPANSION_FILES: [&str; 4] = ["d2exp.mpq", "d2xmusic.mpq", "d2xtalk.mpq", "d2xvideo.mpq"];
/// Not copied: Project Diablo 2 is installed fresh by its own step.
const PROJECT_D2: &str = "projectd2";

/// Where the game folder is.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GameFolder {
    Managed,
    External(ExternalGameFolder),
}

/// A game folder used where it is, and the identity it had when chosen.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExternalGameFolder {
    path: PathBuf,
    /// `f_fsid`: stable across reboots, unlike `st_dev` on btrfs.
    filesystem: u64,
    inode: u64,
}

impl ExternalGameFolder {
    /// The folder at `path` with the identity it has now.
    #[cfg(test)]
    pub(crate) fn at(path: &Path) -> Self {
        let file = std::fs::File::open(path).unwrap();
        Self {
            path: path.to_owned(),
            filesystem: crate::safe_fs::filesystem_id(&file).unwrap(),
            inode: file.metadata().unwrap().ino(),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Open the folder by its path (no symbolic link in any component) and
    /// prove it is still the directory that was chosen.
    pub(crate) fn open(&self) -> io::Result<Directory> {
        let directory = Directory::open(&self.path)?;
        let file = directory.try_clone_file()?;
        if crate::safe_fs::filesystem_id(&file)? != self.filesystem
            || file.metadata()?.ino() != self.inode
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the game folder is not the one that was chosen",
            ));
        }
        Ok(directory)
    }
}

/// The recorded game folder could not be established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum GameFolderError {
    /// The record exists but is not one the Launcher wrote.
    RecordInvalid,
    /// The recorded folder is gone, moved, replaced, or unreadable.
    Unavailable(PathBuf),
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PointerRecord {
    schema: u32,
    path: String,
    filesystem: u64,
    inode: u64,
}

impl GameFolder {
    /// The game folder this managed root uses. A missing record is the
    /// Managed folder; an unreadable or foreign record fails closed.
    pub(crate) fn read(root: &Directory) -> Result<Self, GameFolderError> {
        let prefix = match root.open_directory(Path::new("prefix")) {
            Ok(prefix) => prefix,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::Managed),
            Err(_) => return Err(GameFolderError::RecordInvalid),
        };
        let bytes = match read_bounded(&prefix, POINTER_NAME) {
            Ok(Some(bytes)) => bytes,
            Ok(None) => return Ok(Self::Managed),
            Err(()) => return Err(GameFolderError::RecordInvalid),
        };
        let record = serde_json::from_slice::<PointerRecord>(&bytes)
            .ok()
            .filter(|record| record.schema == SCHEMA)
            .ok_or(GameFolderError::RecordInvalid)?;
        let path = PathBuf::from(record.path);
        if !path.is_absolute() || windows_path_for(&path).is_none() {
            return Err(GameFolderError::RecordInvalid);
        }
        Ok(Self::External(ExternalGameFolder {
            path,
            filesystem: record.filesystem,
            inode: record.inode,
        }))
    }

    /// Read the record and prove an external folder is reachable and still
    /// the same one.
    pub(crate) fn resolve(root: &Directory) -> Result<Self, GameFolderError> {
        let folder = Self::read(root)?;
        if let Self::External(external) = &folder
            && external.open().is_err()
        {
            return Err(GameFolderError::Unavailable(external.path.clone()));
        }
        Ok(folder)
    }

    #[cfg(test)]
    pub(crate) fn is_external(&self) -> bool {
        matches!(self, Self::External(_))
    }

    /// Open the game folder. The Managed one is opened beneath the root.
    pub(crate) fn open(&self, root: &Directory) -> io::Result<Directory> {
        match self {
            Self::Managed => root.open_directory(Path::new(GAME_RELATIVE)),
            Self::External(external) => external.open(),
        }
    }

    /// The folder as Windows programs in the Managed prefix see it: one
    /// used in place on the Android host through the `G:` drive
    /// (`in_place_drive`).
    pub(crate) fn windows_path(&self) -> String {
        match self {
            Self::Managed => MANAGED_WINDOWS_PATH.to_owned(),
            Self::External(external) => {
                crate::in_place_drive::windows_path(&external.path, crate::android_host())
                    .expect("a recorded path was validated")
            }
        }
    }

    /// The folder's host path, for working directories and diagnostics.
    pub(crate) fn host_path(&self, managed_root: &Path) -> PathBuf {
        match self {
            Self::Managed => managed_root.join(GAME_RELATIVE),
            Self::External(external) => external.path.clone(),
        }
    }
}

/// `Z:\…` for an absolute host path whose name Windows programs can use.
/// Printable ASCII only: Wine stores other characters in its registry files
/// as escapes, which the Auditor does not decode, and Windows code pages
/// differ on them.
pub(crate) fn windows_path_for(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    if !text
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return None;
    }
    let relative = text.strip_prefix('/')?;
    if relative.is_empty()
        || relative.split('/').any(|component| {
            component.is_empty()
                || matches!(component, "." | "..")
                || component.chars().any(|character| {
                    character.is_control()
                        || matches!(character, '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|')
                })
        })
    {
        return None;
    }
    Some(format!(r"Z:\{}", relative.replace('/', "\\")))
}

/// Record `external` as the game folder. The caller holds the installation
/// lease and has proven the folder.
pub(crate) fn write_pointer(prefix: &Directory, external: &ExternalGameFolder) -> io::Result<()> {
    let record = PointerRecord {
        schema: SCHEMA,
        path: external
            .path
            .to_str()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path is not UTF-8"))?
            .to_owned(),
        filesystem: external.filesystem,
        inode: external.inode,
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record encode failed"))?;
    prefix.atomic_write(OsStr::new(POINTER_NAME), &bytes)
}

/// How an existing installation became the game.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ImportMode {
    /// Copied into the Managed game folder; the original is not used.
    Copy,
    /// Used where it is; Project Diablo 2 is added to that folder.
    InPlace,
}

/// The Launcher's evidence that an existing installation was imported.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImportReceipt {
    schema: u32,
    pub(crate) mode: ImportMode,
    /// The `InstallPath` the registry must name.
    pub(crate) install_path: String,
}

impl ImportReceipt {
    pub(crate) fn new(mode: ImportMode, install_path: String) -> Self {
        Self {
            schema: SCHEMA,
            mode,
            install_path,
        }
    }
}

/// Write the import receipt last, once the game files and the registry are
/// in place.
pub(crate) fn write_import_receipt(prefix: &Directory, receipt: &ImportReceipt) -> io::Result<()> {
    let bytes = serde_json::to_vec(receipt)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "receipt encode failed"))?;
    prefix.atomic_write(OsStr::new(IMPORT_RECEIPT_NAME), &bytes)
}

/// The import receipt, if this installation was imported. A receipt the
/// Launcher did not write is `Err`.
pub(crate) fn read_import_receipt(root: &Directory) -> Result<Option<ImportReceipt>, ()> {
    let prefix = match root.open_directory(Path::new("prefix")) {
        Ok(prefix) => prefix,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let Some(bytes) = read_bounded(&prefix, IMPORT_RECEIPT_NAME)? else {
        return Ok(None);
    };
    serde_json::from_slice::<ImportReceipt>(&bytes)
        .ok()
        .filter(|receipt| receipt.schema == SCHEMA)
        .map(Some)
        .ok_or(())
}

fn read_bounded(directory: &Directory, name: &str) -> Result<Option<Vec<u8>>, ()> {
    let file = match directory.open_file(Path::new(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(()),
    };
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| ())?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(());
    }
    Ok(Some(bytes))
}

/// Why a folder is not a Diablo II installation the Launcher can use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ExistingInstallProblem {
    /// The folder doesn't exist or isn't a folder.
    NotFound,
    /// The folder or one of its files can't be read.
    Unreadable,
    /// No Diablo II here.
    NotDiablo,
    /// Diablo II, but a version other than 1.14b or 1.14d (named when
    /// recognized).
    UnsupportedVersion(Option<GameVersion>),
    /// `Game.exe` names 1.14b or 1.14d but is not Blizzard's file.
    ModifiedGame,
    /// `Game.exe` is 1.14d but `Patch_D2.mpq` is not.
    ArchiveMismatch,
    /// Lord of Destruction isn't installed here.
    NoExpansion,
    /// A file the game needs is missing.
    MissingFile(&'static str),
    /// A symbolic link (or another special file) is part of the folder.
    Links,
    /// The folder is the launcher's own, or contains it.
    InsideLauncher,
    /// The folder's path can't be used by Windows programs.
    UnsupportedPath,
}

/// A Diablo II version recognized from `Game.exe`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GameVersion {
    V113,
    V114A,
    V114B,
    V114C,
    V114D,
}

impl GameVersion {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::V113 => "1.13",
            Self::V114A => "1.14a",
            Self::V114B => "1.14b",
            Self::V114C => "1.14c",
            Self::V114D => "1.14d",
        }
    }
}

/// A validated Diablo II + Lord of Destruction 1.14b or 1.14d folder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ExistingInstall {
    folder: ExternalGameFolder,
    /// Bytes of everything but ProjectD2.
    copy_bytes: u64,
    /// Bytes of its ProjectD2, which a copy on the desktop brings along.
    project_d2_bytes: u64,
    /// It holds a Project Diablo 2 to adopt: a ProjectD2 with the client's
    /// launch files (`Game.exe`, `ProjectDiablo.dll`) present and not empty.
    project_diablo: bool,
    /// The base games it was checked against (the accepted ones, except
    /// in tests).
    versions: Vec<BaseGameVersion>,
    /// No Project Diablo 2 folder, or one named exactly `ProjectD2`. The
    /// Launcher reads and writes that name; Windows programs would use a
    /// differently cased one, and the two would disagree in place.
    project_d2_canonical: bool,
}

impl ExistingInstall {
    pub(crate) fn path(&self) -> &Path {
        &self.folder.path
    }

    /// Bytes a copy writes: on the desktop its ProjectD2 too, which the
    /// install then adopts; the Android app copies a folder itself and
    /// leaves the engine's copy as it was.
    pub(crate) fn copy_bytes(&self) -> u64 {
        if copies_project_d2() {
            self.copy_bytes.saturating_add(self.project_d2_bytes)
        } else {
            self.copy_bytes
        }
    }

    /// Whether it holds a Project Diablo 2 the install adopts.
    pub(crate) fn project_diablo(&self) -> bool {
        self.project_diablo
    }

    pub(crate) fn folder(&self) -> &ExternalGameFolder {
        &self.folder
    }

    /// Validate again, for the folder recorded at the choice, right before
    /// it is imported: it must still be the same folder and still usable.
    pub(crate) fn revalidate(&self, managed_root: &Path) -> Result<Self, ExistingInstallProblem> {
        let fresh = validate_existing_with(&self.folder.path, managed_root, &self.versions)?;
        if fresh.folder != self.folder {
            return Err(ExistingInstallProblem::NotFound);
        }
        Ok(fresh)
    }
}

/// Check that `path` is the folder of a complete Diablo II + Lord of
/// Destruction 1.14b or 1.14d installation: `Game.exe` (and, for 1.14d,
/// `Patch_D2.mpq`) matches an accepted base game, every file the game needs
/// is present, and no symbolic link or special file is part of it. Names are
/// matched without regard to case, as Windows does. The folder is only read:
/// it is never patched.
pub(crate) fn validate_existing(
    path: &Path,
    managed_root: &Path,
) -> Result<ExistingInstall, ExistingInstallProblem> {
    validate_existing_with(path, managed_root, &ACCEPTED_BASE_GAMES)
}

pub(crate) fn validate_existing_with(
    path: &Path,
    managed_root: &Path,
    versions: &[BaseGameVersion],
) -> Result<ExistingInstall, ExistingInstallProblem> {
    if !path.is_absolute() {
        return Err(ExistingInstallProblem::NotFound);
    }
    if path.starts_with(managed_root) || managed_root.starts_with(path) {
        return Err(ExistingInstallProblem::InsideLauncher);
    }
    let directory = Directory::open(path).map_err(open_problem)?;
    let (filesystem, inode) = directory
        .try_clone_file()
        .and_then(|file| {
            Ok((
                crate::safe_fs::filesystem_id(&file)?,
                file.metadata()?.ino(),
            ))
        })
        .map_err(|_| ExistingInstallProblem::Unreadable)?;
    let names = case_insensitive_names(&directory)?;
    let has = |name: &str| names.contains_key(&name.to_ascii_lowercase());
    if !has("game.exe") && !has("d2data.mpq") {
        return Err(ExistingInstallProblem::NotDiablo);
    }
    let mut game = open_named(&directory, &names, "Game.exe")?
        .ok_or(ExistingInstallProblem::MissingFile("Game.exe"))?;
    let mut version = None;
    for candidate in versions {
        if matches_pin(&mut game, candidate.game)? {
            version = Some(candidate);
            break;
        }
    }
    let Some(version) = version else {
        return Err(match game_version(&mut game) {
            Some(GameVersion::V114B | GameVersion::V114D) => ExistingInstallProblem::ModifiedGame,
            other => ExistingInstallProblem::UnsupportedVersion(other),
        });
    };
    if let Some(archive_pin) = version.archive {
        let mut archive = open_named(&directory, &names, "Patch_D2.mpq")?
            .ok_or(ExistingInstallProblem::ArchiveMismatch)?;
        if !matches_pin(&mut archive, archive_pin)? {
            return Err(ExistingInstallProblem::ArchiveMismatch);
        }
    }
    if open_named(&directory, &names, "d2exp.mpq")?.is_none() {
        return Err(ExistingInstallProblem::NoExpansion);
    }
    for name in BASE_FILES.into_iter().chain(EXPANSION_FILES) {
        if open_named(&directory, &names, name)?.is_none() {
            return Err(ExistingInstallProblem::MissingFile(name));
        }
    }
    // Every part must be a real file or folder: a copy never follows a
    // link, and a folder used in place is audited without following one.
    let mut visited = 0;
    let mut project_d2_bytes = 0;
    let copy_bytes = tree_bytes(&directory, 0, &mut visited, true, &mut project_d2_bytes)?;
    let project_d2_canonical = names
        .get(PROJECT_D2)
        .is_none_or(|name| name == OsStr::new("ProjectD2"));
    let project_diablo = match names.get(PROJECT_D2) {
        Some(name) => {
            let project = directory
                .open_directory(Path::new(name))
                .map_err(open_problem)?;
            let inside = case_insensitive_names(&project)?;
            ["Game.exe", "ProjectDiablo.dll"].into_iter().all(|file| {
                open_named(&project, &inside, file)
                    .ok()
                    .flatten()
                    .and_then(|file| file.metadata().ok())
                    .is_some_and(|metadata| metadata.len() > 0)
            })
        }
        None => false,
    };
    let folder = ExternalGameFolder {
        path: path.to_owned(),
        filesystem,
        inode,
    };
    Ok(ExistingInstall {
        folder,
        copy_bytes,
        project_d2_bytes,
        project_diablo,
        versions: versions.to_vec(),
        project_d2_canonical,
    })
}

/// Whether the folder can be used where it is: Windows programs can name
/// it, and its Project Diablo 2 folder (if any) is named as the Launcher
/// names it.
pub(crate) fn usable_in_place(install: &ExistingInstall) -> bool {
    install.project_d2_canonical && windows_path_for(install.path()).is_some()
}

fn open_problem(error: io::Error) -> ExistingInstallProblem {
    match error.raw_os_error() {
        Some(code) if code == rustix::io::Errno::LOOP.raw_os_error() => {
            ExistingInstallProblem::Links
        }
        Some(code)
            if code == rustix::io::Errno::NOENT.raw_os_error()
                || code == rustix::io::Errno::NOTDIR.raw_os_error() =>
        {
            ExistingInstallProblem::NotFound
        }
        _ if error.kind() == io::ErrorKind::InvalidData => ExistingInstallProblem::NotFound,
        _ => ExistingInstallProblem::Unreadable,
    }
}

/// Lower-case name → the name on disk. Two names that differ only in case
/// are ambiguous for Windows programs and are refused.
fn case_insensitive_names(
    directory: &Directory,
) -> Result<BTreeMap<String, OsString>, ExistingInstallProblem> {
    let mut names = BTreeMap::new();
    for name in directory
        .names()
        .map_err(|_| ExistingInstallProblem::Unreadable)?
    {
        let Some(text) = name.to_str() else {
            continue;
        };
        if names
            .insert(text.to_ascii_lowercase(), name.clone())
            .is_some()
        {
            return Err(ExistingInstallProblem::Unreadable);
        }
    }
    Ok(names)
}

fn open_named(
    directory: &Directory,
    names: &BTreeMap<String, OsString>,
    canonical: &str,
) -> Result<Option<File>, ExistingInstallProblem> {
    let Some(actual) = names.get(&canonical.to_ascii_lowercase()) else {
        return Ok(None);
    };
    let file = directory
        .open_beneath(Path::new(actual), OFlags::RDONLY)
        .map_err(|error| match open_problem(error) {
            ExistingInstallProblem::NotFound => ExistingInstallProblem::Unreadable,
            problem => problem,
        })?;
    let metadata = file
        .metadata()
        .map_err(|_| ExistingInstallProblem::Unreadable)?;
    if !metadata.is_file() {
        return Err(ExistingInstallProblem::Links);
    }
    Ok(Some(file))
}

fn matches_pin(
    file: &mut File,
    (size, sha256): (u64, &str),
) -> Result<bool, ExistingInstallProblem> {
    let metadata = file
        .metadata()
        .map_err(|_| ExistingInstallProblem::Unreadable)?;
    if metadata.len() != size {
        return Ok(false);
    }
    Ok(sha256_hex(file).map_err(|_| ExistingInstallProblem::Unreadable)? == sha256)
}

fn sha256_hex(file: &mut File) -> io::Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// The version in `Game.exe`'s version resource, when it is one this
/// Launcher can name.
fn game_version(file: &mut File) -> Option<GameVersion> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 * 1024).read_to_end(&mut bytes).ok()?;
    let key: Vec<u8> = "FileVersion\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let start = bytes
        .windows(key.len())
        .position(|window| window == key.as_slice())?
        + key.len();
    // The value follows after 32-bit alignment padding.
    let value: String = bytes[start..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .skip_while(|unit| *unit == 0)
        .take_while(|unit| *unit != 0)
        .take(32)
        .filter_map(|unit| char::from_u32(u32::from(unit)))
        .collect();
    let parts: Vec<&str> = value.split(['.', ',']).map(str::trim).collect();
    match parts.as_slice() {
        ["1", "14", "0", ..] => Some(GameVersion::V114A),
        ["1", "14", "1", ..] => Some(GameVersion::V114B),
        ["1", "14", "2", ..] => Some(GameVersion::V114C),
        ["1", "14", "3", ..] => Some(GameVersion::V114D),
        ["1", "0", "13", ..] => Some(GameVersion::V113),
        _ => None,
    }
}

/// Total size of the regular files below `directory`, refusing any link or
/// special file. At the top level ProjectD2 is walked (links there are
/// refused too) and counted apart, in `project`.
fn tree_bytes(
    directory: &Directory,
    depth: usize,
    visited: &mut usize,
    top: bool,
    project: &mut u64,
) -> Result<u64, ExistingInstallProblem> {
    if depth > MAX_TREE_DEPTH {
        return Err(ExistingInstallProblem::Unreadable);
    }
    let mut total = 0_u64;
    for name in directory
        .names()
        .map_err(|_| ExistingInstallProblem::Unreadable)?
    {
        *visited += 1;
        if *visited > MAX_TREE_ENTRIES {
            return Err(ExistingInstallProblem::Unreadable);
        }
        let node = directory
            .open_beneath(Path::new(&name), OFlags::PATH)
            .map_err(open_problem)?;
        let metadata = node
            .metadata()
            .map_err(|_| ExistingInstallProblem::Unreadable)?;
        if metadata.file_type().is_symlink() {
            return Err(ExistingInstallProblem::Links);
        }
        let skipped = top && is_project_d2(&name);
        if metadata.is_dir() {
            let child = directory
                .open_directory(Path::new(&name))
                .map_err(open_problem)?;
            let bytes = tree_bytes(&child, depth + 1, visited, false, project)?;
            if skipped {
                *project = project.saturating_add(bytes);
            } else {
                total = total.saturating_add(bytes);
            }
        } else if metadata.is_file() {
            if skipped {
                *project = project.saturating_add(metadata.len());
            } else {
                total = total.saturating_add(metadata.len());
            }
        } else {
            return Err(ExistingInstallProblem::Links);
        }
    }
    Ok(total)
}

/// Whether a copied import brings the folder's ProjectD2 along: on the
/// desktop, where the install then adopts it; never on the Android host.
pub(crate) fn copies_project_d2() -> bool {
    !crate::android_host()
}

fn is_project_d2(name: &OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| name.eq_ignore_ascii_case(PROJECT_D2))
}

/// The canonical name of a file the Auditor checks, for a name that differs
/// from it only in case (`patch_d2.mpq` → `Patch_D2.mpq`), of the Project
/// Diablo 2 folder (`ProjectD2`, the name the Launcher uses), and on the
/// desktop of the characters folder (`Save`, the name the Launcher keeps
/// them under).
fn canonical_name(name: &OsStr) -> OsString {
    if is_project_d2(name) {
        return OsString::from("ProjectD2");
    }
    if !crate::android_host()
        && name
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(crate::save_keeper::SAVE))
    {
        return OsString::from(crate::save_keeper::SAVE);
    }
    name.to_str()
        .and_then(|text| {
            BASE_FILES
                .into_iter()
                .chain(EXPANSION_FILES)
                .find(|canonical| canonical.eq_ignore_ascii_case(text))
        })
        .map_or_else(|| name.to_owned(), OsString::from)
}

/// Copy the game folder into the empty Managed game folder `destination`:
/// every file and folder (ProjectD2 only with `project_d2`, under that
/// name), reflinked where the filesystem allows it, each verified by
/// SHA-256 against its source after the copy. On the desktop each file
/// keeps its source's times, so a character copied again is not mistaken
/// for one saved since.
/// No link is ever followed; the original is only read. A stop lands
/// between this process's own file operations, under the base-game stage
/// marker, so it is a proven-safe boundary that Start over resolves.
pub(crate) fn copy_game_folder(
    source: &Directory,
    destination: &Directory,
    total_bytes: u64,
    project_d2: bool,
    control: &OperationControl,
) -> io::Result<()> {
    let mut copied = 0_u64;
    copy_tree(
        source,
        destination,
        0,
        top_level(project_d2),
        total_bytes,
        &mut copied,
        control,
    )
    .inspect_err(|error| {
        if error.kind() == io::ErrorKind::Interrupted && control.is_cancelled() {
            control.mark_cancelled_safe();
        }
    })
}

/// Where `copy_tree` is: the game folder itself (with or without its
/// ProjectD2), or below it.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Level {
    Top { project_d2: bool },
    Below,
}

const fn top_level(project_d2: bool) -> Level {
    Level::Top { project_d2 }
}

fn copy_tree(
    source: &Directory,
    destination: &Directory,
    depth: usize,
    level: Level,
    total: u64,
    copied: &mut u64,
    control: &OperationControl,
) -> io::Result<()> {
    let top = matches!(level, Level::Top { .. });
    if depth > MAX_TREE_DEPTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the game folder is too deep",
        ));
    }
    let mut names = source.names()?;
    names.sort();
    // At the top a name takes its canonical form unless another entry has
    // it already (`save` beside `Save`): that one is copied under its own
    // name, so nothing is merged or lost (the keeper merges a second
    // `Save` at the next keep).
    let mut taken = names
        .iter()
        .filter(|name| top && canonical_name(name) == **name)
        .cloned()
        .collect::<BTreeSet<_>>();
    for name in names {
        if control.is_cancelled() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "copy stopped"));
        }
        if level == (Level::Top { project_d2: false }) && is_project_d2(&name) {
            continue;
        }
        let node = source.open_beneath(Path::new(&name), OFlags::PATH)?;
        let metadata = node.metadata()?;
        let target = match top.then(|| canonical_name(&name)) {
            Some(canonical) if canonical == name || taken.insert(canonical.clone()) => canonical,
            _ => name.clone(),
        };
        if metadata.is_dir() {
            let child = source.open_directory(Path::new(&name))?;
            let created = destination.ensure_directory(Path::new(&target))?;
            copy_tree(
                &child,
                &created,
                depth + 1,
                Level::Below,
                total,
                copied,
                control,
            )?;
        } else if metadata.is_file() {
            let mut input = source.open_beneath(Path::new(&name), OFlags::RDONLY)?;
            // Its times before the copy reads it.
            let times = input.metadata()?;
            if !times.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a game file changed while it was copied",
                ));
            }
            let mut output = destination.create_file(&target)?;
            copy_file(&mut input, &mut output, total, copied, control)?;
            if sha256_hex(&mut input)? != sha256_hex(&mut output)? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a copied game file did not verify",
                ));
            }
            if !crate::android_host() {
                output.set_times(
                    std::fs::FileTimes::new()
                        .set_accessed(times.accessed()?)
                        .set_modified(times.modified()?),
                )?;
                output.sync_all()?;
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the game folder contains a link or special file",
            ));
        }
    }
    destination.sync()
}

fn copy_file(
    input: &mut File,
    output: &mut File,
    total: u64,
    copied: &mut u64,
    control: &OperationControl,
) -> io::Result<()> {
    let size = input.metadata()?.len();
    // A reflink shares the blocks on the same filesystem; anything else is
    // a plain copy.
    if rustix::fs::ioctl_ficlone(&*output, &*input).is_err() {
        output.set_len(0)?;
        output.seek(SeekFrom::Start(0))?;
        input.seek(SeekFrom::Start(0))?;
        let mut buffer = vec![0_u8; 1024 * 1024];
        loop {
            if control.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "copy stopped"));
            }
            let read = input.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            output.write_all(&buffer[..read])?;
            report_copy(
                control,
                (*copied).saturating_add(output.stream_position()?),
                total,
            );
        }
    }
    output.sync_all()?;
    if output.metadata()?.len() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a copied game file has the wrong size",
        ));
    }
    *copied = copied.saturating_add(size);
    report_copy(control, *copied, total);
    Ok(())
}

fn report_copy(control: &OperationControl, copied: u64, total: u64) {
    control.report(
        WorkProgress::for_phase(WorkPhase::ImportGame).with_file_bytes(copied, Some(total)),
    );
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{fs, os::unix::fs::symlink};

    use super::*;

    /// A complete Diablo II folder whose `Game.exe` and `Patch_D2.mpq` are
    /// `game` and `archive`.
    pub(crate) fn write_game_folder(path: &Path, game: &[u8], archive: &[u8]) {
        fs::create_dir_all(path.join("Save")).unwrap();
        for name in BASE_FILES.into_iter().chain(EXPANSION_FILES) {
            fs::write(path.join(name), name.as_bytes()).unwrap();
        }
        fs::write(path.join("Game.exe"), game).unwrap();
        fs::write(path.join("Patch_D2.mpq"), archive).unwrap();
        fs::write(path.join("Save/hero.d2s"), b"hero").unwrap();
        fs::create_dir_all(path.join("ProjectD2")).unwrap();
        fs::write(path.join("ProjectD2/Game.exe"), b"pd2").unwrap();
    }

    fn version_resource(version: &str) -> Vec<u8> {
        let mut bytes = b"MZ fixture ".to_vec();
        for text in ["FileVersion\0", "\0", version, "\0"] {
            bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        }
        bytes
    }

    /// A base game whose `Game.exe` and `Patch_D2.mpq` are both pinned,
    /// as 1.14d is.
    pub(crate) fn pinned_version(game: &[u8], archive: &[u8]) -> BaseGameVersion {
        BaseGameVersion {
            game: pin(game),
            archive: Some(pin(archive)),
        }
    }

    fn validate_with_pins(
        path: &Path,
        game_pin: (u64, &'static str),
        archive_pin: (u64, &'static str),
    ) -> Result<ExistingInstall, ExistingInstallProblem> {
        // Blizzard's files can't be fixtures; the same rules run against
        // fixture pins.
        validate_existing_with(
            path,
            Path::new("/nonexistent/pd2-launcher"),
            &[BaseGameVersion {
                game: game_pin,
                archive: Some(archive_pin),
            }],
        )
    }

    pub(crate) fn pin(bytes: &[u8]) -> (u64, &'static str) {
        let digest: String = Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        (bytes.len() as u64, Box::leak(digest.into_boxed_str()))
    }

    #[test]
    fn a_folder_is_named_for_what_keeps_it_from_being_usable() {
        let fixture = tempfile::tempdir().unwrap();
        let root = Path::new("/nonexistent/pd2-launcher");
        let folder = fixture.path().join("Diablo II");

        // Not there, not a folder, not Diablo II.
        assert_eq!(
            validate_existing(&folder, root),
            Err(ExistingInstallProblem::NotFound)
        );
        fs::write(fixture.path().join("file"), b"x").unwrap();
        assert_eq!(
            validate_existing(&fixture.path().join("file"), root),
            Err(ExistingInstallProblem::NotFound)
        );
        fs::create_dir(&folder).unwrap();
        assert_eq!(
            validate_existing(&folder, root),
            Err(ExistingInstallProblem::NotDiablo)
        );

        // Another version is named from Game.exe's version resource.
        for (version, expected) in [
            ("1, 14, 2, 70", Some(GameVersion::V114C)),
            ("1.14.0.64", Some(GameVersion::V114A)),
            ("1, 0, 13, 60", Some(GameVersion::V113)),
            ("2.0.0", None),
        ] {
            write_game_folder(&folder, &version_resource(version), b"archive");
            assert_eq!(
                validate_existing(&folder, root),
                Err(ExistingInstallProblem::UnsupportedVersion(expected)),
                "{version}"
            );
        }
        // A Game.exe that names an accepted version but is not Blizzard's.
        for version in ["1.14.1.68", "1, 14, 3, 71"] {
            write_game_folder(&folder, &version_resource(version), b"archive");
            assert_eq!(
                validate_existing(&folder, root),
                Err(ExistingInstallProblem::ModifiedGame),
                "{version}"
            );
        }

        // Inside, or containing, the launcher's own folder.
        assert_eq!(
            validate_existing(&folder, fixture.path()),
            Err(ExistingInstallProblem::InsideLauncher)
        );
        assert_eq!(
            validate_existing(fixture.path(), &folder.join("pd2-launcher")),
            Err(ExistingInstallProblem::InsideLauncher)
        );
    }

    #[test]
    fn the_rest_of_the_folder_must_be_complete_and_link_free() {
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().join("Diablo II");
        let (game, archive) = (b"game-1.14d".as_slice(), b"archive-1.14d".as_slice());
        let (game_pin, archive_pin) = (pin(game), pin(archive));
        write_game_folder(&folder, game, archive);

        let valid = validate_with_pins(&folder, game_pin, archive_pin).expect("1.14d folder");
        assert_eq!(valid.path(), folder);
        // The desktop's copy brings ProjectD2 along (`pd2`, 3 bytes); it
        // holds no Project Diablo 2 to adopt without its launch files.
        assert!(valid.copy_bytes() > 0);
        assert!(!valid.project_diablo());
        assert_eq!(
            valid.copy_bytes() - 3,
            BASE_FILES
                .into_iter()
                .chain(EXPANSION_FILES)
                .filter(|name| !matches!(*name, "Game.exe" | "Patch_D2.mpq"))
                .map(|name| name.len() as u64)
                .sum::<u64>()
                + game.len() as u64
                + archive.len() as u64
                + 4
        );

        // A ProjectD2 with the client's launch files is one to adopt.
        fs::write(folder.join("ProjectD2/ProjectDiablo.dll"), b"dll").unwrap();
        let found = validate_with_pins(&folder, game_pin, archive_pin).expect("with PD2");
        assert!(found.project_diablo());
        assert_eq!(found.copy_bytes(), valid.copy_bytes() + 3);
        fs::write(folder.join("ProjectD2/ProjectDiablo.dll"), b"").unwrap();
        assert!(
            !validate_with_pins(&folder, game_pin, archive_pin)
                .unwrap()
                .project_diablo(),
            "an empty launch file is what a stopped copy leaves"
        );
        fs::remove_file(folder.join("ProjectD2/ProjectDiablo.dll")).unwrap();

        // Names match without regard to case, as Windows does.
        fs::rename(folder.join("Patch_D2.mpq"), folder.join("patch_d2.mpq")).unwrap();
        assert!(validate_with_pins(&folder, game_pin, archive_pin).is_ok());

        // A wrong Patch_D2.mpq.
        fs::write(folder.join("patch_d2.mpq"), b"older").unwrap();
        assert_eq!(
            validate_with_pins(&folder, game_pin, archive_pin),
            Err(ExistingInstallProblem::ArchiveMismatch)
        );
        fs::write(folder.join("patch_d2.mpq"), archive).unwrap();

        // No Lord of Destruction; a missing file.
        fs::remove_file(folder.join("d2exp.mpq")).unwrap();
        assert_eq!(
            validate_with_pins(&folder, game_pin, archive_pin),
            Err(ExistingInstallProblem::NoExpansion)
        );
        fs::write(folder.join("d2exp.mpq"), b"lod").unwrap();
        fs::remove_file(folder.join("d2music.mpq")).unwrap();
        assert_eq!(
            validate_with_pins(&folder, game_pin, archive_pin),
            Err(ExistingInstallProblem::MissingFile("d2music.mpq"))
        );
        fs::write(folder.join("d2music.mpq"), b"music").unwrap();

        // A link anywhere in it, even one pointing inside, or a linked
        // required file.
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, folder.join("Save/elsewhere")).unwrap();
        assert_eq!(
            validate_with_pins(&folder, game_pin, archive_pin),
            Err(ExistingInstallProblem::Links)
        );
        fs::remove_file(folder.join("Save/elsewhere")).unwrap();
        fs::rename(folder.join("d2xtalk.mpq"), outside.join("d2xtalk.mpq")).unwrap();
        symlink(outside.join("d2xtalk.mpq"), folder.join("d2xtalk.mpq")).unwrap();
        assert_eq!(
            validate_with_pins(&folder, game_pin, archive_pin),
            Err(ExistingInstallProblem::Links)
        );
        fs::remove_file(folder.join("d2xtalk.mpq")).unwrap();
        fs::write(folder.join("d2xtalk.mpq"), b"talk").unwrap();

        // The folder reached through a link.
        symlink(&folder, fixture.path().join("linked")).unwrap();
        assert_eq!(
            validate_with_pins(&fixture.path().join("linked"), game_pin, archive_pin),
            Err(ExistingInstallProblem::Links)
        );
    }

    #[test]
    fn either_accepted_base_game_is_used_as_it_is() {
        let fixture = tempfile::tempdir().unwrap();
        let root = Path::new("/nonexistent/pd2-launcher");
        let folder = fixture.path().join("Diablo II");
        // As 1.14b: only Game.exe is pinned; Patch_D2.mpq must be there.
        let unpatched = BaseGameVersion {
            game: pin(b"game-1.14b"),
            archive: None,
        };
        let patched = pinned_version(b"game-1.14d", b"archive-1.14d");
        let versions = [unpatched, patched];
        write_game_folder(&folder, b"game-1.14b", b"any language's archive");
        let before = fs::read(folder.join("Game.exe")).unwrap();
        assert!(validate_existing_with(&folder, root, &versions).is_ok());
        write_game_folder(&folder, b"game-1.14d", b"archive-1.14d");
        assert!(validate_existing_with(&folder, root, &versions).is_ok());
        // 1.14d pins its archive too.
        fs::write(folder.join("Patch_D2.mpq"), b"other").unwrap();
        assert_eq!(
            validate_existing_with(&folder, root, &versions),
            Err(ExistingInstallProblem::ArchiveMismatch)
        );
        write_game_folder(&folder, b"game-1.14b", b"archive");
        fs::remove_file(folder.join("Patch_D2.mpq")).unwrap();
        assert_eq!(
            validate_existing_with(&folder, root, &versions),
            Err(ExistingInstallProblem::MissingFile("Patch_D2.mpq"))
        );
        // Validation only reads: a 1.14b folder is never patched.
        assert_eq!(fs::read(folder.join("Game.exe")).unwrap(), before);
        // The release accepts Blizzard's 1.14b and 1.14d, nothing else.
        assert_eq!(ACCEPTED_BASE_GAMES, [BASE_GAME_1_14B, BASE_GAME_1_14D]);
        assert_ne!(BASE_GAME_1_14B.game.0, BASE_GAME_1_14D.game.0);
    }

    #[test]
    fn an_unreadable_folder_is_reported_as_such() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().unwrap();
        let folder = fixture.path().join("Diablo II");
        write_game_folder(&folder, b"game", b"archive");
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o000)).unwrap();
        if fs::read_dir(&folder).is_ok() {
            // Permissions don't apply here (running as root).
            fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
            return;
        }
        let result = validate_existing(&folder, Path::new("/nonexistent/x"));
        fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(result, Err(ExistingInstallProblem::Unreadable));
    }

    #[test]
    fn a_copy_is_verified_brings_project_d2_only_when_asked_and_uses_canonical_names() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("Diablo II");
        write_game_folder(&source_path, b"game", b"archive");
        fs::rename(
            source_path.join("Patch_D2.mpq"),
            source_path.join("patch_d2.mpq"),
        )
        .unwrap();
        let destination_path = fixture.path().join("copy");
        fs::create_dir(&destination_path).unwrap();
        let source = Directory::open(&source_path).unwrap();
        let destination = Directory::open(&destination_path).unwrap();
        let control = OperationControl::default();
        // A character's file keeps its time; a `save` folder takes the
        // name the Launcher keeps characters under.
        let saved = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        File::options()
            .write(true)
            .open(source_path.join("Save/hero.d2s"))
            .unwrap()
            .set_modified(saved)
            .unwrap();
        fs::rename(source_path.join("Save"), source_path.join("save")).unwrap();
        copy_game_folder(&source, &destination, 100, false, &control).expect("copied");
        fs::rename(source_path.join("save"), source_path.join("Save")).unwrap();
        assert_eq!(
            fs::read(destination_path.join("Patch_D2.mpq")).unwrap(),
            b"archive"
        );
        assert_eq!(
            fs::read(destination_path.join("Save/hero.d2s")).unwrap(),
            b"hero"
        );
        assert!(!destination_path.join("save").exists());
        assert_eq!(
            fs::metadata(destination_path.join("Save/hero.d2s"))
                .unwrap()
                .modified()
                .unwrap(),
            saved
        );
        assert!(!destination_path.join("ProjectD2").exists());
        // The desktop's copy brings ProjectD2 along, under that name, for the
        // install to adopt.
        assert!(copies_project_d2(), "the tests run as the desktop");
        fs::rename(source_path.join("ProjectD2"), source_path.join("projectd2")).unwrap();
        let with_project = fixture.path().join("with-project");
        fs::create_dir(&with_project).unwrap();
        copy_game_folder(
            &source,
            &Directory::open(&with_project).unwrap(),
            100,
            true,
            &OperationControl::default(),
        )
        .expect("copied");
        assert_eq!(
            fs::read(with_project.join("ProjectD2/Game.exe")).unwrap(),
            b"pd2"
        );
        assert!(!with_project.join("projectd2").exists());
        fs::rename(source_path.join("projectd2"), source_path.join("ProjectD2")).unwrap();
        // The original is untouched.
        assert!(source_path.join("patch_d2.mpq").is_file());
        assert!(source_path.join("ProjectD2/Game.exe").is_file());

        // A stopped copy reports it and leaves the rest for Start over.
        let stopped = fixture.path().join("stopped");
        fs::create_dir(&stopped).unwrap();
        let control = OperationControl::default();
        control.request_cancel();
        assert!(
            copy_game_folder(
                &source,
                &Directory::open(&stopped).unwrap(),
                1,
                true,
                &control
            )
            .is_err()
        );
        assert_eq!(
            control.cancellation_safety(),
            crate::operation::CancellationSafety::ProvenSafe
        );

        // A link found during the copy fails it.
        symlink("/etc", source_path.join("Save/link")).unwrap();
        let again = fixture.path().join("again");
        fs::create_dir(&again).unwrap();
        assert!(
            copy_game_folder(
                &source,
                &Directory::open(&again).unwrap(),
                1,
                true,
                &OperationControl::default()
            )
            .is_err()
        );
    }

    #[test]
    fn a_second_spelling_of_a_name_is_copied_under_its_own_name() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("Diablo II");
        write_game_folder(&source_path, b"game", b"archive");
        // `Save` and `save` both: the second keeps its name, nothing merged.
        fs::create_dir(source_path.join("save")).unwrap();
        fs::write(source_path.join("save/hero.d2s"), b"other hero").unwrap();
        fs::write(source_path.join("game.exe"), b"other game").unwrap();
        let destination_path = fixture.path().join("copy");
        fs::create_dir(&destination_path).unwrap();
        copy_game_folder(
            &Directory::open(&source_path).unwrap(),
            &Directory::open(&destination_path).unwrap(),
            100,
            false,
            &OperationControl::default(),
        )
        .expect("copied");
        let read = |name: &str| fs::read(destination_path.join(name)).unwrap();
        assert_eq!(read("Save/hero.d2s"), b"hero");
        assert_eq!(read("save/hero.d2s"), b"other hero");
        assert_eq!(read("Game.exe"), b"game");
        assert_eq!(read("game.exe"), b"other game");
    }

    #[test]
    fn a_differently_cased_project_diablo_folder_can_only_be_copied() {
        let fixture = tempfile::tempdir().unwrap();
        let game = fixture.path().join("Diablo II");
        write_game_folder(&game, b"game", b"archive");
        let valid = validate_with_pins(&game, pin(b"game"), pin(b"archive")).unwrap();
        assert!(usable_in_place(&valid));
        fs::rename(game.join("ProjectD2"), game.join("projectd2")).unwrap();
        let valid = validate_with_pins(&game, pin(b"game"), pin(b"archive")).unwrap();
        assert!(!usable_in_place(&valid));
        fs::remove_dir_all(game.join("projectd2")).unwrap();
        let valid = validate_with_pins(&game, pin(b"game"), pin(b"archive")).unwrap();
        assert!(usable_in_place(&valid));
    }

    #[test]
    fn a_folder_used_in_place_is_recorded_by_identity_and_fails_closed() {
        let fixture = tempfile::tempdir().unwrap();
        let root_path = fixture.path().join("root");
        fs::create_dir_all(root_path.join("prefix")).unwrap();
        let root = Directory::open(&root_path).unwrap();
        assert_eq!(GameFolder::read(&root), Ok(GameFolder::Managed));

        let game_path = fixture.path().join("My Games/Diablo II");
        fs::create_dir_all(&game_path).unwrap();
        let file = fs::File::open(&game_path).unwrap();
        let external = ExternalGameFolder {
            path: game_path.clone(),
            filesystem: crate::safe_fs::filesystem_id(&file).unwrap(),
            inode: file.metadata().unwrap().ino(),
        };
        write_pointer(
            &root.open_directory(Path::new("prefix")).unwrap(),
            &external,
        )
        .unwrap();
        let folder = GameFolder::resolve(&root).expect("available");
        assert!(folder.is_external());
        assert_eq!(folder.host_path(&root_path), game_path);
        assert!(folder.windows_path().starts_with(r"Z:\"));
        assert!(folder.windows_path().ends_with(r"\My Games\Diablo II"));

        // Replaced by another folder with the same name: unavailable.
        fs::rename(&game_path, fixture.path().join("moved")).unwrap();
        fs::create_dir_all(&game_path).unwrap();
        assert_eq!(
            GameFolder::resolve(&root),
            Err(GameFolderError::Unavailable(game_path.clone()))
        );
        // Gone: unavailable.
        fs::remove_dir(&game_path).unwrap();
        assert_eq!(
            GameFolder::resolve(&root),
            Err(GameFolderError::Unavailable(game_path.clone()))
        );
        // Back where it was: available again.
        fs::rename(fixture.path().join("moved"), &game_path).unwrap();
        assert!(GameFolder::resolve(&root).is_ok());
        // Reached through a link: unavailable.
        fs::rename(&game_path, fixture.path().join("real")).unwrap();
        symlink(fixture.path().join("real"), &game_path).unwrap();
        assert!(matches!(
            GameFolder::resolve(&root),
            Err(GameFolderError::Unavailable(_))
        ));

        // A record the launcher didn't write fails closed.
        fs::write(root_path.join("prefix").join(POINTER_NAME), b"{}").unwrap();
        assert_eq!(GameFolder::read(&root), Err(GameFolderError::RecordInvalid));
        fs::write(
            root_path.join("prefix").join(POINTER_NAME),
            br#"{"schema":1,"path":"relative","filesystem":1,"inode":2}"#,
        )
        .unwrap();
        assert_eq!(GameFolder::read(&root), Err(GameFolderError::RecordInvalid));
    }

    #[test]
    fn windows_paths_use_the_z_drive_and_refuse_unusable_names() {
        assert_eq!(
            windows_path_for(Path::new("/home/deck/Games/Diablo II")).as_deref(),
            Some(r"Z:\home\deck\Games\Diablo II")
        );
        assert_eq!(windows_path_for(Path::new("relative")), None);
        assert_eq!(windows_path_for(Path::new("/")), None);
        assert_eq!(windows_path_for(Path::new("/games/a:b")), None);
        assert_eq!(windows_path_for(Path::new("/games/a\\b")), None);
        assert_eq!(windows_path_for(Path::new("/home/zoë/Diablo II")), None);
    }

    #[test]
    fn the_import_receipt_round_trips_and_a_foreign_one_fails_closed() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("prefix")).unwrap();
        let root = Directory::open(fixture.path()).unwrap();
        assert_eq!(read_import_receipt(&root), Ok(None));
        let receipt = ImportReceipt::new(ImportMode::InPlace, r"Z:\games\Diablo II".to_owned());
        write_import_receipt(&root.open_directory(Path::new("prefix")).unwrap(), &receipt).unwrap();
        assert_eq!(read_import_receipt(&root), Ok(Some(receipt)));
        fs::write(
            fixture.path().join("prefix").join(IMPORT_RECEIPT_NAME),
            br#"{"schema":1,"mode":"copy"}"#,
        )
        .unwrap();
        assert_eq!(read_import_receipt(&root), Err(()));
    }
}
