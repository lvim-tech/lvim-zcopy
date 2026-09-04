// lvim-zcopy — a keyboard copy-mode for zellij panes.
//
// zellij has no tmux-style copy-mode: text selection is mouse-only, and the only
// keyboard route is opening the scrollback in $EDITOR. This is a tiny standalone
// TUI that a zellij keybind opens on a dumped scrollback file (via
// `EditScrollback { ansi true; }`, which preserves colours). It gives a movable
// cursor and vi-style visual selection, keeps the terminal's ANSI colours, and
// yanks PLAIN text straight to the Wayland clipboard (wl-copy, CLIPBOARD +
// PRIMARY) — no editor.
//
// Keys: h/j/k/l + arrows move · 0/$ line ends · g/G top/bottom
//       Ctrl-f/b (or PageUp/Down) page · Ctrl-d/u half page · w/b word
//       / search, n/N next/prev · v char-visual · V line-visual
//       y or Enter yank (selection or current line) then quit
//       o open the same buffer in nvim · q/Esc quit

use std::io::{stdout, Write};
use std::process::{Command, Stdio};

use crossterm::{
    cursor::MoveTo,
    event::{read, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    style::{
        Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
    },
    terminal::{
        disable_raw_mode, enable_raw_mode, size, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};

/// One character's appearance, as the dump described it.
///
/// `None` means "whatever the terminal's default is" — not black and not white,
/// because the pane this came from was drawn against the user's own background
/// and a guessed colour would be wrong on half the themes.
///
/// Doubles as the running style while the dump is parsed: SGR is a state
/// machine over exactly these fields, so the parser's state and a cell's
/// appearance are the same thing.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Cell {
    fg: Option<Color>,
    bg: Option<Color>,
    bold: bool,
    italic: bool,
    underline: bool,
    reverse: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Normal,
    VisualChar,
    VisualLine,
    Search,
}

struct App {
    lines: Vec<Vec<char>>,   // plain text — selection / search / yank work on this
    style: Vec<Vec<Cell>>,   // parallel per-char colours, for rendering only
    cy: usize,
    cx: usize,
    top: usize,
    left: usize,
    rows: usize,
    cols: usize,
    mode: Mode,
    anchor: (usize, usize),
    last_query: String,
    input: String,
    msg: String,
    quit: bool,
}

impl App {
    fn new(lines: Vec<Vec<char>>, style: Vec<Vec<Cell>>, cols: usize, rows: usize) -> Self {
        let cy = lines.len().saturating_sub(1); // start at the last line, like tmux copy-mode
        App {
            lines,
            style,
            cy,
            cx: 0,
            top: 0,
            left: 0,
            rows,
            cols,
            mode: Mode::Normal,
            anchor: (0, 0),
            last_query: String::new(),
            input: String::new(),
            msg: String::from("v/V select · y yank · / search · o nvim · q quit"),
            quit: false,
        }
    }

    fn line_len(&self, li: usize) -> usize {
        self.lines.get(li).map(|l| l.len()).unwrap_or(0)
    }

    fn clamp_cx(&mut self) {
        let len = self.line_len(self.cy);
        if self.cx > len {
            self.cx = len;
        }
    }

    fn ensure_visible(&mut self) {
        if self.cy < self.top {
            self.top = self.cy;
        }
        if self.cy >= self.top + self.rows {
            self.top = self.cy + 1 - self.rows;
        }
        if self.cx < self.left {
            self.left = self.cx;
        }
        if self.cx >= self.left + self.cols {
            self.left = self.cx + 1 - self.cols;
        }
    }

    fn sel_bounds(&self) -> ((usize, usize), (usize, usize)) {
        let a = self.anchor;
        let c = (self.cy, self.cx);
        if a <= c {
            (a, c)
        } else {
            (c, a)
        }
    }

    fn in_selection(&self, li: usize, col: usize) -> bool {
        match self.mode {
            Mode::VisualLine => {
                let ((sl, _), (el, _)) = self.sel_bounds();
                li >= sl && li <= el
            }
            Mode::VisualChar => {
                let ((sl, sc), (el, ec)) = self.sel_bounds();
                if li < sl || li > el {
                    return false;
                }
                if sl == el {
                    col >= sc && col <= ec
                } else if li == sl {
                    col >= sc
                } else if li == el {
                    col <= ec
                } else {
                    true
                }
            }
            _ => false,
        }
    }

    fn extract(&self) -> String {
        match self.mode {
            Mode::VisualLine => {
                let ((sl, _), (el, _)) = self.sel_bounds();
                (sl..=el)
                    .map(|li| self.lines[li].iter().collect::<String>())
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            Mode::VisualChar => {
                let ((sl, sc), (el, ec)) = self.sel_bounds();
                if sl == el {
                    let l = &self.lines[sl];
                    let end = (ec + 1).min(l.len());
                    let start = sc.min(end);
                    l[start..end].iter().collect()
                } else {
                    let mut out = String::new();
                    let first = &self.lines[sl];
                    let fs = sc.min(first.len());
                    out.extend(first[fs..].iter());
                    for li in (sl + 1)..el {
                        out.push('\n');
                        out.extend(self.lines[li].iter());
                    }
                    out.push('\n');
                    let last = &self.lines[el];
                    let le = (ec + 1).min(last.len());
                    out.extend(last[..le].iter());
                    out
                }
            }
            _ => self
                .lines
                .get(self.cy)
                .map(|l| l.iter().collect())
                .unwrap_or_default(),
        }
    }

    fn search(&mut self, forward: bool) {
        if self.last_query.is_empty() {
            return;
        }
        let n = self.lines.len();
        if n == 0 {
            return;
        }
        let needle: String = self.last_query.clone();
        for step in 1..=n {
            let li = if forward {
                (self.cy + step) % n
            } else {
                (self.cy + n - step) % n
            };
            let hay: String = self.lines[li].iter().collect();
            if let Some(bytepos) = hay.find(&needle) {
                let col = hay[..bytepos].chars().count();
                self.cy = li;
                self.cx = col;
                self.clamp_cx();
                self.msg = format!("/{}", self.last_query);
                return;
            }
        }
        self.msg = format!("not found: {}", self.last_query);
    }
}

fn copy_to_clipboard(text: &str) {
    for args in [&[][..], &["--primary"][..]] {
        if let Ok(mut child) = Command::new("wl-copy")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(si) = child.stdin.as_mut() {
                let _ = si.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn sgr_color(code: i64) -> Option<Color> {
    match code {
        30..=37 => Some(Color::AnsiValue((code - 30) as u8)),
        90..=97 => Some(Color::AnsiValue((code - 90 + 8) as u8)),
        _ => None,
    }
}

fn apply_sgr(params: &str, st: &mut Cell) {
    // A field can be empty — `\x1b[;1m`, and the colour-space slot in
    // `38:2::R:G:B` — so a field is an Option, not a number defaulted to 0.
    // Read as 0, that empty slot would be taken for the red component and the
    // colour would come out shifted.
    let codes: Vec<Option<i64>> = if params.is_empty() {
        vec![Some(0)]
    } else {
        params
            .split([';', ':'])
            .map(|f| if f.is_empty() { None } else { f.parse().ok() })
            .collect()
    };

    let mut k = 0;
    while k < codes.len() {
        let Some(code) = codes[k] else {
            k += 1;
            continue;
        };
        match code {
            0 => *st = Cell::default(),
            1 => st.bold = true,
            3 => st.italic = true,
            4 => st.underline = true,
            7 => st.reverse = true,
            22 => st.bold = false,
            23 => st.italic = false,
            24 => st.underline = false,
            27 => st.reverse = false,
            30..=37 | 90..=97 => st.fg = sgr_color(code),
            39 => st.fg = None,
            // The backgrounds. Their absence is what made a neovim scrollback
            // look colourless: an editor paints its theme mostly with the
            // background — the whole surface, the visual selection, the
            // statusline, the cursor line — and only some of it with the
            // foreground.
            40..=47 | 100..=107 => st.bg = sgr_color(code - 10),
            49 => st.bg = None,
            38 | 48 => {
                if let Some((colour, used)) = extended_colour(&codes[k + 1..]) {
                    if code == 38 {
                        st.fg = Some(colour);
                    } else {
                        st.bg = Some(colour);
                    }
                    k += used;
                }
            }
            _ => {}
        }
        k += 1;
    }
}

/// The colour after a `38`/`48`, and how many fields it took.
///
/// Two forms: `5;n` for a palette index, `2;r;g;b` for a truecolour. Empty
/// fields are skipped rather than counted, which is what makes the colon form
/// (`38:2::R:G:B`, with an empty colour-space slot) read the same as the
/// semicolon one.
///
/// `None` for anything else — a truncated sequence at the end of the params, or
/// a form we do not know. The style then simply keeps the colour it had, which
/// is the only safe answer: guessing a colour here paints the wrong one for the
/// rest of the line.
fn extended_colour(rest: &[Option<i64>]) -> Option<(Color, usize)> {
    let mut want = 0usize;
    let mut vals: Vec<i64> = Vec::new();
    for (i, field) in rest.iter().enumerate() {
        let Some(v) = *field else { continue };
        if want == 0 {
            want = match v {
                5 => 1,
                2 => 3,
                _ => return None,
            };
            continue;
        }
        vals.push(v);
        if vals.len() == want {
            let colour = match vals[..] {
                [n] => Color::AnsiValue(n.clamp(0, 255) as u8),
                [r, g, b] => Color::Rgb {
                    r: r.clamp(0, 255) as u8,
                    g: g.clamp(0, 255) as u8,
                    b: b.clamp(0, 255) as u8,
                },
                _ => return None,
            };
            return Some((colour, i + 1));
        }
    }
    None
}

/// Whether a line would leave the screen as it found it.
///
/// Every character is whitespace AND no cell carries something that paints on a
/// space: a background, a reverse (which turns the foreground into one), or an
/// underline. Foreground and bold are invisible on a space and do not count.
fn paints_nothing(line: &[char], style: &[Cell]) -> bool {
    line.iter().all(|c| c.is_whitespace())
        && style
            .iter()
            .all(|c| c.bg.is_none() && !c.reverse && !c.underline)
}

// Parse the (possibly ANSI-styled) dump into plain chars + a parallel colour map.
fn load(path: &str) -> (Vec<Vec<char>>, Vec<Vec<Cell>>) {
    let raw = std::fs::read(path).unwrap_or_default();
    let text = String::from_utf8_lossy(&raw);
    let ch: Vec<char> = text.chars().collect();
    let len = ch.len();

    let mut lines: Vec<Vec<char>> = Vec::new();
    let mut styles: Vec<Vec<Cell>> = Vec::new();
    let mut cur_c: Vec<char> = Vec::new();
    let mut cur_s: Vec<Cell> = Vec::new();
    let mut style = Cell::default();

    let is_final = |c: char| {
        let u = c as u32;
        (0x40..=0x7e).contains(&u)
    };

    let mut i = 0;
    while i < len {
        let c = ch[i];
        if c == '\u{1b}' {
            if i + 1 < len && ch[i + 1] == '[' {
                // CSI ... final
                let mut j = i + 2;
                let mut params = String::new();
                while j < len && !is_final(ch[j]) {
                    params.push(ch[j]);
                    j += 1;
                }
                let fin = if j < len { ch[j] } else { '\0' };
                if fin == 'm' {
                    // Keep ':' as well as ';': SGR has a subparameter form and
                    // real programs use it — `4:3` for a curly underline, and
                    // `38:2::R:G:B` for a truecolour. Dropping the colons used
                    // to turn those into one unparseable number, which is a
                    // silent loss of exactly the colours this parser is for.
                    // Private markers ('?', '>') are still dropped.
                    let p: String = params
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == ';' || *c == ':')
                        .collect();
                    apply_sgr(&p, &mut style);
                }
                i = j + 1;
                continue;
            } else if i + 1 < len && ch[i + 1] == ']' {
                // OSC ... BEL or ST
                let mut j = i + 2;
                while j < len && ch[j] != '\u{07}' && !(ch[j] == '\u{1b}' && j + 1 < len && ch[j + 1] == '\\') {
                    j += 1;
                }
                if j < len && ch[j] == '\u{1b}' {
                    j += 1;
                }
                i = j + 1;
                continue;
            } else {
                i += 2;
                continue;
            }
        } else if c == '\n' {
            lines.push(std::mem::take(&mut cur_c));
            styles.push(std::mem::take(&mut cur_s));
            i += 1;
        } else if c == '\r' {
            i += 1;
        } else {
            cur_c.push(c);
            cur_s.push(style);
            i += 1;
        }
    }
    lines.push(cur_c);
    styles.push(cur_s);

    // A dump ends in whatever was below the last output: empty lines, and lines
    // padded out with spaces. Both are dead space at the bottom of copy mode.
    //
    // "Blank" has to mean blank on screen, not blank in the text. Spaces with a
    // background, a reverse or an underline are a visible bar — a statusline is
    // exactly that — and trimming one would delete something the pane showed.
    while lines.len() > 1
        && lines
            .last()
            .zip(styles.last())
            .is_some_and(|(l, s)| paints_nothing(l, s))
    {
        lines.pop();
        styles.pop();
    }
    if lines.is_empty() {
        lines.push(Vec::new());
        styles.push(Vec::new());
    }
    (lines, styles)
}

/// Establish `cell` on the terminal, from a clean slate.
///
/// A full reset first, then only what is on. Emitting the whole style rather
/// than the difference from the previous one is a few more bytes on a colourful
/// line and removes the entire class of bugs where an attribute is turned on
/// and never turned back off — a stray bold or, worse, a background that runs
/// to the end of the screen.
fn write_style(out: &mut impl Write, cell: Cell) -> std::io::Result<()> {
    queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    if let Some(fg) = cell.fg {
        queue!(out, SetForegroundColor(fg))?;
    }
    if let Some(bg) = cell.bg {
        queue!(out, SetBackgroundColor(bg))?;
    }
    if cell.bold {
        queue!(out, SetAttribute(Attribute::Bold))?;
    }
    if cell.italic {
        queue!(out, SetAttribute(Attribute::Italic))?;
    }
    if cell.underline {
        queue!(out, SetAttribute(Attribute::Underlined))?;
    }
    if cell.reverse {
        queue!(out, SetAttribute(Attribute::Reverse))?;
    }
    Ok(())
}

fn draw(app: &App) -> std::io::Result<()> {
    let mut out = stdout();
    for r in 0..app.rows {
        queue!(
            out,
            MoveTo(0, r as u16),
            SetAttribute(Attribute::Reset),
            ResetColor,
            Clear(ClearType::CurrentLine)
        )?;
        let li = app.top + r;
        if li >= app.lines.len() {
            continue; // blank — no ~ column
        }
        let line = &app.lines[li];
        let styl = &app.style[li];
        let end = (app.left + app.cols).min(line.len());
        // The style last written to the terminal. The row began with a reset,
        // so that is the default cell; a run of identically-styled characters
        // then costs one escape sequence, however long it is.
        let mut applied = Cell::default();
        for (col, ch) in line.iter().enumerate().take(end).skip(app.left) {
            let mut want = styl.get(col).copied().unwrap_or_default();
            // The selection inverts whatever the cell already looks like rather
            // than forcing one colour on it: a cell that is already reversed
            // (a statusline, a highlighted match) has to come back OUT of
            // reverse inside the selection, or it would be the one part of the
            // selection that does not look selected.
            want.reverse ^= app.in_selection(li, col);
            if want != applied {
                write_style(&mut out, want)?;
                applied = want;
            }
            queue!(out, Print(ch))?;
        }
        queue!(out, SetAttribute(Attribute::Reset), ResetColor)?;
    }

    // ---- coloured status bar ----
    queue!(out, MoveTo(0, app.rows as u16), SetAttribute(Attribute::Reset), ResetColor)?;
    let mut used = 0usize;
    if app.mode == Mode::Search {
        let prompt = format!(" SEARCH /{} ", app.input);
        queue!(out, SetForegroundColor(Color::Black), SetBackgroundColor(Color::Cyan), Print(&prompt))?;
        used += prompt.chars().count();
    } else {
        let (mbg, mtxt) = match app.mode {
            Mode::VisualChar => (Color::Magenta, " VISUAL "),
            Mode::VisualLine => (Color::Magenta, " V-LINE "),
            _ => (Color::Blue, " NORMAL "),
        };
        queue!(out, SetForegroundColor(Color::Black), SetBackgroundColor(mbg), Print(mtxt))?;
        used += mtxt.chars().count();
        let pos = format!(" {}:{}/{} ", app.cy + 1, app.cx + 1, app.lines.len());
        queue!(out, SetForegroundColor(Color::White), SetBackgroundColor(Color::DarkGrey), Print(&pos))?;
        used += pos.chars().count();
        let help = format!(" {} ", app.msg);
        queue!(out, SetForegroundColor(Color::Grey), SetBackgroundColor(Color::Reset), Print(&help))?;
        used += help.chars().count();
    }
    if used < app.cols {
        let fill = " ".repeat(app.cols - used);
        queue!(out, SetBackgroundColor(Color::Reset), Print(fill))?;
    }
    queue!(out, ResetColor)?;

    if app.mode == Mode::Search {
        let cxp = (9 + app.input.chars().count()).min(app.cols.saturating_sub(1));
        queue!(out, MoveTo(cxp as u16, app.rows as u16))?;
    } else {
        let scr_y = (app.cy - app.top) as u16;
        let scr_x = (app.cx - app.left) as u16;
        queue!(out, MoveTo(scr_x, scr_y))?;
    }
    out.flush()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn main() {
    let path = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('+'))
        .unwrap_or_default();
    if path.is_empty() {
        eprintln!("usage: lvim-zcopy <dump-file>");
        std::process::exit(2);
    }
    for _ in 0..24 {
        if std::fs::metadata(&path).map(|m| m.len() > 0).unwrap_or(false) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    let (lines, style) = load(&path);
    let (cols, rows) = size().unwrap_or((80, 24));
    let mut app = App::new(lines, style, cols as usize, (rows as usize).saturating_sub(1).max(1));

    enable_raw_mode().ok();
    execute!(stdout(), EnterAlternateScreen).ok();

    let mut copied: Option<String> = None;
    let mut open_nvim = false;
    app.ensure_visible(); // start scrolled to the bottom (cursor on the last line)
    let _ = draw(&app);

    while !app.quit {
        let ev = match read() {
            Ok(e) => e,
            Err(_) => break,
        };
        let (code, mods) = match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => (k.code, k.modifiers),
            Event::Resize(w, h) => {
                app.cols = w as usize;
                app.rows = (h as usize).saturating_sub(1).max(1);
                app.ensure_visible();
                let _ = draw(&app);
                continue;
            }
            _ => continue,
        };
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let last = app.lines.len().saturating_sub(1);

        if app.mode == Mode::Search {
            match code {
                KeyCode::Char(c) => app.input.push(c),
                KeyCode::Backspace => {
                    app.input.pop();
                }
                KeyCode::Enter => {
                    app.last_query = std::mem::take(&mut app.input);
                    app.mode = Mode::Normal;
                    app.search(true);
                }
                KeyCode::Esc => {
                    app.input.clear();
                    app.mode = Mode::Normal;
                    app.msg = "search cancelled".into();
                }
                _ => {}
            }
            app.ensure_visible();
            let _ = draw(&app);
            continue;
        }

        match code {
            KeyCode::Char('q') => app.quit = true,
            KeyCode::Esc => {
                if app.mode != Mode::Normal {
                    app.mode = Mode::Normal;
                    app.msg = String::new();
                } else {
                    app.quit = true;
                }
            }
            KeyCode::Char('c') if ctrl => app.quit = true,
            KeyCode::Char('j') | KeyCode::Down => {
                if app.cy < last {
                    app.cy += 1;
                    app.clamp_cx();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if app.cy > 0 {
                    app.cy -= 1;
                    app.clamp_cx();
                }
            }
            KeyCode::Char('h') | KeyCode::Left => {
                if app.cx > 0 {
                    app.cx -= 1;
                }
            }
            KeyCode::Char('l') | KeyCode::Right => {
                if app.cx < app.line_len(app.cy) {
                    app.cx += 1;
                }
            }
            KeyCode::Char('0') | KeyCode::Home => app.cx = 0,
            KeyCode::Char('$') | KeyCode::End => {
                app.cx = app.line_len(app.cy);
                if app.cx > 0 {
                    app.cx -= 1;
                }
            }
            KeyCode::Char('g') => {
                app.cy = 0;
                app.clamp_cx();
            }
            KeyCode::Char('G') => {
                app.cy = last;
                app.clamp_cx();
            }
            KeyCode::Char('w') => {
                let l = &app.lines[app.cy];
                let mut i = app.cx;
                while i < l.len() && is_word(l[i]) {
                    i += 1;
                }
                while i < l.len() && !is_word(l[i]) {
                    i += 1;
                }
                app.cx = i;
            }
            KeyCode::Char('b') if !ctrl => {
                let l = &app.lines[app.cy];
                let mut i = app.cx;
                while i > 0 && !is_word(l[i - 1]) {
                    i -= 1;
                }
                while i > 0 && is_word(l[i - 1]) {
                    i -= 1;
                }
                app.cx = i;
            }
            KeyCode::PageDown => {
                app.cy = (app.cy + app.rows).min(last);
                app.clamp_cx();
            }
            KeyCode::PageUp => {
                app.cy = app.cy.saturating_sub(app.rows);
                app.clamp_cx();
            }
            KeyCode::Char('f') if ctrl => {
                app.cy = (app.cy + app.rows).min(last);
                app.clamp_cx();
            }
            KeyCode::Char('b') if ctrl => {
                app.cy = app.cy.saturating_sub(app.rows);
                app.clamp_cx();
            }
            KeyCode::Char('d') if ctrl => {
                app.cy = (app.cy + app.rows / 2).min(last);
                app.clamp_cx();
            }
            KeyCode::Char('u') if ctrl => {
                app.cy = app.cy.saturating_sub(app.rows / 2);
                app.clamp_cx();
            }
            KeyCode::Char('v') => {
                if app.mode == Mode::VisualChar {
                    app.mode = Mode::Normal;
                } else {
                    app.mode = Mode::VisualChar;
                    app.anchor = (app.cy, app.cx);
                }
            }
            KeyCode::Char('V') => {
                if app.mode == Mode::VisualLine {
                    app.mode = Mode::Normal;
                } else {
                    app.mode = Mode::VisualLine;
                    app.anchor = (app.cy, app.cx);
                }
            }
            KeyCode::Char('/') => {
                app.mode = Mode::Search;
                app.input.clear();
            }
            KeyCode::Char('n') => app.search(true),
            KeyCode::Char('N') => app.search(false),
            KeyCode::Char('o') => {
                open_nvim = true;
                app.quit = true;
            }
            KeyCode::Char('y') | KeyCode::Enter => {
                let text = app.extract();
                if !text.is_empty() {
                    copied = Some(text);
                }
                app.quit = true;
            }
            _ => {}
        }
        app.ensure_visible();
        let _ = draw(&app);
    }

    execute!(stdout(), LeaveAlternateScreen).ok();
    disable_raw_mode().ok();

    if open_nvim {
        use std::os::unix::process::CommandExt;
        // The dump file carries ANSI escapes (EditScrollback ansi=true), which nvim
        // would show as raw `[36m` noise. Hand nvim a plain, stripped copy instead —
        // we already parsed the text into `lines`.
        let plain: String = app
            .lines
            .iter()
            .map(|l| l.iter().collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        let tmp = format!("/tmp/lvim-zcopy-plain-{}.txt", std::process::id());
        let target = if std::fs::write(&tmp, plain).is_ok() {
            tmp
        } else {
            path.clone()
        };
        let err = Command::new("nvim").arg(&target).exec();
        eprintln!("lvim-zcopy: could not exec nvim: {err}");
        std::process::exit(1);
    }

    if let Some(text) = copied {
        copy_to_clipboard(&text);
        eprintln!("lvim-zcopy: copied {} chars", text.chars().count());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The style a run of SGR parameters leaves behind.
    fn style(params: &[&str]) -> Cell {
        let mut cell = Cell::default();
        for p in params {
            apply_sgr(p, &mut cell);
        }
        cell
    }

    #[test]
    fn backgrounds_are_read() {
        // The whole point: an editor paints its theme with the background.
        assert_eq!(style(&["41"]).bg, Some(Color::AnsiValue(1)));
        assert_eq!(style(&["104"]).bg, Some(Color::AnsiValue(12)));
        assert_eq!(style(&["48;5;236"]).bg, Some(Color::AnsiValue(236)));
        assert_eq!(
            style(&["48;2;40;44;52"]).bg,
            Some(Color::Rgb { r: 40, g: 44, b: 52 })
        );
        // ...and are given back.
        assert_eq!(style(&["41", "49"]).bg, None);
        assert_eq!(style(&["41", "0"]).bg, None);
    }

    #[test]
    fn a_background_does_not_disturb_the_foreground() {
        let cell = style(&["38;5;203;48;5;236"]);
        assert_eq!(cell.fg, Some(Color::AnsiValue(203)));
        assert_eq!(cell.bg, Some(Color::AnsiValue(236)));
    }

    #[test]
    fn the_colon_form_reads_the_same_as_the_semicolon_form() {
        // `38:2::R:G:B` carries an empty colour-space field. Read as a zero it
        // would be taken for the red component and every value would shift by
        // one — a wrong colour rather than a missing one.
        let semi = style(&["38;2;255;128;0"]);
        let colon = style(&["38:2::255:128:0"]);
        assert_eq!(colon.fg, semi.fg);
        assert_eq!(
            colon.fg,
            Some(Color::Rgb { r: 255, g: 128, b: 0 })
        );
        // The short colon form, without the empty slot.
        assert_eq!(style(&["48:5:236"]).bg, Some(Color::AnsiValue(236)));
    }

    #[test]
    fn attributes_go_on_and_off() {
        let on = style(&["1;3;4;7"]);
        assert!(on.bold && on.italic && on.underline && on.reverse);
        let off = style(&["1;3;4;7", "22;23;24;27"]);
        assert!(!off.bold && !off.italic && !off.underline && !off.reverse);
        // A styled underline (`4:3` is curly) is still an underline.
        assert!(style(&["4:3"]).underline);
    }

    #[test]
    fn a_reset_clears_everything() {
        let cell = style(&["1;4;38;5;9;48;5;236", "0"]);
        assert_eq!(cell, Cell::default());
        // An empty parameter list means 0.
        assert_eq!(style(&["1;41", ""]), Cell::default());
    }

    #[test]
    fn a_truncated_colour_leaves_the_style_alone() {
        // Keeping the previous colour is the only safe answer: a guess here
        // paints the wrong colour for the rest of the line.
        let cell = style(&["31", "38;2;255"]);
        assert_eq!(cell.fg, Some(Color::AnsiValue(1)));
        assert!(extended_colour(&[Some(9)]).is_none());
    }

    /// Write a dump, parse it, clean up. The parser's only real input is a file.
    fn parse_file(tag: &str, body: &str) -> (Vec<Vec<char>>, Vec<Vec<Cell>>) {
        let dir = std::env::temp_dir().join(format!(
            "lvim-zcopy-test-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("dump");
        std::fs::write(&path, body).expect("write");
        let out = load(path.to_str().unwrap_or_default());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
        out
    }

    #[test]
    fn trailing_blank_lines_are_trimmed_however_they_are_spelled() {
        // Empty, and padded out with spaces: both are dead space below the
        // output, and a pane dump produces both.
        let (lines, _) = parse_file("trim", "text\n\n    \n\t\n\n");
        assert_eq!(lines.len(), 1, "got {lines:?}");
        assert_eq!(lines[0].iter().collect::<String>(), "text");
    }

    #[test]
    fn a_coloured_bar_of_spaces_is_not_blank() {
        // A statusline is spaces with a background. Trimming it would delete
        // something the pane actually showed.
        let (lines, style) = parse_file("bar", "text\n\x1b[48;5;236m      \x1b[0m\n");
        assert_eq!(lines.len(), 2, "the coloured bar was trimmed: {lines:?}");
        assert_eq!(style[1][0].bg, Some(Color::AnsiValue(236)));
        // Reverse and underline paint on a space too.
        let (lines, _) = parse_file("rev", "text\n\x1b[7m   \x1b[0m\n");
        assert_eq!(lines.len(), 2, "a reversed bar was trimmed");
        let (lines, _) = parse_file("ul", "text\n\x1b[4m   \x1b[0m\n");
        assert_eq!(lines.len(), 2, "an underlined bar was trimmed");
        // ...but a plain foreground on spaces is invisible, so it is blank.
        let (lines, _) = parse_file("fg", "text\n\x1b[31m   \x1b[0m\n");
        assert_eq!(lines.len(), 1, "an invisible line was kept");
    }

    #[test]
    fn the_dump_becomes_lines_with_a_colour_for_every_character() {
        let dir = std::env::temp_dir().join(format!("lvim-zcopy-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("dump");
        std::fs::write(&path, "\x1b[48;5;236m\x1b[38;5;203mred\x1b[0m plain\nsecond\n")
            .expect("write");

        let (lines, style) = load(path.to_str().unwrap_or_default());
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].iter().collect::<String>(), "red plain");
        assert_eq!(lines[1].iter().collect::<String>(), "second");
        // The escapes are gone from the text but not from the appearance.
        assert_eq!(style[0][0].bg, Some(Color::AnsiValue(236)));
        assert_eq!(style[0][0].fg, Some(Color::AnsiValue(203)));
        assert_eq!(style[0][4].bg, None, "the reset must end the background");
    }
}
