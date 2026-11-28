# Changelog

## v0.4.0 (2026-11-28)

### Desktop

- Podbag Desktop draws its own interface with the GPU (Metal, Direct3D 12, Vulkan or
  OpenGL) instead of showing a web page in a WebView. It has the same pages, dialogs,
  tray item and settings, in the same 7 languages.
- Settings, launch at login and the answers to the app's questions carry over from
  earlier versions.
- On Linux, the app no longer needs WebKitGTK or libxdo. The `.deb` and `.rpm` depend on
  GTK 3 and libayatana-appindicator (the tray item), xkbcommon, the X11 and Wayland
  client libraries, and EGL. Mesa or the GPU vendor's driver draws the window.
- On Windows, the installer no longer downloads WebView2. It replaces an installed
  0.3.x in place: the same folder, Start menu and desktop shortcuts, and launch at login.
- On macOS, the window has a standard title bar. Drag the title bar, not the toolbar, to
  move the window.
- Screen readers cannot read the window yet.
- Run a container from the Containers or Images page. The form accepts the image, name,
  ports, environment variables, volumes, command, restart policy and auto-remove option.
  Desktop pulls a missing image and opens the container's logs after it starts.
- Search Settings by name. Configure local engine options, network and DNS defaults,
  registry proxies, task notifications and Buildx builders.
- Migrate shows icons for source and resource types. Desktop can also analyze image
  security, run local models and install declarative extensions.

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
- Do supported Training actions in Desktop, including logs, inspection, and operations
  on containers, images, volumes and networks.
- All modules and their guides: https://podbag.dev/training/.

### Images

- View image history with `podbag history` and import a root filesystem archive with
  `podbag import`. Both operations are available through the Docker API.
- podbag can compress images that no container uses, file by file with zstd. It is off
  by default. Files that two images share stay shared. In our test the store went from
  725 MB to 337 MB.
- `podbag system compression idle` turns it on. Images unused for 7 days (`--after`,
  `compress_idle_after`) are then compressed a little after `pull`, `load`, `build`,
  `rmi` and `prune`.
- `podbag system compress` compresses idle images, or the named images, at once.
  `podbag system decompress` decompresses them.
- A container or a build that uses a compressed image decompresses it first. Only that
  first start is slower (1.4 s for python, 1.8 s for Java, against 30 ms and 55 ms).
- `podbag system df` has a `Compressed` row. The Docker API adds `PodbagCompression` to
  `/images/json` with the size on disk and the size before.
- Podbag Desktop asks once after an install or an update, has a switch in Settings, and
  shows the size on disk and the space saved in the Images list.
- On macOS and Windows the setting is kept where the engine runs (the VM or podbag's WSL
  distribution).

### Kubernetes

- Create, reset and remove a private local Kind cluster from Podbag Desktop. On macOS
  it can use the Podbag VM. Desktop keeps the managed context separate from the user's
  existing kubeconfig.
- Show OpenShift Routes in Podbag Desktop, the TUI and `podbag kube`. They appear only
  when the context's API serves `route.openshift.io/v1`.
- In Podbag Desktop, the Routes tab shows each Route's host, path, Service and TLS mode.
  Select a host to open its address in the browser.
- `podbag kube migrate` does not copy Routes, because a Route's host belongs to its
  cluster.

### Networking

- Attach and detach networks on running containers. Set static addresses on bridge
  networks and use the embedded DNS service for container names.
- `podbag run` and `podbag create` take `--add-host NAME:IP`, `--dns`, `--dns-search` and
  `--dns-option`, as Docker does. `host-gateway` stands for the host.
- The Docker API applies `ExtraHosts`, `Dns`, `DnsSearch` and `DnsOptions`, and inspect
  shows them. Compose `extra_hosts` and `dns` now take effect.
- Where `host-gateway` has no address (rootless bridge, `--network none`, WSL), the CLI
  refuses it. The Docker API leaves the entry out with a warning.

### Builds

- Opt in to managed BuildKit on native Linux with `podbag build --buildkit` or enable it
  in the Docker API service for Buildx. The CLI supports build secrets, SSH agents,
  cache mounts and external cache, and local or multi-platform OCI exports.
- `podbag build` and `docker build` pull private base images with the login for each
  image's registry. Before, base images were pulled anonymously.
- The Docker API reads the `X-Registry-Config` header from the docker CLI and falls back
  to logins saved with `podbag login`. A malformed header gets a 400 reply.
- On macOS and Windows, `podbag build` sends the host's logins for the registries in the
  Dockerfile to the engine. A Dockerfile on stdin sends none.

### Container runtime

- Use Linux device mappings, DRM or NVIDIA GPU selection and read-only container root
  filesystems with the Podbag CLI and Docker API.
- Use NFS and trusted local Unix-socket volume drivers with a rootful Linux engine.
- Forward container logs to syslog or journald on Linux.
- Run Swarm service replicas on registered SSH workers and inspect their placement
  from the manager.

### macOS

- Configure shared folders, synchronized shares and the VM disk from Desktop Settings.
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
