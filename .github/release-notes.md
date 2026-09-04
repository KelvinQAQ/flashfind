# FlashFind v0.1.5

本次发布修复默认以 home 为索引 root 时的 watcher 自触发问题，并显著降低目录事件的索引更新延迟。

## 修复与性能

- 忽略 FlashFind 自身 SQLite 数据目录的事件；`index.sqlite3-wal` / `-shm` 写入不会再触发递归刷新，从而避免索引静默停止更新。
- notify/inotify 报告事件溢出时，daemon 会明确记录日志并完整重建已登记 root，恢复一致性。
- 普通目录新增、删除和改名只重建受影响子树，而不再重扫整个 root。
- inotify 事件以 2 ms 微批合并；递归删除和目录改名产生的重复子项通知会被去重。

在包含约 28,000 条目的隔离 root 中，44 项目录子树操作的数据库提交延迟从原先全 root 重扫的约 0.8–1.0 秒降至约 12–20 ms；105 次文件/目录操作的 p95 为 17.22 ms。

## 下载

请选择与系统和 CPU 架构匹配的归档文件：

| 平台 | CPU 架构 | 文件 |
|---|---|---|
| Linux | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.5-linux-x86_64.tar.gz` |
| Linux | aarch64（ARM64） | `flashfind-v0.1.5-linux-aarch64.tar.gz` |
| Windows | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.5-windows-x86_64.zip` |
| Windows | aarch64（ARM64） | `flashfind-v0.1.5-windows-aarch64.zip` |

每个归档旁附有 `.sha256` 校验文件。Linux 使用静态链接的 musl 二进制，可用于主流发行版。

## 校验

Linux：

```bash
sha256sum -c flashfind-v0.1.5-linux-x86_64.tar.gz.sha256
```

PowerShell：

```powershell
Get-FileHash .\flashfind-v0.1.5-windows-x86_64.zip -Algorithm SHA256
```

解压后直接运行 `flashfind`（Windows 为 `flashfind.exe`）。已有 daemon 的用户应先停止旧 daemon；新版 CLI/TUI 会在协议不匹配时提示重启。
