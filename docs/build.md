# Building podbag from source

## Contents

- [What you need](#what-you-need)
- [Linux](#linux)
  - [Quick build](#quick-build)
  - [Static build](#static-build)
  - [Linux desktop packages](#linux-desktop-packages)
- [Android](#android)
- [macOS](#macos)
  - [macOS desktop app and release files](#macos-desktop-app-and-release-files)
- [Windows](#windows)
  - [Windows desktop installer](#windows-desktop-installer)
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

- The release files are built this way.

```sh
sudo apt-get install musl-tools                  # Debian and Ubuntu
rustup target add x86_64-unknown-linux-musl      # on arm64: aarch64-unknown-linux-musl
CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl
```

- The program is `target/x86_64-unknown-linux-musl/release/podbag`.
- It needs no other library. It runs on Linux 5.12 or later.

### Linux desktop packages

#### Install a package

- Each release has a `.deb` and an `.rpm` for amd64 and arm64:
  [latest release](https://github.com/SteelCrab/podbag/releases/latest).
- A build of `main` is the `podbag-desktop-amd64` or `podbag-desktop-arm64` artifact of
  [Linux desktop packages](https://github.com/SteelCrab/podbag/actions/workflows/desktop-packages.yml).
  Extract the ZIP first.

```sh
sudo apt install ./podbag-desktop_*.deb     # Debian 12 or compatible Ubuntu
sudo dnf install ./podbag-desktop-*.rpm     # Fedora 44
```

- Open **Podbag Desktop** from the applications menu.
- The package includes the engine that the GUI uses.
- Remove it with `sudo apt purge podbag-desktop` or `sudo dnf remove podbag-desktop`.
  This also removes the `/usr/local/bin/podbag` link that the app made.

#### Build the packages

- On Debian 12, install Rust, nFPM, jq, a C compiler, pkg-config and libgtk-3-dev.
- Run `desktop/linux/package.sh` on the target architecture. It builds the engine and
  the app with its tray item (`--features podbag-desktop/linux-tray`).
- The packages are in `packages/` in Cargo's target directory.
- The app links GTK 3. When it starts, it loads libayatana-appindicator, xkbcommon, the
  X11 or Wayland client libraries, and EGL or Vulkan. The packages depend on them.
  A GPU driver draws the window: Mesa, or the driver of the GPU's vendor.

## Android

- The CLI for 64-bit ARM devices with Android 7.0 (API 24) or later.
- It has the image commands. To run containers, use the Linux build.
- Install the [Android NDK](https://developer.android.com/ndk/downloads). Set
  `ANDROID_NDK_HOME` to its folder.
- Run from the repository root:

```sh
rustup target add aarch64-linux-android
# On macOS (including Apple silicon), use darwin-x86_64 instead of linux-x86_64.
export PATH="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin:$PATH"
cargo build --locked --release --target aarch64-linux-android --bin podbag
```

- Check the program on a connected device:

```sh
adb push target/aarch64-linux-android/release/podbag /data/local/tmp/podbag
adb shell chmod 755 /data/local/tmp/podbag
adb shell /data/local/tmp/podbag --version
```

## macOS

- On a Mac, podbag runs containers in a small Linux VM.
- The VM has the Mac's CPU: arm64 on Apple silicon, x86_64 on an Intel Mac.
- A build has three steps.

1. **Build the VM's guest** (a Linux kernel, with podbag in it) on Linux with the Mac's
   CPU. [lima](https://lima-vm.io) runs that Linux VM on the Mac:

   ```sh
   dev/vm.sh up                              # create the podbag-dev VM (once)
   dev/vm.sh run dev/macos/build-guest.sh    # writes target/vm
   ```

   - The first build compiles a Linux kernel. It takes a while.
   - `target/vm/arch` names the guest's CPU.
   - For an Intel Mac on Apple silicon, run `dev/macos/build-guest.sh` on x86_64 Linux.
     Or download the `podbag-vm-guest-x86_64` artifact from a
     [CI run](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml).

2. **Build the program** with the guest in it:

   ```sh
   cargo build --release --features embed-guest
   ```

   - The build stops if the guest is for another CPU.
   - For an Intel Mac on Apple silicon, add `--target x86_64-apple-darwin`, and set
     `PODBAG_GUEST_DIR` to the x86_64 guest's directory.

3. **Sign it.** A program that starts VMs needs the virtualization entitlement:

   ```sh
   dev/macos/sign.sh target/release/podbag
   ```

### macOS desktop app and release files

- Build the guest first (step 1).
- `desktop/bundle-macos.sh release --with-cli` builds `podbag.app` with the CLI in it.
- `dev/macos/release-files.sh` builds both release files for this Mac's CPU in
  `target/release-files`: `podbag-<arch>-macos` and `podbag-desktop-<arch>-macos.zip`.
  CI uses the same script.
- More: [Build the desktop app](usage.md#build-the-desktop-app).

## Windows

- podbag.exe runs a static Linux podbag in WSL 2. A build needs both.

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

#### Install the MSI

- Each release from v0.3.0 has an MSI for x86_64 and ARM64:
  [GitHub release](https://github.com/SteelCrab/podbag/releases).
- A build of `main` is an installer artifact of
  [CI](https://github.com/SteelCrab/podbag/actions/workflows/ci.yml).
- Install the MSI and open **Podbag Desktop** from the Start menu.
- It replaces an installed Podbag Desktop in place, also one from v0.3.2 or earlier.
- Enable WSL 2 to run containers.

#### Build the MSI

- On Windows x86_64 or ARM64, build the CLI with the Linux engine for the same CPU.
- Use a native Rust toolchain for that CPU.
- Install the Visual Studio C++ build tools for your CPU and the .NET SDK 8 or later.
  The script installs WiX 6.0.2 into `target/wix`.
- Run from the repository root (`-Arch aarch64` on ARM64):

```powershell
New-Item -ItemType Directory -Force target/windows-engine | Out-Null
Copy-Item target/release/podbag.exe target/windows-engine/podbag.exe
cargo build --locked --release -p podbag-desktop
./desktop/windows/build-msi.ps1 -Arch x86_64
```

- The MSI is `target/msi/podbag-desktop-<arch>-windows.msi`. It includes the GUI and
  its matching CLI. The script checks the values that let it replace an earlier install.
- `desktop/windows/test-installed.ps1` installs it, opens the app and removes it. Run it
  only on a computer you can reset, such as a CI runner.
- CI publishes the tested MSI as `podbag-desktop-x86_64-windows` or
  `podbag-desktop-aarch64-windows`. The ARM64 one runs natively on Windows on Arm.

## Check your build

- `podbag --version` prints the version.
- `podbag check` (Linux) lists what the system still needs.
- To run the tests, see [CONTRIBUTING.md](../CONTRIBUTING.md#test).
