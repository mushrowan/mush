use std::process::{Command, Stdio};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sound {
    Complete,
    Attention,
    Error,
}

impl Sound {
    fn filename(self) -> &'static str {
        match self {
            Sound::Complete => "message.oga",
            Sound::Attention => "dialog-information.oga",
            Sound::Error => "dialog-warning.oga",
        }
    }
}

pub fn send(summary: &str, body: &str) {
    send_with_sound(summary, body, None);
}

pub fn send_with_sound(summary: &str, body: &str, sound: Option<Sound>) {
    let mut cmd = Command::new("notify-send");
    cmd.arg("--app-name=mush").arg(summary).arg(body);

    if let Some(sound) = sound {
        let sound_path = format!(
            "/run/current-system/sw/share/sounds/freedesktop/stereo/{}",
            sound.filename()
        );
        if std::path::Path::new(&sound_path).exists() {
            cmd.arg(format!("--hint=string:sound-file:{sound_path}"));
        }
    }

    spawn_detached(cmd);

    if let Some(sound) = sound {
        play_sound(sound);
    }
}

pub fn play(sound: Sound) {
    play_sound(sound);
}

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

fn spawn_detached(mut cmd: Command) {
    let spawned = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return;
    };
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
        assert_eq!(Sound::Complete.filename(), "message.oga");
    }

    #[test]
    fn attention_and_error_sounds_use_dialog_theme() {
        assert_eq!(Sound::Attention.filename(), "dialog-information.oga");
        assert_eq!(Sound::Error.filename(), "dialog-warning.oga");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn spawn_detached_does_not_leave_zombies() {
        for _ in 0..5 {
            let cmd = Command::new("true");
            spawn_detached(cmd);
        }

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
