//! What a dev build adds for testing, and a release build leaves out.
//!
//! Every build is a dev build unless it is made with the `release-build`
//! feature, which the build scripts turn on for `PD2_RELEASE_BUILD=1`. A dev
//! build prefills the CD keys it was built with and has Wine say more in its
//! logs.

/// Whether this build carries the testing additions.
pub(crate) const DEV_BUILD: bool = !cfg!(feature = "release-build");

/// Wine's debug channels for the game: in a dev build, which DLLs load, when,
/// in which process, and the exceptions they raise.
pub(crate) const GAME_WINEDEBUG: &str = if DEV_BUILD {
    "fixme-all,+loaddll,+timestamp,+pid,warn+seh"
} else {
    "fixme-all"
};

/// Wine's debug channels for setup steps; the same in a dev build.
pub(crate) const SETUP_WINEDEBUG: &str = if DEV_BUILD {
    "fixme-all,+pid,+timestamp,+loaddll,warn+seh"
} else {
    "fixme-all"
};

/// The CD keys a dev build was compiled with (`PD2_DEV_D2_KEY` and
/// `PD2_DEV_D2_LOD_KEY`, which the container build takes from the build
/// machine's `.env.local`); a release build has none.
pub(crate) fn product_keys() -> Option<(&'static str, &'static str)> {
    #[cfg(not(feature = "release-build"))]
    {
        option_env!("PD2_DEV_D2_KEY").zip(option_env!("PD2_DEV_D2_LOD_KEY"))
    }
    #[cfg(feature = "release-build")]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_release_build_logs_plainly_and_has_no_keys() {
        if DEV_BUILD {
            assert!(GAME_WINEDEBUG.contains("+loaddll"));
            assert!(SETUP_WINEDEBUG.contains("+loaddll"));
        } else {
            assert_eq!(GAME_WINEDEBUG, "fixme-all");
            assert_eq!(SETUP_WINEDEBUG, "fixme-all");
            assert_eq!(product_keys(), None);
        }
    }
}
