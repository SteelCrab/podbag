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
