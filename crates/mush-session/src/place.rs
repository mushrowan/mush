use std::path::{Component, Path, PathBuf};

fn under_home(path: &Path) -> Option<&Path> {
    let mut components = path.components();
    match (components.next(), components.next(), components.next()) {
        (Some(Component::RootDir), Some(Component::Normal(top)), Some(Component::Normal(_)))
            if top == "home" || top == "Users" =>
        {
            Some(components.as_path())
        }
        _ => None,
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn same_place_from(recorded: &str, here: &str, home: Option<&Path>) -> bool {
    if recorded == here {
        return true;
    }
    match (home, under_home(Path::new(recorded))) {
        (Some(home), Some(rest)) => Path::new(here) == home.join(rest),
        _ => false,
    }
}

fn local_place_from(recorded: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = PathBuf::from(recorded);
    if path.is_dir() {
        return Some(path);
    }
    let candidate = home?.join(under_home(&path)?);
    candidate.is_dir().then_some(candidate)
}

#[must_use]
pub fn same_place(recorded: &str, here: &str) -> bool {
    same_place_from(recorded, here, home().as_deref())
}

#[must_use]
pub fn local_place(recorded: &str) -> Option<PathBuf> {
    local_place_from(recorded, home().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directory_under_another_machines_home_matches_the_same_one_here() {
        let home = Path::new("/home/rain");
        assert!(same_place_from(
            "/home/dot/dev/mush",
            "/home/rain/dev/mush",
            Some(home)
        ));
        assert!(same_place_from(
            "/Users/dot/dev/mush",
            "/home/rain/dev/mush",
            Some(home)
        ));
        assert!(same_place_from("/srv/work", "/srv/work", Some(home)));
        assert!(!same_place_from(
            "/home/dot/dev/mush",
            "/home/rain/dev/other",
            Some(home)
        ));
        assert!(!same_place_from(
            "/srv/work",
            "/home/rain/srv/work",
            Some(home)
        ));
        assert!(!same_place_from(
            "/home/dot/dev/mush",
            "/home/rain/dev/mush",
            None
        ));
    }

    #[test]
    fn a_missing_directory_resolves_under_this_home_only_if_it_exists() {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join("dev/mush")).unwrap();
        assert_eq!(
            local_place_from("/home/somebody-else/dev/mush", Some(home.path())),
            Some(home.path().join("dev/mush"))
        );
        assert_eq!(
            local_place_from("/home/somebody-else/dev/gone", Some(home.path())),
            None
        );
        let here = home.path().to_string_lossy().into_owned();
        assert_eq!(local_place_from(&here, None), Some(home.path().to_owned()));
    }
}
