//! Per-machine recent-document persistence.
//!
//! This is deliberately separate from the Libraries registry: one absolute
//! path per line in an injectable file, saved atomically, with an eight-entry
//! MRU bound. Only the native entry point chooses the real XDG path.

use std::path::{Path, PathBuf};

pub const RECENT_LIMIT: usize = 8;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RecentFiles {
    paths: Vec<PathBuf>,
}

impl RecentFiles {
    pub fn new() -> RecentFiles {
        RecentFiles::default()
    }

    pub fn load(path: &Path) -> Result<RecentFiles, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::new()),
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        let mut recent = RecentFiles::new();
        for (line_no, line) in text.lines().enumerate() {
            let value = line.trim();
            if value.is_empty() || value.starts_with('#') {
                continue;
            }
            let entry = PathBuf::from(value);
            if !entry.is_absolute() {
                return Err(format!(
                    "{} line {}: recent path `{value}` is not absolute",
                    path.display(),
                    line_no + 1
                ));
            }
            if !recent.paths.contains(&entry) {
                recent.paths.push(entry);
            }
            if recent.paths.len() == RECENT_LIMIT {
                break;
            }
        }
        Ok(recent)
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let mut text = String::from("# eutectic recent documents: most recent first\n");
        for entry in &self.paths {
            text.push_str(&entry.display().to_string());
            text.push('\n');
        }
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("renaming {} over {}: {e}", tmp.display(), path.display()))
    }

    pub fn push(&mut self, path: PathBuf) {
        debug_assert!(path.is_absolute(), "recent paths are absolute");
        self.paths.retain(|entry| entry != &path);
        self.paths.insert(0, path);
        self.paths.truncate(RECENT_LIMIT);
    }

    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Scratch {
            let path = std::env::temp_dir().join(format!(
                "eutectic-recents-test-{}-{}",
                std::process::id(),
                line!()
            ));
            std::fs::create_dir_all(&path).expect("create scratch");
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn mru_is_deduped_and_bounded_to_eight() {
        let mut recent = RecentFiles::new();
        for i in 0..10 {
            recent.push(PathBuf::from(format!("/tmp/board-{i}.eut")));
        }
        recent.push(PathBuf::from("/tmp/board-5.eut"));
        assert_eq!(recent.paths().len(), RECENT_LIMIT);
        assert_eq!(recent.paths()[0], Path::new("/tmp/board-5.eut"));
        assert_eq!(
            recent
                .paths()
                .iter()
                .filter(|p| *p == Path::new("/tmp/board-5.eut"))
                .count(),
            1
        );
    }

    #[test]
    fn save_load_roundtrip_is_atomic_and_path_injected() {
        let scratch = Scratch::new();
        let file = scratch.0.join("nested/recent");
        let mut recent = RecentFiles::new();
        recent.push(PathBuf::from("/tmp/one.eut"));
        recent.push(PathBuf::from("/tmp/two with spaces.eut"));
        recent.save(&file).expect("save");
        assert_eq!(RecentFiles::load(&file).expect("load"), recent);
        assert!(!file.with_extension("tmp").exists());
    }
}
