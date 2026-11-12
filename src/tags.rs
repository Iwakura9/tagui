//! One audio file's tags as editable text, saved back touching only what changed.
//!
//! Fields come from the file's primary tag through lofty's generic `Tag`. Custom fields
//! (`tidal_*`, `upc`, …) have no `ItemKey`: lofty keeps them aside and writes them back
//! untouched, so tagui never shows them and never loses them.

use anyhow::{bail, Context, Result};
use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{Picture, PictureType};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag, TagExt, TagType};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Editable fields shown first, in this order, after the read-only filename.
pub const MAIN: [(ItemKey, &str); 6] = [
    (ItemKey::TrackArtist, "Artist"),
    (ItemKey::AlbumArtist, "Album artist"),
    (ItemKey::TrackNumber, "Track number"),
    (ItemKey::TrackTitle, "Title"),
    (ItemKey::AlbumTitle, "Album"),
    (ItemKey::Genre, "Genre"),
];

/// Shown after the audio info even when empty. Any other standard field a file has
/// follows these.
pub const MORE: [(ItemKey, &str); 7] = [
    (ItemKey::RecordingDate, "Date"),
    (ItemKey::TrackTotal, "Track total"),
    (ItemKey::DiscNumber, "Disc number"),
    (ItemKey::DiscTotal, "Disc total"),
    (ItemKey::Composer, "Composer"),
    (ItemKey::Comment, "Comment"),
    (ItemKey::Lyrics, "Lyrics"),
];

/// Fields that only take a whole number.
pub const NUMERIC: [ItemKey; 4] = [
    ItemKey::TrackNumber,
    ItemKey::TrackTotal,
    ItemKey::DiscNumber,
    ItemKey::DiscTotal,
];

/// Text value per key. A missing key and an empty value mean the same.
pub type Fields = HashMap<ItemKey, String>;

#[derive(Debug, Clone, PartialEq)]
pub struct Info {
    /// Lowercase extension: flac, mp3, m4a…
    pub ext: String,
    pub kbps: Option<u32>,
    pub bits: Option<u8>,
    pub hz: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub info: Info,
    pub saved: Fields,
    pub draft: Fields,
    /// Embedded front cover (or the first picture) as saved.
    pub cover: Option<Vec<u8>>,
    /// Replacement front cover, written on save.
    pub new_cover: Option<Vec<u8>>,
}

/// Value of `key`, empty when absent.
pub fn get(fields: &Fields, key: ItemKey) -> &str {
    fields.get(&key).map_or("", String::as_str)
}

/// The value every track shares for `key`, or `None` when they differ.
pub fn common(tracks: &[Track], key: ItemKey) -> Option<&str> {
    let first = get(&tracks.first()?.draft, key);
    tracks
        .iter()
        .all(|t| get(&t.draft, key) == first)
        .then_some(first)
}

/// Display name of a field: from `MAIN`/`MORE`, else the key's name split into words.
pub fn label(key: ItemKey) -> String {
    if let Some((_, name)) = MAIN.iter().chain(&MORE).find(|(k, _)| *k == key) {
        return name.to_string();
    }
    let mut out = String::new();
    for c in format!("{key:?}").chars() {
        if c.is_uppercase() && !out.is_empty() {
            out.push(' ');
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `cover.jpg`, `cover.jpeg` or `cover.png` (any case) in `dir`.
pub fn folder_cover(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            matches!(
                name.to_lowercase().as_str(),
                "cover.jpg" | "cover.jpeg" | "cover.png"
            )
        })
}

impl Track {
    pub fn load(path: &Path) -> Result<Self> {
        let file =
            lofty::read_from_path(path).with_context(|| format!("reading {}", path.display()))?;
        let p = file.properties();
        let info = Info {
            ext: path
                .extension()
                .unwrap_or_default()
                .to_string_lossy()
                .to_lowercase(),
            kbps: p.audio_bitrate().or(p.overall_bitrate()),
            bits: p.bit_depth(),
            hz: p.sample_rate(),
        };
        let tag = file.primary_tag();
        let saved = tag.map(fields).unwrap_or_default();
        let cover = tag.and_then(|t| Some(t.pictures()[cover_index(t)?].data().to_vec()));
        Ok(Self {
            path: path.to_path_buf(),
            info,
            draft: saved.clone(),
            saved,
            cover,
            new_cover: None,
        })
    }

    pub fn dirty(&self) -> bool {
        !self.changed().is_empty() || self.new_cover.is_some()
    }

    /// Fields whose draft value differs from the saved one (absent counts as empty).
    pub fn changed(&self) -> Vec<ItemKey> {
        let mut keys: Vec<ItemKey> = self
            .saved
            .keys()
            .chain(self.draft.keys())
            .copied()
            .collect();
        keys.retain(|k| get(&self.draft, *k) != get(&self.saved, *k));
        keys.sort_by_key(|k| format!("{k:?}"));
        keys.dedup();
        keys
    }

    /// Drop the draft and the pending cover.
    pub fn discard(&mut self) {
        self.draft = self.saved.clone();
        self.new_cover = None;
    }

    /// Write the draft: the file is copied next to itself, only the changed fields and
    /// the new cover are written into the copy, and the copy replaces the original. A
    /// failure leaves the original untouched.
    pub fn save(&mut self) -> Result<()> {
        let mut tmp = self.path.as_os_str().to_owned();
        tmp.push(".tagui-tmp");
        let tmp = PathBuf::from(tmp);
        let result = self.write_copy(&tmp).and_then(|()| {
            std::fs::rename(&tmp, &self.path)
                .with_context(|| format!("replacing {}", self.path.display()))
        });
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result?;
        self.saved = self.draft.clone();
        if let Some(cover) = self.new_cover.take() {
            self.cover = Some(cover);
        }
        Ok(())
    }

    fn write_copy(&self, tmp: &Path) -> Result<()> {
        std::fs::copy(&self.path, tmp)
            .with_context(|| format!("copying {}", self.path.display()))?;
        // The copy's extension is .tagui-tmp, so the format is guessed from its content.
        let file = Probe::open(tmp)
            .and_then(|p| p.guess_file_type()?.read())
            .with_context(|| format!("reading {}", tmp.display()))?;
        let mut tag = file
            .primary_tag()
            .cloned()
            .unwrap_or_else(|| Tag::new(file.primary_tag_type()));
        for key in self.changed() {
            let new = get(&self.draft, key);
            tag.remove_key(key);
            if !new.is_empty() && !tag.insert_text(key, new.to_string()) {
                bail!("{} files can't hold {}", self.info.ext, label(key));
            }
        }
        if let Some(data) = &self.new_cover {
            let mut picture =
                Picture::from_reader(&mut data.as_slice()).context("reading cover")?;
            picture.set_pic_type(PictureType::CoverFront);
            // Replace the picture shown as the cover (pushed when there is none).
            tag.set_picture(cover_index(&tag).unwrap_or(0), picture);
        }
        // Keep an ID3v2.3 file at v2.3 (lofty writes v2.4 otherwise); players differ.
        let mut header = [0u8; 4];
        let v23 = std::io::Read::read_exact(&mut std::fs::File::open(tmp)?, &mut header).is_ok()
            && header == *b"ID3\x03";
        let options = WriteOptions::default().use_id3v23(v23);
        let written = if tag.tag_type() == TagType::VorbisComments {
            // lofty 0.25 drops the custom fields when it writes a generic Tag as Vorbis
            // comments; converting to the concrete type merges them back in first.
            VorbisComments::from(tag).save_to_path(tmp, options)
        } else {
            tag.save_to_path(tmp, options)
        };
        written.with_context(|| format!("writing tags to {}", tmp.display()))?;
        std::fs::File::open(tmp)?.sync_all()?;
        Ok(())
    }
}

/// The front cover, else the first picture: MP4 pictures have no type (lofty reads
/// them all as `Other`).
fn cover_index(tag: &Tag) -> Option<usize> {
    let pictures = tag.pictures();
    let front = pictures
        .iter()
        .position(|p| p.pic_type() == PictureType::CoverFront);
    front.or((!pictures.is_empty()).then_some(0))
}

/// Text items of a tag; repeated keys (two artists) are joined with "; ".
// ponytail: editing a joined value writes it back as one item; split it if multi-value matters.
fn fields(tag: &Tag) -> Fields {
    let mut out = Fields::new();
    for item in tag.items() {
        let Some(text) = item.value().text() else {
            continue;
        };
        out.entry(item.key())
            .and_modify(|v| {
                v.push_str("; ");
                v.push_str(text);
            })
            .or_insert_with(|| text.to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    /// A fresh copy of a fixture in its own temp dir.
    fn copy(name: &str, test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tagui-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::copy(Path::new(FIXTURES).join(name), &path).unwrap();
        path
    }

    /// The fixtures' custom field, as UTF-8 (Vorbis, MP4) or UTF-16LE (ID3v2 TXXX).
    fn has_custom_field(path: &Path) -> bool {
        let bytes = std::fs::read(path).unwrap().to_ascii_lowercase();
        let utf8 = b"tidal_track_id".to_vec();
        let utf16: Vec<u8> = utf8.iter().flat_map(|&b| [b, 0]).collect();
        [utf8, utf16]
            .iter()
            .any(|pat| bytes.windows(pat.len()).any(|w| w == pat))
    }

    #[test]
    fn saving_changes_only_the_edited_field() {
        for name in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            let path = copy(name, &format!("edit-{name}"));
            let before = Track::load(&path).unwrap();
            assert_eq!(get(&before.saved, ItemKey::Genre), "Rock", "{name}");
            assert_eq!(get(&before.saved, ItemKey::TrackNumber), "3", "{name}");
            assert!(before.cover.is_some() && has_custom_field(&path), "{name}");

            let mut t = before.clone();
            t.draft.insert(ItemKey::Genre, "Jazz".into());
            t.draft.insert(ItemKey::Composer, "Someone".into());
            assert!(t.dirty());
            t.save().unwrap();
            assert!(!t.dirty());

            let after = Track::load(&path).unwrap();
            let mut expected = before.saved.clone();
            expected.insert(ItemKey::Genre, "Jazz".into());
            expected.insert(ItemKey::Composer, "Someone".into());
            assert_eq!(after.saved, expected, "{name}");
            assert_eq!(after.cover, before.cover, "{name}: cover changed");
            assert_eq!(after.info, before.info, "{name}: audio changed");
            assert!(has_custom_field(&path), "{name}: custom field lost");
            assert!(!path
                .with_extension(format!("{}.tagui-tmp", t.info.ext))
                .exists());
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn id3v23_stays_v23() {
        let path = copy("tiny23.mp3", "v23");
        let mut t = Track::load(&path).unwrap();
        t.draft.insert(ItemKey::Genre, "Jazz".into());
        t.save().unwrap();
        assert_eq!(&std::fs::read(&path).unwrap()[..4], b"ID3\x03");
        assert_eq!(
            get(&Track::load(&path).unwrap().saved, ItemKey::Genre),
            "Jazz"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn clearing_a_field_removes_it() {
        for name in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            let path = copy(name, &format!("clear-{name}"));
            let mut t = Track::load(&path).unwrap();
            t.draft.insert(ItemKey::Genre, String::new());
            t.save().unwrap();
            let after = Track::load(&path).unwrap();
            assert!(!after.saved.contains_key(&ItemKey::Genre), "{name}");
            assert_eq!(get(&after.saved, ItemKey::TrackTitle), "Song", "{name}");
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn new_cover_replaces_the_front_cover_only() {
        let other = std::fs::read(Path::new(FIXTURES).join("other.jpg")).unwrap();
        for name in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            let path = copy(name, &format!("cover-{name}"));
            let before = Track::load(&path).unwrap();
            assert_ne!(before.cover.as_deref(), Some(other.as_slice()));
            let mut t = before.clone();
            t.new_cover = Some(other.clone());
            t.save().unwrap();
            let after = Track::load(&path).unwrap();
            assert_eq!(after.cover.as_deref(), Some(other.as_slice()), "{name}");
            assert_eq!(after.saved, before.saved, "{name}");
            assert!(has_custom_field(&path), "{name}: custom field lost");
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn failed_save_leaves_the_file_alone() {
        let path = copy("tiny.mp3", "fail");
        let original = std::fs::read(&path).unwrap();
        let mut t = Track::load(&path).unwrap();
        t.new_cover = Some(b"not an image at all".to_vec());
        assert!(t.save().is_err());
        assert!(t.dirty());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn empty_and_absent_are_the_same() {
        let path = copy("tiny.flac", "empty");
        let mut t = Track::load(&path).unwrap();
        t.draft.insert(ItemKey::Composer, String::new());
        assert!(!t.dirty());
        t.draft.insert(ItemKey::Genre, String::new());
        assert_eq!(t.changed(), [ItemKey::Genre]);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn common_value_or_various() {
        let path = copy("tiny.flac", "common");
        let a = Track::load(&path).unwrap();
        let mut b = a.clone();
        b.draft.insert(ItemKey::TrackTitle, "Other".into());
        let tracks = [a, b];
        assert_eq!(common(&tracks, ItemKey::Genre), Some("Rock"));
        assert_eq!(common(&tracks, ItemKey::TrackTitle), None);
        assert_eq!(common(&tracks, ItemKey::Composer), Some(""));
        assert_eq!(common(&[], ItemKey::Genre), None);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn labels_and_folder_cover() {
        assert_eq!(label(ItemKey::AlbumArtist), "Album artist");
        assert_eq!(
            label(ItemKey::ReplayGainTrackGain),
            "Replay gain track gain"
        );
        assert_eq!(
            folder_cover(Path::new(FIXTURES)),
            Some(Path::new(FIXTURES).join("cover.jpg"))
        );
        assert_eq!(folder_cover(Path::new("/nonexistent")), None);
    }
}
