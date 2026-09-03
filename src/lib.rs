//! Fast, low-privilege file indexing and lookup primitives.
//!
//! The index is deliberately scoped to paths the current user can read. No
//! kernel driver, raw-volume access, or elevated privileges are required.

use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use ignore::WalkBuilder;
use rusqlite::{params, Connection, Transaction};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{SystemTime, UNIX_EPOCH},
};

const APP_QUALIFIER: &str = "io";
const APP_ORGANIZATION: &str = "FlashFind";
const APP_NAME: &str = "FlashFind";
const INSERT_SQL: &str = r#"
    INSERT INTO files(path, name, name_folded, kind, size, modified, root)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
"#;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    Other,
}

impl Kind {
    fn from_metadata(meta: &fs::Metadata) -> Self {
        let kind = meta.file_type();
        if kind.is_dir() {
            Self::Directory
        } else if kind.is_symlink() {
            Self::Symlink
        } else if kind.is_file() {
            Self::File
        } else {
            Self::Other
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "other",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub path: PathBuf,
    pub kind: Kind,
    pub size: u64,
    pub modified: Option<i64>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct IndexStats {
    pub indexed: u64,
    pub skipped: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedRoot {
    pub path: PathBuf,
    pub entries: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    pub has_more: bool,
}

#[derive(Debug, Clone)]
pub struct DatabaseStats {
    pub database_bytes: u64,
    pub wal_bytes: u64,
    pub free_pages: u64,
    pub page_count: u64,
}

pub struct Index {
    connection: Connection,
    database_path: PathBuf,
}

impl Index {
    /// Opens the per-user index. Its default location is platform appropriate:
    /// `%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS,
    /// and `$XDG_DATA_HOME` / `~/.local/share` on Linux.
    pub fn open_default() -> Result<Self> {
        let dirs = ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, APP_NAME)
            .context("could not determine an application data directory")?;
        fs::create_dir_all(dirs.data_local_dir()).context("could not create index directory")?;
        Self::open(dirs.data_local_dir().join("index.sqlite3"))
    }

    pub fn open(database: impl AsRef<Path>) -> Result<Self> {
        let database_path = database.as_ref().to_path_buf();
        let mut connection =
            Connection::open(&database_path).context("could not open SQLite index")?;
        connection
            .pragma_update(None, "journal_mode", "WAL")
            .context("could not enable SQLite WAL mode")?;
        connection.pragma_update(None, "synchronous", "NORMAL")?;
        connection.pragma_update(None, "temp_store", "MEMORY")?;
        // A reader/writer hand-off around WAL checkpoints should wait briefly,
        // not surface a transient "database is locked" error to the TUI.
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        connection.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS files (
                id          INTEGER PRIMARY KEY,
                path        TEXT NOT NULL UNIQUE,
                name        TEXT NOT NULL,
                name_folded TEXT NOT NULL,
                kind        TEXT NOT NULL,
                size        INTEGER NOT NULL,
                modified    INTEGER,
                root        TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS roots (
                root        TEXT PRIMARY KEY,
                added        INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS files_root ON files(root);
            CREATE VIRTUAL TABLE IF NOT EXISTS file_grams USING fts5(
                name_folded, tokenize='trigram', content='files', content_rowid='id'
            );
            CREATE TRIGGER IF NOT EXISTS files_ai AFTER INSERT ON files BEGIN
                INSERT INTO file_grams(rowid, name_folded) VALUES (new.id, new.name_folded);
            END;
            CREATE TRIGGER IF NOT EXISTS files_ad AFTER DELETE ON files BEGIN
                INSERT INTO file_grams(file_grams, rowid, name_folded) VALUES ('delete', old.id, old.name_folded);
            END;
            CREATE TRIGGER IF NOT EXISTS files_au AFTER UPDATE ON files BEGIN
                INSERT INTO file_grams(file_grams, rowid, name_folded) VALUES ('delete', old.id, old.name_folded);
                INSERT INTO file_grams(rowid, name_folded) VALUES (new.id, new.name_folded);
            END;
            "#,
        )?;
        migrate_compact_schema(&mut connection)?;
        Ok(Self {
            connection,
            database_path,
        })
    }

    /// Rebuilds one root atomically. Traversal uses `ignore`'s parallel walker;
    /// inaccessible children are skipped rather than aborting the whole index.
    pub fn index_root(&mut self, root: impl AsRef<Path>) -> Result<IndexStats> {
        let root = absolute_normalized(root.as_ref())?;
        if !root.is_dir() {
            bail!("index root is not a directory: {}", root.display());
        }
        let root_text = path_text(&root);
        self.connection.execute(
            "INSERT OR IGNORE INTO roots(root, added) VALUES (?1, ?2)",
            params![root_text, unix_seconds(SystemTime::now())],
        )?;
        let (sender, receiver) = mpsc::channel();
        let walk_root = root.clone();
        let worker_sender = sender.clone();

        std::thread::scope(|scope| {
            scope.spawn(move || {
                WalkBuilder::new(walk_root)
                    .hidden(false)
                    .ignore(false)
                    .git_ignore(false)
                    .git_global(false)
                    .git_exclude(false)
                    .follow_links(false)
                    .build_parallel()
                    .run(|| {
                        let sender = worker_sender.clone();
                        Box::new(move |entry| {
                            if let Ok(entry) = entry {
                                let _ = sender.send(entry.into_path());
                            }
                            ignore::WalkState::Continue
                        })
                    });
            });
            drop(sender);
            self.replace_root(&root_text, receiver)
        })
    }

    fn replace_root(&mut self, root: &str, paths: Receiver<PathBuf>) -> Result<IndexStats> {
        let transaction = self.connection.transaction()?;
        remove_root(&transaction, root)?;
        let mut stats = IndexStats::default();
        {
            let mut insert = transaction.prepare_cached(INSERT_SQL)?;
            for path in paths {
                match insert_path(&mut insert, &path, root) {
                    Ok(()) => stats.indexed += 1,
                    Err(error) if is_skippable_filesystem_error(&error) => stats.skipped += 1,
                    // Database/schema failures are never silently counted as
                    // inaccessible files: that would produce a false-success
                    // index with zero entries.
                    Err(error) => {
                        return Err(error.context(format!("could not index {}", path.display())))
                    }
                }
            }
        }
        transaction.commit()?;
        // Large root replacement can grow WAL close to the database size.
        // PASSIVE checkpoint never blocks active readers and caps long-term
        // disk growth; explicit maintenance can later truncate the file.
        let _ = self
            .connection
            .execute_batch("PRAGMA wal_checkpoint(PASSIVE)");
        Ok(stats)
    }

    /// Whether `path` was indexed as a directory before an event. This lets the
    /// watcher distinguish a removed directory (no longer on disk) from a file.
    pub fn is_indexed_directory(&self, path: impl AsRef<Path>) -> Result<bool> {
        let path = path_text(&absolute_normalized(path.as_ref())?);
        Ok(self
            .connection
            .query_row(
                "SELECT kind = 'directory' FROM files WHERE path = ?1",
                [path],
                |row| row.get(0),
            )
            .unwrap_or(false))
    }

    /// Updates or removes a single path after a filesystem notification.
    pub fn refresh_path(&mut self, path: impl AsRef<Path>, root: impl AsRef<Path>) -> Result<()> {
        let path = absolute_normalized(path.as_ref())?;
        let root = path_text(&absolute_normalized(root.as_ref())?);
        let transaction = self.connection.transaction()?;
        remove_path(&transaction, &path_text(&path))?;
        if path.exists() {
            let mut insert = transaction.prepare_cached(INSERT_SQL)?;
            if let Err(error) = insert_path(&mut insert, &path, &root) {
                if !is_skippable_filesystem_error(&error) {
                    return Err(error.context(format!("could not refresh {}", path.display())));
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Searches a case-insensitive Unicode glob term. `*` matches zero or more
    /// characters and `?` exactly one. A longest literal fragment drives FTS5;
    /// the glob itself is always verified in Rust, so FTS never changes meaning.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let query = fold(query.trim());
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // The requested page size drives candidate collection. This keeps the
        // 200-row TUI first page cheap while allowing a CLI/user to page much
        // further than 200 without silently losing indexed matches.
        let candidate_limit = limit.saturating_mul(6).clamp(256, 50_000) as i64;
        let literal = longest_literal(&query);
        let mut candidates = Vec::new();
        if literal.chars().count() >= 3 {
            let mut statement = self.connection.prepare(
                "SELECT f.path, f.kind, f.size, f.modified, f.name_folded, f.name_folded
                 FROM file_grams g JOIN files f ON f.id = g.rowid
                 WHERE g.name_folded MATCH ?1
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(
                params![fts_phrase(&literal), candidate_limit],
                row_from_query,
            )?;
            for row in rows {
                candidates.push(row?);
            }
        } else if !literal.is_empty() {
            // One/two-character text is deliberately supported. It cannot use
            // a trigram index, so SQLite stops as soon as it has a bounded
            // candidate set; the asynchronous TUI keeps this off the UI thread.
            let pattern = like_contains_pattern(&literal);
            let mut statement = self.connection.prepare(
                "SELECT path, kind, size, modified, name_folded, name_folded FROM files
                 WHERE name_folded LIKE ?1 ESCAPE '\\'
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(params![pattern, candidate_limit], row_from_query)?;
            for row in rows {
                candidates.push(row?);
            }
        } else {
            // A pure wildcard is valid. Its minimum-length predicate is cheap,
            // returns immediately for `*`, and avoids an unbounded Rust scan.
            let (question_marks, has_star) = pure_glob_shape(&query).expect("query has no literal");
            let operator = if has_star { ">=" } else { "=" };
            let sql = format!(
                "SELECT path, kind, size, modified, name_folded, name_folded FROM files
                 WHERE length(name_folded) {operator} ?1
                 LIMIT ?2"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let rows = statement.query_map(
                params![question_marks as i64, candidate_limit],
                row_from_query,
            )?;
            for row in rows {
                candidates.push(row?);
            }
        }
        let compiled_glob = query
            .contains(['*', '?'])
            .then(|| query.chars().collect::<Vec<_>>());
        candidates.retain(|(_, _, _, _, name, _)| match &compiled_glob {
            Some(pattern) => glob_matches_compiled(pattern, name),
            None => name.contains(&query),
        });
        candidates.sort_by_key(|candidate| std::cmp::Reverse(score(&literal, &candidate.4)));
        Ok(candidates
            .into_iter()
            .take(limit)
            .map(search_result)
            .collect())
    }

    pub fn database_stats(&self) -> Result<DatabaseStats> {
        let database_bytes = fs::metadata(&self.database_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let wal_path = PathBuf::from(format!("{}-wal", self.database_path.display()));
        let wal_bytes = fs::metadata(wal_path).map(|meta| meta.len()).unwrap_or(0);
        Ok(DatabaseStats {
            database_bytes,
            wal_bytes,
            free_pages: self
                .connection
                .query_row("PRAGMA freelist_count", [], |row| row.get(0))?,
            page_count: self
                .connection
                .query_row("PRAGMA page_count", [], |row| row.get(0))?,
        })
    }

    /// Safely folds committed WAL pages into the main database and truncates
    /// the WAL if no reader holds an old snapshot.
    pub fn checkpoint(&self) -> Result<DatabaseStats> {
        let _: (i64, i64, i64) =
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?;
        self.database_stats()
    }

    /// Rewrites the database into a dense file. Call only while the daemon is
    /// stopped; SQLite VACUUM requires an exclusive write lock.
    pub fn compact(&self) -> Result<DatabaseStats> {
        self.checkpoint()?;
        self.connection.execute_batch("VACUUM")?;
        self.checkpoint()
    }

    pub fn indexed_roots(&self) -> Result<Vec<PathBuf>> {
        Ok(self
            .root_summaries()?
            .into_iter()
            .map(|root| root.path)
            .collect())
    }

    /// Lists every registered root, including empty roots. A zero count means
    /// the root is registered but currently has no readable indexed entries.
    pub fn root_summaries(&self) -> Result<Vec<IndexedRoot>> {
        let mut statement = self.connection.prepare(
            "SELECT r.root, COUNT(f.id)
             FROM roots r LEFT JOIN files f ON f.root = r.root
             GROUP BY r.root ORDER BY r.root",
        )?;
        let roots = statement
            .query_map([], |row| {
                Ok(IndexedRoot {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    entries: row.get::<_, i64>(1)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(roots)
    }

    /// Evaluates `&` (intersection), `|` (union), whitespace-as-AND, and
    /// quoted terms. `&` has higher precedence than `|`.
    pub fn search_expression(&self, expression: &str, limit: usize) -> Result<Vec<SearchResult>> {
        let groups = parse_expression(expression)?;
        if groups.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        // Intersection quality stays high without letting a multi-term query
        // inflate each term's candidate list into tens of thousands of rows.
        let candidate_limit = limit.saturating_mul(4).clamp(256, 40_000);
        let mut union = std::collections::BTreeMap::<PathBuf, (SearchResult, u16)>::new();
        for group in groups {
            let mut intersection: Option<std::collections::BTreeMap<PathBuf, (SearchResult, u16)>> =
                None;
            for term in group {
                let results = self.search(&term, candidate_limit)?;
                let current = results
                    .into_iter()
                    .map(|result| {
                        let path = result.path.clone();
                        (path, (result, 1_u16))
                    })
                    .collect::<std::collections::BTreeMap<_, _>>();
                intersection = Some(match intersection {
                    None => current,
                    Some(previous) => previous
                        .into_iter()
                        .filter_map(|(path, (result, score))| {
                            current
                                .get(&path)
                                .map(|(_, next_score)| (path, (result, score + next_score)))
                        })
                        .collect(),
                });
                if intersection
                    .as_ref()
                    .is_some_and(|results| results.is_empty())
                {
                    break;
                }
            }
            if let Some(results) = intersection {
                for (path, value) in results {
                    union
                        .entry(path)
                        .and_modify(|existing| existing.1 = existing.1.max(value.1))
                        .or_insert(value);
                }
            }
        }
        let mut results = union.into_values().collect::<Vec<_>>();
        results.sort_by_key(|(_, matches)| std::cmp::Reverse(*matches));
        Ok(results
            .into_iter()
            .take(limit)
            .map(|(result, _)| result)
            .collect())
    }

    /// Returns a stable slice of an expression result. `has_more` is exact for
    /// the requested bounded result window and lets a TUI load subsequent pages
    /// instead of exposing a hidden 200-result ceiling.
    pub fn search_expression_page(
        &self,
        expression: &str,
        offset: usize,
        limit: usize,
    ) -> Result<SearchPage> {
        if limit == 0 {
            return Ok(SearchPage {
                results: Vec::new(),
                has_more: false,
            });
        }
        let requested = offset.saturating_add(limit).saturating_add(1).min(10_001);
        let mut results = self.search_expression(expression, requested)?;
        let has_more = results.len() > offset.saturating_add(limit);
        let start = offset.min(results.len());
        let end = start.saturating_add(limit).min(results.len());
        results = results.drain(start..end).collect();
        Ok(SearchPage { results, has_more })
    }
}

/// Returns the database's platform-appropriate user data directory.
pub fn data_dir() -> Result<PathBuf> {
    let dirs = ProjectDirs::from(APP_QUALIFIER, APP_ORGANIZATION, APP_NAME)
        .context("could not determine an application data directory")?;
    Ok(dirs.data_local_dir().to_path_buf())
}

pub fn default_roots() -> Result<Vec<PathBuf>> {
    let home = directories::UserDirs::new()
        .context("could not determine the current user's home directory")?;
    Ok(vec![home.home_dir().to_path_buf()])
}

/// Rebuilds prior schemas into the compact name-only layout. This removes the
/// obsolete path-folded copy and explicit hex trigram text without weakening
/// substring lookup: FTS5's native trigram tokenizer indexes name_folded.
fn migrate_compact_schema(connection: &mut Connection) -> Result<()> {
    let columns = table_columns(connection, "files")?;
    let fts_is_current = table_columns(connection, "file_grams")?
        .iter()
        .any(|column| column == "name_folded");
    let files_are_current = columns
        == [
            "id",
            "path",
            "name",
            "name_folded",
            "kind",
            "size",
            "modified",
            "root",
        ];
    if files_are_current && fts_is_current {
        return Ok(());
    }
    let transaction = connection.transaction()?;
    transaction.execute_batch(
        "DROP TRIGGER IF EXISTS files_ai;
         DROP TRIGGER IF EXISTS files_ad;
         DROP TRIGGER IF EXISTS files_au;
         DROP TABLE IF EXISTS file_grams;
         CREATE TABLE files_current (
            id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, name TEXT NOT NULL,
            name_folded TEXT NOT NULL, kind TEXT NOT NULL, size INTEGER NOT NULL,
            modified INTEGER, root TEXT NOT NULL
         );
         INSERT INTO files_current(id, path, name, name_folded, kind, size, modified, root)
         SELECT id, path, name, name_folded, kind, size, modified, root FROM files;
         DROP TABLE files;
         ALTER TABLE files_current RENAME TO files;
         CREATE INDEX files_root ON files(root);
         CREATE VIRTUAL TABLE file_grams USING fts5(
            name_folded, tokenize='trigram', content='files', content_rowid='id'
         );
         CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
            INSERT INTO file_grams(rowid, name_folded) VALUES (new.id, new.name_folded);
         END;
         CREATE TRIGGER files_ad AFTER DELETE ON files BEGIN
            INSERT INTO file_grams(file_grams, rowid, name_folded) VALUES ('delete', old.id, old.name_folded);
         END;
         CREATE TRIGGER files_au AFTER UPDATE ON files BEGIN
            INSERT INTO file_grams(file_grams, rowid, name_folded) VALUES ('delete', old.id, old.name_folded);
            INSERT INTO file_grams(rowid, name_folded) VALUES (new.id, new.name_folded);
         END;
         INSERT INTO file_grams(file_grams) VALUES ('rebuild');",
    )?;
    transaction.commit()?;
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<Vec<String>> {
    Ok(connection
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn is_skippable_filesystem_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::NotFound
        )
    })
}

fn remove_root(transaction: &Transaction<'_>, root: &str) -> Result<()> {
    transaction.execute("DELETE FROM files WHERE root = ?1", [root])?;
    Ok(())
}

fn remove_path(transaction: &Transaction<'_>, path: &str) -> Result<()> {
    // Remove descendants too: recursive watcher APIs commonly report only the
    // top-level directory when an entire subtree is moved or deleted. `substr`
    // avoids treating literal `%` or `_` in a filename as LIKE wildcards.
    transaction.execute(
        "DELETE FROM files WHERE path = ?1 OR (
            substr(path, 1, length(?1)) = ?1 AND substr(path, length(?1) + 1, 1) = ?2
        )",
        params![path, std::path::MAIN_SEPARATOR.to_string()],
    )?;
    Ok(())
}

fn insert_path(
    statement: &mut rusqlite::CachedStatement<'_>,
    path: &Path,
    root: &str,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_text(path));
    let path = path_text(path);
    let kind = Kind::from_metadata(&metadata);
    let modified = metadata.modified().ok().and_then(unix_seconds);
    statement.execute(params![
        path,
        name,
        fold(&name),
        kind.as_str(),
        metadata.len() as i64,
        modified,
        root,
    ])?;
    Ok(())
}

fn search_result(
    (path, kind, size, modified, _, _): (String, Kind, u64, Option<i64>, String, String),
) -> SearchResult {
    SearchResult {
        path: PathBuf::from(path),
        kind,
        size,
        modified,
    }
}

fn row_from_query(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<(String, Kind, u64, Option<i64>, String, String)> {
    let kind: String = row.get(1)?;
    Ok((
        row.get(0)?,
        match kind.as_str() {
            "directory" => Kind::Directory,
            "symlink" => Kind::Symlink,
            "other" => Kind::Other,
            _ => Kind::File,
        },
        row.get::<_, i64>(2)? as u64,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn longest_literal(pattern: &str) -> String {
    pattern
        .split(['*', '?'])
        .max_by_key(|fragment| fragment.chars().count())
        .unwrap_or_default()
        .to_owned()
}

/// Escapes an FTS5 phrase so punctuation in a filename is treated literally.
fn fts_phrase(literal: &str) -> String {
    format!("\"{}\"", literal.replace('"', "\"\""))
}

fn like_contains_pattern(literal: &str) -> String {
    let escaped = literal
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Returns the number of `?` markers and whether `*` occurs, or `None` if the
/// pattern contains a literal character.
fn pure_glob_shape(pattern: &str) -> Option<(usize, bool)> {
    let mut questions = 0;
    let mut star = false;
    for character in pattern.chars() {
        match character {
            '?' => questions += 1,
            '*' => star = true,
            _ => return None,
        }
    }
    Some((questions, star))
}

/// Iterative greedy glob matching over Unicode scalar values. It has no regex
/// compilation/allocation and uses a remembered `*` position for backtracking.
fn glob_matches_compiled(pattern: &[char], value: &str) -> bool {
    let value: Vec<char> = value.chars().collect();
    let (mut pattern_index, mut value_index) = (0_usize, 0_usize);
    let (mut star, mut retry_value) = (None, 0_usize);
    while value_index < value.len() {
        if pattern_index < pattern.len()
            && (pattern[pattern_index] == '?' || pattern[pattern_index] == value[value_index])
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == '*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry_value += 1;
            value_index = retry_value;
        } else {
            return false;
        }
    }
    while pattern_index < pattern.len() && pattern[pattern_index] == '*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

fn score(query: &str, name: &str) -> u8 {
    if name == query {
        3
    } else if name.starts_with(query) {
        2
    } else if name.contains(query) {
        1
    } else {
        0
    }
}

fn parse_expression(input: &str) -> Result<Vec<Vec<String>>> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut quoted = false;
    for character in input.chars() {
        match character {
            '"' => quoted = !quoted,
            '&' | '|' if !quoted => {
                if !text.trim().is_empty() {
                    tokens.push(std::mem::take(&mut text).trim().to_owned());
                }
                tokens.push(character.to_string());
            }
            character if character.is_whitespace() && !quoted => {
                if !text.trim().is_empty() {
                    tokens.push(std::mem::take(&mut text).trim().to_owned());
                }
            }
            _ => text.push(character),
        }
    }
    if quoted {
        bail!("unclosed quote in query");
    }
    if !text.trim().is_empty() {
        tokens.push(text.trim().to_owned());
    }
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    let mut groups = vec![Vec::new()];
    let mut needs_term = true;
    for token in tokens {
        match token.as_str() {
            "&" => {
                if needs_term {
                    bail!("`&` needs a term on both sides");
                }
                needs_term = true;
            }
            "|" => {
                if needs_term {
                    bail!("`|` needs a term on both sides");
                }
                groups.push(Vec::new());
                needs_term = true;
            }
            _ => {
                groups.last_mut().expect("groups is nonempty").push(token);
                needs_term = false;
            }
        }
    }
    if needs_term {
        bail!("query cannot end with an operator");
    }
    Ok(groups)
}

fn fold(value: &str) -> String {
    value.to_lowercase()
}

#[cfg(test)]
/// Generates overlapping Unicode-character trigrams as safe ASCII FTS tokens.
/// FTS intersects all tokens, producing a compact candidate set; Rust then
/// verifies the exact substring. Hex encoding keeps separators and CJK valid
/// query terms under SQLite's tokenizer.
fn grams(value: &str) -> String {
    let chars: Vec<_> = value.chars().collect();
    if chars.len() < 3 {
        return format!("g{}", hex(value.as_bytes()));
    }
    chars
        .windows(3)
        .map(|window| {
            let gram: String = window.iter().collect();
            format!("g{}", hex(gram.as_bytes()))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 15) as usize] as char);
    }
    output
}

fn absolute_normalized(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn unix_seconds(time: SystemTime) -> Option<i64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_overlapping_trigrams() {
        assert_eq!(grams("flash"), "g666c61 g6c6173 g617368");
    }

    #[test]
    fn matches_unicode_globs_and_extracts_an_anchor() {
        assert!(glob_matches_compiled(
            &"*.pdf".chars().collect::<Vec<_>>(),
            "季度报告.pdf"
        ));
        assert!(glob_matches_compiled(
            &"项目?告.*".chars().collect::<Vec<_>>(),
            "项目报告.md"
        ));
        assert!("季度报告.pdf".contains("报告"));
        assert!(!glob_matches_compiled(
            &"a?c".chars().collect::<Vec<_>>(),
            "ac"
        ));
        assert_eq!(longest_literal("*.tar?gz"), ".tar");
        assert_eq!(pure_glob_shape("*??"), Some((2, true)));
        assert_eq!(pure_glob_shape("?"), Some((1, false)));
        assert_eq!(like_contains_pattern("100%_done"), "%100\\%\\_done%");
    }

    #[test]
    fn parses_and_before_or_with_quoted_terms() {
        assert_eq!(
            parse_expression("report & \"quarterly final\" | invoice").unwrap(),
            vec![
                vec!["report".to_owned(), "quarterly final".to_owned()],
                vec!["invoice".to_owned()],
            ]
        );
        assert!(parse_expression("report | ").is_err());
    }

    #[test]
    fn migrates_old_path_gram_index_to_name_only_index() {
        let database = std::env::temp_dir().join(format!(
            "flashfind-migration-test-{}-{}.sqlite",
            std::process::id(),
            unix_seconds(SystemTime::now()).unwrap_or_default()
        ));
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE files (
                id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, path_folded TEXT NOT NULL,
                name TEXT NOT NULL, name_folded TEXT NOT NULL, grams TEXT NOT NULL,
                kind TEXT NOT NULL, size INTEGER NOT NULL, modified INTEGER, root TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE file_grams USING fts5(
                grams, tokenize='unicode61', content='files', content_rowid='id'
            );
            CREATE TRIGGER files_ai AFTER INSERT ON files BEGIN
                INSERT INTO file_grams(rowid, grams) VALUES (new.id, new.grams);
            END;",
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO files(path, path_folded, name, name_folded, grams, kind, size, root)
             VALUES(?1, ?2, ?3, ?4, ?5, 'directory', 0, ?6)",
                params![
                    "/root/report-dir",
                    "/root/report-dir",
                    "report-dir",
                    "report-dir",
                    grams("/root/report-dir"),
                    "/root"
                ],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO files(path, path_folded, name, name_folded, grams, kind, size, root)
             VALUES(?1, ?2, ?3, ?4, ?5, 'file', 0, ?6)",
                params![
                    "/root/report-dir/unrelated.txt",
                    "/root/report-dir/unrelated.txt",
                    "unrelated.txt",
                    "unrelated.txt",
                    grams("/root/report-dir/unrelated.txt"),
                    "/root"
                ],
            )
            .unwrap();
        drop(connection);
        let mut index = Index::open(&database).unwrap();
        let results = index.search("report", 10).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].path, PathBuf::from("/root/report-dir"));
        // The key regression: a migrated old `grams NOT NULL` table must now
        // accept newly indexed entries instead of marking every path skipped.
        let directory = std::env::temp_dir().join(format!(
            "flashfind-post-migration-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now()).unwrap_or_default()
        ));
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("new-report.txt"), "x").unwrap();
        assert_eq!(index.index_root(&directory).unwrap().skipped, 0);
        assert!(index
            .search("new-report", 10)
            .unwrap()
            .iter()
            .any(|result| result.path.ends_with("new-report.txt")));
        let _ = fs::remove_dir_all(directory);
        let _ = fs::remove_file(database);
    }

    #[test]
    fn migrates_transitional_name_column_with_old_fts_column() {
        let database = std::env::temp_dir().join(format!(
            "flashfind-transition-test-{}-{}.sqlite",
            std::process::id(),
            unix_seconds(SystemTime::now()).unwrap_or_default()
        ));
        let connection = Connection::open(&database).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE files (
                id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE, path_folded TEXT NOT NULL,
                name TEXT NOT NULL, name_folded TEXT NOT NULL, name_grams TEXT NOT NULL,
                kind TEXT NOT NULL, size INTEGER NOT NULL, modified INTEGER, root TEXT NOT NULL
            );
            CREATE VIRTUAL TABLE file_grams USING fts5(
                grams, tokenize='unicode61', content='files', content_rowid='id'
            );",
            )
            .unwrap();
        connection.execute(
            "INSERT INTO files(path, path_folded, name, name_folded, name_grams, kind, size, root)
             VALUES(?1, ?2, ?3, ?4, ?5, 'file', 0, ?6)",
            params!["/root/needle.txt", "/root/needle.txt", "needle.txt", "needle.txt", grams("needle.txt"), "/root"],
        ).unwrap();
        drop(connection);
        let index = Index::open(&database).unwrap();
        assert_eq!(index.search("needle", 10).unwrap().len(), 1);
        let _ = fs::remove_file(database);
    }

    #[test]
    fn pages_results_and_reports_root_entry_count() {
        let mut index = Index::open(":memory:").unwrap();
        let dir = std::env::temp_dir().join(format!(
            "flashfind-page-test-{}-{}",
            std::process::id(),
            unix_seconds(SystemTime::now()).unwrap_or_default()
        ));
        fs::create_dir_all(&dir).unwrap();
        for number in 0..7 {
            fs::write(dir.join(format!("page-{number}.txt")), "x").unwrap();
        }
        index.index_root(&dir).unwrap();
        let first = index.search_expression_page("page", 0, 3).unwrap();
        assert_eq!(first.results.len(), 3);
        assert!(first.has_more);
        let second = index.search_expression_page("page", 3, 3).unwrap();
        assert_eq!(second.results.len(), 3);
        assert!(second.has_more);
        let roots = index.root_summaries().unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].entries, 8); // root directory plus seven files
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn search_uses_case_folded_substring() {
        let mut index = Index::open(":memory:").unwrap();
        let dir = std::env::temp_dir().join(format!("flashfind-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Résumé-Report.txt"), "x").unwrap();
        fs::write(dir.join("项目报告.md"), "x").unwrap();
        fs::create_dir_all(dir.join("report-directory")).unwrap();
        fs::write(dir.join("report-directory").join("unrelated.txt"), "x").unwrap();
        index.index_root(&dir).unwrap();
        assert!(index
            .search("report", 10)
            .unwrap()
            .iter()
            .any(|result| result.path.ends_with("Résumé-Report.txt")));
        assert!(index
            .search("项目报", 10)
            .unwrap()
            .iter()
            .any(|result| result.path.ends_with("项目报告.md")));
        assert!(index
            .search("*.txt", 10)
            .unwrap()
            .iter()
            .any(|result| result.path.ends_with("Résumé-Report.txt")));
        let directory_matches = index.search("report-directory", 10).unwrap();
        assert_eq!(directory_matches.len(), 1);
        assert!(directory_matches[0].path.ends_with("report-directory"));
        let _ = fs::remove_dir_all(dir);
    }
}
