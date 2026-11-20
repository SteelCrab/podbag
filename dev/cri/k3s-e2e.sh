#!/bin/sh
# End-to-end checks on the k3s + podbag cluster that dev/cri/k3s.sh started: a Deployment
# behind a Service, logs, exec, attach, port forwarding, a Job, probes, a restart after a
# failure and an out-of-memory kill. Runs as root, on the node.
#
# usage: sudo dev/cri/k3s-e2e.sh K3S
set -eu
k3s=${1:?usage: k3s-e2e.sh K3S}
k() { "$k3s" kubectl "$@"; }
ns=pbg-e2e
check() {
  if ! echo "$2" | grep -q -- "$3"; then
    echo "FAIL: $1: expected '$3' in:"
    echo "$2"
    k -n $ns get pods -o wide || true
    k -n $ns describe pods | tail -60 || true
    exit 1
  fi
  echo "ok: $1"
}
cleanup() { k delete namespace $ns --wait=false >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Left from an earlier run: gone first.
k delete namespace $ns --ignore-not-found --timeout=180s
k create namespace $ns
# Pods need the namespace's default service account, which comes a moment later.
i=0
until k -n $ns get serviceaccount default >/dev/null 2>&1; do
  i=$((i + 1)); [ $i -lt 60 ] || break; sleep 1
done
k -n $ns apply -f - <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: {name: web}
spec:
  replicas: 2
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec:
      containers:
      - name: nginx
        image: nginx:alpine
        ports: [{containerPort: 80}]
        readinessProbe: {httpGet: {path: /, port: 80}, periodSeconds: 2}
        livenessProbe: {exec: {command: [nginx, -t]}, periodSeconds: 5}
        resources: {limits: {memory: 64Mi, cpu: 500m}}
---
apiVersion: v1
kind: Service
metadata: {name: web}
spec:
  selector: {app: web}
  ports: [{port: 80, targetPort: 80}]
---
apiVersion: batch/v1
kind: Job
metadata: {name: once}
spec:
  backoffLimit: 0
  template:
    spec:
      restartPolicy: Never
      containers:
      - {name: job, image: busybox:1.36, command: [sh, -c, "echo job done"]}
---
apiVersion: v1
kind: Pod
metadata: {name: flaky}
spec:
  restartPolicy: OnFailure
  containers:
  - {name: flaky, image: busybox:1.36, command: [sh, -c, "echo try; sleep 1; exit 1"]}
---
apiVersion: v1
kind: Pod
metadata: {name: hungry}
spec:
  restartPolicy: Never
  containers:
  - name: hungry
    image: busybox:1.36
    command: [sh, -c, "sleep 1; head -c 200m /dev/zero | tail"]
    resources: {limits: {memory: 32Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: talker}
spec:
  containers:
  - name: talker
    image: busybox:1.36
    command: [sh, -c, "while true; do echo tick; sleep 1; done"]
EOF

k -n $ns rollout status deploy/web --timeout=300s
check "deployment ready" "$(k -n $ns get deploy web)" ' 2/2 '
check "kubectl exec" "$(k -n $ns exec deploy/web -- nginx -v 2>&1)" 'nginx version'
check "kubectl exec with stdin" "$(echo from-stdin | k -n $ns exec -i deploy/web -- cat)" 'from-stdin'
k -n $ns wait --for=condition=complete job/once --timeout=180s
check "job logs" "$(k -n $ns logs job/once)" 'job done'
check "service by name, from a pod" \
  "$(k -n $ns exec talker -- wget -qO- http://web.$ns.svc.cluster.local/ 2>&1)" 'Welcome to nginx'
check "kubectl logs" "$(k -n $ns logs talker --tail=2)" 'tick'
check "kubectl attach" "$(timeout -s KILL 4 "$k3s" kubectl -n $ns attach talker 2>&1 || true)" 'tick'
k -n $ns port-forward svc/web 18081:80 >/tmp/pbg-e2e-pf.log 2>&1 &
pf=$!
sleep 3
check "kubectl port-forward" "$(wget -qO- http://127.0.0.1:18081/ 2>&1)" 'Welcome to nginx'
kill $pf
i=0
until [ "$(k -n $ns get pod flaky -o jsonpath='{.status.containerStatuses[0].restartCount}')" -ge 1 ] 2>/dev/null; do
  i=$((i + 1)); [ $i -lt 90 ] || break; sleep 1
done
check "restart after a failure" \
  "$(k -n $ns get pod flaky -o jsonpath='{.status.containerStatuses[0].restartCount}')" '^[1-9]'
i=0
until k -n $ns get pod hungry -o jsonpath='{.status.containerStatuses[0].state.terminated.reason}' | grep -q .; do
  i=$((i + 1)); [ $i -lt 90 ] || break; sleep 1
done
check "out of memory" \
  "$(k -n $ns get pod hungry -o jsonpath='{.status.containerStatuses[0].state.terminated.reason}')" 'OOMKilled'
check "probes keep it ready" "$(k -n $ns get pods -l app=web -o jsonpath='{.items[*].status.containerStatuses[0].ready}')" '^true true$'
echo "PASS"
