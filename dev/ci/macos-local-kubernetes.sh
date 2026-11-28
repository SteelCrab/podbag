#!/usr/bin/env bash
# Exercise Desktop's Kind lifecycle against a private Virtualization.framework VM.
set -euo pipefail

if [[ $# != 1 || ! -x "$1" ]]; then
  echo "Usage: $0 PATH_TO_SIGNED_PODBAG" >&2
  exit 2
fi

case "$(uname -sm)" in
  "Darwin x86_64")
    arch=amd64
    docker_arch=x86_64
    kind_sha=5a99f26f57246dc9319dd294803313197a0f34d33c525b3ea8b655db5916ece0
    kubectl_sha=71a3aa7c2ee2c974d9fbb462cba0c5c04a4df2e8d85eee94714fd819ea3c4e63
    docker_sha=f7d8f835391cd0a8c676d9a8d2547f909da6a2d029595943b0ce8d5f169abe47
    ;;
  "Darwin arm64")
    arch=arm64
    docker_arch=aarch64
    kind_sha=0c8c7dbe5e23594a198b786c4bc13dacc101fa6196b0cb0b23a1ca44e61f4b4f
    kubectl_sha=c9e4f713d6fee0043a3d835cca13077cda2bc0973840eb9779360df0b5bdfc69
    docker_sha=79145e169bca0b9571d02de8e1cd311a36e0e8fdb4599b6661a87b1bb3afbe13
    ;;
  *)
    echo "The local Kubernetes VM test requires macOS." >&2
    exit 2
    ;;
esac

export LC_ALL=C
PODBAG_BIN="$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
export PODBAG_BIN
# Keep Unix socket paths short enough for macOS.
scratch=$(mktemp -d /tmp/podbag-kube.XXXXXX)
export PODBAG_MAC_HOME="$scratch/engine" PODBAG_UI_DIR="$scratch/desktop"
export PODBAG_VM_CPUS="${PODBAG_VM_CPUS:-2}" PODBAG_VM_MEMORY=6144 PODBAG_VM_IDLE=0
export KUBECONFIG="$scratch/external-kubeconfig" DOCKER_CONFIG="$scratch/docker-config"
export DOCKER_HOST="unix://$scratch/unused.sock"
export PODBAG_TEST_KIND_DIAGNOSTICS=1
unset DOCKER_CONTEXT DOCKER_TLS DOCKER_TLS_VERIFY DOCKER_CERT_PATH DOCKER_API_VERSION
unset KIND_EXPERIMENTAL_DOCKER_NETWORK

cleanup() {
  local status=$? log
  if ! "$PODBAG_BIN" vm stop; then
    echo "Could not stop the private VM. Its files are in $scratch." >&2
    if [[ "$status" == 0 ]]; then
      status=1
    fi
    exit "$status"
  fi
  if [[ "$status" != 0 ]]; then
    for log in "$PODBAG_MAC_HOME/console.log" "$PODBAG_MAC_HOME/run/host.log"; do
      if [[ -f "$log" ]]; then
        echo "== $log"
        tail -n 150 "$log"
      fi
    done
  fi
  rm -rf "$scratch"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

mkdir -p "$scratch/bin" "$PODBAG_MAC_HOME" "$PODBAG_UI_DIR" "$DOCKER_CONFIG"
download() {
  local url=$1 file=$2 sha=$3
  curl --fail --silent --show-error --location --retry 3 --proto '=https' --output "$file" "$url"
  printf '%s  %s\n' "$sha" "$file" | shasum -a 256 --check
}

kind_version=v0.33.0
kubectl_version=v1.36.4
docker_version=29.9.3
download "https://github.com/kubernetes-sigs/kind/releases/download/$kind_version/kind-darwin-$arch" "$scratch/bin/kind" "$kind_sha"
download "https://dl.k8s.io/release/$kubectl_version/bin/darwin/$arch/kubectl" "$scratch/bin/kubectl" "$kubectl_sha"
download "https://download.docker.com/mac/static/stable/$docker_arch/docker-$docker_version.tgz" "$scratch/docker.tgz" "$docker_sha"
tar -xOzf "$scratch/docker.tgz" docker/docker > "$scratch/bin/docker"
chmod +x "$scratch/bin/kind" "$scratch/bin/kubectl" "$scratch/bin/docker"
export PATH="$scratch/bin:$PATH"

cat > "$KUBECONFIG" <<'CONFIG'
apiVersion: v1
kind: Config
clusters:
- name: external-demo
  cluster:
    server: https://127.0.0.1:1
contexts:
- name: external-demo
  context:
    cluster: external-demo
    user: external-demo
current-context: external-demo
users:
- name: external-demo
  user: {}
CONFIG

kind --version
kubectl version --client
docker --version
test_name=dashboard::local_kubernetes::tests::macos_desktop_manages_kind_in_its_private_vm
cargo test --locked --lib "$test_name" -- --ignored --exact --nocapture | tee "$scratch/test.log"
grep -qFx "test $test_name ... ok" "$scratch/test.log"
