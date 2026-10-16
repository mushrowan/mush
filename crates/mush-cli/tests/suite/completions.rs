use std::process::Command;

#[expect(clippy::unwrap_used)]
fn script(shell: &str) -> String {
    let home = crate::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mush"))
        .env_clear()
        .env("HOME", home.path())
        .current_dir(home.path())
        .args(["completions", shell])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{shell}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        std::fs::read_dir(home.path()).unwrap().next().is_none(),
        "{shell} completions wrote into the home directory"
    );
    String::from_utf8(output.stdout).unwrap()
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
