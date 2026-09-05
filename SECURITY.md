# Security Policy

## Reporting a vulnerability

Please do not open a public issue for vulnerabilities involving local IPC authentication, daemon endpoint/token files, path disclosure, deletion/rename behavior, or release artifacts.

Report privately through the repository owner's GitHub security contact. Include:

- FlashFind version and commit hash;
- operating system and filesystem;
- minimal reproduction steps;
- impact and suggested mitigation.

We aim to acknowledge reports within 7 days.

## Sensitive local data

FlashFind stores a local SQLite index containing file paths and names. Its daemon log can also contain paths when verbose diagnostics are enabled. Before sharing logs or databases, remove:

```text
ipc.token
daemon.addr
daemon.pid
index.sqlite3
index.sqlite3-wal
index.sqlite3-shm
```

The daemon uses a random loopback endpoint plus a random IPC token. These files are permission-restricted on Unix, but they must never be published.
