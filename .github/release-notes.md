# FlashFind v0.1.9

本次发布将 watcher、daemon 生命周期和高频文件更新路径提升为可观测、可恢复的生产级实现。

## 主要改进

- 每个数据目录使用独立的动态 loopback IPC endpoint；不同 `XDG_DATA_HOME` 可同时运行 daemon，不再争抢固定端口。
- 新增 daemon 管理：

  ```bash
  flashfind daemon start [--wait]
  flashfind daemon status
  flashfind daemon logs --follow
  flashfind daemon restart
  flashfind daemon stop
  ```

- `daemon status` 报告 watcher 初始化、健康、恢复或失败状态，以及 root 数、overflow、queue rescan、初始化/恢复耗时。
- watcher 忽略自身 SQLite 数据目录事件，使用有界 event queue；队列压力或内核 overflow 会触发可观测的一致性恢复，而非静默丢失更新。
- 普通文件事件按微批在一个 SQLite transaction 中写入；目录子树删除使用索引友好的 path range。
- 同一 rename tracker 的 `From`/`To`/`Both` companion events 被合并，避免重复子树重建。
- SQLite WAL 初始化竞争现在重试，24 个并发客户端打开同一新数据库不再偶发 `database is locked`。

## 验证基准

在隔离环境中已验证：

```text
100-file burst median:       9.77 ms
5,000-file rename x10 median: ~150 ms
10,000-file burst:           10,000/10,000 entries indexed
integration suite:           7/7 passed
```

## 下载

| 平台 | CPU 架构 | 文件 |
|---|---|---|
| Linux | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.9-linux-x86_64.tar.gz` |
| Linux | aarch64（ARM64） | `flashfind-v0.1.9-linux-aarch64.tar.gz` |
| Windows | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.9-windows-x86_64.zip` |
| Windows | aarch64（ARM64） | `flashfind-v0.1.9-windows-aarch64.zip` |

每个归档旁附有 `.sha256` 校验文件。升级后请执行：

```bash
flashfind daemon restart
```

对于大 root，`flashfind daemon start` 在 IPC 可用后即返回；如需等待 native recursive watcher 完全就绪，请使用：

```bash
flashfind daemon start --wait
```
