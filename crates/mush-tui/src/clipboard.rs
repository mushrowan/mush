//! clipboard image reading
//!
//! reads image data from the system clipboard via wl-paste (wayland),
//! xclip (x11), or osascript (macos). formats the anthropic api does not
//! accept directly (e.g. the image/bmp a wslg bridge exposes for a windows
//! clipboard image) are transcoded to png before use.

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

/// pick the clipboard type to fetch.
///
/// returns the exact wire mime string to request from wl-paste / xclip,
/// paired with the api mime type when it is directly supported. a `None`
/// api type means the format (e.g. bmp from a wslg/windows clipboard) must
/// be transcoded to png before it can be sent.
fn select_mime(types: &[String]) -> Option<(String, Option<ImageMimeType>)> {
    for &(mime_str, mime_type) in SUPPORTED_MIMES {
        if types.iter().any(|t| t.trim() == mime_str) {
            return Some((mime_str.to_string(), Some(mime_type)));
        }
    }
    // any other image/* type: fetch it under its real name and transcode
    types
        .iter()
        .map(|t| t.trim())
        .find(|t| t.starts_with("image/"))
        .map(|t| (t.to_string(), None))
}

/// build a clipboard image, transcoding to png when the source format is
/// not one the api accepts directly.
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
        // no TARGETS advertised: try png directly
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
        // wslg exposes a windows clipboard image only as image/bmp. we must
        // request it under its real name, not a hardcoded image/png, and flag
        // it for transcoding.
        let types = vec!["text/plain".into(), "image/bmp".into()];
        let (wire, api_mime) = select_mime(&types).unwrap();
        assert_eq!(wire, "image/bmp");
        assert_eq!(api_mime, None);
    }

    #[test]
    fn transcode_bmp_to_png() {
        // a 2x1 24-bit uncompressed bmp (blue then red pixel, bgr, bottom-up)
        let bmp: &[u8] = &[
            0x42, 0x4d, // "BM"
            0x3a, 0x00, 0x00, 0x00, // file size (58)
            0x00, 0x00, 0x00, 0x00, // reserved
            0x36, 0x00, 0x00, 0x00, // pixel data offset (54)
            0x28, 0x00, 0x00, 0x00, // DIB header size (40)
            0x02, 0x00, 0x00, 0x00, // width 2
            0x01, 0x00, 0x00, 0x00, // height 1
            0x01, 0x00, // planes
            0x18, 0x00, // 24 bpp
            0x00, 0x00, 0x00, 0x00, // BI_RGB (no compression)
            0x04, 0x00, 0x00, 0x00, // image size (padded row = 4*? -> 8 here)
            0x00, 0x00, 0x00, 0x00, // x ppm
            0x00, 0x00, 0x00, 0x00, // y ppm
            0x00, 0x00, 0x00, 0x00, // colours used
            0x00, 0x00, 0x00, 0x00, // important colours
            // pixel row (bgr), padded to 4-byte boundary
            0xff, 0x00, 0x00, // blue
            0x00, 0x00, 0xff, // red
            0x00, 0x00, // padding
        ];

        assert_transcodes_to_blue_red(bmp);
    }

    #[test]
    fn transcode_windows_clipboard_bmp_to_png() {
        // the exact shape wslg puts on the wayland clipboard for a windows
        // clipboard image: 32-bit, BI_BITFIELDS (compression 3), a plain
        // 40-byte BITMAPINFOHEADER followed by three rgb colour masks, so
        // the pixel data starts at offset 66. mirrors a real capture.
        let bmp: &[u8] = &[
            0x42, 0x4d, // "BM"
            0x4a, 0x00, 0x00, 0x00, // file size (74)
            0x00, 0x00, 0x00, 0x00, // reserved
            0x42, 0x00, 0x00, 0x00, // pixel data offset (66)
            0x28, 0x00, 0x00, 0x00, // DIB header size (40)
            0x02, 0x00, 0x00, 0x00, // width 2
            0x01, 0x00, 0x00, 0x00, // height 1
            0x01, 0x00, // planes
            0x20, 0x00, // 32 bpp
            0x03, 0x00, 0x00, 0x00, // BI_BITFIELDS
            0x08, 0x00, 0x00, 0x00, // image size (8)
            0x00, 0x00, 0x00, 0x00, // x ppm
            0x00, 0x00, 0x00, 0x00, // y ppm
            0x00, 0x00, 0x00, 0x00, // colours used
            0x00, 0x00, 0x00, 0x00, // important colours
            0x00, 0x00, 0xff, 0x00, // red mask   0x00ff0000
            0x00, 0xff, 0x00, 0x00, // green mask 0x0000ff00
            0xff, 0x00, 0x00, 0x00, // blue mask  0x000000ff
            // pixel row, bgrx per the masks above
            0xff, 0x00, 0x00, 0x00, // blue
            0x00, 0x00, 0xff, 0x00, // red
        ];

        assert_transcodes_to_blue_red(bmp);
    }

    /// decode a transcoded 2x1 bmp back and confirm png output + pixels
    /// (blue at x=0, red at x=1).
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
