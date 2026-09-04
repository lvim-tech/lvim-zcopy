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
| `/`, then `n` / `N` | search, next / prev — every match is highlighted |
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
- **Search** highlights every match on screen, not only the one under the
  cursor, with the current one in a brighter colour. `n` and `N` step through
  *every* match — including a second one on the same line, which the older
  per-line search skipped. Both the stepping and the painting go through one
  matcher, so what `n` visits and what the screen lights up cannot become two
  different answers. Matches are counted in columns, not bytes, or a line with
  any Cyrillic in it would light up in the wrong place. `/` followed by Enter
  on an empty query clears the highlighting.
- **Colours.** Foreground *and* background, 16 / 256 / truecolour, plus bold,
  italic, underline and reverse. Backgrounds matter more than they sound: an
  editor paints its theme mostly with the background — the surface, the visual
  selection, the statusline, the cursor line — so a scrollback of a `nvim`
  session looked washed out until they were read. Both SGR spellings are
  understood, `38;2;R;G;B` and the subparameter form `38:2::R:G:B` (whose empty
  colour-space slot must be skipped, not read as the red component).
- The selection **inverts** whatever a cell already looks like rather than
  forcing one colour on it, so a cell that was already reversed comes back out
  of reverse inside the selection instead of being the one part that does not
  look selected.
- `o` hands **plain** text to nvim, deliberately: nvim would render the escapes
  as literal `[36m` noise. The colours travel beside the text instead — a
  `.spans` sidecar written next to the plain copy, which
  [lvim-ansi](https://github.com/lvim-tech/lvim-ansi) turns into extmarks:

  ```text
  /tmp/lvim-zcopy-plain-<pid>.txt     the plain text nvim opens
  /tmp/lvim-zcopy-plain-<pid>.spans   line<TAB>byte_start<TAB>byte_end<TAB>fg<TAB>bg<TAB>flags
  ```

  `line` is 0-based, `byte_end` is exclusive, offsets are **bytes** (extmarks
  count bytes; Cyrillic and Nerd Font glyphs are 2–4 of them each), `fg`/`bg`
  are `-` / `iN` / `#rrggbb`, and `flags` is a subset of `biur`. Ranges never
  overlap, come sorted, merge identical neighbours, and a fully-default range is
  not written at all. So the buffer stays plain — search, yank and the columns
  keep working, no escape ever reaches a register — while the ANSI is parsed
  exactly once, here, by the parser that is already tested. It is decoration
  only: without the sidecar, or without the plugin installed, `o` still opens
  nvim, just without the colours.
- Trailing blank lines are trimmed — empty ones and ones padded out with
  spaces — so a dump does not open with dead space below the output. "Blank"
  means blank *on screen*: spaces carrying a background, a reverse or an
  underline are a visible bar (a statusline is exactly that) and are kept.
- Cursor-positioning escapes are ignored, so a dump of a full-screen TUI
  (nvim's own screen, `htop`) is read as the text stream it is, not replayed as
  a grid.

## License

MIT
