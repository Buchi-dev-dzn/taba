//! The TUI's state and how keys change it. Pure: no terminal, no I/O.
//! Terminal handling is in `mod.rs`, drawing in `view.rs`.

use crate::error::{Output, TabaError};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

/// One service as the TUI shows it, taken from the `ps` snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub scope: String,
    pub name: String,
    /// `running`, `starting`, `stopping` or `exited`.
    pub status: String,
    pub url: Option<String>,
    pub pid: i32,
    /// Microseconds since the Unix epoch.
    pub start_time: u64,
    pub log_path: PathBuf,
    /// Scopes holding a live borrow of this service.
    pub borrowers: Vec<String>,
    /// Scope holding an exclusive borrow, if any.
    pub locked_by: Option<String>,
}

impl Row {
    pub fn id(&self) -> String {
        format!("{}/{}", self.scope, self.name)
    }

    pub fn is_exited(&self) -> bool {
        self.status == "exited"
    }

    /// Builds the rows from the shared `ps` snapshot.
    pub fn from_snapshot(snap: crate::Snapshot) -> Vec<Row> {
        snap.services
            .into_iter()
            .zip(snap.statuses)
            .zip(snap.rows)
            .zip(snap.borrowers)
            .map(|(((svc, status), json), borrowers)| Row {
                url: svc.url(),
                status: status.to_string(),
                pid: svc.pid,
                start_time: svc.start_time,
                log_path: svc.log_path,
                borrowers,
                locked_by: json["locked_by"].as_str().map(str::to_string),
                scope: svc.scope,
                name: svc.name,
            })
            .collect()
    }
}

/// A line of the tree: a scope, or one of its services.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Item {
    Scope(String),
    Service(String, String),
}

/// What `x` does once confirmed. Each runs the CLI's own code path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// `taba stop SCOPE/NAME`
    Stop { scope: String, name: String },
    /// `taba rm SCOPE/NAME` (an exited service)
    Remove { scope: String, name: String },
    /// `taba stop` of every service of `scope`, each as `SCOPE/NAME`
    StopScope {
        scope: String,
        targets: Vec<(String, String)>,
    },
}

impl Action {
    /// The question asked before running it.
    pub fn question(&self) -> String {
        match self {
            Action::Stop { scope, name } => format!("Stop {scope}/{name}?"),
            Action::Remove { scope, name } => {
                format!("Remove {scope}/{name} (its record and log)?")
            }
            Action::StopScope { scope, targets } => {
                format!("Stop all {} service(s) of {scope}?", targets.len())
            }
        }
    }

    /// Shown while it runs.
    pub fn progress(&self) -> String {
        match self {
            Action::Stop { scope, name } => format!("stopping {scope}/{name}…"),
            Action::Remove { scope, name } => format!("removing {scope}/{name}…"),
            Action::StopScope { scope, .. } => format!("stopping every service of {scope}…"),
        }
    }

    fn ids(&self) -> Vec<String> {
        match self {
            Action::Stop { scope, name } | Action::Remove { scope, name } => {
                vec![format!("{scope}/{name}")]
            }
            Action::StopScope { targets, .. } => {
                targets.iter().map(|(s, n)| format!("{s}/{n}")).collect()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    Normal,
    /// Typing a `/` filter.
    Filter,
    /// Waiting for `y` to run the action; any other key cancels.
    Confirm(Action),
    /// Following a service's log.
    Logs,
}

/// What the event loop must do after a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    Run(Action),
    OpenLog(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub text: String,
    pub error: bool,
}

/// The log being followed.
#[derive(Debug, Default)]
pub struct LogView {
    pub service: String,
    pub lines: Vec<String>,
    /// An unterminated last line, completed by the next read.
    partial: String,
    /// Lines scrolled up from the end; 0 follows the end.
    pub scroll: usize,
}

/// Lines kept per followed log.
const LOG_LINES: usize = 5000;

impl LogView {
    /// Appends raw log bytes, keeping the last [`LOG_LINES`] lines.
    pub fn push(&mut self, bytes: &[u8]) {
        let text = String::from_utf8_lossy(bytes);
        let mut rest = std::mem::take(&mut self.partial);
        rest.push_str(&text);
        let mut parts: Vec<&str> = rest.split('\n').collect();
        let last = parts.pop().unwrap_or_default().to_string();
        for p in parts {
            self.lines.push(sanitize(p));
            if self.scroll > 0 {
                self.scroll += 1; // keep the view still while scrolled up
            }
        }
        self.partial = last;
        if self.lines.len() > LOG_LINES {
            let drop = self.lines.len() - LOG_LINES;
            self.lines.drain(..drop);
        }
    }

    /// The lines to show, with the unterminated last one.
    pub fn all_lines(&self) -> Vec<String> {
        let mut v = self.lines.clone();
        if !self.partial.is_empty() {
            v.push(sanitize(&self.partial));
        }
        v
    }
}

/// Drops ANSI escape sequences and control characters, which would
/// otherwise reach the terminal; tabs become spaces.
pub fn sanitize(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                // CSI: ESC [ params final-byte; OSC: ESC ] ... BEL or ESC \.
                match chars.peek() {
                    Some('[') => {
                        chars.next();
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']') => {
                        chars.next();
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' {
                                break;
                            }
                            if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                                chars.next();
                                break;
                            }
                        }
                    }
                    _ => {
                        chars.next();
                    }
                }
            }
            '\t' => out.push_str("    "),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

pub struct App {
    /// The caller's scope (the one `x` may change), shown first.
    pub current: Option<String>,
    pub rows: Vec<Row>,
    expanded: HashSet<String>,
    seen: HashSet<String>,
    pub cursor: Option<Item>,
    /// Index of the cursor in the last item list, to land nearby when its
    /// item disappears.
    last_index: usize,
    pub filter: String,
    pub mode: Mode,
    pub notice: Option<Notice>,
    /// Why the last action on a service was refused (the CLI's message).
    pub reasons: HashMap<String, String>,
    /// The action running in the background, if any.
    pub busy: Option<Action>,
    /// A failure to read state, shown until the next good read.
    pub error: Option<String>,
    pub log: Option<LogView>,
    /// Seconds since the Unix epoch at the last refresh (for ages).
    pub now: u64,
    /// First list line shown; kept by the view so the cursor stays visible.
    pub scroll: Cell<usize>,
}

impl App {
    pub fn new(current: Option<String>) -> App {
        App {
            current,
            rows: Vec::new(),
            expanded: HashSet::new(),
            seen: HashSet::new(),
            cursor: None,
            last_index: 0,
            filter: String::new(),
            mode: Mode::Normal,
            notice: None,
            reasons: HashMap::new(),
            busy: None,
            error: None,
            log: None,
            now: 0,
            scroll: Cell::new(0),
        }
    }

    /// Replaces the rows after a refresh. A scope seen for the first time
    /// starts folded, except the current one, which starts expanded.
    pub fn set_rows(&mut self, rows: Vec<Row>, now: u64) {
        self.now = now;
        self.error = None;
        for r in &rows {
            if self.seen.insert(r.scope.clone()) && Some(&r.scope) == self.current.as_ref() {
                self.expanded.insert(r.scope.clone());
            }
        }
        // Reasons belong to a service that still exists.
        self.reasons
            .retain(|id, _| rows.iter().any(|r| &r.id() == id));
        self.rows = rows;
        self.fix_cursor();
    }

    pub fn set_error(&mut self, e: &TabaError) {
        self.error = Some(e.message.clone());
    }

    pub fn is_expanded(&self, scope: &str) -> bool {
        self.expanded.contains(scope)
    }

    /// Scopes in display order (current first, then by name) with their
    /// rows, before filtering.
    pub fn scopes(&self) -> Vec<(&str, Vec<&Row>)> {
        let mut by: BTreeMap<&str, Vec<&Row>> = BTreeMap::new();
        for r in &self.rows {
            by.entry(r.scope.as_str()).or_default().push(r);
        }
        let mut v: Vec<(&str, Vec<&Row>)> = by.into_iter().collect();
        if let Some(cur) = &self.current
            && let Some(i) = v.iter().position(|(s, _)| s == cur)
        {
            let c = v.remove(i);
            v.insert(0, c);
        }
        v
    }

    fn filter_lc(&self) -> Option<String> {
        let f = self.filter.trim().to_lowercase();
        (!f.is_empty()).then_some(f)
    }

    /// The visible tree: scopes and, for expanded scopes, their services,
    /// after the filter. A scope whose name matches keeps all its services;
    /// otherwise only matching services are kept.
    pub fn tree(&self) -> Vec<(&str, Vec<&Row>)> {
        let f = self.filter_lc();
        self.scopes()
            .into_iter()
            .filter_map(|(scope, rows)| match &f {
                None => Some((scope, rows)),
                Some(f) if scope.to_lowercase().contains(f) => Some((scope, rows)),
                Some(f) => {
                    let rows: Vec<&Row> = rows
                        .into_iter()
                        .filter(|r| r.name.to_lowercase().contains(f))
                        .collect();
                    (!rows.is_empty()).then_some((scope, rows))
                }
            })
            .collect()
    }

    /// The items the cursor moves over, in order.
    pub fn items(&self) -> Vec<Item> {
        let mut v = Vec::new();
        for (scope, rows) in self.tree() {
            v.push(Item::Scope(scope.to_string()));
            if self.is_expanded(scope) {
                for r in rows {
                    v.push(Item::Service(r.scope.clone(), r.name.clone()));
                }
            }
        }
        v
    }

    pub fn row(&self, scope: &str, name: &str) -> Option<&Row> {
        self.rows
            .iter()
            .find(|r| r.scope == scope && r.name == name)
    }

    fn selected_row(&self) -> Option<&Row> {
        match &self.cursor {
            Some(Item::Service(s, n)) => self.row(s, n),
            _ => None,
        }
    }

    /// Keeps the cursor on a visible item: the same one if it still is,
    /// else its scope, else the item now at its old position.
    fn fix_cursor(&mut self) {
        let items = self.items();
        if items.is_empty() {
            self.cursor = None;
            self.last_index = 0;
            return;
        }
        if let Some(c) = &self.cursor
            && let Some(i) = items.iter().position(|it| it == c)
        {
            self.last_index = i;
            return;
        }
        if let Some(Item::Service(s, _)) = &self.cursor
            && let Some(i) = items.iter().position(|it| *it == Item::Scope(s.clone()))
        {
            self.cursor = Some(items[i].clone());
            self.last_index = i;
            return;
        }
        let i = self.last_index.min(items.len() - 1);
        self.cursor = Some(items[i].clone());
        self.last_index = i;
    }

    fn move_by(&mut self, delta: isize) {
        let items = self.items();
        if items.is_empty() {
            return;
        }
        let i = self
            .cursor
            .as_ref()
            .and_then(|c| items.iter().position(|it| it == c))
            .unwrap_or(0);
        let j = (i as isize + delta).clamp(0, items.len() as isize - 1) as usize;
        self.cursor = Some(items[j].clone());
        self.last_index = j;
    }

    fn fold(&mut self, scope: &str) {
        self.expanded.remove(scope);
        self.cursor = Some(Item::Scope(scope.to_string()));
        self.fix_cursor();
    }

    fn unfold(&mut self, scope: &str) {
        self.expanded.insert(scope.to_string());
        self.fix_cursor();
    }

    /// Unfolds scopes with a service matching the filter, so typing a name
    /// shows it. Folding is only a view; nothing is stopped.
    fn on_filter_change(&mut self) {
        if let Some(f) = self.filter_lc() {
            let hits: Vec<String> = self
                .rows
                .iter()
                .filter(|r| r.name.to_lowercase().contains(&f))
                .map(|r| r.scope.clone())
                .collect();
            self.expanded.extend(hits);
        }
        self.fix_cursor();
    }

    /// The `x` action for the selected item, or a notice why there is none.
    fn plan_x(&self) -> Result<Action, String> {
        match &self.cursor {
            None => Err("nothing selected".into()),
            Some(Item::Scope(scope)) => {
                let targets: Vec<(String, String)> = self
                    .rows
                    .iter()
                    .filter(|r| &r.scope == scope)
                    .map(|r| (r.scope.clone(), r.name.clone()))
                    .collect();
                if self.rows.iter().all(|r| &r.scope != scope || r.is_exited()) {
                    return Err(format!("nothing is running in {scope}"));
                }
                Ok(Action::StopScope {
                    scope: scope.clone(),
                    targets,
                })
            }
            Some(Item::Service(scope, name)) => {
                let (scope, name) = (scope.clone(), name.clone());
                match self.row(&scope, &name) {
                    Some(r) if r.is_exited() => Ok(Action::Remove { scope, name }),
                    Some(_) => Ok(Action::Stop { scope, name }),
                    None => Err(format!("{scope}/{name} is gone")),
                }
            }
        }
    }

    /// What `x` would do for the selection: `stop`, `remove` or `stop all`.
    pub fn x_label(&self) -> &'static str {
        match self.plan_x() {
            Ok(Action::Remove { .. }) => "remove",
            Ok(Action::StopScope { .. }) => "stop all",
            _ => "stop",
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Effect {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
            return Effect::Quit;
        }
        match self.mode.clone() {
            Mode::Confirm(action) => {
                self.mode = Mode::Normal;
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.notice = None;
                    for id in action.ids() {
                        self.reasons.remove(&id);
                    }
                    self.busy = Some(action.clone());
                    Effect::Run(action)
                } else {
                    self.notice = Some(Notice {
                        text: "cancelled".into(),
                        error: false,
                    });
                    Effect::None
                }
            }
            Mode::Filter => {
                match key.code {
                    KeyCode::Enter => self.mode = Mode::Normal,
                    KeyCode::Esc => {
                        self.filter.clear();
                        self.mode = Mode::Normal;
                    }
                    KeyCode::Backspace => {
                        self.filter.pop();
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    KeyCode::Char(c) if !ctrl => self.filter.push(c),
                    _ => {}
                }
                self.on_filter_change();
                Effect::None
            }
            Mode::Logs => {
                match key.code {
                    KeyCode::Esc => {
                        self.mode = Mode::Normal;
                        self.log = None;
                        return Effect::None;
                    }
                    KeyCode::Char('q') => return Effect::Quit,
                    _ => {}
                }
                let log = self.log.get_or_insert_with(LogView::default);
                let page = 10;
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => log.scroll += 1,
                    KeyCode::Down | KeyCode::Char('j') => log.scroll = log.scroll.saturating_sub(1),
                    KeyCode::PageUp => log.scroll += page,
                    KeyCode::PageDown => log.scroll = log.scroll.saturating_sub(page),
                    KeyCode::Home => log.scroll = usize::MAX / 2,
                    KeyCode::End => log.scroll = 0,
                    _ => {}
                }
                let max = log.all_lines().len();
                log.scroll = log.scroll.min(max);
                Effect::None
            }
            Mode::Normal => self.normal_key(key),
        }
    }

    fn normal_key(&mut self, key: KeyEvent) -> Effect {
        match key.code {
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Home => self.move_by(isize::MIN / 2),
            KeyCode::End => self.move_by(isize::MAX / 2),
            KeyCode::Left => match self.cursor.clone() {
                Some(Item::Scope(s) | Item::Service(s, _)) => self.fold(&s),
                None => {}
            },
            KeyCode::Right => {
                if let Some(Item::Scope(s)) = self.cursor.clone() {
                    self.unfold(&s);
                }
            }
            KeyCode::Char(' ') => match self.cursor.clone() {
                Some(Item::Scope(s)) if self.is_expanded(&s) => self.fold(&s),
                Some(Item::Scope(s)) => self.unfold(&s),
                Some(Item::Service(s, _)) => self.fold(&s),
                None => {}
            },
            KeyCode::Char('/') => {
                self.mode = Mode::Filter;
            }
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.on_filter_change();
                }
                self.notice = None;
                self.reasons.clear();
            }
            KeyCode::Char('x') => {
                if let Some(a) = &self.busy {
                    self.notice = Some(Notice {
                        text: format!("wait: {}", a.progress()),
                        error: false,
                    });
                    return Effect::None;
                }
                match self.plan_x() {
                    Ok(action) => {
                        self.notice = None;
                        self.mode = Mode::Confirm(action);
                    }
                    Err(why) => {
                        self.notice = Some(Notice {
                            text: why,
                            error: false,
                        })
                    }
                }
            }
            KeyCode::Char('l') => match self.selected_row() {
                Some(r) => {
                    let path = r.log_path.clone();
                    self.log = Some(LogView {
                        service: r.id(),
                        ..LogView::default()
                    });
                    self.mode = Mode::Logs;
                    self.notice = None;
                    return Effect::OpenLog(path);
                }
                None => {
                    self.notice = Some(Notice {
                        text: "select a service to see its log".into(),
                        error: false,
                    })
                }
            },
            _ => {}
        }
        Effect::None
    }

    /// Records the outcome of a finished action: refusals are kept on their
    /// rows with the CLI's message; the summary goes to the notice line.
    pub fn finish(&mut self, action: &Action, res: Result<Output, TabaError>) {
        self.busy = None;
        match res {
            Err(e) => {
                for id in action.ids() {
                    self.reasons.insert(id, e.message.clone());
                }
                self.notice = Some(Notice {
                    text: first_line(&e.message),
                    error: true,
                });
            }
            Ok(out) => {
                if let Some(Value::Array(results)) = out.body.get("results") {
                    for r in results {
                        let id = r["service"].as_str().unwrap_or_default().to_string();
                        match r["error"]["message"].as_str() {
                            Some(msg) => {
                                self.reasons.insert(id, msg.to_string());
                            }
                            None => {
                                self.reasons.remove(&id);
                            }
                        }
                    }
                }
                self.notice = Some(match &out.failure {
                    Some(f) => Notice {
                        text: first_line(&f.message),
                        error: true,
                    },
                    None => Notice {
                        text: out.text.lines().collect::<Vec<_>>().join("; "),
                        error: false,
                    },
                });
            }
        }
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or_default().to_string()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    pub fn row(scope: &str, name: &str, status: &str) -> Row {
        Row {
            scope: scope.into(),
            name: name.into(),
            status: status.into(),
            url: None,
            pid: 100,
            start_time: 0,
            log_path: PathBuf::from(format!("/logs/{scope}/{name}.log")),
            borrowers: Vec::new(),
            locked_by: None,
        }
    }

    pub fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }
    }

    pub fn sample() -> App {
        let mut app = App::new(Some("login-fix".into()));
        app.set_rows(
            vec![
                row("billing", "api", "running"),
                row("billing", "web", "exited"),
                row("docs", "site", "running"),
                row("login-fix", "api", "running"),
                row("login-fix", "web", "running"),
            ],
            0,
        );
        app
    }

    fn sid(s: &str, n: &str) -> Item {
        Item::Service(s.into(), n.into())
    }

    fn scope(s: &str) -> Item {
        Item::Scope(s.into())
    }

    #[test]
    fn current_scope_first_and_expanded_others_folded() {
        let app = sample();
        assert_eq!(
            app.items(),
            [
                scope("login-fix"),
                sid("login-fix", "api"),
                sid("login-fix", "web"),
                scope("billing"),
                scope("docs"),
            ]
        );
        assert_eq!(app.cursor, Some(scope("login-fix")));
    }

    #[test]
    fn no_current_scope_folds_everything() {
        let mut app = App::new(None);
        app.set_rows(vec![row("b", "x", "running"), row("a", "y", "running")], 0);
        assert_eq!(app.items(), [scope("a"), scope("b")]);
    }

    #[test]
    fn up_down_move_and_stop_at_the_ends() {
        let mut app = sample();
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.cursor, Some(scope("login-fix")));
        for _ in 0..3 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.cursor, Some(scope("billing")));
        for _ in 0..10 {
            app.handle_key(key(KeyCode::Down));
        }
        assert_eq!(app.cursor, Some(scope("docs")));
    }

    #[test]
    fn left_right_space_fold_and_unfold() {
        let mut app = sample();
        app.handle_key(key(KeyCode::Down)); // login-fix/api
        app.handle_key(key(KeyCode::Left)); // folds its scope, cursor to scope
        assert_eq!(app.cursor, Some(scope("login-fix")));
        assert_eq!(app.items().len(), 3);
        app.handle_key(key(KeyCode::Right));
        assert_eq!(app.items().len(), 5);
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.items().len(), 3);
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.items().len(), 5);
        // Space on a service folds its scope too.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(app.cursor, Some(scope("login-fix")));
        // Unfold billing.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Right));
        assert_eq!(
            app.items()[1..],
            [
                scope("billing"),
                sid("billing", "api"),
                sid("billing", "web"),
                scope("docs")
            ]
        );
    }

    #[test]
    fn folding_survives_refresh_and_new_scopes_start_folded() {
        let mut app = sample();
        app.handle_key(key(KeyCode::Left)); // fold login-fix
        let mut rows = app.rows.clone();
        rows.push(row("zeta", "w", "running"));
        app.set_rows(rows, 0);
        assert_eq!(
            app.items(),
            [
                scope("login-fix"),
                scope("billing"),
                scope("docs"),
                scope("zeta")
            ]
        );
    }

    #[test]
    fn cursor_follows_its_item_or_falls_back() {
        let mut app = sample();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // login-fix/web
        // A service sorted before it disappears: the cursor stays on web.
        let rows: Vec<Row> = app
            .rows
            .iter()
            .filter(|r| r.id() != "login-fix/api")
            .cloned()
            .collect();
        app.set_rows(rows.clone(), 0);
        assert_eq!(app.cursor, Some(sid("login-fix", "web")));
        // web itself goes: the cursor goes to its scope.
        let rows: Vec<Row> = rows
            .into_iter()
            .filter(|r| r.id() != "login-fix/web")
            .collect();
        app.set_rows(rows.clone(), 0);
        // Its scope is gone too: the item now at its old position.
        assert_eq!(app.cursor, Some(scope("docs")));
        app.set_rows(Vec::new(), 0);
        assert_eq!(app.cursor, None);
        assert_eq!(app.handle_key(key(KeyCode::Char('x'))), Effect::None);
    }

    #[test]
    fn filter_by_scope_or_service_name() {
        let mut app = sample();
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.mode, Mode::Filter);
        for c in "site".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // docs is unfolded to show the match.
        assert_eq!(app.items(), [scope("docs"), sid("docs", "site")]);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(app.filter, "site");
        // `q` quits again once the filter is no longer being typed.
        app.handle_key(key(KeyCode::Char('/')));
        for _ in 0..4 {
            app.handle_key(key(KeyCode::Backspace));
        }
        for c in "BILL".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        // A scope match keeps all its services ("b" on the way matched
        // billing/web and unfolded billing).
        assert_eq!(
            app.items(),
            [
                scope("billing"),
                sid("billing", "api"),
                sid("billing", "web")
            ]
        );
        app.handle_key(key(KeyCode::Esc));
        assert!(app.filter.is_empty());
        assert_eq!(app.items().len(), 8);
        // In filter mode `q` is text, not quit.
        app.handle_key(key(KeyCode::Char('/')));
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Effect::None);
        assert_eq!(app.filter, "q");
        assert!(app.items().is_empty());
    }

    #[test]
    fn x_plans_stop_remove_or_stop_all_after_confirmation() {
        let mut app = sample();
        // Scope line: stop every service of the scope.
        assert_eq!(app.x_label(), "stop all");
        app.handle_key(key(KeyCode::Char('x')));
        let Mode::Confirm(action) = app.mode.clone() else {
            panic!("no confirmation")
        };
        assert_eq!(
            action,
            Action::StopScope {
                scope: "login-fix".into(),
                targets: vec![
                    ("login-fix".into(), "api".into()),
                    ("login-fix".into(), "web".into())
                ]
            }
        );
        // Any key but `y` cancels.
        assert_eq!(app.handle_key(key(KeyCode::Char('n'))), Effect::None);
        assert_eq!(app.mode, Mode::Normal);
        // Running service: stop.
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.x_label(), "stop");
        app.handle_key(key(KeyCode::Char('x')));
        let stop = Action::Stop {
            scope: "login-fix".into(),
            name: "api".into(),
        };
        assert_eq!(app.mode, Mode::Confirm(stop.clone()));
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Effect::Run(stop));
        assert!(app.busy.is_some());
        // While it runs, `x` only explains.
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.notice.as_ref().unwrap().text.starts_with("wait"));
        app.busy = None;
        // Exited service: remove.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // billing
        app.handle_key(key(KeyCode::Right));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Down)); // billing/web (exited)
        assert_eq!(app.x_label(), "remove");
        app.handle_key(key(KeyCode::Char('x')));
        let rm = Action::Remove {
            scope: "billing".into(),
            name: "web".into(),
        };
        assert_eq!(app.handle_key(key(KeyCode::Char('y'))), Effect::Run(rm));
    }

    #[test]
    fn x_on_a_scope_with_nothing_running_does_nothing() {
        let mut app = App::new(None);
        app.set_rows(vec![row("old", "w", "exited")], 0);
        app.handle_key(key(KeyCode::Char('x')));
        assert_eq!(app.mode, Mode::Normal);
        assert_eq!(
            app.notice.as_ref().unwrap().text,
            "nothing is running in old"
        );
    }

    #[test]
    fn refusals_stay_on_their_rows() {
        let mut app = sample();
        let action = Action::Stop {
            scope: "docs".into(),
            name: "site".into(),
        };
        let e = TabaError::new("E_NOT_OWNER", "docs/site belongs to scope docs\nmore");
        app.finish(&action, Err(e));
        assert_eq!(
            app.reasons["docs/site"],
            "docs/site belongs to scope docs\nmore"
        );
        assert_eq!(
            app.notice,
            Some(Notice {
                text: "docs/site belongs to scope docs".into(),
                error: true
            })
        );
        // Scope results: per-service refusals and successes.
        let out = Output::new(
            serde_json::json!({"results": [
                {"service": "docs/site", "result": "stopped"},
                {"service": "billing/api", "result": "refused",
                 "error": {"code": "E_BORROWED", "message": "billing/api is borrowed by docs"}},
            ]}),
            "docs/site: stopped".into(),
        );
        app.finish(
            &Action::StopScope {
                scope: "x".into(),
                targets: vec![],
            },
            Ok(out),
        );
        assert!(!app.reasons.contains_key("docs/site"));
        assert_eq!(
            app.reasons["billing/api"],
            "billing/api is borrowed by docs"
        );
        // A reason goes with its service.
        let rows: Vec<Row> = app
            .rows
            .iter()
            .filter(|r| r.id() != "billing/api")
            .cloned()
            .collect();
        app.set_rows(rows, 0);
        assert!(app.reasons.is_empty());
        // Esc dismisses reasons and the notice.
        app.reasons.insert("docs/site".into(), "no".into());
        app.handle_key(key(KeyCode::Esc));
        assert!(app.reasons.is_empty());
        assert!(app.notice.is_none());
    }

    #[test]
    fn logs_open_scroll_and_close() {
        let mut app = sample();
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('l'))),
            Effect::None
        )); // on a scope line
        app.handle_key(key(KeyCode::Down));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('l'))),
            Effect::OpenLog(PathBuf::from("/logs/login-fix/api.log"))
        );
        assert_eq!(app.mode, Mode::Logs);
        let log = app.log.as_mut().unwrap();
        log.push(b"one\ntwo\nthr");
        log.push(b"ee\n\x1b[31mred\x1b[0m\tx\n");
        assert_eq!(log.all_lines(), ["one", "two", "three", "red    x"]);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(app.log.as_ref().unwrap().scroll, 1);
        app.handle_key(key(KeyCode::End));
        assert_eq!(app.log.as_ref().unwrap().scroll, 0);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.log.is_none());
    }

    #[test]
    fn q_and_ctrl_c_quit() {
        let mut app = sample();
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Effect::Quit);
        let mut ctrl_c = key(KeyCode::Char('c'));
        ctrl_c.modifiers = KeyModifiers::CONTROL;
        app.mode = Mode::Filter;
        assert_eq!(app.handle_key(ctrl_c), Effect::Quit);
        app.mode = Mode::Confirm(Action::Stop {
            scope: "a".into(),
            name: "b".into(),
        });
        assert_eq!(app.handle_key(ctrl_c), Effect::Quit);
    }

    #[test]
    fn sanitize_strips_escapes_and_controls() {
        assert_eq!(sanitize("a\x1b[1;32mb\x1b[0mc"), "abc");
        assert_eq!(sanitize("\x1b]0;title\x07ok"), "ok");
        assert_eq!(sanitize("\x1b]8;;http://x\x1b\\link"), "link");
        assert_eq!(sanitize("cr\rbell\x07"), "crbell");
        assert_eq!(sanitize("日本"), "日本");
    }
}
