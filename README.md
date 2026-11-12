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

## License

[MIT](LICENSE)
