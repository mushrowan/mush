pub(crate) fn command_with_direnv(command: &str) -> String {
    format!(
        r#"if command -v direnv >/dev/null 2>&1; then
  eval "$(DIRENV_LOG_FORMAT= direnv export bash)"
fi
{command}"#
    )
}

pub type OutputSink = std::sync::Arc<dyn Fn(&str, &str) + Send + Sync>;

#[must_use]
pub fn redacting_sink(sink: OutputSink) -> OutputSink {
    std::sync::Arc::new(move |exec_id: &str, line: &str| {
        sink(exec_id, &mush_agent::redact::redact(line));
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_command_loads_direnv_environment() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(".envrc"),
            "export MUSH_DIRENV_TEST=loaded\n",
        )
        .unwrap();

        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let direnv = bin.join("direnv");
        let original_path = std::env::var_os("PATH");
        let bash = original_path
            .as_deref()
            .and_then(|path| {
                std::env::split_paths(path)
                    .map(|dir| dir.join("bash"))
                    .find(|candidate| candidate.is_file())
            })
            .expect("bash must be present in PATH for bash tool tests");
        std::fs::write(
            &direnv,
            format!(
                r#"#!{}
if [ "$1" = export ] && [ "$2" = bash ]; then
  printf 'export MUSH_DIRENV_TEST=loaded\n'
  exit 0
fi
exit 64
"#,
                bash.display()
            ),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&direnv).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut permissions, 0o755);
        std::fs::set_permissions(&direnv, permissions).unwrap();

        let path = match &original_path {
            Some(original) => format!("{}:{}", bin.display(), original.to_string_lossy()),
            None => bin.display().to_string(),
        };
        let result = tokio::process::Command::new(&bash)
            .args([
                "-c",
                &command_with_direnv("printf '%s' \"$MUSH_DIRENV_TEST\""),
            ])
            .current_dir(tmp.path())
            .env("PATH", path)
            .output()
            .await
            .unwrap();

        assert!(result.status.success());
        let text = String::from_utf8_lossy(&result.stdout);
        assert!(
            text.contains("loaded"),
            "expected bash command to inherit direnv exports, got {text}"
        );
    }
}
