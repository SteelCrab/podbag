# Building podbag from source

## Contents

- [What you need](#what-you-need)
- [Linux](#linux)
- [Android](#android)
- [macOS](#macos)
- [Windows](#windows)
- [Check your build](#check-your-build)

## What you need

- [Rust](https://rustup.rs), stable.
- A C compiler. The TLS library (ring) has some C code.
- The source:

  ```sh
  git clone https://github.com/SteelCrab/podbag
  cd podbag
  ```

## Linux

### Quick build

```sh
cargo build --release
sudo install -m 0755 target/release/podbag /usr/local/bin/podbag
```

- The program uses the system's C library (glibc).
- `cargo install podbag` gives the same program from crates.io.

### Static build

This is how the release files are built.

```sh
sudo apt-get install musl-tools                  # Debian and Ubuntu
rustup target add x86_64-unknown-linux-musl      # on arm64: aarch64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl
```

- The program is `target/x86_64-unknown-linux-musl/release/podbag`.
- It needs no other library. It runs on Linux 5.12 or later.

### Linux desktop packages

Download the `podbag-desktop-amd64` or `podbag-desktop-arm64` artifact from
[Linux desktop packages](https://github.com/SteelCrab/podbag/actions/workflows/desktop-packages.yml).
Extract the ZIP, then install the package for your distribution from that directory:

```sh
sudo apt install ./podbag-desktop_*.deb     # Debian 12 or compatible Ubuntu
sudo dnf install ./podbag-desktop-*.rpm     # Fedora 44
```

Open **Podbag Desktop** from the applications menu. The package includes the
engine used by the GUI. To remove it, run `sudo apt purge podbag-desktop` or
`sudo dnf remove podbag-desktop`.

To build both packages on Debian 12, install Rust, nFPM, jq, a C compiler,
pkg-config, libwebkit2gtk-4.1-dev, libayatana-appindicator3-dev, librsvg2-dev
and libxdo-dev. Run `desktop/linux/package.sh` on the target architecture.
The packages are written to `packages/` in Cargo's target directory.

## Android

Build the CLI for 64-bit ARM devices with Android 7.0 (API 24) or later.
The Android build includes image commands. Running containers locally requires
the Linux build.

Install the [Android NDK](https://developer.android.com/ndk/downloads) and set
`ANDROID_NDK_HOME` to its directory. From the repository root, run:

```sh
rustup target add aarch64-linux-android
# On macOS (including Apple silicon), use darwin-x86_64 instead of linux-x86_64.
export PATH="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin:$PATH"
cargo build --locked --release --target aarch64-linux-android --bin podbag
```

To check the binary on a connected device:

```sh
adb push target/aarch64-linux-android/release/podbag /data/local/tmp/podbag
adb shell chmod 755 /data/local/tmp/podbag
adb shell /data/local/tmp/podbag --version
```

## macOS

On a Mac, podbag runs containers in a small Linux VM. A build has three steps.
Apple silicon only.

1. **Build the VM's guest** (a Linux kernel, with podbag in it) in a Linux VM.
   [lima](https://lima-vm.io) runs that VM:

   ```sh
   dev/vm.sh up                              # create the podbag-dev VM (once)
   dev/vm.sh run dev/macos/build-guest.sh    # writes target/vm
   ```

   - The first build compiles a Linux kernel. It takes a while.

2. **Build the program** with the guest in it:

   ```sh
   cargo build --release --features embed-guest
   ```

3. **Sign it.** A program that starts VMs needs the virtualization entitlement:

   ```sh
   dev/macos/sign.sh target/release/podbag
   ```

## Windows

podbag.exe runs a static Linux podbag in WSL 2. A build needs both.

1. **Get the static Linux program** for the same CPU:
   - build it on Linux (see [Static build](#static-build)), or
   - download `podbag-x86_64-linux` or `podbag-aarch64-linux` from a
     [release](https://github.com/SteelCrab/podbag/releases).

2. **Build podbag.exe** with it:

   ```powershell
   $env:PODBAG_LINUX_BIN = "C:\path\to\podbag-x86_64-linux"
   cargo build --release --features embed-linux
   ```

- On a Mac or on Linux, `--target x86_64-pc-windows-gnu` cross-compiles. It needs
  `mingw-w64`.

### Windows desktop installer

Download the MSI for your CPU from [GitHub Releases](https://github.com/SteelCrab/podbag/releases/latest).
Install it and open **Podbag Desktop** from the Start menu. Enable WSL 2 to run containers.

On Windows x86_64 or ARM64, build the CLI with the Linux engine for the same CPU.
Use a native Rust toolchain for that CPU.
Install the Visual Studio C++ build tools for your CPU, WebView2, and Node.js.
From the repository root, run:

```powershell
New-Item -ItemType Directory -Force target/windows-engine | Out-Null
Copy-Item target/release/podbag.exe target/windows-engine/podbag.exe
Set-Location desktop
npx --yes --package @tauri-apps/cli@2.12.6 tauri build --config windows/msi.json -- --locked
```

Open the MSI in `target/release/bundle/msi`, then start **Podbag Desktop** from
the Start menu. The installer includes the GUI and its matching CLI. Enable WSL 2
to run containers. CI publishes the tested MSI as `podbag-desktop-x86_64-windows` or
`podbag-desktop-aarch64-windows`. The latter runs natively on Windows on Arm.

## Check your build

- `podbag --version` prints the version.
- `podbag check` (Linux) lists what the system still needs.
- To run the tests, see [CONTRIBUTING.md](../CONTRIBUTING.md#test).
