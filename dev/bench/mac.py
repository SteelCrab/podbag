#!/usr/bin/env python3
"""Measure one container tool on this Mac, each with its own Linux VM (design 9):

  - start time of `run --rm --network none alpine true`, timed from the Mac (median, p90),
  - memory of every Mac process of the tool, its VM included (physical footprint), right
    after it started, with N sleeping containers, and 30 s and 6 min after they are gone,
  - the first `run` after those 6 minutes (a VM that went to sleep has to wake up).

One tool runs at a time: the script starts it and stops it again; stop the others first.
It refuses a tool that is already running (it would stop the user's containers). Nothing of
the user's own is removed: the containers it starts carry a label, and a container is
removed only if its own labels say so; a test image is removed only if this script pulled
it, and an unclear answer about an image stops the run. Colima runs in a profile of its
own (`openpod-bench`), so the user's default profile is left as it is.

usage: dev/bench/mac.py docker-desktop|orbstack|kiri|podman|colima|openpod-mac|openpod-mac-default|openpod [results-dir]
openpod-mac runs openpod's own VM (target/release/openpod, signed, and the guest in
target/vm) in a home of its own (~/.openpod-bench) at the others' size; openpod-mac-default
at openpod's default size; openpod is the older measurement in lima's VM.
Results go to <results-dir>/<tool>.env as key=value lines (default: bench-results/mac in
the repository); dev/bench/report.sh turns them into the macOS table of docs/benchmarks.md.
"""

import os
import platform
import plistlib
import re
import signal
import statistics
import subprocess
import sys
import time

HOME = os.path.expanduser("~")
REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
ALPINE = "docker.io/library/alpine:3.22"
LABEL = "openpod-bench=1"
RUNS = int(os.environ.get("BENCH_RUNS", "100"))
COUNT = int(os.environ.get("BENCH_COUNT", "50"))
IDLE_WAIT = int(os.environ.get("BENCH_IDLE_WAIT", "360"))
KIRI = "/Applications/Kiri.app/Contents/MacOS/kiri"
OPENPOD = "/home/pista.guest/openpod-target/aarch64-unknown-linux-musl/release/openpod"
LIMA_SSH = ["ssh", "-F", f"{HOME}/.lima/openpod-dev/ssh.config", "lima-openpod-dev", "--"]
OPENPOD_MAC = f"{REPO}/target/release/openpod"
OPENPOD_HOME = os.environ.get("BENCH_OPENPOD_HOME", f"{HOME}/.openpod-bench")


def docker_at(sock):
    return ["docker", "-H", f"unix://{sock}"]


# For each tool: how to call it, start and stop it, which Mac processes are its own (by
# command), and which files its VM process has open (to tell its VM from other VMs).
TOOLS = {
    "docker-desktop": dict(
        title="Docker Desktop",
        cli=docker_at(f"{HOME}/.docker/run/docker.sock"),
        up=[["docker", "desktop", "start", "--detach"]],
        down=[["docker", "desktop", "stop"]],
        procs=[r"^/Applications/Docker\.app/", r"^/Library/PrivilegedHelperTools/com\.docker\."],
        vm_files=[f"{HOME}/Library/Containers/com.docker.docker/"],
        app="/Applications/Docker.app",
        labels=True,
    ),
    "orbstack": dict(
        title="OrbStack",
        cli=docker_at(f"{HOME}/.orbstack/run/docker.sock"),
        up=[["orbctl", "start"]],
        down=[["orbctl", "stop"]],
        procs=[r"^/Applications/OrbStack\.app/"],
        # OrbStack runs its VM inside its helper process.
        vm_procs=[r"OrbStack Helper\.app/Contents/MacOS/OrbStack Helper"],
        vm_files=[f"{HOME}/Library/Group Containers/HUAQ24HBR6.dev.orbstack/", f"{HOME}/.orbstack/"],
        app="/Applications/OrbStack.app",
        labels=True,
    ),
    "kiri": dict(
        title="Kiri",
        cli=docker_at(f"{HOME}/.kiri/run/docker.sock"),
        up="kiri-daemon",
        down="kiri-daemon",
        procs=[r"^/Applications/Kiri\.app/"],
        vm_files=[f"{HOME}/Library/Application Support/Kiri/", f"{HOME}/.kiri/"],
        app="/Applications/Kiri.app",
        labels=True,
    ),
    "podman": dict(
        title="Podman",
        cli=["podman"],
        up=[["podman", "machine", "start"]],
        down=[["podman", "machine", "stop"]],
        procs=[r"(^|/)(vfkit|krunkit|gvproxy)( |$)"],
        # krunkit runs the VM in its own process; vfkit hands it to a VM process.
        vm_procs=[r"(^|/)(vfkit|krunkit)( |$)"],
        vm_files=[f"{HOME}/.local/share/containers/podman/machine/"],
        labels=True,
    ),
    "colima": dict(
        title="Colima",
        cli=docker_at(f"{HOME}/.colima/openpod-bench/docker.sock"),
        up=[
            [
                "colima", "start", "--profile", "openpod-bench", "--cpu", "10", "--memory", "8",
                "--disk", "60", "--vm-type", "vz",
            ]
        ],
        down=[["colima", "stop", "--profile", "openpod-bench"]],
        procs=[r"\.colima/", r"(^|/)colima( |$)"],
        vm_files=[f"{HOME}/.colima/"],
        labels=True,
    ),
    "openpod": dict(
        title="openpod (lima VM)",
        cli=LIMA_SSH
        + [
            "sudo",
            "-n",
            "env",
            "OPENPOD_DATA_DIR=/var/tmp/openpod-bench-mac/data",
            "OPENPOD_RUNTIME_DIR=/run/openpod-bench-mac",
            OPENPOD,
        ],
        up=[["limactl", "start", "openpod-dev"]],
        down=[["limactl", "stop", "openpod-dev"]],
        procs=[r"\.lima/openpod-dev/", r"limactl .*openpod-dev"],
        vm_files=[f"{HOME}/.lima/openpod-dev/"],
        labels=False,
    ),
}


def openpod_mac(title, size):
    """openpod's own VM on this Mac, in a home of its own; `size` sets the VM's size."""
    return dict(
        title=title,
        cli=docker_at(f"{OPENPOD_HOME}/run/docker.sock"),
        up="openpod-mac",
        down="openpod-mac",
        # The guest the release binary carries (built with --features embed-guest).
        env=dict(size, OPENPOD_MAC_HOME=OPENPOD_HOME),
        procs=[r"openpod __vm-(host|standby)"],
        # Only the host or standby of this home (another openpod may run on the Mac).
        own_files=[f"{OPENPOD_HOME}/"],
        vm_files=[f"{OPENPOD_HOME}/"],
        labels=False,
    )


TOOLS["openpod-mac"] = openpod_mac("openpod", {"OPENPOD_VM_CPUS": "10", "OPENPOD_VM_MEMORY": "8192"})
TOOLS["openpod-mac-default"] = openpod_mac("openpod, default size", {})


def openpod_env(tool):
    env = {k: v for k, v in os.environ.items() if k != "OPENPOD_VM_GUEST"}
    return dict(env, **tool["env"])


def openpod_cmd(tool, *args):
    return subprocess.run(
        [OPENPOD_MAC, *args],
        env=openpod_env(tool),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        timeout=300,
    )


def sh(cmd, check=True, quiet=True, timeout=600):
    r = subprocess.run(
        cmd,
        stdout=subprocess.PIPE if quiet else None,
        stderr=subprocess.PIPE,
        text=True,
        timeout=timeout,
    )
    if check and r.returncode != 0:
        raise SystemExit(f"failed ({r.returncode}): {' '.join(cmd)}\n{r.stderr}")
    return r


def log(msg):
    print(msg, file=sys.stderr, flush=True)


# ---- Processes and memory ----


def processes():
    out = sh(["ps", "-axo", "pid=,uid=,command="]).stdout
    procs = []
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3:
            procs.append((int(parts[0]), int(parts[1]), parts[2]))
    return procs


def open_files(pid):
    r = sh(["lsof", "-p", str(pid), "-Fn"], check=False)
    return [l[1:] for l in r.stdout.splitlines() if l.startswith("n")]


def vm_pids(tool):
    """Virtualization.framework VM processes whose disk files belong to the tool."""
    found = []
    for pid, _, command in processes():
        if "com.apple.Virtualization.VirtualMachine" not in command:
            continue
        if any(f.startswith(p) for f in open_files(pid) for p in tool["vm_files"]):
            found.append(pid)
    return found


def tool_pids(tool):
    """The tool's own processes (pid, uid), and the processes that hold its VM."""
    own, vms = [], vm_pids(tool)
    for pid, uid, command in processes():
        if pid == os.getpid() or not any(re.search(p, command) for p in tool["procs"]):
            continue
        if "own_files" in tool and not any(
            f.startswith(p) for f in open_files(pid) for p in tool["own_files"]
        ):
            continue
        own.append((pid, uid))
        if any(re.search(p, command) for p in tool.get("vm_procs", [])):
            vms.append(pid)
    return own, vms


def footprints(pids):
    """phys_footprint in bytes per process (footprint(1)); root's processes by RSS."""
    out = {}
    mine = [p for p, uid in pids if uid == os.getuid()]
    if mine:
        args = ["footprint", "-f", "bytes", "--noCategories"]
        for p in mine:
            args += ["-p", str(p)]
        current = None
        for line in sh(args, check=False).stdout.splitlines():
            m = re.match(r".*\[(\d+)\]: .*Footprint: (\d+) B", line)
            if m:
                current = int(m.group(1))
                out[current] = int(m.group(2))
                continue
            m = re.match(r"\s*phys_footprint: (\d+) B", line)
            if m and current is not None:
                out[current] = int(m.group(1))
    for p, uid in pids:
        if p not in out:
            r = sh(["ps", "-o", "rss=", "-p", str(p)], check=False)
            if r.stdout.strip():
                out[p] = int(r.stdout.strip()) * 1024
    return out


def sample(tool):
    own, vms = tool_pids(tool)
    pids = own + [(p, os.getuid()) for p in vms if p not in [q for q, _ in own]]
    fp = footprints(pids)
    total = sum(fp.values())
    vm = sum(fp.get(p, 0) for p in vms)
    return total // 1024, vm // 1024, len(pids)


# ---- Starting, stopping, timing ----

kiri_daemon = None


def up(name, tool):
    global kiri_daemon
    if tool["up"] == "kiri-daemon":
        os.makedirs(f"{REPO}/bench-results/mac", exist_ok=True)
        logf = open(f"{REPO}/bench-results/mac/kiri-daemon.log", "a")
        # The same VM size as the others (Kiri's default is 2 CPUs and 8 GiB).
        env = dict(os.environ, KIRI_CPUS="10", KIRI_MEMORY_MIB="8192")
        kiri_daemon = subprocess.Popen(
            [KIRI, "daemon"], stdout=logf, stderr=logf, env=env, start_new_session=True
        )
    elif tool["up"] == "openpod-mac":
        r = openpod_cmd(tool, "vm", "start")
        if r.returncode != 0:
            raise SystemExit(f"openpod vm start failed: {r.stderr}")
    else:
        for cmd in tool["up"]:
            sh(cmd, quiet=True, timeout=900)
    deadline = time.time() + 600
    while time.time() < deadline:
        if sh(tool["cli"] + ["ps"], check=False, timeout=60).returncode == 0:
            return
        time.sleep(1)
    raise SystemExit(f"{name} did not become ready")


def down(name, tool):
    global kiri_daemon
    if tool["down"] == "kiri-daemon":
        if kiri_daemon is not None:
            os.killpg(kiri_daemon.pid, signal.SIGTERM)
            try:
                kiri_daemon.wait(timeout=60)
            except subprocess.TimeoutExpired:
                os.killpg(kiri_daemon.pid, signal.SIGKILL)
            kiri_daemon = None
    elif tool["down"] == "openpod-mac":
        openpod_cmd(tool, "vm", "stop")
    else:
        for cmd in tool["down"]:
            sh(cmd, check=False, timeout=600)


def run_ms(cmd, env=None):
    t0 = time.perf_counter()
    r = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env)
    ms = (time.perf_counter() - t0) * 1000
    if r.returncode != 0:
        raise SystemExit(f"failed: {' '.join(cmd)}\n{r.stderr}")
    return ms


def pct(values, p):
    v = sorted(values)
    i = max(1, -(-len(v) * p // 100))
    return v[i - 1]


def label_args(tool):
    return ["--label", LABEL] if tool["labels"] else []


def image_presence(returncode, stderr):
    """`image inspect`'s answer: present, absent (only a "no such image" message says so),
    or unknown (any other failure: a busy socket must not read as "absent")."""
    if returncode == 0:
        return "present"
    if re.search(r"no such image|image not known", stderr, re.IGNORECASE):
        return "absent"
    return "unknown"


def labelled_ids(inspect_output):
    """IDs from `inspect --format '{{.Id}} {{index .Config.Labels "openpod-bench"}}'` lines
    whose label is set to 1."""
    ids = []
    for line in inspect_output.splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1] == "1":
            ids.append(parts[0])
    return ids


def remove_ours(tool):
    if not tool["labels"]:
        # openpod: its own data directory in the VM holds only this script's containers.
        ids = sh(tool["cli"] + ["ps", "-aq"], check=False).stdout.split()
    else:
        listed = sh(tool["cli"] + ["ps", "-aq", "--filter", f"label={LABEL}"], check=False).stdout.split()
        if not listed:
            return
        # Ask each container again: a tool that ignored the filter would list the user's too.
        fmt = "{{.Id}} {{index .Config.Labels \"openpod-bench\"}}"
        out = sh(tool["cli"] + ["inspect", "--format", fmt] + listed, check=False).stdout
        ids = labelled_ids(out)
    if ids:
        sh(tool["cli"] + ["rm", "-f"] + ids, check=False, timeout=900)


def has_image(tool, image):
    if not tool["labels"]:
        return False
    r = sh(tool["cli"] + ["image", "inspect", image], check=False)
    answer = image_presence(r.returncode, r.stderr)
    if answer == "unknown":
        raise SystemExit(f"cannot tell whether {image} exists (nothing was removed):\n{r.stderr}")
    return answer == "present"


def vm_size(name, tool):
    if name == "podman":
        out = sh(tool["cli"] + ["info", "--format", "{{.Host.CPUs}} {{.Host.MemTotal}}"]).stdout
    elif name == "openpod":
        # ssh joins its arguments into one command line for the remote shell: one string.
        out = sh(
            LIMA_SSH + ["echo $(nproc) $(( $(awk '/^MemTotal/{print $2}' /proc/meminfo) * 1024 ))"]
        ).stdout
    else:
        out = sh(tool["cli"] + ["info", "--format", "{{.NCPU}} {{.MemTotal}}"]).stdout
    cpus, mem = out.split()
    if name == "kiri" and int(mem) == 0:
        mem = 8192 << 20  # Kiri's API reports no memory size; this is KIRI_MEMORY_MIB
    return cpus, int(mem)


def version(name, tool):
    def app_version(app):
        with open(f"{app}/Contents/Info.plist", "rb") as f:
            return plistlib.load(f).get("CFBundleShortVersionString", "?")

    if name == "podman":
        return "Podman " + sh(["podman", "version", "--format", "{{.Client.Version}}"]).stdout.strip()
    if name == "colima":
        return sh(["colima", "version"]).stdout.splitlines()[0]
    if name == "openpod":
        return sh(tool["cli"] + ["--version"]).stdout.strip()
    if tool["up"] == "openpod-mac":
        return openpod_cmd(tool, "--version").stdout.strip()
    return f"{tool['title']} {app_version(tool['app'])}"


def main():
    if len(sys.argv) < 2 or sys.argv[1] not in TOOLS:
        raise SystemExit(f"usage: {sys.argv[0]} {'|'.join(TOOLS)} [results-dir]")
    name = sys.argv[1]
    tool = TOOLS[name]
    out = sys.argv[2] if len(sys.argv) > 2 else f"{REPO}/bench-results/mac"
    os.makedirs(out, exist_ok=True)
    results = {
        "tool": name,
        "title": tool["title"],
        "runs": RUNS,
        "count": COUNT,
        "date": time.strftime("%Y-%m-%d"),
    }

    if sh(tool["cli"] + ["ps"], check=False, timeout=60).returncode == 0:
        raise SystemExit(
            f"{name} is already running: stop it first (the measurement starts and stops it, "
            "and needs it to itself)"
        )
    log(f"== {name}: starting")
    up(name, tool)
    pulled = False
    try:
        results["version"] = version(name, tool)
        cpus, mem = vm_size(name, tool)
        results["vm_cpus"], results["vm_mem_kb"] = cpus, mem // 1024
        if not has_image(tool, ALPINE):
            sh(tool["cli"] + ["pull", ALPINE], timeout=900)
            pulled = tool["labels"]
        remove_ours(tool)
        time.sleep(30)
        results["mem_idle_kb"], results["vm_idle_kb"], results["procs_idle"] = sample(tool)

        log(f"start time: {RUNS} runs")
        if name == "openpod":
            ssh = [run_ms(LIMA_SSH + ["true"]) for _ in range(33)][3:]
            results["transport_ms"] = round(statistics.median(ssh), 1)
        cmd = tool["cli"] + ["run", "--rm", "--network", "none"] + label_args(tool) + [ALPINE, "true"]
        times = [run_ms(cmd) for _ in range(RUNS + 3)][3:]
        results["start_median_ms"] = round(statistics.median(times), 1)
        results["start_p90_ms"] = round(pct(times, 90), 1)
        if tool["up"] == "openpod-mac":
            # Its own CLI too, as Podman's start is timed through Podman's.
            own = [OPENPOD_MAC, "run", "--rm", "--network", "none", ALPINE, "true"]
            env = openpod_env(tool)
            times = [run_ms(own, env) for _ in range(RUNS + 3)][3:]
            results["own_start_median_ms"] = round(statistics.median(times), 1)
            results["own_start_p90_ms"] = round(pct(times, 90), 1)

        log(f"memory: {COUNT} containers")
        for _ in range(COUNT):
            sh(tool["cli"] + ["run", "-d"] + label_args(tool) + [ALPINE, "sleep", "3600"])
        time.sleep(10)
        results["mem_busy_kb"], results["vm_busy_kb"], results["procs_busy"] = sample(tool)
        remove_ours(tool)
        gone = time.time()
        time.sleep(30)
        results["mem_after30_kb"], results["vm_after30_kb"], _ = sample(tool)
        log(f"idle: until {IDLE_WAIT} s after the containers are gone")
        time.sleep(max(0, IDLE_WAIT - (time.time() - gone)))
        results["mem_after6m_kb"], results["vm_after6m_kb"], results["procs_after6m"] = sample(tool)
        results["wake_ms"] = round(run_ms(cmd), 1)
    finally:
        remove_ours(tool)
        if pulled:
            sh(tool["cli"] + ["rmi", ALPINE], check=False)
        log(f"== {name}: stopping")
        down(name, tool)

    with open(f"{out}/{name}.env", "w") as f:
        for k, v in results.items():
            f.write(f"{k}={v}\n")
    mac = {
        "chip": sh(["sysctl", "-n", "machdep.cpu.brand_string"]).stdout.strip(),
        "cpus": sh(["sysctl", "-n", "hw.ncpu"]).stdout.strip(),
        "mem_kb": int(sh(["sysctl", "-n", "hw.memsize"]).stdout) // 1024,
        "macos": platform.mac_ver()[0],
        "date": time.strftime("%Y-%m-%d"),
    }
    with open(f"{out}/mac.env", "w") as f:
        for k, v in mac.items():
            f.write(f"{k}={v}\n")
    print(open(f"{out}/{name}.env").read())


if __name__ == "__main__":
    main()
