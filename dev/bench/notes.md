## Reading the numbers

**Memory per container is where openpod differs most.** An openpod container costs one
shim process of about 140 KiB (PSS) next to the container's own processes. There is no
daemon: with no containers, openpod runs nothing. Docker keeps dockerd and containerd
running (about 174 MiB with no containers) and grows by about 5 MiB per container (its
containerd shim, and dockerd and containerd themselves). Podman adds a conmon process of about 0.3 MiB per container and keeps a pause
process and a helper (12 MiB) after first use.

**Networks.** Rootless openpod connects all bridge containers of a user through one pasta
process, so 50 containers on the default network cost about 15 MiB more than 50 without
a network. Podman (rootless) starts one pasta per container, about 14 MiB each: 50
containers on the default network use about 800 MiB. As root, openpod uses a kernel
bridge and no extra process at all.

**Start time.** The table times a whole `run --rm` cycle: start, wait for the exit,
remove. As root, openpod does it in about 19 ms. Rootless it takes about 88 ms, and more
than half of that is the systemd scope each container gets for limits and the memory
budget: one `systemd-run --user --scope` call to create it, and systemd's work to remove
it at the exit. Without the scope the same cycle takes about 36 ms; replacing the
per-container scope with one delegated scope per user is a planned change. Starting
alone (`run -d`, below) takes 20–25 ms rootless. Docker's daemon has its state ready and
runs the cycle in about 72 ms; Podman takes about 300 ms.

**Disk.** openpod stores each distinct file once, across images. The two Python images
have different Debian bases, so layer-level sharing (Docker, Podman) finds nothing in
common, while openpod shares the files that are the same in both: the second image adds
123 MiB to openpod's store against 152 MiB for Podman. Docker with its containerd image
store also keeps each layer's compressed download, so it needs the most space.

**Many containers.** With 300 rootless containers on one user network, starting stays
fast but grows with the number of containers on that network: about 25 ms for the first
ten and 44 ms for the last ten, because each start rewrites the hosts file of every
container on the network (for names). openpod's own processes for all 300 use 54 MiB
(PSS). The whole system uses about 363 MiB more, most of it the kernel's memory for each
container's namespaces and the containers' own processes. `ps -a` takes 46 ms and
removing all 300 takes 3.7 s. A /24 network holds 253 containers; use `--subnet` for more.

**Caveats.** One VM on a laptop, measured one tool after another; Docker's very first
run after installation was much slower (335 ms) and is not shown; numbers move by a few
percent between runs, and the system-wide memory column by 10–20 MiB: openpod's
system-wide figures are within that noise of each other (as root, the default network's
change even came out below zero, shown as "within noise", while its PSS row shows the
6.9 MiB of shims), so the PSS rows are the ones to compare. Docker runs as root
and Podman rootless here, each in its default mode; openpod is shown in both.
