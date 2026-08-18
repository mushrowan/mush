use std::io::Write;
use std::process::Command;

use mush_ai::types::ImageMimeType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment {
    pub ssh: bool,
    pub tool: Option<String>,
}

impl Environment {
    #[must_use]
    pub fn detect() -> Self {
        Self {
            ssh: std::env::var_os("SSH_CONNECTION").is_some()
                || std::env::var_os("SSH_TTY").is_some(),
            tool: ["wl-copy", "xclip", "pbcopy"]
                .into_iter()
                .find(|tool| on_path(tool))
                .map(str::to_string),
        }
    }
}

fn on_path(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
}

#[derive(Debug, thiserror::Error)]
pub enum CopyError {
    #[error("clipboard failed: {0}")]
    Command(String),
    #[error("clipboard failed: {0}")]
    Terminal(#[from] std::io::Error),
}

pub fn copy(text: &str, env: &Environment, out: &mut impl Write) -> Result<(), CopyError> {
    let _ = env;
    write_osc52(text, out)
}

fn write_osc52(text: &str, out: &mut impl Write) -> Result<(), CopyError> {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(())
}

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

fn select_mime(types: &[String]) -> Option<(String, Option<ImageMimeType>)> {
    for &(mime_str, mime_type) in SUPPORTED_MIMES {
        if types.iter().any(|t| t.trim() == mime_str) {
            return Some((mime_str.to_string(), Some(mime_type)));
        }
    }
    types
        .iter()
        .map(|t| t.trim())
        .find(|t| t.starts_with("image/"))
        .map(|t| (t.to_string(), None))
}

fn finish(bytes: Vec<u8>, api_mime: Option<ImageMimeType>) -> Option<ClipboardImage> {
    match api_mime {
        Some(mime_type) => Some(ClipboardImage { bytes, mime_type }),
        None => transcode_to_png(&bytes),
    }
}

fn transcode_to_png(bytes: &[u8]) -> Option<ClipboardImage> {
    let img = image::load_from_memory(bytes).ok()?;
    let mut out = std::io::Cursor::new(Vec::new());
    img.write_to(&mut out, image::ImageFormat::Png).ok()?;
    let bytes = out.into_inner();
    if bytes.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        bytes,
        mime_type: ImageMimeType::Png,
    })
}

fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|v| v == "wayland")
            .unwrap_or(false)
}

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

    let (wire, api_mime) = select_mime(&types)?;

    let data = Command::new("wl-paste")
        .args(["--type", &wire, "--no-newline"])
        .output()
        .ok()?;
    if !data.status.success() || data.stdout.is_empty() {
        return None;
    }

    finish(data.stdout, api_mime)
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

    let (wire, api_mime) = if !types.is_empty() {
        select_mime(&types)?
    } else {
        ("image/png".to_string(), Some(ImageMimeType::Png))
    };

    let data = Command::new("xclip")
        .args(["-selection", "clipboard", "-t", &wire, "-o"])
        .output()
        .ok()?;
    if !data.status.success() || data.stdout.is_empty() {
        return None;
    }

    finish(data.stdout, api_mime)
}

fn read_macos() -> Option<ClipboardImage> {
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
        let (wire, api_mime) = select_mime(&types).unwrap();
        assert_eq!(wire, "image/png");
        assert_eq!(api_mime, Some(ImageMimeType::Png));
    }

    #[test]
    fn select_mime_no_image_returns_none() {
        let types = vec!["text/plain".into(), "text/html".into()];
        assert!(select_mime(&types).is_none());
    }

    #[test]
    fn select_mime_bmp_requests_real_type_and_transcodes() {
        let types = vec!["text/plain".into(), "image/bmp".into()];
        let (wire, api_mime) = select_mime(&types).unwrap();
        assert_eq!(wire, "image/bmp");
        assert_eq!(api_mime, None);
    }

    #[test]
    fn transcode_bmp_to_png() {
        let bmp: &[u8] = &[
            0x42, 0x4d, 0x3a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x36, 0x00, 0x00, 0x00,
            0x28, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x18, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00,
            0x00, 0x00, 0x00, 0xff, 0x00, 0x00,
        ];

        assert_transcodes_to_blue_red(bmp);
    }

    #[test]
    fn transcode_windows_clipboard_bmp_to_png() {
        let bmp: &[u8] = &[
            0x42, 0x4d, 0x4a, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x42, 0x00, 0x00, 0x00,
            0x28, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x20, 0x00, 0x03, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0xff, 0x00, 0x00, 0xff, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00,
            0x00, 0x00, 0xff, 0x00,
        ];

        assert_transcodes_to_blue_red(bmp);
    }

    fn assert_transcodes_to_blue_red(bmp: &[u8]) {
        let out = transcode_to_png(bmp).expect("bmp should transcode");
        assert_eq!(out.mime_type, ImageMimeType::Png);
        assert_eq!(
            &out.bytes[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "png magic",
        );

        let decoded = image::load_from_memory(&out.bytes).unwrap().to_rgb8();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [0, 0, 255], "left pixel blue");
        assert_eq!(decoded.get_pixel(1, 0).0, [255, 0, 0], "right pixel red");
    }
}

#[cfg(test)]
mod copy_tests {
    use super::*;

    #[test]
    fn on_ssh_the_text_goes_out_as_osc_52() {
        let env = Environment {
            ssh: true,
            tool: Some("wl-copy".into()),
        };
        let mut out = Vec::new();

        copy("world", &env, &mut out).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "\x1b]52;c;d29ybGQ=\x07");
    }
}
