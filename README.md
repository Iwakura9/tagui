# tagui

A terminal editor for music tags. Browse your library by album or by folder, open a
whole album (or folder) at once, and edit its tags in a draft that is only written on
save.

## Install

```sh
cargo install --path .
```

## Usage

```sh
tagui [DIR]
```

`DIR` defaults to `~/.config/tagui/library`. Symlinking it to your music folder works
well.

The album view, both sort orders and the last screen (albums or folders) are remembered
in `~/.config/tagui/config`.

Albums are grouped by album artist (or artist) and album. Opening an album or a folder
shows its tracks on the left, with `all` on top: editing a field there writes it to every
track (Filename, Title and Track number only show track by track). A field whose tracks disagree
shows `<various>`. Filenames are read-only, and custom fields (`tidal_*`, `upc`, …), encoder, ISRC and
replay gain are not shown but are kept on save.

The cover is drawn with the terminal's graphics protocol (kitty, sixel…) through
[ratatui-image], or with half blocks elsewhere. `c` embeds the folder's `cover.jpg` /
`cover.png` as the front cover.

Saving copies each file next to itself, writes only the changed fields into the copy and
then replaces the original, so a failed write never leaves a half-written file.

## Keys

| Where | Key | Action |
|---|---|---|
| albums, folders | `Tab` | switch between albums and folders |
| anywhere | `q` | quit (asks twice when there are unsaved edits) |
| anywhere | `?` | key hints |
| albums | `j` / `k`, `PgUp` / `PgDn` | move |
| albums | `Enter` | open the album |
| albums, folders | `/` | search albums (or folders) and songs: `Enter` opens the result in the editor, `Tab` goes to it in the list, `Esc` closes |
| albums | `s` | sort by album / by artist |
| albums | `v` | view: compact list / rows with the cover / grid of covers |
| albums | `h` / `l` | move left / right (grid) |
| folders | `←` / `→` | collapse / expand |
| folders | `Enter` | open the folder's tracks (on a file: its folder, at that track) |
| folders | `s` | sort by name / newest first (modification time) |
| editor | `h` / `l` | focus tracks / fields |
| editor | `j` / `k` | move |
| editor | `Tab` | next field |
| editor | `Enter` | edit the field; Genre, Artist and Album artist pick from the library (typing filters, `↑` / `↓` choose; artists also take a new name) |
| editor | `d` | delete the field's value |
| editor | `c` | embed the folder cover |
| editor | `u` | undo (back to the saved tags) |
| editor | `Ctrl+s` | save |
| editor | `Esc` | back |

## Development

```sh
cargo test --all-targets
cargo clippy --all-targets -- -D warnings
```

`tests/fixtures/make.sh` regenerates the tiny test files (needs ffmpeg and mutagen).

[ratatui-image]: https://github.com/ratatui/ratatui-image

## License

[MIT](LICENSE)
