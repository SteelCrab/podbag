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
