//! The library two ways: an IDE-style folder tree, and albums grouped from the tags.

use lofty::config::ParseOptions;
use lofty::file::{FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::ItemKey;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Whether tagui lists this file (by extension, as lofty knows it).
pub fn is_audio(path: &Path) -> bool {
    path.extension().and_then(FileType::from_ext).is_some()
}

/// Visible, non-hidden entries of `dir`: directories first, then audio files, each
/// alphabetical, or newest first (by modification time) when `by_date`.
fn entries(dir: &Path, by_date: bool) -> Vec<(bool, PathBuf)> {
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
    if by_date {
        // Stable: ties keep the alphabetical order.
        out.sort_by_cached_key(|(is_dir, p)| {
            let modified = std::fs::metadata(p).and_then(|m| m.modified()).ok();
            (!is_dir, Reverse(modified))
        });
    }
    out
}

/// Audio files directly inside `dir`, alphabetical.
pub fn audio_in(dir: &Path) -> Vec<PathBuf> {
    entries(dir, false)
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
    /// Newest first instead of alphabetical.
    pub by_date: bool,
}

impl Tree {
    pub fn new(root: PathBuf) -> Self {
        let mut tree = Self {
            root,
            expanded: HashSet::new(),
            visible: Vec::new(),
            cursor: 0,
            by_date: false,
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
        for (is_dir, path) in entries(dir, self.by_date) {
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

    /// Switch between alphabetical and newest first, staying on the same entry.
    pub fn toggle_order(&mut self) {
        let keep = self.selected().map(|n| n.path.clone());
        self.by_date = !self.by_date;
        self.refresh();
        if let Some(i) = keep.and_then(|p| self.visible.iter().position(|n| n.path == p)) {
            self.cursor = i;
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
    pub title: String,
    /// The first four characters of the date ("2003-05-01" is 2003).
    pub year: String,
    pub disc: u32,
    pub track: u32,
    /// Every genre, and every artist and album artist, one name per entry.
    pub genres: Vec<String>,
    pub artists: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Album {
    pub artist: String,
    pub album: String,
    /// The first year any track has.
    pub year: String,
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
    // Repeated items and "A; B" both hold several names.
    let names = |key| {
        tag.into_iter()
            .flat_map(move |t| t.get_strings(key))
            .flat_map(|s| s.split(';'))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let artist = match text(ItemKey::AlbumArtist) {
        "" => text(ItemKey::TrackArtist),
        a => a,
    };
    Some(Row {
        path: path.to_path_buf(),
        artist: artist.to_string(),
        album: text(ItemKey::AlbumTitle).to_string(),
        title: text(ItemKey::TrackTitle).to_string(),
        year: match text(ItemKey::RecordingDate) {
            "" => text(ItemKey::Year),
            d => d,
        }
        .chars()
        .take(4)
        .collect(),
        disc: number(ItemKey::DiscNumber),
        track: number(ItemKey::TrackNumber),
        genres: names(ItemKey::Genre).collect(),
        artists: names(ItemKey::TrackArtist)
            .chain(names(ItemKey::AlbumArtist))
            .collect(),
    })
}

/// Distinct names ignoring case, most used first (then by name); each in its most used
/// spelling.
pub fn choices<'a>(names: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    let mut spellings: HashMap<&str, usize> = HashMap::new();
    for n in names {
        *spellings.entry(n).or_default() += 1;
    }
    // lowercase -> (total uses, best spelling and its uses)
    let mut groups: HashMap<String, (usize, &str, usize)> = HashMap::new();
    for (name, count) in spellings {
        let g = groups.entry(name.to_lowercase()).or_insert((0, name, 0));
        g.0 += count;
        if (count, Reverse(name)) > (g.2, Reverse(g.1)) {
            (g.1, g.2) = (name, count);
        }
    }
    let mut out: Vec<_> = groups.into_values().map(|(n, name, _)| (n, name)).collect();
    out.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    out.into_iter().map(|(_, name)| name.to_string()).collect()
}

/// Every audio file under `root`, recursively.
// ponytail: synchronous (1.7k files: 1.2 s cold cache, 50 ms warm); thread it if that hurts.
pub fn scan(root: &Path) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for (is_dir, path) in entries(&dir, false) {
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
                year: rows
                    .iter()
                    .map(|r| r.year.clone())
                    .find(|y| !y.is_empty())
                    .unwrap_or_default(),
                tracks: rows.iter().map(|r| r.path.clone()).collect(),
            }
        })
        .collect()
}

/// Order albums by artist then album, or by album then artist, ignoring case.
pub fn sort_albums(albums: &mut [Album], by_artist: bool) {
    albums.sort_by_cached_key(|a| {
        let (artist, album) = (a.artist.to_lowercase(), a.album.to_lowercase());
        if by_artist {
            (artist, album)
        } else {
            (album, artist)
        }
    });
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
    fn toggle_order_puts_newest_first_and_keeps_the_cursor() {
        let root = std::env::temp_dir().join(format!("tagui-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for (name, age) in [("A", Some(old)), ("B", None), ("C", Some(old))] {
            std::fs::create_dir(root.join(name)).unwrap();
            if let Some(t) = age {
                std::fs::File::open(root.join(name))
                    .unwrap()
                    .set_modified(t)
                    .unwrap();
            }
        }
        std::fs::write(root.join("z.flac"), "").unwrap();
        let names = |tree: &Tree| {
            tree.visible
                .iter()
                .map(|n| n.path.file_name().unwrap().to_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        let mut tree = Tree::new(root.clone());
        tree.cursor = 2;
        tree.toggle_order();
        assert_eq!(
            names(&tree),
            ["B", "A", "C", "z.flac"],
            "dirs first, newest first"
        );
        assert_eq!(tree.cursor, 2, "still on C");
        tree.toggle_order();
        assert_eq!(names(&tree), ["A", "B", "C", "z.flac"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn albums_group_ignoring_case_and_sort_tracks() {
        let r = |path: &str, artist: &str, album: &str, disc, track| Row {
            path: path.into(),
            artist: artist.into(),
            album: album.into(),
            title: String::new(),
            year: if track == 2 {
                "2004".into()
            } else {
                String::new()
            },
            disc,
            track,
            genres: vec![],
            artists: vec![],
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
        assert_eq!(albums[2].year, "2004");
    }

    #[test]
    fn sort_albums_by_album_or_artist() {
        let a = |artist: &str, album: &str| Album {
            artist: artist.into(),
            album: album.into(),
            year: String::new(),
            tracks: vec![],
        };
        let mut albums = vec![
            a("Lamp", "Ato"),
            a("anri", "Timely!"),
            a("Anri", "bi・ki・ni"),
        ];
        let names = |albums: &[Album]| albums.iter().map(|a| a.album.clone()).collect::<Vec<_>>();
        sort_albums(&mut albums, false);
        assert_eq!(names(&albums), ["Ato", "bi・ki・ni", "Timely!"]);
        sort_albums(&mut albums, true);
        assert_eq!(names(&albums), ["bi・ki・ni", "Timely!", "Ato"]);
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
        assert_eq!(r.genres, ["Rock"]);
        assert_eq!(r.artists, ["Artist", "Album Artist"]);
    }

    #[test]
    fn choices_merge_case_most_used_first() {
        let names = ["rock", "Jazz", "Rock", "Rock", "Ambient", "jazz"].map(String::from);
        assert_eq!(choices(&names), ["Rock", "Jazz", "Ambient"]);
    }
}
