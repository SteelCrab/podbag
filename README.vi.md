# podbag

<img src="desktop/icons/128x128.png" alt="podbag" width="120" align="right">

Một container engine không có daemon, chỉ gồm một tệp nhị phân tĩnh.
Image được lưu theo từng tệp: tệp mà nhiều image dùng chung chỉ được lưu một lần.
Bạn có thể dùng dòng lệnh hoặc [Podbag Desktop](#podbag-desktop).

[![CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml/badge.svg)](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/SteelCrab/podbag)](https://github.com/SteelCrab/podbag/releases/latest)
[![crates.io](https://img.shields.io/crates/v/podbag)](https://crates.io/crates/podbag)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)
[![Website](https://img.shields.io/badge/website-podbag.dev-2a78d6)](https://podbag.dev)

[English](README.md) · [한국어](README.ko.md) · [日本語](README.ja.md) · [简体中文](README.zh-CN.md) · [Filipino](README.fil.md) · [Español](README.es.md) · **Tiếng Việt**

<img src="docs/images/intro.svg" alt="Video giới thiệu podbag. Các công cụ khác luôn chạy daemon; podbag chỉ chạy khi có container, và không chạy gì khi nghỉ. Các công cụ khác lưu theo lớp; podbag lưu theo tệp, mỗi tệp một lần: 902 MiB đĩa thay vì 2.970 MiB cho 8 dịch vụ Spring ở 2 bản phát hành, và 324 MiB page cache thay vì 982 MiB. Container khởi động trong 4,1 ms, nhanh hơn 18 lần so với 73,8 ms của Docker. Linux, macOS và Windows; podbag.dev." width="800">

<sub>Phần giới thiệu này cũng có dạng video (tiếng Anh): [intro.mp4](docs/media/intro.mp4).</sub>

## Mục lục

- [Tính năng](#tính-năng)
- [Podbag Desktop](#podbag-desktop)
- [Cài đặt](#cài-đặt)
- [Sử dụng](#sử-dụng)
- [Đo hiệu năng](#đo-hiệu-năng)
- [So sánh với các công cụ khác](#so-sánh-với-các-công-cụ-khác)
- [Tài liệu](#tài-liệu)
- [Đóng góp](#đóng-góp)
- [Giấy phép](#giấy-phép)

## Tính năng

- **Chia sẻ tệp giữa các image.** Các tệp giống nhau có thể dùng chung dung lượng đĩa và bộ nhớ đệm trang của Linux, kể cả khi nằm ở các lớp khác nhau.
- **Chạy theo nhu cầu trên Linux.** Một tiến trình theo dõi các container của mỗi người dùng. Khi container cuối cùng dừng, tiến trình này thoát sau thời gian chờ.
- **Tự tắt máy ảo trên macOS.** Theo mặc định, máy ảo Linux tắt sau 60 giây không hoạt động. Lệnh container tiếp theo sẽ khởi động lại máy ảo.
- **Runtime cho nút Kubernetes (`v0.2.0`).** Trên Linux, `podbag cri serve` cho phép kubelet chạy pod trực tiếp bằng Podbag.
- **Di chuyển giữa các engine (`v0.2.0`).** Sao chép image, volume và container đã dừng giữa các engine tương thích với Docker. Dữ liệu nguồn được giữ nguyên.

<picture><source media="(max-width: 600px) and (prefers-color-scheme: dark)" srcset="docs/images/storage-vi-tall-dark.svg"><source media="(max-width: 600px)" srcset="docs/images/storage-vi-tall-light.svg"><source media="(prefers-color-scheme: dark)" srcset="docs/images/storage-vi-dark.svg"><img alt="Hoạt ảnh. Hai image có hầu hết các tệp giống nhau. Khi lưu theo lớp, một lớp chỉ được lưu một lần khi nó giống hệt; lớp trên của image B khác một tệp, nên spring.jar và jackson.jar được lưu lại: 8 tệp. podbag lưu mỗi tệp một lần: 6 tệp. Đo với 8 dịch vụ Spring trong 2 bản phát hành: 2.970 MiB đĩa khi lưu theo lớp (Docker), 902 MiB với podbag." src="docs/images/storage-vi-light.svg"></picture>

## Podbag Desktop

Quản lý container, image, volume và mạng trên Linux và macOS. Hỗ trợ Windows đang ở giai đoạn thử nghiệm và cần WSL 2. Bạn có thể khởi động, dừng hoặc khởi động lại container ngay trong danh sách. Mở một container để theo dõi nhật ký theo thời gian thực.

![Podbag Desktop hiển thị ba container mẫu đang chạy và hai container đã dừng, cùng với image và các cổng được công bố](docs/images/desktop-containers-en.png)

[Cài Podbag Desktop](#cài-đặt) hoặc [biên dịch ứng dụng desktop từ mã nguồn](docs/usage.md#build-the-desktop-app).

## Cài đặt

### Podbag Desktop (khuyên dùng)

Tải gói cho hệ điều hành của bạn từ [bản phát hành mới nhất](https://github.com/SteelCrab/podbag/releases/latest).

- **Linux (x86_64, arm64).** Cài gói `.deb` trên Debian hoặc Ubuntu, hoặc gói `.rpm` trên Fedora. Gói cài đặt có sẵn engine.
- **macOS (Apple silicon, Intel).** Cài CLI bằng lệnh bên dưới. Giải nén `podbag-desktop-aarch64-macos.zip` (Apple silicon) hoặc `podbag-desktop-x86_64-macos.zip` (Intel) rồi mở `podbag.app`.
- **Windows (x86_64, ARM64, thử nghiệm).** Chạy tệp `.msi` phù hợp với CPU. Nếu bản phát hành chưa có MSI, tải tệp từ một [lần chạy CI thành công](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml?query=branch%3Amain). Bật WSL 2 để chạy container.

### CLI trên Linux (x86_64, arm64) và macOS (Apple silicon, Intel)

```sh
curl -fsSL https://podbag.dev/install.sh | sh
podbag --version
```

Script kiểm tra mã SHA-256 của tệp phát hành rồi cài CLI vào `/usr/local/bin`. Chạy lại script này khi cần cập nhật.

Linux cần kernel 5.12 trở lên và cgroup v2. Chạy `podbag check` để biết hệ thống còn thiếu gì. Nếu dùng mạng bridge mà không có quyền root, hãy kiểm tra các gói `uidmap` và `passt`. [Yêu cầu trên Linux](docs/usage.md#requirements-on-linux)

Để cài thủ công, tải tệp phù hợp với hệ điều hành và CPU rồi đối chiếu mã kiểm tra với `SHA256SUMS`. [Tải bản phát hành](https://github.com/SteelCrab/podbag/releases/latest)

### CLI trên Windows (thử nghiệm)

Chạy `wsl --install` trong PowerShell với quyền quản trị rồi khởi động lại Windows. Tải `podbag-x86_64-windows.exe` hoặc `podbag-aarch64-windows.exe` theo kiến trúc CPU. Đối chiếu mã SHA-256 với `SHA256SUMS` rồi đổi tên tệp thành `podbag.exe`. [Tải bản phát hành](https://github.com/SteelCrab/podbag/releases/latest)

```powershell
.\podbag.exe run --rm alpine echo hello
```

### Các cách cài đặt khác

- **Cargo trên Linux.** Chạy `cargo install --locked podbag` để cài bản CLI đã phát hành.
- **Biên dịch từ mã nguồn.** Dùng Rust để biên dịch CLI và Podbag Desktop. [Hướng dẫn biên dịch](docs/build.md)

## Sử dụng

### Chạy container

Lệnh này in ra `hello` và xóa container khi chạy xong.

```sh
podbag run --rm alpine echo hello
```

Chạy nginx trên cổng 8080, gửi yêu cầu rồi xem nhật ký truy cập.

```sh
podbag run -d --name web -p 8080:80 nginx:alpine
podbag ps
curl -I http://localhost:8080/
podbag logs web
podbag exec web nginx -v
podbag stop web && podbag rm web
```

### Docker CLI và Compose

Linux với người dùng thường và biến `XDG_RUNTIME_DIR` đã được đặt

```sh
podbag system service &
podbag_socket="unix://$XDG_RUNTIME_DIR/podbag/docker.sock"
```

macOS

```sh
podbag vm start
podbag_socket="unix://$HOME/.podbag/run/docker.sock"
```

Sau khi thiết lập socket, chạy các lệnh sau trong thư mục có tệp `compose.yaml` sử dụng các image đã phát hành.

```sh
docker --host "$podbag_socket" ps
docker --host "$podbag_socket" compose up -d
docker --host "$podbag_socket" compose down
```

### Sao chép image từ Docker (`v0.2.0`)

Khởi động cả hai engine. Nếu Docker đã có image `alpine:latest`, hãy dùng Podbag CLI v0.2.0 trở lên để sao chép image đó sang Podbag.

```sh
podbag migrate --from docker --to podbag image alpine:latest
```

[Hướng dẫn di chuyển](docs/usage.md#migrate-between-engines-or-kubernetes-clusters) · [Danh sách lệnh](docs/usage.md)

## Đo hiệu năng

<img src="docs/images/perf.svg" alt="So sánh podbag với Docker, Podman và Kiri trên một VM Linux: container khởi động trong 4,1 ms (Docker 73,8 ms); 100 container chiếm 105 MiB (Docker 562 MiB); không có gì chạy khi không có container (Docker 203 MiB); 8 dịch vụ Spring ở 2 bản phát hành chiếm 902 MiB đĩa (Docker 2.970 MiB) và 324 MiB page cache (Docker 982 MiB)." width="800">

<sub>Cùng phép so sánh dưới dạng video (tiếng Anh): [perf.mp4](docs/media/perf.mp4).</sub>

<table>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/start-dark.svg"><img alt="Thời gian khởi động container, trung vị (ms): podbag không root 4.1, podbag root 4.4, Kiri 9.1, Podman 46.4, Docker 73.8" src="docs/images/start-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/memory-dark.svg"><img alt="Bộ nhớ cho 100 container (MiB): podbag root 105, Kiri 109, podbag không root 117, Podman 203, Docker 562" src="docs/images/memory-light.svg"></picture></td>
</tr>
<tr>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/disk-dark.svg"><img alt="Đĩa cho 8 dịch vụ Spring ở 2 bản phát hành (MiB): podbag root 902, podbag không root 944, Podman 1783, Kiri 2960, Docker 2970" src="docs/images/disk-light.svg"></picture></td>
<td><picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/cache-dark.svg"><img alt="Page cache của 16 dịch vụ Spring (MiB): podbag không root 324, podbag root 333, Kiri 960, Podman 975, Docker 982" src="docs/images/cache-light.svg"></picture></td>
</tr>
</table>

- Giá trị trung vị trên một máy ảo Linux: 4 CPU, 3.8 GiB, arm64.
- Phiên bản: podbag 0.1.0, Podman 5.7.0, Docker 29.1.3, Kiri 0.2.1.
- Tất cả số liệu và cách đo: [docs/benchmarks.md](docs/benchmarks.md) và [docs/benchmarks-msa.md](docs/benchmarks-msa.md) (tiếng Anh).
- Làm runtime cho một node Kubernetes trên k3s, với 30 pod: podbag có 2 tiến trình, 3,5 MiB; containerd có 63 (276 MiB) và CRI-O có 32 (85 MiB): [docs/benchmarks-k8s.md](docs/benchmarks-k8s.md) (tiếng Anh).

## So sánh với các công cụ khác

| | podbag | Docker | OrbStack | Colima | Kiri |
|---|---|---|---|---|---|
| Chạy trên | Linux, macOS, Windows¹ | Linux; macOS và Windows dùng Docker Desktop | macOS | macOS, Linux | Linux, macOS, Windows |
| Chạy nền | không có gì khi container cuối cùng kết thúc (Mac: một tiến trình chờ 2 MiB) | `dockerd`, `containerd`, mỗi container một shim | một máy ảo Linux có Docker Engine | một máy ảo Lima có Docker hoặc containerd | một daemon; trên macOS và Windows là một máy ảo nhỏ |
| Lưu image | theo tệp, mỗi tệp một lần | theo lớp | theo lớp | theo lớp | theo lớp, giải nén ở lần dùng đầu |
| Mac: khởi động bằng `docker run`, trung vị² | 14.4 ms | 97.4 ms | 90.4 ms | 73.6 ms | 11.8 ms |
| Mac: bộ nhớ 6 phút sau container cuối cùng² | 1.7 MiB | 950 MiB | 795 MiB | 1.54 GiB | 12 MiB |
| Giấy phép | Apache-2.0 | Apache-2.0; Docker Desktop tính phí với công ty lớn | độc quyền | MIT | Apache-2.0 |

¹ Thử nghiệm. ² Đo trên một máy Mac, Docker là Docker Desktop: [docs/benchmarks.md](docs/benchmarks.md).

<picture><source media="(prefers-color-scheme: dark)" srcset="docs/images/architecture-dark.svg"><img alt="Docker Engine: CLI docker nói chuyện với dockerd, dockerd nói chuyện với containerd, và containerd khởi động một shim cho mỗi container; image là các lớp. podbag: lệnh tự tạo từng container, mỗi người dùng có một keeper trông coi các container khi chúng chạy; image là các tệp, mỗi tệp chỉ lưu một lần." src="docs/images/architecture-light.svg"></picture>

- Các phần ăn khớp với nhau thế nào, và podbag còn kém ở đâu: [docs/architecture.md](docs/architecture.md) (tiếng Anh).

## Tài liệu

Các tài liệu dưới đây bằng tiếng Anh.

- [Sử dụng](docs/usage.md): lệnh, volume, mạng, giới hạn, Docker API, TUI và GUI, macOS, Windows.
- [Kiến trúc](docs/architecture.md): podbag khác Docker thế nào.
- [Biên dịch từ mã nguồn](docs/build.md): Linux, macOS, Windows.
- [Đo hiệu năng](docs/benchmarks.md) và [bài đo với microservice Spring](docs/benchmarks-msa.md).

## Đóng góp

- Chúng tôi hoan nghênh issue và pull request.
- Xem [CONTRIBUTING.md](CONTRIBUTING.md) (tiếng Anh).

## Giấy phép

[Apache License 2.0](LICENSE)
