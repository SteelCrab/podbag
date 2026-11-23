# Benchmarks: a Kubernetes node

podbag as the container runtime of a Kubernetes node, against the runtimes nodes use today:
containerd and CRI-O. All three run under the same k3s, in the same VM.

- Other benchmarks: [benchmarks.md](benchmarks.md) (container engines) and
  [benchmarks-msa.md](benchmarks-msa.md) (a node of Spring microservices).
- `dev/bench/k8s/session.sh` measures; `dev/bench/k8s/report.sh` writes this page.

## Contents

- [Summary](#summary)
- [Setup](#setup)
  - [Machine](#machine)
  - [Kubernetes](#kubernetes)
  - [The runtimes](#the-runtimes)
  - [What was measured](#what-was-measured)
- [Results](#results)
- [Reading the numbers](#reading-the-numbers)
  - [Pod start](#pod-start)
  - [`kubectl exec`](#kubectl-exec)
  - [Processes](#processes)
  - [Memory](#memory)
  - [Disk](#disk)
- [Caveats](#caveats)
- [Run it yourself](#run-it-yourself)

## Summary

- **The runtime's processes with 30 pods:**
  - count: containerd 63, CRI-O 32, podbag 2,
  - memory (PSS): containerd 275.9 MiB, CRI-O 85.0 MiB, podbag 3.5 MiB.
- **What 30 pods add to the memory the system holds:** containerd 247.5 MiB, CRI-O 59.4 MiB, podbag 39.4 MiB.
- **One pod ready (median):** containerd 462 ms, CRI-O 432 ms, podbag 417 ms.
- **30 pods ready:** containerd 2.2 s, CRI-O 2.1 s, podbag 1.7 s.
  - Most of the start time is Kubernetes itself, the same for all three.
- **Disk for the two Python images:** containerd 394.5 MiB, CRI-O 311.1 MiB, podbag 272.8 MiB.
- **`kubectl exec` (median):** containerd 54 ms, CRI-O 49 ms, podbag 49 ms.

## Setup

### Machine

- One VM: Ubuntu 26.04.1 LTS, kernel 7.0.0-43-generic, aarch64, 4 CPUs,
  3.8 GiB of memory (lima on macOS).
- One runtime at a time. No builds or tests ran in the VM meanwhile.

### Kubernetes

- k3s v1.37.2+k3s1, a single node: the same binary and the same flags for all three
  runtimes. Only `--container-runtime-endpoint` differs (none for containerd).
- k3s's own add-ons are off (`--disable traefik,servicelb,metrics-server,local-storage`).
  CoreDNS runs. At start, k3s also runs one short Helm job (`helm-install-gateway-api-crd`);
  it ends before anything is measured.
- The kubelet uses the cgroupfs driver, and so do the three runtimes.
- The VM's disk is nearly full. So eviction for disk space and image garbage collection
  are off (`--kubelet-arg`), the same for all three.
- The network: k3s's flannel (VXLAN), through its CNI plugins and its CNI
  configuration, for all three.
- crictl v1.37.0.

### The runtimes

- **containerd:** k3s's embedded containerd v2.3.7-k3s1, with the runc it
  ships (1.4.2). One shim process per pod (`containerd-shim-runc-v2`),
  and one pause container per pod (docker.io/rancher/mirrored-pause:3.10.2).
- **CRI-O:** 1.37.4, the static release bundle. Its default OCI runtime:
  crun 1.31. One conmon process per container. Its default configuration
  drops the pause container (`drop_infra_ctr`) when a pod does not share one PID
  namespace, as here: it still pulls the pause image (registry.k8s.io/pause:3.10.2).
  Started with `--cgroup-manager cgroupfs --conmon-cgroup pod` and k3s's CNI folders.
- **podbag:** `podbag cri serve`, podbag 0.1.0, a static release build of this
  repository. No pause container, and no process per pod or container: one keeper
  process watches all containers.

### What was measured

- Each runtime gets a fresh cluster in every round. Its images stay from round to round.
- A Deployment of `busybox:1.36` running `sleep`. The image is pulled before anything
  is timed.
  - **One pod:** scaled from 0 to 1, until the pod is ready; then back to 0, until the
    runtime has removed it. 10 runs, after one that is not counted.
  - **30 pods:** scaled from 0 to 30, until all are ready.
  - The time runs from the `kubectl scale` command until a watch of the pods
    (`kubectl get pods -w`) reports the pod ready.
- **`kubectl exec POD -- true`:** 10 runs, with 30 pods running.
- **Memory:** at rest (only CoreDNS), and with the 30 pods running.
- **Disk:** `crictl pull` of `python:3.12-slim-bookworm`, then `python:3.12-slim-trixie`,
  as in [benchmarks.md](benchmarks.md#disk).
- One round that is not counted, then 3 rounds. The order of the runtimes turns
  from round to round, so that each one comes first once. Each figure is the median of
  the 3 rounds.

## Results

| | containerd (k3s's own) | CRI-O | podbag |
|---|---|---|---|
| Version | containerd v2.3.7-k3s1, runc 1.4.2 | CRI-O 1.37.4, crun 1.31, conmon 2.2.1 | podbag 0.1.0 (no OCI runtime) |
| One pod: scale 0 → 1 until ready, median / p90 (ms, 10 runs) | 462 / 476 | 432 / 475 | 417 / 427 |
| 30 pods: scale 0 → 30 until all are ready | 2.2 s | 2.1 s | 1.7 s |
| `kubectl exec POD -- true`, median (ms) | 54 | 49 | 49 |
| At rest: the runtime's processes (of them pause) | 3 (1 pause) | 2 (0 pause) | 2 (0 pause) |
| At rest: the runtime's processes, PSS | 115.1 MiB | 70.2 MiB | 3.4 MiB |
| At rest: memory the system holds, by category (more than before k3s started) | 55.6 MiB | 47.3 MiB | 12.6 MiB |
| 30 pods: the runtime's processes (of them pause) | 63 (31 pause) | 32 (0 pause) | 2 (0 pause) |
| 30 pods: the runtime's processes, PSS | 275.9 MiB | 85.0 MiB | 3.5 MiB |
| … of it, anonymous memory (Pss_Anon: heaps and stacks, no program code) | 216.0 MiB | 45.5 MiB | 0.4 MiB |
| … per pod (PSS) | 5486 KiB | 505 KiB | 4 KiB |
| 30 pods: what they add to the memory the system holds, by category | 247.5 MiB | 59.4 MiB | 39.4 MiB |
| … per pod | 8448 KiB | 2027 KiB | 1346 KiB |
| 30 pods: change in MemTotal − MemAvailable (noisy) | 244.3 MiB | 68.3 MiB | 41.2 MiB |
| The k3s server (the same for all): PSS at rest / with 30 pods | 566.8 MiB / 527.5 MiB | 610.3 MiB / 561.2 MiB | 571.8 MiB / 574.1 MiB |
| Disk: `python:3.12-slim-bookworm` | 201.1 MiB | 158.7 MiB | 152.9 MiB |
| Disk: then `python:3.12-slim-trixie` adds | 193.4 MiB | 152.3 MiB | 119.9 MiB |
| Disk: both images | 394.5 MiB | 311.1 MiB | 272.8 MiB |

- "The runtime's processes":
  - containerd: containerd itself, one `containerd-shim-runc-v2` per pod, and the pause
    processes. (k3s starts containerd as a child process.)
  - CRI-O: crio, one conmon per container, and the pause processes (none here).
  - podbag: `podbag cri serve` and the keeper (and `podbag __pause` for a pod that
    shares one PID namespace; none here).
  - The pods' own `sleep` processes are not counted: they are the same for all.
- PSS divides shared pages among the processes that share them. "Per pod" is (with 30
  pods − at rest) / 30.
- "Memory the system holds, by category" is the change in AnonPages + Shmem + Slab +
  KernelStack + PageTables + Percpu + VmallocUsed of `/proc/meminfo`, as in
  [benchmarks.md](benchmarks.md). Caches are dropped before each reading. It includes
  the runtime, the pods' processes, CoreDNS, and the kernel's memory for namespaces,
  cgroups, network devices and mounts.
  - The k3s server's own anonymous memory is left out: it is the same program for all
    three, and its Go heap moves by tens of MiB. Its PSS has its own row.
  - lima's guest agent is left out too (it was stopped for the session).
  - What 30 pods add, in each round, MiB: 247.5, 243.5, 247.8; 59.8, 59.4, 58.9; 36.9, 44.8, 38.4.
- The rounds, in the order of the columns:
  - one pod, median ms: 462, 463, 458; 438, 432, 429; 415, 417, 418,
  - one pod, p90 ms: 473, 476, 780; 441, 475, 743; 731, 426, 427,
  - 30 pods, s: 2.1, 2.2, 2.3; 2.1, 2.0, 2.2; 1.7, 1.6, 1.7.
- About one start in ten took 0.3 s longer than the others, with every runtime. The
  p90 shows it in some rounds.
- Disk is what the two images add to `du` of the runtime's image store (hard links
  counted once): `/var/lib/rancher/k3s/agent/containerd` (containerd),
  `/var/lib/containers/storage` (CRI-O), podbag's data folder.

## Reading the numbers

### Pod start

- The time covers the whole path: the API server, the scheduler, the kubelet, the
  runtime, the CNI plugin (flannel) and the kubelet's report back. The runtime is only
  one part of it. The rest is the same for all three.
- podbag is the fastest in both rows, but not by much: 417 ms for one pod, against
  432 ms (CRI-O) and 462 ms (containerd).
- With 30 pods at once, the gap grows: 1.7 s, against 2.1 s and 2.2 s.
- What podbag does not do for each pod:
  - start a pause container (containerd),
  - start a monitor process for each container (a shim, or conmon),
  - run a separate OCI runtime program (runc, crun) for each container.

### `kubectl exec`

- 49–54 ms for all three. Most of it is the `kubectl` process and the API server.

### Processes

- containerd runs one pause container per pod. It holds the pod's namespaces.
- CRI-O drops the pause container by default when a pod does not share one PID namespace.
  So here it ran none, like podbag.
- podbag runs no pause container. It keeps a pod's namespaces with bind mounts of their
  files. A pod that shares one PID namespace gets a tiny init (`podbag __pause`).
- containerd runs one shim process per pod (a Go program). CRI-O runs one conmon process
  per container (a small C program).
- podbag runs `podbag cri serve` and one keeper for all containers. A container's own
  process is its only process. With 30 pods: 2 processes, against 32 (CRI-O) and 63
  (containerd).

### Memory

- **The runtime's own processes:**
  - containerd: 5.4 MiB more per pod. Nearly all of it is the shim's Go heap.
  - CRI-O: about 0.5 MiB per pod, for its conmon.
  - podbag: 4 KiB per pod. Its two processes take 3.4 MiB at rest and 3.5 MiB with 30
    pods.
- **What 30 pods add to the system:** 248 MiB (containerd), 59 MiB (CRI-O), 39 MiB
  (podbag).
  - podbag's 39 MiB is mostly the pods themselves: their `sleep` processes and the
    kernel's memory for their namespaces, cgroups and network devices. Every runtime
    pays this part.
- **At rest:** the runtime's processes take 115 MiB (containerd), 70 MiB (CRI-O) and
  3.4 MiB (podbag).
- **containerd shares its code with k3s:** k3s's containerd is the k3s program itself,
  started under another name. The two processes share its code pages, and PSS gives
  half of them to each.
  - So the k3s server's PSS is lower in the containerd column.
  - A containerd that is installed on its own maps its own, smaller program instead.
  - The anonymous memory (heaps and stacks) does not depend on this: at rest, 39 MiB
    (containerd), 31 MiB (CRI-O) and 0.3 MiB (podbag).
- **The k3s server** itself takes 530–610 MiB, whatever the runtime. On a node this
  small, it is the largest part.

### Disk

- containerd keeps each layer twice: the compressed download in its content store, and
  the unpacked files in its snapshots.
- CRI-O keeps the unpacked layers only.
- podbag stores each distinct file once, across images. The two Python images have
  different Debian bases, so their layers share nothing, but many of their files are
  the same.
- The second image adds 120 MiB to podbag, 152 MiB to CRI-O and 193 MiB to containerd.
  These are the same figures as in [benchmarks.md](benchmarks.md#disk), for podbag,
  Podman and Docker.

## Caveats

- One VM on a laptop, one session. Numbers from other sessions are not compared here.
- The state of the system: the VM had run builds and tests for about four hours before
  the session. What the system holds depends on that: the kernel keeps slab memory that
  earlier runs made it allocate. One round that is not counted came first, and the order
  of the runtimes turned from round to round.
- The pod start and `kubectl exec` times include the start of a `kubectl` process, the
  same for all three.
- CRI-O runs crun, its default; containerd runs runc. Both are what each one ships.
- containerd is k3s's embedded build. A node with containerd installed on its own runs
  separate programs, and its memory figures can differ.
- The VM's disk was nearly full. So eviction for disk space and image garbage collection
  were off, for all three.
- A rootless containerd and BuildKit of the VM's user ran, idle, during the session. They
  are the same for all three, and they are not counted.
- The first session found a bug in podbag's CRI server: it gave the kubelet only 64 KiB
  of HTTP/2 connection window, so the server stalled when 30 pods started at once. The
  numbers here come from the build with the fix ("give the CRI client the whole HTTP/2
  connection window").

## Run it yourself

In the VM (`dev/vm.sh shell`), with k3s, crictl and CRI-O installed:

```sh
dev/vm.sh static                          # the podbag binary
sudo K3S=/path/to/k3s dev/bench/k8s/session.sh   # about 35 minutes
dev/bench/k8s/report.sh                   # writes this page
```
