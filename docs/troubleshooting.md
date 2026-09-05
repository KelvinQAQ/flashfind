# Troubleshooting

## `watcher: initializing`

Large roots can take seconds while native recursive watches are registered. Check:

```bash
flashfind daemon status
flashfind daemon logs --follow
```

Use `flashfind daemon start --wait` if automation must wait for `healthy`.

## `watcher: failed`

Inspect the reported error and daemon log. Common causes include an unavailable root, inotify watch limits, or a failed overflow recovery. Restart after fixing the root/system issue:

```bash
flashfind daemon restart
```

## Search results are stale

Check watcher state and counters. A nonzero `overflows` or `queue rescans` means FlashFind performed an automatic consistency rebuild. If status remains failed, inspect logs and restart.

## Old daemon after upgrade

```bash
flashfind daemon restart
```

If an older daemon does not support managed shutdown, stop that one legacy process once, then rerun `daemon start`.

## Linux inotify limits

Very large homes may exceed `fs.inotify.max_user_watches`. Increase the system limit according to your distribution's documented administration procedure, then restart FlashFind.

## Sensitive diagnostics

Do not publish `ipc.token`, `daemon.addr`, `daemon.pid`, SQLite files, or unredacted logs containing private paths.
