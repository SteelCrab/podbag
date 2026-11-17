# Benchmarks

Measured with `dev/bench/run.sh` and `dev/bench/scale.sh`; this file is written by
`dev/bench/report.sh`. All tools ran one after another in the same VM:
Ubuntu 26.04.1 LTS, kernel 7.0.0-43-generic, aarch64,
4 CPUs, 3.8 GiB of memory (lima on macOS).

| | openpod (rootless) | openpod (root) | Podman (rootless) | Docker (root daemon) |
|---|---|---|---|---|
| Version | openpod 0.1.0 | openpod 0.1.0 (root) | podman version 5.7.0 (rootless) | Docker version 29.1.3 (root daemon) |
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 88.5 / 138.1 | 18.7 / 24.7 | 298.3 / 367.6 | 71.5 / 77.0 |
| 50 sleeping containers, no network: change in the system's used memory | 21.6 MiB | 15.3 MiB | 39.3 MiB | 184.9 MiB |
| 50 sleeping containers, no network: the tool's processes (PSS) | 6.9 MiB (141 KiB per container) | 6.6 MiB (136 KiB per container) | 25.1 MiB (262 KiB per container) | 424.1 MiB (5129 KiB per container) |
| 50 sleeping containers, default network: change in the system's used memory | 45.7 MiB | within noise | 794.7 MiB | 175.8 MiB |
| 50 sleeping containers, default network: the tool's processes (PSS) | 21.7 MiB (445 KiB per container) | 6.9 MiB (141 KiB per container) | 740.9 MiB (14922 KiB per container) | 422.6 MiB (5098 KiB per container) |
| Tool processes with 50 containers: no network / default network | 50 / 52 | 50 / 50 | 52 / 102 | 54 / 54 |
| Tool processes with no containers: count, PSS | none | none | 2, 12.3 MiB | 4, 173.7 MiB |
| Disk: `python:3.12-slim-bookworm` | 157.4 MiB | 157.4 MiB | 158.7 MiB | 203.6 MiB |
| Disk: then `python:3.12-slim-trixie` adds | 122.7 MiB | 122.7 MiB | 152.3 MiB | 195.5 MiB |
| Disk: both images | 280.2 MiB | 280.2 MiB | 311.1 MiB | 399.1 MiB |

- "Change in the system's used memory" is MemTotal − MemAvailable after minus before (caches
  dropped), the median of three rounds. It includes the containers' own processes and
  the kernel's memory for namespaces and cgroups; it moves by 10–20 MiB between runs. It does not
  include daemons that were already running (Docker's).
- "The tool's processes": openpod's shims, session holder and pasta; Podman's
  conmon, pasta and helpers; Docker's dockerd, containerd and shims. PSS divides shared
  pages among the processes that share them. "Per container" is (total − the tool's
  processes with no containers) / 50: a container's own helper plus any growth of the
  daemons. The two memory rows are measured differently and are not parts of each other:
  for Docker, the PSS row counts dockerd and containerd, the system row does not.
- Disk is `du` of the tool's whole store (hard links counted once).

## 300 containers on one network (openpod, rootless)

`dev/bench/scale.sh`: `run -d --network bench --name cN alpine sleep`, one after another.

| | |
|---|---|
| Start, containers 1–10 (median) | 25.0 ms |
| Start, containers 41–50 (median) | 21.7 ms |
| Start, the last 10 (median) | 43.6 ms |
| Memory at 50: change in the system's used memory / openpod's processes (PSS) | 52.8 MiB / 21.9 MiB |
| Memory at 300: change in the system's used memory / openpod's processes (PSS) | 363.0 MiB / 53.5 MiB |
| `ps -a` with 300 containers | 46 ms |
| `rm -f` of all 300 | 3715 ms |

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
