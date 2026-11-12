//! The library two ways: an IDE-style folder tree, and albums grouped from the tags.

use lofty::config::ParseOptions;
use lofty::file::{FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Whether tagui lists this file (by extension, as lofty knows it).
pub fn is_audio(path: &Path) -> bool {
    path.extension().and_then(FileType::from_ext).is_some()
}

/// Visible, non-hidden entries of `dir`: directories first, then audio files, each
/// alphabetical.
fn entries(dir: &Path) -> Vec<(bool, PathBuf)> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(bool, PathBuf)> = read
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| (e.path().is_dir(), e.path()))
        .filter(|(is_dir, p)| *is_dir || is_audio(p))
        .collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    out
}

/// Audio files directly inside `dir`, alphabetical.
pub fn audio_in(dir: &Path) -> Vec<PathBuf> {
    entries(dir)
        .into_iter()
        .filter(|(is_dir, _)| !is_dir)
        .map(|(_, p)| p)
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Node {
    pub path: PathBuf,
    pub depth: usize,
    pub is_dir: bool,
}

pub struct Tree {
    pub root: PathBuf,
    pub expanded: HashSet<PathBuf>,
    pub visible: Vec<Node>,
    pub cursor: usize,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root,
            expanded: HashSet::new(),
            visible: Vec::new(),
            cursor: 0,
        };
        tree.refresh();
        tree
    }

    /// Rebuild the flattened visible list from disk and the expanded set.
    pub fn refresh(&mut self) {
        self.visible.clear();
        let root = self.root.clone();
        self.walk(&root, 0);
        self.cursor = self.cursor.min(self.visible.len().saturating_sub(1));
    }

    fn walk(&mut self, dir: &Path, depth: usize) {
        for (is_dir, path) in entries(dir) {
            self.visible.push(Node {
                path: path.clone(),
                depth,
                is_dir,
            });
            if is_dir && self.expanded.contains(&path) {
                self.walk(&path, depth + 1);
            }
        }
    }

    pub fn selected(&self) -> Option<&Node> {
        self.visible.get(self.cursor)
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.visible.len() as isize;
        if len > 0 {
            self.cursor = (self.cursor as isize + delta).clamp(0, len - 1) as usize;
        }
    }

    /// Expand/collapse the selected directory. Collapsing on a file jumps to (and
    /// collapses) its parent directory.
    pub fn set_expanded(&mut self, expand: bool) {
        let Some(node) = self.selected().cloned() else {
            return;
        };
        if node.is_dir {
            if expand {
                self.expanded.insert(node.path);
            } else {
                self.expanded.remove(&node.path);
            }
        } else if !expand {
            if let Some(i) = self.visible[..self.cursor]
                .iter()
                .rposition(|n| n.is_dir && n.depth + 1 == node.depth)
            {
                self.cursor = i;
                let parent = self.visible[i].path.clone();
                self.expanded.remove(&parent);
            }
        }
        self.refresh();
    }
}

/// What the album index needs from one file's tags.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub path: PathBuf,
    /// Album artist, or artist when there is none.
    pub artist: String,
    pub album: String,
    pub disc: u32,
    pub track: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Album {
    pub artist: String,
    pub album: String,
    /// In disc, track, then path order.
    pub tracks: Vec<PathBuf>,
}

/// Read the index fields of one file (tags only: no audio properties, no pictures).
pub fn row(path: &Path) -> Option<Row> {
    let options = ParseOptions::new()
        .read_properties(false)
        .read_cover_art(false);
    let file = Probe::open(path).ok()?.options(options).read().ok()?;
    let tag = file.primary_tag();
    let text = |key| tag.and_then(|t| t.get_string(key)).unwrap_or("").trim();
    // "3/12" counts as 3.
    let number = |key| {
        text(key)
            .split('/')
            .next()
            .and_then(|n| n.trim().parse().ok())
            .unwrap_or(0)
    };
    let artist = match text(ItemKey::AlbumArtist) {
        "" => text(ItemKey::TrackArtist),
        a => a,
    };
    Some(Row {
        path: path.to_path_buf(),
        artist: artist.to_string(),
        album: text(ItemKey::AlbumTitle).to_string(),
        disc: number(ItemKey::DiscNumber),
        track: number(ItemKey::TrackNumber),
    })
}

/// Every audio file under `root`, recursively.
// ponytail: synchronous (1.7k files: 1.2 s cold cache, 50 ms warm); thread it if that hurts.
pub fn scan(root: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for (is_dir, path) in entries(&dir) {
            if is_dir {
                dirs.push(path);
            } else if let Some(r) = row(&path) {
                rows.push(r);
            }
        }
    }
    rows
}

/// Group rows by (artist, album), ignoring case; sorted by artist, then album.
pub fn albums(rows: &[Row]) -> Vec<Album> {
    let mut groups: BTreeMap<(String, String), Vec<&Row>> = BTreeMap::new();
    for r in rows {
        let key = (r.artist.to_lowercase(), r.album.to_lowercase());
        groups.entry(key).or_default().push(r);
    }
    groups
        .into_values()
        .map(|mut rows| {
            rows.sort_by(|a, b| (a.disc, a.track, &a.path).cmp(&(b.disc, b.track, &b.path)));
            Album {
                artist: rows[0].artist.clone(),
                album: rows[0].album.clone(),
                tracks: rows.iter().map(|r| r.path.clone()).collect(),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirs_first_audio_only_and_expansion_shows_children() {
        let root = std::env::temp_dir().join(format!("tagui-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("B")).unwrap();
        for f in [
            "a.flac",
            "cover.jpg",
            ".hidden.mp3",
            "x.mp3.tagui-tmp",
            "B/x.m4a",
        ] {
            std::fs::write(root.join(f), "").unwrap();
        }

        let mut tree = Tree::new(root.clone());
        let names: Vec<_> = tree
            .visible
            .iter()
            .map(|n| n.path.strip_prefix(&root).unwrap().to_owned())
            .collect();
        assert_eq!(names, [Path::new("B"), Path::new("a.flac")]);

        tree.set_expanded(true);
        assert_eq!(tree.visible.len(), 3);
        assert_eq!(tree.visible[1].depth, 1);

        tree.move_by(1);
        tree.set_expanded(false); // on child file: collapse parent
        assert_eq!(tree.cursor, 0);
        assert_eq!(tree.visible.len(), 2);

        assert_eq!(audio_in(&root), [root.join("a.flac")]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn albums_group_ignoring_case_and_sort_tracks() {
        let r = |path: &str, artist: &str, album: &str, disc, track| Row {
            path: path.into(),
            artist: artist.into(),
            album: album.into(),
            disc,
            track,
        };
        let rows = [
            r("z/2", "Lamp", "Dusk to Dawn", 1, 2),
            r("b/1", "Anri", "Timely!", 0, 0),
            r("z/10", "lamp", "dusk to dawn", 2, 1),
            r("z/1", "Lamp", "Dusk to Dawn", 1, 1),
            r("c/1", "Lamp", "", 0, 0),
        ];
        let albums = albums(&rows);
        let names: Vec<_> = albums
            .iter()
            .map(|a| (a.artist.as_str(), a.album.as_str(), a.tracks.len()))
            .collect();
        assert_eq!(
            names,
            [
                ("Anri", "Timely!", 1),
                ("Lamp", "", 1),
                ("Lamp", "Dusk to Dawn", 3)
            ]
        );
        let order: Vec<_> = albums[2]
            .tracks
            .iter()
            .map(|p| p.to_str().unwrap())
            .collect();
        assert_eq!(order, ["z/1", "z/2", "z/10"]);
    }

    #[test]
    fn row_reads_index_fields() {
        let path = Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/tiny.m4a"
        ));
        let r = row(path).unwrap();
        assert_eq!(
            (r.artist.as_str(), r.album.as_str(), r.track),
            ("Album Artist", "Album", 3)
        );
    }
}
