# Changelog

All notable changes to FlashFind are documented in this file.

## [Unreleased]

### Fixed

- Removed the service-side 200-result ceiling: CLI queries are paginated up to 10,000 entries and TUI results load additional 200-entry pages on demand.
- Added root entry counts to `flashfind roots` and post-index output, making incomplete root registration visible.
- Made `flashfind index` default to the current user's home directory and allowed a running daemon to discover/watch roots added after startup.
- Kept new clients compatible with the pre-pagination daemon response until that daemon is restarted.
- Changed matching from whole-path matching to entry-name matching, so a matching directory no longer causes unrelated descendants to appear.
- Added responsive TUI result columns for `d/f`, filename, human-readable size, and local modification time.

## [0.1.0] - 2026-09-03

### Added

- Cross-platform low-privilege file index with SQLite FTS5 persistence.
- Background per-user daemon with filesystem notifications and protected local IPC.
- Interactive TUI with async live search, Unicode input, result highlighting, open, rename, and confirmed delete operations.
- Boolean `&` / `|` expressions, quoted terms, and Unicode `*` / `?` glob matching.
- Release automation for Linux x86_64/aarch64 and Windows x86_64/aarch64 binaries.
