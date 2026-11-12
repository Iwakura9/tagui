//! Three screens: albums grouped from the tags, the folder tree, and the tag editor for
//! a whole album or folder (track list on the left, fields and cover on the right).
//! Edits stay in each track's draft; files change only on Ctrl+s.

use crate::library::{self, Album, Tree};
use crate::tags::{self, Track, MAIN, MORE, NUMERIC};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use lofty::tag::ItemKey;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row as TableRow, Table, TableState,
};
use std::io::{stdout, Stdout};
use std::path::{Path, PathBuf};

/// Fields that differ per track by nature: not edited for a whole album at once.
const PER_TRACK: [ItemKey; 2] = [ItemKey::TrackNumber, ItemKey::TrackTitle];
/// Index of the first `MORE` field in the field list (after Filename and `MAIN`).
const FIRST_MORE: usize = 1 + MAIN.len();
const VARIOUS: &str = "<various>";

pub fn run(root: PathBuf) -> Result<()> {
    let _session = TerminalSession::enter(CrosstermTerminalOps)?;
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(stdout()))?;
    let scanning = format!("scanning {}…", root.display());
    terminal.draw(|f| f.render_widget(Paragraph::new(scanning.as_str()), f.area()))?;
    App::new(root).event_loop(&mut terminal)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Screen {
    Albums,
    Folders,
    Editor,
}

/// The album or folder in the editor.
struct Open {
    title: String,
    tracks: Vec<Track>,
    /// Where Esc goes back to.
    from: Screen,
}

/// Text being typed into a field, with the cursor as a char index.
struct Input {
    text: String,
    cursor: usize,
}

struct App {
    screen: Screen,
    albums: Vec<Album>,
    album_cursor: usize,
    tree: Tree,
    open: Option<Open>,
    /// 0 = all tracks, 1.. = `tracks[track - 1]`.
    track: usize,
    /// Index into `field_rows(selection)`.
    field: usize,
    focus_tracks: bool,
    editing: Option<Input>,
    /// An action that would drop unsaved edits asked once and waits for its repeat.
    confirm: bool,
    armed: bool,
    status: String,
}

impl App {
    fn new(root: PathBuf) -> Self {
        let rows = library::scan(&root);
        let albums = library::albums(&rows);
        let status = format!("{} tracks in {} albums", rows.len(), albums.len());
        Self {
            screen: Screen::Albums,
            albums,
            album_cursor: 0,
            tree: Tree::new(root),
            open: None,
            track: 0,
            field: 1,
            focus_tracks: false,
            editing: None,
            confirm: false,
            armed: false,
            status,
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
        if self.editing.is_some() && !ctrl_c {
            self.input_key(key.code);
            return false;
        }
        if ctrl_c || key.code == KeyCode::Char('q') {
            return self.may_drop_draft("q");
        }
        match key.code {
            KeyCode::Tab => {
                self.screen = match self.screen {
                    Screen::Albums => Screen::Folders,
                    Screen::Folders if self.open.is_some() => Screen::Editor,
                    _ => Screen::Albums,
                }
            }
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
        let c = &mut self.album_cursor;
        match code {
            KeyCode::Up | KeyCode::Char('k') => *c = c.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => *c = (*c + 1).min(last),
            KeyCode::PageUp => *c = c.saturating_sub(10),
            KeyCode::PageDown => *c = (*c + 10).min(last),
            KeyCode::Home => *c = 0,
            KeyCode::End => *c = last,
            KeyCode::Enter => {
                if let Some(a) = self.albums.get(self.album_cursor).cloned() {
                    self.open_tracks(&a.tracks, album_title(&a), None);
                }
            }
            _ => {}
        }
    }

    fn folders_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Up | KeyCode::Char('k') => self.tree.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.tree.move_by(1),
            KeyCode::PageUp => self.tree.move_by(-10),
            KeyCode::PageDown => self.tree.move_by(10),
            KeyCode::Left | KeyCode::Char('h') => self.tree.set_expanded(false),
            KeyCode::Right | KeyCode::Char('l') => self.tree.set_expanded(true),
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
                let tracks = library::audio_in(&dir);
                if tracks.is_empty() {
                    let open = self.tree.expanded.contains(&node.path);
                    self.tree.set_expanded(!open);
                    return;
                }
                let title = dir.file_name().unwrap_or_default().to_string_lossy();
                self.open_tracks(&tracks, title.into(), select);
            }
            _ => {}
        }
    }

    fn open_tracks(&mut self, paths: &[PathBuf], title: String, select: Option<&Path>) {
        if !self.may_drop_draft("Enter") {
            return;
        }
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
        self.field = self.field.min(FIRST_MORE - 1);
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
        field_rows(self.selection())
            .get(self.field)
            .copied()
            .flatten()
    }

    fn editor_key(&mut self, code: KeyCode) {
        let Some(tracks) = self.open.as_ref().map(|o| 1 + o.tracks.len()) else {
            return;
        };
        let fields = field_rows(self.selection()).len();
        match code {
            KeyCode::Esc => self.screen = self.open.as_ref().map_or(Screen::Albums, |o| o.from),
            KeyCode::Left | KeyCode::Char('h') => self.focus_tracks = true,
            KeyCode::Right | KeyCode::Char('l') => self.focus_tracks = false,
            KeyCode::Enter if self.focus_tracks => self.focus_tracks = false,
            KeyCode::Up | KeyCode::Char('k') if self.focus_tracks => {
                self.track = (self.track + tracks - 1) % tracks;
                self.track_changed();
            }
            KeyCode::Down | KeyCode::Char('j') if self.focus_tracks => {
                self.track = (self.track + 1) % tracks;
                self.track_changed();
            }
            KeyCode::Up | KeyCode::Char('k') => self.field = (self.field + fields - 1) % fields,
            KeyCode::Down | KeyCode::Char('j') => self.field = (self.field + 1) % fields,
            KeyCode::Enter => self.start_edit(),
            KeyCode::Char('d') => self.clear_field(),
            KeyCode::Char('u') => {
                for t in self.open.iter_mut().flat_map(|o| &mut o.tracks) {
                    t.discard();
                }
                self.status = "dropped unsaved changes".into();
            }
            _ => {}
        }
    }

    fn track_changed(&mut self) {
        self.field = self.field.min(field_rows(self.selection()).len() - 1);
    }

    /// Why the selected field can't be edited for the current selection, if it can't.
    fn locked(&self) -> Option<String> {
        let sel = self.selection();
        let Some(key) = self.selected_key() else {
            return Some("filenames are read-only".into());
        };
        if self.track == 0 && sel.len() > 1 && PER_TRACK.contains(&key) {
            return Some(format!("{} is edited track by track", tags::label(key)));
        }
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
        self.editing = Some(Input {
            cursor: text.chars().count(),
            text,
        });
    }

    fn input_key(&mut self, code: KeyCode) {
        let Some(mut input) = self.editing.take() else {
            return;
        };
        let len = input.text.chars().count();
        let byte = |text: &str, at: usize| text.char_indices().nth(at).map_or(text.len(), |c| c.0);
        match code {
            KeyCode::Esc => return,
            KeyCode::Enter => return self.commit(input.text),
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

    fn draw(&self, f: &mut ratatui::Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        match (self.screen, &self.open) {
            (Screen::Editor, Some(o)) => self.draw_editor(f, o, main),
            (Screen::Folders, _) => self.draw_tree(f, main),
            _ => self.draw_albums(f, main),
        }
        f.render_widget(Paragraph::new(self.status.as_str()), status);
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
        let title = format!(" albums · {} ", self.tree.root.display());
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(title))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
        let mut state = ListState::default().with_selected(Some(self.album_cursor));
        f.render_stateful_widget(list, area, &mut state);
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" {} ", self.tree.root.display())),
            )
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
        let [info_area, fields_area] =
            Layout::vertical([Constraint::Length(4), Constraint::Min(1)]).areas(inner);

        let sel = self.selection();
        let info: Vec<Line> = audio_info(sel).into_iter().map(Line::from).collect();
        f.render_widget(Paragraph::new(info), info_area);

        let rows = field_rows(sel);
        let focus = !self.focus_tracks;
        let mut table_rows = Vec::new();
        for (i, key) in rows.iter().enumerate() {
            if i == FIRST_MORE {
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
        let shown = self.field + usize::from(self.field >= FIRST_MORE);
        let table = Table::new(table_rows, [Constraint::Length(16), Constraint::Min(10)]);
        let mut state = TableState::default().with_selected(focus.then_some(shown));
        f.render_stateful_widget(table, fields_area, &mut state);
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
/// standard field the selected tracks have, by label.
fn field_rows(sel: &[Track]) -> Vec<Option<ItemKey>> {
    let listed = |k: &ItemKey| MAIN.iter().chain(&MORE).any(|(m, _)| m == k);
    let mut extra: Vec<ItemKey> = sel
        .iter()
        .flat_map(|t| t.draft.keys().chain(t.saved.keys()))
        .copied()
        .filter(|k| !listed(k))
        .collect();
    extra.sort_by_key(|k| tags::label(*k));
    extra.dedup();
    std::iter::once(None)
        .chain(MAIN.iter().chain(&MORE).map(|(k, _)| Some(*k)))
        .chain(extra.into_iter().map(Some))
        .collect()
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

    /// A library with one album (the three fixtures share their tags).
    fn library(test: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("tagui-tui-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("Album")).unwrap();
        for f in ["tiny.flac", "tiny.mp3", "tiny.m4a"] {
            std::fs::copy(Path::new(FIXTURES).join(f), root.join("Album").join(f)).unwrap();
        }
        root
    }

    fn press(app: &mut App, keys: &str) -> bool {
        let mut quit = false;
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                '\x08' => KeyCode::Backspace,
                '\x1b' => KeyCode::Esc,
                c => KeyCode::Char(c),
            };
            quit = app.key(KeyEvent::from(code));
        }
        quit
    }

    fn screen_text(app: &App) -> String {
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
    fn per_track_edit_and_various() {
        let root = library("track");
        let mut app = App::new(root.clone());
        press(&mut app, "\nhj\njjj\n\x08\x08\x08\x08Intro\n"); // track 1 title
        assert_eq!(app.selection().len(), 1);
        press(&mut app, "hk"); // back to "all"
        assert_eq!(app.track, 0);
        assert!(screen_text(&app).contains(VARIOUS));
        // An untouched <various> field stays as it was.
        press(&mut app, "l");
        let before: Vec<_> = app.selection().iter().map(|t| t.draft.clone()).collect();
        app.field = 4;
        app.editing = Some(Input {
            text: String::new(),
            cursor: 0,
        });
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
    fn folders_open_the_folder_at_a_track() {
        let root = library("folders");
        let mut app = App::new(root.clone());
        app.key(KeyEvent::from(KeyCode::Tab));
        assert_eq!(app.screen, Screen::Folders);
        press(&mut app, "ljj\n"); // expand Album, go to its second file
        assert_eq!(app.screen, Screen::Editor);
        assert_eq!(app.open.as_ref().unwrap().tracks.len(), 3);
        assert_eq!(app.selection()[0].path, root.join("Album/tiny.m4a"));
        press(&mut app, "\x1b");
        assert_eq!(app.screen, Screen::Folders);
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
}
