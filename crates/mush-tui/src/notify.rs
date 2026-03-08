//! desktop notifications with optional sound

use std::process::Command;

/// which sound to play with a notification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    /// task complete (agent finished)
    Complete,
    /// something needs attention (confirmation, cache expiry)
    Attention,
    /// error occurred
    Error,
}

impl Sound {
    /// freedesktop sound theme filename
    fn filename(self) -> &'static str {
        match self {
            Sound::Complete => "complete.oga",
            Sound::Attention => "dialog-information.oga",
            Sound::Error => "dialog-warning.oga",
        }
    }
}

/// send a desktop notification (best-effort, failures are silent)
pub fn send(summary: &str, body: &str) {
    send_with_sound(summary, body, None);
}

/// send a desktop notification with an optional sound
pub fn send_with_sound(summary: &str, body: &str, sound: Option<Sound>) {
    // try notify-send first (works with most notification daemons)
    let mut cmd = Command::new("notify-send");
    cmd.arg("--app-name=mush").arg(summary).arg(body);

    if let Some(sound) = sound {
        // pass sound-file hint per freedesktop notification spec
        let sound_path = format!(
            "/run/current-system/sw/share/sounds/freedesktop/stereo/{}",
            sound.filename()
        );
        if std::path::Path::new(&sound_path).exists() {
            cmd.arg(format!("--hint=string:sound-file:{sound_path}"));
        }
    }

    // fire and forget
    let _ = cmd
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

    // also play sound directly via pw-play as fallback
    // (some notification daemons ignore the sound-file hint)
    if let Some(sound) = sound {
        play_sound(sound);
    }
}

/// play a sound without showing a notification
pub fn play(sound: Sound) {
    play_sound(sound);
}

/// play a freedesktop sound via pipewire (best-effort)
fn play_sound(sound: Sound) {
    let path = format!(
        "/run/current-system/sw/share/sounds/freedesktop/stereo/{}",
        sound.filename()
    );
    if !std::path::Path::new(&path).exists() {
        return;
    }

    let _ = Command::new("pw-play")
        .arg(&path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
