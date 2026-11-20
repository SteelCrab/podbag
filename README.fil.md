# podbag

<img src="desktop/icons/128x128.png" alt="podbag" width="120" align="right">

Isang container engine na walang daemon, sa iisang static binary.
Iniimbak nito ang mga image nang file-by-file: ang file na ginagamit ng maraming image ay iniimbak nang isang beses lang.
Puwede mong gamitin ang command line o ang [Podbag Desktop](#podbag-desktop).

[![CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml/badge.svg)](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SteelCrab/podbag)](https://github.com/SteelCrab/podbag/releases/latest)
[![crates.io](https://img.shields.io/crates/v/podbag)](https://crates.io/crates/podbag)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Website](https://img.shields.io/badge/website-podbag.dev-2a78d6)](https://podbag.dev)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md) · **Filipino** · [Español](README.es.md) · [Tiếng Việt](README.vi.md)

<img src="docs/images/intro.svg" alt="Intro video ng podbag. Laging tumatakbo ang mga daemon ng ibang engine; tumatakbo lang ang podbag habang may container, at walang tumatakbo kapag nakapahinga. Mga layer ang iniimbak ng iba; mga file ang iniimbak ng podbag, bawat isa nang isang beses: 902 MiB na disk sa halip na 2,970 MiB para sa 8 Spring service sa 2 release, at 324 MiB na page cache sa halip na 982 MiB. Nagsisimula ang container sa 4.1 ms, 18 beses na mas mabilis kaysa sa 73.8 ms ng Docker. Linux, macOS at Windows; podbag.dev." width="800">

<sub>Ang parehong intro bilang video (Ingles): [intro.mp4](docs/media/intro.mp4).</sub>

## Nilalaman

- [Mga tampok](#mga-tampok)
- [Podbag Desktop](#podbag-desktop)
- [Pag-install](#pag-install)
- [Paggamit](#paggamit)
- [Mga benchmark](#mga-benchmark)
- [Paghahambing sa ibang tool](#paghahambing-sa-ibang-tool)
- [Dokumentasyon](#dokumentasyon)
- [Pag-ambag](#pag-ambag)
- [Lisensya](#lisensya)

## Mga tampok

- **Pinagsasaluhang file ng mga image.** Maaaring gumamit ng iisang kopya sa disk at sa page cache ng Linux ang magkaparehong file kahit nasa magkaibang layer ang mga ito.
- **Tumatakbo ang Linux engine kapag kailangan.** Iisang proseso ang nagbabantay sa mga container ng bawat user. Kapag huminto ang huling container, magsasara ang proseso pagkalipas ng itinakdang oras ng paghihintay.
- **Kusang pagsara ng VM sa macOS.** Sa default na setting, nagsasara ang Linux VM kapag 60 segundo na itong walang ginagawa. Muli itong bubukas sa susunod na command para sa container.
- **Runtime para sa Kubernetes node (`main`).** Sa Linux, magagamit ng kubelet ang Podbag para direktang magpatakbo ng mga pod sa pamamagitan ng `podbag cri serve`.
- **Paglipat sa ibang engine (`main`).** Maaaring kopyahin ang mga image, volume at nakahintong container mula sa isang Docker-compatible na engine papunta sa isa pa. Mananatili ang orihinal sa pinagmulan.

<picture><source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="docs/images/storage-fil-tall-dark.svg"><source media="(max-width: 600px)" srcset="docs/images/storage-fil-tall-light.svg"><source media="(prefers-color-scheme: dark)" srcset="docs/images/storage-fil-dark.svg"><img alt="Animation. Halos pareho ang mga file ng dalawang image. Sa mga layer, isang beses lang iniimbak ang layer kapag pareho ito; iba ang isang file sa itaas na layer ng image B, kaya iniimbak ulit ang spring.jar at jackson.jar: 8 file. Iniimbak ng podbag ang bawat file nang isang beses: 6 file. Sinukat sa 8 Spring service sa 2 release: 2,970 MiB sa disk gamit ang mga layer (Docker), 902 MiB gamit ang podbag." src="docs/images/storage-fil-light.svg"></picture>

## Podbag Desktop

Sa Linux at macOS, puwede mong pamahalaan ang mga container, image, volume at network. Eksperimental ang suporta sa Windows at kailangan nito ng WSL 2. Mula sa listahan, puwede mong simulan, ihinto o i-restart ang isang container. Buksan ito para makita ang mga log habang dumarating ang mga ito.

![Listahan sa Podbag Desktop ng tatlong tumatakbo at dalawang nakahintong halimbawa ng container, kasama ang mga image at port ng mga ito](docs/images/desktop-containers-en.png)

[Mag-install ng package para sa Linux](docs/build.md#linux-desktop-packages) o [i-build ang desktop app mula sa source](docs/usage.md#build-the-desktop-app).

## Pag-install

### Linux (x86_64, arm64) at macOS (Apple silicon)

```sh
curl -fsSL https://podbag.dev/install.sh | sh
podbag --version
```

Sinusuri ng installer ang SHA-256 checksum ng release file at ini-install ang CLI sa `/usr/local/bin`. Patakbuhin ulit ang script para mag-update.

Kailangan ang Linux kernel 5.12 o mas bago at cgroup v2. Patakbuhin ang `podbag check` para makita kung ano pa ang kulang. Para sa bridge network na walang root access, tingnan kung naka-install ang mga package na `uidmap` at `passt`. [Mga kailangan sa Linux](docs/usage.md#requirements-on-linux)

Para sa manu-manong pag-install, i-download ang file para sa iyong OS at CPU at ihambing ang checksum nito sa `SHA256SUMS`. [Mga release file](https://github.com/SteelCrab/podbag/releases/latest)

### Windows (eksperimental)

Patakbuhin ang `wsl --install` sa PowerShell bilang administrator, pagkatapos ay i-restart ang Windows. I-download ang `podbag-x86_64-windows.exe` o `podbag-aarch64-windows.exe` ayon sa CPU. Ihambing ang SHA-256 checksum nito sa `SHA256SUMS`, pagkatapos ay palitan ang pangalan ng file ng `podbag.exe`. [Mga release file](https://github.com/SteelCrab/podbag/releases/latest)

```powershell
.\podbag.exe run --rm alpine echo hello
```

### Iba pang paraan ng pag-install

- **Cargo sa Linux.** Patakbuhin ang `cargo install --locked podbag` para i-install ang nailabas na bersiyon ng CLI.
- **Desktop app sa Linux.** I-download mula sa GitHub Actions ang archive para sa iyong CPU. I-extract ito at i-install ang `.deb` o `.rpm` para sa iyong distribusyon. Kasama na sa package ang engine. [Mga desktop package](docs/build.md#linux-desktop-packages)
- **Pag-build mula sa source.** I-build ang `main` branch para gamitin ang Kubernetes runtime at maglipat ng data sa ibang engine. [Gabay sa pag-build](docs/build.md)

## Paggamit

### Magpatakbo ng container

Ipi-print ng command na ito ang `hello`. Awtomatikong aalisin ang container kapag natapos ang command.

```sh
podbag run --rm alpine echo hello
```

Patakbuhin ang nginx sa port 8080, magpadala ng request at tingnan ang access log.

```sh
podbag run -d --name web -p 8080:80 nginx:alpine
podbag ps
curl -I http://localhost:8080/
podbag logs web
podbag exec web nginx -v
podbag stop web && podbag rm web
```

### Docker CLI at Compose

Linux bilang karaniwang user na may nakatakdang `XDG_RUNTIME_DIR`

```sh
podbag system service &
podbag_socket="unix://$XDG_RUNTIME_DIR/podbag/docker.sock"
```

macOS

```sh
podbag vm start
podbag_socket="unix://$HOME/.podbag/run/docker.sock"
```

Pagkatapos itakda ang socket, patakbuhin ang mga command sa directory ng proyektong may `compose.yaml` na gumagamit ng mga image mula sa registry.

```sh
docker --host "$podbag_socket" ps
docker --host "$podbag_socket" compose up -d
docker --host "$podbag_socket" compose down
```

### Kumopya ng image mula sa Docker (`main`)

Simulan ang dalawang engine. Kung may lokal na `alpine:latest` image ang Docker, makokopya ito sa Podbag gamit ang CLI na binuo mula sa `main`.

```sh
podbag migrate --from docker --to podbag image alpine:latest
```

[Gabay sa paglipat](docs/usage.md#migrate-between-engines-or-kubernetes-clusters) · [Mga command](docs/usage.md)

## Mga benchmark

<img src="docs/images/perf.svg" alt="Paghahambing ng podbag sa Docker, Podman at Kiri sa isang Linux VM: nagsisimula ang container sa 4.1 ms (Docker 73.8 ms); 105 MiB para sa 100 container (Docker 562 MiB); walang tumatakbo kapag walang container (Docker 203 MiB); 902 MiB na disk para sa 8 Spring service sa 2 release (Docker 2,970 MiB) at 324 MiB na page cache (Docker 982 MiB)." width="800">

<sub>Ang parehong paghahambing bilang video (sa English): [perf.mp4](docs/media/perf.mp4).</sub>

<table>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/start-dark.svg"><img alt="Pagsisimula ng container, median na ms: podbag rootless 4.1, podbag root 4.4, Kiri 9.1, Podman 46.4, Docker 73.8" src="docs/images/start-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.svg"><img alt="Memory para sa 100 container, MiB: podbag root 105, Kiri 109, podbag rootless 117, Podman 203, Docker 562" src="docs/images/memory-light.svg"></picture></td>
</tr>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/disk-dark.svg"><img alt="Disk para sa 8 Spring service sa 2 release, MiB: podbag root 902, podbag rootless 944, Podman 1783, Kiri 2960, Docker 2970" src="docs/images/disk-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/cache-dark.svg"><img alt="Page cache ng 16 Spring service, MiB: podbag rootless 324, podbag root 333, Kiri 960, Podman 975, Docker 982" src="docs/images/cache-light.svg"></picture></td>
</tr>
</table>

- Mga median sa iisang Linux VM: 4 CPU, 3.8 GiB, arm64.
- Mga bersyon: podbag 0.1.0, Podman 5.7.0, Docker 29.1.3, Kiri 0.2.1.
- Lahat ng numero at kung paano sinukat: [docs/benchmarks.md](docs/benchmarks.md) at [docs/benchmarks-msa.md](docs/benchmarks-msa.md) (Ingles).
- Bilang runtime ng isang Kubernetes node sa k3s, na may 30 pod: 2 proseso sa 3.5 MiB ang podbag, laban sa 63 ng containerd (276 MiB) at 32 ng CRI-O (85 MiB): [docs/benchmarks-k8s.md](docs/benchmarks-k8s.md) (Ingles).

## Paghahambing sa ibang tool

| | podbag | Docker | OrbStack | Colima | Kiri |
|---|---|---|---|---|---|
| Tumatakbo sa | Linux, macOS, Windows¹ | Linux; macOS at Windows gamit ang Docker Desktop | macOS | macOS, Linux | Linux, macOS, Windows |
| Tumatakbo sa background | wala kapag natapos ang huling container (Mac: isang 2 MiB na standby) | `dockerd`, `containerd`, isang shim bawat container | isang Linux VM na may Docker Engine | isang Lima VM na may Docker o containerd | isang daemon; sa macOS at Windows, isang maliit na VM |
| Imbakan ng image | mga file, bawat isa ay isang beses | mga layer | mga layer | mga layer | mga layer, ina-unpack sa unang gamit |
| Mac: simula ng `docker run`, median² | 14.4 ms | 97.4 ms | 90.4 ms | 73.6 ms | 11.8 ms |
| Mac: memory 6 na minuto matapos ang huling container² | 1.7 MiB | 950 MiB | 795 MiB | 1.54 GiB | 12 MiB |
| Lisensya | Apache-2.0 | Apache-2.0; may bayad ang Docker Desktop para sa malalaking kumpanya | proprietary | MIT | Apache-2.0 |

¹ Eksperimental. ² Sinukat sa iisang Mac, Docker Desktop para sa Docker: [docs/benchmarks.md](docs/benchmarks.md).

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/architecture-dark.svg"><img alt="Docker Engine: kumakausap ang docker CLI sa dockerd, ang dockerd sa containerd, at nagsisimula ang containerd ng isang shim bawat container; mga layer ang mga image. podbag: ang command mismo ang gumagawa ng bawat container, at isang keeper bawat user ang nagbabantay sa mga ito habang tumatakbo; mga file ang mga image, at isang beses lang iniimbak ang bawat file." src="docs/images/architecture-light.svg"></picture>

- Paano nagtutugma ang mga bahagi, at kung saan kulang pa ang podbag: [docs/architecture.md](docs/architecture.md) (Ingles).

## Dokumentasyon

Nasa Ingles ang mga dokumento sa ibaba.

- [Paggamit](docs/usage.md): mga command, volume, network, limit, Docker API, TUI at GUI, macOS, Windows.
- [Arkitektura](docs/architecture.md): paano naiiba ang podbag sa Docker.
- [Pag-build mula sa source](docs/build.md): Linux, macOS, Windows.
- [Mga benchmark](docs/benchmarks.md) at ang [benchmark ng Spring microservices](docs/benchmarks-msa.md).

## Pag-ambag

- Malugod naming tinatanggap ang mga issue at pull request.
- Tingnan ang [CONTRIBUTING.md](CONTRIBUTING.md) (Ingles).

## Lisensya

[Apache License 2.0](LICENSE)
