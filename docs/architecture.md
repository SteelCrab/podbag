# How podbag works

This page compares podbag with Docker Engine, and with the tools that bring Docker to a
Mac. The numbers come from [benchmarks.md](benchmarks.md) and
[benchmarks-msa.md](benchmarks-msa.md).

<picture><source media="(prefers-color-scheme: dark)" srcset="images/architecture-dark.svg"><img alt="Docker Engine and podbag on Linux: processes and image storage" src="images/architecture-light.svg"></picture>

## Processes

**Docker Engine** is a client and two daemons. The `docker` command sends each request
over `/var/run/docker.sock` to `dockerd`, which passes it to `containerd` over gRPC.
`containerd` starts one `containerd-shim` process for each container, and the shim uses
`runc` to set the container up. Both daemons run all the time, with or without
containers.

**podbag** is one program, with no daemon. The `podbag` command is the engine: it sets up
each container itself (namespaces, cgroups, seccomp) and returns. One small keeper
process per user (about 2 MiB) writes the logs of the detached containers and answers
`stop`, `kill` and `wait` for them; it starts with the first container and leaves 10
seconds after the last one. A container in the foreground keeps a helper process of its
own while it runs. With no container, nothing runs.

For the `docker` CLI, Compose and buildx, `podbag system service` serves the Docker
Engine API on a socket. Containers do not depend on it, and it ends when no client has
connected for a minute; with systemd it can start on the first connection.

## Image storage: layers and files

An image is a list of layers, and a layer is a tar archive of the files it adds or
changes. **Layer stores** (Docker, Podman, OrbStack, Colima, Kiri) unpack each layer
into a directory of its own and keep it whole. When one file changes, or an image is
built again, the new layer is stored whole again; a file that is in two layers is kept,
and read into memory, twice. Docker and Kiri also keep the compressed layers.

**podbag** unpacks each layer into a store of files: a file is kept once for its
content, owner, mode and extended attributes, and an image is a list of paths that
point at those files. As root, podbag mounts an image with composefs (an EROFS image of
the directory tree over the shared files); without root, it builds a tree of hard
links. Two containers that read the same file read the same inode, so the kernel
caches it once.

Eight Spring services in two releases, where the second release was rebuilt on a newer
base with the same jars:

| | podbag | Podman | Docker | Kiri |
|---|---|---|---|---|
| Disk for both releases | 902–944 MiB | 1783 MiB | 2970 MiB | 2960 MiB |
| Page cache to start their 16 services | 324–333 MiB | 975 MiB | 982 MiB | 960 MiB |

Downloads stay the same: registries serve layers, and podbag downloads them whole before
it splits them into files.

## Networks

As root, podbag connects containers to a Linux bridge (`podbag0` for the default
network) and writes its firewall rules to nftables itself, over netlink. Without root,
each user's containers share a network namespace that `pasta` connects to the host.

## On macOS and Windows

Docker Desktop, OrbStack and Colima run Docker Engine, daemons included, in a Linux VM.
On a Mac, six minutes after the last container they still held 0.8 to 1.5 GiB of memory.

podbag runs a small VM of its own on a Mac (Virtualization.framework, Linux 6.18 with
podbag as the VM's first process). The first command starts it in about half a second,
and it powers off 60 seconds after the last container. A standby process (under 2 MiB)
keeps the docker socket open meanwhile and starts the VM for the next client. Six minutes
after the last container, podbag held 1.7 MiB.

On Windows, podbag.exe runs the Linux engine in a WSL 2 distribution of its own. This is
experimental: see [usage.md](usage.md#windows-experimental).

## Where podbag is behind

- Pulls: Kiri unpacks a layer when a container first uses it, so its pulls are faster.
  The pull and the first start together take about as long (6.9–7.4 s for podbag,
  7.5 s for Kiri, in the Spring benchmark).
- On a Mac, the first command after the VM powered off takes about 0.6 s (Kiri: 0.3 s),
  and a `docker run` takes 14.4 ms (Kiri: 11.8 ms).
- There is no CRI, so Kubernetes cannot use podbag as a node's runtime.
- The Docker API has no classic builder (use buildx), `commit`, `export`, `diff`, `top`,
  `stats`, `pause`, `rename`, `update`, `push`, `search`, swarm or plugins.
- Windows support has not yet been tried on a Windows machine.
