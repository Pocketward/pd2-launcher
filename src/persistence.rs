//! Launcher-owned paths and secret-free settings persistence.

use std::{
    collections::BTreeMap,
    env,
    ffi::OsString,
    fs::{self, File},
    io::{self, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};

use std::sync::{Arc, RwLock};

use crate::{
    location::{self, PointerState, SelectionLock, read_pointer},
    safe_fs::Directory,
    sources::{DownloadArtifact, normalize_custom_url},
};

const SETTINGS_LIMIT: u64 = 64 * 1024;
/// Managed-root-relative `XDG_CACHE_HOME` for every Wine process, so Wine
/// never reads or writes the user's `~/.cache/wine` (where a cached Mono or
/// Gecko MSI would change what Wine does) and GPU shader caches stay with
/// the installation.
pub(crate) const WINE_CACHE_DIRECTORY: &str = "cache/xdg";
const SETTINGS_SCHEMA: u32 = 1;

/// The Launcher's home below `$HOME`.
pub(crate) const HOME_DIRECTORY: &str = ".pd2launcher";
/// Overrides the whole home with an absolute path.
pub(crate) const HOME_OVERRIDE: &str = "PD2_LAUNCHER_HOME";
/// The default managed root, inside the home.
pub(crate) const GAME_DIRECTORY: &str = "game";

#[derive(Clone, Debug)]
pub(crate) struct LauncherPaths {
    /// `~/.pd2launcher`, or the path [`HOME_OVERRIDE`] names.
    home: Option<PathBuf>,
    config_root: PathBuf,
    /// Location records: the root-selection lock, the relocation intent,
    /// and the previous-root record.
    state_root: PathBuf,
    diagnostics_root: PathBuf,
    exports_root: PathBuf,
    default_managed_root: PathBuf,
    /// The resolved managed root when it is not the default. Shared by
    /// every clone so a chosen or moved location reaches every component.
    managed: Arc<RwLock<Option<PathBuf>>>,
}

/// Why the managed layout could not be opened.
#[derive(Debug)]
pub(crate) enum LayoutError {
    Io,
    /// The pointer names a root that is not provably this Launcher's.
    /// `None` when the pointer itself is unreadable.
    LocationUnavailable(Option<PathBuf>),
    /// Another Launcher is choosing or moving the location.
    SelectionBusy,
}

impl From<io::Error> for LayoutError {
    /// The error and the step that failed go to standard error: the window
    /// shows only that the folder could not be opened, and on systems such
    /// as DroidDeck the session log that captures standard error is the
    /// only place the cause can be read.
    #[track_caller]
    fn from(error: io::Error) -> Self {
        let at = std::panic::Location::caller();
        eprintln!(
            "pd2-launcher: the launcher folder could not be opened ({}:{}): {error}",
            at.file(),
            at.line()
        );
        Self::Io
    }
}

impl LauncherPaths {
    pub(crate) fn discover() -> io::Result<Self> {
        Self::discover_from(|name| env::var_os(name))
    }

    /// [`Self::discover`] over an explicit environment.
    pub(crate) fn discover_from(
        environment: impl Fn(&str) -> Option<OsString>,
    ) -> io::Result<Self> {
        let home = environment("HOME")
            .map(PathBuf::from)
            .filter(|path| valid_root_path(path))
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory unavailable"))?;
        if let Some(root) = environment(HOME_OVERRIDE) {
            let root = PathBuf::from(root);
            if !valid_root_path(&root) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "PD2_LAUNCHER_HOME was not a normalized absolute path",
                ));
            }
            // An explicit home is used exactly as given.
            return Ok(Self::from_home(root));
        }
        Ok(Self::from_home(home.join(HOME_DIRECTORY)))
    }

    /// Everything below one home: `game/`, `config/`, and `logs/`.
    pub(crate) fn from_home(home: impl Into<PathBuf>) -> Self {
        let home = home.into();
        Self {
            config_root: home.join("config"),
            state_root: home.join("config"),
            diagnostics_root: home.join("logs/diagnostics"),
            exports_root: home.join("logs/exports"),
            default_managed_root: home.join(GAME_DIRECTORY),
            home: Some(home),
            managed: Arc::default(),
        }
    }

    pub(crate) fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// Creates only Launcher-owned user directories and the shared installation
    /// lock, after resolving where the managed root is. It never
    /// creates or mutates a host facility, and it never creates a chosen
    /// root: a chosen root that is missing or not proven by its marker is
    /// unavailable. The returned root-selection lock is held shared for as
    /// long as this Launcher keeps the root open.
    pub(crate) fn open_managed_layout(&self) -> Result<SelectionLock, LayoutError> {
        if let Some(home) = &self.home {
            ensure_app_directory(home)?;
        }
        for root in [&self.state_root, &self.config_root] {
            ensure_app_directory(root)?;
        }
        let selection = SelectionLock::acquire_shared(&self.state_root)
            .map_err(|_| LayoutError::SelectionBusy)?;
        for directory in [self.diagnostics_root(), self.exports_root()] {
            ensure_owned_directory(&directory)?;
        }
        // A move a crash interrupted is completed or undone before the
        // pointer is read; no move can be in progress under a shared hold.
        crate::relocation::recover(self)?;
        let managed = match read_pointer(&self.config_root) {
            PointerState::Absent => {
                self.set_managed_root(None);
                ensure_owned_directory(&self.default_managed_root())?;
                let managed = Directory::open(&self.default_managed_root())?;
                // The default root is Launcher-owned; it receives its marker
                // the first time a Launcher opens it under the lease.
                open_root_layout(&managed)?;
                if location::read_marker(&managed)?.is_none() {
                    let lock = managed.open_file(Path::new(".pd2launcher.lock"))?;
                    if File::try_lock(&lock).is_ok() {
                        let written = location::write_marker(&managed, &location::new_root_id()?);
                        let _ = File::unlock(&lock);
                        written?;
                    }
                }
                return Ok(selection);
            }
            PointerState::Invalid => return Err(LayoutError::LocationUnavailable(None)),
            PointerState::Present(pointer) => match location::open_chosen_root(&pointer) {
                Ok(managed) => {
                    self.set_managed_root(Some(pointer.path().to_owned()));
                    managed
                }
                Err(_) => {
                    return Err(LayoutError::LocationUnavailable(Some(
                        pointer.path().to_owned(),
                    )));
                }
            },
        };
        open_root_layout(&managed)?;
        Ok(selection)
    }

    /// The managed root: the chosen one when a pointer resolved, else the
    /// default.
    pub(crate) fn managed_root(&self) -> PathBuf {
        self.managed
            .read()
            .ok()
            .and_then(|managed| managed.clone())
            .unwrap_or_else(|| self.default_managed_root())
    }

    pub(crate) fn default_managed_root(&self) -> PathBuf {
        self.default_managed_root.clone()
    }

    /// Point every clone at `root` (`None` for the default).
    pub(crate) fn set_managed_root(&self, root: Option<PathBuf>) {
        if let Ok(mut managed) = self.managed.write() {
            *managed = root;
        }
    }

    pub(crate) fn uses_default_root(&self) -> bool {
        self.managed.read().is_ok_and(|managed| managed.is_none())
    }

    pub(crate) fn config_root(&self) -> &Path {
        &self.config_root
    }

    pub(crate) fn state_root(&self) -> &Path {
        &self.state_root
    }

    pub(crate) fn project_d2_root(&self) -> PathBuf {
        self.managed_root()
            .join("prefix/drive_c/Program Files (x86)/Diablo II/ProjectD2")
    }

    #[cfg(test)]
    pub(crate) fn installation_lock(&self) -> PathBuf {
        self.managed_root().join(".pd2launcher.lock")
    }

    pub(crate) fn settings_file(&self) -> PathBuf {
        self.config_root.join("settings.json")
    }

    /// Where the player's characters are kept, outside every installation.
    pub(crate) fn kept_saves(&self) -> Option<crate::save_keeper::KeptSaves> {
        self.home
            .as_deref()
            .map(crate::save_keeper::KeptSaves::below)
    }

    /// Where the setup stands ([`SetupRecord`]), beside the settings.
    pub(crate) fn setup_record_file(&self) -> PathBuf {
        self.config_root.join("setup.json")
    }

    pub(crate) fn diagnostics_root(&self) -> PathBuf {
        self.diagnostics_root.clone()
    }

    /// Saved reports (`logs/reports`), beside the diagnostics.
    pub(crate) fn reports_root(&self) -> PathBuf {
        self.diagnostics_root.parent().map_or_else(
            || self.diagnostics_root.join("reports"),
            |logs| logs.join("reports"),
        )
    }

    pub(crate) fn exports_root(&self) -> PathBuf {
        self.exports_root.clone()
    }

    pub(crate) fn cleanup_lock(&self) -> PathBuf {
        self.diagnostics_root().join("export-prune.lock")
    }
}

/// How PD2's DirectDraw wrapper (cnc-ddraw) draws in DirectDraw mode.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum DirectDrawRendererSetting {
    /// Never chosen: `ddraw.ini` is left as the player keeps it.
    #[default]
    Untouched,
    /// PD2's own setting, restored: OpenGL, as PD2 ships it.
    Pd2,
    OpenGl,
    Direct3d9,
    Software,
}

impl DirectDrawRendererSetting {
    /// cnc-ddraw's `renderer=` value, or `None` to leave the file alone.
    pub(crate) const fn cnc_ddraw_value(self) -> Option<&'static str> {
        match self {
            Self::Untouched => None,
            Self::Pd2 | Self::OpenGl => Some("opengl"),
            Self::Direct3d9 => Some("direct3d9"),
            Self::Software => Some("gdi"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum RendererSetting {
    #[default]
    Glide,
    DirectDraw,
}

/// A settings value this build does not use: read whatever it holds, keep nothing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Parked;

impl<'de> Deserialize<'de> for Parked {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        serde::de::IgnoredAny::deserialize(deserializer).map(|_| Self)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LauncherSettings {
    schema: u32,
    renderer: RendererSetting,
    skip_to_bnet: bool,
    sound_in_background: bool,
    network_concurrency: u8,
    /// User-supplied download sources from the Diagnostics page.
    /// Absent from files written before the feature and omitted when empty,
    /// so the settings schema is unchanged. Read leniently: an unknown or
    /// invalid entry is dropped and counted, never failing the settings.
    #[serde(default, skip_serializing_if = "SavedSources::is_empty")]
    download_sources: SavedSources,
    /// Draw the official PD2 launcher's WPF window in software. On unless
    /// turned off, and written only when off: the settings schema is
    /// unchanged and older Launchers read the file while it is on (they
    /// reject it once the setting is saved off).
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    official_launcher_software_rendering: bool,
    /// Keep the Launcher in the desktop's app menu (`app_menu`). On unless
    /// turned off, and written only when off.
    #[serde(default = "enabled", skip_serializing_if = "is_enabled")]
    app_menu_entry: bool,
    /// Settings this build does not use (a Wine on this computer, the host
    /// environment), still accepted in files that carry them, and dropped
    /// when the settings are next saved.
    #[serde(default, skip_serializing, rename = "existing_wine")]
    parked_existing_wine: Option<Parked>,
    #[serde(default, skip_serializing, rename = "environment")]
    parked_environment: Option<Parked>,
    /// Which FEX runs the launcher's x86 Wine on ARM64 builds; absent until
    /// the player chose one (the device's is used until then).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fex_source: Option<FexSource>,
    /// The device FEX program the player set for ARM64 builds, used before
    /// any FEX the Launcher finds by itself; absent unless set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fex: Option<PathBuf>,
    /// How DirectDraw mode draws; absent until the player chooses.
    #[serde(default, skip_serializing_if = "is_untouched_direct_draw")]
    direct_draw_renderer: DirectDrawRendererSetting,
}

/// Where the launcher's x86 Wine gets FEX on ARM64 builds.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FexSource {
    /// A FEX on this device: the one set in Settings, or one found by itself.
    #[default]
    Device,
    /// The FEX and RootFS the Launcher downloads and keeps in its own folder.
    Builtin,
}

fn is_untouched_direct_draw(value: &DirectDrawRendererSetting) -> bool {
    *value == DirectDrawRendererSetting::Untouched
}

const fn enabled() -> bool {
    true
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes a reference"
)]
const fn is_enabled(value: &bool) -> bool {
    *value
}

/// The key of the retired 1.14d patch source: dropped on load
/// without being counted, since it was once valid and nothing is left for
/// the user to fix.
const RETIRED_SOURCE_KEYS: [&str; 1] = ["patch"];

/// Saved download sources: only known artifacts with valid, normalized
/// custom URLs, plus how many saved entries were ignored on load.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct SavedSources {
    sources: BTreeMap<DownloadArtifact, String>,
    ignored: usize,
}

impl SavedSources {
    fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

impl Serialize for SavedSources {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.sources.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SavedSources {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = serde_json::Value::deserialize(deserializer)?;
        let mut saved = Self::default();
        let serde_json::Value::Object(entries) = value else {
            saved.ignored = 1;
            return Ok(saved);
        };
        for (key, value) in entries {
            if RETIRED_SOURCE_KEYS.contains(&key.as_str()) {
                continue;
            }
            let accepted =
                serde_json::from_value::<DownloadArtifact>(serde_json::Value::String(key))
                    .ok()
                    .zip(value.as_str().map(str::to_owned))
                    .and_then(|(artifact, url)| {
                        normalize_custom_url(artifact, &url)
                            .ok()
                            .map(|url| (artifact, url))
                    });
            match accepted {
                Some((artifact, url)) => {
                    saved.sources.insert(artifact, url);
                }
                None => saved.ignored += 1,
            }
        }
        Ok(saved)
    }
}

impl Default for LauncherSettings {
    fn default() -> Self {
        Self {
            schema: SETTINGS_SCHEMA,
            renderer: RendererSetting::Glide,
            skip_to_bnet: true,
            sound_in_background: false,
            network_concurrency: 2,
            download_sources: SavedSources::default(),
            official_launcher_software_rendering: true,
            app_menu_entry: true,
            parked_existing_wine: None,
            parked_environment: None,
            fex_source: None,
            fex: None,
            direct_draw_renderer: DirectDrawRendererSetting::Untouched,
        }
    }
}

impl LauncherSettings {
    pub(crate) fn renderer(&self) -> RendererSetting {
        self.renderer
    }

    pub(crate) fn skip_to_bnet(&self) -> bool {
        self.skip_to_bnet
    }

    pub(crate) fn sound_in_background(&self) -> bool {
        self.sound_in_background
    }

    pub(crate) fn official_launcher_software_rendering(&self) -> bool {
        self.official_launcher_software_rendering
    }

    /// The FEX in use: the device's until the player chose.
    pub(crate) fn fex_source(&self) -> FexSource {
        self.fex_source.unwrap_or_default()
    }

    /// Whether the player chose a FEX (step 1 or Settings).
    pub(crate) fn fex_source_chosen(&self) -> bool {
        self.fex_source.is_some()
    }

    pub(crate) fn with_fex_source(mut self, source: FexSource) -> Self {
        self.fex_source = Some(source);
        self
    }

    /// The device FEX set in Settings, or `None` to find it automatically.
    pub(crate) fn fex(&self) -> Option<&Path> {
        self.fex.as_deref()
    }

    pub(crate) fn with_fex(mut self, fex: Option<PathBuf>) -> Self {
        self.fex = fex.filter(|fex| fex.is_absolute());
        self
    }

    pub(crate) fn app_menu_entry(&self) -> bool {
        self.app_menu_entry
    }

    pub(crate) fn with_app_menu_entry(mut self, enabled: bool) -> Self {
        self.app_menu_entry = enabled;
        self
    }

    pub(crate) fn with_official_launcher_software_rendering(mut self, enabled: bool) -> Self {
        self.official_launcher_software_rendering = enabled;
        self
    }

    pub(crate) fn direct_draw_renderer(&self) -> DirectDrawRendererSetting {
        self.direct_draw_renderer
    }

    pub(crate) fn with_direct_draw_renderer(mut self, renderer: DirectDrawRendererSetting) -> Self {
        self.direct_draw_renderer = renderer;
        self
    }

    pub(crate) fn with_renderer(mut self, renderer: RendererSetting) -> Self {
        self.renderer = renderer;
        self
    }

    pub(crate) fn with_skip_to_bnet(mut self, enabled: bool) -> Self {
        self.skip_to_bnet = enabled;
        self
    }

    pub(crate) fn with_sound_in_background(mut self, enabled: bool) -> Self {
        self.sound_in_background = enabled;
        self
    }

    pub(crate) fn download_sources(&self) -> &BTreeMap<DownloadArtifact, String> {
        &self.download_sources.sources
    }

    /// Saved download-source entries dropped on load because the artifact
    /// was unknown or the URL invalid. They are not written back on save.
    pub(crate) fn ignored_download_sources(&self) -> usize {
        self.download_sources.ignored
    }

    /// Set (`Some`) or remove (`None`) the GUI override for one artifact. The
    /// value must already be a normalized custom URL.
    pub(crate) fn with_download_source(
        mut self,
        artifact: DownloadArtifact,
        url: Option<String>,
    ) -> Self {
        match url {
            Some(url) => {
                self.download_sources.sources.insert(artifact, url);
            }
            None => {
                self.download_sources.sources.remove(&artifact);
            }
        }
        self
    }

    fn validate(&self) -> io::Result<()> {
        if self.schema != SETTINGS_SCHEMA || !(1..=8).contains(&self.network_concurrency) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settings schema or bounds were invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    pub(crate) fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub(crate) fn load(&self) -> io::Result<LauncherSettings> {
        let bytes = match read_bounded_regular(&self.path, SETTINGS_LIMIT) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LauncherSettings::default());
            }
            Err(error) => return Err(error),
        };
        let settings: LauncherSettings = serde_json::from_slice(&bytes)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "settings JSON was invalid"))?;
        settings.validate()?;
        Ok(settings)
    }

    pub(crate) fn save(&self, settings: &LauncherSettings) -> io::Result<()> {
        settings.validate()?;
        let parent = self.path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "settings parent unavailable")
        })?;
        ensure_app_directory(parent)?;
        reject_unsafe_existing_file(&self.path)?;
        let bytes = serde_json::to_vec(settings)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "settings encode failed"))?;
        atomic_write(&self.path, &bytes)
    }
}

/// How far the setup got, by the Launcher's own record: the page a start
/// opens on while its first check runs (that check's placeholder snapshot
/// says nothing yet). The check still decides once it ends.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum SetupRecord {
    /// Nothing was set up, or Start over reset it. An absent or unreadable
    /// record is the same: the check decides.
    #[default]
    None,
    /// An install step began, and Play was not reachable yet.
    Started,
    /// Play was reachable; from then until Start over.
    Installed,
}

const SETUP_RECORD_SCHEMA: u32 = 1;
const SETUP_RECORD_LIMIT: u64 = 4096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct SetupRecordFile {
    schema: u32,
    setup: SetupRecord,
}

/// The setup record at `path`; [`SetupRecord::None`] when there is none or
/// it is not one the Launcher wrote.
pub(crate) fn load_setup_record(path: &Path) -> SetupRecord {
    read_bounded_regular(path, SETUP_RECORD_LIMIT)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SetupRecordFile>(&bytes).ok())
        .filter(|file| file.schema == SETUP_RECORD_SCHEMA)
        .map_or(SetupRecord::None, |file| file.setup)
}

/// Replace the setup record at `path` atomically.
pub(crate) fn save_setup_record(path: &Path, record: SetupRecord) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "setup record parent unavailable",
        )
    })?;
    ensure_app_directory(parent)?;
    reject_unsafe_existing_file(path)?;
    let bytes = serde_json::to_vec(&SetupRecordFile {
        schema: SETUP_RECORD_SCHEMA,
        setup: record,
    })
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "setup record encode failed"))?;
    atomic_write(path, &bytes)
}

fn reject_unsafe_existing_file(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Err(
            io::Error::new(io::ErrorKind::InvalidData, "settings file was unsafe"),
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file parent unavailable"))?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "atomic parent directory was unsafe",
        ));
    }
    if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "atomic destination was a symlink",
        ));
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file name unavailable"))?;
    Directory::open(parent)?.atomic_write(file_name, bytes)
}

pub(crate) fn read_bounded_regular(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let file = open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent file type or size was invalid",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "persistent file exceeded its bound",
        ));
    }
    Ok(bytes)
}

/// Absolute, with only normal components (no `.` or `..`).
fn valid_root_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    path.is_absolute()
        && path
            .as_os_str()
            .as_bytes()
            .split(|byte| *byte == b'/')
            .all(|segment| segment != b"." && segment != b"..")
}

fn ensure_app_directory(path: &Path) -> io::Result<()> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Launcher path was not absolute",
        ));
    }
    Directory::ensure_absolute(path).map(|_| ())
}

fn ensure_owned_directory(path: &Path) -> io::Result<()> {
    Directory::ensure_absolute(path).map(|_| ())
}

/// The Launcher-owned skeleton inside a proven managed root.
pub(crate) fn open_root_layout(managed: &Directory) -> io::Result<()> {
    managed.ensure_directory(Path::new(WINE_CACHE_DIRECTORY))?;
    let lock = managed.open_or_create_file(std::ffi::OsStr::new(".pd2launcher.lock"))?;
    if !lock.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "installation lock is not a regular file",
        ));
    }
    lock.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pd2s_directdraw_setting_restores_opengl_and_an_unchosen_one_leaves_the_file() {
        assert_eq!(DirectDrawRendererSetting::Untouched.cnc_ddraw_value(), None);
        assert_eq!(
            DirectDrawRendererSetting::Pd2.cnc_ddraw_value(),
            Some("opengl")
        );
        assert_eq!(
            serde_json::to_string(&DirectDrawRendererSetting::Pd2).unwrap(),
            "\"pd2\""
        );
        assert!(is_untouched_direct_draw(
            &DirectDrawRendererSetting::default()
        ));
    }
    use tempfile::TempDir;

    #[test]
    fn opens_only_the_launcher_layout_and_creates_the_shared_regular_lock() {
        let _discover = LauncherPaths::discover;
        let fixture = TempDir::new().expect("fixture");
        let paths = LauncherPaths::from_home(fixture.path().join(".pd2launcher"));

        paths.open_managed_layout().expect("layout");

        assert!(paths.managed_root().is_dir());
        assert!(paths.managed_root().join(WINE_CACHE_DIRECTORY).is_dir());
        assert!(paths.installation_lock().is_file());
        assert!(!paths.project_d2_root().exists());
        let home = fixture.path().join(".pd2launcher");
        assert_eq!(paths.managed_root(), home.join("game"));
        assert_eq!(paths.settings_file(), home.join("config/settings.json"));
        assert!(home.join("logs/diagnostics").is_dir());
        assert!(home.join("logs/exports").is_dir());
    }

    #[test]
    fn the_home_is_below_home_or_where_the_override_names_it() {
        let environment = |override_value: Option<&'static str>| {
            move |name: &str| -> Option<OsString> {
                match name {
                    "HOME" => Some("/home/player".into()),
                    HOME_OVERRIDE => override_value.map(Into::into),
                    _ => None,
                }
            }
        };
        let default = LauncherPaths::discover_from(environment(None)).unwrap();
        assert_eq!(default.home(), Some(Path::new("/home/player/.pd2launcher")));
        let custom = LauncherPaths::discover_from(environment(Some("/srv/pd2"))).unwrap();
        assert_eq!(custom.managed_root(), Path::new("/srv/pd2/game"));
        assert_eq!(
            custom.settings_file(),
            Path::new("/srv/pd2/config/settings.json")
        );
        for invalid in ["relative/pd2", "/tmp/../pd2", "/tmp/./pd2"] {
            assert!(
                LauncherPaths::discover_from(environment(Some(invalid))).is_err(),
                "{invalid}"
            );
        }
    }

    fn fixture_paths(fixture: &TempDir) -> LauncherPaths {
        LauncherPaths::from_home(fixture.path().join(".pd2launcher"))
    }

    #[test]
    fn the_default_root_needs_no_pointer_and_gets_its_marker_once() {
        let fixture = TempDir::new().expect("fixture");
        let paths = fixture_paths(&fixture);
        let selection = paths.open_managed_layout().expect("layout");
        assert!(paths.uses_default_root());
        assert_eq!(
            paths.managed_root(),
            fixture.path().join(".pd2launcher/game")
        );
        let root = Directory::open(&paths.managed_root()).unwrap();
        let id = location::read_marker(&root)
            .unwrap()
            .expect("marker written");
        assert!(!paths.config_root().join(location::POINTER_FILE).exists());
        drop(selection);
        paths.open_managed_layout().expect("reopen");
        assert_eq!(location::read_marker(&root).unwrap(), Some(id));
    }

    #[test]
    fn a_chosen_root_is_used_only_when_its_marker_matches_the_pointer() {
        let fixture = TempDir::new().expect("fixture");
        let paths = fixture_paths(&fixture);
        drop(paths.open_managed_layout().expect("default layout"));
        let chosen = fixture.path().join("card/pd2-launcher");
        fs::create_dir_all(&chosen).unwrap();
        let id = location::new_root_id().unwrap();
        location::write_marker(&Directory::open(&chosen).unwrap(), &id).unwrap();
        location::write_pointer(
            paths.config_root(),
            &location::Pointer::new(chosen.clone(), id.clone()),
        )
        .unwrap();

        let reopened = fixture_paths(&fixture);
        drop(reopened.open_managed_layout().expect("chosen layout"));
        assert_eq!(reopened.managed_root(), chosen);
        assert!(!reopened.uses_default_root());
        assert!(chosen.join(".pd2launcher.lock").is_file());
        assert!(chosen.join(WINE_CACHE_DIRECTORY).is_dir());

        // Unmounted: the mount point is empty. Nothing is created in it and
        // the default is not used instead.
        fs::remove_dir_all(&chosen).unwrap();
        fs::create_dir(&chosen).unwrap();
        let unmounted = fixture_paths(&fixture);
        assert!(matches!(
            unmounted.open_managed_layout(),
            Err(LayoutError::LocationUnavailable(Some(path))) if path == chosen
        ));
        assert_eq!(fs::read_dir(&chosen).unwrap().count(), 0);
        // Gone entirely: never re-created.
        fs::remove_dir(&chosen).unwrap();
        assert!(matches!(
            fixture_paths(&fixture).open_managed_layout(),
            Err(LayoutError::LocationUnavailable(Some(_)))
        ));
        assert!(!chosen.exists());
        // Another root's marker.
        fs::create_dir(&chosen).unwrap();
        location::write_marker(
            &Directory::open(&chosen).unwrap(),
            &location::new_root_id().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            fixture_paths(&fixture).open_managed_layout(),
            Err(LayoutError::LocationUnavailable(Some(_)))
        ));
        // An unreadable pointer is unavailable too, never the default.
        fs::write(paths.config_root().join(location::POINTER_FILE), b"{").unwrap();
        assert!(matches!(
            fixture_paths(&fixture).open_managed_layout(),
            Err(LayoutError::LocationUnavailable(None))
        ));
    }

    #[test]
    fn a_launcher_changing_the_location_blocks_opening_it() {
        let fixture = TempDir::new().expect("fixture");
        let paths = fixture_paths(&fixture);
        let selection = paths.open_managed_layout().expect("layout");
        let exclusive = selection.upgrade().expect("only holder");
        assert!(matches!(
            fixture_paths(&fixture).open_managed_layout(),
            Err(LayoutError::SelectionBusy)
        ));
        drop(exclusive);
        assert!(fixture_paths(&fixture).open_managed_layout().is_ok());
    }

    #[test]
    fn settings_round_trip_contains_choices_but_has_no_authorization_field() {
        let fixture = TempDir::new().expect("fixture");
        let store = SettingsStore::new(fixture.path().join("config/settings.json"));
        let settings = LauncherSettings::default()
            .with_renderer(RendererSetting::DirectDraw)
            .with_skip_to_bnet(false)
            .with_sound_in_background(true);

        store.save(&settings).expect("save");
        assert_eq!(store.load().expect("load"), settings);
        assert_eq!(settings.renderer(), RendererSetting::DirectDraw);
        assert!(!settings.skip_to_bnet());
        assert!(settings.sound_in_background());
        assert_eq!(settings.network_concurrency, 2);
        let bytes = fs::read(fixture.path().join("config/settings.json")).expect("settings");
        let text = String::from_utf8(bytes).expect("UTF-8");
        assert!(!text.contains("key"));
        assert!(!text.contains("authorization"));
    }

    #[test]
    fn official_launcher_software_rendering_is_on_unless_saved_off() {
        let fixture = TempDir::new().expect("fixture");
        let path = fixture.path().join("config/settings.json");
        let store = SettingsStore::new(&path);
        // A file written before the setting existed reads as on.
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2}"#,
        )
        .unwrap();
        assert!(
            store
                .load()
                .expect("load")
                .official_launcher_software_rendering()
        );
        assert!(LauncherSettings::default().official_launcher_software_rendering());

        // On is the default and is not written, so older Launchers still
        // read the file; off is written and survives a restart.
        store.save(&LauncherSettings::default()).expect("save");
        assert!(
            !fs::read_to_string(&path)
                .unwrap()
                .contains("software_rendering")
        );
        let off = LauncherSettings::default().with_official_launcher_software_rendering(false);
        store.save(&off).expect("save");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""official_launcher_software_rendering":false"#)
        );
        assert!(
            !SettingsStore::new(&path)
                .load()
                .expect("load")
                .official_launcher_software_rendering()
        );
    }

    #[test]
    fn an_older_fex_path_is_the_device_fex_and_built_in_round_trips() {
        let fixture = TempDir::new().expect("fixture");
        let path = fixture.path().join("config/settings.json");
        let store = SettingsStore::new(&path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2,"fex":"/usr/bin/FEX"}"#,
        )
        .unwrap();
        let settings = store.load().expect("load");
        assert_eq!(settings.fex_source(), FexSource::Device);
        assert!(!settings.fex_source_chosen());
        assert_eq!(settings.fex(), Some(Path::new("/usr/bin/FEX")));
        store.save(&settings).expect("save");
        let saved = fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("fex_source"), "no choice is not written");
        store
            .save(&settings.clone().with_fex_source(FexSource::Device))
            .expect("save");
        assert!(
            store.load().expect("load").fex_source_chosen(),
            "a chosen device FEX is kept"
        );
        assert!(saved.contains("/usr/bin/FEX"));
        store
            .save(&settings.with_fex_source(FexSource::Builtin))
            .expect("save");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""fex_source":"builtin""#)
        );
        let loaded = store.load().expect("load");
        assert_eq!(loaded.fex_source(), FexSource::Builtin);
        assert!(loaded.fex_source_chosen());
        assert_eq!(
            loaded.fex(),
            Some(Path::new("/usr/bin/FEX")),
            "the device path is kept"
        );
    }

    #[test]
    fn the_app_menu_entry_is_on_unless_saved_off() {
        let fixture = TempDir::new().expect("fixture");
        let path = fixture.path().join("config/settings.json");
        let store = SettingsStore::new(&path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2}"#,
        )
        .unwrap();
        assert!(store.load().expect("load").app_menu_entry());
        store.save(&LauncherSettings::default()).expect("save");
        assert!(!fs::read_to_string(&path).unwrap().contains("app_menu"));
        store
            .save(&LauncherSettings::default().with_app_menu_entry(false))
            .expect("save");
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains(r#""app_menu_entry":false"#)
        );
        assert!(!store.load().expect("load").app_menu_entry());
    }

    #[cfg(unix)]
    #[test]
    fn settings_and_layout_reject_symlink_substitution() {
        use std::os::unix::fs::symlink;

        let fixture = TempDir::new().expect("fixture");
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).expect("outside");
        let home = fixture.path().join(".pd2launcher");
        symlink(&outside, &home).expect("symlink");
        let paths = LauncherPaths::from_home(home);
        assert!(paths.open_managed_layout().is_err());

        let settings = fixture.path().join("settings.json");
        let target = fixture.path().join("target.json");
        fs::write(&target, b"{}").expect("target");
        symlink(&target, &settings).expect("settings symlink");
        let store = SettingsStore::new(settings);
        assert!(store.load().is_err());
        assert!(store.save(&LauncherSettings::default()).is_err());
        assert_eq!(fs::read(target).expect("target unchanged"), b"{}");
    }

    #[cfg(unix)]
    #[test]
    fn the_setup_record_round_trips_and_anything_else_reads_as_none() {
        use std::os::unix::fs::symlink;

        let fixture = TempDir::new().expect("fixture");
        let paths = fixture_paths(&fixture);
        let path = paths.setup_record_file();
        assert_eq!(path, fixture.path().join(".pd2launcher/config/setup.json"));
        // Absent: decided by the check.
        assert_eq!(load_setup_record(&path), SetupRecord::None);
        for record in [
            SetupRecord::Started,
            SetupRecord::Installed,
            SetupRecord::None,
        ] {
            save_setup_record(&path, record).expect("save");
            assert_eq!(load_setup_record(&path), record);
        }
        save_setup_record(&path, SetupRecord::Installed).expect("save");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            r#"{"schema":1,"setup":"installed"}"#
        );

        for written in [
            r#"{"schema":2,"setup":"installed"}"#,
            r#"{"schema":1,"setup":"ready"}"#,
            r#"{"schema":1,"setup":"installed","extra":true}"#,
            "not json",
        ] {
            fs::write(&path, written).unwrap();
            assert_eq!(load_setup_record(&path), SetupRecord::None, "{written}");
        }

        // A link in its place is neither read nor replaced.
        fs::remove_file(&path).unwrap();
        let target = fixture.path().join("target.json");
        fs::write(&target, r#"{"schema":1,"setup":"installed"}"#).unwrap();
        symlink(&target, &path).expect("symlink");
        assert_eq!(load_setup_record(&path), SetupRecord::None);
        assert!(save_setup_record(&path, SetupRecord::Started).is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            r#"{"schema":1,"setup":"installed"}"#
        );
    }

    #[test]
    fn download_sources_round_trip_and_older_files_still_load() {
        let fixture = TempDir::new().expect("fixture");
        let path = fixture.path().join("config/settings.json");
        let store = SettingsStore::new(&path);
        store.save(&LauncherSettings::default()).expect("save");
        let text = fs::read_to_string(&path).expect("settings");
        assert!(!text.contains("download_sources"), "{text}");

        let settings = LauncherSettings::default()
            .with_download_source(
                DownloadArtifact::Wine,
                Some("https://mirror.example/wine.tar.xz".to_owned()),
            )
            .with_download_source(
                DownloadArtifact::Pd2ClientBase,
                Some("https://mirror.example/pd2".to_owned()),
            );
        store.save(&settings).expect("save sources");
        let loaded = SettingsStore::new(&path).load().expect("reload");
        assert_eq!(loaded, settings);
        assert_eq!(
            loaded
                .download_sources()
                .get(&DownloadArtifact::Wine)
                .map(String::as_str),
            Some("https://mirror.example/wine.tar.xz")
        );
        let reset = loaded.with_download_source(DownloadArtifact::Wine, None);
        store.save(&reset).expect("save reset");
        assert_eq!(store.load().expect("reload").download_sources().len(), 1);

        // A settings file from before download sources existed.
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2}"#,
        )
        .expect("legacy settings");
        assert!(store.load().expect("legacy").download_sources().is_empty());

        // A settings file with the unused Wine choice and host environment
        // fields still loads, and saving drops those fields.
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2,"existing_wine":"/opt/proton/files/bin/wine","environment":{"WINEDEBUG":"host"}}"#,
        )
        .expect("parked settings");
        let parked = store.load().expect("parked settings load");
        store.save(&parked).expect("save parked");
        let saved = fs::read_to_string(&path).expect("saved settings");
        assert!(
            !saved.contains("existing_wine") && !saved.contains("environment"),
            "{saved}"
        );

        // Hand-edited invalid or unknown sources are dropped and counted;
        // they never make the rest of the settings (or Save/Reset) unusable.
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"direct-draw","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2,"download_sources":{"wine":"file:///etc/passwd","gecko":"https://mirror.example/g.msi","future-thing":"https://x.example/","other":7}}"#,
        )
        .expect("invalid source");
        let loaded = store.load().expect("lenient sources");
        assert_eq!(loaded.renderer(), RendererSetting::DirectDraw);
        assert_eq!(loaded.ignored_download_sources(), 3);
        assert_eq!(
            loaded
                .download_sources()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [DownloadArtifact::Gecko]
        );
        store
            .save(&loaded.with_download_source(DownloadArtifact::Gecko, None))
            .expect("reset with invalid entries");
        let reset = store.load().expect("reload");
        assert!(reset.download_sources().is_empty());
        assert_eq!(reset.ignored_download_sources(), 0);
        // A saved source for the retired 1.14d patch is dropped
        // quietly; the rest of the file still loads.
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2,"download_sources":{"patch":"https://mirror.example/LODPatch_114d.exe","gecko":"https://mirror.example/g.msi"}}"#,
        )
        .expect("retired source");
        let loaded = store.load().expect("retired source loads");
        assert_eq!(loaded.ignored_download_sources(), 0);
        assert_eq!(
            loaded
                .download_sources()
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [DownloadArtifact::Gecko]
        );
        store.save(&loaded).expect("save drops the retired key");
        assert!(!fs::read_to_string(&path).unwrap().contains("patch"));
        fs::write(
            &path,
            br#"{"schema":1,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2,"download_sources":"nonsense"}"#,
        )
        .expect("non-map sources");
        assert_eq!(store.load().expect("lenient").ignored_download_sources(), 1);
    }

    #[test]
    fn invalid_settings_fail_closed_instead_of_becoming_defaults() {
        let fixture = TempDir::new().expect("fixture");
        let path = fixture.path().join("config/settings.json");
        fs::create_dir_all(path.parent().expect("parent")).expect("config");
        fs::write(
            &path,
            br#"{"schema":99,"renderer":"glide","skip_to_bnet":true,"sound_in_background":false,"network_concurrency":2}"#,
        )
        .expect("invalid settings");

        let store = SettingsStore::new(path);
        assert_eq!(
            store.load().expect_err("schema must fail").kind(),
            io::ErrorKind::InvalidData
        );
    }
}
