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

#[derive(Clone, Copy)]
struct Cell {
    fg: Option<Color>,
    bold: bool,
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

fn apply_sgr(params: &str, fg: &mut Option<Color>, bold: &mut bool) {
    let codes: Vec<i64> = if params.is_empty() {
        vec![0]
    } else {
        params.split(';').map(|s| s.parse().unwrap_or(0)).collect()
    };
    let mut k = 0;
    while k < codes.len() {
        match codes[k] {
            0 => {
                *fg = None;
                *bold = false;
            }
            1 => *bold = true,
            22 => *bold = false,
            30..=37 | 90..=97 => *fg = sgr_color(codes[k]),
            39 => *fg = None,
            38 => {
                if k + 2 < codes.len() && codes[k + 1] == 5 {
                    *fg = Some(Color::AnsiValue(codes[k + 2] as u8));
                    k += 2;
                } else if k + 4 < codes.len() && codes[k + 1] == 2 {
                    *fg = Some(Color::Rgb {
                        r: codes[k + 2] as u8,
                        g: codes[k + 3] as u8,
                        b: codes[k + 4] as u8,
                    });
                    k += 4;
                }
            }
            _ => {}
        }
        k += 1;
    }
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
    let mut fg: Option<Color> = None;
    let mut bold = false;

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
                    // strip a leading '?' or private markers defensively
                    let p: String = params.chars().filter(|c| c.is_ascii_digit() || *c == ';').collect();
                    apply_sgr(&p, &mut fg, &mut bold);
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
            cur_s.push(Cell { fg, bold });
            i += 1;
        }
    }
    lines.push(cur_c);
    styles.push(cur_s);

    while lines.len() > 1 && lines.last().map(|l| l.is_empty()).unwrap_or(false) {
        lines.pop();
        styles.pop();
    }
    if lines.is_empty() {
        lines.push(Vec::new());
        styles.push(Vec::new());
    }
    (lines, styles)
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
        let mut col = app.left;
        let mut rev = false;
        let mut cur_fg: Option<Color> = Some(Color::Reset);
        let mut cur_bold = false;
        while col < end {
            let sel = app.in_selection(li, col);
            if sel != rev {
                queue!(
                    out,
                    SetAttribute(if sel {
                        Attribute::Reverse
                    } else {
                        Attribute::NoReverse
                    })
                )?;
                rev = sel;
            }
            let cell = styl.get(col).copied().unwrap_or(Cell { fg: None, bold: false });
            let want_fg = cell.fg.or(Some(Color::Reset));
            if want_fg != cur_fg {
                queue!(out, SetForegroundColor(want_fg.unwrap()))?;
                cur_fg = want_fg;
            }
            if cell.bold != cur_bold {
                queue!(
                    out,
                    SetAttribute(if cell.bold {
                        Attribute::Bold
                    } else {
                        Attribute::NormalIntensity
                    })
                )?;
                cur_bold = cell.bold;
            }
            queue!(out, Print(line[col]))?;
            col += 1;
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
        let fill: String = std::iter::repeat(' ').take(app.cols - used).collect();
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
