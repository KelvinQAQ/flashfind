use anyhow::{bail, Context, Result};
use chrono::{Local, TimeZone};
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
const IPC_PROTOCOL_VERSION: u16 = 2;
const TUI_PAGE_SIZE: usize = 200;
const CLI_DEFAULT_LIMIT: usize = 1_000;
const MAX_SEARCH_LIMIT: usize = 10_000;

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
    /// Build or rebuild index roots; without paths, indexes the current user's home directory.
    Index { roots: Vec<PathBuf> },
    /// Query the service without opening the TUI.
    Search {
        query: String,
        /// Maximum number of matches to print (1..=10000).
        #[arg(short, long, default_value_t = CLI_DEFAULT_LIMIT)]
        limit: usize,
        /// Skip this many matches before printing, for manual pagination.
        #[arg(long, default_value_t = 0)]
        offset: usize,
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
    Status,
    Search {
        query: String,
        offset: usize,
        limit: usize,
    },
    Delete {
        path: String,
    },
    Rename {
        from: String,
        to: String,
    },
    Open {
        path: String,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum SearchReply {
    Page(flashfind::SearchPage),
    // A freshly upgraded TUI/CLI can still query an old daemon until that
    // daemon is restarted; older daemons returned Results(Vec<SearchResult>).
    Legacy(Vec<SearchResult>),
}

impl SearchReply {
    fn into_page(self) -> flashfind::SearchPage {
        match self {
            Self::Page(page) => page,
            Self::Legacy(results) => flashfind::SearchPage {
                results,
                has_more: false,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
enum Response {
    Pong,
    Status { protocol: u16, version: String },
    Results(SearchReply),
    Ok(String),
    Error(String),
}

fn main() -> Result<()> {
    match Cli::parse().command.unwrap_or(Command::Tui) {
        Command::Tui => run_tui(),
        Command::Daemon { roots } => run_daemon(roots),
        Command::Index { roots } => {
            let roots = if roots.is_empty() {
                default_roots()?
            } else {
                roots
            };
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
            println!("\nRegistered roots:");
            for root in index.root_summaries()? {
                println!("{:>10}  {}", root.entries, root.path.display());
            }
            Ok(())
        }
        Command::Search {
            query,
            limit,
            offset,
        } => {
            if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
                bail!("--limit must be between 1 and {MAX_SEARCH_LIMIT}");
            }
            ensure_daemon()?;
            match send_request(Request::Search {
                query,
                offset,
                limit,
            })? {
                Response::Results(page) => {
                    let page = page.into_page();
                    for result in page.results {
                        println!("{}  {}", kind_code(&result.kind), result.path.display());
                    }
                    if page.has_more {
                        eprintln!(
                            "more results available; continue with --offset {}",
                            offset.saturating_add(limit)
                        );
                    }
                    Ok(())
                }
                Response::Error(error) => bail!(error),
                _ => bail!("unexpected service response"),
            }
        }
        Command::Roots => {
            let index = Index::open_default()?;
            for root in index.root_summaries()? {
                println!("{:>10}  {}", root.entries, root.path.display());
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

fn index_writer(mut roots: Vec<PathBuf>) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(sender, Config::default())?;
    let mut index = Index::open_default()?;
    for root in &roots {
        if !root.is_dir() {
            eprintln!("skip unavailable root: {}", root.display());
            continue;
        }
        watcher.watch(root, RecursiveMode::Recursive)?;
    }
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
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // `flashfind index <new-root>` can run after the daemon has
                // started. Discover it here, index it once, and attach a native
                // watcher without requiring users to restart the service.
                for root in index.indexed_roots()? {
                    if roots.contains(&root) || !root.is_dir() {
                        continue;
                    }
                    watcher.watch(&root, RecursiveMode::Recursive)?;
                    match index.index_root(&root) {
                        Ok(stats) => {
                            eprintln!("added root {} ({} entries)", root.display(), stats.indexed)
                        }
                        Err(error) => eprintln!("could not add root {}: {error:#}", root.display()),
                    }
                    roots.push(root);
                }
            }
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
        Request::Status => Response::Status {
            protocol: IPC_PROTOCOL_VERSION,
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        Request::Search {
            query,
            offset,
            limit,
        } => match index.search_expression_page(&query, offset, limit.clamp(1, MAX_SEARCH_LIMIT)) {
            Ok(results) => Response::Results(SearchReply::Page(results)),
            Err(error) => Response::Error(error.to_string()),
        },
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
    match send_request(Request::Status) {
        Ok(Response::Status { protocol, .. }) if protocol == IPC_PROTOCOL_VERSION => return Ok(()),
        Ok(Response::Status { protocol, version }) => {
            bail!("FlashFind daemon protocol {protocol} (version {version}) is incompatible; restart the background daemon after upgrading")
        }
        // An old daemon does not recognize Status. Confirm it answers the
        // legacy ping, then fail explicitly instead of trying to spawn a new
        // daemon that cannot bind its already occupied local port.
        _ if matches!(send_request(Request::Ping), Ok(Response::Pong)) => {
            bail!("an older FlashFind daemon is still running; restart it so the current binary can migrate and use the new index")
        }
        _ => {}
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
    offset: usize,
}

struct QueryResponse {
    generation: u64,
    offset: usize,
    result: std::result::Result<flashfind::SearchPage, String>,
}

struct App {
    query: String,
    results: Vec<SearchResult>,
    selected: usize,
    has_more: bool,
    loading_more: bool,
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
            has_more: false,
            loading_more: false,
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
        self.has_more = false;
        self.loading_more = false;
    }

    fn load_more(&mut self) {
        if !self.has_more || self.loading_more || self.query.trim().is_empty() {
            return;
        }
        self.loading_more = true;
        let _ = self.query_sender.send(QueryRequest {
            generation: self.generation,
            query: self.query.clone(),
            offset: self.results.len(),
        });
    }

    /// Polling never waits for IPC. A slow database, a cold filesystem cache,
    /// or an obsolete query can therefore never freeze key handling or redraw.
    fn refresh(&mut self) {
        while let Ok(response) = self.query_receiver.try_recv() {
            if response.generation != self.generation {
                continue; // An input event superseded this response.
            }
            self.loading_more = false;
            match response.result {
                Ok(page) => {
                    if response.offset == 0 {
                        self.results = page.results;
                    } else {
                        self.results.extend(page.results);
                    }
                    self.has_more = page.has_more;
                    self.selected = self.selected.min(self.results.len().saturating_sub(1));
                    self.status = if self.has_more {
                        format!("已加载 {} 个结果；继续向下可加载更多", self.results.len())
                    } else {
                        format!("{} 个结果", self.results.len())
                    };
                }
                Err(error) => {
                    if response.offset == 0 {
                        self.results.clear();
                    }
                    self.status = error;
                }
            }
        }
        if self.query.trim().is_empty() {
            self.results.clear();
            self.selected = 0;
            self.has_more = false;
            self.loading_more = false;
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
            offset: 0,
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
                offset: request.offset,
                limit: TUI_PAGE_SIZE,
            }) {
                Ok(Response::Results(results)) => Ok(results.into_page()),
                Ok(Response::Error(error)) => Err(format!("查询语法错误：{error}")),
                Ok(_) => Err("服务响应异常".into()),
                Err(error) => Err(format!("服务不可用：{error}")),
            };
            if response_sender
                .send(QueryResponse {
                    generation: request.generation,
                    offset: request.offset,
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
                app.selected = (app.selected + 1).min(app.results.len().saturating_sub(1));
                if app.selected.saturating_add(40) >= app.results.len() {
                    app.load_more();
                }
            }
            KeyCode::PageUp => app.selected = app.selected.saturating_sub(10),
            KeyCode::PageDown => {
                app.selected = (app.selected + 10).min(app.results.len().saturating_sub(1));
                if app.selected.saturating_add(40) >= app.results.len() {
                    app.load_more();
                }
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

/// The query applies to the leaf name. Render the full path, but do not
/// highlight an identical word in a parent directory, which would suggest a
/// path-match that the search engine intentionally does not perform.
fn highlighted_path_spans(path: &str, literals: &[Vec<char>]) -> Vec<Span<'static>> {
    let separator = std::path::MAIN_SEPARATOR;
    match path.rfind(separator) {
        Some(offset) => {
            let mut spans = vec![Span::raw(path[..=offset].to_owned())];
            spans.extend(highlighted_spans(
                &path[offset + separator.len_utf8()..],
                literals,
            ));
            spans
        }
        None => highlighted_spans(path, literals),
    }
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

#[derive(Clone, Copy)]
struct ResultColumns {
    name_width: usize,
    show_size: bool,
    show_modified: bool,
}

impl ResultColumns {
    fn for_width(width: usize) -> Self {
        // `D  ` consumes three cells. Optional data is separated by exactly
        // two cells: `D  <path>  <size:>8  <timestamp:19>`.
        // Keep at least 13 cells for the full path before showing all metadata.
        if width >= 48 {
            Self {
                name_width: width - 35,
                show_size: true,
                show_modified: true,
            }
        } else if width >= 24 {
            Self {
                name_width: width - 13,
                show_size: true,
                show_modified: false,
            }
        } else {
            Self {
                name_width: width.saturating_sub(3).max(1),
                show_size: false,
                show_modified: false,
            }
        }
    }
}

fn result_item(
    result: &SearchResult,
    highlights: &[Vec<char>],
    columns: ResultColumns,
) -> ListItem<'static> {
    let path = result.path.to_string_lossy().into_owned();
    let path = middle_ellipsis(&path, columns.name_width);
    let mut spans = vec![Span::styled(
        format!("{}  ", kind_code(&result.kind)),
        Style::default().fg(Color::Cyan),
    )];
    spans.extend(highlighted_path_spans(&path, highlights));
    let padding = columns
        .name_width
        .saturating_sub(UnicodeWidthStr::width(path.as_str()));
    spans.push(Span::raw(" ".repeat(padding)));
    if columns.show_size {
        spans.push(Span::styled(
            format!("  {:>8}", format_size(result)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    if columns.show_modified {
        spans.push(Span::styled(
            format!("  {}", format_modified(result.modified)),
            Style::default().fg(Color::DarkGray),
        ));
    }
    ListItem::new(Line::from(spans))
}

fn kind_code(kind: &Kind) -> char {
    if matches!(kind, Kind::Directory) {
        'D'
    } else {
        'F'
    }
}

fn format_size(result: &SearchResult) -> String {
    if matches!(result.kind, Kind::Directory) {
        return "-".into();
    }
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = result.size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", result.size)
    } else if value >= 10.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_modified(modified: Option<i64>) -> String {
    modified
        .and_then(|seconds| Local.timestamp_opt(seconds, 0).single())
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "-".into())
}

/// Fits a display string by retaining both ends. Paths/names often carry their
/// useful extension or suffix at the end, so `…` is better than tail-only trim.
fn middle_ellipsis(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_owned();
    }
    if max_width <= 1 {
        return "…".into();
    }
    let budget = max_width - 1;
    let left_budget = budget.div_ceil(2);
    let right_budget = budget / 2;
    let graphemes = text.graphemes(true).collect::<Vec<_>>();
    let mut left = String::new();
    let mut left_width = 0;
    for grapheme in &graphemes {
        let width = UnicodeWidthStr::width(*grapheme);
        if left_width + width > left_budget {
            break;
        }
        left.push_str(grapheme);
        left_width += width;
    }
    let mut right_parts = Vec::new();
    let mut right_width = 0;
    for grapheme in graphemes.iter().rev() {
        let width = UnicodeWidthStr::width(*grapheme);
        if right_width + width > right_budget {
            break;
        }
        right_parts.push(*grapheme);
        right_width += width;
    }
    right_parts.reverse();
    format!("{left}…{}", right_parts.concat())
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
    let columns = ResultColumns::for_width(chunks[1].width.saturating_sub(2) as usize);
    let items = app
        .results
        .iter()
        .map(|result| result_item(result, &highlights, columns))
        .collect::<Vec<_>>();
    let visible_rows = chunks[1].height.saturating_sub(2).max(1) as usize;
    let offset = centered_list_offset(app.selected, app.results.len(), visible_rows);
    let mut state = ListState::default().with_offset(offset);
    if !app.results.is_empty() {
        state.select(Some(app.selected));
    }
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::default().borders(Borders::ALL))
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

/// Keeps selection in the middle of the list wherever possible. At the first
/// and last page the offset is clamped so the viewport remains filled rather
/// than showing blank rows below the final item.
fn centered_list_offset(selected: usize, item_count: usize, visible_rows: usize) -> usize {
    if item_count <= visible_rows || visible_rows == 0 {
        return 0;
    }
    let middle = visible_rows / 2;
    selected
        .saturating_sub(middle)
        .min(item_count.saturating_sub(visible_rows))
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
    fn result_columns_drop_metadata_before_filename() {
        let wide = ResultColumns::for_width(80);
        assert!(wide.show_size && wide.show_modified);
        let medium = ResultColumns::for_width(40);
        assert!(medium.show_size && !medium.show_modified);
        let narrow = ResultColumns::for_width(15);
        assert!(!narrow.show_size && !narrow.show_modified);
        assert_eq!(
            middle_ellipsis("very-long-report-name.txt", 12),
            "very-l…e.txt"
        );
    }

    #[test]
    fn formats_compact_metadata() {
        let file = SearchResult {
            path: PathBuf::from("report.txt"),
            kind: Kind::File,
            size: 1_572_864,
            modified: Some(0),
        };
        assert_eq!(format_size(&file), "1.5 MiB");
        let modified = format_modified(file.modified);
        assert_eq!(modified.len(), 19);
        assert!(modified.starts_with("1970-01-01"));
        let directory = SearchResult {
            kind: Kind::Directory,
            ..file
        };
        assert_eq!(format_size(&directory), "-");
    }

    #[test]
    fn keeps_selection_centered_except_at_list_edges() {
        assert_eq!(centered_list_offset(0, 100, 11), 0);
        assert_eq!(centered_list_offset(5, 100, 11), 0);
        assert_eq!(centered_list_offset(40, 100, 11), 35);
        assert_eq!(centered_list_offset(99, 100, 11), 89);
        assert_eq!(centered_list_offset(3, 4, 11), 0);
    }

    #[test]
    fn extracts_literals_for_highlighting() {
        let literals = highlight_literals("报告 & *.pdf | final");
        assert!(literals.contains(&"报告".chars().collect()));
        assert!(literals.contains(&".pdf".chars().collect()));
        assert!(literals.contains(&"final".chars().collect()));
    }
}
