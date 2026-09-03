# FlashFind v0.1.2

补丁发布：增加后台 daemon IPC 协议协商。升级后若旧 daemon 仍在运行，客户端会明确提示重启，而不会静默复用旧 FTS schema。

## 下载

请选择与系统和 CPU 架构匹配的归档文件：

| 平台 | CPU 架构 | 文件 |
|---|---|---|
| Linux | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.2-linux-x86_64.tar.gz` |
| Linux | aarch64（ARM64） | `flashfind-v0.1.2-linux-aarch64.tar.gz` |
| Windows | x86_64（Intel/AMD 64-bit） | `flashfind-v0.1.2-windows-x86_64.zip` |
| Windows | aarch64（ARM64） | `flashfind-v0.1.2-windows-aarch64.zip` |

每个归档旁附有 `.sha256` 校验文件。Linux 使用静态链接的 musl 二进制，可用于主流发行版。

## 校验

Linux：

```bash
sha256sum -c flashfind-v0.1.2-linux-x86_64.tar.gz.sha256
```

PowerShell：

```powershell
Get-FileHash .\flashfind-v0.1.2-windows-x86_64.zip -Algorithm SHA256
```

解压后直接运行 `flashfind`（Windows 为 `flashfind.exe`）。首次启动会自动启动当前用户的后台索引服务，并默认索引用户主目录。
