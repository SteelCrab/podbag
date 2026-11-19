<img src="docs/images/logo.png" alt="podbag" width="120" align="right">

# podbag

Un motor de contenedores sin daemon, en un solo binario estático.
Guarda las imágenes archivo por archivo: un archivo que comparten muchas imágenes se guarda una sola vez.

[![CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml/badge.svg)](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SteelCrab/podbag)](https://github.com/SteelCrab/podbag/releases/latest)
[![crates.io](https://img.shields.io/crates/v/podbag)](https://crates.io/crates/podbag)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Website](https://img.shields.io/badge/website-podbag.dev-2a78d6)](https://podbag.dev)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md) · [Filipino](README.fil.md) · **Español** · [Tiếng Việt](README.vi.md)

<img src="docs/images/intro.svg" alt="Vídeo de introducción a podbag. Otros motores mantienen demonios en marcha; podbag solo funciona mientras hay contenedores y, en reposo, no ejecuta nada. Otros guardan capas; podbag guarda archivos, cada uno una vez: 902 MiB de disco en lugar de 2.970 MiB para 8 servicios Spring en 2 versiones, y 324 MiB de caché de páginas en lugar de 982 MiB. Un contenedor arranca en 4,1 ms, 18 veces más rápido que los 73,8 ms de Docker. Linux, macOS y Windows; podbag.dev." width="800">

<sub>La misma introducción en vídeo (en inglés): [intro.mp4](docs/media/intro.mp4).</sub>

## Contenido

- [Características](#características)
- [Instalación](#instalación)
- [Uso](#uso)
- [Rendimiento](#rendimiento)
- [Comparación con otras herramientas](#comparación-con-otras-herramientas)
- [Documentación](#documentación)
- [Contribuir](#contribuir)
- [Licencia](#licencia)

## Características

- **Sin daemon.** Cuando no hay contenedores, no se ejecuta nada.
- **Un solo binario estático.** Unos 4 MB en Linux.
- **Archivos, no capas.** Un archivo que comparten muchas imágenes se guarda y se cachea una sola vez.
- **Rápido.** Un contenedor arranca en unos 4 ms en Linux.
- **Funciona con las herramientas de Docker.** Imágenes Docker y OCI, el CLI `docker`, Compose y buildx.
- **Con o sin root.** El mismo programa funciona de las dos formas.
- **Linux, macOS y Windows.** x86_64 y arm64. El soporte de Windows es experimental.

### Capas y archivos

<picture><source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="docs/images/storage-es-tall-dark.svg"><source media="(max-width: 600px)" srcset="docs/images/storage-es-tall-light.svg"><source media="(prefers-color-scheme: dark)" srcset="docs/images/storage-es-dark.svg"><img alt="Animación. Dos imágenes comparten la mayoría de sus archivos. Con capas, una capa se guarda una vez solo si es igual; la capa superior de la imagen B tiene un archivo distinto, así que spring.jar y jackson.jar se guardan otra vez: 8 archivos. podbag guarda cada archivo una vez: 6 archivos. Medido con 8 servicios Spring en 2 versiones: 2.970 MiB de disco con capas (Docker), 902 MiB con podbag." src="docs/images/storage-es-light.svg"></picture>

- Los almacenes por capas (Docker, Podman, Kiri) guardan una capa una sola vez solo si toda la capa es igual. Si un solo archivo es distinto, se guarda otra vez toda la capa, con los archivos que comparte con otras capas.
- podbag guarda cada archivo una sola vez. Una imagen es una lista de rutas que apuntan a esos archivos.
- Con 8 servicios Spring en 2 versiones: 902 MiB de disco frente a 2.970 MiB de Docker, y 324 MiB de caché de páginas frente a 982 MiB.

## Instalación

### Script de instalación (Linux y macOS)

```sh
curl -fsSL https://raw.githubusercontent.com/SteelCrab/podbag/main/install.sh | sh
```

- Descarga el archivo para tu sistema y comprueba su SHA-256.
- Lo instala en `/usr/local/bin`, con `sudo` si hace falta.
- En Ubuntu 24.04 y posteriores, añade el perfil de AppArmor que necesitan los contenedores sin root.
- Opciones: `PODBAG_VERSION=v0.1.0`, `PODBAG_INSTALL_DIR=$HOME/.local/bin`,
  `PODBAG_SKIP_APPARMOR=1`.

### Descarga

Descarga un archivo de la [última versión](https://github.com/SteelCrab/podbag/releases/latest). Compruébalo con `SHA256SUMS`.

| Sistema | Archivo |
|---|---|
| Linux, x86_64 | `podbag-x86_64-linux` |
| Linux, arm64 | `podbag-aarch64-linux` |
| macOS, Apple silicon | `podbag-aarch64-macos` |
| Windows, x86_64 (experimental) | `podbag-x86_64-windows.exe` |
| Windows, arm64 (experimental) | `podbag-aarch64-windows.exe` |

Windows necesita WSL 2 (ejecuta `wsl --install` una vez):

```powershell
curl.exe -fsSLo podbag.exe https://github.com/SteelCrab/podbag/releases/latest/download/podbag-x86_64-windows.exe
.\podbag.exe run --rm alpine echo hello
```

### Cargo

```sh
cargo install podbag
```

- Para Linux. En macOS y Windows, usa los archivos de la versión: incluyen la VM y el motor de Linux.

### Compilar desde el código fuente

- Consulta [docs/build.md](docs/build.md) para Linux, macOS y Windows (en inglés).

### Requisitos y AppArmor (Linux)

- Linux 5.12 o posterior, con cgroup v2.
- Sin root, podbag también necesita:
  - `newuidmap` (paquete `uidmap`),
  - `pasta` (paquete `passt`) para las redes,
  - un perfil de AppArmor en Ubuntu 24.04 y posteriores. El script de instalación lo añade. A mano:

    ```sh
    podbag check --apparmor-profile | sudo tee /etc/apparmor.d/podbag >/dev/null
    sudo apparmor_parser -r /etc/apparmor.d/podbag
    ```

- `podbag check` muestra lo que aún necesita tu sistema y cómo añadirlo.

## Uso

### Comandos básicos

```sh
podbag run --rm alpine echo hello                 # descarga la imagen si hace falta, ejecuta y borra
podbag run -d --name web -p 8080:80 nginx:alpine  # en segundo plano
podbag ps                                         # lista los contenedores
podbag logs -f web                                # sigue los registros
podbag exec -it web sh                            # abre una shell en el contenedor
podbag stats --no-stream                          # uso de CPU, memoria y red
podbag stop web && podbag rm web                  # detén y borra
podbag images                                     # lista las imágenes
```

### CLI docker, Compose y buildx

```sh
podbag system service &      # Linux (en macOS: podbag vm start)
export DOCKER_HOST=unix://$XDG_RUNTIME_DIR/podbag/docker.sock   # macOS: unix://$HOME/.podbag/run/docker.sock
docker compose up -d
```

- Todos los comandos y opciones: [docs/usage.md](docs/usage.md) (en inglés).

## Rendimiento

<img src="docs/images/perf.svg" alt="Comparación de podbag con Docker, Podman y Kiri en una VM Linux: un contenedor arranca en 4,1 ms (Docker 73,8 ms); 100 contenedores ocupan 105 MiB (Docker 562 MiB); sin contenedores no queda nada en marcha (Docker 203 MiB); 8 servicios Spring en 2 versiones ocupan 902 MiB de disco (Docker 2.970 MiB) y 324 MiB de caché de páginas (Docker 982 MiB)." width="800">

<sub>La misma comparación en vídeo (en inglés): [perf.mp4](docs/media/perf.mp4).</sub>

<table>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/start-dark.svg"><img alt="Arranque de un contenedor, mediana en ms: podbag sin root 4,1, podbag root 4,4, Kiri 9,1, Podman 46,4, Docker 73,8" src="docs/images/start-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.svg"><img alt="Memoria para 100 contenedores, MiB: podbag root 105, Kiri 109, podbag sin root 117, Podman 203, Docker 562" src="docs/images/memory-light.svg"></picture></td>
</tr>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/disk-dark.svg"><img alt="Disco para 8 servicios Spring en 2 versiones, MiB: podbag root 902, podbag sin root 944, Podman 1783, Kiri 2960, Docker 2970" src="docs/images/disk-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/cache-dark.svg"><img alt="Caché de páginas de 16 servicios Spring, MiB: podbag sin root 324, podbag root 333, Kiri 960, Podman 975, Docker 982" src="docs/images/cache-light.svg"></picture></td>
</tr>
</table>

- Medianas en una VM Linux: 4 CPU, 3,8 GiB, arm64.
- Versiones: podbag 0.1.0, Podman 5.7.0, Docker 29.1.3, Kiri 0.2.1.
- Todos los números y cómo se midieron: [docs/benchmarks.md](docs/benchmarks.md) y [docs/benchmarks-msa.md](docs/benchmarks-msa.md) (en inglés).

## Comparación con otras herramientas

| | podbag | Docker | OrbStack | Colima | Kiri |
|---|---|---|---|---|---|
| Funciona en | Linux, macOS, Windows¹ | Linux; macOS y Windows con Docker Desktop | macOS | macOS, Linux | Linux, macOS, Windows |
| En segundo plano | nada cuando termina el último contenedor (Mac: un proceso en espera de 2 MiB) | `dockerd`, `containerd`, un shim por contenedor | una VM Linux con Docker Engine | una VM de Lima con Docker o containerd | un daemon; en macOS y Windows, una VM pequeña |
| Almacenamiento de imágenes | archivos, cada uno una vez | capas | capas | capas | capas, se desempaquetan al primer uso |
| Mac: arranque con `docker run`, mediana² | 14.4 ms | 97.4 ms | 90.4 ms | 73.6 ms | 11.8 ms |
| Mac: memoria 6 minutos después del último contenedor² | 1.7 MiB | 950 MiB | 795 MiB | 1.54 GiB | 12 MiB |
| Licencia | Apache-2.0 | Apache-2.0; Docker Desktop es de pago para empresas grandes | propietaria | MIT | Apache-2.0 |

¹ Experimental. ² Medido en un Mac, con Docker Desktop para Docker: [docs/benchmarks.md](docs/benchmarks.md).

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/architecture-dark.svg"><img alt="Docker Engine: el CLI docker habla con dockerd, que habla con containerd, que arranca un shim por contenedor; las imágenes son capas. podbag: el comando crea cada contenedor por sí mismo, y un keeper por usuario los vigila mientras se ejecutan; las imágenes son archivos, cada uno guardado una vez." src="docs/images/architecture-light.svg"></picture>

- Cómo encajan las piezas y en qué va aún por detrás podbag: [docs/architecture.md](docs/architecture.md) (en inglés).

## Documentación

Los documentos de abajo están en inglés.

- [Uso](docs/usage.md): comandos, volúmenes, redes, límites, API de Docker, macOS, Windows.
- [Arquitectura](docs/architecture.md): en qué se diferencia podbag de Docker.
- [Compilar desde el código fuente](docs/build.md): Linux, macOS, Windows.
- [Rendimiento](docs/benchmarks.md) y la [prueba con microservicios Spring](docs/benchmarks-msa.md).

## Contribuir

- Los issues y pull requests son bienvenidos.
- Consulta [CONTRIBUTING.md](CONTRIBUTING.md) (en inglés).

## Licencia

[Apache License 2.0](LICENSE)
