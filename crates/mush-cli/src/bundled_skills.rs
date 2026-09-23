use std::path::{Path, PathBuf};

use mush_ext::loader::Skill;
use mush_ext::skill_scan::{SkillSource, parse_skill};

const BUNDLED: [(&str, &str); 2] = [
    ("goal", include_str!("../skills/goal/SKILL.md")),
    (
        "rlm-heartbeat",
        include_str!("../skills/rlm-heartbeat/SKILL.md"),
    ),
];

#[must_use]
pub fn root() -> PathBuf {
    mush_session::data_dir().join("bundled-skills")
}

fn skill_path(root: &Path, name: &str) -> PathBuf {
    root.join(name).join("SKILL.md")
}

pub fn install(root: &Path) -> std::io::Result<()> {
    for (name, content) in BUNDLED {
        let path = skill_path(root, name);
        if std::fs::read_to_string(&path).is_ok_and(|existing| existing == content) {
            continue;
        }
        mush_ai::private_io::write_atomic(&path, content)?;
    }
    Ok(())
}

#[must_use]
pub fn skills(root: &Path) -> Vec<Skill> {
    BUNDLED
        .iter()
        .flat_map(|(name, content)| {
            parse_skill(content, &skill_path(root, name), SkillSource::Bundled).skills
        })
        .collect()
}

#[must_use]
pub fn with_discovered(discovered: &[Skill], root: &Path) -> Vec<Skill> {
    let bundled = skills(root).into_iter().filter(|skill| {
        !discovered
            .iter()
            .any(|found| found.name.eq_ignore_ascii_case(&skill.name))
    });
    discovered.iter().cloned().chain(bundled).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_bundled_skill_parses_cleanly_and_matches_its_directory() {
        for (name, content) in BUNDLED {
            let path = skill_path(Path::new("/bundled"), name);
            let parsed = parse_skill(content, &path, SkillSource::Bundled);
            assert!(
                parsed.diagnostics.is_empty(),
                "{name}: {:?}",
                parsed.diagnostics
            );
            assert_eq!(parsed.skills.len(), 1, "{name}");
            assert_eq!(parsed.skills[0].name, name);
            assert!(!parsed.skills[0].disable_model_invocation, "{name}");
        }
    }

    #[test]
    fn install_writes_readable_skill_files_and_repairs_stale_ones() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path()).unwrap();
        for skill in skills(dir.path()) {
            let on_disk = std::fs::read_to_string(&skill.path).unwrap();
            assert!(on_disk.starts_with(&format!("---\nname: {}\n", skill.name)));
        }

        let goal = skill_path(dir.path(), "goal");
        std::fs::write(&goal, "stale").unwrap();
        install(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(&goal).unwrap(), BUNDLED[0].1);
    }

    #[test]
    fn a_discovered_skill_with_the_same_name_overrides_the_bundled_one() {
        let user_goal = Skill {
            name: "Goal".into(),
            description: "my own goal skill".into(),
            path: "/home/u/.config/mush/skills/goal/SKILL.md".into(),
            source: SkillSource::User,
            disable_model_invocation: false,
        };

        let merged = with_discovered(std::slice::from_ref(&user_goal), Path::new("/bundled"));

        let goals: Vec<_> = merged
            .iter()
            .filter(|skill| skill.name.eq_ignore_ascii_case("goal"))
            .collect();
        assert_eq!(goals, [&user_goal]);
        assert!(merged.iter().any(|skill| {
            skill.name == "rlm-heartbeat"
                && skill.source == SkillSource::Bundled
                && skill.path == Path::new("/bundled/rlm-heartbeat/SKILL.md")
        }));
    }
}
