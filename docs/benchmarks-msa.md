# Benchmark: a node of Spring microservices

How each tool's storage copes with a node that runs many Java services in two releases:
layers (Podman, Docker, Kiri) against files (podbag). Measured with `dev/bench/msa.sh`,
one tool after another (`dev/bench/msa-session.sh`), in the VM of
[benchmarks.md](benchmarks.md): Ubuntu 26.04.1 LTS, kernel 7.0.0-43-generic, aarch64,
4 CPUs, 3.8 GiB of memory (lima on macOS). Every tool was run 3 times, in turn; each figure
is the median of its 3 readings.

## The images

The eight services of Spring PetClinic Microservices (tag 3.4.1, arm64): config-server,
discovery-server, api-gateway, customers-service, vets-service, visits-service,
admin-server and genai-service. `dev/bench/msa-setup.sh` puts them in a registry on the
same host, in two releases:

- **n**: the images as published (`springcommunity/spring-petclinic-*:3.4.1`), on
  eclipse-temurin:17, the full JDK 17.0.16;
- **n+1**: the same services built again as a CI pipeline builds them, on a newer and
  smaller base, eclipse-temurin:17-jre (JRE 17.0.21). The application layers are extracted
  again: the same jars, with new file times.

Each image is a Java base that all eight services share (n: 187 MiB compressed, n+1:
105 MiB) and the service's own Spring Boot layers: dependencies, loader and application,
35–100 MiB compressed, most of it jars. The two releases share no layer (30 and 28
layers, 630 and 560 MiB compressed), so every tool downloads the same 1190 MiB.

## What each tool did

1. Pull release n, then release n+1.
2. Start the 16 services one after another, each until Spring logs "Started", then remove
   it. The page cache is dropped first, so the JVMs read the JRE and their jars from the
   store. Twice: right after the pull, then again. There is no config server or Eureka
   here, so the services run with their default profile.
3. Start 100 containers of both releases on one network, spread as a deployment spreads
   replicas (in each release: config-server 2, discovery-server 3, api-gateway 6,
   customers-service 12, vets-service 9, visits-service 12, admin-server 2,
   genai-service 4). Each runs `sleep infinity` in place of its JVM: 100 JVMs do not fit in
   3.8 GiB. Then remove all of them with one forced `rm`. Podman gets `-t 0`: its `rm -f`
   waits 10 s for each container to stop, and the other tools kill at once.
4. Remove the images and prune.

## Results

| | podbag (rootless) | podbag (root) | Podman (rootless) | Docker (root daemon) | Kiri (root daemon) |
|---|---|---|---|---|---|
| Version | podbag 0.1.0 | podbag 0.1.0 (composefs) | podman 5.7.0 | Docker 29.1.3 | Kiri 0.2.1 |
| Pull: release n / then n+1 | 4.1 s / 3.1 s | 3.8 s / 2.8 s | 7.1 s / 5.5 s | 5.1 s / 4.0 s | 1.1 s / 0.8 s |
| Disk: release n | 614 MiB | 593 MiB | 953 MiB | 1582 MiB | 630 MiB (compressed only) |
| Disk: then n+1 adds | 330 MiB | 310 MiB | 830 MiB | 1388 MiB | 560 MiB (compressed only) |
| Disk: both releases, after their services ran | 944 MiB | 902 MiB | 1783 MiB | 2970 MiB | 2960 MiB |
| First start of each of the 16 services: `run -d`, all 16 | 0.2 s | 0.2 s | 1.0 s | 1.2 s | 5.6 s |
| Pull of both releases, then that first `run -d` of all 16 | 7.4 s | 6.9 s | 13.6 s | 10.3 s | 7.5 s |
| `run -d` when each service starts again, median | 7 ms | 10 ms | 55 ms | 66 ms | 15 ms |
| JVM uptime when Spring is ready, all 16: first / second start | 40.8 s / 41.4 s | 46.0 s / 47.4 s | 53.4 s / 53.8 s | 50.2 s / 46.5 s | 40.2 s / 49.6 s |
| Page cache the 16 starts fill: first / second | 334 MiB / 324 MiB | 335 MiB / 333 MiB | 973 MiB / 975 MiB | 991 MiB / 982 MiB | 2596 MiB / 960 MiB |
| 100 containers: start, median / p90 | 5.7 / 7.2 ms | 11.9 / 14.8 ms | 69.9 / 78.7 ms | 81.5 / 92.4 ms | 18.4 / 24.2 ms |
| 100 containers: memory the system holds, by category | 117.4 MiB | 104.7 MiB | 203.3 MiB | 562.0 MiB | 109.3 MiB |
| 100 containers: the tool's processes (count, PSS) | 3, 17.5 MiB | 1, 2.5 MiB | 103, 53.6 MiB | 104, 578.5 MiB | 2, 11.6 MiB |
| 100 containers: forced `rm` of all | 0.3 s | 0.3 s | 3.3 s | 2.6 s | 0.8 s |
| Disk left after `system prune -a` | 0 MiB | 0 MiB | 0 MiB | 0 MiB | 1770 MiB |

- Kiri unpacks a layer when a container first uses it, not at pull. Until then its store
  holds the compressed layers only; after, both. Docker also keeps the compressed layers
  (in containerd's content store) beside the unpacked ones.
- JVM uptime is Spring's own "process running for" when it logs "Started". The sums varied
  by up to 15 s between the 3 runs of a tool.
- Page cache is the growth of `Cached` in `/proc/meminfo` over the 16 starts.

## Reading the numbers

**Disk.** The two releases share no layer, but they share many files: all the jars (n+1
was built again with new file times) and part of the operating system. A layer store keeps
each layer whole: Podman needs 1783 MiB. Docker also keeps the compressed layers: 2970 MiB.
So does Kiri once its containers have run: 2960 MiB. podbag keeps each file once, in
whatever layer it came: 944 MiB rootless, 902 MiB as root. For example, in podbag
`spring-core-6.2.1.jar` is one file (one inode) in the containers of customers-service
and visits-service, n and n+1; libc and the JRE's `modules` changed between the releases,
so there are two of each. Release n+1 adds 310–330 MiB to podbag (its new JRE and the
changed system files) and 830–1388 MiB to the layer stores.

**Memory: the page cache.** The JVMs read the JRE and their jars. In a layer store, a jar
that two services carry is two files, and each is read into memory on its own. In podbag
it is one file. The 16 starts fill 324–335 MiB of page cache with podbag and 960–991 MiB
with the layer stores: podbag needs a third. The kernel can take this memory back under
pressure, but then the services read their code from disk again. In Kiri's first start the
cache also holds the layers it unpacks: 2596 MiB.

**Pull and the first start.** Kiri only downloads at pull, so its pull is the fastest:
1.9 s for both releases. It unpacks when a container first starts: 1.1–1.5 s for the first
service of each release (the base too), up to 0.4 s for each of the others, 5.6 s for
all 16. The
pull and that first start together take 6.9 s with podbag (root), 7.4 s with podbag
(rootless), 7.5 s with Kiri, 10.3 s with Docker and 13.6 s with Podman. After that, a start
takes 7–15 ms with podbag and Kiri, and 55–66 ms with Podman and Docker. The registry was
on the same host, so these times are unpacking and writing, not the network. Every tool
downloads the same 1190 MiB: files save disk and memory, not downloads.

**The JVMs.** podbag (rootless) had the lowest median in the second start: 41.4 s for all
16, against 46.5–53.8 s. In the first start, Kiri's JVMs read files it had just written,
which were still in memory: 40.2 s. But the 3 runs of each tool overlap: on this VM, the
storage did not clearly change how fast the JVMs start.

**100 containers.** Here the tools' processes decide, not their storage. Docker runs a
shim process for each container (104 processes with its daemons, 579 MiB), and Podman a
conmon (103 processes, 54 MiB). Kiri and podbag run none per container. The system holds
105 MiB with podbag (root), 109 MiB with Kiri and 117 MiB with podbag (rootless), whose
container network runs in user space (its processes: 15 MiB). Podman needs 203 MiB and
Docker 562 MiB.

**Clean-up.** After `system prune -a`, Kiri still had 1770 MiB of unpacked layers. They
went when the next container ran. Starting its daemon did not remove them. This was the
same in all 3 runs.

## Limits

- One VM on a Mac. Its disk is a file on the Mac's SSD, and a read that misses the VM's
  page cache can still come from the Mac's memory. With slower disks each read costs more;
  the layer stores read three times as much here.
- 100 sleeping containers, not 100 JVMs. In production the JVMs' heaps use most of a
  node's memory, the same with every tool.
- The eight services use the same Spring Boot and Spring Cloud versions, so they share
  most jars. Services on different versions share fewer.
- Release n+1 changes the base (also from the JDK image to the smaller JRE image) and
  keeps every jar. A real release also changes the service's own classes and some jars.
- Each tool ran from its command line, without Kubernetes. podbag has no CRI (the
  interface the kubelet uses), so it cannot run the pods of a Kubernetes node.
- Podman reported "rootless netns: kill network process: permission denied" when it
  removed the last container of a rootless network; all containers were removed.
- benchmarks.md also gives the change in MemTotal − MemAvailable. Here it varied too much
  (Kiri's: −7 to 55 MiB) and is not given.

## Running it

    dev/vm.sh run dev/bench/msa-session.sh

`msa-session.sh` starts the registry (`msa-setup.sh`; it builds release n+1 the first
time), stops the VM's guest agent and timers while it measures, and runs `msa.sh` for each
tool (`BENCH_TOOLS="podbag podman"` for some of them). The results go to
`~/bench-results/msa-<tool>.env` and `.log` in the VM. Kiri keeps its unpacked layers
(1.8 GiB) until it runs a container again. `dev/bench/msa-setup.sh stop` stops the
registry.
