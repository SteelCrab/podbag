# Benchmarks

- Two comparisons:
  - container engines on one Linux host (no VM; the same kernel for all),
  - the tools Mac users run, each with its own Linux VM.
- Numbers compare only within one table: each table was measured in one session.
- A node of Spring microservices in two releases: [benchmarks-msa.md](benchmarks-msa.md).
- A Kubernetes node with podbag, containerd or CRI-O as its runtime: [benchmarks-k8s.md](benchmarks-k8s.md).
- `dev/bench/report.sh` writes this page.

## Contents

- [Linux host, no VM](#linux-host-no-vm)
- [macOS, each tool with its own VM](#macos-each-tool-with-its-own-vm)
- [300 containers on one network](#300-containers-on-one-network)
- [Reading the numbers](#reading-the-numbers)
  - [Memory per container (Linux)](#memory-per-container-linux)
  - [The keeper against a shim per container](#the-keeper-against-a-shim-per-container)
  - [Where podbag is behind Kiri](#where-podbag-is-behind-kiri)
  - [Networks](#networks)
  - [Start time](#start-time)
  - [composefs and hard links](#composefs-and-hard-links)
  - [Disk](#disk)
  - [Many containers](#many-containers)
  - [The state of the system](#the-state-of-the-system)
  - [macOS](#macos)
  - [Caveats](#caveats)

## Linux host, no VM

Measured with `dev/bench/run.sh` and `dev/bench/scale.sh`, one tool after another in the
same VM: Ubuntu 26.04.1 LTS, kernel 7.0.0-43-generic, aarch64,
4 CPUs, 3.8 GiB of memory (lima on macOS).
OrbStack has no Linux version; Docker Desktop runs this Docker Engine in its VM.

| | podbag (rootless) | podbag (root) | Podman (rootless) | Docker (root daemon) | Kiri (root daemon) |
|---|---|---|---|---|---|
| Version | podbag 0.1.0 | podbag 0.1.0 (root, composefs) | podman version 5.7.0 (rootless) | Docker version 29.1.3 (root daemon) | Kiri 0.2.1 (root daemon) |
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 4.1 / 4.5 | 4.4 / 4.8 | 46.4 / 53.2 | 73.8 / 89.6 | 9.1 / 10.3 |
| The same start when nothing of the tool runs yet, median (ms) | 12.8 | 16.0 | 46.4 (no daemon) | — (its daemon stays) | — (its daemon stays) |
| 50 sleeping containers, no network: memory the system holds, by category | 8.7 MiB | 9.1 MiB | 39.7 MiB | 211.6 MiB | 13.9 MiB |
| 50 sleeping containers, no network: the tool's processes (PSS) | 2.3 MiB (48 KiB per container) | 2.2 MiB (45 KiB per container) | 13.3 MiB (261 KiB per container) | 434.0 MiB (4724 KiB per container) | 10.6 MiB (96 KiB per container) |
| 50 sleeping containers, no network: change in MemTotal − MemAvailable (noisy, see below) | 5.8 MiB | 6.9 MiB | 59.9 MiB | 202.8 MiB | 21.2 MiB |
| 50 sleeping containers, default network: memory the system holds, by category | 28.5 MiB | 12.9 MiB | 796.3 MiB | 235.0 MiB | 18.4 MiB |
| 50 sleeping containers, default network: the tool's processes (PSS) | 17.1 MiB (351 KiB per container) | 2.2 MiB (45 KiB per container) | 729.1 MiB (14921 KiB per container) | 453.0 MiB (5113 KiB per container) | 10.7 MiB (98 KiB per container) |
| 50 sleeping containers, default network: change in MemTotal − MemAvailable (noisy, see below) | 21.2 MiB | 19.1 MiB | 791.8 MiB | 208.1 MiB | 15.9 MiB |
| Tool processes with 50 containers: no network / default network | 1 / 3 | 1 / 1 | 51 / 101 | 54 / 54 | 2 / 2 |
| Tool processes with no containers: count, PSS | none | none | 1, 0.6 MiB | 4, 203.3 MiB | 1, 5.9 MiB |
| Disk: `python:3.12-slim-bookworm` | 156.7 MiB | 154.9 MiB | 158.7 MiB | 203.6 MiB | 201.0 MiB |
| Disk: then `python:3.12-slim-trixie` adds | 122.7 MiB | 120.9 MiB | 152.3 MiB | 195.5 MiB | 193.4 MiB |
| Disk: both images | 279.4 MiB | 275.8 MiB | 311.1 MiB | 399.1 MiB | 394.4 MiB |

- "Memory the system holds, by category" is the change in AnonPages + Shmem + Slab +
  KernelStack + PageTables + Percpu + VmallocUsed of `/proc/meminfo`: what processes and
  the kernel hold, without the page cache. Caches are dropped before each reading; the
  value is the median of three rounds; the rounds of one tool differ by at most
  3.5 MiB for podbag and Kiri (28.1 MiB for Podman and Docker). It includes
  the containers' own processes (the same 50 `sleep` for every tool) and the kernel's
  memory for their namespaces and cgroups, not daemons that already ran (Docker's), and
  not pages the kernel takes straight from its page allocator (they are in no category).
- "Change in MemTotal − MemAvailable" is the kernel's estimate of memory that is no longer
  available, read at the same moments. It cannot tell the small tools apart. The three
  rounds without a network gave, in MiB: podbag rootless 5.8, -1.5, 7.5; podbag as root
  -2.8, 14.9, 6.9; Kiri 1.2, 38.5, 21.2. The sums by category of the same rounds:
  9.9, 8.0, 8.7; 9.5, 9.1, 8.6; 15.1, 13.9, 11.9. Where this row and the sum
  by category disagree, no counter of the kernel says where the memory would be; in a
  separate reading three identical rounds of Kiri moved free memory by −9.5, −17.3 and
  +6.4 MiB. The row stays for the large numbers.
- lima's guest agent was stopped for the session (`dev/bench/session.sh`): it grew by
  23 MiB during one run of 150 containers, shrank by 46 MiB during the next, and took a
  fifth of the CPU time while containers started. A Linux host has no guest agent. Both
  system-wide rows also leave its anonymous memory out, should it run.
- "The tool's processes": podbag's keeper (and a helper for each container that is
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
macOS 27.2.1. The other tools on 2026-11-18, one at a time with the others stopped;
podbag on 2026-11-19, while Docker Desktop's app (its VM stopped) and the lima VM podbag is developed in ran.

| | Docker Desktop | OrbStack | Kiri | Podman | Colima | podbag | podbag, default size |
|---|---|---|---|---|---|---|---|
| Version | Docker Desktop 4.97.0 | OrbStack 2.2.5 | Kiri 0.2.1 | Podman 6.2.0 | colima version 0.10.3 | podbag 0.1.0 | podbag 0.1.0 |
| VM size | 10 CPUs, 7.75 GiB | 10 CPUs, 11.73 GiB | 10 CPUs, 8.00 GiB | 10 CPUs, 7.72 GiB | 10 CPUs, 7.74 GiB | 10 CPUs, 7.82 GiB | 8 CPUs, 1.94 GiB |
| Start: `run --rm --network none alpine true`, median / p90 (ms, 100 runs) | 97.4 / 105.4 | 90.4 / 96.4 | 11.8 / 12.3 | 98.9 / 111.2 | 73.6 / 93.2 | 14.4 / 15.2 | 15.0 / 20.3 |
| … through podbag's own CLI (`podbag run`), median / p90 (ms) | — | — | — | — | — | 7.3 / 7.8 | 8.0 / 8.8 |
| First `run` after 6 minutes without containers (ms) | 1513.2 | 231.0 | 301.6 | 186.9 | 213.2 | 599.4 | 637.9 |
| Memory on the Mac (the tool's processes, VM included): just started | 5.71 GiB | 839 MiB | 6.5 MiB | 2.13 GiB | 1.12 GiB | 284 MiB | 132 MiB |
| … with 50 sleeping containers | 6.61 GiB | 1.16 GiB | 322 MiB | 2.31 GiB | 1.46 GiB | 359 MiB | 199 MiB |
| … 30 s after they are removed | 6.70 GiB | 961 MiB | 325 MiB | 2.34 GiB | 1.54 GiB | 360 MiB | 200 MiB |
| … 6 minutes after they are removed | 950 MiB | 795 MiB | 12 MiB | 2.34 GiB | 1.54 GiB | 1.7 MiB | 1.8 MiB |
| Of that, the VM: 50 containers / 6 minutes after | 2.59 GiB / VM stopped | 1.09 GiB / 722 MiB | 306 MiB / VM stopped | 2.28 GiB / 2.31 GiB | 1.36 GiB / 1.44 GiB | 349 MiB / VM stopped | 190 MiB / VM stopped |
| Processes on the Mac: just started / 50 containers | 12 / 12 | 2 / 2 | 1 / 2 | 2 / 2 | 4 / 4 | 2 / 2 | 2 / 2 |

- Every VM was set to 10 CPUs and 8 GiB, Docker Desktop's default here (the script
  cannot change Docker Desktop's settings); OrbStack's VM reports more than the 8 GiB
  it was limited to. The size is an upper limit: the memory rows show what each VM
  takes from the Mac. podbag is shown at that size and at its own default (the
  Mac's CPUs up to 8, 2 GiB): what its VM holds grows with the size, as the guest
  kernel sets up its bookkeeping for all of the VM's memory when it boots.
- Docker Desktop, OrbStack, Kiri, Colima and podbag were driven by the same docker
  CLI through their sockets, Podman by its own CLI. podbag runs a VM of its own
  (Virtualization.framework, Linux 6.18 with podbag as its init); it powers the VM
  off 60 s after the last container and leaves a small standby process, which starts
  the VM again for the next client.
- "Memory on the Mac" is the sum of macOS physical footprint (`footprint`) of the
  tool's app, helpers and VM process; root helpers by resident size. Other VMs on the Mac
  are not counted (another project's VM ran during the measurements).
- Containers on the default network; the script removes only the containers it labels
  (podbag's are in a home of its own), so Docker Desktop's existing images and
  containers stayed. For that reason disk use is compared in the Linux table only.

## 300 containers on one network

`dev/bench/scale.sh`: `run -d --network bench --name cN alpine sleep`, one after another,
on a network made with `network create --subnet 10.90.0.0/22 bench`; the same program and
VM as the table above. `rm -f` gets all 300 containers on one command line.

Every tool was run 3 times, in turn, after one round that is not counted
(see "The state of the system" below); each figure is the median of its
3 readings. The single runs of three rows, in the order of the columns:

- what the system holds at 300, MiB: 192, 187, 189; 169, 162, 167; 202, 196, 202
- the first `ps -a` after the caches were dropped, ms: 27, 46, 31; 26, 31, 35; 11, 10, 11
- `rm -f` of all 300, ms: 728, 749, 662; 640, 716, 614; 1695, 1418, 1613

| | podbag (rootless) | podbag (root) | Kiri (root daemon) |
|---|---|---|---|
| Start, containers 1–10 (median) | 3.0 ms | 8.8 ms | 11.6 ms |
| Start, containers 41–50 (median) | 3.2 ms | 8.7 ms | 13.0 ms |
| Start, the last 10 (median) | 7.5 ms | 13.3 ms | 18.1 ms |
| Memory at 50: the system holds, by category / the tool's processes (PSS) | 26.2 MiB / 17.1 MiB | 9.4 MiB / 2.2 MiB | 9.9 MiB / 9.2 MiB |
| Memory at 300: the system holds, by category / the tool's processes (PSS) | 189.5 MiB / 17.3 MiB | 166.6 MiB / 2.4 MiB | 201.6 MiB / 19.4 MiB |
| Tool processes at 300 | 3 | 1 | 2 |
| `ps -a` with 300 containers (the first one after the caches were dropped) | 4 ms (31 ms) | 5 ms (31 ms) | 10 ms (11 ms) |
| `rm -f` of all 300 | 728 ms | 640 ms | 1613 ms |

## Reading the numbers

### Memory per container (Linux)

- A detached podbag container has no process of its own.
- One keeper process (2.2–2.3 MiB, PSS) watches all of them. It stops 10 seconds after the
  last one, so with no containers podbag runs nothing.
- Later builds are 0.2 MB larger, and the keeper maps more of them: 2.48 MiB against
  2.35 MiB for 50 containers, measured side by side. Start times did not change.
- Spread over 50 containers, the keeper costs 45–48 KiB each; over 300, 8 KiB each.
- Kiri keeps one daemon running (5.9 MiB) and one init process for all its containers.
  Each of the first 50 containers adds about 96 KiB to the daemon, which also keeps a
  thread for it.
- For 50 containers without a network, the whole system holds 8.7 MiB (podbag without
  root), 9.1 MiB (podbag as root) and 13.9 MiB (Kiri).
- Docker keeps dockerd and containerd running (203 MiB with no containers), and grows by
  about 4.6 MiB per container.
- Podman adds a conmon process of about 0.3 MiB per container.

### The keeper against a shim per container

- `PODBAG_KEEPER=0` gives every container its own shim process, as podbag did before.
  `dev/bench/session.sh` measured it in the same session (in `bench-results-nokeeper`,
  not in the tables).
- With shims, for 50 containers without root:
  - podbag's processes take 7.4 MiB instead of 2.3 MiB,
  - the system holds 25.5 MiB instead of 8.7 MiB (as root: 18.8 instead of 9.1 MiB),
  - a start takes 8.6 ms instead of 4.1 ms.
- Why the keeper is faster without root:
  - each container's cgroup is made below the keeper's systemd scope, with no
    `systemd-run` per container,
  - a container that left only the user's own files is removed without a second user
    namespace.
- As root, the start time is about the same with and without the keeper (4.4 and 5.0 ms).
- The price: the keeper is the parent of its containers. If it is killed, they stop too.

### Where podbag is behind Kiri

Kiri runs as a root daemon, so the root column is the fair comparison. There, podbag is
ahead in every row of both tables, except these:

- **The first start.** When no keeper runs (the first container, or the first after 10
  seconds without one), a start takes 13–16 ms, against Kiri's 9.1 ms. Kiri's daemon is
  always there. That is also why it holds 5.9 MiB with no containers.
- **Without root, on a bridge network.**
  - All of a user's bridge containers go through one pasta process (14.6 MiB) and one
    process that holds their network namespace (1 MiB). This is a fixed cost per user.
  - With 50 containers on the default network: podbag's processes take 17.1 MiB against
    Kiri's 10.7 MiB; the system holds 28.5 MiB against 18.4 MiB; podbag has three
    processes against two.
  - Kiri's processes grow by about 42 KiB per container (9.2 MiB at 50, 19.4 MiB at 300,
    on one network); podbag's do not (17.1 and 17.3 MiB). They meet between 200 and 250
    containers. At 300, the whole system holds less with podbag (190 MiB against 202 MiB).
- **`ps -a` on a cold cache.** podbag reads each container's record; Kiri's daemon keeps
  the list in memory. Right after the page cache was dropped, `ps -a` of 300 containers
  takes about 31 ms, against Kiri's 11 ms. At other times it takes 4–5 ms against 10 ms.
- **Free memory.** In the "MemTotal − MemAvailable" rows, podbag's figures are not always
  the lower ones. The rounds below the table show how much that estimate moves between
  identical rounds. The sum by category, which counts what is allocated, has podbag lower.

### Networks

- As root, podbag, Docker and Kiri use a kernel bridge and no extra process.
- For 50 containers on the default network: podbag's keeper takes 2.2 MiB and the system
  holds 12.9 MiB. Kiri: 10.7 MiB and 18.4 MiB.
- Podman (without root) starts one pasta per container: 50 containers on the default
  network take about 730 MiB.

### Start time

- The table times a whole `run --rm` cycle: start, wait for the exit, remove.
- podbag, with its keeper running: 4.1 ms without root and 4.4 ms as root.
- Kiri: 9.1 ms through the docker CLI, which is itself a separate process. Through the API
  alone, Kiri took about 2.6 ms in an earlier session.
- Podman: about 46 ms. Docker: about 74 ms.
- A detached start on a bridge network takes 3.0 ms (without root) and 8.8 ms (as root),
  against Kiri's 11.6 ms.

### composefs and hard links

- As root, podbag mounts images with composefs: one EROFS metadata file per image, shared
  by all its containers. File contents come from the object store.
- `PODBAG_IMAGE_MOUNT=hardlink` measured hard-link trees in the same session (in
  `bench-results-hardlink`, not in the tables):
  - start: 4.4 ms with composefs and 4.6 ms with hard links. That is no real difference:
    4.4–4.9 against 4.1–4.6 ms over the last four sessions,
  - memory: the same (9.1 and 9.2 MiB held for 50 containers),
  - store: slightly smaller with composefs (154.9 against 156.7 MiB for the first Python
    image), since one metadata file replaces a folder tree.
- What composefs saves: the work at pull time (no tree is written), and the tree's inodes.

### Disk

- podbag stores each distinct file once, across images.
- The two Python images have different Debian bases, so their layers share nothing.
  podbag shares the files that are the same in both.
- The second image adds 121–123 MiB to podbag's store, 152 MiB to Podman's, and
  193–196 MiB to Docker's and Kiri's, which also keep each layer's compressed download.
- Kiri unpacks a layer when a container first uses it. It deletes the layers of removed
  images only when it next creates a container. That is why every store holds alpine, and
  each image is used once before it is measured.

### Many containers

- With 300 containers on one user network, a start takes:
  - podbag: 3.0 ms (without root) and 8.8 ms (as root) for the first ten; 7.5 and
    13.3 ms for the last ten,
  - Kiri: 11.6 ms and 18.1 ms.
- podbag's start time grows by 4.5 ms over the 300 containers, Kiri's by 6.5 ms.
- podbag's own work is the smaller part of that growth: each container's hosts file is
  written again at every start and end (0.6 ms for 250 files), and a new name is compared
  with the others. The rest is the system's work for a bridge with many ports, which Kiri
  pays too.
- podbag's own processes for all 300: 17.3 MiB without root (14.6 MiB of it is pasta) and
  2.4 MiB as root. Kiri's: 19.4 MiB.
- The system holds 190 MiB (podbag without root), 167 MiB (podbag as root) and 202 MiB
  (Kiri). Most of it is the kernel's memory for each container's namespaces, and the
  containers' own processes.
- Removing all 300 with one `rm -f` takes 0.64–0.73 s, against Kiri's 1.6 s. podbag stops
  all of them first, so that the clean-ups overlap.
- A /24 network holds 253 containers. Use `--subnet` for more.

### The state of the system

- What the system holds for N containers depends on what the machine ran before.
- As the first tool after a restart of the VM, 300 containers held 269 MiB (podbag without
  root), 253 MiB (podbag as root) and 292 MiB (Kiri): one run each, after its own restart.
- The kernel keeps slab memory that the first runs make it allocate (the idle system's
  unreclaimable slab grew from 126 to 145 MiB over two more runs), and later runs use it
  again. After a day of runs, the same 300 containers held 172, 147 and 187 MiB.
- A tool that is measured first would carry that cost. So the session measures the second
  table after one round that is not counted (its figures: 221, 166 and 201 MiB). Both
  tables come from one session that began a few runs after a restart.
- In each state that was measured, podbag holds less than Kiri.

### macOS

- Each tool runs Linux in its own VM. What the VM takes, the Mac loses. ("Memory on the
  Mac" is what Activity Monitor shows for the tool's processes.)
- **Kiri** boots its VM only for the first container, and lets it sleep a minute after the
  last one. With no containers it costs only its daemon (7–12 MiB). 50 containers took
  0.32 GiB.
- **podbag** does the same with a VM of its own. It powers the VM off a minute after the
  last container, and leaves a standby process of under 2 MiB: the least of all six
  minutes after.
  - With 50 containers it took 199 MiB at its default size (2 GiB), and 359 MiB at the
    others' 8 GiB. The guest kernel's bookkeeping grows with the VM's memory (64 bytes per
    4 KiB page: 128 MiB at 8 GiB).
  - Its "just started" row has the VM on: the script's own requests 30 s earlier woke it.
  - While the VM runs, it keeps what its containers used (200 and 360 MiB 30 s after they
    were removed). The memory comes back when the VM powers off.
- **OrbStack** keeps its VM running and gives memory back while idle: 1.16 GiB with 50
  containers, 0.8 GiB six minutes after.
- **Docker Desktop**'s Resource Saver stops its VM five minutes after the last container.
  The memory comes back, but the next `run` waits 1.5 s for the VM. 3.4 GiB of Docker
  Desktop's figure is the heap of its backend process, which compresses well: the Mac's
  used memory rose by 1.6 GiB when it started.
- **Colima's and Podman's** VMs keep what they once used (1.5 and 2.3 GiB).
- A start from the Mac through the docker CLI: Kiri 11.8 ms; podbag 14.4–15.0 ms (about
  9 ms of it is the docker CLI's own start); Colima 74 ms; OrbStack 90 ms; Docker Desktop
  97 ms; Podman 99 ms (through its own CLI). Through its own CLI, podbag starts one in
  7.3–8.0 ms.
- The first `run` after six minutes without containers: 0.60–0.64 s with podbag (its VM
  boots), 0.30 s with Kiri, 0.19–0.23 s with Podman, Colima and OrbStack (their VMs kept
  running), and 1.5 s with Docker Desktop.

### Caveats

- The Linux tables come from one VM on a laptop. All tools ran one after another in one
  session (`dev/bench/session.sh`), with nothing else running in the VM. lima's guest
  agent and the system's timers were stopped meanwhile.
- The session ran eight times on the same day. The published one is the last, with the
  final program.
- Over the last four sessions, start times and PSS stayed within 1 ms and 1 MiB of each
  other for podbag and Kiri. The sums by category moved with the state of the system
  (above): 7.1–8.7 MiB for 50 podbag containers without root, 12.1–13.9 MiB for Kiri's.
- An earlier session on the same VM gave podbag and Podman start times 4–6 times higher,
  while Docker's stayed at 71 ms. The cause was not found, so numbers from different days
  are not compared here.
- Docker and Kiri run as root daemons, and Podman without root, each as it installs.
  podbag is shown in both modes.
- The macOS table comes from one session on the Mac (10 CPUs, 24 GiB), with another
  project's VM running at the same time.
