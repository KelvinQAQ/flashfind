# Changelog

All notable changes to FlashFind are documented in this file.

## [0.1.5] - 2026-09-03

### Added

- `Shift+Enter` opens the selected entry's containing directory using the platform file manager.

### Fixed

- Bumped the local IPC protocol after adding the containing-directory request, so clients require a daemon restart rather than sending the new action to an older service.
- Ignored the daemon's own SQLite data directory in filesystem events, preventing WAL/SHM writes under a default home root from recursively re-triggering the watcher and silently stalling updates.
- Rebuild only an affected directory subtree for ordinary directory create, rename, and delete events instead of rescanning the whole root; coalesce short inotify bursts to remove redundant descendant updates and tail-latency spikes.
- Rebuild all roots when notify reports an event-overflow/rescan condition, rather than leaving a silently stale index.

### Added

- Added a repeatable end-to-end watcher benchmark covering file and directory subtree create, modify, rename, and delete operations at depths 1 through 6.

## [0.1.4] - 2026-09-03

### Fixed

- Rebuilt legacy `files.grams NOT NULL` tables during migration; this prevents every new entry from being silently skipped and makes existing indexes writable again.
- Escalated non-filesystem index errors instead of counting them as skipped paths.

## [0.1.3] - 2026-09-03

### Fixed

- Centered the TUI result viewport around the cursor while scrolling, with natural clamping at the first and last page.

## [0.1.2] - 2026-09-03

### Fixed

- Added IPC protocol negotiation so a new client refuses to silently use an old daemon that cannot migrate the FTS index; it now gives a direct restart instruction.

## [0.1.1] - 2026-09-03

### Fixed

- Removed the service-side 200-result ceiling: CLI queries are paginated up to 10,000 entries and TUI results load additional 200-entry pages on demand.
- Added root entry counts to `flashfind roots` and post-index output, making incomplete root registration visible.
- Made `flashfind index` default to the current user's home directory and allowed a running daemon to discover/watch roots added after startup.
- Kept new clients compatible with the pre-pagination daemon response until that daemon is restarted.
- Changed matching from whole-path matching to entry-name matching, so a matching directory no longer causes unrelated descendants to appear.
- Added responsive TUI result columns for `D/F`, full path, human-readable size, and local modification time.
- Migrated FTS to a name-only schema and repaired a transitional FTS schema that could make `*` return too few results.

## [0.1.0] - 2026-09-03

### Added

- Cross-platform low-privilege file index with SQLite FTS5 persistence.
- Background per-user daemon with filesystem notifications and protected local IPC.
- Interactive TUI with async live search, Unicode input, result highlighting, open, rename, and confirmed delete operations.
- Boolean `&` / `|` expressions, quoted terms, and Unicode `*` / `?` glob matching.
- Release automation for Linux x86_64/aarch64 and Windows x86_64/aarch64 binaries.
