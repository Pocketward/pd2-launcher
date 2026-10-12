//! The built-in FEX: FEX's own build of FEX for Ubuntu 22.04,
//! in the CPU level this device supports, and FEX's Ubuntu 24.04 RootFS,
//! kept in the managed root and used only by the Launcher's Wine.
//!
//! Everything lives in `<managed root>/fex`: the two FEX programs in
//! `bin/`, the unpacked RootFS in `rootfs/`, and FEX's own configuration,
//! data, and cache folders. FEX gets its own data folder and server socket
//! name, so a FEXServer the device already runs (which serves the device's
//! own RootFS to every client that reaches it) never answers it.

use std::{
    ffi::OsString,
    fs::{self, File},
    io::{self, BufReader, Read, Write},
    os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _},
    path::{Component, Path, PathBuf},
};

use sha2::{Digest as _, Sha256};

/// The folder in the managed root.
pub(crate) const DIRECTORY: &str = "fex";
/// FEX's release in every package below.
pub(crate) const FEX_VERSION: &str = "2609.1";
const RECORD: &str = "installed.json";
const RECORD_SCHEMA: u32 = 1;
const ROOTFS_NAME: &str = "Ubuntu_24_04";
const STAGING: &str = "fex.staging";
const RETIRED: &str = "fex.retired";

/// FEX's Ubuntu 24.04 RootFS, from FEX's own server.
pub(crate) const ROOTFS_URL: &str =
    "https://rootfs.fex-emu.gg/Ubuntu_24_04/2026-08-11/Ubuntu_24_04.sqsh";
pub(crate) const ROOTFS_SIZE: u64 = 525_615_104;
pub(crate) const ROOTFS_SHA256: &str =
    "2854b06d3ff1b8f6e526135bfb6dd5b7b30ab3ab73e79ae933a3d9fed959a178";
pub(crate) const ROOTFS_CACHE_NAME: &str = "fex-rootfs-ubuntu-24.04-2026-08-11.verified";
/// The unpacked RootFS (1.89 GB), with room to spare for the programs and
/// FEX's caches.
const UNPACKED_BYTES: u64 = 2_100_000_000;

/// FEX's three builds of one release, each compiled for a CPU level
/// (`-march=armv8.0-a`, `armv8.2-a`, `armv8.4-a`).
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum FexVariant {
    Armv8_0,
    Armv8_2,
    Armv8_4,
}

/// The features a CPU level adds that a compiler may use for it, as Linux
/// names them in `/proc/cpuinfo`.
const ARMV8_2_FEATURES: &[&str] = &["atomics", "asimdrdm", "crc32", "dcpop"];
const ARMV8_4_FEATURES: &[&str] = &[
    "atomics", "asimdrdm", "crc32", "dcpop", "lrcpc", "jscvt", "fcma", "ilrcpc", "flagm", "uscat",
    "asimddp", "dit",
];

impl FexVariant {
    pub(crate) const fn package_url(self) -> &'static str {
        match self {
            Self::Armv8_0 => {
                "https://ppa.launchpadcontent.net/fex-emu/fex/ubuntu/pool/main/f/fex-emu-armv8.0/fex-emu-armv8.0_2609.1-1~j_arm64.deb"
            }
            Self::Armv8_2 => {
                "https://ppa.launchpadcontent.net/fex-emu/fex/ubuntu/pool/main/f/fex-emu-armv8.2/fex-emu-armv8.2_2609.1-1~j_arm64.deb"
            }
            Self::Armv8_4 => {
                "https://ppa.launchpadcontent.net/fex-emu/fex/ubuntu/pool/main/f/fex-emu-armv8.4/fex-emu-armv8.4_2609.1-1~j_arm64.deb"
            }
        }
    }

    pub(crate) const fn package_size(self) -> u64 {
        match self {
            Self::Armv8_0 => 3_407_810,
            Self::Armv8_2 => 3_411_742,
            Self::Armv8_4 => 3_415_064,
        }
    }

    pub(crate) const fn package_sha256(self) -> &'static str {
        match self {
            Self::Armv8_0 => "2c4e97b245bf32374cdafc9c6908727e1948319dc959e941d2bdd21bf23a68cc",
            Self::Armv8_2 => "7418a68b713e1828d71019ec7c29b676fbdfc1825873fd68c7b072432f5f65bc",
            Self::Armv8_4 => "b401af7c1b92c304abb640d0b3eaea81ca900d8597079979915c4edea850474d",
        }
    }

    pub(crate) const fn cache_name(self) -> &'static str {
        match self {
            Self::Armv8_0 => "fex-emu-armv8.0_2609.1.verified",
            Self::Armv8_2 => "fex-emu-armv8.2_2609.1.verified",
            Self::Armv8_4 => "fex-emu-armv8.4_2609.1.verified",
        }
    }

    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::Armv8_0, Self::Armv8_2, Self::Armv8_4];

    /// The highest CPU level whose features `features` (a `/proc/cpuinfo`
    /// `Features` line) all has.
    pub(crate) fn for_features(features: &str) -> Self {
        let present = features
            .split_once(':')
            .map_or(features, |(_, list)| list)
            .split_whitespace()
            .collect::<Vec<_>>();
        let has = |wanted: &[&str]| wanted.iter().all(|feature| present.contains(feature));
        if has(ARMV8_4_FEATURES) {
            Self::Armv8_4
        } else if has(ARMV8_2_FEATURES) {
            Self::Armv8_2
        } else {
            Self::Armv8_0
        }
    }

    /// This device's CPU level; the baseline build when it can't be read.
    pub(crate) fn current() -> Self {
        fs::read_to_string("/proc/cpuinfo")
            .ok()
            .and_then(|cpuinfo| {
                cpuinfo
                    .lines()
                    .find(|line| line.starts_with("Features"))
                    .map(Self::for_features)
            })
            .unwrap_or(Self::Armv8_0)
    }
}

/// Whether the built-in FEX is the one in use on this ARM64 build: see
/// [`chosen`].
pub(crate) fn in_use(
    settings: Option<&crate::persistence::LauncherSettings>,
    managed_root: &Path,
) -> bool {
    cfg!(target_arch = "aarch64") && chosen(settings, managed_root)
}

/// The player's choice, or, before they chose, whether `managed_root`
/// holds no installation yet (the step 1 default; an installation set up
/// with the device's FEX keeps it). The Download step saves the default as
/// the choice, since the root does not look fresh after it.
pub(crate) fn chosen(
    settings: Option<&crate::persistence::LauncherSettings>,
    managed_root: &Path,
) -> bool {
    use crate::persistence::FexSource;
    match settings {
        Some(settings) if settings.fex_source_chosen() => {
            settings.fex_source() == FexSource::Builtin
        }
        _ => fresh(managed_root),
    }
}

/// No prefix, runtime, or downloads in `managed_root` yet.
fn fresh(managed_root: &Path) -> bool {
    match crate::safe_fs::Directory::open(managed_root) {
        Ok(root) => crate::relocation::holds_installation(&root).is_ok_and(|holds| !holds),
        Err(error) => error.kind() == io::ErrorKind::NotFound,
    }
}

/// Where each part of the built-in FEX lives below a managed root.
pub(crate) struct Layout {
    directory: PathBuf,
}

impl Layout {
    pub(crate) fn new(managed_root: &Path) -> Self {
        Self {
            directory: managed_root.join(DIRECTORY),
        }
    }

    pub(crate) fn interpreter(&self) -> PathBuf {
        self.directory.join("bin/FEX")
    }

    fn rootfs(&self) -> PathBuf {
        self.directory.join("rootfs").join(ROOTFS_NAME)
    }

    /// The environment every guest gets from the built-in FEX.
    pub(crate) fn environment(&self) -> Vec<(OsString, OsString)> {
        let folder = |name: &str| {
            let mut path = self.directory.join(name).into_os_string();
            path.push("/");
            path
        };
        vec![
            (OsString::from("FEX_ROOTFS"), self.rootfs().into_os_string()),
            (OsString::from("FEX_APP_CONFIG_LOCATION"), folder("config")),
            (OsString::from("FEX_APP_DATA_LOCATION"), folder("data")),
            (OsString::from("FEX_APP_CACHE_LOCATION"), folder("cache")),
            (
                OsString::from("FEX_SERVERSOCKETPATH"),
                OsString::from(server_socket_name(&self.directory)),
            ),
        ]
    }
}

/// An abstract socket name of this installation's own FEXServer: never the
/// default `<uid>.FEXServer.Socket` a device FEX uses.
fn server_socket_name(directory: &Path) -> String {
    let digest = Sha256::digest(directory.as_os_str().as_encoded_bytes());
    let short = digest[..6]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "pd2launcher-{}-{short}.FEXServer.Socket",
        rustix::process::getuid().as_raw()
    )
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    schema: u32,
    fex_version: String,
    variant: FexVariant,
    package_sha256: String,
    rootfs_sha256: String,
}

impl Record {
    fn current(variant: FexVariant) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            fex_version: FEX_VERSION.to_owned(),
            variant,
            package_sha256: variant.package_sha256().to_owned(),
            rootfs_sha256: ROOTFS_SHA256.to_owned(),
        }
    }
}

/// Whether the built-in FEX this release pins for `variant` is installed
/// in `managed_root`, complete.
pub(crate) fn installed_as(managed_root: &Path, variant: FexVariant) -> bool {
    let layout = Layout::new(managed_root);
    let Ok(record) = fs::read(layout.directory.join(RECORD)) else {
        return false;
    };
    serde_json::from_slice::<Record>(&record).is_ok_and(|record| record == Record::current(variant))
        && [layout.interpreter(), layout.directory.join("bin/FEXServer")]
            .iter()
            .all(|program| {
                fs::metadata(program).is_ok_and(|metadata| {
                    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
                })
            })
        && layout.rootfs().join("usr").is_dir()
}

/// [`installed_as`] for this device's CPU level.
pub(crate) fn installed(managed_root: &Path) -> bool {
    installed_as(managed_root, FexVariant::current())
}

/// The engine environment of the Android app that names App-provided FEX:
/// its entry point, read only on the Android host.
const APP_FEX: &str = "PD2_APP_FEX";
/// The app's folders of FEX's host and guest thunk libraries (FEX adds
/// `_32` itself for 32-bit guests).
const APP_FEX_THUNKS_HOST: &str = "PD2_APP_FEX_THUNKS_HOST";
const APP_FEX_THUNKS_GUEST: &str = "PD2_APP_FEX_THUNKS_GUEST";
/// `PD2_APP_FEX_HOSTENV_<NAME>=<value>` becomes FEX's host-only
/// `"HostEnv": "<NAME>=<value>"`: the GL thunk's ARM64 Mesa and Vulkan
/// settings, which never reach a guest.
const APP_FEX_HOSTENV_PREFIX: &str = "PD2_APP_FEX_HOSTENV_";
/// `PD2_APP_FEX_COMPATIBILITY_HOSTENV_<NAME>=<value>`: a host value of the
/// app's Compatibility graphics driver, added to (or replacing) the host
/// values above for a Play with it.
const APP_FEX_COMPATIBILITY_HOSTENV_PREFIX: &str = "PD2_APP_FEX_COMPATIBILITY_HOSTENV_";

/// App-provided FEX: the Android app's FEX, run
/// through the entry point the app names, with the Launcher's RootFS and FEX
/// folders in the managed root as for the built-in FEX.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AppFex {
    entry_point: PathBuf,
    thunks_host: Option<PathBuf>,
    thunks_guest: Option<PathBuf>,
    host_environment: Vec<(String, String)>,
    compatibility_environment: Vec<(String, String)>,
}

impl AppFex {
    /// From the Launcher engine's environment; `None` without an absolute
    /// `PD2_APP_FEX`. Only the Android host calls this.
    pub(crate) fn from_environment(
        environment: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> Option<Self> {
        let mut entry_point = None;
        let mut thunks_host = None;
        let mut thunks_guest = None;
        let mut host_environment = Vec::new();
        let mut compatibility_environment = Vec::new();
        let absolute =
            |value: OsString| Some(PathBuf::from(value)).filter(|path| path.is_absolute());
        for (name, value) in environment {
            let Some(name) = name.to_str() else {
                continue;
            };
            match name {
                APP_FEX => entry_point = absolute(value),
                APP_FEX_THUNKS_HOST => thunks_host = absolute(value),
                APP_FEX_THUNKS_GUEST => thunks_guest = absolute(value),
                _ => {
                    let (values, host_name) =
                        if let Some(host_name) = name.strip_prefix(APP_FEX_HOSTENV_PREFIX) {
                            (&mut host_environment, host_name)
                        } else if let Some(host_name) =
                            name.strip_prefix(APP_FEX_COMPATIBILITY_HOSTENV_PREFIX)
                        {
                            (&mut compatibility_environment, host_name)
                        } else {
                            continue;
                        };
                    if is_environment_name(host_name)
                        && let Some(value) = value.to_str()
                        && !value.chars().any(char::is_control)
                    {
                        values.push((host_name.to_owned(), value.to_owned()));
                    }
                }
            }
        }
        host_environment.sort();
        compatibility_environment.sort();
        Some(Self {
            entry_point: entry_point?,
            thunks_host,
            thunks_guest,
            host_environment,
            compatibility_environment,
        })
    }

    pub(crate) fn entry_point(&self) -> &Path {
        &self.entry_point
    }

    /// The Compatibility graphics driver's host values, as `NAME=value`.
    pub(crate) fn compatibility_values(&self) -> Vec<String> {
        self.compatibility_environment
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect()
    }

    /// FEX's `Config.json`: the thunk folders, the GL thunk on (Vulkan off), and one
    /// `HostEnv` entry per host value, with the Compatibility values over
    /// them when `compatibility`. FEX reads a string-array option as
    /// repeated keys, so the JSON is written by hand.
    fn config(&self, compatibility: bool) -> String {
        let quote = |text: &str| serde_json::to_string(text).expect("a string encodes");
        let mut entries = Vec::new();
        for (key, folder) in [
            ("ThunkHostLibs", &self.thunks_host),
            ("ThunkGuestLibs", &self.thunks_guest),
        ] {
            if let Some(folder) = folder.as_deref().and_then(Path::to_str) {
                entries.push(format!("    {}: {}", quote(key), quote(folder)));
            }
        }
        let mut host_environment = self
            .host_environment
            .iter()
            .cloned()
            .collect::<std::collections::BTreeMap<_, _>>();
        if compatibility {
            host_environment.extend(self.compatibility_environment.iter().cloned());
        }
        for (name, value) in &host_environment {
            entries.push(format!(
                "    \"HostEnv\": {}",
                quote(&format!("{name}={value}"))
            ));
        }
        format!(
            "{{\n  \"Config\": {{\n{}\n  }},\n  \"ThunksDB\": {{\n    \"GL\": 1,\n    \"Vulkan\": 0\n  }}\n}}\n",
            entries.join(",\n")
        )
    }

    /// Write `<managed root>/fex/config/Config.json` (and FEX's data and
    /// cache folders) unless it already says the same; `compatibility`: with
    /// the Compatibility graphics driver's values ([`Self::config`]).
    pub(crate) fn write_config(&self, managed_root: &Path, compatibility: bool) -> io::Result<()> {
        let directory = managed_root.join(DIRECTORY);
        for folder in ["config", "data", "cache"] {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(directory.join(folder))?;
        }
        let path = directory.join("config/Config.json");
        let config = self.config(compatibility);
        if fs::read(&path).is_ok_and(|current| current == config.as_bytes()) {
            return Ok(());
        }
        // A name of its own per writer: the engine and a `--stop-wine` run
        // may write at the same time.
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let staging = directory.join(format!(
            "config/Config.json.{}.{}.tmp",
            std::process::id(),
            WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let written = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)
            .and_then(|mut file| {
                file.write_all(config.as_bytes())?;
                file.sync_all()
            })
            .and_then(|()| fs::rename(&staging, &path));
        if written.is_err() {
            let _ = fs::remove_file(&staging);
        }
        written
    }
}

/// A POSIX environment name.
fn is_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// The record's FEX on the Android host: App-provided FEX, with the RootFS
/// alone, which the app installs from its bundled runtime.
const APP_PROVIDED: &str = "app-provided";

/// `installed.json` of the RootFS installed alone for App-provided FEX.
#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct AppRecord {
    schema: u32,
    fex: String,
    rootfs_sha256: String,
    /// The FEX probe through App-provided FEX passed with this RootFS, so
    /// opening the app runs no FEX.
    fex_probe_passed: bool,
    /// The Launcher's runtime preflight (`wine --version`) through
    /// App-provided FEX passed, so opening the app runs no Wine either.
    #[serde(default)]
    wine_preflight_passed: bool,
}

impl AppRecord {
    fn current(fex_probe_passed: bool, wine_preflight_passed: bool) -> Self {
        Self {
            schema: RECORD_SCHEMA,
            fex: APP_PROVIDED.to_owned(),
            rootfs_sha256: ROOTFS_SHA256.to_owned(),
            fex_probe_passed,
            wine_preflight_passed,
        }
    }
}

/// The App-provided FEX record of `managed_root`, when it names the RootFS
/// this release pins and that RootFS is unpacked.
fn app_record(managed_root: &Path) -> Option<AppRecord> {
    let layout = Layout::new(managed_root);
    let record = fs::read(layout.directory.join(RECORD)).ok()?;
    let record = serde_json::from_slice::<AppRecord>(&record).ok()?;
    (record == AppRecord::current(record.fex_probe_passed, record.wine_preflight_passed)
        && layout.rootfs().join("usr").is_dir())
    .then_some(record)
}

/// Whether the RootFS for App-provided FEX is installed in `managed_root`
/// (App-provided FEX is pending until then).
pub(crate) fn rootfs_present(managed_root: &Path) -> bool {
    app_record(managed_root).is_some()
}

/// An installed RootFS for App-provided FEX, as far as its record and `usr`
/// folder go.
#[cfg(test)]
pub(crate) fn install_test_app_rootfs(managed_root: &Path) {
    let layout = Layout::new(managed_root);
    fs::create_dir_all(layout.rootfs().join("usr")).unwrap();
    write_record(&layout.directory, &AppRecord::current(false, false)).unwrap();
}

/// Whether the FEX probe already passed with the installed RootFS.
pub(crate) fn app_probe_passed(managed_root: &Path) -> bool {
    app_record(managed_root).is_some_and(|record| record.fex_probe_passed)
}

/// Keep the FEX probe's result in the RootFS's record; nothing without an
/// installed RootFS.
pub(crate) fn record_app_probe(managed_root: &Path, passed: bool) -> io::Result<()> {
    match app_record(managed_root) {
        // A failed probe also forgets the runtime preflight's pass.
        Some(record) if record.fex_probe_passed != passed => write_record(
            &Layout::new(managed_root).directory,
            &AppRecord::current(passed, passed && record.wine_preflight_passed),
        ),
        _ => Ok(()),
    }
}

/// Whether the runtime preflight already passed through App-provided FEX.
pub(crate) fn app_wine_preflight_passed(managed_root: &Path) -> bool {
    app_record(managed_root).is_some_and(|record| record.wine_preflight_passed)
}

/// Keep the runtime preflight's result in the RootFS's record; nothing
/// without an installed RootFS.
pub(crate) fn record_app_wine_preflight(managed_root: &Path, passed: bool) -> io::Result<()> {
    match app_record(managed_root) {
        Some(record) if record.wine_preflight_passed != passed => write_record(
            &Layout::new(managed_root).directory,
            &AppRecord::current(record.fex_probe_passed, passed),
        ),
        _ => Ok(()),
    }
}

/// Write `installed.json` in `directory` through a staging file renamed
/// into place.
fn write_record(directory: &Path, record: &impl serde::Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record encode failed"))?;
    let staging = directory.join(format!("{RECORD}.tmp"));
    let mut file = File::create(&staging)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    fs::rename(&staging, directory.join(RECORD))
}

/// Install the built-in FEX from its verified package and RootFS into
/// `managed_root`, replacing an installed one. It is assembled beside the
/// final folder and renamed into place, and its record is written last, so
/// a stop part way leaves no installation that counts as complete.
pub(crate) fn install(
    managed_root: &Path,
    variant: FexVariant,
    package: &Path,
    rootfs: &Path,
) -> io::Result<()> {
    install_with_space(managed_root, variant, package, rootfs, UNPACKED_BYTES)
}

fn install_with_space(
    managed_root: &Path,
    variant: FexVariant,
    package: &Path,
    rootfs: &Path,
    needed: u64,
) -> io::Result<()> {
    ensure_space(managed_root, needed)?;
    let staging = managed_root.join(STAGING);
    remove_if_present(&staging)?;
    fs::create_dir(&staging)?;
    let bin = staging.join("bin");
    fs::create_dir(&bin)?;
    extract_programs(package, &bin)?;
    let rootfs_parent = staging.join("rootfs");
    fs::create_dir(&rootfs_parent)?;
    unpack_squashfs(rootfs, &rootfs_parent.join(ROOTFS_NAME))?;
    for folder in ["config", "data", "cache"] {
        fs::create_dir(staging.join(folder))?;
    }
    let record = serde_json::to_vec_pretty(&Record::current(variant))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "record encode failed"))?;
    let mut file = File::create(staging.join(RECORD))?;
    file.write_all(&record)?;
    file.sync_all()?;
    let directory = managed_root.join(DIRECTORY);
    let retired = managed_root.join(RETIRED);
    remove_if_present(&retired)?;
    if fs::symlink_metadata(&directory).is_ok() {
        fs::rename(&directory, &retired)?;
    }
    fs::rename(&staging, &directory)?;
    File::open(managed_root)?.sync_all()?;
    remove_if_present(&retired)
}

fn ensure_space(managed_root: &Path, needed: u64) -> io::Result<()> {
    let stat = rustix::fs::statvfs(managed_root).map_err(io::Error::from)?;
    let available = stat.f_bavail.saturating_mul(stat.f_frsize);
    if available < needed {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "not enough free space for the built-in FEX",
        ));
    }
    Ok(())
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => remove_tree(path),
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// `remove_dir_all`, after making read-only folders writable (a RootFS has
/// some), so their contents can go.
fn remove_tree(path: &Path) -> io::Result<()> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            fs::set_permissions(entry.path(), fs::Permissions::from_mode(0o700))?;
            remove_tree(&entry.path())?;
        } else {
            fs::remove_file(entry.path())?;
        }
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    fs::remove_dir(path)
}

/// The two programs (`usr/bin/FEX`, `usr/bin/FEXServer`) from a `.deb`: an
/// `ar` archive whose `data.tar.zst` holds the package's files.
fn extract_programs(package: &Path, bin: &Path) -> io::Result<()> {
    const WANTED: [&str; 2] = ["usr/bin/FEX", "usr/bin/FEXServer"];
    let data = deb_member(package, "data.tar.zst")?;
    let decoder = zstd::stream::read::Decoder::new(data.as_slice())?;
    let mut archive = tar::Archive::new(decoder);
    let mut found = 0;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        let name = path.strip_prefix(".").unwrap_or(&path);
        let Some(wanted) = WANTED.iter().find(|wanted| name == Path::new(wanted)) else {
            continue;
        };
        if !entry.header().entry_type().is_file() {
            return Err(invalid("a FEX program in the package is not a file"));
        }
        let target = bin.join(Path::new(wanted).file_name().expect("a program name"));
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o755)
            .open(&target)?;
        io::copy(&mut entry, &mut file)?;
        file.sync_all()?;
        found += 1;
    }
    if found != WANTED.len() {
        return Err(invalid("the FEX package lacks FEX or FEXServer"));
    }
    Ok(())
}

/// The bytes of one member of an `ar` archive.
fn deb_member(package: &Path, name: &str) -> io::Result<Vec<u8>> {
    let mut file = BufReader::new(File::open(package)?);
    let mut magic = [0_u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != b"!<arch>\n" {
        return Err(invalid("the FEX package is not a .deb"));
    }
    loop {
        let mut header = [0_u8; 60];
        if let Err(error) = file.read_exact(&mut header) {
            return Err(if error.kind() == io::ErrorKind::UnexpectedEof {
                invalid("the FEX package has no data.tar.zst")
            } else {
                error
            });
        }
        let member = String::from_utf8_lossy(&header[..16]);
        let member = member.trim_end().trim_end_matches('/');
        let size = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse::<u64>()
            .map_err(|_| invalid("the FEX package has a bad member size"))?;
        if member == name {
            let mut bytes = Vec::new();
            file.by_ref().take(size).read_to_end(&mut bytes)?;
            if bytes.len() as u64 != size {
                return Err(invalid("the FEX package ends early"));
            }
            return Ok(bytes);
        }
        // Members are padded to an even length.
        io::copy(&mut file.by_ref().take(size + size % 2), &mut io::sink())?;
    }
}

/// Unpack a SquashFS image into `target`, which must not exist: folders,
/// files, and symbolic links (kept as they are; FEX resolves them inside
/// the RootFS). Device nodes, pipes, and sockets are skipped, and set-user
/// and set-group bits dropped.
fn unpack_squashfs(image: &Path, target: &Path) -> io::Result<()> {
    let reader = BufReader::new(File::open(image)?);
    let filesystem = backhand::FilesystemReader::from_reader(reader)
        .map_err(|error| invalid_owned(format!("the RootFS cannot be read: {error}")))?;
    fs::create_dir(target)?;
    let mut folders = Vec::new();
    for node in filesystem.files() {
        let relative = safe_relative(&node.fullpath)?;
        let path = target.join(&relative);
        let mode = u32::from(node.header.permissions) & 0o1777;
        match &node.inner {
            backhand::InnerNode::Dir(_) => {
                if relative.as_os_str().is_empty() {
                    folders.push((path, mode));
                    continue;
                }
                fs::create_dir(&path)?;
                folders.push((path, mode));
            }
            backhand::InnerNode::File(file) => {
                let mut output = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&path)?;
                let mut input = filesystem.file(file).reader_checked().map_err(|error| {
                    invalid_owned(format!("a RootFS file cannot be read: {error}"))
                })?;
                io::copy(&mut input, &mut output)?;
                output.set_permissions(fs::Permissions::from_mode(mode))?;
            }
            backhand::InnerNode::Symlink(link) => {
                std::os::unix::fs::symlink(&link.link, &path)?;
            }
            _ => {}
        }
    }
    // Folders last, deepest first, so a read-only folder is filled first.
    for (path, mode) in folders.into_iter().rev() {
        fs::set_permissions(&path, fs::Permissions::from_mode(mode | 0o700))?;
    }
    Ok(())
}

/// A SquashFS path made relative, with nothing that could leave the target.
fn safe_relative(path: &Path) -> io::Result<PathBuf> {
    let mut relative = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(part) => relative.push(part),
            Component::ParentDir | Component::Prefix(_) => {
                return Err(invalid("the RootFS names a path outside itself"));
            }
        }
    }
    Ok(relative)
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn invalid_owned(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Snapdragon 8 Gen 2 (Cortex-X3/A715/A510).
    const SNAPDRAGON_8_GEN_2: &str = "Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm jscvt fcma lrcpc dcpop sha3 sm3 sm4 asimddp sha512 asimdfhm dit uscat ilrcpc flagm ssbs sb paca pacg dcpodp flagm2 frint i8mm bf16 dgh bti";
    /// A Cortex-A55 (ARMv8.2).
    const CORTEX_A55: &str = "Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 atomics fphp asimdhp cpuid asimdrdm lrcpc dcpop asimddp";
    /// A Cortex-A53 (ARMv8.0).
    const CORTEX_A53: &str = "Features\t: fp asimd evtstrm aes pmull sha1 sha2 crc32 cpuid";

    fn environment(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn app_provided_fex_needs_an_absolute_entry_point() {
        assert_eq!(AppFex::from_environment(environment(&[])), None);
        assert_eq!(
            AppFex::from_environment(environment(&[("PD2_APP_FEX", "pd2-fex")])),
            None
        );
        let fex = AppFex::from_environment(environment(&[("PD2_APP_FEX", "/u/bin/pd2-fex")]))
            .expect("selected");
        assert_eq!(fex.entry_point(), Path::new("/u/bin/pd2-fex"));
    }

    #[test]
    fn the_fex_config_names_the_thunks_and_one_host_env_entry_per_host_value() {
        let fex = AppFex::from_environment(environment(&[
            ("PD2_APP_FEX", "/u/bin/pd2-fex"),
            ("PD2_APP_FEX_THUNKS_HOST", "/u/lib/fex-emu/HostThunks"),
            ("PD2_APP_FEX_THUNKS_GUEST", "/u/share/fex-emu/GuestThunks"),
            ("PD2_APP_FEX_HOSTENV_VK_DRIVER_FILES", "/u/icd.json"),
            ("PD2_APP_FEX_HOSTENV_MESA_LOADER_DRIVER_OVERRIDE", "zink"),
            ("PD2_APP_FEX_HOSTENV_LIBGL_KOPPER_DRI2", "1"),
            ("PD2_APP_FEX_HOSTENV_MESA_VK_WSI_DEBUG", "sw"),
            ("PD2_APP_FEX_HOSTENV_BAD-NAME", "x"),
            ("PD2_APP_FEX_HOSTENV_BAD_VALUE", "a\nb"),
            // Values the engine has itself never reach FEX's config.
            ("MESA_LOADER_DRIVER_OVERRIDE", "llvmpipe"),
        ]))
        .expect("selected");
        let root = tempfile::tempdir().unwrap();
        fex.write_config(root.path(), false).unwrap();
        let config =
            fs::read_to_string(root.path().join("fex/config/Config.json")).expect("written");
        assert_eq!(
            config,
            r#"{
  "Config": {
    "ThunkHostLibs": "/u/lib/fex-emu/HostThunks",
    "ThunkGuestLibs": "/u/share/fex-emu/GuestThunks",
    "HostEnv": "LIBGL_KOPPER_DRI2=1",
    "HostEnv": "MESA_LOADER_DRIVER_OVERRIDE=zink",
    "HostEnv": "MESA_VK_WSI_DEBUG=sw",
    "HostEnv": "VK_DRIVER_FILES=/u/icd.json"
  },
  "ThunksDB": {
    "GL": 1,
    "Vulkan": 0
  }
}
"#
        );
        for folder in ["data", "cache"] {
            assert!(root.path().join("fex").join(folder).is_dir());
        }
        // Written again unchanged.
        fex.write_config(root.path(), false).unwrap();
        // No staging file is left behind.
        let names = fs::read_dir(root.path().join("fex/config"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Config.json"]);
    }

    #[test]
    fn the_compatibility_driver_adds_its_host_values_over_the_others() {
        let fex = AppFex::from_environment(environment(&[
            ("PD2_APP_FEX", "/u/bin/pd2-fex"),
            ("PD2_APP_FEX_HOSTENV_VK_DRIVER_FILES", "/u/icd.json"),
            ("PD2_APP_FEX_HOSTENV_MESA_VK_WSI_DEBUG", "sw"),
            (
                "PD2_APP_FEX_COMPATIBILITY_HOSTENV_TU_DEBUG",
                "sysmem,noubwc,nolrz",
            ),
            (
                "PD2_APP_FEX_COMPATIBILITY_HOSTENV_MESA_VK_WSI_DEBUG",
                "sw,x",
            ),
            ("PD2_APP_FEX_COMPATIBILITY_HOSTENV_BAD-NAME", "x"),
        ]))
        .expect("selected");
        assert_eq!(
            fex.compatibility_values(),
            ["MESA_VK_WSI_DEBUG=sw,x", "TU_DEBUG=sysmem,noubwc,nolrz"]
        );
        let host_env = |compatibility| {
            fex.config(compatibility)
                .lines()
                .filter(|line| line.contains("HostEnv"))
                .map(|line| line.trim().trim_end_matches(',').to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            host_env(false),
            [
                r#""HostEnv": "MESA_VK_WSI_DEBUG=sw""#,
                r#""HostEnv": "VK_DRIVER_FILES=/u/icd.json""#,
            ]
        );
        assert_eq!(
            host_env(true),
            [
                r#""HostEnv": "MESA_VK_WSI_DEBUG=sw,x""#,
                r#""HostEnv": "TU_DEBUG=sysmem,noubwc,nolrz""#,
                r#""HostEnv": "VK_DRIVER_FILES=/u/icd.json""#,
            ]
        );
        // The next Play with the standard driver writes it without them.
        let root = tempfile::tempdir().unwrap();
        let written = || fs::read_to_string(root.path().join("fex/config/Config.json")).unwrap();
        fex.write_config(root.path(), true).unwrap();
        assert!(written().contains("TU_DEBUG"));
        fex.write_config(root.path(), false).unwrap();
        assert!(!written().contains("TU_DEBUG"));
    }

    #[test]
    fn the_app_provided_rootfs_counts_with_its_record_and_keeps_the_probe_result() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        // An unpacked RootFS without its record is not installed.
        fs::create_dir_all(root.join("fex/rootfs/Ubuntu_24_04/usr")).unwrap();
        assert!(!rootfs_present(root));
        // The record the app's bundled runtime carries.
        fs::write(
            root.join("fex/installed.json"),
            format!(
                r#"{{"schema":1,"fex":"app-provided","rootfs_sha256":"{ROOTFS_SHA256}","fex_probe_passed":false}}"#
            ),
        )
        .unwrap();
        assert!(rootfs_present(root) && !installed(root));
        assert!(!app_probe_passed(root));
        record_app_probe(root, true).unwrap();
        assert!(app_probe_passed(root) && rootfs_present(root));
        record_app_probe(root, false).unwrap();
        assert!(!app_probe_passed(root));
        // Another RootFS's record does not count.
        fs::write(
            root.join("fex/installed.json"),
            r#"{"schema":1,"fex":"app-provided","rootfs_sha256":"00","fex_probe_passed":true}"#,
        )
        .unwrap();
        assert!(!rootfs_present(root) && !app_probe_passed(root));
    }

    #[test]
    fn the_cpu_level_follows_the_features_present() {
        assert_eq!(
            FexVariant::for_features(SNAPDRAGON_8_GEN_2),
            FexVariant::Armv8_4
        );
        assert_eq!(FexVariant::for_features(CORTEX_A55), FexVariant::Armv8_2);
        assert_eq!(FexVariant::for_features(CORTEX_A53), FexVariant::Armv8_0);
        assert_eq!(FexVariant::for_features(""), FexVariant::Armv8_0);
    }

    #[test]
    fn every_variant_is_pinned_to_its_own_package() {
        for variant in FexVariant::ALL {
            assert!(variant.package_url().ends_with("_2609.1-1~j_arm64.deb"));
            assert_eq!(variant.package_sha256().len(), 64);
            assert!(variant.cache_name().ends_with(".verified"));
            assert!(variant.package_url().contains("/fex-emu-armv8."));
        }
    }

    #[test]
    fn guests_get_their_own_folders_and_server() {
        let layout = Layout::new(Path::new("/home/player/.pd2launcher/game"));
        let environment = layout.environment();
        let get = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        assert_eq!(
            get("FEX_ROOTFS").as_deref(),
            Some("/home/player/.pd2launcher/game/fex/rootfs/Ubuntu_24_04")
        );
        assert_eq!(
            get("FEX_APP_DATA_LOCATION").as_deref(),
            Some("/home/player/.pd2launcher/game/fex/data/")
        );
        let socket = get("FEX_SERVERSOCKETPATH").unwrap();
        assert!(socket.starts_with("pd2launcher-") && socket.ends_with(".FEXServer.Socket"));
        assert!(
            !socket.starts_with('/'),
            "an abstract socket name, not a path"
        );
        assert_ne!(
            socket,
            server_socket_name(Path::new("/elsewhere/fex")),
            "each installation its own server"
        );
        assert_eq!(
            layout.interpreter(),
            Path::new("/home/player/.pd2launcher/game/fex/bin/FEX")
        );
    }

    /// A `.deb` with a `data.tar.zst` holding `files`.
    fn deb(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (name, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, name, *contents).unwrap();
        }
        let data = zstd::stream::encode_all(tar.into_inner().unwrap().as_slice(), 3).unwrap();
        let mut deb = b"!<arch>\n".to_vec();
        for (name, contents) in [
            ("debian-binary", b"2.0\n".to_vec()),
            ("control.tar.zst", b"x".to_vec()),
            ("data.tar.zst", data),
        ] {
            deb.extend_from_slice(
                format!(
                    "{name:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    0,
                    0,
                    0,
                    100_644,
                    contents.len()
                )
                .as_bytes(),
            );
            deb.extend_from_slice(&contents);
            if contents.len() % 2 == 1 {
                deb.push(b'\n');
            }
        }
        deb
    }

    #[test]
    fn the_programs_come_out_of_the_package_and_nothing_else() {
        let fixture = tempfile::tempdir().unwrap();
        let package = fixture.path().join("fex.deb");
        fs::write(
            &package,
            deb(&[
                ("./usr/bin/FEX", b"fex"),
                ("./usr/bin/FEXServer", b"server"),
                ("./usr/bin/FEXConfig", b"config"),
            ]),
        )
        .unwrap();
        let bin = fixture.path().join("bin");
        fs::create_dir(&bin).unwrap();
        extract_programs(&package, &bin).unwrap();
        assert_eq!(fs::read(bin.join("FEX")).unwrap(), b"fex");
        assert_eq!(fs::read(bin.join("FEXServer")).unwrap(), b"server");
        assert!(!bin.join("FEXConfig").exists());
        let mode = fs::metadata(bin.join("FEX")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);

        let incomplete = fixture.path().join("incomplete.deb");
        fs::write(&incomplete, deb(&[("./usr/bin/FEX", b"fex")])).unwrap();
        let other = fixture.path().join("other");
        fs::create_dir(&other).unwrap();
        assert!(extract_programs(&incomplete, &other).is_err());
    }

    /// A small SquashFS image: a folder, a file, an absolute link, and a
    /// set-user file.
    fn squashfs() -> Vec<u8> {
        use backhand::{FilesystemWriter, NodeHeader};
        let header = |permissions| NodeHeader::new(permissions, 0, 0, 0);
        let mut writer = FilesystemWriter::default();
        writer.set_compressor(
            backhand::FilesystemCompressor::new(backhand::compression::Compressor::Zstd, None)
                .unwrap(),
        );
        writer.push_dir("usr", header(0o755)).unwrap();
        writer.push_dir("usr/lib", header(0o555)).unwrap();
        writer
            .push_file(
                io::Cursor::new(b"pulse".to_vec()),
                "usr/lib/libpulse.so.0",
                header(0o644),
            )
            .unwrap();
        writer
            .push_file(io::Cursor::new(b"su".to_vec()), "usr/su", header(0o4755))
            .unwrap();
        writer
            .push_symlink("/usr/lib/libpulse.so.0", "libpulse", header(0o777))
            .unwrap();
        let mut image = io::Cursor::new(Vec::new());
        writer.write(&mut image).unwrap();
        image.into_inner()
    }

    #[test]
    fn a_squashfs_image_unpacks_with_its_links_and_without_privileges() {
        let fixture = tempfile::tempdir().unwrap();
        let image = fixture.path().join("rootfs.sqsh");
        fs::write(&image, squashfs()).unwrap();
        let target = fixture.path().join("rootfs");
        unpack_squashfs(&image, &target).unwrap();
        assert_eq!(
            fs::read(target.join("usr/lib/libpulse.so.0")).unwrap(),
            b"pulse"
        );
        assert_eq!(
            fs::read_link(target.join("libpulse")).unwrap(),
            Path::new("/usr/lib/libpulse.so.0"),
            "links stay as the RootFS has them"
        );
        let su = fs::metadata(target.join("usr/su"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(su & 0o7777, 0o755, "no set-user bit");
        remove_tree(&target).unwrap();
        assert!(!target.exists());
    }

    #[test]
    fn an_installation_counts_only_when_complete_and_current() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path();
        let package = root.join("fex.deb");
        fs::write(
            &package,
            deb(&[
                ("./usr/bin/FEX", b"fex"),
                ("./usr/bin/FEXServer", b"server"),
            ]),
        )
        .unwrap();
        let image = root.join("rootfs.sqsh");
        fs::write(&image, squashfs()).unwrap();
        let variant = FexVariant::Armv8_4;
        assert!(!installed_as(root, variant));
        assert_eq!(
            install_with_space(root, variant, &package, &image, u64::MAX)
                .unwrap_err()
                .kind(),
            io::ErrorKind::StorageFull
        );
        install_with_space(root, variant, &package, &image, 0).unwrap();
        assert!(installed_as(root, variant));
        assert!(
            !installed_as(root, FexVariant::Armv8_0),
            "another CPU level"
        );
        assert!(!root.join(STAGING).exists() && !root.join(RETIRED).exists());
        // Installing again replaces it.
        install_with_space(root, variant, &package, &image, 0).unwrap();
        assert!(installed_as(root, variant));
        // A missing program is not an installation.
        fs::remove_file(root.join("fex/bin/FEXServer")).unwrap();
        assert!(!installed_as(root, variant));
    }

    /// The real image: `PD2_TEST_FEX_ROOTFS=<Ubuntu_24_04.sqsh> cargo test
    /// -- --ignored real_rootfs`.
    #[test]
    #[ignore = "needs the downloaded RootFS image"]
    fn the_real_rootfs_unpacks() {
        let image = PathBuf::from(std::env::var_os("PD2_TEST_FEX_ROOTFS").unwrap());
        let fixture = tempfile::tempdir().unwrap();
        let target = fixture.path().join("rootfs");
        unpack_squashfs(&image, &target).unwrap();
        assert!(target.join("usr/lib/i386-linux-gnu/libpulse.so.0").exists());
        assert!(
            target
                .join("usr/lib/i386-linux-gnu/dri/msm_dri.so")
                .exists()
        );
        remove_tree(&target).unwrap();
    }

    #[test]
    fn the_built_in_fex_is_the_default_only_for_a_fresh_installation() {
        use crate::persistence::{FexSource, LauncherSettings};
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("game");
        assert!(chosen(None, &root), "no folder yet");
        fs::create_dir(&root).unwrap();
        assert!(chosen(None, &root), "an empty folder");
        let unchosen = LauncherSettings::default();
        assert!(chosen(Some(&unchosen), &root));
        fs::create_dir(root.join("prefix")).unwrap();
        assert!(
            !chosen(Some(&unchosen), &root),
            "set up with the device's FEX"
        );
        let builtin = LauncherSettings::default().with_fex_source(FexSource::Builtin);
        assert!(chosen(Some(&builtin), &root), "the player's choice wins");
        let device = LauncherSettings::default().with_fex_source(FexSource::Device);
        fs::remove_dir(root.join("prefix")).unwrap();
        assert!(!chosen(Some(&device), &root));
    }

    #[test]
    fn paths_that_leave_the_rootfs_are_refused() {
        assert!(safe_relative(Path::new("/usr/../../etc")).is_err());
        assert_eq!(
            safe_relative(Path::new("/usr/lib")).unwrap(),
            Path::new("usr/lib")
        );
    }
}
