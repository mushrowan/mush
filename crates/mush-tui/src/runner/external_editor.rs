use crate::app::App;
use crate::terminal_policy::TerminalPolicy;

pub(super) fn edit(app: &mut App, policy: TerminalPolicy) {
    let Some(command) = editor_command() else {
        app.push_system_message(
            "No editor configured. Set $VISUAL or $EDITOR environment variable.",
        );
        return;
    };
    let path = std::env::temp_dir().join(format!(
        "mush-editor-{}.md",
        mush_ai::types::Timestamp::now().as_ms()
    ));
    if let Err(error) = std::fs::write(&path, &app.input.text) {
        app.push_system_message(format!("could not write {}: {error}", path.display()));
        return;
    }

    let mut words = command.split_whitespace();
    let program = words.next().unwrap_or_default().to_string();
    let args: Vec<&str> = words.collect();

    super::terminal::restore_terminal_state();
    let status = std::process::Command::new(&program)
        .args(&args)
        .arg(&path)
        .status();
    if let Err(error) = super::terminal::enter_tui_terminal(policy) {
        tracing::warn!(%error, "could not re-enter the tui after the external editor");
    }
    app.render_state.force_clear.set(true);

    match status {
        Ok(status) if status.success() => match std::fs::read_to_string(&path) {
            Ok(text) => app.set_input_text(strip_final_newline(text)),
            Err(error) => {
                app.push_system_message(format!("could not read {}: {error}", path.display()));
            }
        },
        Ok(_) => {}
        Err(error) => app.push_system_message(format!("could not start {program}: {error}")),
    }
    let _ = std::fs::remove_file(&path);
}

fn editor_command() -> Option<String> {
    ["VISUAL", "EDITOR"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.trim().is_empty())
}

fn strip_final_newline(mut text: String) -> String {
    if text.ends_with('\n') {
        text.pop();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_trailing_newline_is_dropped() {
        assert_eq!(strip_final_newline("draft\n".into()), "draft");
        assert_eq!(strip_final_newline("draft\n\n".into()), "draft\n");
        assert_eq!(strip_final_newline("draft".into()), "draft");
    }
}
