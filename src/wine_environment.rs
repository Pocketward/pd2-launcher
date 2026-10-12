//! The environment a Wine process starts with.
//!
//! The Launcher decides it: a Wine process starts from an empty
//! environment with the Launcher's settings, and keeps from the Launcher's
//! own environment only what describes the session it runs in (display,
//! audio, session bus) and the computer's graphics drivers. Nothing else of
//! the host reaches Wine, so the Launcher's own variables (its paths,
//! download sources, CD keys) never do.

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt as _,
};

/// How a Launcher setting meets what the host passes through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Kind {
    /// Always the Launcher's value.
    Required,
    /// The Launcher's choice (debug channels, DLL overrides, FEX's strict
    /// settings); like `Required`, since no host value reaches it.
    Preference,
    /// Used only where the passed-through environment has none.
    Default,
}

/// One environment variable the Launcher wants a Wine process to have.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Setting {
    pub(crate) name: OsString,
    pub(crate) value: OsString,
    pub(crate) kind: Kind,
}

impl Setting {
    pub(crate) fn new(name: impl Into<OsString>, value: impl Into<OsString>, kind: Kind) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
            kind,
        }
    }
}

/// The session a Wine program runs in: its display, audio, and session bus.
const SESSION: [&str; 7] = [
    "XDG_RUNTIME_DIR",
    "WAYLAND_DISPLAY",
    "DISPLAY",
    "XAUTHORITY",
    "DBUS_SESSION_BUS_ADDRESS",
    "PULSE_SERVER",
    "PIPEWIRE_REMOTE",
];

/// Prefixes of the variables describing the computer's graphics drivers:
/// which Mesa driver to load (Zink on DroidDeck), where the Vulkan driver is
/// (Turnip), and their options. Without them the game may find no GPU
/// driver. NVIDIA's (`__NV_`, `__VK_LAYER_NV_`, `__GL_`) and Mesa's
/// `DRI_PRIME` come with them, so a laptop started with `prime-run` (or
/// `DRI_PRIME=1`) draws on the same GPU throughout.
const GRAPHICS_PREFIXES: [&str; 11] = [
    "MESA_",
    "GALLIUM_",
    "ZINK_",
    "TU_",
    "VK_",
    "LIBGL_",
    "__GLX_",
    "__EGL_",
    "__NV_",
    "__VK_LAYER_NV_",
    "__GL_",
];

/// Other system settings Wine keeps: glibc's tunables (DroidDeck tunes them
/// for proot), gamescope's Vulkan layer switch, and Mesa's PRIME choice.
const SYSTEM: [&str; 3] = ["GLIBC_TUNABLES", "DISABLE_GAMESCOPE_WSI", "DRI_PRIME"];

/// What the Android app's guests also keep: the gamepad file of the XInput
/// bridge. PulseAudio's `PULSE_SERVER` is in [`SESSION`].
const ANDROID: [&str; 1] = ["PD2_GAMEPAD_FILE"];

/// Added to the Launcher's DLL overrides on the Android host: the XInput
/// bridge (`tools/xinput-bridge.c`) installed as `xinput1_4.dll`, and no
/// `nsiproxy.sys`. Android's platform policy denies apps the NETLINK_ROUTE
/// `bind` and `RTM_GETLINK` (a neverallow for every `untrusted_app` domain;
/// only a permissive device allows them), so glibc's `if_nameindex()` fails.
/// Wine's nsiproxy dereferences that NULL while holding its interface lock,
/// and every later network query of the prefix waits on the lock forever:
/// the game's local IP lookup at start, and Blizzard's downloader before it
/// opens its window. Without the driver those queries fail at once and Wine
/// falls back to the host's resolver. The driver runs in `winedevice.exe`,
/// which the first Wine process of a prefix starts with its own environment,
/// so every Wine process carries the override.
const ANDROID_DLL_OVERRIDES: &str = ";xinput1_4=n;nsiproxy.sys=d";

/// Set on the Android host: Wine's esync, so waits on events, mutexes and
/// semaphores are eventfd reads instead of wineserver round trips (about
/// half of the game's wineserver requests). Its shared memory is the
/// RootFS's `/dev/shm`, where the app's FEX creates the file. Every Wine
/// process of a prefix needs the same value: a client with esync refuses a
/// wineserver without it.
const ANDROID_ESYNC: (&str, &str) = ("WINEESYNC", "1");

/// Set by the AppImage's entry point when it, not the host, set
/// `DISABLE_GAMESCOPE_WSI` (for the Launcher's own window).
const LAUNCHER_SET_GAMESCOPE_WSI: &str = "PD2_LAUNCHER_SET_DISABLE_GAMESCOPE_WSI";

/// Whether the host's `name` reaches Wine.
fn passes_through(name: &OsStr, value: &OsStr, android: bool) -> bool {
    let name = name.as_bytes();
    let known = SESSION.iter().any(|known| name == known.as_bytes())
        || (android && ANDROID.iter().any(|known| name == known.as_bytes()))
        || SYSTEM.iter().any(|known| name == known.as_bytes())
        || GRAPHICS_PREFIXES
            .iter()
            .any(|prefix| name.starts_with(prefix.as_bytes()));
    let value = value.as_bytes();
    known
        && !value.is_empty()
        && value.len() <= 4096
        && !value.iter().any(|byte| *byte < b' ' || *byte == 0x7f)
}

/// The environment for a Wine process: what passes through from `host`,
/// with `settings` applied as their kind says.
pub(crate) fn compose(
    host: impl IntoIterator<Item = (OsString, OsString)>,
    settings: &[Setting],
) -> Vec<(OsString, OsString)> {
    compose_on(host, settings, crate::android_host())
}

fn compose_on(
    host: impl IntoIterator<Item = (OsString, OsString)>,
    settings: &[Setting],
    android: bool,
) -> Vec<(OsString, OsString)> {
    let host = host.into_iter().collect::<BTreeMap<_, _>>();
    let launcher_set_wsi = host.contains_key(OsStr::new(LAUNCHER_SET_GAMESCOPE_WSI));
    let mut environment = host
        .into_iter()
        .filter(|(name, value)| passes_through(name, value, android))
        .collect::<BTreeMap<_, _>>();
    // The AppImage turns gamescope's Vulkan layer off for the Launcher's own
    // window; the game keeps whatever the host itself chose.
    if launcher_set_wsi {
        environment.remove(OsStr::new("DISABLE_GAMESCOPE_WSI"));
    }
    for setting in settings {
        if setting.kind == Kind::Default && environment.contains_key(&setting.name) {
            continue;
        }
        environment.insert(setting.name.clone(), setting.value.clone());
    }
    if android {
        let (name, value) = ANDROID_ESYNC;
        environment.insert(OsString::from(name), OsString::from(value));
    }
    // The game's XInput is the Launcher's bridge in the prefix, never Wine's.
    if android && let Some(overrides) = environment.get_mut(OsStr::new("WINEDLLOVERRIDES")) {
        overrides.push(ANDROID_DLL_OVERRIDES);
    }
    environment.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn get<'a>(environment: &'a [(OsString, OsString)], name: &str) -> Option<&'a str> {
        environment
            .iter()
            .find(|(key, _)| key == name)
            .and_then(|(_, value)| value.to_str())
    }

    #[test]
    fn only_the_session_and_graphics_drivers_pass_and_the_launcher_decides_the_rest() {
        let settings = [
            Setting::new("WINEPREFIX", "/launcher/prefix", Kind::Required),
            Setting::new("WINEDEBUG", "fixme-all", Kind::Preference),
            Setting::new("LANG", "C.UTF-8", Kind::Default),
            Setting::new("PATH", "/usr/bin", Kind::Default),
        ];
        let environment = compose(
            host(&[
                ("DISPLAY", ":0"),
                ("GALLIUM_DRIVER", "zink"),
                ("VK_ICD_FILENAMES", "/icd.json"),
                ("GLIBC_TUNABLES", "glibc.pthread.rseq=0"),
                ("WINEPREFIX", "/somewhere/else"),
                ("WINEDEBUG", "+seh"),
                ("LANG", "en_US.UTF-8"),
                ("PATH", "/home/player/bin"),
                ("PD2_LAUNCHER_HOME", "/private"),
                ("D2_KEY", "secret"),
                ("DOTNET_ROOT", "/usr/share/dotnet"),
                ("MESA_SHADER_CACHE_DIR", "bad\nvalue"),
            ]),
            &settings,
        );
        assert_eq!(get(&environment, "DISPLAY"), Some(":0"));
        assert_eq!(get(&environment, "GALLIUM_DRIVER"), Some("zink"));
        assert_eq!(get(&environment, "VK_ICD_FILENAMES"), Some("/icd.json"));
        assert_eq!(
            get(&environment, "GLIBC_TUNABLES"),
            Some("glibc.pthread.rseq=0")
        );
        assert_eq!(get(&environment, "WINEPREFIX"), Some("/launcher/prefix"));
        assert_eq!(get(&environment, "WINEDEBUG"), Some("fixme-all"));
        assert_eq!(get(&environment, "LANG"), Some("C.UTF-8"));
        assert_eq!(get(&environment, "PATH"), Some("/usr/bin"));
        assert_eq!(get(&environment, "PD2_LAUNCHER_HOME"), None);
        assert_eq!(get(&environment, "D2_KEY"), None);
        assert_eq!(get(&environment, "DOTNET_ROOT"), None);
        assert_eq!(get(&environment, "MESA_SHADER_CACHE_DIR"), None);
    }

    #[test]
    fn the_android_host_also_passes_the_gamepad_file_and_pulseaudio() {
        let app = host(&[
            ("PD2_GAMEPAD_FILE", "/data/gamepad.bin"),
            ("PULSE_SERVER", "unix:/data/run/pulse/native"),
            ("PD2_APP_FEX", "/data/pd2-fex"),
            ("PD2_APP_FEX_HOSTENV_VK_DRIVER_FILES", "/data/icd.json"),
        ]);
        let android = compose_on(app.clone(), &[], true);
        assert_eq!(get(&android, "PD2_GAMEPAD_FILE"), Some("/data/gamepad.bin"));
        assert_eq!(
            get(&android, "PULSE_SERVER"),
            Some("unix:/data/run/pulse/native")
        );
        assert_eq!(get(&android, "PD2_APP_FEX"), None);
        assert_eq!(get(&android, "PD2_APP_FEX_HOSTENV_VK_DRIVER_FILES"), None);
        let desktop = compose_on(app, &[], false);
        assert_eq!(get(&desktop, "PD2_GAMEPAD_FILE"), None);
        assert_eq!(
            get(&desktop, "PULSE_SERVER"),
            Some("unix:/data/run/pulse/native")
        );
    }

    #[test]
    fn only_the_android_host_loads_the_xinput_bridge_and_no_nsiproxy() {
        let settings = [Setting::new(
            "WINEDLLOVERRIDES",
            "winemenubuilder.exe=;ddraw=n",
            Kind::Preference,
        )];
        let host = host(&[("WINEDLLOVERRIDES", "xinput1_4=b")]);
        let android = compose_on(host.clone(), &settings, true);
        assert_eq!(
            get(&android, "WINEDLLOVERRIDES"),
            Some("winemenubuilder.exe=;ddraw=n;xinput1_4=n;nsiproxy.sys=d")
        );
        // The installers' and Blizzard's downloader's too: the first Wine
        // process of a prefix starts winedevice.exe with its environment.
        let install = [Setting::new(
            "WINEDLLOVERRIDES",
            "winemenubuilder.exe=;dsound=;winedbg.exe=d",
            Kind::Required,
        )];
        assert_eq!(
            get(&compose_on(Vec::new(), &install, true), "WINEDLLOVERRIDES"),
            Some("winemenubuilder.exe=;dsound=;winedbg.exe=d;xinput1_4=n;nsiproxy.sys=d")
        );
        assert_eq!(
            get(&compose_on(Vec::new(), &install, false), "WINEDLLOVERRIDES"),
            Some("winemenubuilder.exe=;dsound=;winedbg.exe=d")
        );
        let desktop = compose_on(host, &settings, false);
        assert_eq!(
            get(&desktop, "WINEDLLOVERRIDES"),
            Some("winemenubuilder.exe=;ddraw=n")
        );
        // Without the Launcher's overrides there is nothing to add to.
        assert_eq!(
            get(&compose_on(Vec::new(), &[], true), "WINEDLLOVERRIDES"),
            None
        );
    }

    #[test]
    fn only_the_android_host_turns_on_esync() {
        let host = host(&[("WINEESYNC", "0")]);
        assert_eq!(
            get(&compose_on(host.clone(), &[], true), "WINEESYNC"),
            Some("1")
        );
        assert_eq!(get(&compose_on(host, &[], false), "WINEESYNC"), None);
    }

    #[test]
    fn prime_offload_reaches_wine_whole() {
        let prime_run = [
            ("__NV_PRIME_RENDER_OFFLOAD", "1"),
            ("__VK_LAYER_NV_optimus", "NVIDIA_only"),
            ("__GLX_VENDOR_LIBRARY_NAME", "nvidia"),
            ("__GL_SYNC_TO_VBLANK", "0"),
            ("DRI_PRIME", "1"),
        ];
        let environment = compose(host(&prime_run), &[]);
        for (name, value) in prime_run {
            assert_eq!(get(&environment, name), Some(value), "{name}");
        }
    }

    #[test]
    fn gamescopes_vulkan_layer_stays_off_only_where_the_host_turned_it_off() {
        let set_by_launcher = compose(
            host(&[
                ("DISABLE_GAMESCOPE_WSI", "1"),
                ("PD2_LAUNCHER_SET_DISABLE_GAMESCOPE_WSI", "1"),
            ]),
            &[],
        );
        assert_eq!(get(&set_by_launcher, "DISABLE_GAMESCOPE_WSI"), None);
        let set_by_host = compose(host(&[("DISABLE_GAMESCOPE_WSI", "1")]), &[]);
        assert_eq!(get(&set_by_host, "DISABLE_GAMESCOPE_WSI"), Some("1"));
    }
}
