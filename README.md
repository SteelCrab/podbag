# openpod

A daemonless, rootless container engine that runs Docker/OCI images from a single static binary.

## Status

Early development (milestone M3). What works:

```sh
openpod run alpine echo hello            # pulls the image if needed; the container stays
openpod run --rm -it alpine sh           # interactive shell, removed on exit
openpod run -d --name web nginx:alpine   # in the background
openpod ps -a
openpod logs -f web
openpod exec -it web sh
openpod stop web && openpod rm web
openpod pull alpine | openpod images | openpod rmi alpine
openpod system df | openpod system prune -f
openpod run --rootfs ./rootfs sh         # a plain directory instead of an image
```

There is no daemon: each container has a small `openpod __shim` process that keeps its
logs and answers `stop`, `kill` and `wait`. Containers run in new mount, PID, UTS and IPC
namespaces, plus a user namespace when run without root. Identical files in different
images are stored once and share page cache. The store keeps at most 10 GiB by default.

Not yet: resource limits, volumes and networking.

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

If you move openpod to another path, update the profile.

After this one-time step, openpod runs without sudo.

