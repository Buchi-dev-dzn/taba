//! Drawing the TUI: header with the logo, the scope tree or a log, the
//! notice line and the key hints. Pure: renders `App` into a frame.

use super::app::{App, Item, Mode, Row};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Below this width the header is one line.
pub const NARROW: u16 = 40;

/// How much color the terminal gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Colors {
    /// `NO_COLOR` is set: no colors at all, only bold and reverse.
    Off,
    /// The 256-color palette (for example macOS Terminal).
    Ansi256,
    /// 24-bit color.
    TrueColor,
}

impl Colors {
    pub fn detect() -> Colors {
        Colors::from_env(|k| std::env::var(k).ok())
    }

    /// `NO_COLOR` (non-empty) turns colors off. 24-bit color is used when
    /// `COLORTERM` says so, in Windows Terminal (`WT_SESSION`), and in
    /// iTerm2, WezTerm and VS Code; elsewhere the 256-color palette.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Colors {
        if get("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Colors::Off;
        }
        let colorterm = get("COLORTERM").unwrap_or_default().to_lowercase();
        let term_program = get("TERM_PROGRAM").unwrap_or_default();
        if colorterm == "truecolor"
            || colorterm == "24bit"
            || get("WT_SESSION").is_some()
            || matches!(term_program.as_str(), "iTerm.app" | "WezTerm" | "vscode")
        {
            Colors::TrueColor
        } else {
            Colors::Ansi256
        }
    }

    fn rgb(self, r: u8, g: u8, b: u8) -> Style {
        match self {
            Colors::Off => Style::default(),
            Colors::Ansi256 => Style::default().fg(Color::Indexed(ansi256(r, g, b))),
            Colors::TrueColor => Style::default().fg(Color::Rgb(r, g, b)),
        }
    }

    fn basic(self, c: Color) -> Style {
        match self {
            Colors::Off => Style::default(),
            _ => Style::default().fg(c),
        }
    }

    /// The logo's orange, `#FF5533`.
    pub fn orange(self) -> Style {
        self.rgb(0xFF, 0x55, 0x33)
    }

    /// The logo's cream, `#FFE0BF`.
    pub fn cream(self) -> Style {
        self.rgb(0xFF, 0xE0, 0xBF)
    }

    fn status(self, status: &str) -> Style {
        match status {
            "running" => self.basic(Color::Green),
            "exited" => self.basic(Color::DarkGray),
            _ => self.basic(Color::Yellow),
        }
    }

    fn error(self) -> Style {
        self.basic(Color::Red)
    }

    fn dim(self) -> Style {
        match self {
            Colors::Off => Style::default(),
            _ => Style::default().fg(Color::DarkGray),
        }
    }
}

/// The nearest xterm 256-color index (6x6x6 cube or gray ramp).
pub fn ansi256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [i32; 6] = [0, 95, 135, 175, 215, 255];
    let near = |v: u8| {
        (0..6)
            .min_by_key(|&i| (LEVELS[i] - i32::from(v)).abs())
            .unwrap()
    };
    let (ri, gi, bi) = (near(r), near(g), near(b));
    let cube = 16 + 36 * ri + 6 * gi + bi;
    let dist = |(x, y, z): (i32, i32, i32)| {
        (x - i32::from(r)).pow(2) + (y - i32::from(g)).pow(2) + (z - i32::from(b)).pow(2)
    };
    let cube_d = dist((LEVELS[ri], LEVELS[gi], LEVELS[bi]));
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let gi = ((avg - 8).max(0) / 10).min(23);
    let gray = 8 + 10 * gi;
    if dist((gray, gray, gray)) < cube_d {
        (232 + gi) as u8
    } else {
        cube as u8
    }
}

fn plural(n: usize, one: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {one}s")
    }
}

/// `3 scopes · 5 services`
pub fn summary(app: &App) -> String {
    let scopes: std::collections::HashSet<&str> =
        app.rows.iter().map(|r| r.scope.as_str()).collect();
    format!(
        "{} · {}",
        plural(scopes.len(), "scope"),
        plural(app.rows.len(), "service")
    )
}

/// The header: the logo in block characters with name, version and
/// summary; one line on narrow terminals.
fn header(app: &App, colors: Colors, width: u16) -> Vec<Line<'static>> {
    let (o, c) = (colors.orange(), colors.cream());
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if width < NARROW {
        return vec![Line::from(vec![
            Span::raw(" "),
            Span::styled("[", o),
            Span::styled("⋮", c),
            Span::styled("]", o),
            Span::styled(" taba", bold),
            Span::raw(format!(" v{VERSION}")),
        ])];
    }
    let logo = [("█▀▀", "▀▀█"), ("█  ", "  █"), ("█▄▄", "▄▄█")];
    let text = [
        vec![
            Span::styled("taba", bold),
            Span::raw(format!("  v{VERSION}")),
        ],
        vec![Span::raw(summary(app))],
        vec![],
    ];
    logo.into_iter()
        .zip(text)
        .map(|((left, right), text)| {
            let mut spans = vec![
                Span::raw(" "),
                Span::styled(left, o),
                Span::raw(" "),
                Span::styled("▀", c),
                Span::raw(" "),
                Span::styled(right, o),
                Span::raw("   "),
            ];
            spans.extend(text);
            Line::from(spans)
        })
        .collect()
}

/// `45s`, `12m`, `2h`, `3d`; `-` when unknown.
pub fn age(now: u64, start_micros: u64) -> String {
    let start = start_micros / 1_000_000;
    if start == 0 || now < start {
        return "-".into();
    }
    let s = now - start;
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn scope_counts(rows: &[&Row]) -> String {
    let mut parts = Vec::new();
    for status in ["running", "starting", "stopping", "exited"] {
        let n = rows.iter().filter(|r| r.status == status).count();
        if n > 0 {
            parts.push(format!("{n} {status}"));
        }
    }
    parts.join(" · ")
}

/// Splits `text` into lines of at most `width` characters, at spaces.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                out.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        out.push(line);
    }
    out
}

/// Most lines of a refusal reason shown under its row.
const REASON_LINES: usize = 3;

/// The tree as lines, and the index of the cursor's line.
fn tree_lines(app: &App, colors: Colors, width: u16) -> (Vec<Line<'static>>, Option<usize>) {
    let tree = app.tree();
    let shown: Vec<&Row> = tree
        .iter()
        .filter(|(s, _)| app.is_expanded(s))
        .flat_map(|(_, rows)| rows.iter().copied())
        .collect();
    let name_w = shown
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0)
        .max(6);
    let url_w = shown
        .iter()
        .map(|r| r.url.as_deref().map_or(1, |u| u.chars().count()))
        .max()
        .unwrap_or(1);
    let pid_w = shown
        .iter()
        .map(|r| r.pid.to_string().len())
        .max()
        .unwrap_or(1);

    let mut lines = Vec::new();
    let mut cursor_line = None;
    let selected = Style::default().add_modifier(Modifier::REVERSED);
    for (scope, rows) in &tree {
        let is_cursor = app.cursor == Some(Item::Scope(scope.to_string()));
        if is_cursor {
            cursor_line = Some(lines.len());
        }
        let mut left = vec![
            Span::raw(if is_cursor { ">" } else { " " }),
            Span::raw(if app.is_expanded(scope) {
                "▾ "
            } else {
                "▸ "
            }),
            Span::styled(
                scope.to_string(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ];
        if app.current.as_deref() == Some(*scope) {
            left.push(Span::styled("  (current)", colors.dim()));
        }
        let right = scope_counts(rows);
        let left_w: usize = left.iter().map(Span::width).sum();
        let pad = (width as usize)
            .saturating_sub(left_w + right.chars().count() + 1)
            .max(2);
        left.push(Span::raw(" ".repeat(pad)));
        left.push(Span::raw(right));
        let mut line = Line::from(left);
        if is_cursor {
            line = line.style(selected);
        }
        lines.push(line);
        if !app.is_expanded(scope) {
            continue;
        }
        for r in rows {
            let is_cursor = app.cursor == Some(Item::Service(r.scope.clone(), r.name.clone()));
            if is_cursor {
                cursor_line = Some(lines.len());
            }
            let mut spans = vec![
                Span::raw(if is_cursor { "   > " } else { "     " }),
                Span::styled("●", colors.status(&r.status)),
                Span::raw(format!(" {:<name_w$}  ", r.name)),
                Span::styled(format!("{:<8}", r.status), colors.status(&r.status)),
                Span::raw(format!(
                    "  {:<url_w$}  pid {:<pid_w$}  {:>3}",
                    r.url.as_deref().unwrap_or("-"),
                    r.pid,
                    age(app.now, r.start_time),
                )),
            ];
            if let Some(by) = &r.locked_by {
                spans.push(Span::styled(format!("   locked by {by}"), colors.dim()));
            } else if !r.borrowers.is_empty() {
                spans.push(Span::styled(
                    format!("   borrowed by {}", r.borrowers.join(", ")),
                    colors.dim(),
                ));
            }
            let mut line = Line::from(spans);
            if is_cursor {
                line = line.style(selected);
            }
            lines.push(line);
            if let Some(reason) = app.reasons.get(&r.id()) {
                let indent = "       ";
                let mut wrapped = wrap(reason, (width as usize).saturating_sub(indent.len() + 3));
                if wrapped.len() > REASON_LINES {
                    wrapped.truncate(REASON_LINES);
                    if let Some(last) = wrapped.last_mut() {
                        last.push('…');
                    }
                }
                for (i, w) in wrapped.into_iter().enumerate() {
                    let mark = if i == 0 { "✕ " } else { "  " };
                    lines.push(Line::from(Span::styled(
                        format!("{indent}{mark}{w}"),
                        colors.error(),
                    )));
                }
            }
        }
    }
    (lines, cursor_line)
}

/// Lines `offset..` that keep `cursor` within `height` lines, updating the
/// stored offset.
fn scrolled(
    app: &App,
    lines: Vec<Line<'static>>,
    cursor: Option<usize>,
    height: usize,
) -> Vec<Line<'static>> {
    let mut offset = app.scroll.get().min(lines.len().saturating_sub(1));
    if let Some(c) = cursor {
        if c < offset {
            offset = c;
        } else if height > 0 && c >= offset + height {
            offset = c + 1 - height;
        }
    }
    if lines.len() <= height {
        offset = 0;
    }
    app.scroll.set(offset);
    lines.into_iter().skip(offset).take(height).collect()
}

fn body(app: &App, colors: Colors, area: Rect) -> Vec<Line<'static>> {
    let height = area.height as usize;
    if app.mode == Mode::Logs {
        let Some(log) = &app.log else {
            return Vec::new();
        };
        let state = if log.scroll == 0 {
            "following".to_string()
        } else {
            format!("{} lines up · End follows", log.scroll)
        };
        let mut out = vec![Line::from(vec![
            Span::styled(" log ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(log.service.clone()),
            Span::styled(format!("  ({state})"), colors.dim()),
        ])];
        let all = log.all_lines();
        let room = height.saturating_sub(1);
        let end = all.len().saturating_sub(log.scroll);
        let start = end.saturating_sub(room);
        out.extend(all[start..end].iter().map(|l| Line::raw(format!(" {l}"))));
        return out;
    }
    if app.rows.is_empty() {
        return vec![Line::styled(
            " No services yet. Start one with: taba run --name web -- <command>",
            colors.dim(),
        )];
    }
    let (lines, cursor) = tree_lines(app, colors, area.width);
    if lines.is_empty() {
        return vec![Line::styled(
            format!(" Nothing matches /{}", app.filter),
            colors.dim(),
        )];
    }
    scrolled(app, lines, cursor, height)
}

fn notice(app: &App, colors: Colors) -> Option<Line<'static>> {
    if let Some(e) = &app.error {
        return Some(Line::styled(
            format!(" cannot read state: {e}"),
            colors.error(),
        ));
    }
    if let Some(a) = &app.busy {
        return Some(Line::styled(
            format!(" {}", a.progress()),
            colors.basic(Color::Yellow),
        ));
    }
    if let Some(n) = &app.notice {
        let style = if n.error {
            colors.error()
        } else {
            Style::default()
        };
        return Some(Line::styled(format!(" {}", n.text), style));
    }
    if !app.filter.is_empty() && app.mode != Mode::Filter {
        return Some(Line::styled(
            format!(" filter /{}   Esc clears", app.filter),
            colors.dim(),
        ));
    }
    None
}

fn footer(app: &App) -> Line<'static> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    match &app.mode {
        Mode::Confirm(action) => Line::from(vec![
            Span::styled(format!(" {}", action.question()), bold),
            Span::raw("   y yes · any other key cancels"),
        ]),
        Mode::Filter => Line::from(vec![
            Span::styled(format!(" /{}▏", app.filter), bold),
            Span::raw("   Enter keep · Esc clear"),
        ]),
        Mode::Logs => Line::raw(" ↑↓ PgUp PgDn scroll   End follow   Esc back   q quit"),
        Mode::Normal => Line::raw(format!(
            " ↑↓ move   ←→ / space collapse·expand   x {}   l logs   / filter   q quit",
            app.x_label()
        )),
    }
}

pub fn render(f: &mut Frame, app: &App, colors: Colors) {
    let area = f.area();
    let head = header(app, colors, area.width);
    let note = notice(app, colors);
    let rule = Line::styled("─".repeat(area.width as usize), colors.dim());
    let [h, s1, b, s2, n, foot] = Layout::vertical([
        Constraint::Length(head.len() as u16),
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(1),
        Constraint::Length(u16::from(note.is_some())),
        Constraint::Length(1),
    ])
    .areas(area);
    f.render_widget(Paragraph::new(head), h);
    f.render_widget(Paragraph::new(rule.clone()), s1);
    f.render_widget(Paragraph::new(body(app, colors, b)), b);
    f.render_widget(Paragraph::new(rule), s2);
    if let Some(note) = note {
        f.render_widget(Paragraph::new(note), n);
    }
    f.render_widget(Paragraph::new(footer(app)), foot);
}

#[cfg(test)]
mod tests {
    use super::super::app::tests::{key, row, sample};
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    /// Renders `app` at `w`x`h` and returns the screen as text lines with
    /// trailing spaces trimmed.
    fn screen(app: &App, colors: Colors, w: u16, h: u16) -> (Vec<String>, ratatui::buffer::Buffer) {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| render(f, app, colors)).unwrap();
        let buf = t.backend().buffer().clone();
        let lines = (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        (lines, buf)
    }

    /// The sample app with URLs, pids, ages and a borrow, at a fixed time.
    fn demo() -> App {
        let mut app = sample();
        let now = 1_800_000_000;
        let mut rows = app.rows.clone();
        for (i, r) in rows.iter_mut().enumerate() {
            r.pid = 9100 + i as i32;
            r.url = Some(format!("http://localhost:{}", 4100 + i));
            r.start_time = (now - 2 * 3600) * 1_000_000;
        }
        rows[2].start_time = (now - 12 * 60) * 1_000_000; // docs/site
        rows[2].borrowers = vec!["login-fix".into()];
        rows[1].url = None; // billing/web, exited
        app.set_rows(rows, now);
        app
    }

    #[test]
    fn renders_the_tree_at_a_fixed_size() {
        let mut app = demo();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // login-fix/web
        let (lines, _) = screen(&app, Colors::Off, 80, 14);
        let v = format!("v{VERSION}");
        let expected = [
            format!(" █▀▀ ▀ ▀▀█   taba  {v}"),
            " █   ▀   █   3 scopes · 5 services".to_string(),
            " █▄▄ ▀ ▄▄█".to_string(),
            "─".repeat(80),
            " ▾ login-fix  (current)                                               2 running"
                .into(),
            "     ● api     running   http://localhost:4103  pid 9103   2h".into(),
            "   > ● web     running   http://localhost:4104  pid 9104   2h".into(),
            " ▸ billing                                                 1 running · 1 exited"
                .into(),
            " ▸ docs                                                               1 running"
                .into(),
            String::new(),
            String::new(),
            String::new(),
            "─".repeat(80),
            " ↑↓ move   ←→ / space collapse·expand   x stop   l logs   / filter   q quit".into(),
        ];
        assert_eq!(lines, expected);
    }

    #[test]
    fn shows_borrows_refusals_and_confirmation() {
        let mut app = demo();
        // Unfold docs and select docs/site.
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Down));
        }
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Down));
        app.reasons.insert(
            "docs/site".into(),
            "docs/site belongs to scope docs (~/docs), and you are in login-fix; taba does not \
             stop another scope's service."
                .into(),
        );
        app.handle_key(key(KeyCode::Char('x')));
        let (lines, _) = screen(&app, Colors::Off, 80, 16);
        let site: String =
            "   > ● site    running   http://localhost:4102  pid 9102  12m   borrowed by login-fix"
                .chars()
                .take(80)
                .collect();
        assert_eq!(lines[9], site.trim_end());
        assert_eq!(
            lines[10],
            "       ✕ docs/site belongs to scope docs (~/docs), and you are in login-fix;"
        );
        assert_eq!(
            lines[11],
            "         taba does not stop another scope's service."
        );
        assert_eq!(
            lines[15],
            " Stop docs/site?   y yes · any other key cancels"
        );
    }

    #[test]
    fn narrow_terminals_get_a_one_line_header() {
        let app = demo();
        let (lines, _) = screen(&app, Colors::Off, 39, 8);
        assert_eq!(lines[0], format!(" [⋮] taba v{VERSION}"));
        assert_eq!(lines[1], "─".repeat(39));
        let (lines, _) = screen(&app, Colors::Off, 40, 8);
        assert!(lines[0].starts_with(" █▀▀ ▀ ▀▀█"));
    }

    #[test]
    fn logo_colors_follow_the_terminal() {
        let app = demo();
        let fg = |c: Colors, x: u16| screen(&app, c, 80, 10).1[(x, 0)].fg;
        assert_eq!(fg(Colors::TrueColor, 1), Color::Rgb(0xFF, 0x55, 0x33));
        assert_eq!(fg(Colors::TrueColor, 5), Color::Rgb(0xFF, 0xE0, 0xBF));
        assert_eq!(fg(Colors::Ansi256, 1), Color::Indexed(203));
        assert_eq!(fg(Colors::Ansi256, 5), Color::Indexed(223));
        assert_eq!(fg(Colors::Off, 1), Color::Reset);
        assert_eq!(fg(Colors::Off, 5), Color::Reset);
    }

    #[test]
    fn color_detection() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(Colors::from_env(env(&[])), Colors::Ansi256);
        assert_eq!(
            Colors::from_env(env(&[("TERM_PROGRAM", "Apple_Terminal")])),
            Colors::Ansi256
        );
        assert_eq!(
            Colors::from_env(env(&[("COLORTERM", "truecolor")])),
            Colors::TrueColor
        );
        assert_eq!(
            Colors::from_env(env(&[("WT_SESSION", "x")])),
            Colors::TrueColor
        );
        assert_eq!(
            Colors::from_env(env(&[("TERM_PROGRAM", "iTerm.app")])),
            Colors::TrueColor
        );
        assert_eq!(
            Colors::from_env(env(&[("NO_COLOR", "1"), ("COLORTERM", "truecolor")])),
            Colors::Off
        );
        assert_eq!(Colors::from_env(env(&[("NO_COLOR", "")])), Colors::Ansi256);
    }

    #[test]
    fn nearest_256_colors() {
        assert_eq!(ansi256(0xFF, 0x55, 0x33), 203);
        assert_eq!(ansi256(0xFF, 0xE0, 0xBF), 223);
        assert_eq!(ansi256(0, 0, 0), 16);
        assert_eq!(ansi256(255, 255, 255), 231);
        assert_eq!(ansi256(0x80, 0x80, 0x80), 244);
    }

    #[test]
    fn logs_view_follows_the_end() {
        let mut app = demo();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char('l')));
        let text: String = (1..=20).map(|i| format!("line {i}\n")).collect();
        app.log.as_mut().unwrap().push(text.as_bytes());
        let (lines, _) = screen(&app, Colors::Off, 60, 12);
        assert_eq!(lines[4], " log login-fix/api  (following)");
        assert_eq!(lines[5], " line 16");
        assert_eq!(lines[9], " line 20");
        assert_eq!(
            lines[11],
            " ↑↓ PgUp PgDn scroll   End follow   Esc back   q quit"
        );
        app.handle_key(key(KeyCode::Up));
        let (lines, _) = screen(&app, Colors::Off, 60, 12);
        assert_eq!(lines[9], " line 19");
    }

    #[test]
    fn long_lists_scroll_to_the_cursor() {
        let mut app = App::new(Some("s".into()));
        app.set_rows(
            (0..30)
                .map(|i| row("s", &format!("svc{i:02}"), "running"))
                .collect(),
            0,
        );
        for _ in 0..25 {
            app.handle_key(key(KeyCode::Down));
        }
        let (lines, _) = screen(&app, Colors::Off, 70, 12);
        // 12 rows: header 3, rule, 6 body lines, rule, footer.
        assert!(lines[9].starts_with("   > ● svc24"), "{lines:#?}");
        assert!(lines[4].starts_with("     ● svc19"), "{lines:#?}");
    }

    #[test]
    fn empty_and_filtered_out() {
        let app = App::new(None);
        let (lines, _) = screen(&app, Colors::Off, 80, 9);
        assert!(lines[4].contains("No services yet"));
        let mut app = demo();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('z')));
        let (lines, _) = screen(&app, Colors::Off, 80, 9);
        assert_eq!(lines[4], " Nothing matches /z");
        assert_eq!(lines[8], " /z▏   Enter keep · Esc clear");
    }

    /// The README screenshot: the sketch from issue #10, drawn by this view
    /// code in 24-bit color, as an SVG in the style of
    /// `scripts/term2svg.py`. Refresh with
    /// `cargo test --bin taba readme_screenshot -- --ignored`.
    #[test]
    #[ignore = "writes docs/images/ui.svg"]
    fn readme_screenshot() {
        let now = 1_800_000_000;
        let ago = |s: u64| (now - s) * 1_000_000;
        let svc =
            |scope: &str, name: &str, status: &str, port: Option<u16>, pid: i32, age: u64| Row {
                url: port.map(|p| format!("http://localhost:{p}")),
                pid,
                start_time: ago(age),
                ..row(scope, name, status)
            };
        let mut app = App::new(Some("login-fix".into()));
        let mut site = svc("docs", "site", "running", Some(4200), 9301, 12 * 60);
        site.borrowers = vec!["login-fix".into()];
        app.set_rows(
            vec![
                svc("billing", "api", "running", Some(4102), 9188, 3 * 3600),
                svc("billing", "worker", "exited", None, 9190, 3 * 3600),
                site,
                svc("login-fix", "api", "running", Some(4101), 9140, 2 * 3600),
                svc("login-fix", "web", "running", Some(4100), 9123, 2 * 3600),
            ],
            now,
        );
        // Unfold docs, then put the cursor on login-fix/api.
        app.cursor = Some(Item::Scope("docs".into()));
        app.handle_key(key(KeyCode::Right));
        app.cursor = Some(Item::Service("login-fix".into(), "api".into()));
        let (w, h) = (88u16, 13u16);
        let (_, buf) = screen(&app, Colors::TrueColor, w, h);
        let svg = to_svg(&buf, w, h, "taba — ui");
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/images/ui.svg");
        std::fs::write(path, svg).unwrap();
    }

    fn hex(c: Color, default: &str) -> String {
        match c {
            Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
            Color::Green => "#3fb950".into(),
            Color::Yellow => "#d29922".into(),
            Color::Red => "#ff7b72".into(),
            Color::DarkGray => "#8b949e".into(),
            _ => default.into(),
        }
    }

    fn to_svg(buf: &ratatui::buffer::Buffer, w: u16, h: u16, title: &str) -> String {
        const FONT: &str =
            "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace";
        const BG: &str = "#0d1117";
        const FG: &str = "#c9d1d9";
        let (char_w, line_h, pad_x, pad_top) = (8.75_f64, 20.0_f64, 22.0_f64, 48.0_f64);
        let width = (pad_x * 2.0 + f64::from(w) * char_w) as u32;
        let height = (pad_top + line_h * f64::from(h) + 18.0) as u32;
        let esc = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
        };
        let mut out = vec![
            format!(
                r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}" role="img" aria-label="{t}">"#,
                t = esc(title)
            ),
            format!(
                r##"<rect x="0.5" y="0.5" width="{}" height="{}" rx="8" fill="{BG}" stroke="#30363d"/>"##,
                width - 1,
                height - 1
            ),
            format!(
                r##"<path d="M0.5 30 V8.5 A8 8 0 0 1 8.5 0.5 H{} A8 8 0 0 1 {} 8.5 V30 Z" fill="#161b22"/>"##,
                f64::from(width) - 8.5,
                f64::from(width) - 0.5
            ),
            format!(
                r##"<line x1="0.5" y1="30.5" x2="{}" y2="30.5" stroke="#30363d"/>"##,
                f64::from(width) - 0.5
            ),
            r##"<circle cx="18" cy="15.5" r="5.5" fill="#ff5f57"/>"##.into(),
            r##"<circle cx="36" cy="15.5" r="5.5" fill="#febc2e"/>"##.into(),
            r##"<circle cx="54" cy="15.5" r="5.5" fill="#28c840"/>"##.into(),
            format!(
                r##"<text x="{}" y="20" text-anchor="middle" fill="#8b949e" font-family="{FONT}" font-size="12">{}</text>"##,
                f64::from(width) / 2.0,
                esc(title)
            ),
        ];
        let mut text = Vec::new();
        for y in 0..h {
            let top = pad_top + line_h * f64::from(y);
            let base = top + 14.0;
            let mut x = 0;
            while x < w {
                let cell = &buf[(x, y)];
                let style = (cell.fg, cell.modifier);
                let start = x;
                let mut run = String::new();
                // One x per character keeps the text on the cell grid
                // whatever the font's advance width.
                let mut xs = Vec::new();
                while x < w && (buf[(x, y)].fg, buf[(x, y)].modifier) == style {
                    for c in buf[(x, y)].symbol().chars() {
                        run.push(c);
                        xs.push(format!("{}", pad_x + f64::from(x) * char_w));
                    }
                    x += 1;
                }
                let px = pad_x + f64::from(start) * char_w;
                let reversed = style.1.contains(Modifier::REVERSED);
                let mut fill = hex(style.0, FG);
                if reversed {
                    out.push(format!(
                        r#"<rect x="{px}" y="{}" width="{}" height="{line_h}" fill="{fill}"/>"#,
                        top - 1.0,
                        f64::from(x - start) * char_w
                    ));
                    fill = BG.into();
                }
                if run.trim().is_empty() {
                    continue;
                }
                let weight = if style.1.contains(Modifier::BOLD) {
                    r#" font-weight="bold""#
                } else {
                    ""
                };
                text.push(format!(
                    r#"<tspan x="{}" y="{base}" fill="{fill}"{weight}>{}</tspan>"#,
                    xs.join(" "),
                    esc(&run)
                ));
            }
        }
        out.push(format!(
            r#"<text font-family="{FONT}" font-size="14" xml:space="preserve">"#
        ));
        out.extend(text);
        out.push("</text>".into());
        out.push("</svg>".into());
        out.push(String::new());
        out.join("\n")
    }
}
