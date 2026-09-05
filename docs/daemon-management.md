# Daemon management

```bash
flashfind daemon start
flashfind daemon start --wait
flashfind daemon status
flashfind daemon logs --follow
flashfind daemon restart
flashfind daemon stop
```

`start` returns once IPC is available, so existing SQLite results can be searched immediately. Native recursive filesystem watches can still be initializing for a large root. Use `start --wait` in scripts that must wait until watcher status is `healthy`.

`status` reports the local endpoint, watcher state, watched roots, queue rescans, kernel overflow recoveries, initialization/recovery duration, and the latest writer error.

Watcher states:

| State | Meaning |
|---|---|
| `starting` | Writer thread has not initialized yet. |
| `initializing` | Native recursive watches are being created. |
| `healthy` | Filesystem events are being applied. |
| `recovering` | An overflow/queue-pressure rescan is rebuilding indexed roots. |
| `failed` | Watcher setup or recovery failed; inspect logs. |

Logs are stored in the application data directory as `daemon.log`, rotate at 5 MiB to `daemon.log.1`, and can be followed with `daemon logs --follow`.
