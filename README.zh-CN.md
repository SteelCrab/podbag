# podbag

<img src="desktop/icons/128x128.png" alt="podbag" width="120" align="right">

没有守护进程的容器引擎，只是一个静态二进制文件。
镜像按文件存储：多个镜像共用的文件只保存一份。
可以使用命令行或 [Podbag Desktop](#podbag-desktop)。

[![CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml/badge.svg)](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SteelCrab/podbag)](https://github.com/SteelCrab/podbag/releases/latest)
[![crates.io](https://img.shields.io/crates/v/podbag)](https://crates.io/crates/podbag)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Website](https://img.shields.io/badge/website-podbag.dev-2a78d6)](https://podbag.dev)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · **简体中文** · [Filipino](README.fil.md) · [Español](README.es.md) · [Tiếng Việt](README.vi.md)

<img src="docs/images/intro.svg" alt="podbag 介绍视频。其他引擎的守护进程一直在运行；podbag 只在容器运行时工作，空闲时什么也不运行。其他引擎按层存储，podbag 按文件存储，每个文件只存一份：8 个 Spring 服务、2 个版本占用磁盘 902 MiB（而不是 2,970 MiB），页缓存 324 MiB（而不是 982 MiB）。容器 4.1 毫秒启动，比 Docker（73.8 毫秒）快 18 倍。支持 Linux、macOS 和 Windows。podbag.dev。" width="800">

<sub>同样的介绍也有视频版（英文）: [intro.mp4](docs/media/intro.mp4).</sub>

## 目录

- [特性](#特性)
- [Podbag Desktop](#podbag-desktop)
- [安装](#安装)
- [使用](#使用)
- [基准测试](#基准测试)
- [与其他工具比较](#与其他工具比较)
- [文档](#文档)
- [参与贡献](#参与贡献)
- [许可证](#许可证)

## 特性

- **跨镜像共享文件。** 即使位于不同的镜像层，相同的文件也可以共享磁盘空间和 Linux 页缓存。
- **Linux 引擎按需运行。** 一个进程负责管理同一用户的容器。最后一个容器停止后，进程会在空闲超时后退出。
- **macOS 虚拟机自动关机。** 默认情况下，Linux 虚拟机空闲 60 秒后会关机。下一条容器命令会再次启动它。
- **Kubernetes 节点运行时（`v0.2.0`）。** 在 Linux 上运行 `podbag cri serve` 后，kubelet 可以直接通过 Podbag 运行 Pod。
- **跨引擎迁移（`v0.2.0`）。** 可以在兼容 Docker 的引擎之间复制镜像、卷和已停止的容器。源端会保留原有数据。

<picture><source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="docs/images/storage-zh-CN-tall-dark.svg"><source media="(max-width: 600px)" srcset="docs/images/storage-zh-CN-tall-light.svg"><source media="(prefers-color-scheme: dark)" srcset="docs/images/storage-zh-CN-dark.svg"><img alt="动画。两个镜像的大部分文件相同。按层存储时，只有完全相同的层才只存一次；镜像 B 的上层有一个文件不同，所以 spring.jar 和 jackson.jar 又存了一次：共 8 个文件。podbag 每个文件只存一次：共 6 个文件。8 个 Spring 服务、2 个版本的实测磁盘用量：按层（Docker）2,970 MiB，podbag 902 MiB。" src="docs/images/storage-zh-CN-light.svg"></picture>

## Podbag Desktop

在 Linux 和 macOS 上，可以管理容器、镜像、卷和网络。Windows 支持尚处于实验阶段，需要 WSL 2。可以在列表中启动、停止或重启容器，也可以打开容器查看实时日志。

![Podbag Desktop 示例列表显示 3 个运行中和 2 个已停止的容器，以及各自的镜像和已发布端口。](docs/images/desktop-containers-en.png)

参见 [Podbag Desktop 安装说明](#安装)或[桌面应用源码构建说明](docs/usage.md#build-the-desktop-app)。

## 安装

### Podbag Desktop（推荐）

从[最新版本](https://github.com/SteelCrab/podbag/releases/latest)下载适合当前系统的安装包。

- **Linux（x86_64、arm64）。** Debian 和 Ubuntu 使用 `.deb`，Fedora 使用 `.rpm`。安装包内含引擎。
- **macOS（Apple 芯片）。** 运行下方命令安装 CLI。解压 `podbag-desktop-aarch64-macos.zip`，然后打开 `podbag.app`。
- **Windows（x86_64、ARM64，实验性支持）。** 运行与 CPU 架构匹配的 `.msi`。如果发行版没有提供 MSI，可从成功的 [CI 运行记录](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml?query=branch%3Amain)下载。运行容器前请启用 WSL 2。

### Linux（x86_64、arm64）和 macOS（Apple 芯片）的 CLI

```sh
curl -fsSL https://podbag.dev/install.sh | sh
podbag --version
```

安装脚本会核对发行文件的 SHA-256 校验值，然后将 CLI 安装到 `/usr/local/bin`。更新时再次运行此脚本即可。

Linux 需要 5.12 或更高版本的内核以及 cgroup v2。运行 `podbag check` 可检查缺少的依赖。普通用户使用桥接网络时，请检查 `uidmap` 和 `passt` 软件包。[Linux 运行要求](docs/usage.md#requirements-on-linux)

手动安装时，请下载适合当前操作系统和 CPU 的文件，并用 `SHA256SUMS` 校验。[下载发行文件](https://github.com/SteelCrab/podbag/releases/latest)

### Windows 的 CLI（实验性支持）

在管理员 PowerShell 中运行 `wsl --install`，然后重启 Windows。根据 CPU 架构下载 `podbag-x86_64-windows.exe` 或 `podbag-aarch64-windows.exe`。将文件的 SHA-256 与 `SHA256SUMS` 核对后，将文件重命名为 `podbag.exe`。[下载发行文件](https://github.com/SteelCrab/podbag/releases/latest)

```powershell
.\podbag.exe run --rm alpine echo hello
```

### 其他安装方式

- **Linux 上的 Cargo 安装。** 运行 `cargo install --locked podbag` 可安装已发布的 CLI。
- **从源码构建。** 使用 Rust 构建 CLI 和 Podbag Desktop。 [构建指南](docs/build.md)

## 使用

### 运行容器

以下命令输出 `hello`，并在运行结束后删除容器。

```sh
podbag run --rm alpine echo hello
```

在 8080 端口启动 nginx，发送请求并查看访问日志。

```sh
podbag run -d --name web -p 8080:80 nginx:alpine
podbag ps
curl -I http://localhost:8080/
podbag logs web
podbag exec web nginx -v
podbag stop web && podbag rm web
```

### Docker CLI 和 Compose

Linux 普通用户（需已设置 `XDG_RUNTIME_DIR`）

```sh
podbag system service &
podbag_socket="unix://$XDG_RUNTIME_DIR/podbag/docker.sock"
```

macOS

```sh
podbag vm start
podbag_socket="unix://$HOME/.podbag/run/docker.sock"
```

设置套接字后，进入包含 `compose.yaml` 的项目目录并运行以下命令。此示例使用已发布的镜像。

```sh
docker --host "$podbag_socket" ps
docker --host "$podbag_socket" compose up -d
docker --host "$podbag_socket" compose down
```

### 从 Docker 复制镜像（`v0.2.0`）

启动两个引擎。如果 Docker 本地已有 `alpine:latest` 镜像，可以使用Podbag CLI v0.2.0 或更高版本 将它复制到 Podbag。

```sh
podbag migrate --from docker --to podbag image alpine:latest
```

[迁移指南](docs/usage.md#migrate-between-engines-or-kubernetes-clusters) · [命令参考](docs/usage.md)

## 基准测试

<img src="docs/images/perf.svg" alt="podbag 与 Docker、Podman、Kiri 的比较（一台 Linux 虚拟机）：启动容器 4.1 毫秒（Docker 73.8 毫秒）；100 个容器的内存 105 MiB（Docker 562 MiB）；没有容器时不运行任何进程（Docker 203 MiB）；8 个 Spring 服务、2 个版本的磁盘 902 MiB（Docker 2,970 MiB），页缓存 324 MiB（Docker 982 MiB）。" width="800">

<sub>同样的比较，视频版（英文）: [perf.mp4](docs/media/perf.mp4).</sub>

<table>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/start-dark.svg"><img alt="容器启动时间（中位数，毫秒）：podbag rootless 4.1，podbag root 4.4，Kiri 9.1，Podman 46.4，Docker 73.8" src="docs/images/start-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.svg"><img alt="100 个容器的内存（MiB）：podbag root 105，Kiri 109，podbag rootless 117，Podman 203，Docker 562" src="docs/images/memory-light.svg"></picture></td>
</tr>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/disk-dark.svg"><img alt="8 个 Spring 服务、2 个版本的磁盘（MiB）：podbag root 902，podbag rootless 944，Podman 1783，Kiri 2960，Docker 2970" src="docs/images/disk-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/cache-dark.svg"><img alt="16 个 Spring 服务的页缓存（MiB）：podbag rootless 324，podbag root 333，Kiri 960，Podman 975，Docker 982" src="docs/images/cache-light.svg"></picture></td>
</tr>
</table>

- 在一台 Linux 虚拟机上测得的中位数：4 个 CPU，3.8 GiB，arm64。
- 版本：podbag 0.1.0，Podman 5.7.0，Docker 29.1.3，Kiri 0.2.1。
- 所有数据和测量方法：[docs/benchmarks.md](docs/benchmarks.md)、[docs/benchmarks-msa.md](docs/benchmarks-msa.md)（英文）。
- 作为 k3s 节点的运行时运行 30 个 pod：podbag 只有 2 个进程，共 3.5 MiB；containerd 有 63 个（276 MiB），CRI-O 有 32 个（85 MiB）。[docs/benchmarks-k8s.md](docs/benchmarks-k8s.md)（英文）。

## 与其他工具比较

| | podbag | Docker | OrbStack | Colima | Kiri |
|---|---|---|---|---|---|
| 运行平台 | Linux、macOS、Windows¹ | Linux；macOS 和 Windows 用 Docker Desktop | macOS | macOS、Linux | Linux、macOS、Windows |
| 后台运行的进程 | 最后一个容器结束后无（Mac：2 MiB 的待机进程） | `dockerd`、`containerd`，每个容器一个 shim | 带 Docker Engine 的 Linux 虚拟机 | 带 Docker 或 containerd 的 Lima 虚拟机 | 一个守护进程；在 macOS 和 Windows 上还有一个小虚拟机 |
| 镜像存储 | 文件，每个只存一份 | 层 | 层 | 层 | 层，首次使用时解压 |
| Mac：`docker run` 启动，中位数² | 14.4 ms | 97.4 ms | 90.4 ms | 73.6 ms | 11.8 ms |
| Mac：最后一个容器结束 6 分钟后的内存² | 1.7 MiB | 950 MiB | 795 MiB | 1.54 GiB | 12 MiB |
| 许可证 | Apache-2.0 | Apache-2.0；Docker Desktop 对较大公司收费 | 专有 | MIT | Apache-2.0 |

¹ 实验阶段。² 在一台 Mac 上测得，Docker 为 Docker Desktop：[docs/benchmarks.md](docs/benchmarks.md)。

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/architecture-dark.svg"><img alt="Docker Engine：docker CLI 与 dockerd 通信，dockerd 与 containerd 通信，containerd 为每个容器启动一个 shim；镜像是层。podbag：命令自己创建每个容器，每个用户一个 keeper 在容器运行时看管它们；镜像是文件，每个文件只存一份。" src="docs/images/architecture-light.svg"></picture>

- 各部分如何配合，以及 podbag 目前的不足：[docs/architecture.md](docs/architecture.md)（英文）。

## 文档

以下文档为英文。

- [使用](docs/usage.md)：命令、卷、网络、限制、Docker API、TUI 和 GUI、macOS、Windows。
- [架构](docs/architecture.md)：podbag 与 Docker 的区别。
- [从源码构建](docs/build.md)：Linux、macOS、Windows。
- [基准测试](docs/benchmarks.md)和 [Spring 微服务基准测试](docs/benchmarks-msa.md)。

## 参与贡献

- 欢迎提交 issue 和 pull request。
- 请参阅 [CONTRIBUTING.md](CONTRIBUTING.md)（英文）。

## 许可证

[Apache License 2.0](LICENSE)
