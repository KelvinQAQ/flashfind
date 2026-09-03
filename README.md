# FlashFind

跨平台、低权限的“Everything 风格”文件搜索器。它由**每用户后台守护进程**和**轻量终端 TUI**组成：关闭 TUI 后索引与监听仍继续运行；再次启动 TUI 时通过本机 IPC 即时查询。

不会读取 NTFS MFT、安装内核驱动或请求管理员/root 权限。它仅索引当前用户可读取的目录，因此在 Windows、macOS、Linux 具有一致、可审计的权限模型。

## 架构与资源策略

```text
TUI / CLI ──本机回环 TCP + 随机令牌──► flashfind daemon
                                             │
                              SQLite WAL ◄───┼──► notify 原生文件监听
                                             │
                                  ignore 并行初始扫描
```

- **后台守护进程**：后台独占索引写入及原生通知监听（Windows ReadDirectoryChangesW / macOS FSEvents / Linux inotify，由 `notify` 统一适配）。首次 TUI/CLI 查询会自动拉起它；首次运行默认索引用户主目录。
- **TUI**：不扫描、不写数据库；只发送查询和文件操作请求。按键输入经 45ms 去抖，避免每个字符都发请求。
- **存储与并发**：SQLite WAL + `synchronous=NORMAL`。监听写入不会阻塞 TUI 读取；根目录重建在单一事务内完成，读取方要么看到旧索引，要么看到完整新索引。
- **搜索**：路径做 Unicode 小写折叠，并以安全 ASCII 编码的 Unicode 三元组存入 SQLite FTS5；FTS 快速缩小候选集，Rust 精确校验子串。`&` 运算使用候选集交集，`|` 使用并集。
- **安全**：IPC 仅绑定 `127.0.0.1`，并需 256-bit 随机令牌。Unix 上令牌文件权限为 `0600`。删除/重命名仍受当前用户原有的文件系统权限限制。
- **监听健壮性**：目录创建、删除、改名可能携带整个子树，守护进程仅重扫该已配置根目录，避免错误的逐项状态。普通文件事件采用单路径增量更新。

## 构建

安装稳定版 Rust 后执行：

```bash
cargo build --release
```

产物为 `target/release/flashfind`（Windows 为 `.exe`）。SQLite 使用内置 FTS5，不依赖系统 SQLite 的编译选项。

## 二进制发行版

对 `v*` Git tag，GitHub Actions 会构建并附加以下校验过 SHA-256 的发行包：

| 平台 | CPU 架构 | 归档 |
|---|---|---|
| Linux（静态 musl） | x86_64 | `flashfind-vX.Y.Z-linux-x86_64.tar.gz` |
| Linux（静态 musl） | aarch64 / ARM64 | `flashfind-vX.Y.Z-linux-aarch64.tar.gz` |
| Windows | x86_64 | `flashfind-vX.Y.Z-windows-x86_64.zip` |
| Windows | aarch64 / ARM64 | `flashfind-vX.Y.Z-windows-aarch64.zip` |

解压后直接运行 `flashfind`（Windows 为 `flashfind.exe`）。Linux 可使用 `sha256sum -c <归档>.sha256` 校验；PowerShell 可使用 `Get-FileHash <归档> -Algorithm SHA256`。发布者执行 `git push origin main --follow-tags` 后，tag 会自动创建 GitHub Release。

## TUI 使用

```bash
# 自动启动后台服务；首次运行默认索引用户主目录
flashfind
# 或等价命令
flashfind tui
```

界面顶部是搜索框，底部实时显示结果。支持：

```text
report                 # 子串查询
*.pdf                  # 通配符：匹配任意 PDF 文件
项目?告.*              # ? 匹配一个字符，* 匹配零个或多个字符
report quarterly       # 空格为 AND
report & quarterly     # 显式 AND
invoice | receipt      # OR
"2025 final" & *.pdf  # 引号内为一个词组
```

`&` 的优先级高于 `|`；暂不支持括号。没有通配符的词按“包含”匹配；带 `*` 或 `?` 的词按完整文件名或完整路径的 glob 匹配。短关键词、`*`、`?` 等纯通配符也是合法的实时查询：后台会根据模式选取 FTS 三元组、受限 LIKE 或最小长度筛选，并始终限制候选数；非法表达式会在状态栏提示，不会让 TUI 退出。

| 按键 | 操作 |
|---|---|
| `↑` / `↓`、`PageUp` / `PageDown` | 选择结果 |
| `Enter` | 用系统默认程序打开 |
| `F2` | 重命名（输入完整目标路径后回车） |
| `Delete` | 删除，随后需 `y` 或 `Enter` 确认 |
| `Backspace` / `Ctrl-H` | 删除一个完整 Unicode 字素（中文、emoji 也不会被截断） |
| `Esc` / `Ctrl-C` | 退出 TUI；后台服务继续运行 |

搜索模式**没有 Vim 单键快捷键**：`j`、`k`、`q`、`h` 和所有其他普通可打印字符都只会输入搜索框，避免与中文、英文搜索内容冲突。终端启用 bracketed paste，粘贴的多行内容会安全地转换为空格。搜索结果会以黄色加粗下划线标出当前查询的普通文本片段。

## 后台服务与根目录

```bash
# 前台运行，适合调试；首启时显式配置索引根目录
flashfind daemon --root ~/Documents --root ~/Projects

# 只建立/重建指定根目录（并将其持久化为后台根目录）
flashfind index ~/Documents ~/Projects

# 非 TUI 查询
flashfind search 'report & quarterly'

# 查看持久化根目录
flashfind roots
```

TUI 和 `search` 会尝试连接服务；若不存在便在当前用户会话中后台启动 `flashfind daemon`。若希望登录时服务已就绪，可由系统登录项启动：

- **Linux systemd user unit**：`ExecStart=/绝对路径/flashfind daemon`，执行 `systemctl --user enable --now flashfind`。
- **macOS LaunchAgent**：启动命令为 `/绝对路径/flashfind daemon`。
- **Windows Task Scheduler**：创建“用户登录时”任务，动作为 `flashfind.exe daemon`；不需要“使用最高权限”。

具体服务清单刻意未由程序自动写入，以免未经确认修改用户的系统启动配置。

## 测试

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
