"""Exercise the terminal dashboard in a private tmux session and Docker API socket."""
import argparse
import io
import json
import os
from pathlib import Path
import socketserver
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler


def check_translations():
    repo = Path(__file__).resolve().parents[2]
    labels = json.loads((repo / "src/bin/podbag/tui/labels.json").read_text())
    for index, language in enumerate(("en", "ko", "ja", "zh-CN", "fil", "es", "vi")):
        desktop = json.loads((repo / "desktop/ui/i18n" / (language + ".json")).read_text())
        for key, translations in labels.items():
            assert len(translations) == 7 and translations[index], (language, key)
            if key in desktop:
                assert translations[index] == desktop[key], (language, key)


class Engine(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self, path, handler=None):
        self.volumes = []
        self.requests = []
        self.imports = []
        super().__init__(str(path), handler or Handler)


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def reply(self, status, value):
        data = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def image_archive(self):
        output = io.BytesIO()
        with tarfile.open(fileobj=output, mode="w") as tar:
            for path, value in [("manifest.json", [{"Config": "config.json", "RepoTags": ["demo:stable"]}]),
                                ("config.json", {"os": "linux", "rootfs": {"diff_ids": []}})]:
                data = json.dumps(value).encode()
                entry = tarfile.TarInfo(path)
                entry.size = len(data)
                tar.addfile(entry, io.BytesIO(data))
        return output.getvalue()

    def do_GET(self):
        path = self.path.split("?")[0]
        if path == "/images/get":
            self.server.requests.append(self.path)
            archive = self.image_archive()
            self.send_response(200)
            self.send_header("Content-Length", str(len(archive)))
            self.end_headers()
            self.wfile.write(archive)
        elif path.startswith("/images/") and path.endswith("/json") and path != "/images/json":
            if self.server.server_address.endswith("/docker.sock") or self.server.imports:
                self.reply(200, {"Id": "sha256:1234"})
            else:
                self.reply(404, {"message": "No such image"})
        elif path == "/containers/json":
            self.reply(200, [{"Id": "abcd", "Names": ["/sleeping"], "Image": "demo:stable",
                              "State": "paused", "Status": "Paused"}])
        elif path == "/images/json":
            self.reply(200, [{"Id": "sha256:1234", "RepoTags": ["demo:first", "demo:stable"]}])
        elif path == "/volumes":
            self.reply(200, {"Volumes": [{"Name": v, "Driver": "local"} for v in self.server.volumes]})
        elif path == "/networks":
            self.reply(200, [])
        elif path == "/_ping":
            self.reply(200, "OK")
        else:
            self.reply(404, {"message": "Unsupported fixture request"})

    def do_POST(self):
        if self.path == "/images/load":
            archive = bytearray()
            while size := int(self.rfile.readline(), 16):
                archive.extend(self.rfile.read(size))
                self.rfile.readline()
            self.rfile.readline()
            self.server.imports.append(bytes(archive))
            self.reply(200, {"stream": "Loaded image"})
        elif self.path == "/volumes/create":
            name = json.loads(self.rfile.read(int(self.headers["Content-Length"])))["Name"]
            self.server.volumes.append(name)
            self.reply(201, {"Name": name})
        else:
            self.reply(404, {"message": "Unsupported fixture request"})

    def do_DELETE(self):
        self.server.requests.append(self.path)
        self.reply(200, {})


def exercise(binary, root, engine, docker):
    socket = root / "tmux.sock"
    env = {**os.environ, "PODBAG_UI_DIR": str(root), "DOCKER_CONFIG": str(root),
           "PODBAG_NO_UPDATE_CHECK": "1", "PODBAG_BIN": str(root / "test-podbag"),
           "PODBAG_MAC_HOME": str(root), "HOME": str(root), "DOCKER_HOST": "unix://" + docker.server_address,
           "LANG": "C"}
    (root / "run").mkdir()
    (root / "run" / "docker.sock").symlink_to(root / "engine.sock")
    (root / "vm-state").write_text("running")
    (root / "test-podbag").write_text("""#!/bin/sh
case "$*" in
  'login --help') exit 0 ;;
  'login ghcr.io --username tester --password-stdin') [ "$(cat)" = "fixture-token" ] && touch "$PODBAG_UI_DIR/logged-in" ;;
  '--version') echo 'podbag 0.2.0' ;;
  'upgrade --check --json') echo '{"current":"0.2.0","latest":"0.3.0"}' ;;
  'upgrade --yes') echo 'fixture updated' > "$PODBAG_UI_DIR/updated"; cat "$PODBAG_UI_DIR/updated" ;;
  'vm stop') printf stopped > "$PODBAG_UI_DIR/vm-state" ;;
  'vm start'|'vm restart --yes') printf running > "$PODBAG_UI_DIR/vm-state" ;;
  'vm status --json') printf '{"state":"%s","resourcesEditable":true,"containers":2,"next":{"cpus":2,"memoryMiB":2048,"idleSeconds":60}}' "$(cat "$PODBAG_UI_DIR/vm-state")" ;;
  'vm set '*) echo "$*" > "$PODBAG_UI_DIR/vm-args"; echo 'fixture settings saved' ;;
  *) exit 1 ;;
esac
""")
    (root / "test-podbag").chmod(0o700)
    (root / "volume-api-tutorial.json").write_text("invalid old record")
    (root / "terminal.json").write_text('{"language":"en"}')
    kubectl = root / "kubectl"
    kubectl.write_text("""#!/bin/sh
case "$*" in
  'config get-contexts -o name') printf 'development\\nstaging\\n' ;;
  'config view'*) printf '{"name":"development","context":{"namespace":"apps"}}' ;;
  '--context=development --namespace=apps get pods web '*) printf '{"apiVersion":"v1","kind":"Pod","metadata":{"name":"web","uid":"1"},"spec":{}}' ;;
  '--context=staging --namespace=apps create '*) cat > "$PODBAG_UI_DIR/created.json"; printf '{"metadata":{"uid":"2"}}' ;;
  '--context=staging --namespace=apps get pods web '*) printf '{"metadata":{"uid":"2"}}' ;;
  *custom-columns*) printf 'Pod\\n' ;;
  'get pods '*) printf '{"items":[{"kind":"Pod","metadata":{"uid":"1","name":"web","namespace":"apps"},"status":{"phase":"Running"}}]}' ;;
  *) exit 1 ;;
esac
""")
    kubectl.chmod(0o700)
    env["PATH"] = str(root) + os.pathsep + env["PATH"]
    package = {"format": "podbag-volume-api-lab-v1", "starter": '{"answer":42}',
               "translations": {"en": {"title": "HTTP lesson", "goal": "Test a response.",
                                       "create": "Name a volume.", "write": "Write a response.",
                                       "serve": "Serve the file.", "request": "Read the response."}}}
    lesson = root / "http.podbag"
    lesson.write_text(json.dumps(package))

    def tmux(*args):
        return subprocess.check_output(["tmux", "-f", "/dev/null", "-S", str(socket), *args],
                                       text=True, env=env)

    def keys(*args):
        tmux("send-keys", "-t", "audit", *args)

    def enter(text):
        tmux("send-keys", "-t", "audit", "-l", text)
        keys("Enter")

    def wait(text):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            view = tmux("capture-pane", "-p", "-t", "audit")
            if text in view:
                return
            time.sleep(0.04)
        raise AssertionError(f"Missing {text!r}\n{view}")

    def action(name):
        keys("a")
        wait("Settings")
        enter(name)

    def setting(name):
        action("Settings")
        wait("Language")
        enter(name)

    tmux("new-session", "-d", "-x", "115", "-y", "32", "-s", "audit", str(binary),
         "tui", "--no-mouse", "--engine", "unix://" + str(root / "engine.sock"))
    try:
        wait("sleeping")
        keys("d")
        wait("Stop and remove")
        keys("y")
        wait("Removed sleeping")
        assert engine.requests == ["/containers/abcd?force=1"]
        keys("2", "/")
        enter("stable")
        wait("demo:first")
        wait("sleeping")
        keys("3", "v")
        wait("Create volume")
        enter("test-data")
        wait("test-data")
        assert engine.volumes == ["test-data"]
        keys("a")
        wait("Kubernetes")
        enter("Kubernetes")
        wait("Current context")
        keys("Enter")
        wait("All namespaces")
        keys("Enter")
        wait("Pods (1)")
        keys("Enter")
        wait("apps/web")
        keys("Enter")
        wait('"uid": "1"')
        keys("Escape")
        wait("apps/web")
        keys("Escape")
        wait("a actions")
        action("Migrate")
        wait("Kubernetes")
        enter("Kubernetes")
        wait("Current context")
        keys("Enter")
        wait("All namespaces")
        keys("Enter")
        wait("Pods (1)")
        keys("Enter")
        wait("apps/web")
        keys("Enter")
        wait("staging")
        keys("Enter")
        wait("Destination namespace")
        keys("Enter")
        wait("Start migration")
        keys("Down", "Enter")
        wait("destination resource has been verified")
        created = json.loads((root / "created.json").read_text())
        assert created["metadata"] == {"name": "web", "namespace": "apps"}
        keys("Enter")
        wait("a actions")

        keys("F7")
        wait("Engine")
        enter("Engine")
        wait("All local engines")
        enter("docker")
        wait("Images")
        enter("Images")
        wait("demo:stable")
        enter("stable")
        wait("Custom Unix socket")
        enter("Custom Unix socket")
        wait("unix://")
        enter(str(root / "engine.sock"))
        wait("Start migration")
        keys("Down", "Enter")
        wait("destination resource has been verified")
        assert len(engine.imports) == 1
        with tarfile.open(fileobj=io.BytesIO(engine.imports[0])) as result:
            assert "config.json" in result.getnames()
        keys("Enter")
        wait("a actions")

        keys("b")
        wait("F7 Migrate")
        keys("]")
        wait("F7 Migrate")
        keys("b")
        wait("a actions")
        keys("2", "p", "F2")
        wait("Amazon ECR")
        enter("GitHub")
        wait("F3 login")
        keys("F3")
        wait("User")
        enter("tester")
        wait("Password or token")
        tmux("send-keys", "-t", "audit", "-l", "fixture-token")
        wait("••••")
        assert "fixture-token" not in tmux("capture-pane", "-p", "-t", "audit")
        keys("Enter")
        wait("Pull image")
        assert (root / "logged-in").exists()
        keys("Escape")
        wait("a actions")

        setting("Updates")
        wait("Check for updates")
        enter("Check for updates")
        wait("0.3.0 is available")
        keys("Enter")
        wait("Update podbag")
        enter("Update podbag")
        wait("Cancel")
        keys("Down", "Enter")
        wait("fixture updated")
        assert (root / "updated").exists()
        keys("Enter")
        wait("a actions")

        if sys.platform == "darwin":
            # Podbag's native engine points only to our private socket.
            setting("Engine")
            wait("Connect")
            keys("Enter")
            wait("podman")
            enter("podbag")
            wait("sleeping")
            assert json.loads((root / "ui.json").read_text())["engine"] == "podbag"
            action("Import from Docker")
            wait("demo:stable")
            keys("Space", "Down", "Space", "Enter")
            wait("Copy to podbag")
            keys("Down", "Enter")
            wait("a actions")
            assert len(engine.imports) == 2
            assert engine.imports[0] == engine.imports[1]
            assert "/images/get?names=demo%3Afirst&names=demo%3Astable" in docker.requests

            setting("Virtual machine")
            wait("Restart VM")
            enter("Apply")
            wait("vCPU")
            keys("C-u")
            enter("4")
            wait("MiB")
            keys("Enter")
            wait("Idle timeout")
            keys("Enter")
            wait("Restart now")
            keys("Enter")
            wait("4 vCPU")
            keys("Down", "Enter")
            wait("fixture settings saved")
            assert (root / "vm-args").read_text().strip() == "vm set --cpus 4 --memory 2048M --idle 60s"
            keys("Enter")
            wait("a actions")
            for verb in ("Restart VM", "Stop VM", "Start VM"):
                setting("Virtual machine")
                wait(verb)
                enter(verb)
                if verb != "Start VM":
                    wait("Containers that will stop: 2")
                    keys("Down", "Enter")
                wait("a actions")
                expected = "stopped" if verb == "Stop VM" else "running"
                assert (root / "vm-state").read_text() == expected
        keys("a")
        wait("Tutorials")
        enter("Tutorials")
        wait("podbag.dev/blog/")
        keys("Enter")
        wait("Choose file")
        enter(str(lesson))
        wait("Install module")
        keys("Down", "Enter")
        wait("Start lab")
        keys("Enter")
        wait("Name a volume.")
        keys("Enter")
        wait("Volume name")
        enter("practice")
        wait("Write a response.")
        keys("Enter")
        wait("response.json")
        keys("C-s")
        wait("Serve the file.")
        keys("Enter")
        wait("Read the response.")
        keys("Enter")
        wait("HTTP 200")
        wait('{"answer":42}')
        assert json.loads((root / "volume-api-tutorial.json").read_text())["complete"]
        keys("Enter")
        wait("End lab")
        enter("End lab")
        wait("Cancel")
        keys("Down", "Enter")
        wait("Lesson complete")
        keys("Escape")
        wait("a actions")
        setting("Engine")
        wait("Connect")
        keys("Enter")
        wait("Custom Unix socket")
        enter("Custom Unix socket")
        wait("unix://")
        enter(str(root / "offline.sock"))
        wait("offline.sock")
        keys("F9")
        wait("Language")
        enter("Language")
        wait("한국어")
        # System, English, Korean. Send keys through the terminal like a local user.
        keys("Down", "Down", "Enter")
        wait("컨테이너")
        keys("F9")
        wait("테마")
        enter("테마")
        wait("어둡게")
        enter("어둡게")
        wait("컨테이너")
        saved = json.loads((root / "terminal.json").read_text())
        assert saved["language"] == "ko" and saved["theme"] == "dark"
        keys("q")
    finally:
        subprocess.run(["tmux", "-S", str(socket), "kill-server"], capture_output=True)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    check_translations()
    with tempfile.TemporaryDirectory(prefix="pb-tui-", dir="/tmp") as tmp:
        root = Path(tmp)
        with Engine(root / "engine.sock") as engine, Engine(root / "docker.sock") as docker:
            for server in (engine, docker):
                threading.Thread(target=server.serve_forever, daemon=True).start()
            try:
                exercise(args.binary.resolve(), root, engine, docker)
            finally:
                engine.shutdown()
                docker.shutdown()
    print("PASS: resources, registry login, engine and Kubernetes migrations, updates and the HTTP lesson")
    if sys.platform == "darwin":
        print("PASS: engine selection, Docker image import and VM settings")
