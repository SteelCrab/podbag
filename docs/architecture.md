# How podbag works

This page compares podbag with Docker Engine, and with the tools that bring Docker to a
Mac. The numbers come from [benchmarks.md](benchmarks.md) and
[benchmarks-msa.md](benchmarks-msa.md).

## Contents

- [Processes](#processes)
- [Image storage: layers and files](#image-storage-layers-and-files)
- [Networks](#networks)
- [On macOS and Windows](#on-macos-and-windows)
- [Where podbag is behind](#where-podbag-is-behind)

<picture><source media="(prefers-color-scheme: dark)" srcset="images/architecture-dark.svg"><img alt="Docker Engine and podbag on Linux: processes and image storage" src="images/architecture-light.svg"></picture>

## Processes

### Docker Engine

- Docker is a client and two daemons.
- The `docker` command sends each request to `dockerd`, over `/var/run/docker.sock`.
- `dockerd` passes it to `containerd`, over gRPC.
- `containerd` starts one `containerd-shim` process for each container.
- The shim uses `runc` to set the container up.
- Both daemons run all the time, with or without containers.

### podbag

- podbag is one program, with no daemon.
- The `podbag` command is the engine. It sets up each container itself (namespaces,
  cgroups, seccomp), and then returns.
- One small **keeper** per user (about 2 MiB) watches the containers in the background:
  - it writes their logs,
  - it answers `stop`, `kill` and `wait`,
  - it starts with the first container and stops 10 seconds after the last one.
- A container in the foreground has a small helper process of its own.
- With no container, nothing runs.

### The Docker API

- `podbag system service` serves the Docker Engine API on a socket, for the `docker` CLI,
  Compose and buildx.
- Containers do not depend on it.
- It stops after one minute without a client. With systemd, it can start on the first
  connection.

## Image storage: layers and files

<picture><source media="(prefers-color-scheme: dark)" srcset="images/storage-dark.svg"><img alt="Layers against files: as layers, the same jars rebuilt into a new layer are stored again; as files, the jars that two images share are kept once and both images point to them." src="images/storage-light.svg"></picture>

### Layers

- An image is a list of layers. A layer is a tar archive of the files it adds or changes.
- Layer stores (Docker, Podman, OrbStack, Colima, Kiri) unpack each layer into a folder of
  its own, and keep it whole.
- When one file changes, or an image is built again, the new layer is stored whole again.
- So a file that is in two layers is kept twice, and read into memory twice.
- Docker and Kiri also keep the compressed layers.

### Files

- podbag splits each layer into files, and keeps each file once. A file is the same when
  its content, owner, mode and extended attributes are the same.
- An image is a list of paths that point to these files.
- As root, podbag mounts an image with composefs: an EROFS image of the folder tree, over
  the shared files.
- Without root, it makes a tree of hard links.
- Two containers that read the same file read the same inode, so the kernel caches it once.

### Example

Eight Spring services in two releases. The second release was built again on a newer
base, with the same jars:

| | podbag | Podman | Docker | Kiri |
|---|---|---|---|---|
| Disk for both releases | 902–944 MiB | 1783 MiB | 2970 MiB | 2960 MiB |
| Page cache to start their 16 services | 324–333 MiB | 975 MiB | 982 MiB | 960 MiB |

- Downloads stay the same: registries serve layers, and podbag downloads them whole before
  it splits them into files.

## Networks

- As root:
  - containers connect to a Linux bridge (`podbag0` for the default network),
  - podbag writes its firewall rules to nftables itself, over netlink.
- Without root:
  - each user's containers share a network namespace,
  - `pasta` connects it to the host.

## On macOS and Windows

### Other tools on a Mac

- Docker Desktop, OrbStack and Colima run Docker Engine, daemons included, in a Linux VM.
- Six minutes after the last container, they still held 0.8 to 1.5 GiB of the Mac's
  memory.

### podbag on a Mac

- podbag runs a small VM of its own: Virtualization.framework, Linux 6.18, with podbag as
  the VM's first process.
- The first command starts the VM in about half a second.
- The VM powers off 60 seconds after the last container.
- A standby process (under 2 MiB) keeps the docker socket open, and starts the VM for the
  next client.
- Six minutes after the last container, podbag held 1.7 MiB.

### podbag on Windows

- `podbag.exe` runs the Linux engine in a WSL 2 distribution of its own.
- This is experimental: see [usage.md](usage.md#windows-experimental).

## Where podbag is behind

- **Pulls.** Kiri unpacks a layer only when a container first uses it, so its pulls are
  faster. A pull plus the first start take about the same time: 6.9–7.4 s for podbag and
  7.5 s for Kiri, in the Spring benchmark.
- **Waking up on a Mac.** The first command after the VM powered off takes about 0.6 s
  (Kiri: 0.3 s).
- **docker run on a Mac.** 14.4 ms (Kiri: 11.8 ms).
- **Kubernetes.** There is no CRI, so Kubernetes cannot use podbag as a node's runtime.
- **Docker API.** No classic builder (use buildx), `commit`, `export`, `diff`, `top`,
  `stats`, `pause`, `rename`, `update`, `push`, `search`, swarm or plugins.
- **Windows.** Not yet tried on a Windows machine.
