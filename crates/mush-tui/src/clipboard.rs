//! clipboard image reading
//!
//! reads image data from the system clipboard via wl-paste (wayland),
//! xclip (x11), or osascript (macos)

use std::process::Command;

use mush_ai::types::ImageMimeType;

/// raw clipboard image
#[derive(Debug, Clone)]
pub struct ClipboardImage {
    pub bytes: Vec<u8>,
    pub mime_type: ImageMimeType,
}

const SUPPORTED_MIMES: &[(&str, ImageMimeType)] = &[
    ("image/png", ImageMimeType::Png),
    ("image/jpeg", ImageMimeType::Jpeg),
    ("image/webp", ImageMimeType::Webp),
    ("image/gif", ImageMimeType::Gif),
];

fn select_mime(types: &[String]) -> Option<(&'static str, ImageMimeType)> {
    for &(mime_str, ref mime_type) in SUPPORTED_MIMES {
        if types.iter().any(|t| t.trim() == mime_str) {
            return Some((mime_str, *mime_type));
        }
    }
    // any image/* as fallback
    if types.iter().any(|t| t.trim().starts_with("image/")) {
        // default to png for unknown image types
        return Some(("image/png", ImageMimeType::Png));
    }
    None
}

fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v == "wayland")
            .unwrap_or(false)
}

/// read image from clipboard (blocking, should be called from spawn_blocking)
pub fn read_clipboard_image() -> Option<ClipboardImage> {
    if cfg!(target_os = "macos") {
        read_macos()
    } else if is_wayland() {
        read_wl_paste().or_else(read_xclip)
    } else {
        read_xclip().or_else(read_wl_paste)
    }
}

fn read_wl_paste() -> Option<ClipboardImage> {
    let list = Command::new("wl-paste")
        .args(["--list-types"])
        .output()
        .ok()?;
    if !list.status.success() {
        return None;
    }

    let types: Vec<String> = String::from_utf8_lossy(&list.stdout)
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();

    let (mime_str, mime_type) = select_mime(&types)?;

    let data = Command::new("wl-paste")
        .args(["--type", mime_str, "--no-newline"])
        .output()
        .ok()?;
    if !data.status.success() || data.stdout.is_empty() {
        return None;
    }

    Some(ClipboardImage {
        bytes: data.stdout,
        mime_type,
    })
}

fn read_xclip() -> Option<ClipboardImage> {
    let targets = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "TARGETS", "-o"])
        .output()
        .ok()?;

    let types: Vec<String> = if targets.status.success() {
        String::from_utf8_lossy(&targets.stdout)
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    } else {
        vec![]
    };

    let (mime_str, mime_type) = if !types.is_empty() {
        select_mime(&types)?
    } else {
        // try png directly
        ("image/png", ImageMimeType::Png)
    };

    let data = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", mime_str, "-o"])
        .output()
        .ok()?;
    if !data.status.success() || data.stdout.is_empty() {
        return None;
    }

    Some(ClipboardImage {
        bytes: data.stdout,
        mime_type,
    })
}

fn read_macos() -> Option<ClipboardImage> {
    // use osascript to check if clipboard has image, then pbpaste
    let check = Command::new("osascript")
        .args([
            "-e",
            "try\n\
             set theClip to the clipboard as «class PNGf»\n\
             return \"png\"\n\
             on error\n\
             return \"none\"\n\
             end try",
        ])
        .output()
        .ok()?;

    let result = String::from_utf8_lossy(&check.stdout).trim().to_string();
    if result != "png" {
        return None;
    }

    // write clipboard image to a temp file and read it back
    let tmp = std::env::temp_dir().join(format!("mush-clip-{}.png", std::process::id()));
    let script = format!(
        "set theFile to POSIX file \"{}\"\n\
         set theClip to the clipboard as «class PNGf»\n\
         set fileRef to open for access theFile with write permission\n\
         write theClip to fileRef\n\
         close access fileRef",
        tmp.display()
    );

    let write = Command::new("osascript")
        .args(["-e", &script])
        .output()
        .ok()?;

    if !write.status.success() {
        return None;
    }

    let bytes = std::fs::read(&tmp).ok()?;
    let _ = std::fs::remove_file(&tmp);

    if bytes.is_empty() {
        return None;
    }

    Some(ClipboardImage {
        bytes,
        mime_type: ImageMimeType::Png,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_mime_prefers_png() {
        let types = vec!["text/plain".into(), "image/png".into(), "image/jpeg".into()];
        let (mime, _) = select_mime(&types).unwrap();
        assert_eq!(mime, "image/png");
    }

    #[test]
    fn select_mime_no_image_returns_none() {
        let types = vec!["text/plain".into(), "text/html".into()];
        assert!(select_mime(&types).is_none());
    }
}
