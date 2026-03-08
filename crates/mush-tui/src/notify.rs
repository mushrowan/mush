//! desktop notifications for cache expiry and other alerts

use std::process::Command;

/// send a desktop notification (best-effort, failures are silent)
pub fn send(summary: &str, body: &str) {
    // try platform-specific tools in order
    let tools: &[(&str, &[&str])] = &[
        ("notify-send", &["-a", "mush", summary, body]),
        (
            "osascript",
            &[
                "-e",
                &format!(
                    "display notification \"{}\" with title \"mush\" subtitle \"{}\"",
                    body, summary
                ),
            ],
        ),
        (
            "terminal-notifier",
            &["-title", "mush", "-subtitle", summary, "-message", body],
        ),
    ];

    for (cmd, args) in tools {
        if Command::new(cmd)
            .args(*args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .is_ok()
        {
            return;
        }
    }
}
