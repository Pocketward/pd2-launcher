//! The managed Wine runtime each compile-target row provisions and executes.
//!
//! The AArch64 row uses the FEX-translated Wine-GE 8-26 runtime. The x86-64
//! row uses an upstream Wine 11.11 build in WoW64 mode: 32-bit Windows code
//! runs inside 64-bit Unix processes, so the host needs no i386 loader,
//! 32-bit multilib, or `libunwind`. An installation an older release made
//! with Wine 11.18 keeps that runtime until Start over.

/// Exact size and SHA-256 of one executable inside the extracted runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RuntimeFile {
    pub(crate) size: u64,
    pub(crate) sha256: &'static str,
}

/// Frozen identity and execution shape of one managed Wine runtime.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ManagedRuntime {
    /// Runner directory name, equal to the archive's single top-level entry.
    pub(crate) identity: &'static str,
    /// Player-facing runtime name.
    pub(crate) display_name: &'static str,
    /// Verified archive name in the Launcher's artifact cache.
    pub(crate) cache_name: &'static str,
    pub(crate) archive_url: &'static str,
    pub(crate) archive_size: u64,
    pub(crate) archive_sha512: &'static str,
    pub(crate) wine: RuntimeFile,
    /// The ELF the Launcher executes (`bin/<loader_name>`): Wine-GE's
    /// `wine-preloader`, or `wine` itself for builds without a preloader.
    pub(crate) loader_name: &'static str,
    pub(crate) loader: RuntimeFile,
    /// Wine's preloader takes the `wine` loader path as its first argument;
    /// a runtime executed through `wine` itself does not.
    pub(crate) loader_takes_wine_path: bool,
    /// Exact `wine --version` output accepted by the runtime preflight.
    pub(crate) version: &'static str,
    /// Windows version written to a new prefix, when the runtime's default
    /// is not one the 1.14d client accepts. Recent Wine reports Windows 10,
    /// whose critical-section debug layout Fog.dll rejects at startup.
    pub(crate) windows_version: Option<&'static str>,
}

/// The artifact-cache name older releases used for every row. An x86-64
/// cache may hold the WoW64 archive under it.
pub(crate) const LEGACY_CACHE_NAME: &str = "wine-ge-8-26.verified";

pub(crate) const WINE_GE_8_26: ManagedRuntime = ManagedRuntime {
    identity: "lutris-GE-Proton8-26-x86_64",
    display_name: "Wine-GE 8-26",
    cache_name: LEGACY_CACHE_NAME,
    archive_url: "https://github.com/GloriousEggroll/wine-ge-custom/releases/download/GE-Proton8-26/wine-lutris-GE-Proton8-26-x86_64.tar.xz",
    archive_size: 233_577_252,
    archive_sha512: "3abd578ed45a16096843936e0258799450e6a5751bc67afaf8c58cd245c40e62794a89af25ae482d74d5ca2b7527d17ff77232781c6d9cf97bc02976c2f960bd",
    wine: RuntimeFile {
        size: 13_788,
        sha256: "d66993551779e46fd982953f3b6b1572a81a2c7341421314cc7dd9ded345dcbc",
    },
    loader_name: "wine-preloader",
    loader: RuntimeFile {
        size: 17_004,
        sha256: "6f6b47455492a697ecd264bfbe960a5331b0fa28cb57530ce095fcde8f022a4e",
    },
    loader_takes_wine_path: true,
    version: "wine-8.0-3001-g39021e609a2 (Staging)",
    windows_version: None,
};

/// Kron4ek Wine-Builds 11.11 `amd64-wow64` (upstream release SHA-256
/// `529d443301c7c798cbdee80769e855f47848d02c9b325d572d3f91988603e226`).
/// Wine 11.12 and later need `wl_display_create_queue_with_name`
/// (libwayland-client 1.23) for the Wayland driver, which Ubuntu 24.04 and
/// Debian 12 lack.
pub(crate) const WINE_WOW64_11_11: ManagedRuntime = ManagedRuntime {
    identity: "wine-11.11-amd64-wow64",
    display_name: "Wine 11.11",
    cache_name: "wine-11.11-amd64-wow64.verified",
    archive_url: "https://github.com/Kron4ek/Wine-Builds/releases/download/11.11/wine-11.11-amd64-wow64.tar.xz",
    archive_size: 95_429_744,
    archive_sha512: "022af24738636a5513d73771abd83115baeb6f93dd25ee6fa690aa572a2db433ca0cfd69f521c0941c93fdffb68c13d82d9db89a2805da5dd113347de75bf469",
    wine: RuntimeFile {
        size: 13_424,
        sha256: "302823189c8d93a597a3d7a6dd7bf710fafac1be1c665fc57666143476bdfe4e",
    },
    loader_name: "wine",
    loader: RuntimeFile {
        size: 13_424,
        sha256: "302823189c8d93a597a3d7a6dd7bf710fafac1be1c665fc57666143476bdfe4e",
    },
    loader_takes_wine_path: false,
    version: "wine-11.11",
    windows_version: Some("win7"),
};

/// Kron4ek Wine-Builds 11.18 `amd64-wow64` (upstream release SHA-256
/// `f899879b8c37e0b20adca19d147cf77436f3f1a37bf16d08d27fa7137a52b9ba`), the
/// x86-64 row's previous runtime. It is never downloaded; an installation
/// whose prefix it made keeps running it (see [`retained`]).
pub(crate) const WINE_WOW64_11_18: ManagedRuntime = ManagedRuntime {
    identity: "wine-11.18-amd64-wow64",
    display_name: "Wine 11.18",
    cache_name: "wine-11.18-amd64-wow64.verified",
    archive_url: "https://github.com/Kron4ek/Wine-Builds/releases/download/11.18/wine-11.18-amd64-wow64.tar.xz",
    archive_size: 99_305_644,
    archive_sha512: "81673789c42860626396f77506ab31eb6a74fa5527d3de26fd7da198f014817c494bcafca968de953fee724a17f5012825408ef238b52df2b64017f541406899",
    wine: RuntimeFile {
        size: 13_424,
        sha256: "40d5d0f45702a66d1837f3cf44673bfd970901ea52d06aaa1ba39a3a478d6a79",
    },
    loader_name: "wine",
    loader: RuntimeFile {
        size: 13_424,
        sha256: "40d5d0f45702a66d1837f3cf44673bfd970901ea52d06aaa1ba39a3a478d6a79",
    },
    loader_takes_wine_path: false,
    version: "wine-11.18",
    windows_version: Some("win7"),
};

/// Runtime of the row for `architecture` (a `std::env::consts::ARCH` value).
pub(crate) fn for_architecture(architecture: &str) -> &'static ManagedRuntime {
    match architecture {
        "x86_64" => &WINE_WOW64_11_11,
        _ => &WINE_GE_8_26,
    }
}

/// The runtime the row for `architecture` pinned before its current one,
/// kept for installations made with it.
pub(crate) fn retained_for_architecture(architecture: &str) -> Option<&'static ManagedRuntime> {
    match architecture {
        "x86_64" => Some(&WINE_WOW64_11_18),
        _ => None,
    }
}

/// Earlier pinned runtime of the running Launcher binary's row. A prefix
/// that runtime created stays on it: running an older Wine on a newer
/// Wine's prefix is a downgrade Wine does not support. It is selected only
/// while the current runtime is not installed beside it, and Start over
/// removes it with the prefix.
pub(crate) fn retained() -> Option<&'static ManagedRuntime> {
    retained_for_architecture(std::env::consts::ARCH)
}

/// Runtime of the running Launcher binary's compile target.
pub(crate) fn current() -> &'static ManagedRuntime {
    for_architecture(std::env::consts::ARCH)
}

/// Receipt the Launcher writes last inside a pinned runtime directory.
pub(crate) const BUILTIN_RUNTIME_RECEIPT: &str = ".pd2launcher-archive.sha512";
/// Runner directory of a runtime materialized from a user-supplied custom
/// archive. The archive's own single top-level directory is
/// stripped, so any build layout with `bin/wine` works under one fixed name.
pub(crate) const CUSTOM_RUNTIME_DIRECTORY: &str = "custom-wine";
/// Custom-source provenance receipt written last inside the custom runtime.
pub(crate) const CUSTOM_RUNTIME_RECEIPT: &str = ".pd2launcher-custom-source.json";
/// Whether `runner/custom-wine` carries a custom-source receipt.
pub(crate) fn custom_receipt_present(root: &crate::safe_fs::Directory) -> bool {
    root.open_beneath(
        &std::path::Path::new("runner")
            .join(CUSTOM_RUNTIME_DIRECTORY)
            .join(CUSTOM_RUNTIME_RECEIPT),
        rustix::fs::OFlags::PATH,
    )
    .is_ok()
}

/// Whether `runner/<identity>` of the pinned `runtime` carries its receipt.
fn builtin_receipt_present(root: &crate::safe_fs::Directory, runtime: &ManagedRuntime) -> bool {
    root.open_beneath(
        &std::path::Path::new("runner")
            .join(runtime.identity)
            .join(BUILTIN_RUNTIME_RECEIPT),
        rustix::fs::OFlags::PATH,
    )
    .is_ok()
}

/// Windows version written to a prefix created by a custom runtime. Recent
/// Wine reports Windows 10, whose layout the 1.14d Fog.dll rejects.
const CUSTOM_WINDOWS_VERSION: &str = "win7";

/// Runner directory of the Android app's runtime: an ARM64 Wine 11.0 in
/// WoW64 mode, whose 32-bit x86 code Box64's `wowbox64.dll` runs. The app
/// unpacks it there from its APK (made for its own data folder, so it has no
/// pinned digest) and replaces it with each new app version; the Launcher
/// never downloads it.
pub(crate) const APP_RUNTIME_DIRECTORY: &str = "wine-11.0-arm64";
/// Player-facing name of the Android app's runtime.
pub(crate) const APP_RUNTIME_DISPLAY_NAME: &str = "Wine 11.0";
/// Exact `wine --version` output of the Android app's runtime.
pub(crate) const APP_RUNTIME_VERSION: &str = "wine-11.0";
/// The prefix the Android app's runtime starts from, in the managed root: a
/// copy of it becomes a new prefix (the app makes it with the same Wine, so
/// no `wineboot` runs on the phone). Game.exe already runs as Windows 7 in it.
pub(crate) const APP_PREFIX_TEMPLATE: &str = "runner/wine-11.0-arm64-prefix";
/// The link a prefix of the Android app's runtime reaches the app's x86
/// .NET runtime (the official PD2 launcher's) by, and its target, relative
/// to the link's folder: `runner/dotnet-6.0.36-x86` in the managed root.
pub(crate) const APP_DOTNET_LINK: &str = "prefix/drive_c/Program Files (x86)/dotnet";
pub(crate) const APP_DOTNET_LINK_TARGET: &str = "../../../runner/dotnet-6.0.36-x86";

/// The runtime an installation actually uses: the row's pinned runtime, or a
/// runtime installed from a custom source. A custom runtime is
/// used only while the Wine download source is custom; with the built-in
/// source configured, only the pinned runtime is ever selected, audited, or
/// executed, whatever custom receipt may exist beside it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstalledRuntime {
    BuiltIn(&'static ManagedRuntime),
    Custom,
    /// The Android app's runtime ([`APP_RUNTIME_DIRECTORY`]), the only one
    /// an Android host runs.
    App,
}

impl InstalledRuntime {
    /// Select the runtime for an installation whose Wine source is currently
    /// custom (`custom_source`) or built-in.
    pub(crate) fn select(root: &crate::safe_fs::Directory, custom_source: bool) -> Self {
        Self::select_with(root, custom_source, current(), retained())
    }

    /// With a custom Wine source, a runtime carrying a custom-source receipt
    /// is used; otherwise (no custom runtime yet, or the built-in source) a
    /// pinned runtime is used and audited against its pins: the `retained`
    /// earlier one while it carries its receipt beside a prefix and
    /// `builtin` carries none, else `builtin`.
    pub(crate) fn select_with(
        root: &crate::safe_fs::Directory,
        custom_source: bool,
        builtin: &'static ManagedRuntime,
        retained: Option<&'static ManagedRuntime>,
    ) -> Self {
        if crate::android_host() {
            return Self::App;
        }
        if custom_source && custom_receipt_present(root) {
            return Self::Custom;
        }
        match retained {
            Some(retained)
                if !builtin_receipt_present(root, builtin)
                    && builtin_receipt_present(root, retained)
                    && root
                        .open_beneath(std::path::Path::new("prefix"), rustix::fs::OFlags::PATH)
                        .is_ok() =>
            {
                Self::BuiltIn(retained)
            }
            _ => Self::BuiltIn(builtin),
        }
    }

    pub(crate) fn is_custom(self) -> bool {
        self == Self::Custom
    }

    /// Directory below `runner/`.
    pub(crate) fn directory(self) -> &'static str {
        match self {
            Self::BuiltIn(runtime) => runtime.identity,
            Self::Custom => CUSTOM_RUNTIME_DIRECTORY,
            Self::App => APP_RUNTIME_DIRECTORY,
        }
    }

    /// ELF the Launcher executes from `bin/`. Any Wine build can be started
    /// through `bin/wine` itself, so a custom runtime never needs a preloader.
    pub(crate) fn loader_name(self) -> &'static str {
        match self {
            Self::BuiltIn(runtime) => runtime.loader_name,
            Self::Custom | Self::App => "wine",
        }
    }

    pub(crate) fn loader_takes_wine_path(self) -> bool {
        match self {
            Self::BuiltIn(runtime) => runtime.loader_takes_wine_path,
            Self::Custom | Self::App => false,
        }
    }

    pub(crate) fn windows_version(self) -> Option<&'static str> {
        match self {
            Self::BuiltIn(runtime) => runtime.windows_version,
            Self::Custom => Some(CUSTOM_WINDOWS_VERSION),
            // Its prefix template sets Game.exe's own version.
            Self::App => None,
        }
    }

    /// Player-facing name of this runtime.
    pub(crate) fn display_name(self) -> &'static str {
        match self {
            Self::BuiltIn(runtime) => runtime.display_name,
            Self::Custom => current().display_name,
            Self::App => APP_RUNTIME_DISPLAY_NAME,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_row_selects_its_frozen_runtime() {
        assert_eq!(for_architecture("x86_64"), &WINE_WOW64_11_11);
        assert_eq!(for_architecture("aarch64"), &WINE_GE_8_26);
        for runtime in [&WINE_WOW64_11_11, &WINE_WOW64_11_18] {
            assert!(
                runtime
                    .archive_url
                    .ends_with(&format!("/{}.tar.xz", runtime.identity))
            );
            assert_eq!(runtime.cache_name, format!("{}.verified", runtime.identity));
        }
        assert_eq!(retained_for_architecture("x86_64"), Some(&WINE_WOW64_11_18));
        assert_eq!(retained_for_architecture("aarch64"), None);
        for runtime in [&WINE_GE_8_26, &WINE_WOW64_11_11, &WINE_WOW64_11_18] {
            assert_eq!(runtime.archive_sha512.len(), 128);
            assert_eq!(runtime.wine.sha256.len(), 64);
            assert_eq!(runtime.loader.sha256.len(), 64);
        }
    }

    #[test]
    fn each_row_names_and_caches_its_own_runtime() {
        assert_eq!(for_architecture("x86_64").display_name, "Wine 11.11");
        assert_eq!(for_architecture("x86_64").version, "wine-11.11");
        assert_eq!(for_architecture("aarch64").display_name, "Wine-GE 8-26");
        assert_eq!(
            for_architecture("x86_64").cache_name,
            "wine-11.11-amd64-wow64.verified"
        );
        // The AArch64 row keeps the shared cache name.
        assert_eq!(for_architecture("aarch64").cache_name, LEGACY_CACHE_NAME);
        assert_eq!(LEGACY_CACHE_NAME, "wine-ge-8-26.verified");
    }

    #[test]
    fn a_custom_runtime_is_selected_only_while_the_wine_source_is_custom() {
        let fixture = tempfile::tempdir().unwrap();
        let root = crate::safe_fs::Directory::open(fixture.path()).unwrap();
        let builtin = current();
        for custom_source in [false, true] {
            assert_eq!(
                InstalledRuntime::select(&root, custom_source),
                InstalledRuntime::BuiltIn(builtin)
            );
        }

        let custom = fixture.path().join("runner").join(CUSTOM_RUNTIME_DIRECTORY);
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join(CUSTOM_RUNTIME_RECEIPT), b"{}").unwrap();
        let detected = InstalledRuntime::select(&root, true);
        assert_eq!(detected, InstalledRuntime::Custom);
        assert_eq!(detected.directory(), "custom-wine");
        assert_eq!(detected.loader_name(), "wine");
        assert!(!detected.loader_takes_wine_path());
        assert_eq!(detected.windows_version(), Some("win7"));
        // With the built-in source configured, a custom receipt is ignored.
        assert_eq!(
            InstalledRuntime::select(&root, false),
            InstalledRuntime::BuiltIn(builtin)
        );

        // A custom source keeps using its custom runtime beside a pinned one.
        let pinned = fixture.path().join("runner").join(builtin.identity);
        std::fs::create_dir_all(&pinned).unwrap();
        std::fs::write(pinned.join(BUILTIN_RUNTIME_RECEIPT), b"tampered").unwrap();
        assert_eq!(
            InstalledRuntime::select(&root, true),
            InstalledRuntime::Custom
        );
        assert_eq!(
            InstalledRuntime::select(&root, false),
            InstalledRuntime::BuiltIn(builtin)
        );
    }

    #[test]
    fn a_runtime_without_a_preloader_executes_wine_directly() {
        for runtime in [&WINE_WOW64_11_11, &WINE_WOW64_11_18] {
            assert_eq!(runtime.loader_name, "wine");
            assert_eq!(runtime.loader, runtime.wine);
            assert!(!runtime.loader_takes_wine_path);
            assert_eq!(runtime.windows_version, Some("win7"));
        }
        const { assert!(WINE_GE_8_26.loader_takes_wine_path) };
    }

    #[test]
    fn a_prefix_made_by_the_retained_runtime_stays_on_it_until_the_current_one_is_installed() {
        let fixture = tempfile::tempdir().unwrap();
        let root = crate::safe_fs::Directory::open(fixture.path()).unwrap();
        let select = |custom_source| {
            InstalledRuntime::select_with(
                &root,
                custom_source,
                &WINE_WOW64_11_11,
                Some(&WINE_WOW64_11_18),
            )
        };
        let receipt = |runtime: &ManagedRuntime| {
            let directory = fixture.path().join("runner").join(runtime.identity);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(BUILTIN_RUNTIME_RECEIPT),
                runtime.archive_sha512,
            )
            .unwrap();
        };
        let current = InstalledRuntime::BuiltIn(&WINE_WOW64_11_11);
        let earlier = InstalledRuntime::BuiltIn(&WINE_WOW64_11_18);

        // An earlier runtime without a prefix (never installed, or after
        // Start over) is not used: installing materializes the current one.
        receipt(&WINE_WOW64_11_18);
        assert_eq!(select(false), current);
        std::fs::create_dir(fixture.path().join("prefix")).unwrap();
        assert_eq!(select(false), earlier);
        assert_eq!(select(true), earlier);
        assert_eq!(earlier.directory(), "wine-11.18-amd64-wow64");
        assert_eq!(earlier.loader_name(), "wine");
        assert_eq!(earlier.windows_version(), Some("win7"));
        // A custom runtime still wins while the Wine source is custom.
        let custom = fixture.path().join("runner").join(CUSTOM_RUNTIME_DIRECTORY);
        std::fs::create_dir_all(&custom).unwrap();
        std::fs::write(custom.join(CUSTOM_RUNTIME_RECEIPT), b"{}").unwrap();
        assert_eq!(select(true), InstalledRuntime::Custom);
        assert_eq!(select(false), earlier);
        // Once the current runtime is installed it is always the one used.
        receipt(&WINE_WOW64_11_11);
        assert_eq!(select(false), current);
        // A row without an earlier runtime never selects one.
        assert_eq!(
            InstalledRuntime::select_with(&root, false, &WINE_GE_8_26, None),
            InstalledRuntime::BuiltIn(&WINE_GE_8_26)
        );
    }
}
