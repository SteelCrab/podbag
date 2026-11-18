## Reading the numbers

**Memory per container (Linux).** A detached openpod container has no process of its own:
one keeper process (2.2–2.3 MiB, PSS) watches all of them and leaves 10 seconds after the
last one, so with no containers openpod runs nothing. Spread over 50 containers the keeper
is 45–48 KiB each; over 300 it is 8 KiB each. Kiri keeps one daemon running (5.9 MiB) and
one init process that all its containers share; each of the first 50 containers adds about
96 KiB to the daemon, which also keeps a thread for it. For 50 containers without a network
the whole system holds 8.7 MiB (openpod rootless), 9.1 MiB (openpod as root) and 13.9 MiB
(Kiri). Docker keeps dockerd and containerd running (203 MiB with no containers) and grows
by about 4.6 MiB per container. Podman adds a conmon process of about 0.3 MiB per container.

**The keeper against a shim per container.** `OPENPOD_KEEPER=0` gives every container its
own shim process, as openpod did before; `dev/bench/session.sh` measured it in the same
session (its figures are not in the tables; they are in `bench-results-nokeeper`). With
shims, openpod's processes for 50 containers take 7.4 MiB instead of 2.3 MiB, and the
system holds 25.5 MiB instead of 8.7 MiB (rootless; as root 18.8 instead of 9.1 MiB). A
rootless start takes 8.6 ms instead of 4.1 ms: under the keeper a container's cgroup is
made below the keeper's systemd scope, without a `systemd-run` for each container, and a
container that left only the user's own files is removed without a second user namespace.
As root the start time is about the same with and without the keeper (4.4 and 5.0 ms). The
price of the keeper: it is the parent of the containers it watches, so if it is killed they
end with it.

**Where openpod is behind Kiri.** In the root column, which is the like-for-like comparison
(Kiri runs as a root daemon), openpod is ahead in every row of both tables except the ones
named here:

- *The first start.* When no keeper runs (the first container, or the first after 10
  seconds without one) a start takes 13–16 ms, against Kiri's 9.1 ms: Kiri's daemon is
  always there, which is also why it holds 5.9 MiB with no containers.
- *Rootless on a bridge network.* All bridge containers of a user go through one pasta
  process (14.6 MiB from its start) and one process that holds their network namespace
  (1 MiB): a fixed cost per user, whatever the number of containers. With 50 containers on
  the default network openpod's processes take 17.1 MiB against Kiri's 10.7 MiB, the system
  holds 28.5 MiB against 18.4 MiB, and openpod has three processes against two. Kiri's
  processes grow by about 42 KiB with each further container (9.2 MiB at 50, 19.4 MiB at
  300, on one network) and openpod's do not (17.1 and 17.3 MiB), so the two meet between
  200 and 250 containers; at 300 the whole system holds less with openpod (190 MiB against
  202 MiB).
- *`ps -a` on a cold cache.* openpod reads each container's record; Kiri's daemon has the
  list in memory. Right after the page cache was dropped, `ps -a` of 300 containers takes
  about 31 ms against Kiri's 11 ms; at other times it takes 4–5 ms against 10 ms.
- *Free memory.* In the "MemTotal − MemAvailable" rows openpod's figures are not always
  the lower ones. The rounds printed below the table show how far that estimate moves
  between identical rounds; the sum by category, which counts what is allocated, has
  openpod lower.

**Networks.** As root, openpod, Docker and Kiri use a kernel bridge and no extra process:
openpod's keeper takes 2.2 MiB and the system holds 12.9 MiB for 50 containers on the
default network, against 10.7 MiB and 18.4 MiB for Kiri. Podman (rootless) starts one pasta
per container: 50 containers on the default network take about 730 MiB.

**Start time.** The table times a whole `run --rm` cycle: start, wait for the exit, remove.
With its keeper running, openpod does it in 4.1 ms rootless and 4.4 ms as root. Kiri takes
9.1 ms through the docker CLI, which itself is a separate process start; Kiri's own figure
through the API alone was about 2.6 ms in an earlier session. Podman takes about 46 ms and
Docker about 74 ms. A detached start on a bridge network takes 3.0 ms rootless and 8.8 ms
as root (the first rows of the second table), against Kiri's 11.6 ms.

**composefs and hard links.** As root, openpod mounts images with composefs (one EROFS
metadata file per image, shared by all its containers, with the file contents read from the
object store); `OPENPOD_IMAGE_MOUNT=hardlink` measured the hard-link trees in the same
session (in `bench-results-hardlink`, not in the tables): a start takes 4.4 ms with
composefs and 4.6 ms with hard links (no difference: over the last four sessions 4.4–4.9
against 4.1–4.6), the memory is the same (9.1 and 9.2 MiB held for 50 containers), and
the store is slightly smaller (154.9 against 156.7 MiB for the first Python image), since
one metadata file replaces a directory tree. What composefs saves is the work at pull time
(no tree is written) and the tree's inodes.

**Disk.** openpod stores each distinct file once, across images. The two Python images have
different Debian bases, so layer-level sharing finds nothing in common, while openpod shares
the files that are the same in both: the second image adds 121–123 MiB to openpod's store,
152 MiB to Podman's, and 193–196 MiB to Docker's and Kiri's, which also keep each layer's
compressed download next to the unpacked files. Kiri unpacks a layer when a container first
uses it, and deletes the layers of removed images only when it next creates a container;
that is why every store holds alpine and each image is used once before it is measured.

**Many containers.** With 300 containers on one user network, a start takes 3.0 ms
(rootless) and 8.8 ms (as root) for the first ten and 7.5 ms and 13.3 ms for the last ten;
Kiri's take 11.6 ms and 18.1 ms. openpod's grow by 4.5 ms over the 300 containers and
Kiri's by 6.5 ms. Of openpod's growth, its own work is the smaller part (the name list in
each container's hosts file is written again at every start and end: 0.6 ms for 250 files;
and the name of a new container is compared with the names of the others); the rest is the
system's work for a bridge with many ports, which Kiri pays too. openpod's own processes
for all 300 use 17.3 MiB rootless (14.6 MiB of that is pasta) and 2.4 MiB as root; Kiri's
use 19.4 MiB. The system holds 190 MiB (openpod rootless), 167 MiB (openpod as root) and
202 MiB (Kiri), most of it the kernel's memory for each container's namespaces and the
containers' own processes. Removing all 300 with one `rm -f` takes 0.64–0.73 s (all of
them are ended first, so that what follows their ends overlaps) against Kiri's 1.6 s. A /24
network holds 253 containers; use `--subnet` for more.

**The state of the system.** What the system holds for a number of containers depends on
what the machine ran before. As the first tool after a restart of the VM, 300 containers
hold 269 MiB (openpod rootless), 253 MiB (openpod as root) and 292 MiB (Kiri), one run each
after its own restart. The kernel keeps slab memory that the first runs make it allocate
(the idle system's unreclaimable slab grew from 126 to 145 MiB over two more runs), and
later runs use it again: after a day of runs the same 300 containers held 172, 147 and
187 MiB. A tool that is measured first would carry that. So the session measures the second
table after one round that is not counted (its figures: 221, 166 and 201 MiB), and both
tables are from one session that began a few runs after a restart. In each state that was
measured, openpod holds less than Kiri.

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
one session (`dev/bench/session.sh`), with nothing else running in the VM; lima's guest
agent and the system's timers (package lists, firmware data) were stopped for its
duration. The session was run eight times on the same day; the published one is the last,
with the final program. Over the last four sessions the start times and PSS came out
within 1 ms and 1 MiB of each other for openpod and Kiri; the sums by category moved with
the state of the system (above): 7.1–8.7 MiB for 50 rootless openpod containers,
12.1–13.9 MiB for Kiri's. An earlier session on the same VM (milestone M7) gave openpod and
Podman start times 4–6 times higher while Docker's stayed at 71 ms; the cause was not
found, so numbers from different days are not compared here. Docker and Kiri run as root
daemons and Podman rootless, each as it installs; openpod is shown in both modes. The macOS
table comes from one session on the Mac (10 CPUs, 24 GiB), with another project's VM
running at the same time; it was measured before openpod had the keeper and is not
repeated here.
