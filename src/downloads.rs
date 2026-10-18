//! The Download step's durable result.
//!
//! After the Download job verified everything the remaining install needs,
//! it records each downloaded file below the managed root with its size and
//! identity (filesystem id, inode, modification time) in `cache/downloads.json`.
//! "Downloads complete" is derived from that record and the files as they
//! are now: every recorded file must still be the very file that was
//! verified. Nothing is re-hashed here; every file is verified again, by
//! its own rules, right before it is used.

use std::{
    ffi::OsStr,
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::Path,
};

use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};

use crate::{InstallationStatus, safe_fs::Directory};

pub(crate) const RECEIPT_NAME: &str = "downloads.json";
const SCHEMA: u32 = 1;
const RECEIPT_LIMIT: u64 = 4 * 1024 * 1024;
const MAX_ITEMS: usize = 16_384;
const MAX_DEPTH: usize = 4;

/// What the downloads were for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DownloadPlan {
    /// Everything, including Blizzard's installers.
    Installers,
    /// Everything but Blizzard's installers: the game comes from a Diablo II
    /// the user already has.
    ExistingInstall,
}

impl DownloadPlan {
    /// Whether downloads made for `self` also serve `other`.
    pub(crate) const fn covers(self, other: Self) -> bool {
        matches!(
            (self, other),
            (Self::Installers, _) | (Self::ExistingInstall, Self::ExistingInstall)
        )
    }
}

/// How far back a Download reaches: the earliest installation step its
/// files serve. A Download skips what installed evidence covered, so one
/// made when only the client was missing holds no Blizzard installers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DownloadReach {
    BaseGame,
    Expansion,
    /// A Download recorded at the retired 1.14d patch step held
    /// no Blizzard installers: it serves the client step.
    #[serde(alias = "patch")]
    Client,
    Runtime,
}

/// What the installation needs from the downloads now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DownloadNeed {
    pub(crate) reach: DownloadReach,
    /// The Wine prefix must be (re)built, so its components are needed.
    pub(crate) prefix: bool,
}

impl DownloadNeed {
    /// The need for an installation at `status`. States that are not a
    /// missing step (ready, interrupted, damaged) ask for nothing more.
    pub(crate) const fn for_status(status: InstallationStatus, prefix: bool) -> Self {
        let reach = match status {
            InstallationStatus::NeedsBaseGame => DownloadReach::BaseGame,
            InstallationStatus::NeedsExpansion => DownloadReach::Expansion,
            InstallationStatus::NeedsClient => DownloadReach::Client,
            _ => return Self::NOTHING,
        };
        Self { reach, prefix }
    }

    pub(crate) const NOTHING: Self = Self {
        reach: DownloadReach::Runtime,
        prefix: false,
    };
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    plan: DownloadPlan,
    reach: DownloadReach,
    /// Whether the Wine prefix components were downloaded.
    prefix: bool,
    total_bytes: u64,
    items: Vec<Item>,
}

#[derive(Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct Item {
    path: String,
    size: u64,
    /// `f_fsid`: stable across reboots, unlike `st_dev` on btrfs.
    filesystem: u64,
    inode: u64,
    modified: i64,
    modified_nanos: i64,
}

/// The recorded Download result, as it holds now.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DownloadReadiness {
    /// Nothing recorded, or a recorded file changed or is gone.
    Incomplete,
    /// Every recorded file is still the one that was verified.
    Complete {
        plan: DownloadPlan,
        total_bytes: u64,
        reach: DownloadReach,
        prefix: bool,
    },
}

impl DownloadReadiness {
    /// Whether the downloads serve `plan` for what the installation needs
    /// now. An existing installation needs no Blizzard installers, so only
    /// the prefix components matter for it.
    pub(crate) fn serves(self, plan: DownloadPlan, need: DownloadNeed) -> bool {
        match self {
            Self::Incomplete => false,
            Self::Complete {
                plan: made,
                reach,
                prefix,
                ..
            } => {
                made.covers(plan)
                    && (plan == DownloadPlan::ExistingInstall || reach <= need.reach)
                    && (prefix || !need.prefix)
            }
        }
    }

    /// This result, or `Incomplete` when it doesn't serve `need` for the
    /// plan it was made for.
    pub(crate) fn for_need(self, need: DownloadNeed) -> Self {
        match self {
            Self::Complete { plan, .. } if !self.serves(plan, need) => Self::Incomplete,
            other => other,
        }
    }
}

/// Record `paths` (files, or folders whose files are all recorded),
/// relative to the managed root, as the verified Download result. The
/// caller holds the installation lease and verified every file.
pub(crate) fn record(
    root: &Directory,
    plan: DownloadPlan,
    need: DownloadNeed,
    paths: &[String],
) -> io::Result<u64> {
    let mut items = Vec::new();
    for path in paths {
        collect(root, path, 0, &mut items)?;
    }
    items.sort_by(|left, right| left.path.cmp(&right.path));
    items.dedup_by(|left, right| left.path == right.path);
    let total_bytes = items.iter().map(|item| item.size).sum();
    let record = Record {
        schema: SCHEMA,
        plan,
        reach: need.reach,
        prefix: need.prefix,
        total_bytes,
        items,
    };
    let bytes = serde_json::to_vec(&record)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "receipt encode failed"))?;
    root.ensure_directory(Path::new("cache"))?
        .atomic_write(OsStr::new(RECEIPT_NAME), &bytes)?;
    Ok(total_bytes)
}

/// Forget the Download result (a new download is about to replace it).
pub(crate) fn clear(root: &Directory) -> io::Result<()> {
    let cache = match root.open_directory(Path::new("cache")) {
        Ok(cache) => cache,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match cache.open_file(Path::new(RECEIPT_NAME)) {
        Ok(file) => cache.remove_open_file_bounded(OsStr::new(RECEIPT_NAME), &file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// The installer downloads the player may remove once the game works
/// (Android only): the game files
/// Blizzard's downloaders fetched and the Project Diablo 2 client cache,
/// about 2.5 GB. Kept, they let a reinstall reuse them; only the player's
/// Remove removes them. The Android app sizes the same folders for its
/// Storage entry (`InstallerDownloads`). The environment dependency
/// installers (Gecko, Mono, .NET and the prefix's Windows Installer cache)
/// are not here: the app removes those by itself once the game is Ready.
pub(crate) const INSTALLER_DOWNLOADS: &[(&str, &str)] = &[
    ("cache/artifacts", "d2-payload"),
    ("cache/artifacts", "lod-payload"),
    ("cache", "client"),
];

/// Remove what [`INSTALLER_DOWNLOADS`] names and forget the Download result.
/// The caller holds the installation lease. Whether anything was removed.
pub(crate) fn remove_installer_downloads(root: &Directory) -> io::Result<bool> {
    let mut removed = false;
    for (parent, name) in INSTALLER_DOWNLOADS {
        let parent = match root.open_directory(Path::new(parent)) {
            Ok(parent) => parent,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        match parent.open_directory(Path::new(name)) {
            Ok(_) => {
                parent.remove_owned_tree(OsStr::new(name))?;
                removed = true;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if removed {
        clear(root)?;
    }
    Ok(removed)
}

/// The recorded Download result, checked against the files as they are.
/// Anything unexpected is simply `Incomplete`: downloading again is safe.
pub(crate) fn readiness(root: &Directory) -> DownloadReadiness {
    let Some(record) = read(root) else {
        return DownloadReadiness::Incomplete;
    };
    if record.schema != SCHEMA || record.items.is_empty() || record.items.len() > MAX_ITEMS {
        return DownloadReadiness::Incomplete;
    }
    // Downloads made for the row's earlier runtime lack the current one,
    // which an install needs unless a prefix keeps the earlier runtime.
    if let Some(retained) = crate::runtime::retained() {
        let archive = format!("cache/artifacts/{}", retained.cache_name);
        if record.items.iter().any(|item| item.path == archive) {
            return DownloadReadiness::Incomplete;
        }
    }
    for item in &record.items {
        match identify(root, &item.path) {
            Ok(Some(current)) if current.same(item) => {}
            _ => return DownloadReadiness::Incomplete,
        }
    }
    DownloadReadiness::Complete {
        plan: record.plan,
        total_bytes: record.total_bytes,
        reach: record.reach,
        prefix: record.prefix,
    }
}

fn read(root: &Directory) -> Option<Record> {
    let file = root
        .open_directory(Path::new("cache"))
        .and_then(|cache| cache.open_file(Path::new(RECEIPT_NAME)))
        .ok()?;
    let mut bytes = Vec::new();
    file.take(RECEIPT_LIMIT + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > RECEIPT_LIMIT {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

fn collect(root: &Directory, path: &str, depth: usize, items: &mut Vec<Item>) -> io::Result<()> {
    if depth > MAX_DEPTH || items.len() > MAX_ITEMS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "download record is too large",
        ));
    }
    let node = root.open_beneath(Path::new(path), OFlags::PATH)?;
    let metadata = node.metadata()?;
    if metadata.is_dir() {
        let directory = root.open_directory(Path::new(path))?;
        for name in directory.names()? {
            let name = name.to_str().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "download name is not UTF-8")
            })?;
            collect(root, &format!("{path}/{name}"), depth + 1, items)?;
        }
        return Ok(());
    }
    match identify(root, path)? {
        Some(item) => {
            items.push(item);
            Ok(())
        }
        None => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a download is not a regular file",
        )),
    }
}

impl Item {
    /// The recorded file, unchanged. On the Android host the filesystem id is
    /// not compared (see `safe_fs::same_recorded_file`).
    fn same(&self, recorded: &Self) -> bool {
        self.path == recorded.path
            && self.size == recorded.size
            && crate::safe_fs::same_recorded_file(
                recorded.filesystem,
                recorded.inode,
                self.filesystem,
                self.inode,
            )
            && self.modified == recorded.modified
            && self.modified_nanos == recorded.modified_nanos
    }
}

/// The identity of the regular file at `path`, never following a link.
fn identify(root: &Directory, path: &str) -> io::Result<Option<Item>> {
    let node = root.open_beneath(Path::new(path), OFlags::PATH)?;
    let metadata = node.metadata()?;
    if !metadata.is_file() {
        return Ok(None);
    }
    Ok(Some(Item {
        path: path.to_owned(),
        size: metadata.len(),
        filesystem: crate::safe_fs::filesystem_id(&node)?,
        inode: metadata.ino(),
        modified: metadata.mtime(),
        modified_nanos: metadata.mtime_nsec(),
    }))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    #[test]
    fn downloads_holding_the_earlier_runtime_archive_are_downloaded_again() {
        let fixture = tempfile::tempdir().unwrap();
        let root_path = fixture.path();
        fs::create_dir_all(root_path.join("cache/artifacts")).unwrap();
        let root = Directory::open(root_path).unwrap();
        let need = DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, true);
        let mut paths = vec!["cache/artifacts/gecko.verified".to_owned()];
        fs::write(root_path.join(&paths[0]), b"gecko").unwrap();
        record(&root, DownloadPlan::Installers, need, &paths).unwrap();
        assert!(readiness(&root).serves(DownloadPlan::Installers, need));

        let Some(retained) = crate::runtime::retained() else {
            return;
        };
        let archive = format!("cache/artifacts/{}", retained.cache_name);
        fs::write(root_path.join(&archive), b"earlier runtime").unwrap();
        paths.push(archive);
        record(&root, DownloadPlan::Installers, need, &paths).unwrap();
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);
    }

    #[test]
    fn the_result_holds_only_while_every_recorded_file_is_unchanged() {
        let fixture = tempfile::tempdir().unwrap();
        let root_path = fixture.path();
        fs::create_dir_all(root_path.join("cache/artifacts/d2-payload/DirectX")).unwrap();
        fs::write(root_path.join("cache/artifacts/gecko.verified"), b"gecko").unwrap();
        fs::write(
            root_path.join("cache/artifacts/d2-payload/Installer.exe"),
            b"installer",
        )
        .unwrap();
        fs::write(
            root_path.join("cache/artifacts/d2-payload/DirectX/dxsetup.exe"),
            b"dx",
        )
        .unwrap();
        let root = Directory::open(root_path).unwrap();
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);

        let fresh = DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, true);
        let total = record(
            &root,
            DownloadPlan::Installers,
            fresh,
            &[
                "cache/artifacts/gecko.verified".to_owned(),
                "cache/artifacts/d2-payload".to_owned(),
            ],
        )
        .unwrap();
        assert_eq!(total, 5 + 9 + 2);
        let complete = readiness(&root);
        assert_eq!(
            complete,
            DownloadReadiness::Complete {
                plan: DownloadPlan::Installers,
                total_bytes: 16,
                reach: DownloadReach::BaseGame,
                prefix: true,
            }
        );
        assert!(complete.serves(DownloadPlan::Installers, fresh));
        assert!(complete.serves(DownloadPlan::ExistingInstall, fresh));
        assert_eq!(complete.for_need(fresh), complete);

        // A replaced file (new inode) is no longer the verified one.
        fs::remove_file(root_path.join("cache/artifacts/d2-payload/DirectX/dxsetup.exe")).unwrap();
        fs::write(
            root_path.join("cache/artifacts/d2-payload/DirectX/dxsetup.exe"),
            b"dx",
        )
        .unwrap();
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);

        // Downloads for an existing installation don't serve an install
        // from Blizzard's installers.
        record(
            &root,
            DownloadPlan::ExistingInstall,
            fresh,
            &["cache/artifacts/gecko.verified".to_owned()],
        )
        .unwrap();
        let existing = readiness(&root);
        assert!(existing.serves(DownloadPlan::ExistingInstall, fresh));
        assert!(!existing.serves(DownloadPlan::Installers, fresh));

        // A removed file, a foreign record, and a cleared record.
        fs::remove_file(root_path.join("cache/artifacts/gecko.verified")).unwrap();
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);
        fs::write(root_path.join("cache").join(RECEIPT_NAME), b"{}").unwrap();
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);
        clear(&root).unwrap();
        assert!(!root_path.join("cache").join(RECEIPT_NAME).exists());
        clear(&root).unwrap();
    }

    #[test]
    fn removing_installer_downloads_keeps_everything_else() {
        let fixture = tempfile::tempdir().unwrap();
        let root_path = fixture.path();
        let removed = [
            "cache/artifacts/d2-payload/Installer Tome.mpq",
            "cache/artifacts/d2-payload/DirectX/dxsetup.exe",
            "cache/artifacts/lod-payload/Installer Tome.mpq",
            "cache/client/objects/a",
            "cache/client/manifest.json",
        ];
        let kept = [
            "cache/artifacts/d2-wrapper.verified",
            "cache/artifacts/pd2-bootstrap.verified",
            "cache/artifacts/wine-gecko-2.47.4-x86.verified",
            "cache/xdg/x",
            "game/Diablo II/Game.exe",
            "game/Diablo II/ProjectD2/Game.exe",
            "prefix/system.reg",
            "prefix/drive_c/windows/Installer/5923.msi",
            "prefix/drive_c/windows/Installer/{GUID}/cached.msi",
        ];
        for path in removed.iter().chain(&kept) {
            let path = root_path.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"x").unwrap();
        }
        let root = Directory::open(root_path).unwrap();
        record(
            &root,
            DownloadPlan::Installers,
            DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, true),
            &[
                "cache/artifacts/d2-payload".to_owned(),
                "cache/client".to_owned(),
            ],
        )
        .unwrap();

        assert!(remove_installer_downloads(&root).unwrap());
        for path in [
            "cache/artifacts/d2-payload",
            "cache/artifacts/lod-payload",
            "cache/client",
        ] {
            assert!(!root_path.join(path).exists(), "{path}");
        }
        for path in kept {
            assert!(root_path.join(path).is_file(), "{path}");
        }
        assert!(!root_path.join("cache").join(RECEIPT_NAME).exists());
        // Nothing left, and no cache at all, are fine.
        assert!(!remove_installer_downloads(&root).unwrap());
        let empty = tempfile::tempdir().unwrap();
        let empty = Directory::open(empty.path()).unwrap();
        assert!(!remove_installer_downloads(&empty).unwrap());
    }

    #[test]
    fn a_linked_download_is_never_recorded() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("cache/artifacts")).unwrap();
        fs::write(fixture.path().join("outside"), b"x").unwrap();
        std::os::unix::fs::symlink(
            fixture.path().join("outside"),
            fixture.path().join("cache/artifacts/link"),
        )
        .unwrap();
        let root = Directory::open(fixture.path()).unwrap();
        assert!(
            record(
                &root,
                DownloadPlan::Installers,
                DownloadNeed::NOTHING,
                &["cache/artifacts/link".to_owned()]
            )
            .is_err()
        );
        assert_eq!(readiness(&root), DownloadReadiness::Incomplete);
    }

    #[test]
    fn downloads_serve_only_the_steps_they_reached() {
        let fixture = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("cache/client")).unwrap();
        fs::write(fixture.path().join("cache/client/manifest.json"), b"{}").unwrap();
        let root = Directory::open(fixture.path()).unwrap();
        // Made when only the client was missing and the prefix was sound.
        record(
            &root,
            DownloadPlan::Installers,
            DownloadNeed::for_status(InstallationStatus::NeedsClient, false),
            &["cache/client".to_owned()],
        )
        .unwrap();
        let made = readiness(&root);
        let client = DownloadNeed::for_status(InstallationStatus::NeedsClient, false);
        assert!(made.serves(DownloadPlan::Installers, client));
        assert_eq!(made.for_need(client), made);
        // After Start over the game and the prefix are needed again: these
        // downloads hold neither Blizzard's installers nor the prefix parts.
        let fresh = DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, true);
        assert!(!made.serves(DownloadPlan::Installers, fresh));
        assert_eq!(made.for_need(fresh), DownloadReadiness::Incomplete);
        let unprefixed = DownloadNeed::for_status(InstallationStatus::NeedsBaseGame, false);
        assert!(!made.serves(DownloadPlan::Installers, unprefixed));
        // An existing installation needs no Blizzard installers, but still
        // the prefix parts.
        assert!(made.serves(DownloadPlan::ExistingInstall, unprefixed));
        assert!(!made.serves(DownloadPlan::ExistingInstall, fresh));
        // Ready, interrupted, or damaged installations ask for nothing more.
        assert_eq!(
            DownloadNeed::for_status(InstallationStatus::Ready, true),
            DownloadNeed::NOTHING
        );
        // A Download recorded at the retired patch step still serves the
        // client step.
        let receipt = fixture.path().join("cache").join(RECEIPT_NAME);
        let recorded = fs::read_to_string(&receipt).unwrap();
        assert!(recorded.contains(r#""reach":"client""#), "{recorded}");
        fs::write(
            &receipt,
            recorded.replace(r#""reach":"client""#, r#""reach":"patch""#),
        )
        .unwrap();
        assert_eq!(readiness(&root), made);
    }
}
