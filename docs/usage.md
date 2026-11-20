# Using podbag

The [README](../README.md) shows how to install podbag. This page shows what it can do.

## Contents

- [Commands](#commands)
- [How containers run](#how-containers-run)
- [Images](#images)
- [Builds](#builds)
- [Checks](#checks)
- [Volumes](#volumes)
- [Limits and security](#limits-and-security)
- [Networks](#networks)
- [Docker API](#docker-api)
- [TUI and GUI](#tui-and-gui)
- [macOS](#macos)
- [Windows (experimental)](#windows-experimental)
- [Update podbag](#update-podbag)
- [Requirements on Linux](#requirements-on-linux)
- [AppArmor](#apparmor)

## Commands

```sh
podbag run alpine echo hello                 # pull the image if needed; the container stays
podbag run --rm -it alpine sh                # an interactive shell, removed when it ends
podbag run -d --name web nginx:alpine        # in the background
podbag run -d --restart on-failure:3 myjob   # start again when it fails
podbag run -m 512m --cpus 1.5 --pids-limit 200 alpine sh                  # limits
podbag run -v ./src:/work:ro -v pgdata:/var/lib/pg --tmpfs /run alpine sh # volumes
podbag volume create|ls|rm|inspect
podbag network create app                    # a network
podbag run -d --name db --network app alpine sleep 1d
podbag run -d --name api --network app -p 8080:80 nginx:alpine
podbag exec api ping -c1 db                  # containers find each other by name
podbag ps -a
podbag logs -f web
podbag exec -it web sh
podbag stop web && podbag rm web
podbag pull alpine | podbag images | podbag rmi alpine
podbag save -o app.tar myapp:1 && podbag load -i app.tar   # image archives
podbag build -t myapp:1 .                    # build an image from ./Dockerfile
podbag commit web web-snapshot:1             # a container's changes as an image
podbag diff web                              # the files a container changed
podbag export -o web.tar web                 # all its files as a tar
podbag run --rm --privileged alpine mount -t tmpfs none /mnt
podbag system df | podbag system prune -f
podbag system verify                         # check every stored file (exit code 1 if one is damaged)
podbag run --rootfs ./rootfs sh              # a plain folder instead of an image
podbag check                                 # is this system ready? (exit code 1 if not)
podbag check-image myapp:1                   # programs that cannot run in the image
podbag system service &                      # the Docker API, for the docker CLI
podbag vm status                             # macOS and Windows: podbag's Linux VM
podbag tui                                   # containers, images, volumes and networks
```

## How containers run

### No daemon

- podbag has no daemon.
- One small **keeper** process (about 2 MiB) watches the containers in the background:
  - it writes their logs,
  - it answers `stop`, `kill` and `wait` for them.
- The keeper starts with the first container. It stops 10 seconds after the last one.
- With no container, nothing runs.
- A container in the foreground, or one that publishes ports, has a small helper process
  of its own while it runs.

### The keeper

- The keeper is the parent of its containers. If it is killed, they stop too.
- It serves only the user who started it.
- After an upgrade, new containers get their own helper until the old keeper stops.
- Turn it off with `keeper = false` in `config.toml`, or `PODBAG_KEEPER=0`. Then each
  container gets its own `podbag __shim` process.
- `keeper_linger = 10000` (milliseconds) sets how long an idle keeper stays.

### Isolation

- Each container gets new mount, PID, UTS, IPC and cgroup namespaces.
- Without root, it also gets a user namespace.

### Restart policies

- Use `--restart no|always|unless-stopped|on-failure[:N]`.
- No daemon is needed. When a container stops, a small `podbag __restart` process starts
  it again.
- The wait before a restart is 100 ms, then twice as long each time, up to 1 minute.
  After a run of 10 seconds, it is 100 ms again.
- `stop` and `kill` end the restarts until the next `start`.
- `--rm` does not go with a restart policy.
- Nothing starts containers when the machine starts, so `always` and `unless-stopped`
  work the same.
- `inspect` shows the restarts since the last `start`.

### Health checks

- Use `--health-cmd`, `--health-interval`, `--health-timeout`, `--health-retries`,
  `--health-start-period` and `--health-start-interval`. An image's `HEALTHCHECK` works too.
- No process runs between checks. When a check is due, the keeper (or the container's
  helper) starts `podbag __health`.
- `podbag __health` runs the check, keeps the last five results, and stops.
- A check that runs past its timeout is stopped.
- `ps` and `inspect` show the health: `starting`, `healthy` or `unhealthy`.
- `--no-healthcheck` turns the image's check off.
- Images pulled with an older podbag get their check at the next pull.

## Images

### Storage

- Each distinct file is stored once, whatever image it comes from.
- Identical files in different images share the page cache (memory). In hard-link trees
  (without root) they must have the same time too: see [File times and links](#file-times-and-links).
- The store keeps at most 10 GiB by default.

### How a container sees its image

- **composefs** (as root, Linux 6.5 or later):
  - each image has a small metadata file (EROFS) that lists its files,
  - podbag mounts it once; all containers of the image share that mount,
  - a pull does not unpack a folder tree.
- **Hard-link trees** (without root, or on older kernels):
  - each image is unpacked once, as a folder of hard links to the stored files.
- Both ways show the same files, owners, modes, times, extended attributes and hard links.

### File times and links

- Files keep the times they have in the image:
  - Python's `.pyc` files stay valid, so containers do not compile them again,
  - nginx sends the same `ETag` and `Last-Modified` headers as with Docker.
- In a hard-link tree, files share one inode only when their times match too. A file
  with the contents of another but a different time gets a copy of its own. A rebuilt
  image whose files got new times shares less with the old one.
- Crontabs (`/etc/crontab`, and everything in `/etc/cron.d` and `/var/spool/cron`) get
  an inode of their own: cron skips a crontab that has more than one link.
- Images stored by podbag 0.1.0 keep the times and links they had. To fix them, remove
  them and pull or load them again.

### Damaged files

- podbag writes new files to disk before it records them. A crash cannot leave an image
  whose files never reached the disk.
- A stored file that a crash cut short is written again by the next pull or load that
  reads a layer with that file. All images that share the file get it back.
- `podbag system verify` reads every stored file and checks its hash:
  - it names the images that use a damaged file, and exits with 1,
  - `--fix` removes those images (not the ones a container uses); then pull or load
    them again.

### Choose the method

- As root, podbag tests composefs once per kernel and remembers the result.
- `podbag check` shows the method in use, and why composefs is not available.
- To use hard-link trees as root too: `image_mount = "hardlink"` in `config.toml`, or
  `PODBAG_IMAGE_MOUNT=hardlink`.
- When the method changes, an image gets what it needs the first time a container uses it.
- Removing an image unmounts it.

## Builds

### Build an image

```sh
podbag build -t myapp:1 .                          # ./Dockerfile, with . as the context
podbag build -f app.Dockerfile --build-arg VERSION=2 --target app -t myapp .
tar -c . | podbag build -t myapp -                 # the context as a tar on stdin
podbag build -t myapp - < Dockerfile               # only a Dockerfile, no context
```

- No BuildKit and no daemon: podbag reads the Dockerfile and does each step itself.
- Options: `-t` (more than once), `-f`, `--build-arg`, `--target`, `--no-cache`, `--pull`,
  `--label`, `--network`, `-q`.

### How the steps work

- `RUN` runs in a podbag container on the image so far, with the build arguments, `ENV`,
  `USER`, `WORKDIR` and `SHELL` of the stage.
  - The network is the default bridge; `--network host|none|<name>` changes it.
  - The files it changed become a new layer. Deleted files stay deleted in the image.
- `COPY` and `ADD` put the files in a new layer, owned by root unless `--chown` says
  otherwise. `--chmod` sets the mode.
  - `ADD` unpacks a local tar archive (plain, gzip or zstd) and downloads a URL.
- `WORKDIR` makes the folder when it is missing.
- Steps that change only settings (`ENV`, `CMD`, `LABEL`, `EXPOSE`, …) make no layer.
- The new layers go into the store like pulled ones: each file is kept once.

### Dockerfiles

- All the classic instructions: `FROM` (`AS`), `RUN`, `CMD`, `ENTRYPOINT`, `ENV`, `ARG`,
  `LABEL`, `EXPOSE`, `VOLUME`, `USER`, `WORKDIR`, `COPY`, `ADD`, `ONBUILD`, `STOPSIGNAL`,
  `HEALTHCHECK`, `SHELL`.
- Variables (`$V`, `${V:-default}`, `${V:+set}`), `ARG` before `FROM`, `# escape=`, line
  continuations, JSON and shell forms, `RUN <<EOF` heredocs.
- Several stages: only the stages the target needs are built. `COPY --from` takes a stage
  name, a stage number or an image.
- `.dockerignore` (or `<Dockerfile>.dockerignore`) leaves files out of the context, with
  Docker's patterns (`**`, `!` exceptions).
- `ARG TARGETARCH` and the other platform arguments have this machine's values.

### The cache

- Each step's result is kept. A step is found again by:
  - the image before it,
  - the instruction, after its variables,
  - the build arguments a `RUN` sees,
  - the contents of the files a `COPY` or `ADD` copies.
- A second build prints `Using cache` for the steps it did not do again.
- `--no-cache` does every step again.
- The steps' images have no name: `podbag system prune` (or `docker image prune`) removes
  them, and the cache entries with them. `docker builder prune` forgets the cache.

### commit, export and diff

- `podbag commit CONTAINER [NAME]`: the container's image plus its changes, as a new image.
  - `-c` sets the config with a Dockerfile instruction: `-c 'CMD ["app"]'`.
  - It works on running containers too, without stopping them.
  - It does not pause a running container (Docker pauses by default), so a file that is
    being written at that moment can be cut.
- `podbag export [-o FILE] CONTAINER`: all its files as a tar, without its volumes.
- `podbag diff CONTAINER`: the paths it changed: `A` added, `C` changed, `D` deleted.

### Not supported in builds

- BuildKit features: `RUN --mount`, secrets, SSH, `--output`, cache export and import,
  builds for other platforms, `COPY --parents` and `--exclude`, `ADD --checksum`, `ADD` of a
  Git repository, heredocs in `COPY` and `ADD`. podbag stops with a message; build such
  Dockerfiles with buildx (see [buildx](#buildx)).
- `ADD` of bzip2 or xz archives.
- Base images from private registries: pulls are anonymous for now.
- Image history (`docker history`).
- Changes that a `RUN` makes in a `VOLUME` path stay in the image (Docker drops them).

## Checks

### podbag check

- Lists what podbag needs on this system, one line each:
  - the kernel and cgroup v2,
  - how images are mounted (as root),
  - the keeper,
  - without root: `newuidmap`/`newgidmap`, subordinate IDs, `pasta`, the systemd user
    session, AppArmor, linger, and the lowest port a user can publish.
- Each problem comes with the command that fixes it.
- As root, it warns when Docker's firewall blocks bridge networks.
- `podbag check --apparmor-profile` prints the AppArmor profile (see [AppArmor](#apparmor)).

### podbag check-image

- `podbag check-image IMAGE` reads every program and library in a pulled image. It does not
  run them.
- It reports what cannot run:
  - programs built for another C library (glibc in a musl image such as Alpine, or the
    reverse),
  - programs built for another CPU,
  - programs that need a library the image does not have (a warning).
- It exits with code 1 when it finds an error, so CI can use it.

### Clear errors

- When a program cannot start for one of these reasons, `run` and `exec` say why, not just
  "no such file". For example:
  - the program needs `/lib/ld-linux-aarch64.so.1` (glibc), and the image does not have it,
  - a script's first line names an interpreter that the image does not have.

## Volumes

| Option | Effect |
|---|---|
| `-v /host/dir:/dir[:ro]`, `-v ./dir:/dir` | a host folder; made if missing, as with `docker run -v` |
| `-v name:/dir[:ro][,nocopy]` | a named volume in `<data>/volumes/<name>/data`, made on first use |
| `-v /dir` | an anonymous volume, removed with the container by `--rm` or `rm -v` |
| `--tmpfs /dir[:size=64m,mode=1777,…]` | a file system in memory (`nosuid,nodev,noexec` unless asked) |

- The first time an empty volume is mounted, it gets the image's files at its target (with
  their owners and modes). `nocopy` turns this off.
- An image's `Volumes` become anonymous volumes.
- `podbag volume rm` refuses a volume that a container still names.
- Mount targets are found inside the container's root. A symbolic link in the image cannot
  send a mount outside it.
- `/proc` and `/sys` cannot hold mounts.

## Limits and security

### Options

| Option | Effect |
|---|---|
| `-m, --memory 512m` | `memory.max` (and as much swap); at least 6 MiB |
| `--cpus 0.5` | `cpu.max`; at least 0.01 |
| `--pids-limit 100` | `pids.max`; 0 or less means no limit |
| `--cap-add NAME`, `--cap-drop NAME` | change Docker's 14 default capabilities (`ALL` works; drops first) |
| `--security-opt no-new-privileges` | setuid programs cannot gain privileges (off by default, as in Docker) |
| `--security-opt seccomp=unconfined` | turn off the default seccomp filter |
| `--privileged` | every capability, no seccomp filter, kernel files shown, writable `/sys` and cgroup, any device; host devices are not added to `/dev` |

### Memory budget

- All containers run in one parent cgroup. Its `memory.max` is the budget.
- The default budget is 50% of the host's memory.
- Change it with `memory_budget` in `config.toml` (`"8GiB"`, `"25%"`, or `"0"` for no
  budget), or with `PODBAG_MEMORY_BUDGET`.
- Above 90% of the budget, the kernel frees cache first.
- Above the budget, the kernel stops one container.
- `ps` and `inspect` show why: `Exited (137, out of memory: memory budget)` or
  `… container limit`.
- Rarely, the kernel stops a second container before the first one's memory is free.

### Where the cgroups are

- As root: `/sys/fs/cgroup/podbag/<id>`.
- Without root:
  - the keeper runs in one systemd scope in the user's `podbag.slice`
    (`systemd-run --user --scope`),
  - each container's cgroup is below it, `<scope>/c/<id>`, so a start needs no
    `systemd-run` of its own,
  - without the keeper, each container gets its own scope, `podbag-<id>.scope`.
- `exec` runs commands in the same cgroup.

### Security defaults

- Docker's 14 default capabilities.
- A seccomp filter that refuses dangerous system calls: loading kernel modules, kexec,
  mount, new namespaces, keyrings, BPF, and others.
- Hidden kernel files: `/proc/kcore`, `/proc/keys`, `/sys/firmware`, and others.
- Read-only kernel settings: `/proc/sys`, `/proc/sysrq-trigger`, and others.
- Devices:
  - a container can make device nodes, as with Docker,
  - it can open only the usual ones: `/dev/null`, `zero`, `full`, `random`, `urandom`,
    `tty`, `ptmx`, terminals and `net/tun`,
  - as root, a cgroup BPF program applies runc's rules; without root, the user namespace
    refuses device nodes.
- `exec` gets the same rules.

### --privileged

- Turns all of these defaults off.
- Without root, it gives nothing the user does not have already:
  - the capabilities hold only inside the container's user namespace,
  - the host's kernel settings stay out of reach,
  - no host device is passed in.
- As root, the container has the host's root powers.

## Networks

### Options

| Option | Effect |
|---|---|
| `--network bridge` (default) | the default network: bridge `podbag0`, `10.77.0.0/16` (`default_subnet` in `config.toml`) |
| `--network NAME` | a network from `podbag network create`; its containers find each other by name |
| `--network A --network B` | more bridge networks (`eth1`, …); the default route stays on the first |
| `--network-alias NAME` | one more name on the first user network |
| `--network host` | the host's network |
| `--network none` | loopback only |
| `-p [IP:][HOST:]CONTAINER[/tcp\|udp]` | publish a port; without a host port, the kernel picks one (see `ps`, `inspect`) |

### Names and addresses

- New networks get `10.78.<n>.0/24`, unless `--subnet` gives another.
- A subnet that overlaps another network or a host route is refused.
- Container names and aliases are written into each container's `/etc/hosts`. podbag
  updates them as containers start and stop.
- There is no DNS server. When two containers share an alias, the first one answers.
- `podbag network connect [--alias NAME] NET CONTAINER` and
  `podbag network disconnect NET CONTAINER` change the networks of a stopped container,
  for its next start. A running container is refused.

### How it works

- As root:
  - the bridges are in the host's network namespace,
  - the NAT rules are in an nftables table named `podbag`; no other table is changed,
  - IPv4 forwarding is turned on.
- Without root:
  - one session process per user holds a user namespace and a network namespace, with the
    bridges in it,
  - `pasta` connects them to the host; DNS goes through pasta at `169.254.1.1`,
  - the first container starts the session, and the last one stops it.
- Published ports go through each container's helper, in both modes. The container sees
  the gateway as the client's address.

## Docker API

### Start it

- `podbag system service` serves the Docker Engine API on a Unix socket: version 1.47,
  and clients down to 1.24.
- It is not a daemon: containers do not depend on it.
- It stops after 60 seconds without a connection (`--timeout`; `0` means never).

```sh
podbag system service &         # without root
podbag system docker use        # the docker command now goes to podbag
docker run --rm -it alpine sh
podbag system docker restore    # back to the context you had (Docker Desktop, …)
```

- `podbag system docker status` shows where the `docker` command goes: podbag, Docker
  Desktop, OrbStack, Colima, or another host.
- `use` makes a docker context named `podbag` when there is none, remembers the context in
  use, and makes `podbag` the current one. `restore` goes back. Other contexts are never
  changed or removed. It needs the docker CLI.
- `DOCKER_HOST` wins over contexts. In a shell that sets it, `status` says so.
- The same switch is in the TUI (`D`) and the desktop app.
- By hand, it is: `docker context create podbag --docker
  host=unix://$XDG_RUNTIME_DIR/podbag/docker.sock` (as root: `/run/podbag/docker.sock`),
  then `docker context use podbag`. Or set
  `DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podbag/docker.sock` in one shell.
- On Windows, the docker CLI cannot reach podbag's API yet.

- With systemd, the service can start on the first connection:
  - `podbag system service --print-units` prints a socket unit and a service unit,
  - put them in `~/.config/systemd/user/` (as root: `/etc/systemd/system/`), and enable
    the socket unit.
- The socket is private to its user (`0600`; `0660` as root).

### What works

- System: `version`, `info`, `system df`, `events` (live only; there is no history).
- Images: `pull`, `images`, `image inspect`, `tag`, `rmi`, `image prune`, `save`, `load`,
  `build` with the classic builder (see [Builds with the docker CLI](#builds-with-the-docker-cli)),
  `builder prune`.
- Containers: `run` (attached, `-d`, `-i`, `-t`, `--rm`), `create`, `start [-a]`, `stop`,
  `restart`, `kill`, `wait`, `rm`, `logs` (`-f`, `--tail`, `-t`, `--since`), `attach`,
  `ps` (filters `label`, `name`, `id`, `status`), `inspect`, `port`,
  `exec` (`-i`, `-t`, `-d`, `-e`, `-w`, `-u`), `cp`, `commit`, `export`, `diff`,
  `container prune`.
- Volumes and networks: `create`, `ls`, `inspect`, `rm`, `prune`.

### What does not work

- Refused, with the reason: devices, `--read-only`, `--pid host`, `--volumes-from`,
  `--link`, port ranges, other network or volume drivers.
- Accepted with a warning, because ignoring them is safe: `--dns`, `--add-host`, ulimits,
  log drivers, fixed addresses.
- Answered with 501 and what to do instead: BuildKit's build API (use buildx with the
  `docker-container` driver), `top`, `stats`, `pause`, `rename`, `update`, `push`,
  `search`, swarm and plugins.

### cp, save and load

- `docker cp` works on running and stopped containers:
  - in a running container, it goes through the container's own mounts,
  - in a stopped container, podbag mounts its files privately for the copy, and no start
    begins meanwhile,
  - paths are the container's: a link to `/etc` means the container's `/etc`,
  - `/proc` and `/sys` come out empty, as with Docker.
- `docker save` writes Docker's archive format:
  - the store keeps files, not layer archives, so the saved layers (and the image ID) get
    new digests,
  - it keeps the image's command, environment, user, working folder, volumes, health
    check, labels, exposed ports and stop signal, but not its history (images stored
    before podbag kept labels have none).
- `docker load` reads `docker save` archives and OCI image layouts, plain or gzip:
  - it checks every digest,
  - it refuses an archive that is larger than the store limit.

### Differences from Docker

- `docker attach` to a container that another process started reads its output from the
  log. It cannot send input.
- When a client leaves an attached `exec`, the command ends. (Docker keeps it running.)
- Volumes and networks keep their labels.
- `network connect` changes stopped containers only.

### Docker Compose

- Works through the same socket: `up`, `down` (`-v`), `ps`, `logs`, `exec`, `run`.
- Supports the project's networks and volumes, several networks per service, service and
  container names, `depends_on` with `condition: service_healthy`, restart policies and
  health checks.
- Names live in each container's `/etc/hosts` (no DNS server). When replicas share a name,
  the first one answers.
- `docker compose build` works with the classic builder (see below), or with a buildx
  builder (`--builder`).

### Builds with the docker CLI

```sh
export DOCKER_BUILDKIT=0          # the classic builder: podbag builds the image
docker build -t myapp .
docker compose build
```

- podbag serves the classic builder (`POST /build`), and its `_ping` answers
  `Builder-Version: 1`.
- A docker CLI with the buildx plugin still sends `docker build` to buildx, whatever the
  server says. `DOCKER_BUILDKIT=0` makes it use the classic builder.
- Without `DOCKER_BUILDKIT=0`, buildx starts its own BuildKit in a podbag container, and
  keeps the result in its cache (add `--load` to get the image).
- The build works as `podbag build` does: see [Builds](#builds).

### buildx

- buildx's `docker-container` driver runs BuildKit in a podbag container (privileged), and
  loads the result through `docker load`.

```sh
docker buildx create --driver docker-container --name podbag --use
docker buildx build --load -t myapp .      # then: docker run myapp
docker compose build --builder podbag
```

- The builder keeps running after the service stops: it is an ordinary container.
- Its cache stays in its volume until `docker buildx rm`.
- The default `docker` driver (the daemon's own BuildKit) is not available.

## TUI and GUI

- `podbag tui` shows containers, images, volumes and networks in the terminal.
- `podbag-desktop` shows them in a window, with a tray (menu bar) item.
- Both are clients of the Docker API. They work the same on Linux, macOS and Windows.

### Which engine they use

- They manage podbag, Docker (Docker Engine or Docker Desktop), or a socket you name.
- The order:
  1. `podbag tui --engine podbag|docker|unix:///path`, for one run.
  2. The saved choice. Both UIs read and write it in `ui.json`: `~/.podbag/ui.json` on
     macOS, `~/.config/podbag/ui.json` on Linux, `%LOCALAPPDATA%\podbag\ui.json` on
     Windows.
  3. `DOCKER_HOST`, when it is a Unix socket (`unix://…`).
  4. podbag's own socket.
- Docker is found as the docker CLI finds it: `DOCKER_HOST`, the current context, then the
  usual sockets.
- Docker is never started. When it does not answer, the UIs say how to start it.
- podbag's own socket:
  - Linux: `$XDG_RUNTIME_DIR/podbag/docker.sock` (as root: `/run/podbag/docker.sock`).
    When nothing answers, they start `podbag system service`. It stops a minute after the
    last client.
  - macOS: `~/.podbag/run/docker.sock`. The TUI starts the VM when it is off. The desktop
    app asks first, with a Start VM button.
  - Windows: `podbag tui` runs in the WSL distribution. The desktop app reaches the API
    through `podbag.exe system dial-stdio`.

### podbag tui

- Four lists: containers, images, volumes and networks. The selected one's details show
  under the list.
- The lists change as the engine's events come. Without events, they are read every 2
  seconds.
- It fits a terminal of 80×24.
- The top line shows the engine. On macOS, with podbag, it also shows the VM's state
  (running, standby, starting or stopped). It reads the state from files, so it never
  wakes the VM.
- The top line also shows where the `docker` command goes, for example
  `docker → Docker Desktop`. It is hidden when the docker CLI is not installed.
- When a newer podbag is out, the bottom line says "Update x.y.z available".
  `PODBAG_NO_UPDATE_CHECK=1` turns the check off.

| Key | What it does |
|---|---|
| `1`–`4`, `Tab`, `←` `→` | Switch the list |
| `↑` `↓`, `PgUp` `PgDn` | Move |
| `Enter` or `i` | All details (inspect) |
| `s`, `x`, `r` | Start, stop, restart a container |
| `l` | Logs, as they come |
| `e` | A shell in the container, then back to the TUI |
| `d` | Remove (it asks first) |
| `p` | Pull an image (in Images). `Tab` changes where from |
| `c` | Remove all that is not used (it asks first) |
| `/` | Search. `Esc` clears it |
| `E` | Switch between podbag and Docker. The choice is saved |
| `D` | Point the `docker` command at podbag, or back (it asks first) |
| `m` | Mouse on or off |
| `?` | All keys |
| `q` | Quit |

#### The engine

- `E`, or a click on the engine's name, switches between podbag and Docker. From a socket,
  it goes to podbag.
- The choice is saved in `ui.json`, so the next `podbag tui` and the desktop app use it
  too.
- The shell (`e`) is `podbag exec -it <container> sh` with podbag. With Docker, it is
  `docker exec -it <container> sh`. Without the docker CLI, there is no `e`.
- With Docker, pause (`p`), rename (`n`), stats and top show, because Docker has them.

#### The docker command

- `D`, or a click on `docker → …`, points the `docker` command at podbag. It asks first.
- It makes a docker context named `podbag` and uses it. The context in use before is
  saved in `ui.json`. Other contexts stay as they are.
- At podbag, `D` goes back to the context before (`default` when none is saved).
- When `DOCKER_HOST` is set, contexts do not apply. Shells that set it keep using it.
- `podbag system docker status|use|restore` does the same in a shell.

#### Pull

- `p` in Images opens the pull form.
- `Tab` changes where from: Docker Hub, GitHub, Amazon ECR, Quay.io, Google Artifact
  Registry, GitLab or another registry. `Shift+Tab` goes back.
- The form shows who is logged in there. It reads the docker CLI's logins. It never shows
  a password.
- ECR, GAR and another registry need a host. Type it, then press `Enter`.
- ECR and GAR also show the command to log in.
- A short name gets the registry's host: `me/app:1` from GitHub is `ghcr.io/me/app:1`.
- From Docker Hub, typing searches Docker Hub, when the engine can search. `↑` `↓` choose
  a result. podbag cannot search yet, so there is no search with podbag.
- The pull sends your login for that registry to the engine.

#### The mouse

- Click a tab to switch the list. Click a row to select it.
- Double-click a row to see all its details (like `Enter`).
- Click a key at the bottom to do what the key does.
- Click Yes or No in a question.
- The wheel scrolls the list, the details and the logs.
- While the TUI has the mouse, the terminal does not select text. To select text, hold
  `Shift` while you drag. Some macOS terminals use `Option` instead, for example iTerm2.
  In Terminal.app, View > Allow Mouse Reporting (`⌘R`) gives the mouse back to the
  terminal.
- `m` turns the mouse off and on. `podbag tui --no-mouse` starts with it off.

### podbag-desktop

- A window with Containers, Images, Volumes, Networks and Settings.
- Each list has a search box, and the same actions as the TUI.
- A container's page has its details, its logs (as they come) and all its data.
- With no containers, the window shows how to start one.
- The tray item shows the engine's state and how many containers run. It has Open podbag
  and Quit. On macOS, it also has Start VM or Stop VM.
- Closing the window keeps the tray item. Quit ends the app.
- While the window is closed, the app asks the engine nothing. So it keeps no VM or
  service awake. On macOS, the tray still shows the VM's state: it reads it from files.
- Open shell opens a terminal window: Terminal on macOS, the first terminal found on
  Linux, a console window on Windows.
- Light and dark themes follow the system. Settings can change the theme.
- Languages: English, 한국어, 日本語, 简体中文, Filipino, Español and Tiếng Việt. The app
  follows the system language. Settings can change it.

#### Engine switch

- "This window: podbag / Docker" sits at the top of the sidebar and in Settings.
- The choice is saved in `ui.json`. `podbag tui` uses the same choice.
- Switching reloads the lists. The app never starts Docker. When Docker does not answer,
  it says so: "Docker is not running. Start Docker Desktop."
- With Docker, Open shell runs `docker exec -it <container> sh`. It is hidden when the
  docker CLI is missing. Pause, rename, stats and top show when the engine has them.
- The tray shows the engine in use.

#### Sidebar

- Drag its edge to change its width (160–360 px). A double click sets it back.
- The sidebar button in the top bar makes it a narrow rail of icons. Keys: `⌃⌘S` on
  macOS, `Ctrl+B` elsewhere.
- Below about 720 px it becomes a rail by itself. It comes back when there is room.
- The edge works with the keyboard: arrow keys change the width, `Home` and `End` go to
  the limits, `Enter` hides or shows the sidebar.
- The app keeps the width and the rail choice.
- Back and forward: the buttons in the top bar, `⌘[` `⌘]` on macOS, `Alt+←` `Alt+→`
  elsewhere. `⌘K` or `Ctrl+K` goes to the search box.

#### VM card and status bar

- The VM card (podbag on macOS and Windows) shows the VM's state, running containers,
  CPU and memory, and Start or Stop. It reads `podbag vm status --json`, which wakes
  nothing. It updates only while the window shows.
- The status bar shows podbag's version, a new version when one is known, a Terminal
  button, the engine's state, "N of M running", the VM's memory and where the `docker`
  command goes.

#### Settings

- Settings opens from the sidebar, from the tray (Settings…), with `⌘,` on macOS and
  with `Ctrl+,` elsewhere.
- Virtual machine (podbag on macOS): the state, the settings in use, uptime and disk use.
  Start, Stop and Restart. CPUs, memory and the idle power-off time (0 = never). Apply
  saves them with `podbag vm set`.
  - When the VM runs, Apply asks: Restart now (it says how many containers stop) or At
    the next start.
  - When an environment variable (`PODBAG_VM_CPUS`, …) sets a value, the app says so.
  - Windows: the state, Start and Stop only. WSL's CPUs and memory are set for all
    distributions in `.wslconfig`. podbag does not change them.
- Updates: podbag's version and the app's version. Check for updates. The app also
  checks when the window opens, at most once a day. While the window is closed, it asks
  nothing.
  - Update podbag runs `podbag upgrade --yes`. When podbag's folder needs administrator
    rights, the app shows the command (`sudo <path> upgrade`) with Copy and Open terminal.
  - The desktop app has no installer yet. It cannot update itself.
- The tray shows "Update available (x.y.z)…" when one is known. It opens Updates.

#### Pull panel

- Pull image on Images opens it. `⌘⇧P` (macOS) or `Ctrl+Shift+P` opens it from any
  page.
- One field: type to search Docker Hub, or paste a full reference. `↑` `↓` move, `Enter`
  pulls, `Esc` closes.
- A search result asks for the tag (`latest` first), then pulls.
- When the engine cannot search (podbag today), the panel says so. Type the full name.
- Pull from: Docker Hub, GitHub, Amazon ECR, Quay.io, Google Artifact Registry, GitLab or
  your own registry. Each shows who is logged in.
- Log in asks for a user and a password or token. The password goes only to the login
  program's stdin. The app does not keep it or show it. ECR and Google Artifact Registry
  show their cloud's login command.
- A private pull sends the login only to an engine on this computer.

#### docker command

- When `docker` goes to another engine (for example Docker Desktop), a banner says so:
  what you run in a terminal does not show here.
- Use podbag for docker points the `docker` command at podbag with podbag's own docker
  context. Other contexts stay as they are. The banner, the status bar, Settings and the
  tray all offer it. Switch back goes to the context in use before.
- When `DOCKER_HOST` is set, shells that set it do not follow the switch. The app says so.
- Without the docker CLI, the app says so and hides the switch.
- Import from Docker (on Images) copies chosen images from Docker to podbag. It only
  reads from Docker.

### Build the desktop app

- It is a separate program in `desktop/` (Tauri 2). The `podbag` binary does not carry
  it, and `cargo build` leaves it out.
- On Linux, it needs WebKitGTK 4.1 and an app indicator library:

```sh
sudo apt install libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev
cargo build --release -p podbag-desktop
./target/release/podbag-desktop
```

- It finds the `podbag` program next to itself, in `PATH`, or in `/usr/local/bin`,
  `/opt/homebrew/bin` and `~/.local/bin`. `PODBAG_BIN` sets another one.
- There are no installers yet (.dmg, .msi, AppImage).

#### Its name: podbag

- The executable is `podbag-desktop`, so that it does not take the place of the `podbag`
  command in the same folder. The name people see is podbag.
- macOS: build `podbag.app` (the name podbag in the menu bar and the Dock, and its icon):

```sh
desktop/bundle-macos.sh            # target/release/podbag.app (debug: add "debug")
open target/release/podbag.app
```

- The script makes the bundle itself (Tauri's bundler needs the separate `tauri-cli`).
- Run bare, the executable's menu says About, Hide and Quit podbag, but macOS shows
  `podbag-desktop` as the menu bar's title and in the Dock. Only a bundle changes that.
- Windows: the .exe's product name and file description are podbag.
- Linux: `desktop/podbag.desktop` is a desktop entry named podbag. Copy it to
  `~/.local/share/applications/`.

#### Its icon

- The app icon follows Apple's grid: a rounded square of 824 px on a 1024 px canvas, a
  soft shadow, a light blue background, and the bag from `docs/images/logo.png`.
- At 16 and 32 px the bag takes more of the square, so it stays readable.
- `desktop/icons/make-icons.sh` makes every icon file again (macOS: Swift and `iconutil`).

### What is hidden

- The API has no `pause`, `rename`, `stats` or `top` yet. Both UIs ask the engine when they
  start, and hide what it does not have.

## macOS

### The VM

- On a Mac (Apple silicon), podbag runs containers in a small Linux VM of its own
  (Virtualization.framework, Linux 6.18).
- The first command starts the VM in about half a second.
- The VM powers off 60 seconds after the last container. `podbag vm set --idle` changes
  this.
- With no container, only a small standby process runs (under 2 MiB).
- With the VM running, `podbag run --rm --network none alpine true` takes about 8 ms, and
  the same `docker run` about 15 ms.

### Commands and files

- Commands run in the VM. Their output, exit code, signals and terminal size pass through.
- Only commands that read input (`-i`, or `load` without `-i FILE`) take the Mac's stdin.
- `-e NAME` takes the Mac's value of NAME.
- The Mac's `/Users` is in the VM at the same path, so `-v ./src:/work` works from a folder
  under `/Users`.
- Other Mac paths are refused, also for `save -o`, `load -i` and `--rootfs`.
- Named volumes live in the VM.
- Images, containers and volumes stay on a 64 GiB sparse disk, `~/.podbag/disk.img`.
- `podbag vm stop`, or logging out, powers the VM off cleanly.
- `podbag vm start`, `vm restart` and `vm status` start it, restart it and show its state.
  `vm status` wakes no VM.
- Containers cannot reach the VM's agent.

### Resources

- Memory: 2 GiB. CPUs: the Mac's, up to 8. Idle power-off: 60 seconds.
- `podbag vm set` changes them and saves them in `~/.podbag/vm.json`:

```sh
podbag vm set --cpus 4 --memory 6G --idle 5m
podbag vm set --memory 4096M --restart     # apply now
```

- `--cpus`: from 1 to the Mac's CPUs.
- `--memory`: from 1 GiB to the Mac's memory minus 4 GiB, which macOS keeps for itself.
  On a 24 GiB Mac that is 20 GiB. A plain number is MiB. A small Mac always allows 1 GiB.
- `--idle`: `90s`, `5m`, `1h`, or `0` (never power off by itself).
- The VM uses new settings at its next start. `--restart`, or `podbag vm restart`, applies
  them now.
- A restart powers the VM off, so running containers stop. Both commands ask first. Without
  a terminal they need `--yes`.
- `PODBAG_VM_CPUS`, `PODBAG_VM_MEMORY` (MiB) and `PODBAG_VM_IDLE` (seconds) still win over
  the saved values. `vm set` says so when one is set.
- `podbag vm status --json` is for apps. It has the state (`running`, `standby`,
  `starting`, `stopped`), the settings in use and those of the next start, this Mac's
  limits, the uptime, the disk use and, while the VM runs, the running containers and the
  VM's CPU and memory use. It reads files only, so it wakes no VM.
- The disk use is what the sparse disk takes on the Mac (`allocatedBytes`), and its size
  (64 GiB).
- Containers get 1048576 open files and `vm.max_map_count` 262144, as in Docker Desktop.
- A container that needs more memory than the VM has is stopped with exit code 137. The VM
  keeps running.

### docker CLI, Compose and buildx

```sh
podbag system docker use        # or: export DOCKER_HOST=unix://$HOME/.podbag/run/docker.sock
docker run --rm alpine echo hi
docker compose up -d
docker buildx create --use --driver docker-container
docker buildx build --load -t app .
```

- The socket stays while the VM is off. The standby process waits on it, and the first
  request starts the VM.
- Bind mounts must come from `/Users`.

### Ports, disk and memory

- Published ports (`-p 8080:80`) answer on the Mac's `localhost:8080`. TCP only for now.
- Disk space comes back while the VM runs: the Mac gets back what deleted images,
  containers and volumes used, within seconds.
- Memory comes back when the VM powers off.
- Disk writes are synced with fsync, as lima does. A power cut can lose what the drive's
  cache held.

### Build it yourself

- Release files carry the VM already. To build one: [build.md](build.md#macos).
- Images must be for arm64. There is no Rosetta in the VM yet.

## Windows (experimental)

- On Windows 10 or 11 (x86_64 or arm64), `podbag.exe` runs its Linux engine in a WSL 2
  distribution of its own, named `podbag`.
- It is built and tested in CI, but not yet tried on a Windows machine. Please report what
  you find.

### Setup

- Turn WSL 2 on: run `wsl --install` in an administrator terminal, then restart.
- The first command imports the distribution into `%LOCALAPPDATA%\podbag\wsl`, with the
  engine that `podbag.exe` carries.
- A newer `podbag.exe` updates the engine there. Images, containers and volumes stay.

### How it works

- Commands run in the distribution with `wsl.exe --exec`, as root, in the current folder.
- Their output, input and exit code are those of the Windows command.
- Windows paths in `-v`, `--rootfs`, `load -i` and `save -o` become WSL paths:
  `C:\src` is `/mnt/c/src`.
- Network paths (`\\server\share`) are refused.
- Named volumes and images live in the distribution.
- `podbag vm status|start|stop|restart|rm` shows, starts, stops, restarts or removes the
  distribution. `vm rm` deletes its images, containers and volumes. A stop or a restart
  stops its containers.
- `podbag vm status --json` gives the state, for apps.
- WSL's CPUs and memory are set for all distributions at once, in
  `%UserProfile%\.wslconfig`. podbag does not change them, so there is no `vm set` on
  Windows.

### Limits

- The distribution shares WSL's network with your other WSL distributions. They see
  podbag's bridge (`podbag0`, `10.77.0.0/16`) and its firewall rules.
- Published ports reach Windows through WSL's localhost forwarding.
- Resource limits need cgroup v2. Where WSL's kernel does not have it, containers run
  without limits, with a warning.
- Not yet: the Docker API for Windows programs (the docker CLI on Windows).

## Update podbag

```sh
podbag upgrade --check          # only say whether a newer release exists
podbag upgrade                  # install the latest release (it asks first)
podbag upgrade --version 0.1.0  # install this release; the only way to go back
```

- `--yes` skips the question. Without a terminal, `upgrade` needs it.
- `--check --json` prints `{"current", "latest", "newer"}`, for apps.

### How it works

- It reads the latest release from GitHub's API, over HTTPS only.
- It downloads this system's file and the release's `SHA256SUMS` next to podbag.
- The file must match its SHA-256 sum. Nothing downloaded runs before that.
- Then the new file must print the expected version (`podbag --version`). On macOS, it
  must also have a valid signature with the virtualization entitlement.
- Only then is it renamed over podbag. The old one stays as `podbag.old`.
- On Windows a running `.exe` cannot be overwritten, so it is renamed first.
- It never goes to an older release unless `--version` asks for it.

### Where podbag is installed

- `install.sh` puts podbag in `/usr/local/bin`. Writing there needs root, so run
  `sudo podbag upgrade`. podbag says so, with the exact command (exit code 77).
- On Windows, use an administrator terminal when podbag is in a protected folder.

### After the update

- macOS: the VM's guest is in the program, so the VM must restart to use the new one.
  `upgrade` restarts it at once when no container runs. Otherwise it says so: run
  `podbag vm restart` (the containers stop), or wait until the VM powers off when idle.
- Linux: a `podbag system service` that started on demand ends when no client is
  connected, and the next client starts the new one. A service with `--timeout 0` stays.
  Running containers keep running, with their keeper.
- Each build of podbag has its own keeper (`keeper-<build>.sock`). So after an update, the
  new program starts its own keeper at once, and starts stay fast. The old keeper watches
  its containers until they end, then exits.
- Windows: the next command brings the new engine into podbag's WSL distribution.

### Limits

- The download is checked against the release's `SHA256SUMS`, which comes from the same
  place. A signature with a release key is not checked yet.

## Requirements on Linux

- Linux 5.12 or later, with cgroup v2 at `/sys/fs/cgroup`.
- composefs (as root) needs Linux 6.5 or later with EROFS. Older kernels use hard-link
  trees.
- Without root:
  - `newuidmap` and `newgidmap` (package `uidmap`), with entries in `/etc/subuid` and
    `/etc/subgid`,
  - `pasta` (package `passt`) for bridge networks, or `--network host` or `none`,
  - on Ubuntu 24.04 and later, an AppArmor profile (below),
  - a systemd user session, for resource limits and the memory budget. Without it,
    containers still run, with a warning, but `--memory`, `--cpus` and `--pids-limit` are
    refused.

## AppArmor

- Ubuntu 24.04 and later set `kernel.apparmor_restrict_unprivileged_userns=1`.
- With this setting, a program without an AppArmor profile cannot make the user namespaces
  that rootless containers need.
- podbag then stops with "cannot create an unprivileged user namespace".
- Add a profile once:

  ```sh
  podbag check --apparmor-profile | sudo tee /etc/apparmor.d/podbag >/dev/null
  sudo apparmor_parser -r /etc/apparmor.d/podbag
  ```

- `podbag check --apparmor-profile` fills in podbag's path. If you move podbag, add the
  profile again.
- The [install script](../install.sh) does this for you.
- After this, podbag runs without sudo.
