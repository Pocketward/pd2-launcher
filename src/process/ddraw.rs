//! Chooses how PD2's DirectDraw wrapper (cnc-ddraw) draws, before a
//! DirectDraw launch.
//!
//! PD2 ships `ProjectD2/ddraw.ini` with `renderer=opengl`. Where OpenGL
//! inside Wine shows nothing (DroidDeck, whose Windows games draw through
//! Vulkan), `direct3d9` (DXVK, Vulkan) or `gdi` (software) work instead. The
//! file is the player's (updates preserve it); only its `renderer=` line in
//! the `[ddraw]` section is changed, every failure skips the change, and the
//! launch continues.

use std::{
    ffi::OsStr,
    io::{self, Read as _},
    path::Path,
};

use crate::safe_fs::Directory;

const PROJECT_D2: &str = "ProjectD2";
const CONFIG: &str = "ddraw.ini";
const MAX_CONFIG_BYTES: u64 = 256 * 1024;

/// Set cnc-ddraw's renderer to `renderer` (`opengl`, `direct3d9`, `gdi`).
/// Whether the file changed.
pub(crate) fn set_renderer(folder: &Directory, renderer: &str) -> io::Result<bool> {
    let project = folder.open_directory(Path::new(PROJECT_D2))?;
    let mut text = String::new();
    project
        .open_file(Path::new(CONFIG))?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ddraw.ini is too large",
        ));
    }
    let Some(updated) = with_renderer(&text, renderer) else {
        return Ok(false);
    };
    project.atomic_write(OsStr::new(CONFIG), updated.as_bytes())?;
    Ok(true)
}

/// `text` with the `[ddraw]` section's `renderer=` set to `renderer`, or
/// `None` when it already is (or there is no `[ddraw]` section).
fn with_renderer(text: &str, renderer: &str) -> Option<String> {
    let mut section = String::new();
    let mut lines = Vec::new();
    let mut changed = false;
    let mut found = false;
    let mut header = None;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            section = trimmed[1..trimmed.len() - 1].trim().to_ascii_lowercase();
            if section == "ddraw" {
                header = Some(index);
            }
        } else if section == "ddraw"
            && !found
            && let Some((key, value)) = trimmed.split_once('=')
            && key.trim().eq_ignore_ascii_case("renderer")
        {
            found = true;
            if !value.trim().eq_ignore_ascii_case(renderer) {
                let ending = &line[line.trim_end_matches(['\r', '\n']).len()..];
                lines.push(format!("renderer={renderer}{ending}"));
                changed = true;
                continue;
            }
        }
        lines.push(line.to_owned());
    }
    if !found {
        let header = header?;
        let ending = if text.contains("\r\n") { "\r\n" } else { "\n" };
        lines.insert(header + 1, format!("renderer={renderer}{ending}"));
        changed = true;
    }
    changed.then(|| lines.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_ddraw_sections_renderer_line_changes() {
        let text = "; cnc-ddraw\r\n[ddraw]\r\nwidth=0\r\nrenderer=opengl\r\nvsync=false\r\n[Game.exe]\r\nrenderer=gdi\r\n";
        assert_eq!(
            with_renderer(text, "direct3d9").unwrap(),
            "; cnc-ddraw\r\n[ddraw]\r\nwidth=0\r\nrenderer=direct3d9\r\nvsync=false\r\n[Game.exe]\r\nrenderer=gdi\r\n"
        );
        assert_eq!(with_renderer(text, "OpenGL"), None, "already set");
        assert_eq!(
            with_renderer("[ddraw]\nwidth=0\n", "gdi").unwrap(),
            "[ddraw]\nrenderer=gdi\nwidth=0\n"
        );
        assert_eq!(
            with_renderer("width=0\n", "gdi"),
            None,
            "no [ddraw] section"
        );
    }

    #[test]
    fn the_file_in_projectd2_is_rewritten_in_place() {
        let fixture = tempfile::tempdir().unwrap();
        std::fs::create_dir(fixture.path().join(PROJECT_D2)).unwrap();
        let config = fixture.path().join(PROJECT_D2).join(CONFIG);
        std::fs::write(&config, "[ddraw]\nrenderer=opengl\n").unwrap();
        let folder = Directory::open(fixture.path()).unwrap();
        assert!(set_renderer(&folder, "direct3d9").unwrap());
        assert_eq!(
            std::fs::read_to_string(&config).unwrap(),
            "[ddraw]\nrenderer=direct3d9\n"
        );
        assert!(!set_renderer(&folder, "direct3d9").unwrap());
    }
}
