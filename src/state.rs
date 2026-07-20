use std::{
    fmt::Write as _,
    path::{Path, PathBuf},
};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default)]
pub struct SavedState {
    pub page: usize,
    pub rotation: u16,
    pub inverted: bool,
    pub auto_crop: bool,
    pub tinted: bool,
    pub epub_font_size: Option<f32>,
}

fn state_path(file_path: &Path) -> Option<PathBuf> {
    let dirs = directories::ProjectDirs::from("", "", "vvrd")?;
    let hash = Md5::digest(file_path.to_string_lossy().as_bytes());
    let mut name = String::with_capacity(32 + ".json".len());
    for byte in hash.as_slice() {
        write!(&mut name, "{byte:02x}").ok()?;
    }
    name.push_str(".json");
    Some(dirs.cache_dir().join(name))
}

pub fn load_state(file_path: &Path) -> Option<SavedState> {
    serde_json::from_str(&std::fs::read_to_string(state_path(file_path)?).ok()?).ok()
}

pub fn save_state(file_path: &Path, state: &SavedState) {
    let Some(path) = state_path(file_path) else {
        return;
    };
    let Some(parent) = path.parent() else { return };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    if let Ok(json) = serde_json::to_string(state) {
        let _ = std::fs::write(path, json);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_json() {
        let state = SavedState {
            page: 9,
            rotation: 270,
            inverted: true,
            auto_crop: true,
            tinted: false,
            epub_font_size: Some(12.0),
        };
        let restored: SavedState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert_eq!(restored.page, 9);
        assert_eq!(restored.rotation, 270);
        assert!(restored.inverted);
    }

    #[test]
    fn state_names_are_stable_and_distinct() {
        assert_eq!(
            state_path(Path::new("/a.pdf")),
            state_path(Path::new("/a.pdf"))
        );
        assert_ne!(
            state_path(Path::new("/a.pdf")),
            state_path(Path::new("/b.pdf"))
        );
    }
}
