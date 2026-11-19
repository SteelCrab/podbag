# openpod

A daemonless, rootless container engine that runs Docker/OCI images from a single static binary.

## Status

Early development (milestones M0 to M10 and M8a to M8c of the plan). What works:

```sh
openpod run alpine echo hello            # pulls the image if needed; the container stays
openpod run --rm -it alpine sh           # interactive shell, removed on exit
openpod run -d --name web nginx:alpine   # in the background
openpod run -d --restart on-failure:3 myjob   # started again when it fails (no, always, …)
openpod run -m 512m --cpus 1.5 --pids-limit 200 alpine sh   # resource limits
openpod run -v ./src:/work:ro -v pgdata:/var/lib/pg --tmpfs /run alpine sh   # volumes
openpod volume create|ls|rm|inspect
openpod network create app                                    # networks
openpod run -d --name db --network app alpine sleep 1d
openpod run -d --name api --network app -p 8080:80 nginx:alpine
openpod exec api ping -c1 db                                  # by name
openpod ps -a
openpod logs -f web
openpod exec -it web sh
openpod stop web && openpod rm web
openpod pull alpine | openpod images | openpod rmi alpine
openpod save -o app.tar myapp:1 && openpod load -i app.tar     # image archives
openpod run --rm --privileged alpine mount -t tmpfs none /mnt   # every capability (rootless: in its user namespace)
openpod system df | openpod system prune -f
openpod run --rootfs ./rootfs sh         # a plain directory instead of an image
openpod check                            # is this system ready? (exit 1 if not)
openpod check-image myapp:1              # programs that cannot run in the image (exit 1)
openpod system service &                 # the Docker API, for the docker CLI (see below)
openpod vm status                        # macOS: the Linux VM openpod starts by itself (see below)
```

There is no daemon. While containers run, one small keeper process (`openpod __keeper`,
about 2 MiB) writes the logs of the detached ones and answers `stop`, `kill` and `wait`
for them, so a detached container costs no process of its own. The keeper starts with the
first container and leaves 10 seconds after the last one: with no containers, nothing
runs. A container in the foreground, and one that publishes ports, keeps a helper process
of its own while it runs. The keeper is the parent of the containers it watches: if it is
killed, they end with it. It serves only the user that started it, in a runtime directory
that nobody else can write to, and only the openpod program file it was started from:
after an upgrade, new containers get a helper of their own until the old keeper has
left. `keeper = false` in `config.toml` (or `OPENPOD_KEEPER=0`) gives
every container its own `openpod __shim` process instead; `keeper_linger = 10000`
(milliseconds) sets how long an idle keeper stays. Containers run in new mount, PID, UTS, IPC and
cgroup namespaces, plus a user namespace when run without root. Identical files in
different images are stored once and share page cache. The store keeps at most 10 GiB by
default.

Restart policies (`--restart no|always|unless-stopped|on-failure[:N]`, Docker's
`RestartPolicy`) need no daemon either: when a container ends, whoever cleaned up after it
starts a small `openpod __restart` process that waits (100 ms, then twice as long each
time, at most a minute; 100 ms again after a run of 10 seconds) and starts the container
again. `stop` and `kill` end the restarts until the next `start`; `--rm` does not go with a
policy. Nothing starts containers when the machine starts, so `always` and
`unless-stopped` behave the same; `inspect` shows the restarts since the last `start`.

Health checks (`--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`,
`--health-start-period`, `--health-start-interval`, Docker's `Healthcheck`, and an image's
`HEALTHCHECK`) cost no process between checks either: when one is due, the keeper (or the
container's own helper) starts `openpod __health`, which runs the check in the container,
keeps the result with the last five checks, and ends. A check that runs past its timeout
is ended. `inspect` and `ps` show the health (`starting`, `healthy`, `unhealthy`);
`--no-healthcheck` turns an image's check off. Images pulled with an earlier openpod have
their check after the next pull.

Not yet: macOS and Windows.

## Image storage

Every distinct file is stored once, whatever image it comes from. A container sees its
image in one of two ways; the files, owners, modes, extended attributes and hard links it
sees are the same:

- **composefs** (root, Linux 6.5 or later): each image has a small metadata file (EROFS)
  that names the stored files. openpod mounts it once per image and every container of
  the image shares that mount; overlayfs reads file contents from the store. Pulls do not
  unpack a directory tree.
- **Hard-link trees** (rootless, or older kernels): each image is unpacked once as a
  directory of hard links to the stored files.

As root, openpod tries composefs once per kernel and remembers the result; `openpod check`
shows the method in use and, when composefs is not available, why. Set
`image_mount = "hardlink"` in `config.toml` (or `OPENPOD_IMAGE_MOUNT=hardlink`) to use
hard-link trees as root too. When the method changes, an image gets what it needs the first
time a container uses it. Removing an image unmounts it.

## Checks

`openpod check` lists what openpod needs on this system, one line each: the kernel,
cgroup v2, how images are mounted (as root), whether the keeper is in use and, without root,
`newuidmap`/`newgidmap`, subordinate IDs, `pasta`, the systemd
user session, AppArmor, linger and the lowest port a user may publish. Each problem comes
with the command that fixes it. As root, it warns when Docker's firewall blocks bridge
networks. `openpod check --apparmor-profile` prints the profile described under "AppArmor".

`openpod check-image IMAGE` reads every program and library in a pulled image (it never
runs them) and reports the ones that cannot run there: built for another C library (a
glibc program in a musl image such as Alpine, or the reverse), built for another CPU, or
needing a library the image does not have (a warning). It exits with 1 when it finds an
error, for use in CI.

When a container's program cannot start for one of these reasons, `run` and `exec` say
which, instead of "no such file": for example, that the program needs
`/lib/ld-linux-aarch64.so.1` (glibc) and the image has no such file, or that a script's
first line names an interpreter the image lacks.

Measurements are in [docs/benchmarks.md](docs/benchmarks.md): against Podman, Docker and Kiri on
one Linux host, and against Docker Desktop, OrbStack, Kiri, Podman and Colima on a Mac.

## Volumes

| Option | Effect |
|---|---|
| `-v /host/dir:/dir[:ro]`, `-v ./dir:/dir` | a host path; created if missing, like `docker run -v` |
| `-v name:/dir[:ro][,nocopy]` | a named volume in `<data>/volumes/<name>/data`, created on first use |
| `-v /dir` | an anonymous volume: removed with the container by `--rm` or `rm -v` |
| `--tmpfs /dir[:size=64m,mode=1777,…]` | a memory file system (`nosuid,nodev,noexec` unless asked) |

An empty volume is filled with the image's files at its target the first time it is
mounted (owners and modes kept), unless `nocopy` is given. An image's `Volumes` become
anonymous volumes. `openpod volume rm` refuses a volume that any container still names.
Mount targets are resolved inside the container's root, so a symbolic link in the image
cannot send a mount outside it; `/proc` and `/sys` cannot hold mounts.

## Limits and security

| Option | Effect |
|---|---|
| `-m, --memory 512m` | `memory.max` (and as much swap); at least 6 MiB |
| `--cpus 0.5` | `cpu.max`; at least 0.01 |
| `--pids-limit 100` | `pids.max`; 0 or less is no limit |
| `--cap-add NAME`, `--cap-drop NAME` | change Docker's 14 default capabilities (`ALL` allowed; drops first) |
| `--security-opt no-new-privileges` | setuid programs cannot gain privileges (off by default, like Docker) |
| `--security-opt seccomp=unconfined` | turn off the default seccomp filter |
| `--privileged` | every capability, no seccomp filter, the kernel's files unmasked, a writable `/sys` and cgroup, any device (for BuildKit and other container tools); host devices are not added to `/dev` |

**Memory budget.** All containers run in one parent cgroup. Its `memory.max` is the
budget: by default 50% of the host's memory (`memory_budget` in `config.toml`, such as
`"8GiB"`, `"25%"`, or `"0"` for no budget; `OPENPOD_MEMORY_BUDGET` overrides it). Above
90% of the budget the kernel reclaims cache first; above the budget it ends one container.
A container ended for memory shows why in `ps` and `inspect`:
`Exited (137, out of memory: memory budget)` or `… container limit`. The kernel may
rarely end a second container when the first one's memory is not given back yet.

**Where the cgroups are.** As root: `/sys/fs/cgroup/openpod/<id>`. Without root: the
keeper runs in one transient systemd scope in the user's `openpod.slice` (started with
`systemd-run --user --scope`) and makes each container's cgroup below it, `<scope>/c/<id>`,
so a start needs no `systemd-run` of its own. Without the keeper every container gets a
scope of its own, `openpod-<id>.scope`. `exec` runs commands in the same cgroup.

**Security defaults.** Docker's 14 capabilities, a seccomp filter that refuses dangerous
system calls (module loading, kexec, mount, new namespaces, keyrings, BPF, …), hidden
kernel files (`/proc/kcore`, `/proc/keys`, `/sys/firmware`, …) and read-only kernel
settings (`/proc/sys`, `/proc/sysrq-trigger`, …). A container may make device nodes, as
with Docker, but open only the usual ones (`/dev/null`, `zero`, `full`, `random`,
`urandom`, `tty`, `ptmx`, terminals, `net/tun`): as root these are runc's rules, in a cgroup
BPF program; without root the user namespace refuses device nodes anyway. `exec` applies
the same.
`--privileged` drops all of these. Without root it still gives nothing the user does not
have: the capabilities hold inside the container's user namespace, the host's kernel
settings stay out of reach, and no host device is passed in. As root it is the host's root.

## Networks

| Option | Effect |
|---|---|
| `--network bridge` (default) | the default network: bridge `openpod0`, `10.77.0.0/16` (`default_subnet` in `config.toml`) |
| `--network NAME` | a network from `openpod network create`; containers on it find each other by name |
| `--network A --network B` | further bridge networks too (`eth1`, …); the default route stays on the first |
| `--network-alias NAME` | a further name on the (first) user network |
| `--network host` | the host's network |
| `--network none` | loopback only |
| `-p [IP:][HOST:]CONTAINER[/tcp\|udp]` | publish a port; without a host port the kernel picks one (`ps`, `inspect`) |

New networks get `10.78.<n>.0/24` unless `--subnet` says otherwise; a subnet that overlaps
another network or a host route is refused. Names on a network (container names and
aliases) are written into each container's `/etc/hosts`, which openpod rewrites as
containers start and stop; when two containers share an alias, the first line answers
(there is no DNS server to rotate them). `openpod network connect [--alias NAME] NET
CONTAINER` and `network disconnect NET CONTAINER` change a stopped container's further
networks for its next start; a running container is refused.

As root, bridges live in the host's network namespace with an nftables table `openpod` for
NAT (no other table is touched), and IPv4 forwarding is turned on. Without root, one
session process per user holds a user and a network namespace; the bridges live there and
`pasta` connects them to the host (DNS through pasta at `169.254.1.1`). The first container
starts the session and the last one stops it. Rootless bridge containers share the
session's user namespace. Published ports are relayed by each container's shim in both
modes, so the container sees the gateway as the client address.

## Docker API

`openpod system service` serves the Docker Engine API (version 1.47; clients down to 1.24
work) on a Unix socket, so the `docker` CLI can drive openpod. It is not a daemon in the
usual sense: containers do not depend on it, and it ends after 60 seconds without a
connection (`--timeout`, 0 for never).

```sh
openpod system service &                                   # rootless
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/openpod/docker.sock   # root: /run/openpod/docker.sock
docker run --rm -it alpine sh
docker context create openpod --docker host=$DOCKER_HOST   # or keep it as a context
```

With systemd, the service can start on the first connection instead:
`openpod system service --print-units` prints a socket and a service unit (for
`~/.config/systemd/user/`, or `/etc/systemd/system/` as root); enable the socket unit.
The socket is private to its user (0600; 0660 as root).

What works: `version`, `info`, `system df`, `events` (live: there is no history, so
`--since` in the past shows nothing older); `pull`, `images`, `image inspect`, `tag`, `rmi`,
`image prune`; `run` (attached, `-d`, `-i`, `-t`, `--rm`), `create`, `start [-a]`, `stop`,
`restart`, `kill`, `wait`, `rm`, `logs` (`-f`, `--tail`, `-t`, `--since`), `attach`, `ps`
(with `label`, `name`, `id`, `status` filters), `inspect`, `port`, `exec` (`-i`, `-t`, `-d`,
`-e`, `-w`, `-u`), `cp` (both ways, running or not), `container prune`; `save` and `load`;
`volume` and `network` (create, ls, inspect, rm, prune).

Settings openpod does not have are refused with a reason (devices, `--read-only`,
`--pid host`, `--volumes-from`, `--link`, port ranges, other network or volume drivers),
or accepted with a warning when ignoring them is harmless (`--dns`, `--add-host`,
ulimits, log drivers, fixed addresses).
Features it lacks answer 501 with what to do instead: the classic builder (`POST /build`;
use buildx, below), `commit`, `export`, `diff`, `top`, `stats`, `pause`, `rename`,
`update`, `push`, `search`, swarm and plugins. `network connect` and `disconnect` work on
containers that do not run.

`docker cp` works on running and stopped containers. In a running one it goes through
the container's own mounts; in a stopped one openpod mounts its files (root filesystem,
volumes, bind mounts) privately for the copy, and no start begins meanwhile. Paths are
the container's: a symbolic link to `/etc` means the container's `/etc`, never the
host's. The kernel's file systems (`/proc`, `/sys`) come out empty, as with Docker; a copy
that fails partway is an error. `docker save` writes Docker's archive format; the store keeps files, not the
original layer archives, so a saved image's layers (and its ID) get new digests, and it
carries the image's command, environment, user, working directory, volumes and health
check, but not its labels or history. `docker load` reads `docker save` archives and OCI
image layouts (both, as BuildKit writes them), plain or gzip-compressed; every digest in
the archive is checked, and an archive larger than the store limit is refused. An image
whose name moves to a newly loaded one is removed unless a container uses it; images
that came without a name are found by ID (the config's digest too, which buildx prints)
and removed by `docker image prune`.

Differences from Docker: `docker attach` to a container that another process started
reads its output from the log and cannot send input; a client that leaves an attached
`exec` ends its command (Docker leaves it running). Volumes and networks keep their
labels (`--label` on `network create` and `volume create`, and the `label` filters of
`ls` and `prune`). `network connect` changes a stopped container only.

Docker Compose works through the same socket: `docker compose up` (again without making
anything new when nothing changed), `down` (`-v`), `ps`, `logs`, `exec`, `run`, with the
project's networks and volumes, several networks per service, services that reach each
other by service and container name, `depends_on` with `condition: service_healthy`,
restart policies and health checks. Names live in each container's `/etc/hosts` (there is
no DNS server): when replicas share a name, the first one answers. `docker compose build`
builds with a buildx builder (`--builder`, below).

**Building images with buildx.** buildx's `docker-container` driver runs BuildKit in an
openpod container (privileged, so it can run each step in containers of its own) and
loads the result through `docker load`:

```sh
docker buildx create --driver docker-container --name openpod --use
docker buildx build --load -t myapp .          # then: docker run myapp
docker compose build --builder openpod
```

The builder keeps running after the service ends (it is an ordinary container), and its
cache stays in its volume until `docker buildx rm`. The default `docker` driver (the
daemon's built-in BuildKit) is not available.

## macOS

On a Mac, openpod runs containers in a small Linux VM of its own (Virtualization.framework,
Linux 6.18). The first command starts it (about one second); it powers off 60 seconds
after the last connection ends while no container runs or waits to restart
(`OPENPOD_VM_IDLE`, seconds), so nothing runs while no container does. A command that comes
while the VM stops waits for the next one. A running VM answers `openpod run --rm alpine
true` in about 30 ms.

```sh
dev/vm.sh run dev/macos/build-guest.sh     # the guest: kernel, initramfs, empty data disk
cargo build && dev/macos/sign.sh           # VMs need the virtualization entitlement
target/debug/openpod run --rm -it alpine sh
target/debug/openpod vm status             # also: vm start, vm stop, vm logs
```

Commands run in the VM: the Mac's `openpod` passes them on with their output, exit code,
signals and terminal size, as a local one would run. Only commands that read input
(`-i`, `load` without `-i FILE`) take the Mac's stdin, and only `-i` on a terminal puts it
in raw mode; a closed terminal is a hangup for the command, as locally. `-e NAME` takes the
Mac's value of NAME. The Mac's `/Users` appears in the VM at the same path, so
`-v ./src:/work` works from a directory under `/Users`; other Mac paths (also through `..`
or a link, and files for `save -o`, `load -i`, `--rootfs`) are refused, and named volumes
live in the VM. Of the `OPENPOD_*` settings, the keeper's, the store's and the default
subnet pass to the VM; paths and programs stay the Mac's. Containers cannot reach the VM's
agent (vsock is closed to them). Images, containers and volumes stay on a sparse 64 GiB
disk, `~/.openpod/disk.img`, between VM runs; `openpod vm stop` and a logout power the VM
off cleanly, and when the disk cannot be mounted, commands say so instead of using memory.
The VM uses 2 GiB of memory (`OPENPOD_VM_MEMORY`, MiB); containers get 1048576 open files
and `vm.max_map_count` 262144, like Docker Desktop's.

The docker CLI, Compose and buildx use openpod through its Docker API socket:

```sh
export DOCKER_HOST=unix://$HOME/.openpod/run/docker.sock
docker run --rm alpine echo hi
docker compose up -d
docker buildx create --use --driver docker-container
docker buildx build --load -t app .
```

The socket stays while the VM is off: a standby of about 8 MiB waits on it, and the first
request starts the VM. It appears with the first openpod command (`openpod vm start`) and
goes with `openpod vm stop`. Bind mounts must come from under `/Users` (`mounts denied`
otherwise); named volumes live in the VM. Each docker request after a pause brings the
VM's clock to the Mac's. Still to come: published ports on the Mac's localhost, and memory
and disk space given back while the VM runs.

## Requirements

- Linux 5.12 or later, with cgroup v2 at `/sys/fs/cgroup`. composefs (root only) needs
  6.5 or later with EROFS; older kernels use hard-link trees.
- Without root: `newuidmap`/`newgidmap` and entries in `/etc/subuid` and `/etc/subgid`;
  `pasta` (package `passt`) for bridge networks (or use `--network host`/`none`);
  on Ubuntu 24.04 and later an AppArmor profile (see below); for resource limits and the
  memory budget, a systemd user session. Without one, containers still run, with a
  warning, but `--memory`, `--cpus` and `--pids-limit` are refused.

## Development

openpod needs Linux kernel features. On macOS, all Linux builds and tests run in a lima VM.

```sh
dev/vm.sh up          # create and provision the openpod-dev VM (Ubuntu 26.04)
dev/vm.sh test        # cargo test inside the VM, rootless
dev/vm.sh test-root   # cargo test inside the VM, as root (plus library tests that need root)
dev/vm.sh static      # build the static musl release binary and check its size
dev/vm.sh shell       # open a shell in the VM
```

Unit tests that do not need Linux also run on the host with `cargo test`.

## AppArmor

Ubuntu 24.04 and later set `kernel.apparmor_restrict_unprivileged_userns=1`.
With this setting, a program without an AppArmor profile cannot use the user
namespaces that rootless containers need. openpod then stops with
"cannot create an unprivileged user namespace". Install a profile once.

1. Find the full path of openpod: `command -v openpod` (for example `/usr/local/bin/openpod`,
   or `/home/you/.cargo/bin/openpod` after `cargo install`).
2. Save this as `openpod.apparmor`, with `/usr/local/bin/openpod` replaced by that path:

   ```
   abi <abi/4.0>,
   include <tunables/global>

   profile openpod /usr/local/bin/openpod flags=(unconfined) {
     userns,
   }
   ```

3. Load it:

   ```sh
   sudo install -m 0644 openpod.apparmor /etc/apparmor.d/openpod
   sudo apparmor_parser -r /etc/apparmor.d/openpod
   ```

`openpod check --apparmor-profile > openpod.apparmor` writes the same file with the path
already filled in.

If you move openpod to another path, update the profile.

After this one-time step, openpod runs without sudo.

