//! Three screens: albums grouped from the tags, the folder tree, and the tag editor for
//! a whole album or folder (track list on the left, fields and cover on the right).
//! Edits stay in each track's draft; files change only on Ctrl+s.

use crate::library::{self, Album, Tree};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use std::io::{stdout, Stdout};
use std::path::PathBuf;

pub fn run(root: PathBuf) -> Result<()> {
    let _session = TerminalSession::enter(CrosstermTerminalOps)?;
    let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(stdout()))?;
    let scanning = format!("scanning {}…", root.display());
    terminal.draw(|f| f.render_widget(Paragraph::new(scanning.as_str()), f.area()))?;
    App::new(root).event_loop(&mut terminal)
}

struct App {
    albums: Vec<Album>,
    album_cursor: usize,
    tree: Tree,
    status: String,
}

impl App {
    fn new(root: PathBuf) -> Self {
        let rows = library::scan(&root);
        let albums = library::albums(&rows);
        let status = format!("{} tracks in {} albums", rows.len(), albums.len());
        Self {
            albums,
            album_cursor: 0,
            tree: Tree::new(root),
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

    /// Handle one key press; true means quit.
    fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ctrl_c = ctrl && key.code == KeyCode::Char('c');
        if ctrl_c || key.code == KeyCode::Char('q') {
            return true;
        }
        self.albums_key(key.code);
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
            _ => {}
        }
    }

    fn draw(&self, f: &mut ratatui::Frame) {
        let [main, status] =
            Layout::vertical([Constraint::Min(5), Constraint::Length(1)]).areas(f.area());
        self.draw_albums(f, main);
        f.render_widget(Paragraph::new(self.status.as_str()), status);
    }

    fn draw_albums(&self, f: &mut ratatui::Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .albums
            .iter()
            .map(|a| {
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
}

fn or_none<'a>(s: &'a str, none: &'a str) -> &'a str {
    if s.is_empty() {
        none
    } else {
        s
    }
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
