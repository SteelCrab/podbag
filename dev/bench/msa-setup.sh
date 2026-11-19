#!/usr/bin/env bash
# The images of dev/bench/msa.sh, in a registry on this host (127.0.0.1:5000), so that
# every tool pulls the same bytes without Docker Hub's limits:
#   petclinic/<service>:n      the eight services of Spring PetClinic Microservices as
#                              published (springcommunity/spring-petclinic-*, arm64),
#   petclinic/<service>:n1     the same services rebuilt by a CI pipeline on a newer base,
#                              eclipse-temurin:17-jre (the published images use the full
#                              JDK, eclipse-temurin:17), with the application layers
#                              extracted again (same jars, new times).
# The registry is the distribution binary taken from the registry:2 image, run as a plain
# process (no container engine of its own, so that it is in no tool's figures).
#
# usage: dev/bench/msa-setup.sh            (dev/bench/msa-setup.sh stop: stop the registry)
set -euo pipefail

reg_dir=/var/tmp/msa-registry
bin=/var/tmp/msa-registry-bin/registry
addr=127.0.0.1:5000
services=(config-server discovery-server api-gateway customers-service vets-service
  visits-service admin-server genai-service)

if [ "${1:-}" = stop ]; then
  pkill -f "$bin serve" || true
  exit 0
fi

# The registry binary, from the official image.
if [ ! -x "$bin" ]; then
  tmp="$(mktemp -d)"
  skopeo copy -q --override-arch "$(dpkg --print-architecture)" \
    docker://mirror.gcr.io/library/registry:2 "dir:$tmp/img"
  mkdir -p "$(dirname "$bin")"
  for layer in $(python3 -c "import json,sys; print(' '.join(l['digest'].split(':')[1] for l in json.load(open('$tmp/img/manifest.json'))['layers']))"); do
    tar -xzf "$tmp/img/$layer" -C "$tmp" bin/registry 2>/dev/null || true
  done
  cp "$tmp/bin/registry" "$bin"
  rm -rf "$tmp"
fi
if ! curl -fs "http://$addr/v2/" >/dev/null 2>&1; then
  mkdir -p "$reg_dir"
  cat >"$reg_dir.yml" <<EOF
version: 0.1
log:
  level: warn
storage:
  filesystem:
    rootdirectory: $reg_dir
  delete:
    enabled: true
http:
  addr: $addr
EOF
  nohup "$bin" serve "$reg_dir.yml" >"$reg_dir.log" 2>&1 &
  for _ in $(seq 50); do curl -fs "http://$addr/v2/" >/dev/null 2>&1 && break; sleep 0.2; done
fi

have() { skopeo inspect --tls-verify=false "docker://$addr/petclinic/$1" >/dev/null 2>&1; }

# Release n: the published images.
for s in "${services[@]}"; do
  have "$s:n" && continue
  skopeo copy -q --override-arch "$(dpkg --print-architecture)" --dest-tls-verify=false \
    "docker://docker.io/springcommunity/spring-petclinic-$s:3.4.1" "docker://$addr/petclinic/$s:n"
done

# Release n+1: rebuilt on a newer base, in a builder store of its own (removed after).
builder=(podman --root /var/tmp/msa-builder/storage --runroot /var/tmp/msa-builder/run)
ctx=/var/tmp/msa-builder/ctx
mkdir -p "$ctx"
cat >"$ctx/Dockerfile" <<'EOF'
ARG SRC
FROM ${SRC} AS src

# A new extraction, as `java -Djarmode=tools extract --layers` does in each CI build: the
# same files, with this build's times.
FROM mirror.gcr.io/library/eclipse-temurin:17-jre AS extract
COPY --from=src /application /src
RUN mkdir -p /out/deps/BOOT-INF /out/loader /out/app/BOOT-INF \
 && cp -r --no-preserve=timestamps /src/BOOT-INF/lib /out/deps/BOOT-INF/ \
 && cp -r --no-preserve=timestamps /src/org /out/loader/ \
 && cp -r --no-preserve=timestamps /src/BOOT-INF/classes /src/BOOT-INF/classpath.idx \
      /src/BOOT-INF/layers.idx /out/app/BOOT-INF/ \
 && cp -r --no-preserve=timestamps /src/META-INF /out/app/

FROM mirror.gcr.io/library/eclipse-temurin:17-jre
ARG PORT
# What the published image sets on top of its base.
ENV SPRING_PROFILES_ACTIVE=docker
EXPOSE ${PORT}
WORKDIR /application
COPY --from=extract /out/deps/ ./
COPY --from=extract /out/loader/ ./
COPY --from=extract /out/app/ ./
ENTRYPOINT ["java", "org.springframework.boot.loader.launch.JarLauncher"]
EOF
built=0
for s in "${services[@]}"; do
  have "$s:n1" && continue
  port="$(skopeo inspect --tls-verify=false --config "docker://$addr/petclinic/$s:n" |
    python3 -c "import json,sys; print(next(iter(json.load(sys.stdin)['config'].get('ExposedPorts') or {'8080/tcp': 0})).split('/')[0])")"
  "${builder[@]}" build -q --tls-verify=false --build-arg "SRC=$addr/petclinic/$s:n" \
    --build-arg "PORT=$port" \
    -t "localhost/petclinic/$s:n1" "$ctx" >/dev/null
  "${builder[@]}" push -q --tls-verify=false "localhost/petclinic/$s:n1" \
    "docker://$addr/petclinic/$s:n1"
  built=1
done
if [ "$built" = 1 ]; then
  "${builder[@]}" system reset -f >/dev/null 2>&1 || true
  rm -rf /var/tmp/msa-builder
fi

for r in n n1; do
  for s in "${services[@]}"; do
    skopeo inspect --tls-verify=false "docker://$addr/petclinic/$s:$r" |
      python3 -c "import json,sys; d=json.load(sys.stdin); print('$r $s', ' '.join(l['Digest'][7:19]+':'+str(l['Size']) for l in d['LayersData']))"
  done
done >/var/tmp/msa-registry.layers
echo "registry at $addr: $(du -sh "$reg_dir" | cut -f1)"
