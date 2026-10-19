use std::path::PathBuf;

#[must_use]
pub fn config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_CONFIG_DIR") {
        PathBuf::from(dir)
    } else if let Some(config) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(config).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".config/mush")
    } else {
        PathBuf::from(".mush")
    }
}

#[must_use]
pub fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("MUSH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/share/mush")
    } else {
        PathBuf::from(".mush")
    }
}

#[must_use]
pub fn state_dir() -> PathBuf {
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(state).join("mush")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home).join(".local/state/mush")
    } else {
        PathBuf::from(".mush")
    }
}

#[must_use]
pub fn saved_output_name(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".txt") else {
        return false;
    };
    let digits = stem
        .strip_prefix("tool_")
        .or_else(|| stem.strip_prefix("rejected_"))
        .unwrap_or_default();
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::saved_output_name;

    #[test]
    fn saved_output_names_match_the_previous_acceptance_rule() {
        let old = |name: &str| {
            name.strip_suffix(".txt")
                .and_then(|stem| {
                    stem.strip_prefix("tool_")
                        .or_else(|| stem.strip_prefix("rejected_"))
                })
                .is_some_and(|digits| {
                    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
                })
        };
        let mut state = 0x7a5c_324d_1e8b_960fu64;
        for _ in 0..1024 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let digits = state.to_string();
            let arbitrary: String = (0..state as usize % 64)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    char::from_u32((state % 0x11_0000) as u32).unwrap_or('�')
                })
                .collect();
            for prefix in ["tool_", "rejected_", "", "../tool_"] {
                for value in [digits.as_str(), arbitrary.as_str(), ""] {
                    for suffix in [".txt", "", ".txt.sync-in-ab"] {
                        let name = format!("{prefix}{value}{suffix}");
                        assert_eq!(saved_output_name(&name), old(&name), "{name:?}");
                    }
                }
            }
        }
    }
}
