use std::path::Path;
use std::process::{Command, Output};

fn mush(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mush"));
    command
        .env_clear()
        .env("HOME", home)
        .env("MUSH_DATA_DIR", home.join("data"))
        .env("MUSH_CONFIG_DIR", home.join("config"))
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .current_dir(home);
    command
}

#[expect(clippy::unwrap_used)]
fn stdout(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[expect(clippy::unwrap_used)]
fn untouched(home: &Path) -> bool {
    std::fs::read_dir(home).unwrap().next().is_none()
}

#[expect(clippy::unwrap_used)]
fn script(shell: &str) -> String {
    let home = crate::tempdir().unwrap();
    let script = stdout(
        mush(home.path())
            .args(["completions", shell])
            .output()
            .unwrap(),
    );
    assert!(
        untouched(home.path()),
        "{shell} completions wrote into the home directory"
    );
    script
}

#[expect(clippy::unwrap_used)]
fn complete(home: &Path, words: &[&str]) -> String {
    stdout(
        mush(home)
            .env("COMPLETE", "fish")
            .arg("--")
            .args(words)
            .output()
            .unwrap(),
    )
}

#[test]
fn every_shell_gets_a_script_for_the_whole_command_tree() {
    for (shell, registration, nested, flag) in [
        (
            "bash",
            "complete -F _mush ",
            "mush__subcmd__sync,status)",
            "--resume",
        ),
        ("zsh", "#compdef mush", ":mush-sync-command-", "--resume"),
        (
            "fish",
            "complete -c mush ",
            "__fish_mush_using_subcommand sync",
            "-l resume",
        ),
        (
            "nushell",
            "export extern mush [",
            "export extern \"mush sync status\" [",
            "--resume",
        ),
        (
            "elvish",
            "edit:completion:arg-completer[mush]",
            "&'mush;sync;status'=",
            "--resume",
        ),
    ] {
        let script = script(shell);
        assert!(script.contains(registration), "{shell} registration");
        assert!(script.contains(nested), "{shell} nested subcommand");
        assert!(script.contains(flag), "{shell} flag");
    }
}

#[test]
fn nushell_takes_resume_with_or_without_an_id() {
    let script = script("nushell");
    assert!(script.lines().any(|line| line.trim() == "--resume"));
    assert!(script.contains("--fork: string"));
}

#[test]
fn the_dynamic_registration_calls_back_into_this_binary() {
    let home = crate::tempdir().unwrap();
    let binary = env!("CARGO_BIN_EXE_mush");
    for (shell, registration) in [
        ("bash", "-F _clap_complete_mush mush"),
        (
            "zsh",
            "if [[ ${funcstack[1]} = _mush ]]; then _clap_dynamic_completer_mush \"$@\"; fi",
        ),
        ("fish", "complete --keep-order --exclusive --command mush"),
    ] {
        let script = stdout(mush(home.path()).env("COMPLETE", shell).output().unwrap());
        assert!(script.contains(registration), "{shell} registration");
        assert!(script.contains(binary), "{shell} completer path");
    }
    let zsh = stdout(mush(home.path()).env("COMPLETE", "zsh").output().unwrap());
    assert!(zsh.contains("function _clap_dynamic_completer_mush()"));
    assert!(untouched(home.path()));
}

#[test]
fn saved_sessions_complete_with_their_titles() {
    let home = crate::tempdir().unwrap();
    let store = mush_session::SessionStore::new(home.path().join("data/sessions"));
    let mut session = mush_session::Session::new("model", "/work/place");
    session.meta.title = Some("synthetic fixture".into());
    store.save(&session).unwrap();
    let id = session.meta.id.as_str();
    let expected = format!("{id}\tsynthetic fixture");

    for words in [
        ["mush", "--resume", &id[..4]],
        ["mush", "attach", ""],
        ["mush", "delete", &id[..8]],
    ] {
        let offered = complete(home.path(), &words);
        assert!(
            offered.lines().any(|line| line == expected),
            "{words:?}: {offered}"
        );
    }
    assert!(
        !complete(home.path(), &["mush", "delete", "zz"]).contains(id),
        "a prefix that matches no file offers no session"
    );
}

#[test]
fn live_agents_complete_nothing_without_a_daemon() {
    let home = crate::tempdir().unwrap();
    for words in [
        ["mush", "stop", ""],
        ["mush", "send", ""],
        ["mush", "pause", ""],
    ] {
        let offered = complete(home.path(), &words);
        assert!(
            offered.lines().all(|line| line.starts_with('-')),
            "{words:?}: {offered}"
        );
    }
    assert!(untouched(home.path()));
}
