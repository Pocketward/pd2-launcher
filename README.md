# PD2 Launcher

> **Unofficial.** PD2 Launcher is a community-made tool. It is not affiliated with or endorsed by the Project Diablo 2 team or Blizzard Entertainment.

A Linux-native installer, updater, and launcher for [Project Diablo 2](https://www.projectdiablo2.com/), written in Rust with GPUI. It sets up Diablo II, Lord of Destruction, and Project Diablo 2 in its own Wine environment, keeps Project Diablo 2 up to date, and starts the game, on x86-64 PCs and on ARM64 devices (through FEX-Emu).

## Install

You need your Diablo II and Lord of Destruction CD keys (two different keys), an internet connection, and about 8 GB of free space. The launcher downloads everything else, including Blizzard's official installers (about 2.2 GB). If you already have Diablo II and Lord of Destruction installed, of any version, you can use that folder as it is instead and need no CD keys.

Download the executable `pd2-launcher-<version>-<arch>` from the [releases page](https://github.com/Pocketward/pd2-launcher/releases) (`x86_64` for PCs, `aarch64` for ARM computers). It is one file with nothing to install: make it executable (`chmod +x pd2-launcher-*`) and run it.

The launcher guides you through six steps: it checks your computer, asks where to install and how to get Diablo II, downloads and checks everything, installs Diablo II, installs Project Diablo 2, and then offers **Play**. The first installation usually takes 20–40 minutes, depending on your connection; keep the window open.

## What your computer needs

Step 1 of the launcher checks what it needs and, when something is missing, shows the command that installs it.

**x86-64 (Ubuntu, Debian, Arch, and similar):**

| | Ubuntu 24.04 / Debian | Arch Linux / CachyOS |
| --- | --- | --- |
| Launcher window | `libxcb1 libxkbcommon0 libxkbcommon-x11-0` | `libxcb libxkbcommon libxkbcommon-x11` |
| Wine and the game | `libx11-6 libxext6 libfreetype6 libfontconfig1 libegl1` | `libx11 libxext freetype2 fontconfig libglvnd mesa` (NVIDIA: `nvidia-utils`) |

A default Ubuntu desktop install already has all of these.

**ARM64:** [FEX-Emu](https://fex-emu.com/) with its x86 system files (its RootFS; run `FEXRootFSFetcher` once) on a kernel with 4 KB memory pages, plus the same libraries. The game runs through FEX, so it is slower than on a PC of the same class.

**Built-in FEX (ARM64):** the launcher downloads FEX-Emu and its Ubuntu RootFS, unchanged, from this project's `fex-<version>` mirror release, and from FEX's own sources (its Launchpad PPA, its RootFS server) when the mirror does not give them; every file must match its pinned SHA-256.

**Display and graphics:** Xorg, or Wayland with XWayland (the default on GNOME and KDE). No particular GPU driver is required: the game draws with OpenGL, and when that is done in software (as in a virtual machine), the Play screen says so; the game still runs, only slower.

## Using it

- **Play** starts Project Diablo 2. The launcher checks for Project Diablo 2 updates and offers them; it never forces one.
- **Settings** has the graphics mode (Glide, recommended, or DirectDraw), going straight to the online login, sound in the background, the install location, and **Official PD2 launcher → Software rendering** (keep it on for ARM64).
- **Open official PD2 launcher** (on the Play screen) opens Project Diablo 2's own launcher, for example to manage item filters.

## Where things are kept

Everything lives in `~/.pd2launcher`: the game and its Wine environment in `game/`, settings in `config/`, and logs and reports in `logs/`. Your CD keys are never saved. A copy of this folder from another computer can be used: the launcher checks what it finds and downloads whatever does not fit this computer.

To uninstall, delete the launcher and the `~/.pd2launcher` folder. Characters you played offline are stored inside that folder, so copy them out first if you want to keep them.

## Problems

If the launcher window never appears, it saves what went wrong to `~/.pd2launcher/logs/startup-error.txt` and tries to show a message. For anything else, open **Diagnostics** and choose **Save report**; the report never includes your CD keys. Attach it when you open an [issue](https://github.com/Pocketward/pd2-launcher/issues). [docs/DIAGNOSTICS.md](docs/DIAGNOSTICS.md) describes what the report contains.

## Building from source

From a clean checkout with Podman or Docker, `scripts/build-appimage-container.sh dist <x86_64|aarch64> binary` builds the standalone executable in a pinned Ubuntu 22.04 container and writes `dist/pd2-launcher-<version>-<arch>`; the script's header describes its options. The container runs in a build image with the build packages from a dated Ubuntu snapshot and the Rust toolchain of `rust-toolchain.toml`: the first build makes it, and later builds use it again. The releases are built in the images on this repository's `build-images` release (`.github/workflows/build-image.yml`); load one with `zstd -dc <file> | docker load` to build in the same image.

The window is the `gui` Cargo feature, which is on by default. `cargo build --release --locked --no-default-features --features release-build` builds the executable without it: the Launcher engine of the Android app, which runs only through the app.

## Disclaimer

Project Diablo 2, Diablo, and Blizzard are names of their respective owners. PD2 Launcher contains none of their software or data: it downloads Blizzard's installers and Project Diablo 2's files from their official sources on your computer.
