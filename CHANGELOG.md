# Changelog

## Unreleased

### Training

- Training modules come from the catalog on podbag.dev. Podbag Desktop refreshes it at
  start, when Training opens, and with **Refresh**. `podbag lab list` refreshes it too.
  New modules appear without a new podbag.
- Course modules are data files (`podbag-course-v1`): steps, commands, checks and step
  names. podbag only reads the practice engine to check a step; it never runs a command
  from a module file.
- Each file is checked against the catalog's SHA-256. Offline, the installed modules stay.
  A finished module stays finished after an update.
- New module `web-service-recovery`: a web server exits because a setting is missing.
  Read the cause with `podbag logs`, then start it again with `-e` and open its page.
- New module `container-network`: create a network, run a server without a port, and
  reach it by name from other containers on the same network.
- Module format `podbag-course-v2`: checks for images, container states, health, restarts
  and networks; text files for the practice directory; steps with the Docker CLI and
  Compose. Training modules for every podbag feature use it.
- All modules and their guides: https://podbag.dev/training/.

### Kubernetes

- Show OpenShift Routes in Podbag Desktop, the TUI and `podbag kube`. They appear only
  when the context's API serves `route.openshift.io/v1`.
- In Podbag Desktop, the Routes tab shows each Route's host, path, Service and TLS mode.
  Select a host to open its address in the browser.
- `podbag kube migrate` does not copy Routes, because a Route's host belongs to its
  cluster.

### Networking

- `podbag run` and `podbag create` take `--add-host NAME:IP`, `--dns`, `--dns-search` and
  `--dns-option`, as Docker does. `host-gateway` stands for the host.
- The Docker API applies `ExtraHosts`, `Dns`, `DnsSearch` and `DnsOptions`, and inspect
  shows them. Compose `extra_hosts` and `dns` now take effect.
- Where `host-gateway` has no address (rootless bridge, `--network none`, WSL), the CLI
  refuses it. The Docker API leaves the entry out with a warning.

### Builds

- `podbag build` and `docker build` pull private base images with the login for each
  image's registry. Before, base images were pulled anonymously.
- The Docker API reads the `X-Registry-Config` header from the docker CLI and falls back
  to logins saved with `podbag login`. A malformed header gets a 400 reply.
- On macOS and Windows, `podbag build` sends the host's logins for the registries in the
  Dockerfile to the engine. A Dockerfile on stdin sends none.

### Desktop

- Run a container from Podbag Desktop: Run container on the Containers page, and Run on
  each image row. The form takes the image, a name and ports.
- More options set environment variables, volumes, the command, a restart policy and
  auto-remove.
- A missing image is pulled first. The new container then opens with its logs.

### macOS

- Published UDP ports, such as `-p 5353:53/udp`, answer on the Mac's localhost, like TCP
  ports.
- A UDP client's flow ends after 90 seconds without traffic, as with Docker.
- A port that another Mac program holds no longer stops the other published ports. It is
  reported in `~/.podbag/run/host.log`.

## v0.3.2 (2026-11-23)

### Desktop

- Use the CLI that comes with Podbag Desktop in a terminal. When a terminal finds no
  `podbag`, a notice offers to install the `podbag` command. Settings > Terminal command
  can install it later.
- The command leads to the app's own podbag, so it is updated with the app. On macOS and
  Linux it is a link in `/usr/local/bin`. On Windows, the app's folder is added to the
  user's `Path`.
- When the app was updated another way and a separate terminal podbag is older, a notice
  offers to update it to the app's version.

## v0.3.1 (2026-11-22)

### macOS

- Run podbag and Podbag Desktop on Intel Macs. The VM runs an x86_64 Linux guest, and
  images are pulled for `linux/amd64`.
- `install.sh` and `podbag upgrade` choose `podbag-x86_64-macos` on an Intel Mac. On
  Apple silicon, they choose the arm64 build, also from a shell that Rosetta runs.
- Desktop updates choose `podbag-desktop-x86_64-macos.zip` on an Intel Mac.

### Updates

- Update Podbag Desktop in Settings > Updates. The app installs the new release
  package, checks it against `SHA256SUMS`, and starts again. The package carries
  the app's podbag, so both change together.
- macOS replaces `podbag.app` after it checks the version and signature. Linux uses
  `pkexec apt-get` or `pkexec dnf`. Windows runs `msiexec` after the app ends.
- The app also updates a separate terminal podbag, for example one from `install.sh`,
  to the same version.
- `podbag upgrade` does not replace the podbag inside a Desktop package. It points to
  the app. `podbag upgrade --check` still works.
- Stable releases are published to crates.io automatically.

## v0.3.0 (2026-11-21)

### Desktop and installation

- Install Podbag Desktop on Windows with a native x86_64 or ARM64 MSI. Each
  installer includes the matching Podbag CLI and Linux engine. Enable WSL 2
  to run containers.
- The macOS Desktop bundle can include the signed CLI and its Linux VM guest.
  Desktop can start its VM using the bundled engine.
- Resize the Desktop sidebar and collapse it to icons.

### Training and terminal workflows

- Podbag Training adds four beginner modules for running containers, opening a
  published web page, keeping files in a volume, and replacing a web service.
  Desktop shows step progress and runs exercises in isolated practice sessions.
- Import a `.podbag` volume lab to create a volume, save a JSON file, and check
  the response from a local HTTP server. Ending the lab removes its temporary data.
- Manage registries, volumes, images, Kubernetes resources, and settings from
  the terminal dashboard. The CLI also provides `podbag engine`, `import`, `kube`,
  `settings`, and `tutorial` commands.

### Kubernetes

- Read container records once per CRI stats request to reduce repeated work
  when the kubelet collects stats for multiple containers.

## v0.2.0 (2026-11-20)

### Containers and images

- Build images from a Dockerfile with `podbag build` or the classic Docker builder.
- Save container changes as an image with `podbag commit`, list changed files with
  `podbag diff`, or export the container filesystem as a tar archive with `podbag export`.
- Log in to registries, pull private images, push images, and search Docker Hub.
  Registry commands can use existing Docker credentials.
- View resource use with `podbag stats` and running processes with `podbag top`.
  Pause, resume, or rename a container. Use `podbag update` to change resource
  limits and restart policies.
- Check stored files for corruption with `podbag system verify`. The command lists
  the images that use each damaged file.

### Kubernetes, Swarm, and migration

- Use `podbag cri serve` as the container runtime on a Linux Kubernetes node.
  It supports the kubelet and k3s, including exec, attach, and port forwarding.
- Run Docker Swarm services and stacks on a single node. Services support rolling
  updates and rollbacks.
- Copy single-platform images, stopped containers, and local volumes between engines with
  `podbag migrate` or Podbag Desktop. The source stays in place.
- Browse and copy standalone Pods, Services, Deployments, and filesystem
  persistent volume claims between Kubernetes contexts in Podbag Desktop.
  Bound claims include their data. The [migration guide](docs/usage.md#migrate-between-engines-or-kubernetes-clusters)
  lists the storage requirements.

### Terminal and desktop

- Manage containers, images, volumes, and networks in `podbag tui` or
  Podbag Desktop. Connect to Podbag, Docker, Podman, Kiri, or a Docker context.
  Pull images using registry presets or a custom host.
- Switch the Docker CLI to Podbag and restore the previous context from either
  interface or with `podbag system docker`.
- Set the macOS VM's CPU count, memory, and idle shutdown time in Desktop or
  with `podbag vm set`.
- Set Desktop to open at login. The window follows the selected theme.
  The macOS app includes a window toolbar and a monochrome menu bar icon.
- On first launch, Desktop offers a link to star Podbag on GitHub. Choose Skip
  to dismiss the prompt.
- The READMEs and project videos use the same bag icon as Podbag Desktop.

### Installation and updates

- Install podbag on Linux and macOS with a script that checks the download's
  SHA-256 sum. Use `podbag upgrade --check` to check for updates or
  `podbag upgrade` to install the latest release.
- Install Podbag Desktop and its engine from DEB or RPM packages on amd64 and
  arm64. See the [Linux desktop package instructions](docs/build.md#linux-desktop-packages).
- Download the Android arm64 CLI from CI for image commands.

### Fixed

- Preserve file modification times from images and give each crontab its own inode
  so cron can load it. For images stored by v0.1.0, remove the old image and pull
  or load it again to apply the fix.
- Restore truncated files when a pull or load reads them again from an image layer.
- Honor the configured timeout when stopping a container.
- Keep the installed CLI if a download fails its checksum or cannot run.
  Keep the existing Ubuntu AppArmor profile if the installer cannot generate
  or load its replacement.

## v0.1.0 (2026-11-20)

- Run Docker and OCI images on Linux as root or as an unprivileged user.
  The engine starts containers on demand and shares identical files across images.
- Run containers on Apple silicon Macs in an on-demand Linux VM.
  Windows support uses WSL 2 and is experimental.
- Use the Docker CLI, Compose, and buildx through podbag's Docker API on Linux and macOS.
