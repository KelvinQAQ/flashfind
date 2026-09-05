# FlashFind v0.1.6

本次发布加入 daemon 生命周期管理与可观察日志，并强制新 CLI 识别旧 daemon，避免升级后静默复用不含 watcher 修复的后台服务。

## 新增 daemon 管理命令

```bash
flashfind daemon start
flashfind daemon status
flashfind daemon logs --lines 100
flashfind daemon restart
flashfind daemon stop
```

- `start` 在后台启动 daemon，并把 stdout/stderr 追加到应用数据目录的 `daemon.log`。
- `stop` 通过本机认证 IPC 让受管 daemon 优雅退出。
- `restart` 用于升级后切换到当前二进制。
- `status` 显示 PID、协议、版本、兼容性和日志位置。
- 前台诊断可使用 `flashfind daemon --verbose run` 查看原生文件监听事件批次。

## 修复

- IPC protocol 升级，新的 CLI 不会再静默连接到旧 watcher daemon。
- 目录事件继续使用子树级增量刷新；SQLite WAL/SHM 自触发过滤和 notify overflow 兜底重扫仍然生效。

## 下载

请选择与系统和 CPU 架构匹配的归档文件：

| 平台 | CPU 架构 | 文件 |
|---|---|---|
| Linux | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.6-linux-x86_64.tar.gz` |
| Linux | aarch64（ARM64） | `flashfind-v0.1.6-linux-aarch64.tar.gz` |
| Windows | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.6-windows-x86_64.zip` |
| Windows | aarch64（ARM64） | `flashfind-v0.1.6-windows-aarch64.zip` |

每个归档旁附有 `.sha256` 校验文件。Linux 使用静态链接的 musl 二进制，可用于主流发行版。

升级后执行：

```bash
flashfind daemon restart
```

若提示存在不支持受管关闭的旧 daemon，请按提示手动结束那一次旧进程；之后使用上述 daemon 子命令管理即可。
