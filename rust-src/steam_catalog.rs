//! Read-only Steam appinfo cache support, including uninstalled games.
//! Record layout follows SteamDatabase/SteamAppInfo's reference parser:
//! https://github.com/SteamDatabase/SteamAppInfo/blob/master/SteamAppInfoParser/AppInfo.cs
use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::SystemTime,
};

#[derive(Clone, Debug)]
pub(super) struct App {
    pub id: String,
    pub name: String,
    pub icon: Option<String>,
}

pub(super) fn title_key(title: &str) -> String {
    title
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}
fn roman_title_key(title: &str) -> String {
    title_key(title)
        .split(' ')
        .map(|word| match word {
            "ii" => "2",
            "iii" => "3",
            "iv" => "4",
            "v" => "5",
            "vi" => "6",
            "vii" => "7",
            "viii" => "8",
            "ix" => "9",
            "x" => "10",
            _ => word,
        })
        .collect::<Vec<_>>()
        .join(" ")
}
pub(super) fn matching_ids(game: &str, apps: &[App]) -> Vec<String> {
    let wanted = title_key(game);
    let ids = |roman: bool| {
        let mut found: Vec<_> = apps
            .iter()
            .filter(|app| {
                if roman {
                    roman_title_key(&app.name) == roman_title_key(game)
                } else {
                    title_key(&app.name) == wanted
                }
            })
            .map(|app| app.id.clone())
            .collect();
        found.sort();
        found.dedup();
        found
    };
    let exact = ids(false);
    if !exact.is_empty() {
        return exact;
    }
    let fallback = ids(true);
    if fallback.len() == 1 {
        fallback
    } else {
        Vec::new()
    }
}

const MAX_BYTES: u64 = 64 * 1024 * 1024;
type Cached = ((u64, Option<SystemTime>), Vec<App>);
pub(super) fn read(path: &Path) -> Vec<App> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Cached>>> = OnceLock::new();
    let Ok(metadata) = std::fs::metadata(path) else {
        return Vec::new();
    };
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Vec::new();
    }
    let revision = (metadata.len(), metadata.modified().ok());
    let cache = CACHE.get_or_init(Default::default);
    if let Ok(guard) = cache.lock()
        && let Some((_, apps)) = guard.get(path).filter(|(old, _)| *old == revision)
    {
        return apps.clone();
    }
    let mut bytes = Vec::new();
    let apps = std::fs::File::open(path)
        .and_then(|file| file.take(MAX_BYTES + 1).read_to_end(&mut bytes))
        .ok()
        .filter(|_| bytes.len() as u64 <= MAX_BYTES)
        .and_then(|_| parse(&bytes))
        .unwrap_or_default();
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 8 {
            guard.clear();
        }
        guard.insert(path.into(), (revision, apps.clone()));
    }
    apps
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}
impl<'a> Cursor<'a> {
    fn take(&mut self, size: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(size)?;
        let value = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(value)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn text(&mut self) -> Option<&'a str> {
        let end = self
            .bytes
            .get(self.pos..)?
            .iter()
            .take(65_536)
            .position(|b| *b == 0)?;
        let bytes = self.take(end + 1)?;
        std::str::from_utf8(&bytes[..end]).ok()
    }
}

fn fields<'a>(
    cursor: &mut Cursor<'a>,
    strings: Option<&[&'a str]>,
    depth: usize,
    common: bool,
    name: &mut Option<&'a str>,
    icon: &mut Option<&'a str>,
    budget: &mut usize,
) -> Option<()> {
    if depth > 32 {
        return None;
    }
    loop {
        *budget = budget.checked_sub(1)?;
        let kind = *cursor.take(1)?.first()?;
        if kind == 8 {
            return Some(());
        }
        let key = match strings {
            Some(table) => *table.get(cursor.u32()? as usize)?,
            None => cursor.text()?,
        };
        match kind {
            0 => fields(
                cursor,
                strings,
                depth + 1,
                key == "common",
                name,
                icon,
                budget,
            )?,
            1 => {
                let value = cursor.text()?;
                if common && key == "name" {
                    *name = Some(value);
                }
                if common
                    && key == "icon"
                    && value.len() == 40
                    && value.bytes().all(|b| b.is_ascii_hexdigit())
                {
                    *icon = Some(value);
                }
            }
            2 | 3 | 4 | 6 => {
                cursor.take(4)?;
            }
            5 => {
                let mut terminated = false;
                for _ in 0..32_768 {
                    if cursor.take(2)? == [0, 0] {
                        terminated = true;
                        break;
                    }
                }
                if !terminated {
                    return None;
                }
            }
            7 => {
                cursor.take(8)?;
            }
            _ => return None,
        }
        if name.is_some() && icon.is_some() {
            return Some(());
        }
    }
}

fn parse(bytes: &[u8]) -> Option<Vec<App>> {
    let mut cursor = Cursor { bytes, pos: 0 };
    let magic = cursor.u32()?;
    let version = magic & 0xff;
    if magic >> 8 != 0x075644 || !(39..=41).contains(&version) {
        return None;
    }
    cursor.u32()?;
    let mut strings = Vec::new();
    let records_end = if version == 41 {
        let offset = usize::try_from(u64::from_le_bytes(cursor.take(8)?.try_into().ok()?)).ok()?;
        if offset < cursor.pos || offset > bytes.len() {
            return None;
        }
        let mut table = Cursor { bytes, pos: offset };
        let count = table.u32()?;
        if count > 200_000 {
            return None;
        }
        for _ in 0..count {
            strings.push(table.text()?);
        }
        offset
    } else {
        bytes.len()
    };
    let mut apps = Vec::new();
    while cursor.pos < records_end {
        let id = cursor.u32()?;
        if id == 0 {
            return Some(apps);
        }
        let size = cursor.u32()? as usize;
        let end = cursor.pos.checked_add(size)?;
        if end > records_end || size > 8 * 1024 * 1024 {
            return None;
        }
        let header = if version >= 40 { 60 } else { 40 };
        let payload = bytes.get(cursor.pos.checked_add(header)?..end)?;
        let mut body = Cursor {
            bytes: payload,
            pos: 0,
        };
        let (mut name, mut icon) = (None, None);
        if fields(
            &mut body,
            (version == 41).then_some(strings.as_slice()),
            0,
            false,
            &mut name,
            &mut icon,
            &mut 1_000_000,
        )
        .is_some()
            && let Some(name) = name.filter(|name| !name.is_empty())
        {
            apps.push(App {
                id: id.to_string(),
                name: name.into(),
                icon: icon.map(str::to_ascii_lowercase),
            });
        }
        cursor.pos = end;
    }
    Some(apps)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(version: u32) -> Vec<u8> {
        let keys = ["appinfo", "common", "name", "icon"];
        let mut body = Vec::new();
        let mut key = |kind: u8, index: usize, value: Option<&str>| {
            body.push(kind);
            if version == 41 {
                body.extend((index as u32).to_le_bytes());
            } else {
                body.extend(keys[index].bytes());
                body.push(0);
            }
            if let Some(value) = value {
                body.extend(value.bytes());
                body.push(0);
            }
        };
        key(0, 0, None);
        key(0, 1, None);
        key(1, 2, Some("Blasphemous 2"));
        key(1, 3, Some("0123456789012345678901234567890123456789"));
        body.extend([8, 8]);
        let mut data = (0x07564400 | version).to_le_bytes().to_vec();
        data.extend(1_u32.to_le_bytes());
        if version == 41 {
            data.extend([0; 8]);
        }
        data.extend(2114740_u32.to_le_bytes());
        let header = if version >= 40 { 60 } else { 40 };
        data.extend((header + body.len() as u32).to_le_bytes());
        data.extend(vec![0; header as usize]);
        data.extend(body);
        data.extend(0_u32.to_le_bytes());
        if version == 41 {
            let offset = data.len() as u64;
            data[8..16].copy_from_slice(&offset.to_le_bytes());
            data.extend((keys.len() as u32).to_le_bytes());
            for key in keys {
                data.extend(key.bytes());
                data.push(0);
            }
        }
        data
    }
    #[test]
    fn supported_catalog_versions_and_truncated_data() {
        for version in [39, 40, 41] {
            let data = fixture(version);
            let apps = parse(&data).unwrap();
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].id, "2114740");
            assert_eq!(apps[0].name, "Blasphemous 2");
            assert!(apps[0].icon.is_some());
            for end in 0..data.len() - 1 {
                let _ = parse(&data[..end]);
            }
        }
        let mut data = fixture(41);
        data[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(parse(&data).is_none());
    }
    #[test]
    fn uninstalled_game_uses_catalog_identity_and_exact_icon_hash() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join(".local/share/Steam");
        let cache = root.join("appcache");
        std::fs::create_dir_all(cache.join("librarycache/2114740")).unwrap();
        std::fs::write(cache.join("appinfo.vdf"), fixture(41)).unwrap();
        let icon = b"\xff\xd8\xffsmall-icon";
        std::fs::write(
            cache.join("librarycache/2114740/0123456789012345678901234567890123456789.jpg"),
            icon,
        )
        .unwrap();
        // This lexically earlier cached image must not be mistaken for the icon.
        std::fs::write(
            cache.join("librarycache/2114740/0000000000000000000000000000000000000000.jpg"),
            b"\xff\xd8\xffwrong-cover",
        )
        .unwrap();
        assert_eq!(
            super::super::steam_icon_at(home.path(), "Blasphemous II"),
            Some((icon.to_vec(), "image/jpeg"))
        );
        assert_eq!(
            super::super::steam_icon_at(home.path(), "Blasphemous"),
            None
        );
    }
    #[test]
    fn literal_names_win_and_ambiguous_roman_aliases_are_rejected() {
        let app = |id: &str, name: &str| App {
            id: id.into(),
            name: name.into(),
            icon: None,
        };
        let catalog = vec![app("1", "Mega Man X"), app("2", "Mega Man 10")];
        assert_eq!(matching_ids("Mega Man X", &catalog), vec!["1"]);
        assert_eq!(matching_ids("Mega Man 10", &catalog), vec!["2"]);
        let ambiguous = vec![app("3", "Game 3"), app("4", "Game 3")];
        assert!(matching_ids("Game III", &ambiguous).is_empty());
        let duplicate_roots = vec![app("3", "Game 3"), app("3", "Game 3")];
        assert_eq!(matching_ids("Game III", &duplicate_roots), vec!["3"]);
    }
    #[test]
    fn titles_normalize_without_confusing_sequels_or_subtitles() {
        assert_eq!(
            roman_title_key("ARMORED CORE™ VI FIRES OF RUBICON™"),
            roman_title_key("Armored Core 6: Fires of Rubicon")
        );
        assert_eq!(
            roman_title_key("Blasphemous II"),
            roman_title_key("Blasphemous 2")
        );
        assert_ne!(title_key("Blasphemous"), title_key("Blasphemous 2"));
        assert_ne!(
            title_key("Armored Core VI"),
            title_key("Armored Core VI Fires of Rubicon")
        );
    }
}
