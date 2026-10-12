//! One home for everything the Launcher keeps, and moving an installation
//! from the XDG layout earlier releases used into it.
//!
//! The home (`~/.pd2launcher`) carries a marker once it is set up. Until
//! then, a legacy layout (`$XDG_DATA_HOME/pd2-launcher/managed`, the state
//! and config roots beside it) is moved in behind a journal, holding every
//! lock an earlier Launcher would hold, so it never runs while one is open:
//!
//! - The default installation is renamed to `game/` when it is on the
//!   home's filesystem. Otherwise it stays where it is and the pointer
//!   names it, as for a location the user chose, and a chosen
//!   location's pointer is carried over as it is.
//! - Settings, the previous-root record, and the diagnostics history move
//!   along. Legacy directories are removed only once they are empty.
//!
//! Every step can be repeated, so a crash is recovered by running the
//! migration again from the journal; the marker is written only when it
//! is complete. A home that already exists is preferred and the legacy
//! layout is left untouched.

use std::{ffi::OsStr, fs::File, io, path::Path};

use serde::{Deserialize, Serialize};

use crate::{
    location::{self, PointerState, SelectionLock},
    persistence::{
        GAME_DIRECTORY, HomeMigration, LauncherPaths, LayoutError, LegacyRoots, atomic_write,
        read_bounded_regular,
    },
    relocation,
    safe_fs::{AtomicIntentSnapshot, Directory},
};

/// Written into the home once it is set up.
pub(crate) const HOME_MARKER: &str = ".pd2launcher-home.json";
/// The migration journal, in the home's state root.
pub(crate) const JOURNAL_FILE: &str = "migration.json";
const SCHEMA: u32 = 1;
const RECORD_LIMIT: u64 = 8 * 1024;
const MOVED_FILE_LIMIT: u64 = 64 * 1024;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct HomeMarker {
    schema: u32,
    /// Whether a legacy layout was moved in; a later legacy layout is then
    /// its leftovers, not a second installation.
    migrated: bool,
    /// Whether a legacy layout left beside this home was already reported,
    /// so the advisory is recorded once rather than on every start.
    #[serde(default)]
    legacy_reported: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    legacy: LegacyRoots,
    /// The legacy default root's marker id, recorded before it is renamed.
    root_id: Option<String>,
}

/// Crash-injection seam.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MigrationStep {
    JournalWritten,
    Renamed,
    CarriedOver,
    MarkerWritten,
    Finished,
}

pub(crate) trait MigrationFaults {
    fn after(&self, step: MigrationStep) -> io::Result<()>;

    /// Treat the rename of the legacy root as crossing filesystems.
    fn cross_filesystem(&self) -> bool {
        false
    }
}

pub(crate) struct NoMigrationFaults;

impl MigrationFaults for NoMigrationFaults {
    fn after(&self, _step: MigrationStep) -> io::Result<()> {
        Ok(())
    }
}

/// Set up the home before anything else is created in it, migrating a
/// legacy layout into it the first time. `selection` is this Launcher's
/// shared hold on the home's selection lock.
pub(crate) fn prepare_home(
    paths: &LauncherPaths,
    selection: &SelectionLock,
    faults: &dyn MigrationFaults,
) -> Result<Option<HomeMigration>, LayoutError> {
    let home_path = paths.home().ok_or(LayoutError::Io)?;
    let home = Directory::open(home_path)?;
    let marker = read_home_marker(&home)?;
    let journal = read_journal(paths)?;
    let legacy_present = paths.legacy().is_some_and(LegacyRoots::present);
    if let Some(marker) = &marker
        && journal.is_none()
    {
        if !legacy_present || marker.migrated || marker.legacy_reported {
            return Ok(None);
        }
        // Report the ignored legacy layout once. While another Launcher
        // holds the home it is reported by a later start instead.
        let Ok(_exclusive) = selection.upgrade() else {
            return Ok(None);
        };
        write_home_marker(&home, false, true)?;
        return Ok(Some(HomeMigration::LegacyIgnored));
    }
    let _exclusive = selection
        .upgrade()
        .map_err(|_| LayoutError::SelectionBusy)?;
    let legacy = match (journal, paths.legacy()) {
        (Some(journal), _) => Some((journal.legacy, journal.root_id)),
        (None, Some(legacy)) if legacy_present => {
            if holds_new_layout(paths, &home)? {
                // Both exist: the home wins and the legacy layout is kept
                // as it is, for the user to inspect.
                write_home_marker(&home, false, true)?;
                return Ok(Some(HomeMigration::LegacyIgnored));
            }
            Some((legacy.clone(), None))
        }
        _ => None,
    };
    let Some((legacy, root_id)) = legacy else {
        write_home_marker(&home, false, false)?;
        return Ok(None);
    };
    let _locks = LegacyLocks::acquire(&legacy)?;
    let outcome = migrate(paths, &home, legacy, root_id, faults)?;
    Ok(Some(outcome))
}

fn migrate(
    paths: &LauncherPaths,
    home: &Directory,
    legacy: LegacyRoots,
    mut root_id: Option<String>,
    faults: &dyn MigrationFaults,
) -> Result<HomeMigration, LayoutError> {
    let step = |step| faults.after(step).map_err(|_| LayoutError::Io);
    // A move an earlier Launcher left interrupted is completed or undone
    // first, exactly as that Launcher would have.
    relocation::recover(&LauncherPaths::legacy_view(&legacy))?;
    let mut journal = Journal {
        schema: SCHEMA,
        legacy,
        root_id: root_id.clone(),
    };
    write_journal(paths, &journal)?;
    step(MigrationStep::JournalWritten)?;
    let legacy = &journal.legacy;

    let mut kept_in_place = false;
    match location::read_pointer(paths.config_root()) {
        PointerState::Present(_) => kept_in_place = true,
        PointerState::Invalid => return Err(LayoutError::LocationUnavailable(None)),
        PointerState::Absent => match location::read_pointer(&legacy.config) {
            PointerState::Present(pointer) => {
                location::write_pointer(paths.config_root(), &pointer)?;
                kept_in_place = true;
            }
            PointerState::Invalid => return Err(LayoutError::LocationUnavailable(None)),
            PointerState::Absent => {
                if let Some(legacy_root) = open_legacy_root(legacy)? {
                    let id = match root_id.take() {
                        Some(id) => id,
                        None => match location::read_marker(&legacy_root)? {
                            Some(id) => id,
                            None => {
                                let id = location::new_root_id()?;
                                location::write_marker(&legacy_root, &id)?;
                                id
                            }
                        },
                    };
                    journal.root_id = Some(id.clone());
                    write_journal(paths, &journal)?;
                    kept_in_place =
                        !move_legacy_root(paths, home, legacy, legacy_root, &id, faults)?;
                } else if let Some(id) = &root_id
                    && !holds_root(home, id)
                {
                    // The journal names a root that is at neither place.
                    return Err(LayoutError::LocationUnavailable(Some(
                        paths.default_managed_root(),
                    )));
                }
            }
        },
    }
    step(MigrationStep::Renamed)?;
    let legacy = &journal.legacy;

    let config = Directory::open(paths.config_root())?;
    let state = Directory::open(paths.state_root())?;
    move_file(&legacy.config, "settings.json", &config)?;
    move_file(&legacy.state, relocation::PREVIOUS_FILE, &state)?;
    let logs = home.ensure_directory(Path::new("logs"))?;
    for name in ["diagnostics", "exports"] {
        move_history(&legacy.state, name, &logs);
    }
    step(MigrationStep::CarriedOver)?;

    write_home_marker(home, true, false)?;
    step(MigrationStep::MarkerWritten)?;
    // Only what is now empty is removed; anything else stays for the user.
    remove_file_if_present(&legacy.config, location::POINTER_FILE);
    remove_file_if_present(&legacy.state, location::SELECTION_LOCK);
    for root in [&legacy.config, &legacy.state, &legacy.data] {
        remove_if_empty(root);
    }
    clear_journal(paths)?;
    step(MigrationStep::Finished)?;
    Ok(if kept_in_place {
        HomeMigration::InstallationKeptInPlace
    } else {
        HomeMigration::Migrated
    })
}

/// Rename the legacy default root to `game/`, or, when it is on another
/// filesystem, point at it where it is. Returns whether it was moved.
fn move_legacy_root(
    paths: &LauncherPaths,
    home: &Directory,
    legacy: &LegacyRoots,
    root: Directory,
    root_id: &str,
    faults: &dyn MigrationFaults,
) -> Result<bool, LayoutError> {
    let same_filesystem = !faults.cross_filesystem()
        && root.device_id()? == home.device_id()?
        && root.filesystem_id()? == home.filesystem_id()?;
    let renamed = same_filesystem
        && match Directory::open(&legacy.data)?.rename_directory_noreplace(
            OsStr::new("managed"),
            &root,
            home,
            OsStr::new(GAME_DIRECTORY),
        ) {
            Ok(()) => true,
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::XDEV.raw_os_error()) => {
                false
            }
            Err(error) => return Err(error.into()),
        };
    if !renamed {
        location::write_pointer(
            paths.config_root(),
            &location::Pointer::new(legacy.managed_root(), root_id.to_owned()),
        )?;
    }
    Ok(renamed)
}

/// The legacy default root, when it exists.
fn open_legacy_root(legacy: &LegacyRoots) -> io::Result<Option<Directory>> {
    match Directory::open(&legacy.managed_root()) {
        Ok(root) => Ok(Some(root)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn holds_root(home: &Directory, root_id: &str) -> bool {
    home.open_directory(Path::new(GAME_DIRECTORY))
        .ok()
        .and_then(|root| location::read_marker(&root).ok().flatten())
        .is_some_and(|id| id == root_id)
}

/// Whether the home already holds an installation, settings, or a pointer
/// of its own.
fn holds_new_layout(paths: &LauncherPaths, home: &Directory) -> io::Result<bool> {
    let config = Directory::open(paths.config_root())?;
    Ok(home
        .names()?
        .iter()
        .any(|name| name == OsStr::new(GAME_DIRECTORY) || name == OsStr::new("logs"))
        || config.names()?.iter().any(|name| {
            name == OsStr::new("settings.json") || name == OsStr::new(location::POINTER_FILE)
        }))
}

/// Move one small regular file into `target` unless it is already there.
/// A file that cannot be read safely fails the migration rather than being
/// silently left behind (settings fail closed).
fn move_file(source_root: &Path, name: &str, target: &Directory) -> io::Result<()> {
    let source = source_root.join(name);
    let bytes = match read_bounded_regular(&source, MOVED_FILE_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    match target.open_file(Path::new(name)) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            target.atomic_write(OsStr::new(name), &bytes)?;
        }
        Err(error) => return Err(error),
    }
    remove_file_if_present(source_root, name);
    Ok(())
}

/// Rename a legacy history directory into `logs/`. History is not needed
/// to use the installation, so one that cannot be moved (another
/// filesystem, or already present) is left where it is.
fn move_history(state: &Path, name: &str, logs: &Directory) {
    if let Ok(parent) = Directory::open(state)
        && let Ok(directory) = parent.open_directory(Path::new(name))
    {
        let _ =
            parent.rename_directory_noreplace(OsStr::new(name), &directory, logs, OsStr::new(name));
    }
}

fn remove_file_if_present(root: &Path, name: &str) {
    if let Ok(parent) = Directory::open(root)
        && let Ok(file) = parent.open_file(Path::new(name))
    {
        let _ = parent.remove_open_file_bounded(OsStr::new(name), &file);
    }
}

/// Remove a legacy directory that holds nothing but an idle atomic-write
/// namespace.
fn remove_if_empty(path: &Path) {
    if let (Some(parent), Some(name)) = (path.parent(), path.file_name())
        && let Ok(parent) = Directory::open(parent)
        && parent
            .open_directory(Path::new(name))
            .is_ok_and(|directory| {
                directory.names().is_ok_and(|names| names.is_empty())
                    && matches!(
                        directory.pending_atomic_intent(RECORD_LIMIT),
                        Ok(AtomicIntentSnapshot::Clear)
                    )
            })
    {
        let _ = parent.remove_owned_tree(name);
    }
}

fn read_home_marker(home: &Directory) -> io::Result<Option<HomeMarker>> {
    let file = match home.open_file(Path::new(HOME_MARKER)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    io::Read::read_to_end(&mut io::Read::take(file, RECORD_LIMIT + 1), &mut bytes)?;
    serde_json::from_slice::<HomeMarker>(&bytes)
        .ok()
        .filter(|marker| bytes.len() as u64 <= RECORD_LIMIT && marker.schema == SCHEMA)
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "home marker was invalid"))
}

fn write_home_marker(home: &Directory, migrated: bool, legacy_reported: bool) -> io::Result<()> {
    let bytes = serde_json::to_vec(&HomeMarker {
        schema: SCHEMA,
        migrated,
        legacy_reported,
    })
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "home marker encode failed"))?;
    home.atomic_write(OsStr::new(HOME_MARKER), &bytes)
}

fn read_journal(paths: &LauncherPaths) -> io::Result<Option<Journal>> {
    let bytes = match read_bounded_regular(&paths.state_root().join(JOURNAL_FILE), RECORD_LIMIT) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    serde_json::from_slice::<Journal>(&bytes)
        .ok()
        .filter(|journal| {
            journal.schema == SCHEMA
                && [
                    &journal.legacy.data,
                    &journal.legacy.state,
                    &journal.legacy.config,
                ]
                .into_iter()
                .all(|path| path.is_absolute())
        })
        .map(Some)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "migration journal was invalid"))
}

fn write_journal(paths: &LauncherPaths, journal: &Journal) -> io::Result<()> {
    let bytes = serde_json::to_vec(journal)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "journal encode failed"))?;
    atomic_write(&paths.state_root().join(JOURNAL_FILE), &bytes)
}

fn clear_journal(paths: &LauncherPaths) -> io::Result<()> {
    let state = Directory::open(paths.state_root())?;
    match state.open_file(Path::new(JOURNAL_FILE)) {
        Ok(file) => state.remove_open_file_bounded(OsStr::new(JOURNAL_FILE), &file),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Every lock an earlier Launcher holds while it uses its layout, held
/// exclusively for the migration. Only existing lock files are opened;
/// nothing is created in the legacy layout.
struct LegacyLocks(Vec<File>);

impl LegacyLocks {
    fn acquire(legacy: &LegacyRoots) -> Result<Self, LayoutError> {
        let diagnostics = legacy.state.join("diagnostics");
        let managed = legacy.managed_root();
        let candidates: [(&Path, &str); 5] = [
            (&legacy.state, location::SELECTION_LOCK),
            (&diagnostics, "recent.lock"),
            (&diagnostics, "active-job-owner.lock"),
            (&diagnostics, "export-prune.lock"),
            (&managed, ".pd2launcher.lock"),
        ];
        let mut held = Vec::new();
        for (root, name) in candidates {
            let file = match Directory::open(root).and_then(|root| root.open_file(Path::new(name)))
            {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            File::try_lock(&file).map_err(|_| LayoutError::SelectionBusy)?;
            held.push(file);
        }
        Ok(Self(held))
    }
}

impl Drop for LegacyLocks {
    fn drop(&mut self) {
        for file in &self.0 {
            let _ = File::unlock(file);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::PathBuf};

    use super::*;
    use crate::{
        diagnostics::{DiagnosticCode, DiagnosticStore},
        persistence::{LauncherSettings, RendererSetting, SettingsStore},
    };

    const SENTINEL: &str = "prefix/drive_c/installed.txt";

    struct Fixture {
        _directory: tempfile::TempDir,
        user_home: PathBuf,
        legacy: LegacyRoots,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let user_home = directory.path().join("home");
            fs::create_dir(&user_home).unwrap();
            let legacy = LegacyRoots {
                data: user_home.join(".local/share/pd2-launcher"),
                state: user_home.join(".local/state/pd2-launcher"),
                config: user_home.join(".config/pd2-launcher"),
            };
            Self {
                _directory: directory,
                user_home,
                legacy,
            }
        }

        fn home(&self) -> PathBuf {
            self.user_home.join(".pd2launcher")
        }

        fn paths(&self) -> LauncherPaths {
            LauncherPaths::from_home(self.home()).with_legacy(self.legacy.clone())
        }

        /// What an earlier release leaves behind after an installation.
        fn legacy_install(&self) -> String {
            let managed = self.legacy.managed_root();
            fs::create_dir_all(managed.join("prefix/drive_c")).unwrap();
            fs::write(managed.join(SENTINEL), b"installed").unwrap();
            fs::write(managed.join(".pd2launcher.lock"), b"").unwrap();
            let id = location::new_root_id().unwrap();
            location::write_marker(&Directory::open(&managed).unwrap(), &id).unwrap();
            SettingsStore::new(self.legacy.config.join("settings.json"))
                .save(&LauncherSettings::default().with_renderer(RendererSetting::DirectDraw))
                .unwrap();
            fs::create_dir_all(self.legacy.state.join("exports")).unwrap();
            fs::write(self.legacy.state.join(location::SELECTION_LOCK), b"").unwrap();
            let mut store = DiagnosticStore::open(
                self.legacy.state.join("diagnostics/recent.json"),
                self.legacy.state.join("exports"),
            )
            .unwrap();
            store.record_event(DiagnosticCode::AuditCompleted).unwrap();
            id
        }

        fn assert_migrated_home(&self, paths: &LauncherPaths, root_id: &str) {
            let game = self.home().join(GAME_DIRECTORY);
            assert_eq!(paths.managed_root(), game);
            assert!(paths.uses_default_root());
            assert_eq!(fs::read(game.join(SENTINEL)).unwrap(), b"installed");
            assert_eq!(
                location::read_marker(&Directory::open(&game).unwrap()).unwrap(),
                Some(root_id.to_owned())
            );
            assert!(!paths.config_root().join(location::POINTER_FILE).exists());
            assert!(!paths.state_root().join(JOURNAL_FILE).exists());
            assert_eq!(
                SettingsStore::new(paths.settings_file())
                    .load()
                    .unwrap()
                    .renderer(),
                RendererSetting::DirectDraw
            );
            let store = DiagnosticStore::open(
                paths.diagnostics_root().join("recent.json"),
                paths.exports_root(),
            )
            .unwrap();
            assert!(format!("{store:?}").contains("AuditCompleted"));
            for root in [&self.legacy.data, &self.legacy.state, &self.legacy.config] {
                assert!(!root.exists(), "{} was left", root.display());
            }
        }
    }

    struct FailAfter(MigrationStep);

    impl MigrationFaults for FailAfter {
        fn after(&self, step: MigrationStep) -> io::Result<()> {
            if step == self.0 {
                Err(io::Error::other("injected crash"))
            } else {
                Ok(())
            }
        }
    }

    struct CrossFilesystem;

    impl MigrationFaults for CrossFilesystem {
        fn after(&self, _step: MigrationStep) -> io::Result<()> {
            Ok(())
        }

        fn cross_filesystem(&self) -> bool {
            true
        }
    }

    #[test]
    fn a_fresh_home_is_created_with_game_config_and_logs() {
        let fixture = Fixture::new();
        let paths = fixture.paths();
        drop(paths.open_managed_layout().unwrap());
        let home = fixture.home();
        for directory in ["game", "config", "logs/diagnostics", "logs/exports"] {
            assert!(home.join(directory).is_dir(), "{directory}");
        }
        assert!(home.join(HOME_MARKER).is_file());
        assert_eq!(paths.managed_root(), home.join("game"));
        assert_eq!(paths.take_migration(), None);
        assert!(
            !fixture.legacy.present(),
            "nothing is created in XDG folders"
        );
    }

    #[test]
    fn a_legacy_layout_on_the_same_filesystem_moves_in_by_rename() {
        let fixture = Fixture::new();
        let root_id = fixture.legacy_install();
        let paths = fixture.paths();
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(paths.take_migration(), Some(HomeMigration::Migrated));
        fixture.assert_migrated_home(&paths, &root_id);

        let reopened = fixture.paths();
        drop(reopened.open_managed_layout().unwrap());
        assert_eq!(reopened.take_migration(), None);
        fixture.assert_migrated_home(&reopened, &root_id);
    }

    #[test]
    fn every_crash_cut_of_a_migration_recovers_on_the_next_start() {
        for step in [
            MigrationStep::JournalWritten,
            MigrationStep::Renamed,
            MigrationStep::CarriedOver,
            MigrationStep::MarkerWritten,
            MigrationStep::Finished,
        ] {
            let fixture = Fixture::new();
            let root_id = fixture.legacy_install();
            let crashed = fixture.paths();
            let result = crashed.open_managed_layout_with(&FailAfter(step));
            assert!(matches!(result, Err(LayoutError::Io)), "{step:?}");
            // Exactly one place holds the installation after the cut.
            let at_home = fixture.home().join(GAME_DIRECTORY).join(SENTINEL).exists();
            let at_legacy = fixture.legacy.managed_root().join(SENTINEL).exists();
            assert!(at_home != at_legacy, "{step:?}");

            let recovered = fixture.paths();
            drop(recovered.open_managed_layout().expect("recovers"));
            fixture.assert_migrated_home(&recovered, &root_id);
            // A cut after the journal was cleared lost only the report.
            let reported = (step != MigrationStep::Finished).then_some(HomeMigration::Migrated);
            assert_eq!(recovered.take_migration(), reported, "{step:?}");
        }
    }

    #[test]
    fn a_legacy_installation_on_another_filesystem_is_pointed_at_in_place() {
        let fixture = Fixture::new();
        let root_id = fixture.legacy_install();
        let paths = fixture.paths();
        drop(paths.open_managed_layout_with(&CrossFilesystem).unwrap());
        assert_eq!(
            paths.take_migration(),
            Some(HomeMigration::InstallationKeptInPlace)
        );
        let legacy_root = fixture.legacy.managed_root();
        assert_eq!(paths.managed_root(), legacy_root);
        assert!(!paths.uses_default_root());
        assert!(legacy_root.join(SENTINEL).is_file());
        assert!(!fixture.home().join(GAME_DIRECTORY).exists());
        assert_eq!(
            location::read_pointer(paths.config_root()),
            PointerState::Present(location::Pointer::new(legacy_root.clone(), root_id))
        );
        assert!(paths.settings_file().is_file());
        assert!(!fixture.legacy.config.exists());

        // Later starts keep using it, and a crash after the pointer was
        // written converges on the same result.
        let reopened = fixture.paths();
        drop(reopened.open_managed_layout().unwrap());
        assert_eq!(reopened.managed_root(), legacy_root);
        assert_eq!(reopened.take_migration(), None);
    }

    #[test]
    fn a_crash_after_pointing_at_another_filesystem_recovers_to_the_pointer() {
        let fixture = Fixture::new();
        fixture.legacy_install();
        struct CrossThenCrash;
        impl MigrationFaults for CrossThenCrash {
            fn after(&self, step: MigrationStep) -> io::Result<()> {
                if step == MigrationStep::Renamed {
                    Err(io::Error::other("injected crash"))
                } else {
                    Ok(())
                }
            }
            fn cross_filesystem(&self) -> bool {
                true
            }
        }
        assert!(
            fixture
                .paths()
                .open_managed_layout_with(&CrossThenCrash)
                .is_err()
        );
        let recovered = fixture.paths();
        drop(recovered.open_managed_layout().unwrap());
        assert_eq!(recovered.managed_root(), fixture.legacy.managed_root());
        assert!(!fixture.home().join(GAME_DIRECTORY).exists());
        assert_eq!(
            recovered.take_migration(),
            Some(HomeMigration::InstallationKeptInPlace)
        );
    }

    #[test]
    fn a_legacy_chosen_location_keeps_its_pointer() {
        let fixture = Fixture::new();
        let chosen = fixture.user_home.join("card/pd2-launcher");
        fs::create_dir_all(chosen.join("prefix/drive_c")).unwrap();
        fs::write(chosen.join(SENTINEL), b"installed").unwrap();
        let id = location::new_root_id().unwrap();
        location::write_marker(&Directory::open(&chosen).unwrap(), &id).unwrap();
        fs::create_dir_all(&fixture.legacy.config).unwrap();
        let pointer = location::Pointer::new(chosen.clone(), id);
        location::write_pointer(&fixture.legacy.config, &pointer).unwrap();

        let paths = fixture.paths();
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(
            paths.take_migration(),
            Some(HomeMigration::InstallationKeptInPlace)
        );
        assert_eq!(paths.managed_root(), chosen);
        assert_eq!(
            location::read_pointer(paths.config_root()),
            PointerState::Present(pointer)
        );
        assert!(chosen.join(SENTINEL).is_file());
        assert!(!fixture.home().join(GAME_DIRECTORY).exists());
        assert!(!fixture.legacy.config.exists());
    }

    #[test]
    fn an_existing_home_is_preferred_and_the_legacy_layout_left_alone() {
        let fixture = Fixture::new();
        drop(
            LauncherPaths::from_home(fixture.home())
                .open_managed_layout()
                .unwrap(),
        );
        fixture.legacy_install();
        let paths = fixture.paths();
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(paths.take_migration(), Some(HomeMigration::LegacyIgnored));
        assert_eq!(paths.managed_root(), fixture.home().join(GAME_DIRECTORY));
        assert!(fixture.legacy.managed_root().join(SENTINEL).is_file());
        assert!(fixture.legacy.config.join("settings.json").is_file());
        assert!(!paths.settings_file().exists());
        // It is reported once, not on every start.
        let again = fixture.paths();
        drop(again.open_managed_layout().unwrap());
        assert_eq!(again.take_migration(), None);
        assert!(fixture.legacy.managed_root().join(SENTINEL).is_file());

        // A home without a marker that already holds an installation is
        // preferred the same way.
        let other = Fixture::new();
        fs::create_dir_all(other.home().join("game")).unwrap();
        other.legacy_install();
        let paths = other.paths();
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(paths.take_migration(), Some(HomeMigration::LegacyIgnored));
        assert!(other.legacy.managed_root().join(SENTINEL).is_file());
        let again = other.paths();
        drop(again.open_managed_layout().unwrap());
        assert_eq!(again.take_migration(), None);

        // A marker written before the field existed reports it once.
        let earlier = Fixture::new();
        drop(
            LauncherPaths::from_home(earlier.home())
                .open_managed_layout()
                .unwrap(),
        );
        fs::write(
            earlier.home().join(HOME_MARKER),
            br#"{"schema":1,"migrated":false}"#,
        )
        .unwrap();
        earlier.legacy_install();
        let paths = earlier.paths();
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(paths.take_migration(), Some(HomeMigration::LegacyIgnored));
        let again = earlier.paths();
        drop(again.open_managed_layout().unwrap());
        assert_eq!(again.take_migration(), None);
    }

    #[test]
    fn a_running_earlier_launcher_blocks_the_migration() {
        let fixture = Fixture::new();
        let root_id = fixture.legacy_install();
        let held = SelectionLock::acquire_shared(&fixture.legacy.state).unwrap();
        let paths = fixture.paths();
        assert!(matches!(
            paths.open_managed_layout(),
            Err(LayoutError::SelectionBusy)
        ));
        assert!(fixture.legacy.managed_root().join(SENTINEL).is_file());
        assert!(!fixture.home().join(HOME_MARKER).exists());
        drop(held);

        let lease =
            fs::File::open(fixture.legacy.managed_root().join(".pd2launcher.lock")).unwrap();
        fs::File::lock_shared(&lease).unwrap();
        assert!(matches!(
            fixture.paths().open_managed_layout(),
            Err(LayoutError::SelectionBusy)
        ));
        drop(lease);

        let paths = fixture.paths();
        drop(paths.open_managed_layout().unwrap());
        fixture.assert_migrated_home(&paths, &root_id);
    }

    #[test]
    fn the_home_override_is_used_as_given_without_migration() {
        let fixture = Fixture::new();
        fixture.legacy_install();
        let custom = fixture.user_home.join("elsewhere/pd2");
        let environment = |override_value: Option<PathBuf>| {
            let home = fixture.user_home.clone();
            move |name: &str| -> Option<OsString> {
                match name {
                    "HOME" => Some(home.clone().into()),
                    HOME_OVERRIDE_NAME => override_value.clone().map(Into::into),
                    _ => None,
                }
            }
        };
        let paths = LauncherPaths::discover_from(environment(Some(custom.clone()))).unwrap();
        assert!(paths.legacy().is_none());
        drop(paths.open_managed_layout().unwrap());
        assert_eq!(paths.managed_root(), custom.join("game"));
        assert_eq!(paths.settings_file(), custom.join("config/settings.json"));
        assert_eq!(paths.take_migration(), None);
        assert!(fixture.legacy.managed_root().join(SENTINEL).is_file());

        for invalid in ["relative/pd2", "/tmp/../pd2", "/tmp/./pd2"] {
            assert!(
                LauncherPaths::discover_from(environment(Some(PathBuf::from(invalid)))).is_err(),
                "{invalid}"
            );
        }

        let default = LauncherPaths::discover_from(environment(None)).unwrap();
        assert_eq!(default.home(), Some(fixture.home().as_path()));
        assert_eq!(default.legacy(), Some(&fixture.legacy));
    }

    const HOME_OVERRIDE_NAME: &str = crate::persistence::HOME_OVERRIDE;
}
