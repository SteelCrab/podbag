# openpod

A daemonless, rootless container engine that runs Docker/OCI images from a single static binary.

## Status

Early development (milestone M7). What works:

```sh
openpod run alpine echo hello            # pulls the image if needed; the container stays
openpod run --rm -it alpine sh           # interactive shell, removed on exit
openpod run -d --name web nginx:alpine   # in the background
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
openpod system df | openpod system prune -f
openpod run --rootfs ./rootfs sh         # a plain directory instead of an image
openpod check                            # is this system ready? (exit 1 if not)
openpod check-image myapp:1              # programs that cannot run in the image (exit 1)
```

There is no daemon: each container has a small `openpod __shim` process that keeps its
logs and answers `stop`, `kill` and `wait`. Containers run in new mount, PID, UTS, IPC and
cgroup namespaces, plus a user namespace when run without root. Identical files in
different images are stored once and share page cache. The store keeps at most 10 GiB by
default.

Not yet: macOS and Windows.

## Checks

`openpod check` lists what openpod needs on this system, one line each: the kernel,
cgroup v2 and, without root, `newuidmap`/`newgidmap`, subordinate IDs, `pasta`, the systemd
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

Measurements against Docker and Podman are in [docs/benchmarks.md](docs/benchmarks.md).

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

**Memory budget.** All containers run in one parent cgroup. Its `memory.max` is the
budget: by default 50% of the host's memory (`memory_budget` in `config.toml`, such as
`"8GiB"`, `"25%"`, or `"0"` for no budget; `OPENPOD_MEMORY_BUDGET` overrides it). Above
90% of the budget the kernel reclaims cache first; above the budget it ends one container.
A container ended for memory shows why in `ps` and `inspect`:
`Exited (137, out of memory: memory budget)` or `… container limit`. The kernel may
rarely end a second container when the first one's memory is not given back yet.

**Where the cgroups are.** As root: `/sys/fs/cgroup/openpod/<id>`. Without root: a
transient systemd scope `openpod-<id>.scope` in the user's `openpod.slice`, started with
`systemd-run --user --scope`. `exec` runs commands in the same cgroup.

**Security defaults.** Docker's 14 capabilities, a seccomp filter that refuses dangerous
system calls (module loading, kexec, mount, new namespaces, keyrings, BPF, …), hidden
kernel files (`/proc/kcore`, `/proc/keys`, `/sys/firmware`, …) and read-only kernel
settings (`/proc/sys`, `/proc/sysrq-trigger`, …). `exec` applies the same.

## Networks

| Option | Effect |
|---|---|
| `--network bridge` (default) | the default network: bridge `openpod0`, `10.77.0.0/16` (`default_subnet` in `config.toml`) |
| `--network NAME` | a network from `openpod network create`; containers on it find each other by name |
| `--network host` | the host's network |
| `--network none` | loopback only |
| `-p [IP:][HOST:]CONTAINER[/tcp\|udp]` | publish a port; without a host port the kernel picks one (`ps`, `inspect`) |

New networks get `10.78.<n>.0/24` unless `--subnet` says otherwise; a subnet that overlaps
another network or a host route is refused. Names on a network are written into each
container's `/etc/hosts`, which openpod rewrites as containers start and stop.

As root, bridges live in the host's network namespace with an nftables table `openpod` for
NAT (no other table is touched), and IPv4 forwarding is turned on. Without root, one
session process per user holds a user and a network namespace; the bridges live there and
`pasta` connects them to the host (DNS through pasta at `169.254.1.1`). The first container
starts the session and the last one stops it. Rootless bridge containers share the
session's user namespace. Published ports are relayed by each container's shim in both
modes, so the container sees the gateway as the client address.

## Requirements

- Linux 5.12 or later, with cgroup v2 at `/sys/fs/cgroup`.
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
dev/vm.sh test-root   # cargo test inside the VM, as root
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

