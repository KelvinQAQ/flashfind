# Architecture

```text
TUI / CLI ── data-directory-private loopback endpoint + token ── daemon
                                                               │
                                             native watcher ──┼── SQLite WAL index
                                                               │
                                                       ignore parallel scan
```

The daemon is the sole index writer. Search clients use authenticated local IPC and independent SQLite reader connections in daemon workers.

## Consistency

- Ordinary file events are coalesced and written per root in one SQLite transaction.
- Directory events refresh only the affected subtree.
- Kernel overflow or user-space queue pressure causes a full root recovery, not silent event loss.
- The daemon data directory is excluded from watcher updates and default-root traversal.
- Large scans use bounded channels to avoid unbounded path accumulation.

## IPC isolation

Each application data directory has its own random loopback endpoint in `daemon.addr`, PID in `daemon.pid`, and random token in `ipc.token`. Independent `XDG_DATA_HOME` values therefore run independent daemons.
