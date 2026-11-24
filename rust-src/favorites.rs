//! Portable favorite markers in PNG text chunks and JPEG comment segments.
use anyhow::{Context, Result, bail};
use std::os::unix::fs::MetadataExt;
use std::{fs, path::Path};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
const MARK: &[u8] = b"vice_favorite\0";
const JPEG_MARK: &[u8] = b"vice_favorite=1";

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// Rewrite only our metadata block; retain image pixels and all other metadata.
pub fn image_marker(input: &[u8], desired: Option<bool>) -> Result<(bool, Vec<u8>)> {
    if input.starts_with(PNG) {
        let mut out = PNG.to_vec();
        let mut pos = 8usize;
        let mut found = false;
        let mut ended = false;
        while pos + 12 <= input.len() {
            let len = u32::from_be_bytes(input[pos..pos + 4].try_into()?) as usize;
            let end = pos
                .checked_add(12)
                .and_then(|n| n.checked_add(len))
                .context("invalid PNG chunk")?;
            if end > input.len() {
                bail!("truncated PNG chunk");
            }
            let kind = &input[pos + 4..pos + 8];
            let data = &input[pos + 8..pos + 8 + len];
            let ours = kind == b"tEXt" && data.starts_with(MARK);
            if ours {
                found = &data[MARK.len()..] == b"1";
            }
            if !ours || desired.is_none() {
                out.extend_from_slice(&input[pos..end]);
            }
            if kind == b"IEND" {
                // Insert before IEND, not after it.
                if desired == Some(true) {
                    let insert = out.len() - (end - pos);
                    let mut chunk = Vec::new();
                    let mut data = MARK.to_vec();
                    data.push(b'1');
                    chunk.extend_from_slice(&(data.len() as u32).to_be_bytes());
                    chunk.extend_from_slice(b"tEXt");
                    chunk.extend_from_slice(&data);
                    chunk.extend_from_slice(&crc32(&chunk[4..]).to_be_bytes());
                    out.splice(insert..insert, chunk);
                }
                out.extend_from_slice(&input[end..]);
                ended = true;
                break;
            }
            pos = end;
        }
        if !ended {
            bail!("invalid PNG: missing IEND");
        }
        Ok((found, out))
    } else if input.starts_with(&[0xff, 0xd8]) {
        let mut out = vec![0xff, 0xd8];
        if desired == Some(true) {
            out.extend_from_slice(&[0xff, 0xfe]);
            out.extend_from_slice(&((JPEG_MARK.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(JPEG_MARK);
        }
        let mut pos = 2;
        let mut found = false;
        loop {
            if pos + 2 > input.len() || input[pos] != 0xff {
                bail!("invalid JPEG marker");
            }
            let start = pos;
            while pos < input.len() && input[pos] == 0xff {
                pos += 1;
            }
            if pos >= input.len() {
                bail!("truncated JPEG marker");
            }
            let marker = input[pos];
            pos += 1;
            if marker == 0xda || marker == 0xd9 {
                out.extend_from_slice(&input[start..]);
                break;
            }
            if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                out.extend_from_slice(&input[start..pos]);
                continue;
            }
            if pos + 2 > input.len() {
                bail!("truncated JPEG segment");
            }
            let len = u16::from_be_bytes(input[pos..pos + 2].try_into()?) as usize;
            if len < 2 || pos + len > input.len() {
                bail!("invalid JPEG segment");
            }
            let ours = marker == 0xfe && &input[pos + 2..pos + len] == JPEG_MARK;
            found |= ours;
            if !ours || desired.is_none() {
                out.extend_from_slice(&input[start..pos + len]);
            }
            pos += len;
        }
        Ok((found, out))
    } else {
        bail!("unsupported image format")
    }
}

pub fn read_image(path: &Path) -> Result<bool> {
    Ok(image_marker(&fs::read(path)?, None)?.0)
}

pub fn write_image(path: &Path, favorite: bool) -> Result<()> {
    let before = fs::metadata(path)?;
    let (_, bytes) = image_marker(&fs::read(path)?, Some(favorite))?;
    let tmp = path.with_file_name(format!(".vice-meta-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        fs::write(&tmp, bytes)?;
        fs::set_permissions(&tmp, before.permissions())?;
        fs::File::options().write(true).open(&tmp)?.set_times(
            fs::FileTimes::new()
                .set_accessed(before.accessed()?)
                .set_modified(before.modified()?),
        )?;
        let now = fs::metadata(path)?;
        if (
            now.dev(),
            now.ino(),
            now.len(),
            now.mtime(),
            now.mtime_nsec(),
        ) != (
            before.dev(),
            before.ino(),
            before.len(),
            before.mtime(),
            before.mtime_nsec(),
        ) {
            bail!("image changed while saving favorite");
        }
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn png_and_jpeg_markers_round_trip_without_changing_other_bytes() {
        // The parser preserves every unrelated chunk/segment byte for byte.
        let png = [PNG, b"\0\0\0\x03tEXtfoo\0\0\0\0", b"\0\0\0\0IEND\xaeB`\x82"].concat();
        let jpeg = b"\xff\xd8\xff\xe1\x00\x06Exif\xff\xda\x00\x02\x11\x22\xff\xd9".to_vec();
        for original in [png, jpeg] {
            let (initial, marked) = image_marker(&original, Some(true)).unwrap();
            assert!(!initial);
            assert!(image_marker(&marked, None).unwrap().0);
            let (was_marked, unmarked) = image_marker(&marked, Some(false)).unwrap();
            assert!(was_marked);
            assert_eq!(unmarked, original);
            assert!(!image_marker(&unmarked, None).unwrap().0);
        }
    }
    #[test]
    fn rejects_truncated_images_without_writing() {
        assert!(image_marker(b"\x89PNG\r\n\x1a\n\0\0", Some(true)).is_err());
        assert!(image_marker(b"\xff\xd8\xff\xe1\0\x20", Some(true)).is_err());
    }
    #[test]
    fn marker_follows_file_when_copied_to_another_folder() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.png");
        let second = dir.path().join("second.png");
        let png = [PNG, b"\0\0\0\0IEND\xaeB`\x82"].concat();
        fs::write(&first, &png).unwrap();
        write_image(&first, true).unwrap();
        fs::copy(&first, &second).unwrap();
        assert!(read_image(&second).unwrap());
        write_image(&second, false).unwrap();
        assert!(!read_image(&second).unwrap());
        assert!(read_image(&first).unwrap());
    }
}

#[cfg(test)]
mod video_tests {
    use super::*;
    use std::collections::HashMap;

    #[tokio::test]
    async fn video_favorites_round_trip_and_preserve_existing_tags() {
        for (ext, codec) in [
            ("mp4", "mpeg4"),
            ("mov", "mpeg4"),
            ("mkv", "mpeg4"),
            ("webm", "libvpx"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(format!("clip.{ext}"));
            let result = std::process::Command::new("ffmpeg")
                .args([
                    "-v",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "color=size=16x16:rate=1",
                    "-t",
                    "1",
                    "-c:v",
                    codec,
                    "-y",
                    path.to_str().unwrap(),
                ])
                .output()
                .expect("ffmpeg is required");
            assert!(
                result.status.success(),
                "{ext}: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let tags = HashMap::from([
                ("vice_playlists".to_string(), "existing".to_string()),
                ("vice_favorite".to_string(), "1".to_string()),
            ]);
            assert!(
                crate::media::write_metadata_tags(&path, &tags, 30.0, None)
                    .await
                    .unwrap(),
                "{ext}"
            );
            let copy = dir.path().join(format!("other.{ext}"));
            fs::copy(&path, &copy).unwrap();
            let read = crate::media::probe_container_tags(&copy).await.unwrap();
            assert_eq!(
                read.get("vice_favorite").map(String::as_str),
                Some("1"),
                "{ext}"
            );
            assert_eq!(
                read.get("vice_playlists").map(String::as_str),
                Some("existing"),
                "{ext}"
            );
            let tags = HashMap::from([("vice_favorite".to_string(), "0".to_string())]);
            assert!(
                crate::media::write_metadata_tags(&copy, &tags, 30.0, None)
                    .await
                    .unwrap(),
                "{ext}"
            );
            let read = crate::media::probe_container_tags(&copy).await.unwrap();
            assert_eq!(
                read.get("vice_favorite").map(String::as_str),
                Some("0"),
                "{ext}"
            );
            assert_eq!(
                read.get("vice_playlists").map(String::as_str),
                Some("existing"),
                "{ext}"
            );
        }
    }
}
