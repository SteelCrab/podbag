# Benchmarks

Two comparisons, written by `dev/bench/report.sh`: container engines on one Linux host
(no VM; the same kernel for all), and the tools Mac users run, each with its own Linux VM.
Numbers compare only within one table: each table was measured in one session.

## Linux host, no VM

Measured with `dev/bench/run.sh` and `dev/bench/scale.sh`, one tool after another in the
same VM: Ubuntu 26.04.1 LTS, kernel 7.0.0-43-generic, aarch64,
4 CPUs, 3.8 GiB of memory (lima on macOS).
OrbStack has no Linux version; Docker Desktop runs this Docker Engine in its VM.

| | openpod (rootless) | openpod (root) | Podman (rootless) | Docker (root daemon) | Kiri (root daemon) |
|---|---|---|---|---|---|
| Version | openpod 0.1.0 | openpod 0.1.0 (root, composefs) | podman version 5.7.0 (rootless) | Docker version 29.1.3 (root daemon) | Kiri 0.2.1 (root daemon) |
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 13.6 / 19.1 | 5.5 / 6.8 | 50.1 / 58.7 | 71.2 / 78.1 | 9.4 / 10.4 |
| 50 sleeping containers, no network: change in the system's used memory | 29.6 MiB | 39.1 MiB | 49.2 MiB | 140.8 MiB | 14.6 MiB |
| 50 sleeping containers, no network: the tool's processes (PSS) | 6.9 MiB (142 KiB per container) | 6.9 MiB (140 KiB per container) | 26.7 MiB (264 KiB per container) | 410.6 MiB (4844 KiB per container) | 10.7 MiB (97 KiB per container) |
| 50 sleeping containers, default network: change in the system's used memory | 22.8 MiB | 14.3 MiB | 818.2 MiB | 190.1 MiB | within noise |
| 50 sleeping containers, default network: the tool's processes (PSS) | 21.6 MiB (443 KiB per container) | 6.8 MiB (140 KiB per container) | 742.4 MiB (14921 KiB per container) | 425.7 MiB (5154 KiB per container) | 10.7 MiB (98 KiB per container) |
| Tool processes with 50 containers: no network / default network | 50 / 52 | 50 / 50 | 52 / 102 | 54 / 54 | 2 / 2 |
| Tool processes with no containers: count, PSS | none | none | 2, 13.8 MiB | 4, 174.1 MiB | 1, 5.9 MiB |
| Disk: `python:3.12-slim-bookworm` | 156.7 MiB | 154.9 MiB | 158.7 MiB | 203.6 MiB | 201.0 MiB |
| Disk: then `python:3.12-slim-trixie` adds | 122.7 MiB | 120.9 MiB | 152.3 MiB | 195.5 MiB | 193.4 MiB |
| Disk: both images | 279.4 MiB | 275.8 MiB | 311.1 MiB | 399.1 MiB | 394.4 MiB |

- "Change in the system's used memory" is MemTotal − MemAvailable after minus before (caches
  dropped), the median of three rounds. It includes the containers' own processes and
  the kernel's memory for namespaces and cgroups; it moves by 10–20 MiB between runs. It does not
  include daemons that were already running (Docker's).
- "The tool's processes": openpod's shims, session holder and pasta; Podman's
  conmon, pasta and helpers; Docker's dockerd, containerd and shims; Kiri's daemon and
  the one init process all its containers share. PSS divides shared
  pages among the processes that share them. "Per container" is (total − the tool's
  processes with no containers) / 50: a container's own helper plus any growth of the
  daemons. The two memory rows are measured differently and are not parts of each other:
  for Docker, the PSS row counts dockerd and containerd, the system row does not.
- Disk is what the images add to `du` of the tool's whole store (hard links counted once),
  each image pulled and then used by one container; the store already holds alpine.

## macOS, each tool with its own VM

Measured with `dev/bench/mac.py` from the Mac: Apple M5,
10 CPUs, 24 GiB,
macOS 27.2.1, 2026-11-18. One tool ran at a time; the others were stopped.

| | Docker Desktop | OrbStack | Kiri | Podman | Colima | openpod (lima VM) |
|---|---|---|---|---|---|---|
| Version | Docker Desktop 4.97.0 | OrbStack 2.2.5 | Kiri 0.2.1 | Podman 6.2.0 | colima version 0.10.3 | openpod 0.1.0 |
| VM size | 10 CPUs, 7.75 GiB | 10 CPUs, 11.73 GiB | 10 CPUs, 8.00 GiB | 10 CPUs, 7.72 GiB | 10 CPUs, 7.74 GiB | 10 CPUs, 7.73 GiB |
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 97.4 / 105.4 | 90.4 / 96.4 | 11.8 / 12.3 | 98.9 / 111.2 | 73.6 / 93.2 | 14.8 / 15.9 |
| First `run` after 6 minutes without containers (ms) | 1513.2 | 231.0 | 301.6 | 186.9 | 213.2 | 22.4 |
| Memory on the Mac (the tool's processes, VM included): just started | 5.71 GiB | 839 MiB | 6.5 MiB | 2.13 GiB | 1.12 GiB | 1.81 GiB |
| … with 50 sleeping containers | 6.61 GiB | 1.16 GiB | 322 MiB | 2.31 GiB | 1.46 GiB | 1.83 GiB |
| … 30 s after they are removed | 6.70 GiB | 961 MiB | 325 MiB | 2.34 GiB | 1.54 GiB | 1.83 GiB |
| … 6 minutes after they are removed | 950 MiB | 795 MiB | 12 MiB | 2.34 GiB | 1.54 GiB | 1.86 GiB |
| Of that, the VM: 50 containers / 6 minutes after | 2.59 GiB / VM stopped | 1.09 GiB / 722 MiB | 306 MiB / VM stopped | 2.28 GiB / 2.31 GiB | 1.36 GiB / 1.44 GiB | 1.77 GiB / 1.77 GiB |
| Processes on the Mac: just started / 50 containers | 12 / 12 | 2 / 2 | 1 / 2 | 2 / 2 | 4 / 4 | 4 / 4 |

- Every VM was set to 10 CPUs and 8 GiB, Docker Desktop's default here (the script
  cannot change Docker Desktop's settings); OrbStack's VM reports more than the 8 GiB
  it was limited to. The size is an upper limit: the memory rows show what each VM
  takes from the Mac.
- Docker Desktop, OrbStack, Kiri and Colima were driven by the same docker CLI through
  their sockets, Podman by its own CLI. openpod has no macOS front end yet (design 7):
  it runs in lima's Ubuntu VM and its CLI is reached over ssh, which alone takes
  5.7 ms of each start; its memory is mostly that Ubuntu system.
- "Memory on the Mac" is the sum of macOS physical footprint (`footprint`) of the
  tool's app, helpers and VM process; root helpers by resident size. Other VMs on the Mac
  are not counted (another project's VM ran during the measurements).
- Containers on the default network; the script removes only the containers it labels,
  so Docker Desktop's existing images and containers stayed. For that reason disk use is
  compared in the Linux table only.

## 300 containers on one network (openpod, rootless)

`dev/bench/scale.sh`: `run -d --network bench --name cN alpine sleep`, one after another.

| | |
|---|---|
| Start, containers 1–10 (median) | 25.3 ms |
| Start, containers 41–50 (median) | 23.6 ms |
| Start, the last 10 (median) | 43.7 ms |
| Memory at 50: change in the system's used memory / openpod's processes (PSS) | 52.5 MiB / 21.9 MiB |
| Memory at 300: change in the system's used memory / openpod's processes (PSS) | 398.1 MiB / 53.5 MiB |
| `ps -a` with 300 containers | 65 ms |
| `rm -f` of all 300 | 4267 ms |

## Reading the numbers

**Memory per container (Linux).** An openpod container costs one shim process of about
140 KiB (PSS) next to the container's own processes, and with no containers openpod runs
nothing. Kiri keeps one daemon running (about 6 MiB) and starts one init process that all
its containers share, so a container adds only about 97 KiB: past roughly 140 containers
Kiri's total would be the smaller one (a straight-line estimate; Kiri was measured with 50).
Docker keeps dockerd and containerd running (174 MiB with no containers) and grows by about
4.8 MiB per container. Podman adds a conmon process of about 0.26 MiB per container and keeps
a pause process and a helper (14 MiB) after first use.

**Networks.** Rootless openpod connects all bridge containers of a user through one pasta
process: 50 containers on the default network cost about 15 MiB more than 50 without a
network. Podman (rootless) starts one pasta per container: 50 containers on the default
network take about 740 MiB. As root, openpod, Docker and Kiri use a kernel bridge and no
extra process.

**Start time.** The table times a whole `run --rm` cycle: start, wait for the exit, remove.
openpod does it in 5.5 ms as root and 13.6 ms rootless (that includes the systemd scope a
rootless container gets for limits and the memory budget). Kiri takes 9.4 ms through the
docker CLI, which itself is a separate process start; Kiri's own figure through the API alone
is about 2.6 ms. Podman takes about 50 ms and Docker about 71 ms.

**composefs and hard links.** As root, openpod mounts images with composefs (one EROFS
metadata file per image, shared by all its containers, with the file contents read from the
object store); `OPENPOD_IMAGE_MOUNT=hardlink` measured the hard-link trees in the same
session: a start takes about 1 ms more with composefs (5.5 against 4.3 ms: the check for the
shared mount, its lock, and an overlay with a data-only layer), memory is the same, and the
store is slightly smaller (154.9 against 156.7 MiB for the first Python image), since one
metadata file replaces a directory tree. What composefs saves is the work at pull time (no
tree is written) and the tree's inodes.

**Disk.** openpod stores each distinct file once, across images. The two Python images have
different Debian bases, so layer-level sharing finds nothing in common, while openpod shares
the files that are the same in both: the second image adds 121–123 MiB to openpod's store,
152 MiB to Podman's, and 194–196 MiB to Docker's and Kiri's, which also keep each layer's
compressed download next to the unpacked files. Kiri unpacks a layer when a container first
uses it, and deletes the layers of removed images only when it next creates a container;
that is why every store holds alpine and each image is used once before it is measured.

**Many containers.** With 300 rootless containers on one user network, starting stays fast
but grows with the number of containers on that network: about 25 ms for the first ten and
44 ms for the last ten, because each start rewrites the hosts file of every container on the
network (for names). openpod's own processes for all 300 use 54 MiB (PSS). The whole system
uses about 400 MiB more, most of it the kernel's memory for each container's namespaces and
the containers' own processes. `ps -a` takes 65 ms and removing all 300 takes 4.3 s. A /24
network holds 253 containers; use `--subnet` for more.

**macOS.** Each tool here runs Linux in its own VM, and what the VM takes is what the Mac
loses ("Memory on the Mac" is what Activity Monitor shows for the tool's processes). Kiri
boots its VM only for the first container and lets it sleep a minute after the last one, so
with no containers it costs only its daemon (7–12 MiB), and 50 containers took 0.32 GiB.
OrbStack keeps its VM running and gives memory back while idle (1.16 GiB with 50 containers,
0.8 GiB six minutes after). Docker Desktop's Resource Saver stops its VM five minutes after
the last container: the memory comes back, but the next `run` waits 1.5 s for the VM. 3.4 GiB
of Docker Desktop's figure is the heap of its backend process, which compresses well: the
Mac's used memory (active, wired and compressed pages) rose by 1.6 GiB when it started.
Colima's and Podman's VMs keep what they once used (1.5 and 2.3 GiB). openpod has no macOS
app yet: in lima's Ubuntu VM it takes 1.8 GiB, nearly all of it the Ubuntu system, while 50
containers add only 16 MiB (0.3 MiB each, the least of all); openpod's own macOS VM (design
7) is meant to start small and give memory back, as Kiri and OrbStack do. Starting from the
Mac: Kiri 11.8 ms; openpod 14.8 ms, of which 5.7 ms is ssh into the VM; Colima 74 ms,
OrbStack 90 ms, Docker Desktop 97 ms and Podman 99 ms, each through its CLI and the socket
forwarding into its VM.

**Caveats.** The Linux table comes from one VM on a laptop, all tools one after another in
one session; numbers move by a few percent between runs, and the system-wide memory rows by
10–20 MiB (root openpod's came out at 39 MiB with composefs and 20 MiB with hard links
without a network, and the other way round with one), so the PSS rows are the ones to
compare. An earlier session on the same VM (milestone M7) gave openpod and Podman start
times 4–6 times higher while Docker's stayed at 71 ms; the cause was not found, so numbers
from different sessions are not compared here. Docker and Kiri run as root and Podman
rootless, each in its default mode; openpod is shown in both. The macOS table comes from one
session on the Mac (10 CPUs, 24 GiB), with another project's VM running at the same time.
