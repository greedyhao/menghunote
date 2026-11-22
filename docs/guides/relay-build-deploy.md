# Windows 下编译与部署 Linux ARM64 Relay

本文说明如何在 Windows 的 PowerShell 中交叉编译 `menghunote-relay`，并将 Linux ARM64 二进制部署到服务器。
运行服务、初始化数据库、编写配置和配置 TLS 反向代理，请参阅 [`crates/relay/README.md`](../../crates/relay/README.md)。

> 这份指南针对 `aarch64`（Linux ARM64）服务器。若服务器是 `x86_64`，不能直接使用这里的目标三元组。
> Relay 已有独立部署版本；客户端日常更新不需要随之重编 Relay，见 [ADR 0026](../adr/0026-relay-deployment-and-client-releases.md)。

## 选择 Linux 目标

| 目标 | 适用场景 | 说明 |
| --- | --- | --- |
| `aarch64-unknown-linux-gnu` | 服务器的 glibc 版本已知且兼容 | GNU libc 动态链接目标。 |
| `aarch64-unknown-linux-musl` | 不确定服务器 glibc 版本 | musl 目标，通常是部署最省心的选择。 |
| `aarch64-unknown-linux-gnu.2.28` | 需要以 glibc 2.28 为兼容基线 | 服务器应使用 glibc 2.28 或更高版本。 |

## 前置条件

- 在仓库根目录打开 PowerShell。
- 已安装 Rust 工具链、`rustup` 和 Scoop。
- 安装 `cargo-zigbuild` 与 Zig。Scoop 的 `versions/zig-dev` 应为 **0.18 或更高版本**；较旧 Zig 版本编译 SQLite 时效率较低。安装说明见 [Zig 环境安装指南](https://course.ziglang.cc/environment/install-environment)。

```powershell
cargo install cargo-zigbuild
scoop bucket add versions
scoop install versions/zig-dev
```

> 如果 `versions` bucket 已添加，可跳过 `scoop bucket add versions`。

## 编译 GNU 目标

添加 Rust 目标后编译：

```powershell
rustup target add aarch64-unknown-linux-gnu
cargo zigbuild --release -p menghunote-relay `
  --target aarch64-unknown-linux-gnu
```

预期产物：

```text
target\aarch64-unknown-linux-gnu\release\menghunote-relay
```

这是 Linux 可执行文件，**没有 `.exe` 后缀**；它不是可在 Windows 上直接运行的程序。

## 不确定 glibc 版本时：编译 musl

```powershell
rustup target add aarch64-unknown-linux-musl
cargo zigbuild --release -p menghunote-relay `
  --target aarch64-unknown-linux-musl
```

预期产物：

```text
target\aarch64-unknown-linux-musl\release\menghunote-relay
```

该目标面向静态 musl 链接；在服务器 glibc 版本不确定时，通常可以减少部署时的运行库兼容问题。产物同样没有 `.exe` 后缀。

## 指定 glibc 兼容基线

如果要生成以 glibc 2.28 为兼容基线的 GNU 目标，可以使用：

```powershell
cargo zigbuild --release -p menghunote-relay `
  --target aarch64-unknown-linux-gnu.2.28
```

此时目标服务器需要 glibc 2.28 或更高版本。选择目标前应确认服务器架构与 glibc 版本；无法确认时优先考虑上面的 musl 目标。

## 部署到服务器

1. 先确认服务器架构为 ARM64（`uname -m` 应显示 `aarch64`）。
2. 将所选目标目录中的 `menghunote-relay` 传到服务器。以下以 musl 产物为例；若使用 GNU 目标，请替换本地文件路径：

   ```powershell
   scp .\target\aarch64-unknown-linux-musl\release\menghunote-relay `
     user@server.example:/tmp/menghunote-relay
   ```

3. 在服务器上，将文件安装到实际部署路径并设置可执行权限。以下路径仅作示例：

   ```bash
   sudo install -m 0755 /tmp/menghunote-relay /usr/local/bin/menghunote-relay
   ```

4. 首次部署时，按 Relay README 初始化实例并启动服务。升级现有实例时，不要重复执行一次性 `init`；替换二进制前按运维流程停服并备份数据库和配置，再确认本次 Relay 改动是否要求数据库迁移或配置调整。

服务配置、`init` / `serve` / `status` 命令、管理面和 TLS 反向代理的要求以 [`Relay README`](../../crates/relay/README.md) 为准。生产环境由反向代理终止 TLS；不要将开发用 `dev_server` 暴露到公网。
