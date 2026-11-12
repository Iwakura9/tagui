//! Three screens: albums grouped from the tags, the folder tree, and the tag editor for
//! a whole album or folder (track list on the left, fields and cover on the right).
//! Edits stay in each track's draft; files change only on Ctrl+s.

use crate::config;
use crate::library::{self, Album, Row, Tree};
use crate::tags::{self, Track, MAIN, MORE, NUMERIC};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use image::imageops::FilterType;
use lofty::tag::ItemKey;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Flex, Layout, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, List, ListItem, ListState, Paragraph, Row as TableRow, Table,
    TableState,
};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};
use std::collections::{BTreeSet, HashMap};
use std::io::{stdout, Stdout};
use std::path::{Path, PathBuf};

/// Cover box in cells; about square with the usual 1:2 cell shape.
const COVER: Size = Size::new(20, 10);
/// Thumbnail of each album in the rows view, in cells.
const THUMB: Size = Size::new(6, 3);
/// Cover of each album in the grid view, in cells; two lines of text go under it.
const GRID: Size = Size::new(16, 8);
/// Fields that differ per track by nature: not edited for a whole album at once.
/// Fields the "all" row hides: they only make sense track by track.
const PER_TRACK: [ItemKey; 2] = [ItemKey::TrackNumber, ItemKey::TrackTitle];
/// Standard fields never shown (kept on save like custom ones).
const HIDDEN: [ItemKey; 6] = [
    ItemKey::EncoderSoftware,
    ItemKey::Isrc,
    ItemKey::ReplayGainAlbumGain,
    ItemKey::ReplayGainAlbumPeak,
    ItemKey::ReplayGainTrackGain,
    ItemKey::ReplayGainTrackPeak,
];
const VARIOUS: &str = "<various>";

pub fn run(root: PathBuf) -> Result<()> {
    let _session = TerminalSession::enter(CrosstermTerminalOps)?;
    // Asks the terminal for its graphics protocol (kitty, sixel…) and font size.
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(stdout()))?;
    let scanning = format!("scanning {}…", root.display());
    terminal.draw(|f| f.render_widget(Paragraph::new(scanning.as_str()), f.area()))?;
    let mut app = App::new(root, picker);
    app.apply(&config::load());
    app.event_loop(&mut terminal)?;
    config::save(&app.settings())
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Screen {
    Albums,
    Folders,
    Editor,
}

/// How Albums shows the albums (`v` cycles).
#[derive(Clone, Copy, PartialEq, Debug)]
enum AlbumView {
    Compact,
    Rows,
    Grid,
}

impl AlbumView {
    fn name(self) -> &'static str {
        match self {
            AlbumView::Compact => "compact",
            AlbumView::Rows => "rows",
            AlbumView::Grid => "grid",
        }
    }

    const ALL: [AlbumView; 3] = [AlbumView::Compact, AlbumView::Rows, AlbumView::Grid];

    fn next(self) -> Self {
        match self {
            AlbumView::Compact => AlbumView::Rows,
            AlbumView::Rows => AlbumView::Grid,
            AlbumView::Grid => AlbumView::Compact,
        }
    }
}

/// The album or folder in the editor.
struct Open {
    title: String,
    tracks: Vec<Track>,
    /// Where Esc goes back to.
    from: Screen,
}

/// Text being typed into a field, with the cursor as a char index. A field with
/// `choices` is a picker: the text filters them and `pick` is the highlighted match.
/// A `free` picker also offers the text itself when no choice is exactly that.
#[derive(Default)]
struct Input {
    text: String,
    cursor: usize,
    choices: Vec<String>,
    pick: usize,
    free: bool,
}

impl Input {
    /// The choices that contain the text, ignoring case, then the text if it is new.
    // ponytail: plain lowercase match like `find`; fold accents if that bites.
    fn matches(&self) -> Vec<&str> {
        let query = self.text.to_lowercase();
        let mut out: Vec<&str> = self
            .choices
            .iter()
            .filter(|c| c.to_lowercase().contains(&query))
            .map(String::as_str)
            .collect();
        if self.free && !self.is_choice(&self.text) && !self.text.trim().is_empty() {
            out.push(&self.text);
        }
        out
    }

    fn is_choice(&self, text: &str) -> bool {
        let text = text.to_lowercase();
        self.choices.iter().any(|c| c.to_lowercase() == text)
    }
}

/// A `/` search: results filter as the query changes, Enter opens the picked one.
struct Search {
    query: String,
    pick: usize,
}

/// One `/` result: an album (index into `albums`), a folder, or a song (index into `rows`).
enum Hit {
    Album(usize),
    Folder(PathBuf),
    Song(usize),
}

/// The decoded cover on show.
struct Cover {
    bytes: Vec<u8>,
    size: Option<(u32, u32)>,
    protocol: Option<Protocol>,
}

struct App {
    screen: Screen,
    rows: Vec<Row>,
    albums: Vec<Album>,
    album_cursor: usize,
    /// Albums ordered by artist, else by album.
    albums_by_artist: bool,
    album_view: AlbumView,
    /// First line of albums on show in the views that scroll themselves (rows, grid).
    album_top: usize,
    /// Albums per line and per screen at the last draw, for the keys that move by them.
    tile_cols: usize,
    tile_page: usize,
    /// Album covers by first track and size; None when there is none to show.
    // ponytail: decoded synchronously while drawing; thread it if scrolling stutters.
    thumbs: HashMap<(PathBuf, Size), Option<Protocol>>,
    tree: Tree,
    open: Option<Open>,
    /// 0 = all tracks, 1.. = `tracks[track - 1]`.
    track: usize,
    /// Index into `field_rows(selection)`.
    field: usize,
    focus_tracks: bool,
    editing: Option<Input>,
    search: Option<Search>,
    show_help: bool,
    /// An action that would drop unsaved edits asked once and waits for its repeat.
    confirm: bool,
    armed: bool,
    status: String,
    picker: Picker,
    cover: Option<Cover>,
}

impl App {
    fn new(root: PathBuf, picker: Picker) -> Self {
        let rows = library::scan(&root);
        let mut albums = library::albums(&rows);
        library::sort_albums(&mut albums, false);
        let status = format!(
            "{} tracks in {} albums · ? for help",
            rows.len(),
            albums.len()
        );
        Self {
            screen: Screen::Albums,
            rows,
            albums,
            album_cursor: 0,
            albums_by_artist: false,
            album_view: AlbumView::Compact,
            album_top: 0,
            tile_cols: 1,
            tile_page: 10,
            thumbs: HashMap::new(),
            tree: Tree::new(root),
            open: None,
            track: 0,
            field: 0,
            focus_tracks: false,
            editing: None,
            search: None,
            show_help: false,
            confirm: false,
            armed: false,
            status,
            picker,
            cover: None,
        }
    }

    /// What `config` keeps between runs: the album view, both orders and the screen.
    fn settings(&self) -> Vec<(&'static str, &'static str)> {
        let screen = match self.open.as_ref().filter(|_| self.screen == Screen::Editor) {
            Some(o) => o.from,
            None => self.screen,
        };
        vec![
            ("view", self.album_view.name()),
            ("albums", self.album_order()),
            ("folders", self.folder_order()),
            (
                "screen",
                if screen == Screen::Folders {
                    "folders"
                } else {
                    "albums"
                },
            ),
        ]
    }

    /// Restore `settings`; unknown keys and values are left at their default.
    fn apply(&mut self, settings: &HashMap<String, String>) {
        let get = |key: &str| settings.get(key).map(String::as_str);
        if let Some(v) = AlbumView::ALL
            .into_iter()
            .find(|v| get("view") == Some(v.name()))
        {
            self.album_view = v;
        }
        if get("albums") == Some("artist") {
            self.albums_by_artist = true;
            library::sort_albums(&mut self.albums, true);
        }
        if get("folders") == Some("date") && !self.tree.by_date {
            self.tree.toggle_order();
        }
        if get("screen") == Some("folders") {
            self.screen = Screen::Folders;
        }
    }

    fn event_loop(
        &mut self,
        terminal: &mut ratatui::Terminal<CrosstermBackend<Stdout>>,
    ) -> Result<()> {
        loop {
            terminal.draw(|f| self.draw(f))?;
            let Event::Key(key) = event::read()? else {
                continue;
            };
            if key.kind == KeyEventKind::Press && self.key(key) {
                return Ok(());
            }
        }
    }

    fn dirty(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.tracks.iter().any(Track::dirty))
    }

    /// Whether an action that drops unsaved edits may go ahead: yes when nothing is
    /// unsaved or when it is the repeat of the key that just warned.
    fn may_drop_draft(&mut self, what: &str) -> bool {
        if !self.dirty() || self.armed {
            return true;
        }
        self.confirm = true;
        self.status = format!("unsaved changes: {what} again to drop them, Ctrl+s to save");
        false
    }

    /// Handle one key press; true means quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        self.armed = std::mem::take(&mut self.confirm);
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ctrl_c = ctrl && key.code == KeyCode::Char('c');
        // While typing, keys belong to the input (letters must not trigger shortcuts).
        if self.search.is_some() && !ctrl_c {
            self.search_key(key.code);
            return false;
        }
        if self.editing.is_some() && !ctrl_c {
            self.input_key(key.code);
            return false;
        }
        if ctrl_c || key.code == KeyCode::Char('q') {
            return self.may_drop_draft("q");
        }
        match key.code {
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('s') if ctrl => self.save(),
            KeyCode::Tab if self.screen == Screen::Albums => self.screen = Screen::Folders,
            KeyCode::Tab if self.screen == Screen::Folders => self.screen = Screen::Albums,
            code => match self.screen {
                Screen::Albums => self.albums_key(code),
                Screen::Folders => self.folders_key(code),
                Screen::Editor => self.editor_key(code),
            },
        }
        false
    }

    fn albums_key(&mut self, code: KeyCode) {
        let last = self.albums.len().saturating_sub(1);
        let (line, page) = match self.album_view {
            AlbumView::Compact => (1, 10),
            _ => (self.tile_cols, self.tile_page),
        };
        let c = &mut self.album_cursor;
        match code {
            KeyCode::Up | KeyCode::Char('k') => *c = c.saturating_sub(line),
            KeyCode::Down | KeyCode::Char('j') => *c = (*c + line).min(last),
            KeyCode::PageUp => *c = c.saturating_sub(page),
            KeyCode::PageDown => *c = (*c + page).min(last),
            KeyCode::Left | KeyCode::Char('h') if self.album_view == AlbumView::Grid => {
                *c = c.saturating_sub(1)
            }
            KeyCode::Right | KeyCode::Char('l') if self.album_view == AlbumView::Grid => {
                *c = (*c + 1).min(last)
            }
            KeyCode::Home => *c = 0,
            KeyCode::End => *c = last,
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('v') => {
                self.album_view = self.album_view.next();
                self.status = format!("albums: {} view", self.album_view.name());
            }
            KeyCode::Char('s') => {
                let keep = self.albums.get(self.album_cursor).cloned();
                self.albums_by_artist = !self.albums_by_artist;
                library::sort_albums(&mut self.albums, self.albums_by_artist);
                if let Some(i) = keep.and_then(|k| self.albums.iter().position(|a| *a == k)) {
                    self.album_cursor = i;
                }
                self.status = format!("albums by {}", self.album_order());
            }
            KeyCode::Enter => {
                if let Some(a) = self.albums.get(self.album_cursor).cloned() {
                    self.open_tracks(&a.tracks, album_title(&a), None);
                }
            }
            _ => {}
        }
    }

    fn album_order(&self) -> &'static str {
        if self.albums_by_artist {
            "artist"
        } else {
            "album"
        }
    }

    fn folder_order(&self) -> &'static str {
        if self.tree.by_date {
            "date"
        } else {
            "name"
        }
    }

    fn start_search(&mut self) {
        self.search = Some(Search {
            query: String::new(),
            pick: 0,
        });
        self.status = "type to filter · ↑↓ pick · ⏎ open · esc close".into();
    }

    /// Albums (or folders, in Folders) whose name contains the query, ignoring case,
    /// then songs whose title or artists do.
    // ponytail: plain lowercase match, "uteis" won't find "Úteis"; fold accents if that bites.
    fn search_results(&self, query: &str) -> Vec<Hit> {
        let query = query.to_lowercase();
        let hit = |text: &str| text.to_lowercase().contains(&query);
        let groups: Vec<Hit> = match self.screen {
            Screen::Folders => {
                let dirs: BTreeSet<&Path> =
                    self.rows.iter().filter_map(|r| r.path.parent()).collect();
                dirs.into_iter()
                    .filter(|d| hit(&d.file_name().unwrap_or_default().to_string_lossy()))
                    .map(|d| Hit::Folder(d.to_owned()))
                    .collect()
            }
            _ => (0..self.albums.len())
                .filter(|&i| {
                    hit(&format!(
                        "{} {}",
                        self.albums[i].album, self.albums[i].artist
                    ))
                })
                .map(Hit::Album)
                .collect(),
        };
        let songs = (0..self.rows.len())
            .filter(|&i| {
                hit(&format!(
                    "{} {}",
                    self.rows[i].title,
                    self.rows[i].artists.join(" ")
                ))
            })
            .map(Hit::Song);
        groups.into_iter().chain(songs).collect()
    }

    fn search_key(&mut self, code: KeyCode) {
        let Some(mut search) = self.search.take() else {
            return;
        };
        match code {
            KeyCode::Esc => {
                self.status.clear();
                return;
            }
            KeyCode::Enter => {
                let hit = self
                    .search_results(&search.query)
                    .into_iter()
                    .nth(search.pick);
                if let Some(hit) = hit {
                    self.open_hit(hit);
                }
                // Kept open when nothing opened (unsaved draft: Enter again drops it).
                if self.screen != Screen::Editor {
                    self.search = Some(search);
                }
                return;
            }
            KeyCode::Up => search.pick = search.pick.saturating_sub(1),
            KeyCode::Down => {
                let last = self.search_results(&search.query).len().saturating_sub(1);
                search.pick = (search.pick + 1).min(last);
            }
            KeyCode::Backspace => {
                search.query.pop();
                search.pick = 0;
            }
            KeyCode::Char(c) => {
                search.query.push(c);
                search.pick = 0;
            }
            _ => {}
        }
        self.search = Some(search);
    }

    /// Open a result in the editor; a song opens its album (or folder) at that track.
    fn open_hit(&mut self, hit: Hit) {
        let (album, song) = match hit {
            Hit::Folder(dir) => {
                self.open_dir(&dir, None);
                return;
            }
            Hit::Album(i) => (Some(i), None),
            Hit::Song(i) => {
                let path = self.rows[i].path.clone();
                if self.screen == Screen::Folders {
                    let dir = path.parent().unwrap_or(Path::new("")).to_owned();
                    self.open_dir(&dir, Some(&path));
                    return;
                }
                let album = self.albums.iter().position(|a| a.tracks.contains(&path));
                (album, Some(path))
            }
        };
        let Some(a) = album.and_then(|i| self.albums.get(i)).cloned() else {
            return;
        };
        self.album_cursor = album.unwrap_or(0);
        self.open_tracks(&a.tracks, album_title(&a), song.as_deref());
    }

    /// Open a folder's tracks at `select`; false when it holds no audio.
    fn open_dir(&mut self, dir: &Path, select: Option<&Path>) -> bool {
        let tracks = library::audio_in(dir);
        if tracks.is_empty() {
            return false;
        }
        let title = dir.file_name().unwrap_or_default().to_string_lossy();
        self.open_tracks(&tracks, title.into(), select);
        true
    }

    fn folders_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.tree.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.tree.move_by(1),
            KeyCode::PageUp => self.tree.move_by(-10),
            KeyCode::PageDown => self.tree.move_by(10),
            KeyCode::Left | KeyCode::Char('h') => self.tree.set_expanded(false),
            KeyCode::Right | KeyCode::Char('l') => self.tree.set_expanded(true),
            KeyCode::Char('s') => {
                self.tree.toggle_order();
                self.status = format!("folders by {}", self.folder_order());
            }
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Enter => {
                let Some(node) = self.tree.selected().cloned() else {
                    return;
                };
                // A folder opens its own tracks; a file opens its folder at that track.
                let (dir, select) = if node.is_dir {
                    (node.path.clone(), None)
                } else {
                    let parent = node.path.parent().unwrap_or(Path::new("")).to_owned();
                    (parent, Some(node.path.as_path()))
                };
                if !self.open_dir(&dir, select) {
                    let open = self.tree.expanded.contains(&node.path);
                    self.tree.set_expanded(!open);
                }
            }
            _ => {}
        }
    }

    fn open_tracks(&mut self, paths: &[PathBuf], title: String, select: Option<&Path>) {
        let from = self.screen;
        if let Some(o) = self.open.as_mut().filter(|o| {
            o.tracks
                .iter()
                .map(|t| t.path.as_path())
                .eq(paths.iter().map(PathBuf::as_path))
        }) {
            // Already open: go back to it, draft and all.
            o.from = from;
            let at = select.and_then(|s| o.tracks.iter().position(|t| t.path == s));
            self.screen = Screen::Editor;
            if let Some(i) = at {
                let key = self.selected_key();
                self.track = i + 1;
                self.track_changed(key);
            }
            self.update_cover();
            return;
        }
        if !self.may_drop_draft("Enter") {
            return;
        }
        let key = self.selected_key();
        let mut tracks = Vec::new();
        let mut failed = 0;
        for path in paths {
            match Track::load(path) {
                Ok(t) => tracks.push(t),
                Err(_) => failed += 1,
            }
        }
        if tracks.is_empty() {
            self.status = "no readable audio files here".into();
            return;
        }
        self.track = select
            .and_then(|s| tracks.iter().position(|t| t.path == s))
            .map_or(0, |i| i + 1);
        self.focus_tracks = false;
        self.status = match failed {
            0 => format!("{} tracks", tracks.len()),
            n => format!("{} tracks ({n} could not be read)", tracks.len()),
        };
        self.open = Some(Open {
            title,
            tracks,
            from: self.screen,
        });
        self.screen = Screen::Editor;
        // Keep a `MAIN` field selected, else start at the first one.
        let in_main = |k: &Option<ItemKey>| *k == key && MAIN.iter().any(|(m, _)| Some(*m) == key);
        let rows = self.rows();
        self.field = rows
            .iter()
            .position(in_main)
            .or_else(|| rows.iter().position(|k| *k == Some(MAIN[0].0)))
            .expect("MAIN is always listed");
        self.update_cover();
    }

    /// The field rows of the current selection ("all" hides Filename and `PER_TRACK`).
    fn rows(&self) -> Vec<Option<ItemKey>> {
        field_rows(self.selection(), self.track == 0)
    }

    fn selection(&self) -> &[Track] {
        match &self.open {
            Some(o) if self.track == 0 => &o.tracks,
            Some(o) => std::slice::from_ref(&o.tracks[self.track - 1]),
            None => &[],
        }
    }

    fn selection_mut(&mut self) -> &mut [Track] {
        let Some(o) = &mut self.open else {
            return &mut [];
        };
        match self.track {
            0 => &mut o.tracks,
            i => std::slice::from_mut(&mut o.tracks[i - 1]),
        }
    }

    fn selected_key(&self) -> Option<ItemKey> {
        self.rows().get(self.field).copied().flatten()
    }

    fn editor_key(&mut self, code: KeyCode) {
        let Some(tracks) = self.open.as_ref().map(|o| 1 + o.tracks.len()) else {
            return;
        };
        let fields = self.rows().len();
        let key = self.selected_key();
        match code {
            KeyCode::Esc => self.screen = self.open.as_ref().map_or(Screen::Albums, |o| o.from),
            KeyCode::Left | KeyCode::Char('h') => self.focus_tracks = true,
            KeyCode::Right | KeyCode::Char('l') => self.focus_tracks = false,
            KeyCode::Enter if self.focus_tracks => self.focus_tracks = false,
            KeyCode::Up | KeyCode::Char('k') if self.focus_tracks => {
                self.track = (self.track + tracks - 1) % tracks;
                self.track_changed(key);
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus_tracks => {
                self.track = (self.track + 1) % tracks;
                self.track_changed(key);
            }
            KeyCode::Up | KeyCode::Char('k') => self.field = (self.field + fields - 1) % fields,
            KeyCode::Down | KeyCode::Char('j') => self.field = (self.field + 1) % fields,
            KeyCode::Tab => {
                self.focus_tracks = false;
                self.field = (self.field + 1) % fields;
            }
            KeyCode::Enter => self.start_edit(),
            KeyCode::Char('d') => self.clear_field(),
            KeyCode::Char('c') => self.stage_cover(),
            KeyCode::Char('u') => {
                for t in self.open.iter_mut().flat_map(|o| &mut o.tracks) {
                    t.discard();
                }
                self.status = "dropped unsaved changes".into();
                self.update_cover();
            }
            _ => {}
        }
    }

    /// Keep the field that was selected (`key`) when the new selection has it.
    fn track_changed(&mut self, key: Option<ItemKey>) {
        let rows = self.rows();
        self.field = rows
            .iter()
            .position(|k| *k == key)
            .unwrap_or(self.field.min(rows.len() - 1));
        self.update_cover();
    }

    /// Why the selected field can't be edited for the current selection, if it can't.
    fn locked(&self) -> Option<String> {
        let sel = self.selection();
        let Some(key) = self.selected_key() else {
            return Some("filenames are read-only".into());
        };
        // ponytail: multi-line values (lyrics) are read-only; open them in $EDITOR if needed.
        if sel.iter().any(|t| tags::get(&t.draft, key).contains('\n')) {
            return Some("multi-line values are read-only".into());
        }
        None
    }

    fn start_edit(&mut self) {
        if let Some(why) = self.locked() {
            self.status = why;
            return;
        }
        let key = self.selected_key();
        let text = key
            .and_then(|k| tags::common(self.selection(), k))
            .unwrap_or("")
            .to_string();
        let choices = key.map_or(vec![], |k| self.choices(k));
        if choices.is_empty() {
            self.editing = Some(Input {
                cursor: text.chars().count(),
                text,
                ..Input::default()
            });
            return;
        }
        // A picker starts empty, on the current value.
        self.editing = Some(Input {
            pick: choices.iter().position(|c| *c == text).unwrap_or(0),
            choices,
            free: key != Some(ItemKey::Genre),
            ..Input::default()
        });
    }

    /// What a picker field offers: the values the library already has.
    fn choices(&self, key: ItemKey) -> Vec<String> {
        match key {
            ItemKey::Genre => library::choices(self.rows.iter().flat_map(|r| &r.genres)),
            ItemKey::TrackArtist | ItemKey::AlbumArtist => {
                library::choices(self.rows.iter().flat_map(|r| &r.artists))
            }
            _ => vec![],
        }
    }

    fn input_key(&mut self, code: KeyCode) {
        let Some(mut input) = self.editing.take() else {
            return;
        };
        let len = input.text.chars().count();
        let byte = |text: &str, at: usize| text.char_indices().nth(at).map_or(text.len(), |c| c.0);
        let picker = !input.choices.is_empty();
        match code {
            KeyCode::Esc => return,
            KeyCode::Enter if picker => match input.matches().get(input.pick) {
                Some(choice) => return self.commit(choice.to_string()),
                None => self.status = "not in the library: pick one from the list".into(),
            },
            KeyCode::Enter => return self.commit(input.text),
            KeyCode::Up if picker => input.pick = input.pick.saturating_sub(1),
            KeyCode::Down if picker => {
                input.pick = (input.pick + 1).min(input.matches().len().saturating_sub(1));
            }
            KeyCode::Backspace if input.cursor > 0 => {
                input.cursor -= 1;
                input.text.remove(byte(&input.text, input.cursor));
            }
            KeyCode::Delete if input.cursor < len => {
                input.text.remove(byte(&input.text, input.cursor));
            }
            KeyCode::Left => input.cursor = input.cursor.saturating_sub(1),
            KeyCode::Right => input.cursor = (input.cursor + 1).min(len),
            KeyCode::Home => input.cursor = 0,
            KeyCode::End => input.cursor = len,
            KeyCode::Char(c) => {
                input.text.insert(byte(&input.text, input.cursor), c);
                input.cursor += 1;
            }
            _ => {}
        }
        if picker
            && matches!(
                code,
                KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
            )
        {
            input.pick = 0;
        }
        self.editing = Some(input);
    }

    /// Put a typed value into the draft of every selected track.
    fn commit(&mut self, text: String) {
        let Some(key) = self.selected_key() else {
            return;
        };
        // An untouched <various> field must not wipe every track's value.
        if text.is_empty() && tags::common(self.selection(), key).is_none() {
            return;
        }
        if NUMERIC.contains(&key) && !text.chars().all(|c| c.is_ascii_digit()) {
            self.status = format!("{} takes a whole number", tags::label(key));
            return;
        }
        for t in self.selection_mut() {
            t.draft.insert(key, text.clone());
        }
    }

    fn clear_field(&mut self) {
        if let Some(why) = self.locked() {
            self.status = why;
            return;
        }
        if let Some(key) = self.selected_key() {
            for t in self.selection_mut() {
                t.draft.insert(key, String::new());
            }
            self.status = format!("cleared {}", tags::label(key));
        }
    }

    /// Stage each selected track's folder cover (cover.jpg/.jpeg/.png) as its new cover.
    fn stage_cover(&mut self) {
        let (mut staged, mut missing) = (0, 0);
        for t in self.selection_mut() {
            let dir = t.path.parent().unwrap_or(Path::new(""));
            let Some(bytes) = tags::folder_cover(dir).and_then(|p| std::fs::read(p).ok()) else {
                missing += 1;
                continue;
            };
            if image::guess_format(&bytes).is_err() {
                missing += 1;
            } else if t.cover.as_ref() != Some(&bytes) {
                t.new_cover = Some(bytes);
                staged += 1;
            }
        }
        self.status = match (staged, missing) {
            (0, 0) => "the folder cover is already embedded".into(),
            (0, _) => "no cover.jpg / cover.png in the folder".into(),
            (n, 0) => format!("folder cover staged for {n} track(s): Ctrl+s writes it"),
            (n, m) => format!("folder cover staged for {n} track(s), {m} without one"),
        };
        self.update_cover();
    }

    fn save(&mut self) {
        let Some(o) = &mut self.open else {
            return;
        };
        let mut saved = Vec::new();
        let mut errors = Vec::new();
        for t in o.tracks.iter_mut().filter(|t| t.dirty()) {
            match t.save() {
                Ok(()) => saved.push(t.path.clone()),
                Err(e) => errors.push(format!("{e:#}")),
            }
        }
        let first = o.tracks[0].path.clone();
        self.status = match errors.first() {
            None if saved.is_empty() => "nothing to save".into(),
            None => format!("saved {} file(s)", saved.len()),
            Some(e) => format!("saved {}, {} failed: {e}", saved.len(), errors.len()),
        };
        // Re-read the saved files into the album index; follow the open album.
        for path in &saved {
            if let (Some(i), Some(row)) = (
                self.rows.iter().position(|r| &r.path == path),
                library::row(path),
            ) {
                self.rows[i] = row;
            }
        }
        self.albums = library::albums(&self.rows);
        library::sort_albums(&mut self.albums, self.albums_by_artist);
        self.thumbs.clear();
        if let Some(i) = self.albums.iter().position(|a| a.tracks.contains(&first)) {
            self.album_cursor = i;
        }
        self.update_cover();
    }

    /// Decode the cover the selection shows, when it changed.
    fn update_cover(&mut self) {
        let (shown, _) = shown_cover(self.selection());
        if shown == self.cover.as_ref().map(|c| c.bytes.as_slice()) {
            return;
        }
        let Some(bytes) = shown.map(<[u8]>::to_vec) else {
            self.cover = None;
            return;
        };
        let decoded = image::load_from_memory(&bytes).ok();
        let size = decoded.as_ref().map(|i| (i.width(), i.height()));
        let protocol = decoded.and_then(|img| {
            let fit = Resize::Fit(Some(FilterType::Triangle));
            self.picker.new_protocol(img, COVER, fit).ok()
        });
        self.cover = Some(Cover {
            bytes,
            size,
            protocol,
        });
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        match (self.screen, &self.open) {
            (Screen::Editor, Some(o)) => self.draw_editor(f, o, main),
            (Screen::Folders, _) => self.draw_tree(f, main),
            _ if self.album_view == AlbumView::Compact => self.draw_albums(f, main),
            _ => self.draw_album_tiles(f, main),
        }
        if let Some(search) = &self.search {
            self.draw_search(f, search, main);
        }
        if !self.show_help || self.search.is_some() {
            f.render_widget(Paragraph::new(self.status.as_str()), status);
            return;
        }
        let hints: &[(&str, &str)] = match self.screen {
            Screen::Albums => &[
                ("⏎", "open"),
                ("/", "search"),
                ("s", "sort"),
                ("v", "view"),
                ("tab", "folders"),
                ("q", "quit"),
            ],
            Screen::Folders => &[
                ("⏎", "open"),
                ("/", "search"),
                ("s", "sort"),
                ("←→", "fold"),
                ("tab", "albums"),
                ("q", "quit"),
            ],
            Screen::Editor => &[
                ("←→", "tracks/fields"),
                ("⏎", "edit"),
                ("d", "delete"),
                ("c", "cover"),
                ("u", "undo"),
                ("^s", "save"),
                ("esc", "back"),
                ("q", "quit"),
            ],
        };
        let key_style = Style::default()
            .fg(Color::Rgb(190, 225, 255))
            .add_modifier(Modifier::BOLD);
        let cells =
            Layout::horizontal(hints.iter().map(|(k, w)| {
                Constraint::Length((k.chars().count() + 1 + w.chars().count()) as u16)
            }))
            .flex(Flex::SpaceBetween)
            .split(status);
        for ((key, word), cell) in hints.iter().zip(cells.iter()) {
            let hint = Line::from(vec![
                Span::styled(*key, key_style),
                Span::raw(format!(" {word}")),
            ]);
            f.render_widget(hint, *cell);
        }
    }

    /// The `/` results over the list: albums and folders in bold, songs with their album.
    fn draw_search(&self, f: &mut ratatui::Frame, search: &Search, area: Rect) {
        let width = area.width.saturating_sub(4).min(90);
        let height = area.height.saturating_sub(2).min(24);
        let area = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        let dim = Style::default().fg(Color::DarkGray);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let hits = self.search_results(&search.query);
        let items: Vec<ListItem> = if hits.is_empty() {
            vec![ListItem::new(Span::styled("no match", dim))]
        } else {
            hits.iter()
                .map(|hit| match hit {
                    Hit::Album(i) => ListItem::new(Span::styled(
                        format!("▸ {}", album_title(&self.albums[*i])),
                        bold,
                    )),
                    Hit::Folder(d) => {
                        let name = d.file_name().unwrap_or_default().to_string_lossy();
                        let parent = d
                            .parent()
                            .and_then(|p| p.strip_prefix(&self.tree.root).ok())
                            .unwrap_or(Path::new(""));
                        ListItem::new(Line::from(vec![
                            Span::styled(format!("▸ {name}"), bold),
                            Span::styled(format!("  {}", parent.display()), dim),
                        ]))
                    }
                    Hit::Song(i) => {
                        let r = &self.rows[*i];
                        let title = match r.title.as_str() {
                            "" => r.path.file_name().unwrap_or_default().to_string_lossy(),
                            t => t.into(),
                        };
                        let from = format!(
                            "  {} — {}",
                            or_none(&r.album, "(no album)"),
                            or_none(&r.artist, "(no artist)")
                        );
                        ListItem::new(Line::from(vec![
                            Span::raw(format!("♪ {title}")),
                            Span::styled(from, dim),
                        ]))
                    }
                })
                .collect()
        };
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" /{}▏", search.query)),
            )
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        let pick = (!hits.is_empty()).then(|| search.pick.min(hits.len() - 1));
        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut ListState::default().with_selected(pick));
    }

    fn draw_albums(&self, f: &mut ratatui::Frame, area: Rect) {
        let open: Option<&Path> = self.open.as_ref().map(|o| o.tracks[0].path.as_path());
        let items: Vec<ListItem> = self
            .albums
            .iter()
            .map(|a| {
                let mark = if open.is_some_and(|p| a.tracks.iter().any(|t| t == p)) {
                    " ●"
                } else {
                    ""
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        or_none(&a.album, "(no album)"),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}", or_none(&a.artist, "(no artist)")),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("  {}", a.tracks.len()),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(mark, Style::default().fg(Color::Green)),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(self.albums_title()),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(self.album_cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn albums_title(&self) -> String {
        format!(
            " albums · {} · by {} · {} ",
            self.tree.root.display(),
            self.album_order(),
            self.album_view.name()
        )
    }

    /// The albums as tiles with their cover: one per line in the rows view, as many as fit
    /// the width in the grid.
    fn draw_album_tiles(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.albums_title());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let grid = self.album_view == AlbumView::Grid;
        let (thumb, cell) = if grid {
            (GRID, Size::new(GRID.width + 2, GRID.height + 3))
        } else {
            (THUMB, Size::new(inner.width, THUMB.height))
        };
        let cols = (inner.width / cell.width.max(1)).max(1) as usize;
        let lines = (inner.height / cell.height).max(1) as usize;
        (self.tile_cols, self.tile_page) = (cols, cols * lines);
        let line = self.album_cursor / cols;
        self.album_top = self
            .album_top
            .min(line)
            .max((line + 1).saturating_sub(lines));

        let open: Option<PathBuf> = self.open.as_ref().map(|o| o.tracks[0].path.clone());
        let first = self.album_top * cols;
        for i in (first..self.albums.len()).take(lines * cols) {
            let n = i - first;
            let tile = Rect::new(
                inner.x + (n % cols) as u16 * cell.width,
                inner.y + (n / cols) as u16 * cell.height,
                cell.width,
                cell.height,
            )
            .intersection(inner);
            let [pic, text] = if grid {
                let tile = Rect {
                    width: thumb.width,
                    ..tile
                }
                .intersection(tile);
                Layout::vertical([Constraint::Length(thumb.height), Constraint::Length(2)])
                    .areas(tile)
            } else {
                Layout::horizontal([Constraint::Length(thumb.width + 1), Constraint::Min(1)])
                    .areas(tile)
            };
            let a = &self.albums[i];
            let mark = if open.as_ref().is_some_and(|p| a.tracks.contains(p)) {
                " ●"
            } else {
                ""
            };
            let lines = vec![
                Line::from(vec![
                    Span::styled(
                        or_none(&a.album, "(no album)").to_string(),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(mark, Style::default().fg(Color::Green)),
                ]),
                Line::from(Span::styled(
                    album_meta(a, !grid),
                    Style::default().fg(Color::Cyan),
                )),
            ];
            let style = if i == self.album_cursor {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            f.render_widget(Paragraph::new(lines).style(style), text);
            let pic = Rect::new(pic.x, pic.y, thumb.width, thumb.height).intersection(pic);
            match self.thumb(i, thumb) {
                Some(p) => f.render_widget(Image::new(p), pic),
                None => {
                    let mut lines = vec![Line::default(); (pic.height / 2) as usize];
                    lines.push(Line::from("♪"));
                    let placeholder = Paragraph::new(lines)
                        .centered()
                        .style(Style::default().fg(Color::DarkGray));
                    f.render_widget(placeholder, pic);
                }
            }
        }
    }

    /// Album `i`'s cover fit to `size`, decoded on first use.
    fn thumb(&mut self, i: usize, size: Size) -> Option<&Protocol> {
        let path = self.albums.get(i)?.tracks.first()?.clone();
        let picker = &self.picker;
        self.thumbs
            .entry((path, size))
            .or_insert_with_key(|(path, _)| {
                let img = image::load_from_memory(&tags::cover(path)?).ok()?;
                let fit = Resize::Fit(Some(FilterType::Triangle));
                picker.new_protocol(img, size, fit).ok()
            })
            .as_ref()
    }

    fn draw_tree(&self, f: &mut ratatui::Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .tree
            .visible
            .iter()
            .map(|n| {
                let name = n.path.file_name().unwrap_or_default().to_string_lossy();
                let (icon, style) = match (n.is_dir, self.tree.expanded.contains(&n.path)) {
                    (true, open) => (
                        if open { "▾ " } else { "▸ " },
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                    _ => ("  ", Style::default()),
                };
                ListItem::new(Line::from(vec![
                    Span::raw("  ".repeat(n.depth)),
                    Span::styled(format!("{icon}{name}"), style),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(format!(
                " {} · by {} ",
                self.tree.root.display(),
                self.folder_order()
            )))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(self.tree.cursor));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_editor(&self, f: &mut ratatui::Frame, o: &Open, area: Rect) {
        let left = (area.width / 3).clamp(24, 44);
        let [list_area, right] =
            Layout::horizontal([Constraint::Length(left), Constraint::Min(30)]).areas(area);

        // Track list: "all" first, then each track; * marks unsaved edits.
        let dirty = Span::styled(" *", Style::default().fg(Color::Green));
        let all_dirty = o.tracks.iter().any(Track::dirty);
        let mut items = vec![ListItem::new(Line::from(vec![
            Span::raw(format!("◆ all ({})", o.tracks.len())),
            if all_dirty {
                dirty.clone()
            } else {
                Span::raw("")
            },
        ]))];
        items.extend(o.tracks.iter().map(|t| {
            let mut spans = vec![Span::raw(track_label(t))];
            if t.dirty() {
                spans.push(dirty.clone());
            }
            ListItem::new(Line::from(spans))
        }));
        let highlight = if self.focus_tracks {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().fg(Color::Yellow)
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" tracks "))
            .highlight_style(highlight);
        let mut state = ListState::default().with_selected(Some(self.track));
        f.render_stateful_widget(list, list_area, &mut state);

        let title = format!(
            " {}{} ",
            or_none(&o.title, "(no album)"),
            if all_dirty { " * (unsaved)" } else { "" }
        );
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(right);
        f.render_widget(block, right);
        let [top, fields_area] =
            Layout::vertical([Constraint::Length(COVER.height + 1), Constraint::Min(1)])
                .areas(inner);
        let [cover_area, info_area] =
            Layout::horizontal([Constraint::Length(COVER.width + 2), Constraint::Min(1)])
                .areas(top);

        let sel = self.selection();
        let (_, same_cover) = shown_cover(sel);
        let cover_text = match &self.cover {
            None => "no cover".to_string(),
            Some(c) => {
                let size = c
                    .size
                    .map_or("unreadable".into(), |(w, h)| format!("{w}×{h}"));
                let new = if sel.iter().any(|t| t.new_cover.is_some()) {
                    " (new, unsaved)"
                } else {
                    ""
                };
                let differ = if same_cover { "" } else { ", covers differ" };
                format!("cover {size}{new}{differ}")
            }
        };
        let mut info: Vec<Line> = audio_info(sel).into_iter().map(Line::from).collect();
        info.push(Line::from(Span::styled(
            cover_text,
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(Paragraph::new(info), info_area);
        if let Some(protocol) = self.cover.as_ref().and_then(|c| c.protocol.as_ref()) {
            let area = Rect::new(cover_area.x, cover_area.y, COVER.width, COVER.height)
                .intersection(cover_area);
            f.render_widget(Image::new(protocol), area);
        }

        let rows = self.rows();
        let more = first_more(&rows);
        let focus = !self.focus_tracks;
        let mut table_rows = Vec::new();
        for (i, key) in rows.iter().enumerate() {
            if i == more {
                table_rows.push(TableRow::new(vec![Cell::from(Span::styled(
                    "── more ──",
                    Style::default().fg(Color::DarkGray),
                ))]));
            }
            let here = focus && i == self.field;
            let label = key.map_or("Filename".into(), tags::label);
            let label_style = if here {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            let value = match (&self.editing, here) {
                (Some(input), true) => Line::from(input_spans(input)),
                _ => self.value_line(sel, *key, here),
            };
            table_rows.push(TableRow::new(vec![
                Cell::from(Span::styled(label, label_style)),
                Cell::from(value),
            ]));
        }
        let shown = self.field + usize::from(self.field >= more);
        let table = Table::new(table_rows, [Constraint::Length(16), Constraint::Min(10)]);
        let mut state = TableState::default().with_selected(focus.then_some(shown));
        f.render_stateful_widget(table, fields_area, &mut state);

        // A picker's matches drop down under the field (or rise above it).
        let Some(input) = self.editing.as_ref().filter(|i| !i.choices.is_empty()) else {
            return;
        };
        let matches = input.matches();
        let row = fields_area.y + (shown - state.offset()) as u16;
        let below = fields_area.bottom().saturating_sub(row + 1);
        let above = row.saturating_sub(fields_area.y);
        let height = (matches.len().max(1) as u16 + 2)
            .min(12)
            .min(below.max(above));
        let y = if height <= below {
            row + 1
        } else {
            row - height
        };
        let x = fields_area.x + 17;
        let area = Rect::new(x, y, 40.min(fields_area.right().saturating_sub(x)), height);
        let items: Vec<ListItem> = if matches.is_empty() {
            vec![ListItem::new(Span::styled(
                "no match",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            matches
                .iter()
                .map(|m| match input.is_choice(m) {
                    true => ListItem::new(m.to_string()),
                    false => ListItem::new(Line::from(vec![
                        Span::raw(m.to_string()),
                        Span::styled("  (new)", Style::default().fg(Color::DarkGray)),
                    ])),
                })
                .collect()
        };
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL))
            .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow));
        let mut pick =
            ListState::default().with_selected((!matches.is_empty()).then_some(input.pick));
        f.render_widget(Clear, area);
        f.render_stateful_widget(list, area, &mut pick);
    }

    /// A field's value for the selection: shared value, or <various>; green when edited.
    fn value_line(&self, sel: &[Track], key: Option<ItemKey>, here: bool) -> Line<'static> {
        let (text, edited) = match key {
            None => match sel {
                [t] => (
                    t.path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into(),
                    false,
                ),
                _ => (VARIOUS.to_string(), false),
            },
            Some(k) => {
                let edited = sel
                    .iter()
                    .any(|t| tags::get(&t.draft, k) != tags::get(&t.saved, k));
                let text = match tags::common(sel, k) {
                    Some(v) if v.contains('\n') => {
                        format!("{}…", v.lines().next().unwrap_or(""))
                    }
                    Some(v) => v.to_string(),
                    None => VARIOUS.to_string(),
                };
                (text, edited)
            }
        };
        let mut style = Style::default();
        if text == VARIOUS {
            style = style.fg(Color::DarkGray);
        }
        if edited {
            style = style.fg(Color::Green);
        }
        if here {
            style = style.fg(Color::Black).bg(Color::Yellow);
        }
        Line::from(Span::styled(text, style))
    }
}

/// The field list for a selection: Filename (`None`), `MAIN`, `MORE`, then every other
/// standard field the selected tracks have (but `HIDDEN`), by label. `all` (the "all" row) leaves out
/// Filename and `PER_TRACK`.
fn field_rows(sel: &[Track], all: bool) -> Vec<Option<ItemKey>> {
    let listed = |k: &ItemKey| MAIN.iter().chain(&MORE).any(|(m, _)| m == k);
    let mut extra: Vec<ItemKey> = sel
        .iter()
        .flat_map(|t| t.draft.keys().chain(t.saved.keys()))
        .copied()
        .filter(|k| !listed(k) && !HIDDEN.contains(k))
        .collect();
    extra.sort_by_key(|k| tags::label(*k));
    extra.dedup();
    std::iter::once(None)
        .chain(MAIN.iter().chain(&MORE).map(|(k, _)| Some(*k)))
        .chain(extra.into_iter().map(Some))
        .filter(|k| !all || k.is_some_and(|k| !PER_TRACK.contains(&k)))
        .collect()
}

/// Index of the first `MORE` field in `rows`.
fn first_more(rows: &[Option<ItemKey>]) -> usize {
    rows.iter()
        .position(|k| *k == Some(MORE[0].0))
        .expect("MORE is always listed")
}

/// The cover the selection shows (the new one if staged) and whether every selected
/// track has that same cover.
fn shown_cover(sel: &[Track]) -> (Option<&[u8]>, bool) {
    fn cover(t: &Track) -> Option<&[u8]> {
        t.new_cover.as_deref().or(t.cover.as_deref())
    }
    let first = sel.first().and_then(cover);
    (first, sel.iter().all(|t| cover(t) == first))
}

/// Format, quality and bitrate lines; ranges when the tracks differ.
fn audio_info(sel: &[Track]) -> Vec<String> {
    fn joined(mut values: Vec<String>) -> String {
        values.dedup();
        values.join(", ")
    }
    let mut exts: Vec<String> = sel.iter().map(|t| t.info.ext.clone()).collect();
    exts.sort();
    let mut quality: Vec<String> = sel
        .iter()
        .map(|t| {
            let bits = t
                .info
                .bits
                .map(|b| format!("{b} bit · "))
                .unwrap_or_default();
            let khz = t.info.hz.map_or("? kHz".into(), |hz| {
                format!("{} kHz", (hz as f64 / 100.0).round() / 10.0)
            });
            format!("{bits}{khz}")
        })
        .collect();
    quality.sort();
    let kbps: Vec<u32> = sel.iter().filter_map(|t| t.info.kbps).collect();
    let rate = match (kbps.iter().min(), kbps.iter().max()) {
        (Some(lo), Some(hi)) if lo == hi => format!("{lo} kbps"),
        (Some(lo), Some(hi)) => format!("{lo}–{hi} kbps"),
        _ => "? kbps".into(),
    };
    vec![joined(exts), joined(quality), rate]
}

/// "03 Title" (title falls back to the file name).
fn track_label(t: &Track) -> String {
    let number = tags::get(&t.draft, ItemKey::TrackNumber);
    let number = match number.parse::<u32>() {
        Ok(n) => format!("{n:02} "),
        Err(_) => String::new(),
    };
    let title = match tags::get(&t.draft, ItemKey::TrackTitle) {
        "" => t
            .path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into(),
        title => title.to_string(),
    };
    format!("{number}{title}")
}

fn album_title(a: &Album) -> String {
    format!(
        "{} — {}",
        or_none(&a.album, "(no album)"),
        or_none(&a.artist, "(no artist)")
    )
}

/// "Artist · 2004 · 12 tracks" (the year only when asked), without the parts that are
/// missing.
fn album_meta(a: &Album, year: bool) -> String {
    let n = a.tracks.len();
    let tracks = format!("{n} track{}", if n == 1 { "" } else { "s" });
    let year = if year { a.year.as_str() } else { "" };
    [or_none(&a.artist, "(no artist)"), year, &tracks]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" · ")
}

fn or_none<'a>(s: &'a str, none: &'a str) -> &'a str {
    if s.is_empty() {
        none
    } else {
        s
    }
}

/// The text being typed, with the char under the cursor reversed.
fn input_spans(input: &Input) -> Vec<Span<'static>> {
    let chars: Vec<char> = input.text.chars().collect();
    let before: String = chars[..input.cursor].iter().collect();
    let at = chars.get(input.cursor).map_or(" ".into(), char::to_string);
    let after: String = chars
        .get(input.cursor + 1..)
        .unwrap_or(&[])
        .iter()
        .collect();
    let style = Style::default().fg(Color::Black).bg(Color::Yellow);
    vec![
        Span::styled(before, style),
        Span::styled(at, style.add_modifier(Modifier::REVERSED)),
        Span::styled(after, style),
    ]
}

trait TerminalOps {
    fn enable_raw(&mut self) -> Result<()>;
    fn enter_alternate(&mut self) -> Result<()>;
    fn leave_alternate(&mut self) -> Result<()>;
    fn disable_raw(&mut self) -> Result<()>;
}

struct CrosstermTerminalOps;

impl TerminalOps for CrosstermTerminalOps {
    fn enable_raw(&mut self) -> Result<()> {
        enable_raw_mode().map_err(Into::into)
    }

    fn enter_alternate(&mut self) -> Result<()> {
        execute!(stdout(), EnterAlternateScreen).map_err(Into::into)
    }

    fn leave_alternate(&mut self) -> Result<()> {
        execute!(stdout(), LeaveAlternateScreen).map_err(Into::into)
    }

    fn disable_raw(&mut self) -> Result<()> {
        disable_raw_mode().map_err(Into::into)
    }
}

/// Restores terminal modes on normal return, error and panic unwinding.
struct TerminalSession<T: TerminalOps> {
    ops: T,
    raw: bool,
    alternate: bool,
}

impl<T: TerminalOps> TerminalSession<T> {
    fn enter(mut ops: T) -> Result<Self> {
        ops.enable_raw()?;
        if let Err(error) = ops.enter_alternate() {
            let _ = ops.disable_raw();
            return Err(error);
        }
        Ok(Self {
            ops,
            raw: true,
            alternate: true,
        })
    }
}

impl<T: TerminalOps> Drop for TerminalSession<T> {
    fn drop(&mut self) {
        if self.alternate {
            let _ = self.ops.leave_alternate();
            self.alternate = false;
        }
        if self.raw {
            let _ = self.ops.disable_raw();
            self.raw = false;
        }
    }
}

#[cfg(test)]
mod terminal_tests {
    use super::{TerminalOps, TerminalSession};
    use anyhow::{bail, Result};
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Clone)]
    struct FakeOps {
        calls: Rc<RefCell<Vec<&'static str>>>,
        fail_alternate: bool,
    }

    impl TerminalOps for FakeOps {
        fn enable_raw(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("enable_raw");
            Ok(())
        }

        fn enter_alternate(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("enter_alternate");
            if self.fail_alternate {
                bail!("alternate screen failed")
            }
            Ok(())
        }

        fn leave_alternate(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("leave_alternate");
            Ok(())
        }

        fn disable_raw(&mut self) -> Result<()> {
            self.calls.borrow_mut().push("disable_raw");
            Ok(())
        }
    }

    fn fake(fail_alternate: bool) -> (FakeOps, Rc<RefCell<Vec<&'static str>>>) {
        let calls = Rc::new(RefCell::new(Vec::new()));
        (
            FakeOps {
                calls: calls.clone(),
                fail_alternate,
            },
            calls,
        )
    }

    #[test]
    fn session_restores_terminal_on_drop() {
        let (ops, calls) = fake(false);
        drop(TerminalSession::enter(ops).unwrap());
        assert_eq!(
            *calls.borrow(),
            [
                "enable_raw",
                "enter_alternate",
                "leave_alternate",
                "disable_raw"
            ]
        );
    }

    #[test]
    fn partial_entry_rolls_back_raw_mode() {
        let (ops, calls) = fake(true);
        assert!(TerminalSession::enter(ops).is_err());
        assert_eq!(
            *calls.borrow(),
            ["enable_raw", "enter_alternate", "disable_raw"]
        );
    }

    #[test]
    fn panic_unwinding_restores_terminal() {
        let (ops, calls) = fake(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _session = TerminalSession::enter(ops).unwrap();
            panic!("test panic");
        }));
        assert!(result.is_err());
        assert_eq!(calls.borrow().last(), Some(&"disable_raw"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    /// A library with one album (the three fixtures share their tags) whose folder
    /// cover differs from the embedded one.
    fn library(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tagui-tui-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Album")).unwrap();
        for f in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            std::fs::copy(Path::new(FIXTURES).join(f), root.join("Album").join(f)).unwrap();
        }
        std::fs::copy(
            Path::new(FIXTURES).join("other.jpg"),
            root.join("Album/cover.jpg"),
        )
        .unwrap();
        root
    }

    fn press(app: &mut App, keys: &str) -> bool {
        let mut quit = false;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\t' => KeyCode::Tab,
                '\x08' => KeyCode::Backspace,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            quit = app.key(KeyEvent::from(code));
        }
        quit
    }

    fn ctrl_s(app: &mut App) {
        app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    }

    fn screen_text(app: &mut App) -> String {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn batch_edit_an_album_and_save() {
        let root = library("batch");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        assert_eq!(app.albums.len(), 1);

        press(&mut app, "\n"); // open the album, "all" selected, field Artist
        assert_eq!(app.screen, Screen::Editor);
        assert_eq!(app.selection().len(), 3);
        let text = screen_text(&mut app);
        assert!(text.contains("Album Artist"));
        for hidden in ["Filename", "Track number", "Title"] {
            assert!(!text.contains(hidden), "{hidden} is per track");
        }

        press(&mut app, "jj\n\x08\x08\x08\x08\x08Live\n"); // Album: Album -> Live
        assert!(app.dirty());
        assert!(screen_text(&mut app).contains("Live"));

        press(&mut app, "c"); // folder cover differs from the embedded one
        assert!(app.selection().iter().all(|t| t.new_cover.is_some()));

        assert!(!press(&mut app, "q"), "first q only warns");
        ctrl_s(&mut app);
        assert!(!app.dirty(), "{}", app.status);
        let cover = std::fs::read(root.join("Album/cover.jpg")).unwrap();
        for f in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            let t = Track::load(&root.join("Album").join(f)).unwrap();
            assert_eq!(tags::get(&t.saved, ItemKey::AlbumTitle), "Live", "{f}");
            assert_eq!(t.cover.as_ref(), Some(&cover), "{f}");
        }
        assert!(press(&mut app, "q"), "nothing unsaved: q quits");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn per_track_edit_and_various() {
        let root = library("track");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        press(&mut app, "\nhj\njjj\n\x08\x08\x08\x08Intro\n"); // track 1 title
        assert_eq!(app.selection().len(), 1);
        assert!(screen_text(&mut app).contains("Filename"));
        press(&mut app, "kkkd"); // track 1 artist cleared
        press(&mut app, "hk"); // back to "all", still on Artist
        assert_eq!(app.track, 0);
        assert_eq!(app.selected_key(), Some(ItemKey::TrackArtist));
        assert!(screen_text(&mut app).contains(VARIOUS));
        // An untouched <various> field stays as it was.
        press(&mut app, "l");
        let before: Vec<_> = app.selection().iter().map(|t| t.draft.clone()).collect();
        app.editing = Some(Input::default());
        press(&mut app, "\n");
        let after: Vec<_> = app.selection().iter().map(|t| t.draft.clone()).collect();
        assert_eq!(before, after);

        // Numbers only in numeric fields; u drops everything.
        app.track = 1;
        app.field = 3;
        press(&mut app, "\n\x08x\n");
        assert!(app.status.contains("whole number"));
        press(&mut app, "u");
        assert!(!app.dirty());
        assert!(press(&mut app, "q"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn slash_opens_albums_and_songs() {
        let root = library("search");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        assert!(!press(&mut app, "/zzq"), "typing q-free text never quits");
        assert!(screen_text(&mut app).contains("no match"));
        press(&mut app, "\x1b");
        assert_eq!((app.screen, app.search.is_none()), (Screen::Albums, true));
        press(&mut app, "/ALBUM ARTIST");
        let text = screen_text(&mut app);
        assert!(text.contains("▸ Album — Album Artist"), "{text}");
        assert!(text.contains("♪ Song"), "{text}");
        press(&mut app, "\n");
        assert_eq!((app.screen, app.track), (Screen::Editor, 0));
        assert!(app.search.is_none());
        press(&mut app, "\x1b/song");
        app.key(KeyEvent::from(KeyCode::Down));
        press(&mut app, "\n");
        assert_eq!(app.screen, Screen::Editor);
        assert_eq!(app.selection()[0].path, app.rows[1].path);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn s_toggles_the_album_order_keeping_the_cursor() {
        let root = library("album-sort");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        let album = |album: &str, artist: &str| Album {
            artist: artist.into(),
            album: album.into(),
            year: String::new(),
            tracks: vec![],
        };
        app.albums = vec![album("Ato", "Lamp"), album("Timely!!", "Anri")];
        app.album_cursor = 0;
        press(&mut app, "s");
        assert_eq!(app.albums[0].album, "Timely!!");
        assert_eq!(app.albums[app.album_cursor].album, "Ato");
        assert!(screen_text(&mut app).contains("by artist"));
        press(&mut app, "s");
        assert_eq!((app.albums[0].album.as_str(), app.album_cursor), ("Ato", 0));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn v_shows_albums_as_rows_with_their_cover() {
        let root = library("album-rows");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        assert!(screen_text(&mut app).contains("· compact "));
        press(&mut app, "v");
        let screen = screen_text(&mut app);
        assert!(screen.contains("· rows "));
        assert!(screen.contains("Album Artist · 3 tracks"));
        assert!(app.thumbs.values().all(Option::is_some) && !app.thumbs.is_empty());
        press(&mut app, "vv");
        assert!(screen_text(&mut app).contains("· compact "));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn grid_moves_by_line_and_column() {
        let root = library("album-grid");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        let album = |album: String| Album {
            artist: "Lamp".into(),
            album,
            year: String::new(),
            tracks: vec![],
        };
        app.albums = (0..7).map(|i| album(format!("A{i}"))).collect();
        press(&mut app, "vv");
        assert!(screen_text(&mut app).contains("· grid "));
        assert_eq!(app.tile_cols, 5); // 98 cells inside the border, 18 per tile
        press(&mut app, "j");
        assert_eq!(app.album_cursor, 5);
        press(&mut app, "lj");
        assert_eq!(app.album_cursor, 6);
        press(&mut app, "hk");
        assert_eq!(app.album_cursor, 0);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn settings_round_trip() {
        let root = library("settings");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        let defaults = app.settings();
        press(&mut app, "vs\ts\n"); // rows, by artist, folders by date, open the folder
        let saved = app.settings();
        assert_eq!(
            saved,
            [
                ("view", "rows"),
                ("albums", "artist"),
                ("folders", "date"),
                ("screen", "folders")
            ]
        );
        let text: String = saved.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
        let mut fresh = App::new(root.clone(), Picker::halfblocks());
        fresh.apply(&config::parse(&text));
        assert_eq!(fresh.settings(), saved);
        let mut bad = App::new(root.clone(), Picker::halfblocks());
        bad.apply(&config::parse("view=huge\nscreen=editor"));
        assert_eq!(bad.settings(), defaults);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn folders_open_the_folder_at_a_track() {
        let root = library("folders");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Folders);
        press(&mut app, "ljj\n"); // expand Album, go to its second file
        assert_eq!(app.screen, Screen::Editor);
        let field = app.field;
        app.key(KeyEvent::from(KeyCode::Tab)); // next field, still in the editor
        assert_eq!((app.screen, app.field), (Screen::Editor, field + 1));
        assert_eq!(app.open.as_ref().unwrap().tracks.len(), 3);
        assert_eq!(app.selection()[0].path, root.join("Album/tiny.m4a"));
        press(&mut app, "\x1b");
        assert_eq!(app.screen, Screen::Folders);
        press(&mut app, "/song"); // songs open their folder at that track
        app.key(KeyEvent::from(KeyCode::Down));
        press(&mut app, "\n");
        assert_eq!(app.screen, Screen::Editor);
        assert_eq!(app.selection()[0].path, app.rows[1].path);
        press(&mut app, "\x1b/album\n"); // folders match by name
        assert_eq!(app.screen, Screen::Editor);
        assert_eq!(app.open.as_ref().unwrap().title, "Album");
        press(&mut app, "\x1b");
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Albums);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn audio_info_shows_ranges() {
        let root = library("info");
        let mut tracks: Vec<Track> = ["tiny.flac", "tiny.mp3"]
            .iter()
            .map(|f| Track::load(&root.join("Album").join(f)).unwrap())
            .collect();
        tracks[0].info.kbps = Some(900);
        tracks[1].info.kbps = Some(320);
        let info = audio_info(&tracks);
        assert_eq!(info[0], "flac, mp3");
        assert_eq!(info[2], "320–900 kbps");
        assert_eq!(audio_info(&tracks[..1])[1], "16 bit · 44.1 kHz");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn enter_on_the_open_album_keeps_the_draft() {
        let root = library("reopen");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        press(&mut app, "\njjjd\x1b"); // clear Genre, back to the albums
        assert_eq!(app.screen, Screen::Albums);
        press(&mut app, "\n");
        assert_eq!(app.screen, Screen::Editor, "no unsaved-changes warning");
        assert!(app.dirty());
        assert_eq!(app.selected_key(), Some(ItemKey::Genre));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn genre_picks_from_the_library() {
        let root = library("genre");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        app.rows[0].genres = vec!["Jazz".into()];
        press(&mut app, "\njjj\n"); // Genre opens the picker, on the current value
        let input = app.editing.as_ref().unwrap();
        assert_eq!(input.choices, ["Rock", "Jazz"]);
        assert_eq!(input.pick, 0);
        assert!(screen_text(&mut app).contains("│Jazz"));

        press(&mut app, "x\n"); // no invented genres
        assert!(app.editing.is_some());
        assert!(app.status.contains("not in the library"));

        press(&mut app, "\x08JA\n");
        assert!(app.editing.is_none());
        assert!(app
            .selection()
            .iter()
            .all(|t| tags::get(&t.draft, ItemKey::Genre) == "Jazz"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn artist_picks_from_the_library_or_takes_a_new_name() {
        let root = library("artist");
        let mut app = App::new(root.clone(), Picker::halfblocks());
        press(&mut app, "\n\n"); // Artist: track and album artists, on the current one
        let input = app.editing.as_ref().unwrap();
        assert_eq!(input.choices, ["Album Artist", "Artist"]);
        assert_eq!(input.pick, 1);

        press(&mut app, "Lamp");
        assert!(screen_text(&mut app).contains("Lamp  (new)"));
        press(&mut app, "\n");
        assert_eq!(app.selected_key(), Some(ItemKey::TrackArtist));
        assert_eq!(
            tags::common(app.selection(), ItemKey::TrackArtist),
            Some("Lamp")
        );

        app.key(KeyEvent::from(KeyCode::Tab)); // Album artist
        press(&mut app, "\nart");
        assert_eq!(
            app.editing.as_ref().unwrap().matches(),
            ["Album Artist", "Artist", "art"]
        );
        app.key(KeyEvent::from(KeyCode::Down));
        press(&mut app, "\n");
        assert_eq!(
            tags::common(app.selection(), ItemKey::AlbumArtist),
            Some("Artist")
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn hidden_fields_are_not_listed() {
        let mut t = Track::load(&Path::new(FIXTURES).join("tiny.flac")).unwrap();
        t.saved.insert(ItemKey::Isrc, "USX9P0000001".into());
        t.draft
            .insert(ItemKey::ReplayGainTrackGain, "-6.5 dB".into());
        t.draft.insert(ItemKey::Label, "ECM".into());
        let rows = field_rows(std::slice::from_ref(&t), false);
        assert!(rows.contains(&Some(ItemKey::Label)));
        assert!(!rows.contains(&Some(ItemKey::Isrc)));
        assert!(!rows.contains(&Some(ItemKey::ReplayGainTrackGain)));
    }
}
