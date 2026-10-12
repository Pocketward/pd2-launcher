//! Seeds D2GL's untouched default window size before a Glide launch.
//!
//! D2GL renders its fullscreen viewport at the stored `screen.window_size_*`
//! values, and a fresh configuration stores 800x600. Replacing only that exact
//! default with the physical size of the display the Launcher window is on lets
//! the first fullscreen launch fill the monitor. Any other stored size was
//! chosen by the player or by D2GL itself and is never touched.
//!
//! The configuration is user-owned, so every failure of the seed skips it and
//! the launch continues unchanged.
//!
//! On the Android host a handheld cannot toggle Fullscreen comfortably, so
//! when D2GL has not written its configuration yet the Launcher writes one
//! (fixed first-launch settings, viewport = the X screen) before the first
//! Glide launch. The desktop never creates it.
//! There an existing configuration's window size also follows the X screen,
//! whose size the app owns, its cursor and full-screen values are the first
//! launch's ([`ANDROID_SCREEN`]: an imported one keeps a desktop player's,
//! often windowed with a locked cursor, which the X screen cannot use), and
//! the app's graphics profile ([`GraphicsProfile`]) sets its few `graphics`
//! values when it is not the profile last applied ([`PROFILE_RECORD`]);
//! nothing else in it is touched (the player changes D2GL's graphics in the
//! game, Ctrl+O, and those changes stay until the profile changes).
//!
//! D2GL also loads the DLLs its configuration names (`load_dlls_early`,
//! `load_dlls_late`) into the game. On the Android host other apps the player
//! lets into the game folder can edit the configuration, so before D2GL can
//! start the Launcher sets both to the values PD2 ships ([`pin_dlls`]), and
//! the game does not start when it cannot.

use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Read as _, Write as _},
    ops::Range,
    os::unix::fs::MetadataExt as _,
    path::Path,
};

use rustix::fs::{AtFlags, Mode, OFlags, RenameFlags};
use serde_json::{Value, json};

use crate::safe_fs::Directory;

/// ProjectD2 below the game folder (the Managed one, or one used in place).
const PROJECT_D2: &str = "ProjectD2";
const CONFIG: &str = "d2gl.json";
const BACKUP: &str = "d2gl.json.bak";
/// The new configuration before its rename. Other apps on the Android host
/// cannot reach a name with the `.pd2launcher` prefix in the game folder.
const STAGED: &str = ".pd2launcher-d2gl.json.staged";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
const DEFAULT_SIZE: (u64, u64) = (800, 600);
const UTF8_BOM: &[u8] = b"\xEF\xBB\xBF";
/// The Launcher's own folder in ProjectD2, which also holds the update
/// transactions' state; the audit never counts it as a game file.
const LAUNCHER_FOLDER: &str = ".pd2launcher";
/// In [`LAUNCHER_FOLDER`]: the graphics profile last written into
/// `d2gl.json`, by name. It lives and goes with the game folder, so a new
/// installation or an imported one gets the profile at its first Play.
const PROFILE_RECORD: &str = "d2gl-profile";

/// The `screen` values the Android host writes into every configuration, a
/// new one and an existing one alike, because the game on the X screen
/// needs them. An imported configuration holds a desktop player's
/// choices, made for a real X server and window manager; on the desktop
/// they stand.
const ANDROID_SCREEN: [(&str, Value); 3] = [
    // Unlocked: D2GL's ClipCursor puts Wine in pointer-clipping mode, where
    // the X screen's XTEST pointer does not move the game's cursor and the
    // first click after a start is lost.
    ("unlock_cursor", Value::Bool(true)),
    // Full screen: Xvfb has no window manager, so Wine places a windowed
    // game as if one framed it, below a caption and partly off the X screen.
    ("window_fullscreen", Value::Bool(true)),
    // D2GL minimizes a full-screen game that loses focus when this is on,
    // and the X screen has no taskbar or window manager to bring it back.
    ("auto_minimize", Value::Bool(false)),
];

/// The `other` values naming the DLLs D2GL loads, as PD2 ships them: the
/// defaults of PD2's D2GL build, which it writes into a new configuration.
fn shipped_dlls() -> [(&'static str, Value); 2] {
    [
        ("load_dlls_early", Value::from("")),
        ("load_dlls_late", Value::from("SGD2FreeRes.dll")),
    ]
}

/// D2GL's graphics profile, which the Android app sends with each Play (its
/// Settings → Game → Graphics profile, Automatic already resolved). The
/// desktop sends none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GraphicsProfile {
    /// Project Diablo 2's own D2GL settings: the `advanced-aa` upscaling
    /// shader with sharpening and bloom.
    Default,
    /// D2GL's advanced settings tuned down for slower GPUs: the plain
    /// bilinear upscale, no sharpening, no bloom.
    Performance,
}

impl GraphicsProfile {
    /// The `graphics` values the profile sets; every other key stays.
    fn values(self) -> [(&'static str, Value); 3] {
        let (shader, effects) = match self {
            Self::Default => ("anti-aliasing\\advanced-aa.slangp", true),
            Self::Performance => ("bilinear.slangp", false),
        };
        [
            ("shader_preset", Value::from(shader)),
            ("sharpen", Value::from(effects)),
            ("bloom", Value::from(effects)),
        ]
    }

    /// Its name in [`PROFILE_RECORD`] and the log.
    fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Performance => "performance",
        }
    }
}

/// What the pre-launch display seed did to `ProjectD2/d2gl.json`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DisplaySeed {
    /// No display evidence arrived, or the renderer does not use D2GL.
    NotRequested,
    /// The configuration already stores a non-default size.
    Unchanged,
    /// D2GL has not written its configuration yet, so this is its first
    /// launch and nothing can be seeded; D2GL opens at its 800x600 default.
    ConfigurationAbsent,
    /// The default 800x600 size was replaced with the detected display size
    /// (on the Android host: the configuration was written, or its window
    /// size, screen values or graphics profile were).
    Seeded,
    /// The configuration could not be read, parsed, or replaced safely.
    Skipped,
}

/// `profile`: the Play's graphics profile, which only the Android host
/// writes.
pub(super) fn seed(
    game: &Directory,
    width: u32,
    height: u32,
    profile: Option<GraphicsProfile>,
) -> DisplaySeed {
    // On the Android host a missing configuration is written before the
    // first Glide launch instead of being left to D2GL.
    seed_config(game, width, height, crate::android_host(), profile).unwrap_or(DisplaySeed::Skipped)
}

/// Reports whether D2GL has not yet written its configuration, without
/// reading or creating it. Used when no display size is available to seed.
pub(super) fn configuration_absent(game: &Directory) -> bool {
    game.open_directory(Path::new(PROJECT_D2))
        .and_then(|project| project.open_file(Path::new(CONFIG)))
        .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// Sets `d2gl.json`'s DLLs to [`shipped_dlls`]'s values, every other byte
/// as it is; the Android host calls it before anything that can start D2GL.
///
/// The configuration is written anew even when it has them already, so a
/// descriptor another app opened before no longer reaches the file D2GL
/// reads. One that cannot be edited (not JSON, over the size limit, or the
/// values not uniquely addressable) is replaced with its backup,
/// [`BACKUP`], with these values, as D2GL itself falls back to the backup
/// for a configuration it cannot parse (an unchecked backup would name any
/// DLL too); it is removed when the backup cannot be edited either, and
/// without a configuration D2GL starts from its defaults, which are these
/// values. Other spellings of the name go first ([`remove_spellings`]).
///
/// An error means the configuration may still name other DLLs, so D2GL
/// must not start.
pub(crate) fn pin_dlls(game: &Directory) -> io::Result<()> {
    let project = game.open_directory(Path::new(PROJECT_D2))?;
    remove_spellings(&project)?;
    let config = match project.open_file(Path::new(CONFIG)) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let edited = read_limited(&config).and_then(|raw| {
        let pinned = pinned(&raw)?;
        Ok((pinned != raw, pinned))
    });
    match edited {
        Ok((changed, bytes)) => {
            if changed {
                eprintln!("pd2-launcher: d2gl.json names only the DLLs PD2 ships");
            }
            replace(&project, &config, &bytes)
        }
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            let backup = project
                .open_file(Path::new(BACKUP))
                .and_then(|backup| read_limited(&backup))
                .and_then(|raw| pinned(&raw));
            if let Ok(bytes) = backup {
                eprintln!("pd2-launcher: d2gl.json is its backup with PD2's DLLs: {error}");
                replace(&project, &config, &bytes)
            } else {
                eprintln!(
                    "pd2-launcher: d2gl.json removed, D2GL starts from its defaults: {error}"
                );
                remove(&project, &config)
            }
        }
        Err(error) => Err(error),
    }
}

/// `raw` with [`shipped_dlls`]'s values. serde_json and D2GL's JSON library
/// both keep the last of repeated keys, so the values checked here are the
/// ones D2GL reads.
fn pinned(raw: &[u8]) -> io::Result<Vec<u8>> {
    Ok(set_values(raw, "other", &shipped_dlls())?.unwrap_or_else(|| raw.to_vec()))
}

/// Removes the names in ProjectD2 other than `d2gl.json` that Wine takes for
/// it, the same letters in another case: Wine opens one of them when
/// `d2gl.json` itself is missing. A folder among them is an error.
fn remove_spellings(project: &Directory) -> io::Result<()> {
    let config = wine_upper(CONFIG);
    let directory = project.try_clone_file()?;
    for name in project.names()? {
        if name == CONFIG || name.to_str().map(wine_upper).as_ref() != Some(&config) {
            continue;
        }
        rustix::fs::unlinkat(&directory, name.as_os_str(), AtFlags::empty())?;
        eprintln!(
            "pd2-launcher: ProjectD2/{} removed, which Wine would read as d2gl.json",
            name.to_string_lossy()
        );
    }
    Ok(())
}

/// `name` as Wine compares names that differ only in case: each character
/// upper-cased on its own (one whose capital takes more than one character
/// stays as it is). This matches Wine for an ASCII name like `d2gl.json`.
fn wine_upper(name: &str) -> String {
    name.chars()
        .map(|character| {
            let mut upper = character.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(capital), None) => capital,
                _ => character,
            }
        })
        .collect()
}

/// Reads the configuration, refusing one over [`MAX_CONFIG_BYTES`].
fn read_limited(config: &File) -> io::Result<Vec<u8>> {
    if config.metadata()?.len() > MAX_CONFIG_BYTES {
        return Err(invalid("D2GL configuration exceeded its size limit"));
    }
    let mut raw = Vec::new();
    config.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut raw)?;
    if raw.len() as u64 > MAX_CONFIG_BYTES {
        return Err(invalid("D2GL configuration exceeded its size limit"));
    }
    Ok(raw)
}

fn seed_config(
    game: &Directory,
    width: u32,
    height: u32,
    android: bool,
    profile: Option<GraphicsProfile>,
) -> io::Result<DisplaySeed> {
    let profile = profile.filter(|_| android);
    let project = game.open_directory(Path::new(PROJECT_D2))?;
    let config = match project.open_file(Path::new(CONFIG)) {
        Ok(config) => config,
        Err(error) if error.kind() == io::ErrorKind::NotFound && android => {
            let first = profile.unwrap_or(GraphicsProfile::Default);
            create(&project, &first_launch_config(width, height, first))?;
            // What was written, so an older record never stands for it.
            record_profile(&project, first);
            return Ok(DisplaySeed::Seeded);
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(DisplaySeed::ConfigurationAbsent);
        }
        Err(error) => return Err(error),
    };
    let raw = read_limited(&config)?;
    // A profile is written only when it is not the one last applied, so the
    // player's own changes in the game stay until the profile changes.
    let changed = profile.filter(|profile| applied_profile(&project) != Some(*profile));
    // The window size, the Android screen values and the profile stand
    // alone: one that cannot address the configuration never keeps the
    // others from being written. A profile that cannot be written is not
    // recorded, so the next Play tries again.
    let seeded = seeded_config(&raw, width, height, android);
    let base = match &seeded {
        Ok(Some(seeded)) => seeded.as_slice(),
        _ => &raw,
    };
    // On the Android host an existing configuration gets the first launch's
    // cursor and full-screen values too: an imported one keeps a desktop
    // player's, with which the game loses the first click after each start
    // and runs windowed partly off the X screen.
    let screened = if android {
        set_values(base, "screen", &ANDROID_SCREEN).unwrap_or_else(|error| {
            eprintln!("pd2-launcher: d2gl.json keeps its cursor and full-screen settings: {error}");
            None
        })
    } else {
        None
    };
    let screen_written = screened.is_some();
    let base = screened.as_deref().unwrap_or(base);
    let profiled = changed.and_then(|profile| match profiled_config(base, profile) {
        Ok(profiled) => Some((profile, profiled)),
        Err(error) => {
            eprintln!(
                "pd2-launcher: d2gl.json keeps its graphics, the {} profile could not be written: {error}",
                profile.name()
            );
            None
        }
    });
    let (applied, profiled) = profiled.unzip();
    // Each edit was made on the bytes of the ones before it.
    let edited = profiled.flatten().or(screened);
    let written = match (edited, seeded) {
        (edited, Ok(seeded)) => edited.or(seeded),
        (None, Err(error)) if applied.is_none() => return Err(error),
        (edited, Err(error)) => {
            eprintln!("pd2-launcher: d2gl.json keeps its window size: {error}");
            edited
        }
    };
    if let Some(bytes) = &written {
        replace(&project, &config, bytes)?;
    }
    if screen_written {
        eprintln!("pd2-launcher: d2gl.json is full screen with the cursor unlocked");
    }
    if let Some(profile) = applied {
        record_profile(&project, profile);
    }
    Ok(match written {
        None => DisplaySeed::Unchanged,
        Some(_) => DisplaySeed::Seeded,
    })
}

/// The profile [`PROFILE_RECORD`] names, or `None` when there is none (no
/// Play has applied one to this game folder) or it cannot be read.
fn applied_profile(project: &Directory) -> Option<GraphicsProfile> {
    let record = project
        .open_directory(Path::new(LAUNCHER_FOLDER))
        .and_then(|folder| folder.open_file(Path::new(PROFILE_RECORD)))
        .ok()?;
    let mut name = String::new();
    record.take(64).read_to_string(&mut name).ok()?;
    [GraphicsProfile::Default, GraphicsProfile::Performance]
        .into_iter()
        .find(|profile| profile.name() == name.trim())
}

/// Records `profile` as written into `d2gl.json`. A record that fails only
/// means the next Play writes the profile's values again.
fn record_profile(project: &Directory, profile: GraphicsProfile) {
    let recorded = project
        .ensure_directory(Path::new(LAUNCHER_FOLDER))
        .and_then(|folder| {
            folder.atomic_write(
                OsStr::new(PROFILE_RECORD),
                format!("{}\n", profile.name()).as_bytes(),
            )
        });
    match recorded {
        Ok(()) => eprintln!(
            "pd2-launcher: d2gl.json has the {} graphics profile",
            profile.name()
        ),
        Err(error) => eprintln!(
            "pd2-launcher: d2gl.json has the {} graphics profile, not recorded: {error}",
            profile.name()
        ),
    }
}

/// Returns the configuration with `profile`'s `graphics` values set, or
/// `None` when it has them already ([`set_values`]).
fn profiled_config(raw: &[u8], profile: GraphicsProfile) -> io::Result<Option<Vec<u8>>> {
    set_values(raw, "graphics", &profile.values())
}

/// Returns the configuration with `values` set in its `section` object, or
/// `None` when it has them already. Present values are replaced in place and
/// missing ones inserted at the start of the section (or of the
/// configuration, in a new section), so every other byte stays as it is.
fn set_values(raw: &[u8], section: &str, values: &[(&str, Value)]) -> io::Result<Option<Vec<u8>>> {
    let (bom, body) = match raw.strip_prefix(UTF8_BOM) {
        Some(body) => (UTF8_BOM, body),
        None => (&[][..], raw),
    };
    let original: Value =
        serde_json::from_slice(body).map_err(|_| invalid("D2GL configuration was not JSON"))?;
    let mut expected = original.clone();
    let object = expected
        .as_object_mut()
        .ok_or_else(|| invalid("D2GL configuration was not an object"))?
        .entry(section)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| invalid(format!("D2GL {section} was not an object")))?;
    for (key, value) in values {
        object.insert((*key).to_owned(), value.clone());
    }
    if expected == original {
        return Ok(None);
    }

    let unaddressable = || {
        invalid(format!(
            "D2GL {section} values were not uniquely addressable"
        ))
    };
    let mut scanner = Scanner { bytes: body, at: 0 };
    let top = scanner.object_members().ok_or_else(unaddressable)?;
    // (span, text): spans are disjoint; an insertion is an empty span.
    let mut edits: Vec<(Range<usize>, String)> = Vec::new();
    let member = |members: &[(Range<usize>, Range<usize>)], name: &[u8]| {
        let mut found = members
            .iter()
            .filter(|(key, _)| &body[key.clone()] == name)
            .map(|(_, value)| value.clone());
        match (found.next(), found.next()) {
            (None, _) => Ok(None),
            (Some(value), None) => Ok(Some(value)),
            (Some(_), Some(_)) => Err(unaddressable()),
        }
    };
    let rendered = |key: &str, value: &Value| format!("\"{key}\": {value}");
    match member(&top, section.as_bytes())? {
        Some(span) => {
            let mut scanner = Scanner {
                bytes: body,
                at: span.start,
            };
            let members = scanner.object_members().ok_or_else(unaddressable)?;
            let mut missing = Vec::new();
            for (key, value) in values {
                match member(&members, key.as_bytes())? {
                    Some(span) => edits.push((span, value.to_string())),
                    None => missing.push(rendered(key, value)),
                }
            }
            if !missing.is_empty() {
                let mut text = missing.join(", ");
                if !members.is_empty() {
                    text.push_str(", ");
                }
                let open = span.start + 1;
                edits.push((open..open, text));
            }
        }
        None => {
            let open = body
                .iter()
                .position(|byte| *byte == b'{')
                .ok_or_else(unaddressable)?
                + 1;
            let members = values
                .iter()
                .map(|(key, value)| rendered(key, value))
                .collect::<Vec<_>>()
                .join(", ");
            let mut text = format!("\"{section}\": {{{members}}}");
            if !top.is_empty() {
                text.push_str(", ");
            }
            edits.push((open..open, text));
        }
    }
    edits.sort_by_key(|(span, _)| span.start);
    let mut edited = bom.to_vec();
    let mut cursor = 0;
    for (span, text) in edits {
        edited.extend_from_slice(&body[cursor..span.start]);
        edited.extend_from_slice(text.as_bytes());
        cursor = span.end;
    }
    edited.extend_from_slice(&body[cursor..]);

    // The textual splice must change exactly the given values.
    let observed: Value = serde_json::from_slice(&edited[bom.len()..])
        .map_err(|_| invalid("edited D2GL configuration was not JSON"))?;
    if observed != expected {
        return Err(invalid("edited D2GL configuration changed other values"));
    }
    Ok(Some(edited))
}

/// Returns the configuration with only the default window size replaced, or
/// `None` when the stored size is not the untouched 800x600 default.
/// `follow_screen` (the Android host): any other stored size is replaced
/// too. There the viewport must be the X screen, whose size the app owns, so
/// another size is stale: an X screen of another size, or a configuration
/// copied from another device.
fn seeded_config(
    raw: &[u8],
    width: u32,
    height: u32,
    follow_screen: bool,
) -> io::Result<Option<Vec<u8>>> {
    let (bom, body) = match raw.strip_prefix(UTF8_BOM) {
        Some(body) => (UTF8_BOM, body),
        None => (&[][..], raw),
    };
    let original: Value =
        serde_json::from_slice(body).map_err(|_| invalid("D2GL configuration was not JSON"))?;
    let screen = original
        .as_object()
        .ok_or_else(|| invalid("D2GL configuration was not an object"))?
        .get("screen");
    let stored = screen.and_then(|screen| {
        Some((
            screen.get("window_size_width")?.as_u64()?,
            screen.get("window_size_height")?.as_u64()?,
        ))
    });
    let replace = match stored {
        Some(DEFAULT_SIZE) => true,
        Some(size) => follow_screen && size != (u64::from(width), u64::from(height)),
        None => false,
    };
    if !replace {
        return Ok(None);
    }

    let (width_span, height_span) = locate_window_size(body)
        .ok_or_else(|| invalid("D2GL window size was not uniquely addressable"))?;
    let mut replacements = [
        (width_span, width.to_string()),
        (height_span, height.to_string()),
    ];
    replacements.sort_by_key(|(span, _)| span.start);
    let mut seeded = bom.to_vec();
    let mut cursor = 0;
    for (span, value) in replacements {
        seeded.extend_from_slice(&body[cursor..span.start]);
        seeded.extend_from_slice(value.as_bytes());
        cursor = span.end;
    }
    seeded.extend_from_slice(&body[cursor..]);

    // The textual splice must change exactly the two size values.
    let mut expected = original;
    expected["screen"]["window_size_width"] = Value::from(width);
    expected["screen"]["window_size_height"] = Value::from(height);
    let observed: Value = serde_json::from_slice(&seeded[bom.len()..])
        .map_err(|_| invalid("seeded D2GL configuration was not JSON"))?;
    if observed != expected {
        return Err(invalid("seeded D2GL configuration changed other values"));
    }
    Ok(Some(seeded))
}

/// Finds the byte spans of the unique `screen.window_size_width` and
/// `screen.window_size_height` values in an already validated document.
fn locate_window_size(body: &[u8]) -> Option<(Range<usize>, Range<usize>)> {
    let mut scanner = Scanner { bytes: body, at: 0 };
    let top = scanner.object_members()?;
    let screen = unique_member(body, &top, b"screen")?;
    if body.get(screen.start) != Some(&b'{') {
        return None;
    }
    let mut scanner = Scanner {
        bytes: body,
        at: screen.start,
    };
    let members = scanner.object_members()?;
    Some((
        unique_member(body, &members, b"window_size_width")?,
        unique_member(body, &members, b"window_size_height")?,
    ))
}

fn unique_member(
    body: &[u8],
    members: &[(Range<usize>, Range<usize>)],
    name: &[u8],
) -> Option<Range<usize>> {
    let mut matches = members
        .iter()
        .filter(|(key, _)| &body[key.clone()] == name)
        .map(|(_, value)| value.clone());
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}

/// Minimal JSON span scanner. Values are validated by `serde_json` first;
/// this only recovers raw member key/value spans so formatting is preserved.
struct Scanner<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Scanner<'_> {
    fn skip_whitespace(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn eat(&mut self, expected: u8) -> Option<()> {
        self.skip_whitespace();
        (self.bytes.get(self.at) == Some(&expected)).then(|| self.at += 1)
    }

    /// Returns the raw span between the quotes.
    fn string(&mut self) -> Option<Range<usize>> {
        self.eat(b'"')?;
        let start = self.at;
        loop {
            match *self.bytes.get(self.at)? {
                b'"' => {
                    let span = start..self.at;
                    self.at += 1;
                    return Some(span);
                }
                b'\\' => self.at += 2,
                _ => self.at += 1,
            }
        }
    }

    fn value(&mut self) -> Option<Range<usize>> {
        self.skip_whitespace();
        let start = self.at;
        match *self.bytes.get(self.at)? {
            b'"' => {
                self.string()?;
            }
            b'{' | b'[' => {
                let mut depth = 0_usize;
                loop {
                    match *self.bytes.get(self.at)? {
                        b'"' => {
                            self.string()?;
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth = depth.checked_sub(1)?;
                            if depth == 0 {
                                self.at += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.at += 1;
                }
            }
            _ => {
                while self.bytes.get(self.at).is_some_and(|byte| {
                    !matches!(byte, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                }) {
                    self.at += 1;
                }
            }
        }
        (self.at > start).then_some(start..self.at)
    }

    fn object_members(&mut self) -> Option<Vec<(Range<usize>, Range<usize>)>> {
        self.eat(b'{')?;
        let mut members = Vec::new();
        if self.eat(b'}').is_some() {
            return Some(members);
        }
        loop {
            let key = self.string()?;
            self.eat(b':')?;
            let value = self.value()?;
            members.push((key, value));
            if self.eat(b',').is_some() {
                continue;
            }
            self.eat(b'}')?;
            return Some(members);
        }
    }
}

/// Replaces `d2gl.json` by rename from a staged sibling, only while the name
/// still denotes the descriptor that was read. No lookup follows a symlink.
fn replace(project: &Directory, original: &File, bytes: &[u8]) -> io::Result<()> {
    let directory = project.try_clone_file()?;
    let read = original.metadata()?;
    match rustix::fs::unlinkat(&directory, STAGED, AtFlags::empty()) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(error) => return Err(error.into()),
    }
    let staged = File::from(rustix::fs::openat(
        &directory,
        STAGED,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(read.mode() & 0o777),
    )?);
    let result = (|| {
        (&staged).write_all(bytes)?;
        staged.sync_all()?;
        let current = rustix::fs::statat(&directory, CONFIG, AtFlags::SYMLINK_NOFOLLOW)?;
        if current.st_dev != read.dev() || current.st_ino != read.ino() {
            return Err(invalid("D2GL configuration changed while seeding"));
        }
        rustix::fs::renameat(&directory, STAGED, &directory, CONFIG)?;
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&directory, STAGED, AtFlags::empty());
    }
    result
}

/// Removes `d2gl.json`, only while the name still denotes the descriptor
/// that was read. No lookup follows a symlink.
fn remove(project: &Directory, original: &File) -> io::Result<()> {
    let directory = project.try_clone_file()?;
    let read = original.metadata()?;
    let current = rustix::fs::statat(&directory, CONFIG, AtFlags::SYMLINK_NOFOLLOW)?;
    if current.st_dev != read.dev() || current.st_ino != read.ino() {
        return Err(io::Error::other(
            "D2GL configuration changed while it was read",
        ));
    }
    rustix::fs::unlinkat(&directory, CONFIG, AtFlags::empty())?;
    directory.sync_all()
}

/// The Android host's first-launch D2GL settings: the fullscreen viewport
/// set to the X screen, D2GL's corner mini map off, and [`ANDROID_SCREEN`]'s,
/// [`shipped_dlls`]' and `profile`'s values. The player changes them in the
/// game (Ctrl+O); [`ANDROID_SCREEN`]'s are set again at the next Play.
fn first_launch_config(width: u32, height: u32, profile: GraphicsProfile) -> Vec<u8> {
    let mut config = json!({
        "screen": {
            "window_size_width": width,
            "window_size_height": height,
            "window_centered": true,
            "window_position_x": 0,
            "window_position_y": 0,
            // These three keep their place here; their values are
            // ANDROID_SCREEN's, set below.
            "unlock_cursor": true,
            "window_fullscreen": true,
            "window_vsync": true,
            "foreground_fps_active": true,
            "foreground_fps_value": 60,
            "background_fps_active": true,
            "background_fps_value": 25,
            "auto_minimize": false,
            "window_dark_mode": true
        },
        "graphics": {
            "shader_preset": "anti-aliasing\\advanced-aa.slangp",
            "sharpen": true,
            "sharpen_strength": 1.2000000476837158_f64,
            "sharpen_clamp": 0.800000011920929_f64,
            "sharpen_radius": 0.4000000059604645_f64,
            "fxaa": false,
            "fxaa_preset": 1,
            "lut": 0,
            "bloom": true,
            "bloom_exposure": 1.0,
            "bloom_gamma": 0.75,
            "stretched_horizontal": false,
            "stretched_vertical": false
        },
        "features": {
            "hd_cursor": true,
            "hd_text": true,
            "hd_text_scale": 1.0,
            "mini_map": false,
            "mini_map_text_over": true,
            "mini_map_width": 200,
            "mini_map_height": 140,
            "motion_prediction": true,
            "skip_intro": false,
            "no_pickup": false,
            "show_item_quantity": true,
            "show_fps": false
        },
        "other": {
            "gl_ver_major": 4,
            "gl_ver_minor": 6,
            "use_compute_shader": false,
            "frame_latency": 1,
            // These two keep their place here; their values are
            // shipped_dlls()'s, set below.
            "load_dlls_early": "",
            "load_dlls_late": ""
        }
    });
    for (key, value) in ANDROID_SCREEN {
        config["screen"][key] = value;
    }
    for (key, value) in shipped_dlls() {
        config["other"][key] = value;
    }
    for (key, value) in profile.values() {
        config["graphics"][key] = value;
    }
    serde_json::to_vec_pretty(&config).expect("a JSON value serializes")
}

/// Writes a new `d2gl.json` from a staged sibling, never replacing a file
/// that appeared meanwhile. No lookup follows a symlink.
fn create(project: &Directory, bytes: &[u8]) -> io::Result<()> {
    let directory = project.try_clone_file()?;
    match rustix::fs::unlinkat(&directory, STAGED, AtFlags::empty()) {
        Ok(()) => {}
        Err(error) if error == rustix::io::Errno::NOENT => {}
        Err(error) => return Err(error.into()),
    }
    let staged = File::from(rustix::fs::openat(
        &directory,
        STAGED,
        OFlags::CREATE | OFlags::EXCL | OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    )?);
    let result = (|| {
        (&staged).write_all(bytes)?;
        staged.sync_all()?;
        rustix::fs::renameat_with(
            &directory,
            STAGED,
            &directory,
            CONFIG,
            RenameFlags::NOREPLACE,
        )?;
        directory.sync_all()
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(&directory, STAGED, AtFlags::empty());
    }
    result
}

fn invalid(message: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink};

    use super::*;

    const DEFAULT_CONFIG: &str = r#"{
    "screen": {
        "window_size_width": 800,
        "window_size_height": 600,
        "window_centered": true,
        "window_fullscreen": true,
        "foreground_fps_value": 60
    },
    "graphics": {
        "shader_preset": "anti-aliasing\\advanced-aa.slangp",
        "sharpen_strength": 1.2000000476837158,
        "window_size_width": 800
    },
    "other": {
        "load_dlls_late": "SGD2FreeRes.dll"
    }
}"#;

    fn managed_root(config: Option<&[u8]>) -> (tempfile::TempDir, Directory) {
        let temporary = tempfile::tempdir().expect("temporary managed root");
        let project = temporary.path().join(PROJECT_D2);
        fs::create_dir_all(&project).expect("ProjectD2");
        if let Some(config) = config {
            fs::write(project.join(CONFIG), config).expect("d2gl.json");
        }
        let root = Directory::open(temporary.path()).expect("retained root");
        (temporary, root)
    }

    fn read_config(root: &tempfile::TempDir) -> Vec<u8> {
        fs::read(root.path().join(PROJECT_D2).join(CONFIG)).expect("read d2gl.json")
    }

    #[test]
    fn default_size_is_replaced_and_everything_else_is_byte_identical() {
        let seeded = seeded_config(DEFAULT_CONFIG.as_bytes(), 3840, 2560, false)
            .unwrap()
            .expect("default size is seeded");

        let expected = DEFAULT_CONFIG
            .replacen(
                "\"window_size_width\": 800",
                "\"window_size_width\": 3840",
                1,
            )
            .replacen(
                "\"window_size_height\": 600",
                "\"window_size_height\": 2560",
                1,
            );
        assert_eq!(String::from_utf8(seeded).unwrap(), expected);
    }

    #[test]
    fn a_non_default_size_is_never_replaced() {
        for (width, height) in [(3840, 2560), (800, 601), (1024, 600)] {
            let custom = DEFAULT_CONFIG
                .replacen("800", &width.to_string(), 1)
                .replacen("600", &height.to_string(), 1);
            assert_eq!(
                seeded_config(custom.as_bytes(), 2560, 1440, false).unwrap(),
                None,
                "{width}x{height} must stay"
            );
        }
        let float = DEFAULT_CONFIG.replacen("800", "800.0", 1);
        assert_eq!(
            seeded_config(float.as_bytes(), 2560, 1440, false).unwrap(),
            None
        );
        assert_eq!(
            seeded_config(b"{\"graphics\": {}}", 2560, 1440, false).unwrap(),
            None
        );
    }

    #[test]
    fn invalid_or_ambiguous_configuration_is_rejected() {
        for invalid in [
            &b"not json"[..],
            b"[800, 600]",
            b"{\"screen\": {\"window_size_width\": 800, \"window_size_height\": 600,}}",
            b"{\"screen\": {\"window_size_width\": 800, \"window_size_height\": 600, \"window_size_width\": 800}}",
        ] {
            assert!(seeded_config(invalid, 2560, 1440, false).is_err());
        }
    }

    #[test]
    fn byte_order_mark_and_compact_formatting_are_preserved() {
        let compact =
            b"\xEF\xBB\xBF{\"screen\":{\"window_size_height\":600,\"window_size_width\":800}}";
        let seeded = seeded_config(compact, 2560, 1440, false).unwrap().unwrap();
        assert_eq!(
            seeded,
            b"\xEF\xBB\xBF{\"screen\":{\"window_size_height\":1440,\"window_size_width\":2560}}"
        );
    }

    #[test]
    fn seed_rewrites_only_the_default_managed_configuration() {
        let (temporary, root) = managed_root(Some(DEFAULT_CONFIG.as_bytes()));
        let project = temporary.path().join(PROJECT_D2);
        let mut permissions = fs::metadata(project.join(CONFIG)).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o644);
        fs::set_permissions(project.join(CONFIG), permissions).unwrap();

        assert_eq!(seed(&root, 3840, 2560, None), DisplaySeed::Seeded);

        let seeded: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        assert_eq!(seeded["screen"]["window_size_width"], 3840);
        assert_eq!(seeded["screen"]["window_size_height"], 2560);
        assert_eq!(seeded["graphics"]["window_size_width"], 800);
        assert_eq!(
            fs::metadata(project.join(CONFIG)).unwrap().mode() & 0o777,
            0o644
        );
        assert!(!project.join(STAGED).exists());

        // The seeded size is no longer the default, so a second launch keeps it.
        assert_eq!(seed(&root, 2560, 1440, None), DisplaySeed::Unchanged);
        let kept: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        assert_eq!(kept["screen"]["window_size_width"], 3840);
    }

    #[test]
    fn the_android_host_writes_the_first_launch_configuration() {
        let (temporary, root) = managed_root(None);
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        let project = temporary.path().join(PROJECT_D2);
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        assert_eq!(written["screen"]["window_size_width"], 1280);
        assert_eq!(written["screen"]["window_size_height"], 960);
        assert_eq!(written["screen"]["window_fullscreen"], true);
        // A locked cursor puts Wine in pointer-clipping mode, where the
        // X screen's pointer stops reaching the game and the first click
        // is lost.
        assert_eq!(written["screen"]["unlock_cursor"], true);
        assert_eq!(
            written["graphics"]["shader_preset"],
            "anti-aliasing\\advanced-aa.slangp"
        );
        assert_eq!(written["other"]["load_dlls_late"], "SGD2FreeRes.dll");
        assert!(!project.join(STAGED).exists());

        // An existing configuration keeps everything but its window size,
        // which follows the X screen (another X screen, or a copy from
        // another device).
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Unchanged
        );
        let before = String::from_utf8(read_config(&temporary)).unwrap();
        assert_eq!(
            seed_config(&root, 1068, 480, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        let after = String::from_utf8(read_config(&temporary)).unwrap();
        assert_eq!(
            after,
            before
                .replacen(
                    "\"window_size_width\": 1280",
                    "\"window_size_width\": 1068",
                    1
                )
                .replacen(
                    "\"window_size_height\": 960",
                    "\"window_size_height\": 480",
                    1
                )
        );
        let (temporary, root) = managed_root(Some(DEFAULT_CONFIG.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        let seeded = String::from_utf8(read_config(&temporary)).unwrap();
        assert!(seeded.contains("\"window_size_width\": 1280"), "{seeded}");
        assert!(seeded.contains("\"foreground_fps_value\": 60"), "{seeded}");
    }

    #[test]
    fn each_profile_has_its_first_launch_graphics_and_the_rest_is_the_same() {
        // The first-launch configuration is the same whatever the size (the
        // game resolution the player picked), with D2GL's corner mini map off
        // (it costs frame rate; D2GL's in-game options turn it on); the
        // profile sets only its graphics values.
        let full: Value =
            serde_json::from_slice(&first_launch_config(2400, 1080, GraphicsProfile::Default))
                .unwrap();
        let small: Value =
            serde_json::from_slice(&first_launch_config(1068, 480, GraphicsProfile::Default))
                .unwrap();
        assert_eq!(full["graphics"], small["graphics"]);
        assert_eq!(full["features"], small["features"]);
        assert_eq!(
            full["graphics"]["shader_preset"],
            "anti-aliasing\\advanced-aa.slangp"
        );
        assert_eq!(full["graphics"]["bloom"], true);
        assert_eq!(full["graphics"]["sharpen"], true);
        assert_eq!(full["features"]["mini_map"], false);

        let mut performance: Value = serde_json::from_slice(&first_launch_config(
            1068,
            480,
            GraphicsProfile::Performance,
        ))
        .unwrap();
        assert_eq!(performance["graphics"]["shader_preset"], "bilinear.slangp");
        assert_eq!(performance["graphics"]["sharpen"], false);
        assert_eq!(performance["graphics"]["bloom"], false);
        assert_eq!(performance["features"]["mini_map"], false);
        for (key, value) in GraphicsProfile::Default.values() {
            performance["graphics"][key] = value;
        }
        assert_eq!(performance, small);
    }

    fn record_path(root: &tempfile::TempDir) -> std::path::PathBuf {
        root.path()
            .join(PROJECT_D2)
            .join(LAUNCHER_FOLDER)
            .join(PROFILE_RECORD)
    }

    #[test]
    fn the_first_launch_takes_the_profile_and_records_it() {
        let (temporary, root) = managed_root(None);
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            first_launch_config(1068, 480, GraphicsProfile::Performance)
        );
        assert_eq!(
            fs::read_to_string(record_path(&temporary)).unwrap(),
            "performance\n"
        );
    }

    #[test]
    fn a_profile_is_written_only_when_it_changes_and_the_players_values_stay_until_then() {
        let (temporary, root) = managed_root(None);
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );

        // The player turns bloom and FXAA on in the game (Ctrl+O): every
        // Play with the same profile keeps them.
        let mut played: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        played["graphics"]["bloom"] = json!(true);
        played["graphics"]["fxaa"] = json!(true);
        let played_bytes = serde_json::to_vec_pretty(&played).unwrap();
        fs::write(
            temporary.path().join(PROJECT_D2).join(CONFIG),
            &played_bytes,
        )
        .unwrap();
        for _ in 0..2 {
            assert_eq!(
                seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
                DisplaySeed::Unchanged
            );
            assert_eq!(read_config(&temporary), played_bytes);
        }

        // Default picked: its three values, nothing else.
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Default)).unwrap(),
            DisplaySeed::Seeded
        );
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        let mut expected = played.clone();
        expected["graphics"]["shader_preset"] = json!("anti-aliasing\\advanced-aa.slangp");
        expected["graphics"]["sharpen"] = json!(true);
        expected["graphics"]["bloom"] = json!(true);
        assert_eq!(written, expected);
        assert_eq!(written["graphics"]["fxaa"], true);
        assert_eq!(
            fs::read_to_string(record_path(&temporary)).unwrap(),
            "default\n"
        );
        let default_bytes = read_config(&temporary);
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Default)).unwrap(),
            DisplaySeed::Unchanged
        );
        assert_eq!(read_config(&temporary), default_bytes);

        // Back to Performance with another X screen: both in one write.
        assert_eq!(
            seed_config(&root, 1604, 720, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        expected["graphics"]["shader_preset"] = json!("bilinear.slangp");
        expected["graphics"]["sharpen"] = json!(false);
        expected["graphics"]["bloom"] = json!(false);
        expected["screen"]["window_size_width"] = json!(1604);
        expected["screen"]["window_size_height"] = json!(720);
        assert_eq!(written, expected);
    }

    #[test]
    fn a_configuration_without_a_readable_record_gets_the_profile() {
        // An imported game folder has its own d2gl.json and no record; a
        // record the Launcher cannot read counts as none.
        for record in [None, Some(&b"ultra\n"[..])] {
            let (temporary, root) = managed_root(Some(DEFAULT_CONFIG.as_bytes()));
            if let Some(record) = record {
                let folder = temporary.path().join(PROJECT_D2).join(LAUNCHER_FOLDER);
                fs::create_dir_all(&folder).unwrap();
                fs::write(folder.join(PROFILE_RECORD), record).unwrap();
            }
            assert_eq!(
                seed_config(&root, 1280, 960, true, Some(GraphicsProfile::Performance)).unwrap(),
                DisplaySeed::Seeded
            );
            let written = String::from_utf8(read_config(&temporary)).unwrap();
            assert_eq!(
                written,
                DEFAULT_CONFIG
                    .replacen(
                        "\"window_size_width\": 800",
                        "\"window_size_width\": 1280",
                        1
                    )
                    .replacen(
                        "\"window_size_height\": 600",
                        "\"window_size_height\": 960",
                        1
                    )
                    .replacen(
                        "\"screen\": {",
                        "\"screen\": {\"unlock_cursor\": true, \"auto_minimize\": false, ",
                        1
                    )
                    .replacen(
                        "\"graphics\": {",
                        "\"graphics\": {\"sharpen\": false, \"bloom\": false, ",
                        1
                    )
                    .replacen("anti-aliasing\\\\advanced-aa.slangp", "bilinear.slangp", 1)
            );
            assert_eq!(
                fs::read_to_string(record_path(&temporary)).unwrap(),
                "performance\n"
            );
        }
    }

    #[test]
    fn the_window_size_and_the_profile_are_written_without_each_other() {
        // Graphics that are not an object: the window size still follows the
        // X screen, and nothing is recorded, so the next Play tries again.
        let (temporary, root) = managed_root(Some(
            br#"{"screen": {"window_size_width": 1604, "window_size_height": 720, "unlock_cursor": true, "window_fullscreen": true, "auto_minimize": false}, "graphics": null}"#,
        ));
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            br#"{"screen": {"window_size_width": 1068, "window_size_height": 480, "unlock_cursor": true, "window_fullscreen": true, "auto_minimize": false}, "graphics": null}"#
        );
        assert!(!record_path(&temporary).exists());

        // A window size that is not addressable: the profile is still written.
        let ambiguous = r#"{"screen": {"window_size_width": 800, "window_size_height": 600, "window_size_width": 800, "unlock_cursor": true, "window_fullscreen": true, "auto_minimize": false}, "graphics": {}}"#;
        let (temporary, root) = managed_root(Some(ambiguous.as_bytes()));
        assert!(seed_config(&root, 1068, 480, true, None).is_err());
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            ambiguous.replacen(
                "\"graphics\": {}",
                "\"graphics\": {\"shader_preset\": \"bilinear.slangp\", \"sharpen\": false, \"bloom\": false}",
                1
            )
        );
        assert_eq!(
            fs::read_to_string(record_path(&temporary)).unwrap(),
            "performance\n"
        );
    }

    #[test]
    fn a_first_launch_records_what_it_wrote_without_a_profile_too() {
        // An older record never stands for a configuration written anew.
        let (temporary, root) = managed_root(None);
        let folder = temporary.path().join(PROJECT_D2).join(LAUNCHER_FOLDER);
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(PROFILE_RECORD), "performance\n").unwrap();
        assert_eq!(
            seed_config(&root, 1068, 480, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            first_launch_config(1068, 480, GraphicsProfile::Default)
        );
        assert_eq!(
            fs::read_to_string(record_path(&temporary)).unwrap(),
            "default\n"
        );
        assert_eq!(
            seed_config(&root, 1068, 480, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        assert_eq!(written["graphics"]["shader_preset"], "bilinear.slangp");
    }

    #[test]
    fn a_profile_changes_only_its_values_byte_for_byte() {
        // Present values in place, missing ones at the start of graphics.
        let profiled = profiled_config(DEFAULT_CONFIG.as_bytes(), GraphicsProfile::Default)
            .unwrap()
            .expect("sharpen and bloom are missing");
        assert_eq!(
            String::from_utf8(profiled.clone()).unwrap(),
            DEFAULT_CONFIG.replacen(
                "\"graphics\": {",
                "\"graphics\": {\"sharpen\": true, \"bloom\": true, ",
                1
            )
        );
        assert_eq!(
            profiled_config(&profiled, GraphicsProfile::Default).unwrap(),
            None
        );

        // A byte order mark and compact formatting stay; no graphics yet.
        let compact = b"\xEF\xBB\xBF{\"screen\":{\"window_size_width\":1068}}";
        assert_eq!(
            profiled_config(compact, GraphicsProfile::Performance)
                .unwrap()
                .unwrap(),
            b"\xEF\xBB\xBF{\"graphics\": {\"shader_preset\": \"bilinear.slangp\", \"sharpen\": false, \"bloom\": false}, \"screen\":{\"window_size_width\":1068}}"
        );
        let empty: Value = serde_json::from_slice(
            &profiled_config(b"{}", GraphicsProfile::Default)
                .unwrap()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            empty,
            json!({"graphics": {"shader_preset": "anti-aliasing\\advanced-aa.slangp", "sharpen": true, "bloom": true}})
        );

        // Graphics that are not one object are left alone.
        for invalid in [
            &b"{\"graphics\": []}"[..],
            b"{\"graphics\": {\"bloom\": true, \"bloom\": false}}",
            b"[]",
        ] {
            assert!(profiled_config(invalid, GraphicsProfile::Performance).is_err());
        }
    }

    #[test]
    fn the_desktop_never_writes_a_profile() {
        let (temporary, root) = managed_root(Some(DEFAULT_CONFIG.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, false, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        let original: Value = serde_json::from_str(DEFAULT_CONFIG).unwrap();
        assert_eq!(written["graphics"], original["graphics"]);
        assert!(!record_path(&temporary).exists());

        let (temporary, root) = managed_root(None);
        assert_eq!(
            seed_config(&root, 1280, 960, false, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::ConfigurationAbsent
        );
        assert!(!temporary.path().join(PROJECT_D2).join(CONFIG).exists());
        assert!(!record_path(&temporary).exists());
    }

    /// A desktop player's configuration, as an import from a PC brings it:
    /// windowed, with D2GL's locked cursor, their own fps limit, FXAA, corner
    /// mini map and fps counter.
    const IMPORTED_CONFIG: &str = r#"{
    "screen": {
        "window_size_width": 3840,
        "window_size_height": 2160,
        "window_centered": true,
        "unlock_cursor": false,
        "window_fullscreen": false,
        "foreground_fps_value": 123,
        "auto_minimize": false,
        "window_dark_mode": true
    },
    "graphics": {
        "shader_preset": "anti-aliasing\\advanced-aa.slangp",
        "fxaa": true
    },
    "features": {
        "mini_map": true,
        "show_fps": true
    }
}"#;

    #[test]
    fn the_android_host_makes_an_imported_configuration_full_screen_with_the_cursor_unlocked() {
        // With the locked cursor the first click after each start is lost on
        // the X screen, and the window is placed partly off it; the window
        // size follows the X screen.
        let (temporary, root) = managed_root(Some(IMPORTED_CONFIG.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        let expected = IMPORTED_CONFIG
            .replacen(
                "\"window_size_width\": 3840",
                "\"window_size_width\": 1280",
                1,
            )
            .replacen(
                "\"window_size_height\": 2160",
                "\"window_size_height\": 960",
                1,
            )
            .replacen("\"unlock_cursor\": false", "\"unlock_cursor\": true", 1)
            .replacen(
                "\"window_fullscreen\": false",
                "\"window_fullscreen\": true",
                1,
            );
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            expected
        );

        // Every other key stays as the player had it.
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        let original: Value = serde_json::from_str(IMPORTED_CONFIG).unwrap();
        assert_eq!(written["graphics"], original["graphics"]);
        assert_eq!(written["features"], original["features"]);
        assert_eq!(written["screen"]["foreground_fps_value"], 123);
        assert_eq!(written["screen"]["window_dark_mode"], true);
        assert!(!record_path(&temporary).exists());

        // The next Play finds nothing to change.
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Unchanged
        );
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            expected
        );
    }

    #[test]
    fn the_desktop_keeps_an_imported_configuration_as_the_player_set_it() {
        // A real X server and window manager handle a locked cursor and a
        // window, so the player's choices stand.
        let (temporary, root) = managed_root(Some(IMPORTED_CONFIG.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, false, None).unwrap(),
            DisplaySeed::Unchanged
        );
        assert_eq!(read_config(&temporary), IMPORTED_CONFIG.as_bytes());
        // The production entry decides by the host, which is the desktop here.
        assert_eq!(seed(&root, 1280, 960, None), DisplaySeed::Unchanged);
        assert_eq!(read_config(&temporary), IMPORTED_CONFIG.as_bytes());

        // D2GL's untouched default size is still seeded, and only that.
        let default_size = IMPORTED_CONFIG
            .replacen("3840", "800", 1)
            .replacen("2160", "600", 1);
        let (temporary, root) = managed_root(Some(default_size.as_bytes()));
        assert_eq!(seed(&root, 1280, 960, None), DisplaySeed::Seeded);
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            IMPORTED_CONFIG
                .replacen("3840", "1280", 1)
                .replacen("2160", "960", 1)
        );
    }

    #[test]
    fn the_android_screen_values_are_inserted_where_missing_and_stand_alone() {
        // Missing values at the start of screen, present ones in place: a
        // player's auto_minimize goes off with the game full screen.
        let (temporary, root) = managed_root(Some(
            br#"{"screen": {"window_size_width": 1280, "window_size_height": 960, "auto_minimize": true}, "other": {}}"#,
        ));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            br#"{"screen": {"unlock_cursor": true, "window_fullscreen": true, "window_size_width": 1280, "window_size_height": 960, "auto_minimize": false}, "other": {}}"#
        );

        // No screen at all: a new one with just these values.
        let (temporary, root) = managed_root(Some(br#"{"other": {}}"#));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            br#"{"screen": {"unlock_cursor": true, "window_fullscreen": true, "auto_minimize": false}, "other": {}}"#
        );

        // A window size that is not addressable: the screen values are still
        // written.
        let ambiguous = r#"{"screen": {"window_size_width": 800, "window_size_height": 600, "window_size_width": 800}}"#;
        let (temporary, root) = managed_root(Some(ambiguous.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            ambiguous.replacen(
                "\"screen\": {",
                "\"screen\": {\"unlock_cursor\": true, \"window_fullscreen\": true, \"auto_minimize\": false, ",
                1
            )
        );

        // Screen values that are not addressable (a key twice): the window
        // size is still written, and the rest stays.
        let twice = r#"{"screen": {"window_size_width": 800, "window_size_height": 600, "unlock_cursor": false, "unlock_cursor": false}}"#;
        let (temporary, root) = managed_root(Some(twice.as_bytes()));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            twice.replacen("800", "1280", 1).replacen("600", "960", 1)
        );

        // A screen that is not an object: it stays, and the profile is still
        // written.
        let (temporary, root) = managed_root(Some(br#"{"screen": null, "graphics": {}}"#));
        assert_eq!(
            seed_config(&root, 1280, 960, true, None).unwrap(),
            DisplaySeed::Unchanged
        );
        assert_eq!(
            seed_config(&root, 1280, 960, true, Some(GraphicsProfile::Performance)).unwrap(),
            DisplaySeed::Seeded
        );
        assert_eq!(
            read_config(&temporary),
            br#"{"screen": null, "graphics": {"shader_preset": "bilinear.slangp", "sharpen": false, "bloom": false}}"#
        );
    }

    #[test]
    fn the_first_launch_has_the_android_screen_values() {
        let first: Value =
            serde_json::from_slice(&first_launch_config(1280, 960, GraphicsProfile::Default))
                .unwrap();
        for (key, value) in ANDROID_SCREEN {
            assert_eq!(first["screen"][key], value, "{key}");
        }
    }

    #[test]
    fn graphics_the_player_chose_stay_without_a_profile() {
        // Only the window size follows the X screen.
        let (temporary, root) = managed_root(None);
        let mut chosen: Value =
            serde_json::from_slice(&first_launch_config(1068, 480, GraphicsProfile::Default))
                .unwrap();
        chosen["graphics"]["shader_preset"] = json!("bilinear.slangp");
        chosen["graphics"]["bloom"] = json!(false);
        fs::write(
            temporary.path().join(PROJECT_D2).join(CONFIG),
            serde_json::to_vec_pretty(&chosen).unwrap(),
        )
        .unwrap();
        assert_eq!(
            seed_config(&root, 1068, 480, true, None).unwrap(),
            DisplaySeed::Unchanged
        );
        assert_eq!(
            seed_config(&root, 1600, 720, true, None).unwrap(),
            DisplaySeed::Seeded
        );
        let written: Value = serde_json::from_slice(&read_config(&temporary)).unwrap();
        chosen["screen"]["window_size_width"] = json!(1600);
        chosen["screen"]["window_size_height"] = json!(720);
        assert_eq!(written, chosen);
        assert!(!record_path(&temporary).exists());
    }

    #[test]
    fn the_desktop_never_replaces_a_chosen_size() {
        let custom = DEFAULT_CONFIG
            .replacen("800", "1280", 1)
            .replacen("600", "960", 1);
        assert_eq!(
            seeded_config(custom.as_bytes(), 1068, 480, false).unwrap(),
            None
        );
        assert!(
            seeded_config(custom.as_bytes(), 1068, 480, true)
                .unwrap()
                .is_some()
        );
        assert_eq!(
            seeded_config(custom.as_bytes(), 1280, 960, true).unwrap(),
            None
        );
    }

    #[test]
    fn the_android_host_never_follows_a_dangling_configuration_link() {
        let (temporary, root) = managed_root(None);
        let outside = temporary.path().join("outside.json");
        symlink(&outside, temporary.path().join(PROJECT_D2).join(CONFIG)).unwrap();
        assert!(seed_config(&root, 1280, 960, true, None).is_err());
        assert!(!outside.exists());
    }

    #[test]
    fn missing_configuration_is_left_absent() {
        let (temporary, root) = managed_root(None);
        assert!(configuration_absent(&root));
        assert_eq!(
            seed(&root, 3840, 2560, None),
            DisplaySeed::ConfigurationAbsent
        );
        assert!(!temporary.path().join(PROJECT_D2).join(CONFIG).exists());
    }

    #[test]
    fn present_or_unreadable_configuration_is_not_reported_absent() {
        let (_temporary, root) = managed_root(Some(DEFAULT_CONFIG.as_bytes()));
        assert!(!configuration_absent(&root));

        let (temporary, root) = managed_root(None);
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, DEFAULT_CONFIG).unwrap();
        symlink(&outside, temporary.path().join(PROJECT_D2).join(CONFIG)).unwrap();
        assert!(!configuration_absent(&root));
    }

    #[test]
    fn invalid_or_oversized_configuration_is_skipped_untouched() {
        let mut oversized = DEFAULT_CONFIG.as_bytes().to_vec();
        oversized.resize(MAX_CONFIG_BYTES as usize + 1, b' ');
        for config in [b"{ not json".to_vec(), oversized] {
            let (temporary, root) = managed_root(Some(&config));
            assert_eq!(seed(&root, 3840, 2560, None), DisplaySeed::Skipped);
            assert_eq!(read_config(&temporary), config);
        }
    }

    #[test]
    fn symlinked_configuration_is_skipped_without_following() {
        let (temporary, root) = managed_root(None);
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, DEFAULT_CONFIG).unwrap();
        symlink(&outside, temporary.path().join(PROJECT_D2).join(CONFIG)).unwrap();

        assert_eq!(seed(&root, 3840, 2560, None), DisplaySeed::Skipped);
        assert_eq!(fs::read_to_string(outside).unwrap(), DEFAULT_CONFIG);
    }

    /// The `other` section as PD2's D2GL writes it.
    const PD2_OTHER: &str = r#""other": {
        "gl_ver_major": 4,
        "gl_ver_minor": 6,
        "use_compute_shader": false,
        "frame_latency": 1,
        "load_dlls_early": "",
        "load_dlls_late": "SGD2FreeRes.dll"
    }"#;

    fn pd2_config(other: &str) -> String {
        format!(
            "{{\n    \"screen\": {{\n        \"window_size_width\": 1068,\n        \"unlock_cursor\": true\n    }},\n    {other}\n}}"
        )
    }

    #[test]
    fn the_dlls_are_set_to_pd2s_own_and_every_other_byte_stays() {
        // Another app pointed both at files it dropped in the game folder;
        // D2GL loads a PE file whatever its name.
        let planted = PD2_OTHER
            .replacen(
                "\"load_dlls_early\": \"\"",
                "\"load_dlls_early\": \"Save/notes.txt\"",
                1,
            )
            .replacen(
                "\"SGD2FreeRes.dll\"",
                "\"SGD2FreeRes.dll, filters/x.png:stdcall:_Init@0\"",
                1,
            );
        let (temporary, root) = managed_root(Some(pd2_config(&planted).as_bytes()));
        pin_dlls(&root).unwrap();
        assert_eq!(
            String::from_utf8(read_config(&temporary)).unwrap(),
            pd2_config(PD2_OTHER)
        );

        // Missing values go at the start of `other`, a missing `other` at
        // the start of the configuration.
        for (config, pinned) in [
            (
                r#"{"other": {"frame_latency": 2}}"#,
                r#"{"other": {"load_dlls_early": "", "load_dlls_late": "SGD2FreeRes.dll", "frame_latency": 2}}"#,
            ),
            (
                r#"{"screen": {}}"#,
                r#"{"other": {"load_dlls_early": "", "load_dlls_late": "SGD2FreeRes.dll"}, "screen": {}}"#,
            ),
        ] {
            let (temporary, root) = managed_root(Some(config.as_bytes()));
            pin_dlls(&root).unwrap();
            assert_eq!(read_config(&temporary), pinned.as_bytes());
        }

        // A byte order mark stays.
        let (temporary, root) = managed_root(Some(b"\xEF\xBB\xBF{\"other\": {}}"));
        pin_dlls(&root).unwrap();
        assert_eq!(
            read_config(&temporary),
            b"\xEF\xBB\xBF{\"other\": {\"load_dlls_early\": \"\", \"load_dlls_late\": \"SGD2FreeRes.dll\"}}"
        );
    }

    #[test]
    fn pinned_dlls_are_written_anew_so_an_earlier_descriptor_misses_them() {
        let (temporary, root) = managed_root(Some(pd2_config(PD2_OTHER).as_bytes()));
        let path = temporary.path().join(PROJECT_D2).join(CONFIG);
        let held = fs::OpenOptions::new().write(true).open(&path).unwrap();
        let before = fs::metadata(&path).unwrap().ino();

        pin_dlls(&root).unwrap();
        assert_ne!(fs::metadata(&path).unwrap().ino(), before);
        (&held)
            .write_all(br#"{"other": {"load_dlls_late": "Save/notes.txt"}}"#)
            .unwrap();
        assert_eq!(read_config(&temporary), pd2_config(PD2_OTHER).as_bytes());
        assert!(!temporary.path().join(PROJECT_D2).join(STAGED).exists());
    }

    #[test]
    fn a_configuration_whose_dlls_cannot_be_set_becomes_its_backup_with_pd2s_dlls() {
        // D2GL loads its backup in place of a configuration it cannot parse.
        let mut oversized = pd2_config(PD2_OTHER).into_bytes();
        oversized.resize(MAX_CONFIG_BYTES as usize + 1, b' ');
        let unpinnable = [
            b"{ not json".to_vec(),
            oversized,
            br#"{"other": {"load_dlls_late": "SGD2FreeRes.dll", "load_dlls_late": "Save/notes.txt"}}"#.to_vec(),
            br#"{"other": {"load_dlls_lat\u0065": "Save/notes.txt"}}"#.to_vec(),
            br#"{"other": {}, "\u006fther": {"load_dlls_early": "Save/notes.txt"}}"#.to_vec(),
            br#"{"other": "load_dlls_late"}"#.to_vec(),
            br#"{"other": null}"#.to_vec(),
            b"[]".to_vec(),
        ];
        let planted_backup =
            pd2_config(&PD2_OTHER.replacen("\"SGD2FreeRes.dll\"", "\"Save/notes.txt\"", 1));
        for config in &unpinnable {
            let (temporary, root) = managed_root(Some(config));
            fs::write(
                temporary.path().join(PROJECT_D2).join(BACKUP),
                &planted_backup,
            )
            .unwrap();
            pin_dlls(&root).unwrap();
            assert_eq!(
                String::from_utf8(read_config(&temporary)).unwrap(),
                pd2_config(PD2_OTHER),
                "{}",
                String::from_utf8_lossy(config)
            );
        }

        // Without a backup that can be edited it is removed, and D2GL starts
        // from its defaults (PD2's DLLs).
        for backup in [None, Some(&b"{ not json"[..])] {
            let (temporary, root) = managed_root(Some(b"{ not json"));
            if let Some(backup) = backup {
                fs::write(temporary.path().join(PROJECT_D2).join(BACKUP), backup).unwrap();
            }
            pin_dlls(&root).unwrap();
            assert!(!temporary.path().join(PROJECT_D2).join(CONFIG).exists());
        }

        // Repeated keys whose last is PD2's: what D2GL reads is already
        // right, and the file stays as it is.
        let repeated = br#"{"other": {"load_dlls_late": "Save/notes.txt", "load_dlls_late": "SGD2FreeRes.dll", "load_dlls_early": ""}}"#;
        let (temporary, root) = managed_root(Some(repeated));
        pin_dlls(&root).unwrap();
        assert_eq!(read_config(&temporary), repeated);
    }

    #[test]
    fn a_missing_configuration_stays_missing_and_one_that_is_no_file_stops_the_pin() {
        let (temporary, root) = managed_root(None);
        pin_dlls(&root).unwrap();
        assert!(!temporary.path().join(PROJECT_D2).join(CONFIG).exists());

        // D2GL cannot write a configuration over a folder and loads its
        // backup instead.
        let (temporary, root) = managed_root(None);
        fs::create_dir(temporary.path().join(PROJECT_D2).join(CONFIG)).unwrap();
        assert!(pin_dlls(&root).is_err());

        let (temporary, root) = managed_root(None);
        let outside = temporary.path().join("outside.json");
        fs::write(&outside, DEFAULT_CONFIG).unwrap();
        symlink(&outside, temporary.path().join(PROJECT_D2).join(CONFIG)).unwrap();
        assert!(pin_dlls(&root).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), DEFAULT_CONFIG);
    }

    #[test]
    fn other_spellings_of_the_configuration_are_removed() {
        // Wine opens a name that differs only in case when d2gl.json itself
        // is missing (`ſ` is a lowercase s).
        let (temporary, root) = managed_root(None);
        let project = temporary.path().join(PROJECT_D2);
        for name in ["D2GL.json", "d2gl.JSON", "d2gl.jſon"] {
            fs::write(
                project.join(name),
                br#"{"other": {"load_dlls_late": "Save/notes.txt"}}"#,
            )
            .unwrap();
        }
        for name in [BACKUP, "d2gl.jsonx", "d2gl.jßon"] {
            fs::write(project.join(name), b"{}").unwrap();
        }
        pin_dlls(&root).unwrap();
        let mut names = fs::read_dir(&project)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(names, [BACKUP, "d2gl.jsonx", "d2gl.jßon"]);

        // A folder among them stops the pin.
        fs::create_dir(project.join("D2GL.JSON")).unwrap();
        assert!(pin_dlls(&root).is_err());
    }

    #[test]
    fn the_first_launch_has_the_dlls_pd2_ships() {
        let first: Value =
            serde_json::from_slice(&first_launch_config(1280, 960, GraphicsProfile::Default))
                .unwrap();
        assert_eq!(first["other"]["load_dlls_early"], "");
        assert_eq!(first["other"]["load_dlls_late"], "SGD2FreeRes.dll");
        let pinned = first_launch_config(1280, 960, GraphicsProfile::Default);
        assert_eq!(set_values(&pinned, "other", &shipped_dlls()).unwrap(), None);
    }
}
