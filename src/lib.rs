//! PD2 Launcher modular-monolith interface.

#![forbid(unsafe_code)]

mod activity;
mod app_menu;
pub mod audit;
mod automation;
mod builtin_fex;
mod composition;
mod control_channel;
mod coordinator;
mod dev_build;
mod diagnostics;
mod domain;
mod downloads;
mod game_folder;
mod in_place_drive;
mod install;
mod location;
mod manifest;
mod migration;
mod network;
mod operation;
mod orphaned_wine;
mod persistence;
mod presentation;
mod process;
mod redact;
#[cfg(test)]
mod release_verification;
mod relocation;
mod report;
mod runtime;
mod safe_fs;
mod save_keeper;
mod sources;
mod stage_marker;
mod startup_credentials;
mod startup_failure;
mod transaction;
mod update;
mod wine_environment;

pub mod host_execution;

/// Set once by the Launcher engine on the Android host, before its first
/// lookup; never on the desktop.
static ANDROID_HOST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The Launcher engine runs on the Android host: [`safe_fs`] avoids
/// `openat2` and opens `home`'s ancestors search-only, Wine's guests also
/// keep the Android values and load the XInput bridge, and a missing D2GL
/// configuration is written before the first Glide launch.
pub(crate) fn use_android_host(home: Option<&std::path::Path>) {
    safe_fs::use_android_host(home);
    ANDROID_HOST.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the Launcher engine runs on the Android host.
pub(crate) fn android_host() -> bool {
    ANDROID_HOST.load(std::sync::atomic::Ordering::Relaxed)
}

pub use domain::{
    Activity, InstallationStatus, InterruptedStage, InvalidRelativeGamePath, LaunchPreflight,
    LauncherSnapshot, RelativeGamePath, UpdateStatus,
};

pub use automation::ArgumentError;
pub use composition::{package_identity_probe, run};

/// Process exit status for the finished run: nonzero when the window could
/// not open, when unattended mode stopped without finishing, or when the
/// Launcher engine ended on a Control channel failure.
pub fn exit_status() -> u8 {
    automation::unattended_exit_status()
}

/// Parse launcher arguments and start either the interactive or explicitly
/// authorized unattended workflow.
pub fn run_with_arguments(
    arguments: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<(), ArgumentError> {
    match automation::parse_arguments(arguments)? {
        automation::ArgumentMode::Interactive => {
            startup_failure::install_panic_hook();
            composition::run();
        }
        automation::ArgumentMode::SaveReport => {
            automation::request_report();
            startup_failure::install_panic_hook();
            composition::run();
        }
        automation::ArgumentMode::Help => println!("{}", automation::USAGE),
        automation::ArgumentMode::Unattended(request) => {
            startup_failure::install_panic_hook();
            composition::run_with_automation(request);
        }
        // No desktop: a failure goes to standard error and the Control
        // channel, never to a dialog.
        automation::ArgumentMode::Engine => {
            automation::set_unattended_exit_status(composition::run_engine());
        }
        automation::ArgumentMode::StopWine => {
            automation::set_unattended_exit_status(composition::stop_wine());
        }
        automation::ArgumentMode::RemoveDownloads => {
            automation::set_unattended_exit_status(composition::remove_downloads());
        }
    }
    Ok(())
}
