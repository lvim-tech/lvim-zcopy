# lvim-zcopy

A keyboard **copy-mode** for [zellij](https://zellij.dev) panes — tmux-style
visual selection over the full scrollback, straight to the Wayland clipboard.
No mouse, no editor.

## Why

zellij has no copy-mode. In a pane you can only:

- select text with the **mouse** (and `copy_on_select`), or
- open the scrollback in your **`$EDITOR`** (`EditScrollback`).

There is no keyboard-driven cursor + visual selection inside the pane the way
tmux has. `lvim-zcopy` fills that gap: it is a tiny TUI that opens on the pane's
dumped scrollback, gives you a vi-style cursor and `v`/`V` selection, keeps the
terminal's ANSI colours, and yanks **plain** text to the clipboard.

## How it works

zellij's `EditScrollback` dumps a pane's **full** scrollback to a temp file and
opens it in `scrollback_editor`. Point `scrollback_editor` at `lvim-zcopy` and
bind a key to `EditScrollback { ansi true; }` — the `ansi true` keeps the
colours, which `lvim-zcopy` parses and renders. (A keybind `DumpScreen` can only
capture the viewport — zellij hardcodes no-scrollback there — so `EditScrollback`
is the only route to the whole history.)

Yank writes to both the **CLIPBOARD** and **PRIMARY** selections via `wl-copy`,
so it pastes with `Ctrl+V` *and* middle-click.

## Keys

| | |
|---|---|
| `h j k l`, arrows | move |
| `0` / `$` | line start / end |
| `g` / `G` | top / bottom |
| `Ctrl-f`/`Ctrl-b`, `PageUp`/`PageDown` | page |
| `Ctrl-d` / `Ctrl-u` | half page |
| `w` / `b` | word forward / back |
| `/`, then `n` / `N` | search, next / prev |
| `v` / `V` | char / line visual selection |
| `y` or `Enter` | yank selection (or current line) → clipboard, quit |
| `o` | open the same buffer in `nvim` |
| `q` / `Esc` | quit |

The cursor starts on the **last line** (like tmux copy-mode); scroll up from
there.

## Install

```sh
cargo build --release
install -m0755 target/release/lvim-zcopy ~/.local/bin/lvim-zcopy
```

## zellij config

```kdl
// config.kdl
scrollback_editor "/home/you/.local/bin/lvim-zcopy"

keybinds {
    tmux {  // or whichever mode
        bind "e" { EditScrollback { ansi true; }; SwitchToMode "normal"; }
        bind "y" { EditScrollback { ansi true; }; SwitchToMode "normal"; }
    }
}
```

Both keys open `lvim-zcopy` on the full, coloured scrollback; press `o` inside to
hand the same buffer to nvim.

## Notes

- Wayland only (uses `wl-copy`). For X11, swap the copy command in `main.rs` for
  `xclip -selection clipboard` / `-selection primary`.
- Selection, search and yank operate on plain text; the ANSI colours are a
  render-only layer, so copied text never carries escape codes.

## License

MIT
