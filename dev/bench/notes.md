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
