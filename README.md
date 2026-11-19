# podbag

A container engine with no daemon, in one static binary. It runs Docker and OCI images,
rootless or as root, and stores them file by file: a file that many images share is kept
on disk, and read into memory, once.

[![CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml/badge.svg)](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SteelCrab/podbag)](https://github.com/SteelCrab/podbag/releases/latest)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

<img src="docs/images/intro.svg" alt="podbag intro: a container starts in about 4 ms on Linux and nothing runs once it ends; images stored file by file take 902 MiB of disk against Docker's 2,970 MiB; Linux, macOS and Windows" width="800">

<sub>The same intro as a video: [intro.mp4](docs/media/intro.mp4).</sub>

- **Linux** (x86_64, arm64): runs containers itself. With no container, nothing runs.
- **macOS** (Apple silicon): starts a small Linux VM of its own when it is needed.
- **Windows** (x86_64, arm64): runs in a WSL 2 distribution of its own. *Experimental.*
- Works with the `docker` CLI, Docker Compose and buildx through its Docker API.

## Install

Linux:

```sh
curl -fsSLo podbag https://github.com/SteelCrab/podbag/releases/latest/download/podbag-$(uname -m)-linux
sudo install -m 0755 podbag /usr/local/bin/podbag
podbag check    # what this system still needs, and how to add it
```

Without root, podbag needs `newuidmap` (package `uidmap`), `pasta` (package `passt`) for
networks, and on Ubuntu 24.04 or later an AppArmor profile. `podbag check` says which.

macOS (Apple silicon):

```sh
curl -fsSLo podbag https://github.com/SteelCrab/podbag/releases/latest/download/podbag-aarch64-macos
sudo mkdir -p /usr/local/bin && sudo install -m 0755 podbag /usr/local/bin/podbag
```

Windows (experimental, needs WSL 2: `wsl --install`):

```powershell
curl.exe -fsSLo podbag.exe https://github.com/SteelCrab/podbag/releases/latest/download/podbag-x86_64-windows.exe
.\podbag.exe run --rm alpine echo hello
```

On arm64, download `podbag-aarch64-windows.exe`. Each release lists the SHA-256 sums of its
files.

## Use

```sh
podbag run --rm alpine echo hello                 # pulls the image if needed
podbag run -d --name web -p 8080:80 nginx:alpine  # in the background
podbag ps
podbag logs -f web
podbag exec -it web sh
podbag stop web && podbag rm web
podbag images
```

The `docker` CLI, Compose and buildx work through podbag's Docker API:

```sh
podbag system service &      # Linux (on macOS: podbag vm start)
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podbag/docker.sock   # macOS: unix://$HOME/.podbag/run/docker.sock
docker compose up -d
```

Volumes, networks, limits, and the details for macOS and Windows:
[docs/usage.md](docs/usage.md).

## Benchmarks

<table>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/start-dark.svg"><img alt="Start a container, median ms: podbag rootless 4.1, podbag root 4.4, Kiri 9.1, Podman 46.4, Docker 73.8" src="docs/images/start-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.svg"><img alt="Memory for 100 containers, MiB: podbag root 105, Kiri 109, podbag rootless 117, Podman 203, Docker 562" src="docs/images/memory-light.svg"></picture></td>
</tr>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/disk-dark.svg"><img alt="Disk for 8 Spring services in 2 releases, MiB: podbag root 902, podbag rootless 944, Podman 1783, Kiri 2960, Docker 2970" src="docs/images/disk-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/cache-dark.svg"><img alt="Page cache of 16 Spring services, MiB: podbag rootless 324, podbag root 333, Kiri 960, Podman 975, Docker 982" src="docs/images/cache-light.svg"></picture></td>
</tr>
</table>

Medians on one Linux VM (4 CPUs, 3.8 GiB, arm64) with podbag 0.1.0, Podman 5.7.0,
Docker 29.1.3 and Kiri 0.2.1. How each was measured, and every number:
[docs/benchmarks.md](docs/benchmarks.md) and [docs/benchmarks-msa.md](docs/benchmarks-msa.md).

## Compared with other tools

| | podbag | Docker | OrbStack | Colima | Kiri |
|---|---|---|---|---|---|
| Runs on | Linux, macOS, Windows¹ | Linux; macOS and Windows with Docker Desktop | macOS | macOS, Linux | Linux, macOS, Windows |
| Runs in the background | nothing once the last container ends (Mac: a 2 MiB standby) | `dockerd`, `containerd`, a shim per container | a Linux VM with Docker Engine | a Lima VM with Docker or containerd | a daemon; on macOS and Windows a small VM |
| Image storage | files, each kept once | layers | layers | layers | layers, unpacked when first used |
| Mac: `docker run` start, median² | 14.4 ms | 97.4 ms | 90.4 ms | 73.6 ms | 11.8 ms |
| Mac: memory 6 minutes after the last container² | 1.7 MiB | 950 MiB | 795 MiB | 1.54 GiB | 12 MiB |
| License | Apache-2.0 | Apache-2.0; Docker Desktop is paid for larger companies | proprietary | MIT | Apache-2.0 |

¹ Experimental. ² Measured on one Mac with Docker Desktop for Docker:
[docs/benchmarks.md](docs/benchmarks.md).

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/architecture-dark.svg"><img alt="Docker Engine: the docker CLI talks to dockerd, which talks to containerd, which starts a shim per container; images are layers. podbag: the command creates each container itself, a keeper per user watches them while they run; images are files, each kept once." src="docs/images/architecture-light.svg"></picture>

How the parts fit, and where podbag is still behind: [docs/architecture.md](docs/architecture.md).

## Contributing

Issues and pull requests are welcome. See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

[Apache License 2.0](LICENSE)
