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
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 5.2 / 5.9 | 4.8 / 6.2 | 47.7 / 53.6 | 69.7 / 76.6 | 8.9 / 9.5 |
| The same start when nothing of the tool runs yet, median (ms) | 17.0 | 15.3 | 47.7 (no daemon) | — (its daemon stays) | — (its daemon stays) |
| 50 sleeping containers, no network: memory the system holds, by category | 9.2 MiB | 8.4 MiB | 39.8 MiB | 215.2 MiB | 12.9 MiB |
| 50 sleeping containers, no network: the tool's processes (PSS) | 2.3 MiB (46 KiB per container) | 2.1 MiB (44 KiB per container) | 17.3 MiB (343 KiB per container) | 435.2 MiB (5064 KiB per container) | 10.6 MiB (96 KiB per container) |
| 50 sleeping containers, no network: change in MemTotal − MemAvailable (noisy, see below) | 12.9 MiB | 9.1 MiB | 44.0 MiB | 158.6 MiB | 1.7 MiB |
| 50 sleeping containers, default network: memory the system holds, by category | 27.0 MiB | 11.7 MiB | 789.8 MiB | 224.8 MiB | 15.9 MiB |
| 50 sleeping containers, default network: the tool's processes (PSS) | 17.1 MiB (349 KiB per container) | 2.1 MiB (44 KiB per container) | 729.3 MiB (14925 KiB per container) | 438.3 MiB (5126 KiB per container) | 10.7 MiB (97 KiB per container) |
| 50 sleeping containers, default network: change in MemTotal − MemAvailable (noisy, see below) | 30.1 MiB | within noise | 789.2 MiB | 174.6 MiB | 11.4 MiB |
| Tool processes with 50 containers: no network / default network | 1 / 3 | 1 / 1 | 51 / 101 | 54 / 54 | 2 / 2 |
| Tool processes with no containers: count, PSS | none | none | 1, 0.6 MiB | 4, 188.0 MiB | 1, 5.9 MiB |
| Disk: `python:3.12-slim-bookworm` | 156.7 MiB | 154.9 MiB | 158.7 MiB | 203.6 MiB | 201.0 MiB |
| Disk: then `python:3.12-slim-trixie` adds | 122.7 MiB | 120.9 MiB | 152.3 MiB | 195.5 MiB | 193.4 MiB |
| Disk: both images | 279.4 MiB | 275.8 MiB | 311.1 MiB | 399.1 MiB | 394.4 MiB |

- "Memory the system holds, by category" is the change in AnonPages + Shmem + Slab +
  KernelStack + PageTables + Percpu + VmallocUsed of `/proc/meminfo`: what processes and
  the kernel hold, without the page cache. Caches are dropped before each reading; the
  value is the median of three rounds; the rounds of one tool differ by at most
  4.1 MiB for openpod and Kiri (28.1 MiB for Podman and Docker). It includes
  the containers' own processes (the same 50 `sleep` for every tool) and the kernel's
  memory for their namespaces and cgroups, not daemons that already ran (Docker's), and
  not pages the kernel takes straight from its page allocator (they are in no category).
- "Change in MemTotal − MemAvailable" is the kernel's estimate of memory that is no longer
  available, read at the same moments. It cannot tell the small tools apart. The three
  rounds without a network gave, in MiB: openpod rootless 12.9, 15.5, 7.7; openpod as root
  12.0, 8.7, 9.1; Kiri 7.0, 1.7, 1.3. The sums by category of the same rounds:
  9.2, 8.7, 10.2; 8.3, 8.4, 8.7; 13.7, 12.7, 12.9. openpod's figures in this row are
  higher than Kiri's; no counter of the kernel says where that memory would be, and in
  a separate reading three identical rounds of Kiri moved free memory by −9.5, −17.3 and
  +6.4 MiB. The row stays for the large numbers.
- Both system-wide rows leave out the anonymous memory of the VM's own helper (lima's guest
  agent), which grew by 23 MiB during one run of 150 containers and shrank by 46 MiB during
  the next, and of the firmware updater that a timer starts; a Linux host has no guest agent.
- "The tool's processes": openpod's keeper (and a helper for each container that is
  attached or publishes ports), session holder and pasta; Podman's
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

## 300 containers on one network

`dev/bench/scale.sh`: `run -d --network bench --name cN alpine sleep`, one after another,
on a network made with `network create --subnet 10.90.0.0/22 bench`; same session as the
table above.

| | openpod (rootless) | openpod (root) | Kiri (root daemon) |
|---|---|---|---|
| Start, containers 1–10 (median) | 10.0 ms | 15.0 ms | 15.2 ms |
| Start, containers 41–50 (median) | 10.7 ms | 12.5 ms | 16.0 ms |
| Start, the last 10 (median) | 28.6 ms | 34.9 ms | 21.4 ms |
| Memory at 50: the system holds, by category / the tool's processes (PSS) | 28.2 MiB / 17.1 MiB | 8.6 MiB / 2.1 MiB | 14.1 MiB / 10.3 MiB |
| Memory at 300: the system holds, by category / the tool's processes (PSS) | 199.9 MiB / 17.3 MiB | 168.8 MiB / 2.3 MiB | 201.6 MiB / 20.5 MiB |
| Tool processes at 300 | 3 | 1 | 2 |
| `ps -a` with 300 containers | 35 ms | 68 ms | 12 ms |
| `rm -f` of all 300 | 3688 ms | 4127 ms | 1882 ms |

## Reading the numbers

**Memory per container (Linux).** A detached openpod container has no process of its own:
one keeper process (2.1–2.3 MiB, PSS) watches all of them and leaves 10 seconds after the
last one, so with no containers openpod runs nothing. Spread over 50 containers the keeper
is 44–46 KiB each; over 300 it is 8 KiB each. Kiri keeps one daemon running (5.9 MiB) and
one init process that all its containers share; each of the first 50 containers adds about
96 KiB to the daemon, which also keeps a thread for it. For 50 containers without a network
the whole system holds 9.2 MiB (openpod rootless), 8.4 MiB (openpod as root) and 12.9 MiB
(Kiri). Docker keeps dockerd and containerd running (188 MiB with no containers) and grows
by about 5.1 MiB per container. Podman adds a conmon process of about 0.3 MiB per container.

**The keeper against a shim per container.** `OPENPOD_KEEPER=0` gives every container its
own shim process, as openpod did before; it was measured in the same session. With shims,
openpod's processes for 50 containers take 7.2 MiB instead of 2.3 MiB, and the system holds
23.6 MiB instead of 9.2 MiB (rootless; as root 16.9 instead of 8.4 MiB). A rootless start
takes 12.3 ms instead of 5.2 ms: under the keeper a container's cgroup is made below the
keeper's systemd scope, without a `systemd-run` for each container, and a container that
left only the user's own files is removed without a second user namespace. As root the
start time is the same with and without the keeper (4.8 and 5.1 ms). The price of the
keeper: it is the parent of the containers it watches, so if it is killed they end with it.

**Where openpod is behind Kiri.** In the root column, which is the like-for-like comparison
(Kiri runs as a root daemon), openpod is ahead in every row of the first table except the
two noisy ones. It is behind in these places:

- *The first start.* When no keeper runs (the first container, or the first after 10
  seconds without one) a start takes 15–17 ms, against Kiri's 8.9 ms: Kiri's daemon is
  always there, which is also why it holds 5.9 MiB with no containers.
- *Rootless on a bridge network.* All bridge containers of a user go through one pasta
  process (14.6 MiB from its start) and one process that holds their network namespace
  (1 MiB): a fixed cost per user, whatever the number of containers. With 50 containers on
  the default network openpod's processes take 17.1 MiB against Kiri's 10.7 MiB, the system
  holds 27.0 MiB against 15.9 MiB, and openpod has three processes against two. Kiri's
  processes grow by about 41 KiB with each further container (10.3 MiB at 50, 20.5 MiB at
  300, on one network) and openpod's do not (17.1 and 17.3 MiB), so the two meet near 220
  containers.
- *Many containers on one network.* Starting is as fast as Kiri's or faster for the first
  50 (10–15 ms against 15–16 ms) but grows with the number of containers on the network:
  29–35 ms for the last ten of 300 against Kiri's 21 ms, because each start and each end
  rewrites the hosts file of every container on the network (for names). `ps -a` takes
  35–68 ms (openpod reads each container's record; Kiri's daemon has the list in memory and
  answers in 12 ms), and removing all 300 takes about 4 s (one container after another)
  against 1.9 s. These rows belong to the network and CLI code and are the subject of the
  next milestone.
- *Free memory.* In the "MemTotal − MemAvailable" rows openpod's figures are higher than
  Kiri's. The rounds printed below the table show how far that estimate moves between
  identical rounds; the sum by category, which counts what is allocated, has openpod lower.

**Networks.** As root, openpod, Docker and Kiri use a kernel bridge and no extra process:
openpod's keeper takes 2.1 MiB and the system holds 11.7 MiB for 50 containers on the
default network, against 10.7 MiB and 15.9 MiB for Kiri. Podman (rootless) starts one pasta
per container: 50 containers on the default network take about 730 MiB.

**Start time.** The table times a whole `run --rm` cycle: start, wait for the exit, remove.
With its keeper running, openpod does it in 5.2 ms rootless and 4.8 ms as root. Kiri takes
8.9 ms through the docker CLI, which itself is a separate process start; Kiri's own figure
through the API alone was about 2.6 ms in an earlier session. Podman takes about 48 ms and
Docker about 70 ms.

**composefs and hard links.** As root, openpod mounts images with composefs (one EROFS
metadata file per image, shared by all its containers, with the file contents read from the
object store); `OPENPOD_IMAGE_MOUNT=hardlink` measured the hard-link trees in the same
session: a start takes 4.8 ms with composefs and 4.5 ms with hard links (the check for the
shared mount, its lock, and an overlay with a data-only layer), the memory is the same
(8.4 and 8.2 MiB held for 50 containers), and the store is slightly smaller (154.9 against
156.7 MiB for the first Python image), since one metadata file replaces a directory tree.
What composefs saves is the work at pull time (no tree is written) and the tree's inodes.

**Disk.** openpod stores each distinct file once, across images. The two Python images have
different Debian bases, so layer-level sharing finds nothing in common, while openpod shares
the files that are the same in both: the second image adds 121–123 MiB to openpod's store,
152 MiB to Podman's, and 193–196 MiB to Docker's and Kiri's, which also keep each layer's
compressed download next to the unpacked files. Kiri unpacks a layer when a container first
uses it, and deletes the layers of removed images only when it next creates a container;
that is why every store holds alpine and each image is used once before it is measured.

**Many containers.** With 300 containers on one user network, openpod's own processes use
17.3 MiB rootless (14.6 MiB of that is pasta) and 2.3 MiB as root; Kiri's use 20.5 MiB. The
system holds 200 MiB (openpod rootless), 169 MiB (openpod as root) and 202 MiB (Kiri), most
of it the kernel's memory for each container's namespaces and the containers' own
processes. The rows where openpod is behind are listed above. A /24 network holds 253
containers; use `--subnet` for more.

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

**Caveats.** The Linux tables come from one VM on a laptop, all tools one after another in
one session; the session was run four times on the same day (the published one last, with
the final program), and start times, PSS and the sum by category came out within about
1 ms and 3 MiB of each other for openpod and Kiri. An earlier session on the same VM
(milestone M7) gave openpod and Podman start times 4–6 times higher while Docker's stayed
at 71 ms; the cause was not found, so numbers from different days are not compared here.
Docker and Kiri run as root daemons and Podman rootless, each as it installs; openpod is
shown in both modes. The disk rows of openpod as root were measured again after the
session (a pull failed on a closed connection to the registry); they do not depend on
timing. The macOS table comes from one session on the Mac (10 CPUs, 24 GiB), with another
project's VM running at the same time; it was measured before openpod had the keeper and
is not repeated here.
