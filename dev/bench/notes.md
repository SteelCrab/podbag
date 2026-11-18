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
