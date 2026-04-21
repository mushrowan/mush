//! desktop notifications with optional sound

use std::process::{Command, Stdio};

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
            // message.oga is a soft mail-like ping. complete.oga (big chime)
            // is meant for the end of long batch tasks and gets grating as
            // a per-turn completion alert
            Sound::Complete => "message.oga",
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
    spawn_detached(cmd);

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

    let mut cmd = Command::new("pw-play");
    cmd.arg(&path);
    spawn_detached(cmd);
}

/// spawn a command fire-and-forget. redirects stdio to /dev/null and
/// reaps the child in a detached thread so it doesn't accumulate as a
/// `<defunct>` zombie while mush keeps running.
fn spawn_detached(mut cmd: Command) {
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return;
    };
    // reaper thread: just wait for the child to exit and drop it.
    // very lightweight since the child is always a short-lived helper
    // like notify-send or pw-play
    let _ = std::thread::Builder::new()
        .name("mush-notify-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complete_sound_is_softer_than_default_complete() {
        // `complete.oga` in the freedesktop sound theme is a chime meant
        // for the end of a long batch task. used as our per-turn completion
        // alert it gets grating fast, so we pick a softer sound
        assert_eq!(Sound::Complete.filename(), "message.oga");
    }

    #[test]
    fn attention_and_error_sounds_use_dialog_theme() {
        // these are the conventional freedesktop dialog sounds and stay put
        assert_eq!(Sound::Attention.filename(), "dialog-information.oga");
        assert_eq!(Sound::Error.filename(), "dialog-warning.oga");
    }

    // spawn_detached must reap children on linux so we don't accumulate
    // <defunct> entries in the process table for every notify-send or pw-play
    // we fire off. without this we accumulate hundreds of zombies over a
    // long-running mush process, one pair per agent turn.
    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_detached_does_not_leave_zombies() {
        // spawn several short-lived children via the helper
        for _ in 0..5 {
            let cmd = Command::new("true");
            spawn_detached(cmd);
        }

        // give children time to exit and our reaper to run
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let zombies = zombie_child_count(std::process::id());
            if zombies == 0 {
                return;
            }
            if std::time::Instant::now() >= deadline {
                panic!("expected no zombie children, found {zombies}");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    #[cfg(target_os = "linux")]
    fn zombie_child_count(parent: u32) -> usize {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return 0;
        };
        let mut count = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.parse::<u32>().is_err() {
                continue;
            }
            let stat_path = format!("/proc/{name}/stat");
            let Ok(stat) = std::fs::read_to_string(&stat_path) else {
                continue;
            };
            // /proc/<pid>/stat: "pid (comm) state ppid ..."
            // comm can contain spaces or parens so we split on the last ')'
            let Some(rparen) = stat.rfind(')') else {
                continue;
            };
            let rest = stat[rparen + 1..].trim_start();
            let mut fields = rest.split_ascii_whitespace();
            let state = fields.next().unwrap_or("");
            let ppid: u32 = fields.next().and_then(|s| s.parse().ok()).unwrap_or(0);
            if ppid == parent && state == "Z" {
                count += 1;
            }
        }
        count
    }
}
