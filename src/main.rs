use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use crossterm::{
    event::DisableBracketedPaste,
    event::EnableBracketedPaste,
    event::{self, Event as TerminalEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use flashfind::{data_dir, default_roots, Index, Kind, SearchResult};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const ADDRESS: &str = "127.0.0.1:35185";
const SEARCH_LIMIT: usize = 200;

#[derive(Parser)]
#[command(
    name = "flashfind",
    version,
    about = "Everything-like low-privilege file finder"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the interactive search interface (the default command).
    Tui,
    /// Run the per-user index and filesystem-monitoring service in the foreground.
    Daemon {
        /// Roots to add/build before serving; defaults to the user's home directory on first run.
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
    },
    /// Build or rebuild an index root without starting the service.
    Index { roots: Vec<PathBuf> },
    /// Query the service without opening the TUI.
    Search {
        query: String,
        #[arg(short, long, default_value_t = 30)]
        limit: usize,
    },
    /// Show roots registered for background indexing.
    Roots,
}

#[derive(Serialize, Deserialize)]
struct WireRequest {
    token: String,
    request: Request,
}

#[derive(Serialize, Deserialize)]
enum Request {
    Ping,
    Search { query: String, limit: usize },
    Delete { path: String },
    Rename { from: String, to: String },
    Open { path: String },
}

#[derive(Serialize, Deserialize)]
enum Response {
    Pong,
    Results(Vec<SearchResult>),
    Ok(String),
    Error(String),
}

fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(),
        Command::Daemon { roots } => run_daemon(roots),
        Command::Index { roots } => {
            let mut index = Index::open_default()?;
            for root in roots {
                let stats = index.index_root(&root)?;
                println!(
                    "{}: indexed {}, skipped {}",
                    root.display(),
                    stats.indexed,
                    stats.skipped
                );
            }
            Ok(())
        }
        Command::Search { query, limit } => {
            ensure_daemon()?;
            match send_request(Request::Search { query, limit })? {
                Response::Results(results) => {
                    for result in results {
                        println!("{:<9} {}", kind_label(&result.kind), result.path.display());
                    }
                    Ok(())
                }
                Response::Error(error) => bail!(error),
                _ => bail!("unexpected service response"),
            }
        }
        Command::Roots => {
            let index = Index::open_default()?;
            for root in index.indexed_roots()? {
                println!("{}", root.display());
            }
            Ok(())
        }
    }
}

/// The daemon owns writes and filesystem notifications. TUI clients only read
/// through a separate SQLite connection managed by this local IPC server.
fn run_daemon(extra_roots: Vec<PathBuf>) -> Result<()> {
    let listener = TcpListener::bind(ADDRESS).with_context(|| {
        format!("could not bind {ADDRESS}; another FlashFind daemon may already be running")
    })?;
    let token = ipc_token()?;
    let read_index = Index::open_default()?;
    let mut roots = read_index.indexed_roots()?;
    if roots.is_empty() {
        roots = if extra_roots.is_empty() {
            default_roots()?
        } else {
            extra_roots
        };
    } else {
        for root in extra_roots {
            if !roots.contains(&root) {
                roots.push(root);
            }
        }
    }
    let writer_roots = roots.clone();
    let server_token = token.clone();
    thread::spawn(move || {
        if let Err(error) = index_writer(writer_roots) {
            eprintln!("FlashFind index writer stopped: {error:#}");
        }
    });

    // A fixed worker pool bounds memory/threads when a user pastes rapidly or
    // several terminals query at once. SQLite connections are local to workers
    // and WAL still permits the index writer to run concurrently.
    let (client_sender, client_receiver) = mpsc::sync_channel::<TcpStream>(32);
    let client_receiver = Arc::new(Mutex::new(client_receiver));
    for _ in 0..2 {
        let receiver = Arc::clone(&client_receiver);
        let token = server_token.clone();
        thread::spawn(move || {
            let index = match Index::open_default() {
                Ok(index) => index,
                Err(error) => {
                    eprintln!("IPC worker could not open index: {error:#}");
                    return;
                }
            };
            loop {
                let stream = match receiver.lock().expect("IPC queue lock poisoned").recv() {
                    Ok(stream) => stream,
                    Err(_) => return,
                };
                if let Err(error) = handle_client(stream, &index, &token) {
                    eprintln!("IPC request failed: {error:#}");
                }
            }
        });
    }
    eprintln!("FlashFind daemon listening on {ADDRESS}");
    loop {
        let (stream, _) = listener.accept()?;
        if client_sender.send(stream).is_err() {
            bail!("FlashFind IPC workers stopped");
        }
    }
}

fn index_writer(roots: Vec<PathBuf>) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(sender, Config::default())?;
    for root in &roots {
        if !root.is_dir() {
            eprintln!("skip unavailable root: {}", root.display());
            continue;
        }
        watcher.watch(root, RecursiveMode::Recursive)?;
    }
    let mut index = Index::open_default()?;
    for root in &roots {
        if root.is_dir() {
            match index.index_root(root) {
                Ok(stats) => eprintln!("indexed {} ({} entries)", root.display(), stats.indexed),
                Err(error) => eprintln!("could not index {}: {error:#}", root.display()),
            }
        }
    }
    loop {
        match receiver.recv_timeout(Duration::from_secs(2)) {
            Ok(Ok(event)) => apply_event(&mut index, &roots, event),
            Ok(Err(error)) => eprintln!("filesystem watch error: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => bail!("filesystem watcher stopped"),
        }
    }
}

fn apply_event(index: &mut Index, roots: &[PathBuf], event: Event) {
    if matches!(event.kind, EventKind::Access(_)) {
        return;
    }
    let may_change_tree = matches!(
        event.kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    for path in event.paths {
        if let Some(root) = roots.iter().find(|root| path.starts_with(root)) {
            // A directory event can represent an entire subtree on some native
            // watcher backends. `is_indexed_directory` covers a removed or
            // renamed directory, which no longer answers true to `is_dir()`.
            if may_change_tree
                && (path.is_dir() || index.is_indexed_directory(&path).unwrap_or(false))
            {
                if let Err(error) = index.index_root(root) {
                    eprintln!("rescan failed for {}: {error:#}", root.display());
                }
                return;
            }
            if let Err(error) = index.refresh_path(&path, root) {
                eprintln!("refresh failed for {}: {error:#}", path.display());
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, index: &Index, token: &str) -> Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let wire: WireRequest = serde_json::from_str(&line)?;
    if wire.token != token {
        write_response(
            &mut stream,
            &Response::Error("unauthorized local request".into()),
        )?;
        return Ok(());
    }
    let response = match wire.request {
        Request::Ping => Response::Pong,
        Request::Search { query, limit } => {
            match index.search_expression(&query, limit.min(SEARCH_LIMIT)) {
                Ok(results) => Response::Results(results),
                Err(error) => Response::Error(error.to_string()),
            }
        }
        Request::Delete { path } => match delete_path(Path::new(&path)) {
            Ok(()) => Response::Ok("deleted".into()),
            Err(error) => Response::Error(error.to_string()),
        },
        Request::Rename { from, to } => match fs::rename(&from, &to) {
            Ok(()) => Response::Ok("renamed".into()),
            Err(error) => Response::Error(error.to_string()),
        },
        Request::Open { path } => match open_path(Path::new(&path)) {
            Ok(()) => Response::Ok("opened".into()),
            Err(error) => Response::Error(error.to_string()),
        },
    };
    write_response(&mut stream, &response)
}

fn write_response(stream: &mut TcpStream, response: &Response) -> Result<()> {
    serde_json::to_writer(&mut *stream, response)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    Ok(())
}

fn send_request(request: Request) -> Result<Response> {
    let mut stream = TcpStream::connect_timeout(&ADDRESS.parse()?, Duration::from_millis(250))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    serde_json::to_writer(
        &mut stream,
        &WireRequest {
            token: ipc_token()?,
            request,
        },
    )?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line)?;
    Ok(serde_json::from_str(&line)?)
}

fn ipc_token() -> Result<String> {
    let directory = data_dir()?;
    fs::create_dir_all(&directory)?;
    let path = directory.join("ipc.token");
    if let Ok(token) = fs::read_to_string(&path) {
        return Ok(token);
    }
    let mut bytes = [0_u8; 32];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not generate IPC secret: {error}"))?;
    let token = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    fs::write(&path, &token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

fn ensure_daemon() -> Result<()> {
    if matches!(send_request(Request::Ping), Ok(Response::Pong)) {
        return Ok(());
    }
    let executable = std::env::current_exe().context("could not locate FlashFind executable")?;
    ProcessCommand::new(executable)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not start background daemon")?;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        thread::sleep(Duration::from_millis(80));
        if matches!(send_request(Request::Ping), Ok(Response::Pong)) {
            return Ok(());
        }
    }
    bail!("FlashFind daemon did not start; run `flashfind daemon` to see its error output")
}

fn delete_path(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        fs::remove_dir_all(path)?;
    } else {
        fs::remove_file(path)?;
    }
    Ok(())
}

fn open_path(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = {
        let mut command = ProcessCommand::new("explorer");
        command.arg(path);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = ProcessCommand::new("open");
        command.arg(path);
        command
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut command = {
        let mut command = ProcessCommand::new("xdg-open");
        command.arg(path);
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Search,
    Rename,
    ConfirmDelete,
}

struct QueryRequest {
    generation: u64,
    query: String,
}

struct QueryResponse {
    generation: u64,
    result: std::result::Result<Vec<SearchResult>, String>,
}

struct App {
    query: String,
    results: Vec<SearchResult>,
    selected: usize,
    status: String,
    mode: Mode,
    rename: String,
    submitted_query: String,
    generation: u64,
    changed_at: Instant,
    query_sender: mpsc::Sender<QueryRequest>,
    query_receiver: mpsc::Receiver<QueryResponse>,
}

impl App {
    fn new() -> Self {
        let (query_sender, query_receiver) = start_query_worker();
        Self {
            query: String::new(),
            results: Vec::new(),
            selected: 0,
            status: "输入关键词；空格/& 为“且”，| 为“或”，支持 * 与 ?".into(),
            mode: Mode::Search,
            rename: String::new(),
            submitted_query: String::new(),
            generation: 0,
            changed_at: Instant::now(),
            query_sender,
            query_receiver,
        }
    }

    fn selected_path(&self) -> Option<String> {
        self.results
            .get(self.selected)
            .map(|result| result.path.to_string_lossy().into_owned())
    }

    fn query_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.changed_at = Instant::now();
    }

    /// Polling never waits for IPC. A slow database, a cold filesystem cache,
    /// or an obsolete query can therefore never freeze key handling or redraw.
    fn refresh(&mut self) {
        while let Ok(response) = self.query_receiver.try_recv() {
            if response.generation != self.generation {
                continue; // An input event superseded this response.
            }
            match response.result {
                Ok(results) => {
                    self.results = results;
                    self.selected = self.selected.min(self.results.len().saturating_sub(1));
                    self.status = format!("{} 个结果", self.results.len());
                }
                Err(error) => {
                    self.results.clear();
                    self.status = error;
                }
            }
        }
        if self.query.trim().is_empty() {
            self.results.clear();
            self.selected = 0;
            self.submitted_query.clear();
            return;
        }
        if self.query == self.submitted_query
            || self.changed_at.elapsed() < Duration::from_millis(110)
        {
            return;
        }
        self.status = "搜索中…".into();
        self.submitted_query = self.query.clone();
        let _ = self.query_sender.send(QueryRequest {
            generation: self.generation,
            query: self.query.clone(),
        });
    }
}

fn start_query_worker() -> (mpsc::Sender<QueryRequest>, mpsc::Receiver<QueryResponse>) {
    let (request_sender, request_receiver) = mpsc::channel::<QueryRequest>();
    let (response_sender, response_receiver) = mpsc::channel::<QueryResponse>();
    thread::spawn(move || {
        while let Ok(mut request) = request_receiver.recv() {
            // If several keys arrive before this worker starts a query, only
            // execute the newest request; intermediate text is not useful.
            while let Ok(newer) = request_receiver.try_recv() {
                request = newer;
            }
            let result = match send_request(Request::Search {
                query: request.query,
                limit: SEARCH_LIMIT,
            }) {
                Ok(Response::Results(results)) => Ok(results),
                Ok(Response::Error(error)) => Err(format!("查询语法错误：{error}")),
                Ok(_) => Err("服务响应异常".into()),
                Err(error) => Err(format!("服务不可用：{error}")),
            };
            if response_sender
                .send(QueryResponse {
                    generation: request.generation,
                    result,
                })
                .is_err()
            {
                break;
            }
        }
    });
    (request_sender, response_receiver)
}

fn run_tui() -> Result<()> {
    ensure_daemon()?;
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let result = tui_loop(&mut terminal);
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn tui_loop(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    let mut app = App::new();
    loop {
        app.refresh();
        terminal.draw(|frame| draw(frame, &app))?;
        if event::poll(Duration::from_millis(35))? {
            match event::read()? {
                TerminalEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    if handle_key(&mut app, key.code, key.modifiers)? {
                        return Ok(());
                    }
                }
                TerminalEvent::Paste(text) => handle_paste(&mut app, &text),
                _ => {}
            }
        }
    }
}

fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> Result<bool> {
    match app.mode {
        Mode::Search => match code {
            // In search mode, every unmodified printable character—including
            // j/k/q/h—belongs to the query. Navigation/actions are reserved to
            // non-printable keys or Ctrl-combinations to prevent collisions.
            KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => return Ok(true),
            KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => {
                pop_grapheme(&mut app.query);
                app.query_changed();
            }
            KeyCode::Backspace => {
                pop_grapheme(&mut app.query);
                app.query_changed();
            }
            KeyCode::Esc => return Ok(true),
            KeyCode::Up => app.selected = app.selected.saturating_sub(1),
            KeyCode::Down => {
                app.selected = (app.selected + 1).min(app.results.len().saturating_sub(1))
            }
            KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
            KeyCode::PageDown => {
                app.selected = (app.selected + 10).min(app.results.len().saturating_sub(1))
            }
            KeyCode::Enter => operate(app, |path| Request::Open { path }),
            KeyCode::Delete => {
                if app.selected_path().is_some() {
                    app.mode = Mode::ConfirmDelete;
                }
            }
            KeyCode::F(2) => {
                if let Some(path) = app.selected_path() {
                    app.rename = path;
                    app.mode = Mode::Rename;
                }
            }
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.query.push(character);
                app.query_changed();
            }
            _ => {}
        },
        Mode::Rename => match code {
            KeyCode::Esc => app.mode = Mode::Search,
            KeyCode::Char('h') if modifiers.contains(KeyModifiers::CONTROL) => {
                pop_grapheme(&mut app.rename);
            }
            KeyCode::Backspace => {
                pop_grapheme(&mut app.rename);
            }
            KeyCode::Enter => {
                let target = app.rename.clone();
                if let Some(from) = app.selected_path() {
                    operate(app, |_| Request::Rename { from, to: target });
                }
                app.mode = Mode::Search;
            }
            KeyCode::Char(character)
                if !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                app.rename.push(character)
            }
            _ => {}
        },
        Mode::ConfirmDelete => match code {
            KeyCode::Char('y') | KeyCode::Enter => {
                operate(app, |path| Request::Delete { path });
                app.mode = Mode::Search;
            }
            _ => app.mode = Mode::Search,
        },
    }
    Ok(false)
}

fn handle_paste(app: &mut App, text: &str) {
    let text = text.replace(['\r', '\n'], " ");
    match app.mode {
        Mode::Search => {
            app.query.push_str(&text);
            app.query_changed();
        }
        Mode::Rename => app.rename.push_str(&text),
        Mode::ConfirmDelete => {}
    }
}

fn pop_grapheme(text: &mut String) {
    if let Some((offset, _)) = text.grapheme_indices(true).next_back() {
        text.truncate(offset);
    }
}

fn input_tail(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    let ellipsis_width = UnicodeWidthStr::width("…");
    let available = max_width.saturating_sub(ellipsis_width);
    let mut width = 0;
    let mut start = text.len();
    for (offset, grapheme) in text.grapheme_indices(true).rev() {
        let grapheme_width = UnicodeWidthStr::width(grapheme);
        if width + grapheme_width > available {
            break;
        }
        width += grapheme_width;
        start = offset;
    }
    format!("…{}", &text[start..])
}

fn highlight_literals(query: &str) -> Vec<Vec<char>> {
    let mut literals = query
        .split(|character: char| {
            matches!(character, '*' | '?' | '&' | '|' | '"') || character.is_whitespace()
        })
        .filter(|literal| !literal.is_empty())
        .map(|literal| literal.chars().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    literals.sort_by_key(|literal| std::cmp::Reverse(literal.len()));
    literals.dedup();
    literals
}

fn highlighted_spans(value: &str, literals: &[Vec<char>]) -> Vec<Span<'static>> {
    let characters = value.chars().collect::<Vec<_>>();
    let mut mask = vec![false; characters.len()];
    for literal in literals {
        if literal.is_empty() || literal.len() > characters.len() {
            continue;
        }
        for start in 0..=characters.len() - literal.len() {
            if characters[start..start + literal.len()]
                .iter()
                .zip(literal)
                .all(|(value, needle)| value.to_lowercase().eq(needle.to_lowercase()))
            {
                mask[start..start + literal.len()].fill(true);
            }
        }
    }
    let mut spans = Vec::new();
    let mut start = 0;
    while start < characters.len() {
        let highlighted = mask[start];
        let mut end = start + 1;
        while end < characters.len() && mask[end] == highlighted {
            end += 1;
        }
        let text = characters[start..end].iter().collect::<String>();
        let span = if highlighted {
            Span::styled(
                text,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            )
        } else {
            Span::raw(text)
        };
        spans.push(span);
        start = end;
    }
    spans
}

fn operate(app: &mut App, build: impl FnOnce(String) -> Request) {
    let Some(path) = app.selected_path() else {
        return;
    };
    match send_request(build(path)) {
        Ok(Response::Ok(message)) => {
            app.status = message;
            app.submitted_query.clear();
            app.query_changed();
        }
        Ok(Response::Error(error)) => app.status = error,
        Ok(_) => app.status = "服务响应异常".into(),
        Err(error) => app.status = error.to_string(),
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(3),
            Constraint::Length(2),
        ])
        .split(frame.area());
    let title = match app.mode {
        Mode::Search => " 搜索（& / |，支持 * 与 ?，引号支持词组） ",
        Mode::Rename => " 重命名：输入完整目标路径并回车 ",
        Mode::ConfirmDelete => " 确认删除：按 y/回车确认，其他键取消 ",
    };
    let text = match app.mode {
        Mode::Rename => &app.rename,
        _ => &app.query,
    };
    let input_width = chunks[0].width.saturating_sub(2) as usize;
    let visible_text = input_tail(text, input_width);
    frame.render_widget(
        Paragraph::new(visible_text.as_str())
            .block(Block::default().borders(Borders::ALL).title(title)),
        chunks[0],
    );
    let highlights = highlight_literals(&app.query);
    let items = app
        .results
        .iter()
        .map(|result| {
            let mut spans = vec![Span::styled(
                format!("{:<9}", kind_label(&result.kind)),
                Style::default().fg(Color::Cyan),
            )];
            spans.extend(highlighted_spans(
                &result.path.to_string_lossy(),
                &highlights,
            ));
            ListItem::new(Line::from(spans))
        })
        .collect::<Vec<_>>();
    let mut state = ListState::default();
    if !app.results.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" 结果 "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            ),
        chunks[1],
        &mut state,
    );
    frame.render_widget(
        Paragraph::new(format!(
            " {}  │ ↑↓ 选择 │ Enter 打开 │ F2 重命名 │ Delete 删除 │ Esc/Ctrl-C 退出",
            app.status
        ))
        .wrap(Wrap { trim: true }),
        chunks[2],
    );
    if app.mode == Mode::ConfirmDelete {
        let area = centered_rect(66, 20, frame.area());
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new("将永久删除所选文件/目录。\n按 y 或 Enter 确认；其他键取消。")
                .block(Block::default().borders(Borders::ALL).title(" 删除确认 "))
                .wrap(Wrap { trim: true }),
            area,
        );
    }
    let cursor_area = chunks[0];
    if app.mode != Mode::ConfirmDelete {
        frame.set_cursor_position((
            cursor_area.x + 1 + UnicodeWidthStr::width(visible_text.as_str()) as u16,
            cursor_area.y + 1,
        ));
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn kind_label(kind: &Kind) -> &'static str {
    match kind {
        Kind::File => "file",
        Kind::Directory => "directory",
        Kind::Symlink => "symlink",
        Kind::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing_removes_an_entire_chinese_grapheme() {
        let mut text = "报告👨‍👩‍👧".to_owned();
        pop_grapheme(&mut text);
        assert_eq!(text, "报告");
        pop_grapheme(&mut text);
        assert_eq!(text, "报");
    }

    #[test]
    fn visible_input_uses_terminal_cell_width() {
        assert_eq!(input_tail("abcd", 3), "…cd");
        assert_eq!(input_tail("搜索报告", 5), "…报告");
    }

    #[test]
    fn all_plain_characters_are_available_for_search() {
        let mut app = App::new();
        for character in "jkq中文".chars() {
            handle_key(&mut app, KeyCode::Char(character), KeyModifiers::NONE).unwrap();
        }
        assert_eq!(app.query, "jkq中文");
        handle_key(&mut app, KeyCode::Backspace, KeyModifiers::NONE).unwrap();
        assert_eq!(app.query, "jkq中");
    }

    #[test]
    fn extracts_literals_for_highlighting() {
        let literals = highlight_literals("报告 & *.pdf | final");
        assert!(literals.contains(&"报告".chars().collect()));
        assert!(literals.contains(&".pdf".chars().collect()));
        assert!(literals.contains(&"final".chars().collect()));
    }
}
