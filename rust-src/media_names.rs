//! Preserve library identity and media format when a user chooses any filename.
use crate::media::FileIdentity;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NamedMedia {
    pub id: String,
    pub image: bool,
    pub format: String,
}
pub type Names = BTreeMap<PathBuf, NamedMedia>;

pub fn valid_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// Keep legacy IDs where possible, with a deterministic path ID when a renamed
/// clip still owns the stem of a newly recorded or externally added file.
pub fn available_id(path: &Path, preferred: &str, files: &BTreeMap<String, PathBuf>) -> String {
    if files.get(preferred).is_none_or(|owner| owner == path) {
        return preferred.to_owned();
    }
    use sha2::{Digest, Sha256};
    let mut attempt = 0_u64;
    loop {
        let mut hash = Sha256::new();
        hash.update(b"vice-media-id\0");
        hash.update(path.as_os_str().as_encoded_bytes());
        hash.update(attempt.to_le_bytes());
        let id = format!("{:x}", hash.finalize());
        if files.get(&id).is_none_or(|owner| owner == path) {
            return id;
        }
        attempt += 1;
    }
}

pub fn apply(
    names: &Names,
    directory: &Path,
    image: bool,
    files: &mut BTreeMap<String, PathBuf>,
    identities: &mut HashMap<PathBuf, FileIdentity>,
) {
    for (path, record) in names {
        if record.image != image || path.parent() != Some(directory) {
            continue;
        }
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let Ok(identity) = FileIdentity::from_metadata(&metadata) else {
            continue;
        };
        files.retain(|_, candidate| candidate != path);
        if let Some(displaced) = files.insert(record.id.clone(), path.clone())
            && displaced != *path
        {
            let id = available_id(&displaced, &record.id, files);
            files.insert(id, displaced);
        }
        identities.insert(path.clone(), identity);
    }
}

pub fn mime(format: &str) -> Option<&'static str> {
    match format {
        "mp4" | "mov" | "m4v" => Some("video/mp4"),
        "mkv" => Some("video/x-matroska"),
        "webm" => Some("video/webm"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn filenames_are_not_titles_or_paths() {
        for name in [
            " A  clip! #1?.MP4 ",
            ".hidden",
            "a\\b",
            "😀 finale.mkv",
            "clip",
            "a:b*<>|",
            "  ",
        ] {
            assert!(valid_filename(name), "{name}");
        }
        for name in ["", ".", "..", "../escape", "dir/clip", "bad\0name"] {
            assert!(!valid_filename(name));
        }
    }
    #[test]
    fn registered_names_stay_indexed_without_extensions_or_with_leading_dots() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".my clip!  ");
        std::fs::write(&path, b"media").unwrap();
        let names = Names::from([(
            path.clone(),
            NamedMedia {
                id: "original-id".into(),
                image: false,
                format: "mp4".into(),
            },
        )]);
        let mut files = BTreeMap::new();
        let mut identities = HashMap::new();
        apply(&names, dir.path(), false, &mut files, &mut identities);
        assert_eq!(files["original-id"], path);
        assert!(identities.contains_key(&path));
    }
    #[test]
    fn a_new_file_can_reuse_a_renamed_clips_original_filename() {
        let dir = tempfile::tempdir().unwrap();
        let original = dir.path().join("Vice_Clip_1.mp4");
        let renamed = dir.path().join("my clip");
        std::fs::write(&original, b"first recording").unwrap();
        std::fs::rename(&original, &renamed).unwrap();
        let names = Names::from([(
            renamed.clone(),
            NamedMedia {
                id: "Vice_Clip_1".into(),
                image: false,
                format: "mp4".into(),
            },
        )]);
        std::fs::write(&original, b"second recording").unwrap();
        let scan = || {
            let mut files = BTreeMap::from([("Vice_Clip_1".into(), original.clone())]);
            let mut identities = HashMap::new();
            apply(&names, dir.path(), false, &mut files, &mut identities);
            files
        };
        let files = scan();
        assert_eq!(files.len(), 2);
        assert_eq!(files["Vice_Clip_1"], renamed);
        let new_id = available_id(&original, "Vice_Clip_1", &files);
        assert_ne!(new_id, "Vice_Clip_1");
        assert_eq!(files[&new_id], original);
        assert_eq!(scan(), files, "rescanning must preserve both clip IDs");
    }
}
