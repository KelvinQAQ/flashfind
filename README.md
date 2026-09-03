# FlashFind

跨平台、低权限的“Everything 风格”文件搜索器。它由**每用户后台守护进程**和**轻量终端 TUI**组成：关闭 TUI 后索引与监听仍继续运行；再次启动 TUI 时通过本机 IPC 即时查询。

不会读取 NTFS MFT、安装内核驱动或请求管理员/root 权限。它仅索引当前用户可读取的**已登记根目录**，因此在 Windows、macOS、Linux 具有一致、可审计的权限模型；它不会无权限扫描整个系统盘。首次后台启动或执行不带路径的 `flashfind index` 时，默认根目录是当前用户主目录。

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

### 本地交叉构建（中国网络优化）

仓库提供不需要 `sudo` 或 Docker 的本地交叉构建脚本。Cargo crates 默认沿用已配置的 rsproxy；脚本将 Rustup 配置为 rsproxy，并从 `ziglang.com.cn` 下载 Zig。工具和发行产物均放在项目内的忽略目录，不污染系统环境：

```bash
# 可断点续传地下载 Zig，并安装四个 Rust target / cargo-zigbuild
scripts/bootstrap-cross.sh

# 生成 Linux x86_64/aarch64、Windows x86_64/aarch64 的归档和 SHA-256
scripts/build-release-local.sh
```

默认产物位置为 `dist/v<版本>/`。可按需替换镜像或工具目录：

```bash
ZIG_MIRROR=https://<你的内网或国内镜像>/download scripts/bootstrap-cross.sh
FLASHFIND_TOOLS_DIR=$HOME/.cache/flashfind-tools scripts/build-release-local.sh
```

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

`&` 的优先级高于 `|`；暂不支持括号。没有通配符的词按“包含”匹配；带 `*` 或 `?` 的词按完整**条目名称**的 glob 匹配。目录名命中时只返回该目录，绝不会因为父目录命中而返回其所有子文件。短关键词、`*`、`?` 等纯通配符也是合法的实时查询：后台会根据模式选取 FTS 三元组、受限 LIKE 或最小长度筛选，并始终限制候选数；非法表达式会在状态栏提示，不会让 TUI 退出。

| 按键 | 操作 |
|---|---|
| `↑` / `↓`、`PageUp` / `PageDown` | 选择结果 |
| `Enter` | 用系统默认程序打开所选文件/目录 |
| `Shift+Enter` | 用系统文件管理器打开所选条目的所在目录 |
| `F2` | 重命名（输入完整目标路径后回车） |
| `Delete` | 删除，随后需 `y` 或 `Enter` 确认 |
| `Backspace` / `Ctrl-H` | 删除一个完整 Unicode 字素（中文、emoji 也不会被截断） |
| `Esc` / `Ctrl-C` | 退出 TUI；后台服务继续运行 |

搜索模式**没有 Vim 单键快捷键**：`j`、`k`、`q`、`h` 和所有其他普通可打印字符都只会输入搜索框，避免与中文、英文搜索内容冲突。终端启用 bracketed paste，粘贴的多行内容会安全地转换为空格。搜索结果会以黄色加粗下划线标出当前查询的普通文本片段。

结果列表采用 `D/F | 完整路径 | 大小 | 修改时间` 的响应式列布局：`D` 为目录，`F` 为其他文件条目；完整路径可直接用于定位，大小自动使用 `B/KiB/MiB/GiB/TiB`，修改时间为本地时区的 `YYYY-MM-DD hh:mm:ss`。字段间使用两字符空隙。终端变窄时先隐藏修改时间，再隐藏大小；完整路径始终保留，并在必要时以中间 `…` 省略、保留路径前缀和文件名/扩展名。

## 后台服务与根目录

```bash
# 前台运行，适合调试；首启时显式配置索引根目录
flashfind daemon --root ~/Documents --root ~/Projects

# 不带路径：建立/重建当前用户主目录；带路径：建立/重建指定根目录。
# 完成后显示每个 root 的实际索引条目数。
flashfind index
flashfind index ~/Documents ~/Projects

# 非 TUI 查询：默认最多显示 1000 条；可取 1..10000 条并手动分页
flashfind search 'report & quarterly'
flashfind search report --limit 10000
flashfind search report --limit 1000 --offset 1000

# 查看持久化根目录及每个 root 的实际条目数
flashfind roots
```

TUI 和 `search` 会尝试连接服务；若不存在便在当前用户会话中后台启动 `flashfind daemon`。TUI 首屏加载 200 条以保持流畅；选择项接近列表末尾时会自动继续加载，不存在 200 条总上限。`flashfind index <新目录>` 后，已运行的 daemon 最多在约 2 秒内发现该 root、建立原生监听，无需手动重启。若希望登录时服务已就绪，可由系统登录项启动：

- **Linux systemd user unit**：`ExecStart=/绝对路径/flashfind daemon`，执行 `systemctl --user enable --now flashfind`。
- **macOS LaunchAgent**：启动命令为 `/绝对路径/flashfind daemon`。
- **Windows Task Scheduler**：创建“用户登录时”任务，动作为 `flashfind.exe daemon`；不需要“使用最高权限”。

具体服务清单刻意未由程序自动写入，以免未经确认修改用户的系统启动配置。

### 升级后必须重启后台服务

TUI/CLI 与 daemon 是两个独立进程。升级 `flashfind` 二进制不会替换已经监听在本机端口上的旧 daemon；旧 daemon 也不会执行新版本的 SQLite/FTS migration。自 `v0.1.2` 起，客户端会通过 IPC 协议协商识别这种状态并明确要求重启，而不会静默复用旧服务。使用更早版本时，若升级后 `*` 或其他搜索结果明显过少，先确认版本并重启 daemon：

```bash
flashfind --version        # 应显示当前安装版本，例如 0.1.1
flashfind roots            # 重启后核对每个 root 的实际条目数
flashfind search '*' --limit 1000
```

- **systemd user service**：`systemctl --user restart flashfind`
- **macOS LaunchAgent**：重新启动对应 LaunchAgent，或先停止其 `flashfind daemon` 进程。
- **Windows Task Scheduler**：结束旧任务实例后重新运行任务。
- **前台/自动拉起 daemon**：先退出旧 `flashfind daemon` 进程，再重新执行 `flashfind` 或 `flashfind daemon`。

在 Linux/macOS 上，确认没有其他 FlashFind 会话后可使用 `pkill -f 'flashfind daemon'`；Windows PowerShell 可使用 `Get-Process flashfind | Stop-Process`。这些命令会同时影响同一用户运行的所有 FlashFind daemon/TUI，请先保存需要的终端操作。

## 测试

```bash
cargo fmt -- --check
cargo test
cargo clippy --all-targets -- -D warnings
```
