"""Run the scripting commands against private engine sockets and a kubectl fixture."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import urllib.request

from tui_dashboard import Engine, Handler


class CLIHandler(Handler):
    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/version":
            self.reply(200, {"Version": "fixture"})
        elif path == "/events":
            self.reply(200, {"Type": "container", "Action": "start"})
        elif path == "/containers/abcd/stats":
            self.reply(200, {"cpu_stats": {"online_cpus": 2}})
        elif path == "/containers/abcd/top":
            self.reply(200, {"Titles": ["CMD"], "Processes": [["sleep"]]})
        elif path == "/containers/abcd/logs":
            line = b"hello from abcd\n"
            self.send_response(200)
            self.send_header("Content-Type", "application/vnd.docker.raw-stream")
            self.send_header("Content-Length", str(len(line)))
            self.end_headers()
            self.wfile.write(line)
        elif path == "/images/search":
            self.reply(200, [{"name": "demo", "description": "Fixture image"}])
        else:
            super().do_GET()

    def do_POST(self):
        path = self.path.split("?")[0]
        if path in ("/containers/abcd/unpause", "/containers/abcd/rename", "/images/create"):
            self.server.requests.append(self.path)
            self.reply(200, {"status": "OK"})
        elif path == "/volumes/prune":
            self.reply(200, {"VolumesDeleted": self.server.volumes})
        else:
            super().do_POST()


def exercise(binary, root, source, destination):
    env = {
        **os.environ,
        "HOME": str(root),
        "PODBAG_UI_DIR": str(root),
        "PODBAG_BIN": str(binary),
        "PODBAG_MAC_HOME": str(root),
        "PODBAG_NO_UPDATE_CHECK": "1",
        "DOCKER_CONFIG": str(root),
        "DOCKER_HOST": "unix://" + source.server_address,
        "PATH": str(root) + os.pathsep + os.environ["PATH"],
    }

    def run(*args, input=None, ok=True):
        result = subprocess.run([binary, *args], input=input, text=True, env=env,
                                capture_output=True, timeout=15)
        assert (result.returncode == 0) == ok, (args, result.returncode, result.stderr, result.stdout)
        return result

    def read(*args, input=None):
        return json.loads(run(*args, input=input).stdout)

    help_text = run("--help").stdout
    for command in ("engine", "import", "kube", "settings", "tutorial"):
        assert command in help_text

    socket = "unix://" + destination.server_address
    assert "docker" in read("engine", "choices")
    assert read("engine", "use", socket)["engine"] == socket
    assert json.loads((root / "ui.json").read_text())["engine"] == socket
    assert read("engine", "list", "containers")[0]["Id"] == "abcd"
    assert read("engine", "status")["version"]["Version"] == "fixture"
    assert read("engine", "stats", "abcd")["cpu_stats"]["online_cpus"] == 2
    assert read("engine", "top", "abcd")["Processes"] == [["sleep"]]
    assert read("engine", "events")["Action"] == "start"
    assert run("engine", "logs", "abcd").stdout == "hello from abcd\n"
    assert read("engine", "--target", "docker", "inspect", "image", "demo:stable")["Id"] == "sha256:1234"
    assert read("engine", "search", "demo", "--limit", "1")[0]["name"] == "demo"
    assert read("engine", "create-volume", "cli-data")["name"] == "cli-data"
    assert read("engine", "list", "volume")[0]["Name"] == "cli-data"
    run("engine", "rm", "container", "abcd", "--force")
    assert destination.requests == ["/containers/abcd?force=1"]
    run("engine", "prune", "volumes", ok=False)
    assert read("engine", "prune", "volumes", "--force")["count"] == 1
    run("engine", "action", "resume", "abcd")
    run("engine", "rename", "abcd", "changed")
    assert destination.requests[1:3] == ["/containers/abcd/unpause", "/containers/abcd/rename?name=changed"]
    docker = root / "docker"
    docker.write_text("#!/bin/sh\nprintf '%s\\n' \"$@\"\n")
    docker.chmod(0o700)
    assert run("engine", "exec", "-it", "abcd", "sh", "-c", "true").stdout.splitlines() == [
        "--host=" + socket, "exec", "-i", "-t", "abcd", "sh", "-c", "true"
    ]

    tags = read("import", "list", "--to", socket)
    assert [image["tag"] for image in tags] == ["demo:first", "demo:stable"]
    assert all(image["present"] for image in tags)
    copied = read("import", "images", "demo:first", "demo:stable", "--to", socket)
    assert copied["images"] == ["demo:first", "demo:stable"]
    assert len(destination.imports) == 1
    assert "/images/get?names=demo%3Afirst&names=demo%3Astable" in source.requests
    run("import", "list", "--from", socket, "--to", socket, ok=False)

    kubectl = root / "kubectl"
    kubectl.write_text("""#!/bin/sh
case "$*" in
  'config get-contexts -o name') printf 'development\nstaging\n' ;;
  'config view'*'--context staging') printf '{"name":"staging","context":{"namespace":"review"}}' ;;
  'config view'*) printf '{"name":"development","context":{"namespace":"apps"}}' ;;
  *custom-columns*) printf 'Pod\n' ;;
  'get pods --context development '*--all-namespaces)
    printf '{"items":[{"kind":"Pod","metadata":{"name":"web","namespace":"apps","uid":"1"}}]}' ;;
  'get pods --context development '*"--namespace apps")
    printf '{"items":[{"kind":"Pod","metadata":{"name":"web","namespace":"apps","uid":"1"}}]}' ;;
  'get pods web --context development '*"--namespace apps"|'--context=development --namespace=apps get pods web '*)
    printf '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web","namespace":"apps","uid":"1"},"spec":{}}' ;;
  '--context=staging --namespace=review create '*)
    cat > "$PODBAG_UI_DIR/created.json"; printf '{"metadata":{"uid":"2"}}' ;;
  '--context=staging --namespace=review get pods web '*) printf '{"metadata":{"uid":"2"}}' ;;
  *) printf 'unexpected kubectl arguments: %s\n' "$*" >&2; exit 1 ;;
esac
""")
    kubectl.chmod(0o700)
    assert read("kube", "contexts") == ["development", "staging"]
    assert read("kube", "counts", "--context", "development")["pods"] == 1
    assert read("kube", "list", "pod", "--context", "development", "-A")["items"][0]["metadata"]["uid"] == "1"
    assert read("kube", "list", "pod", "-n", "apps")["namespace"] == "apps"
    assert read("kube", "inspect", "pod", "web", "-n", "apps")["metadata"]["uid"] == "1"
    run("kube", "list", "pod", "-A", "-n", "apps", ok=False)
    migration = ("kube", "migrate", "pod", "web", "--from", "development", "--to", "staging")
    wrong = run(*migration, "--uid", "wrong", ok=False)
    assert "source object changed" in wrong.stderr
    assert not (root / "created.json").exists()
    assert read(*migration, "--uid", "1")["verified"]
    created = json.loads((root / "created.json").read_text())
    assert created["metadata"] == {"name": "web", "namespace": "review"}

    with ThreadPoolExecutor(3) as pool:
        tasks = [
            pool.submit(read, "settings", "language", "ko"),
            pool.submit(read, "settings", "theme", "dark"),
            pool.submit(read, "settings", "sidebar", "--visible", "true", "--width", "24"),
        ]
        for task in tasks:
            task.result()
    look = read("settings", "show")
    assert look == {"language": "ko", "theme": "dark", "sidebar": True, "sidebar_width": 24}
    run("settings", "sidebar", "--width", "99", ok=False)
    assert read("settings", "registry", "host", "custom", "registry.example.test")["host"] == "registry.example.test"
    assert read("settings", "registry", "show", "custom")["host"] == "registry.example.test"
    assert read("engine", "pull", "demo:stable", "--registry", "custom")["image"] == "registry.example.test/demo:stable"
    assert destination.requests[-1] == "/images/create?fromImage=registry.example.test%2Fdemo&tag=stable"

    if sys.platform == "linux":
        # XDG startup is isolated by HOME. The executable is only inspected by auto-launch.
        env["PODBAG_BIN"] = str(root / "podbag")
        (root / "podbag-desktop").touch()
        assert not read("settings", "startup")["enabled"]
        assert read("settings", "startup", "enable")["enabled"]
        assert (root / ".config" / "autostart" / "podbag.desktop").is_file()
        assert not read("settings", "startup", "disable")["enabled"]
        assert not (root / ".config" / "autostart" / "podbag.desktop").exists()
        env["PODBAG_BIN"] = str(binary)

    words = dict(title="HTTP lesson", goal="Test a response.", create="Name a volume.",
                 write="Write a response.", serve="Serve the file.", request="Read the response.")
    module = root / "http.podbag"
    module.write_text(json.dumps({"format": "podbag-volume-api-lab-v1", "starter": '{"answer":42}',
                                  "translations": {"en": words}}))
    assert read("tutorial", "install", module)["instructions"]["title"] == words["title"]
    assert not read("tutorial", "status")["complete"]
    outcome = read("tutorial", "run", "practice", "--input", "-", input='{"hello":"world"}')
    assert outcome["response"] == {"status": 200, "body": '{"hello":"world"}'}
    assert read("tutorial", "status")["complete"]

    assert not read("tutorial", "install", module)["complete"]
    proc = subprocess.Popen([binary, "tutorial", "session"], env=env, text=True,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE)
    try:
        def send(action, value=None):
            proc.stdin.write(json.dumps({"action": action, "value": value}) + "\n")
            proc.stdin.flush()
            return json.loads(proc.stdout.readline())

        assert not send("request")["ok"]
        assert send("start")["state"]["active"]
        assert send("create", "session-data")["state"]["volume"] == "session-data"
        assert send("save")["state"]["source"] == '{"answer":42}'
        url = send("serve")["state"]["url"]
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        with opener.open(url, timeout=3) as reply:
            assert json.load(reply) == {"answer": 42}
        assert send("request")["state"]["response"]["status"] == 200
        assert not send("end")["state"]["active"]
        assert read("tutorial", "status")["complete"]
    finally:
        proc.stdin.close()
        assert proc.wait(timeout=5) == 0
    assert not read("tutorial", "remove")["installed"]


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="pb-cli-", dir="/tmp") as tmp:
        root = Path(tmp)
        with Engine(root / "docker.sock", CLIHandler) as source, Engine(root / "engine.sock", CLIHandler) as destination:
            for engine in (source, destination):
                threading.Thread(target=engine.serve_forever, daemon=True).start()
            try:
                exercise(args.binary.resolve(), root, source, destination)
            finally:
                source.shutdown()
                destination.shutdown()
    print("PASS: scripting commands for engines, imports, Kubernetes, settings and tutorials")
