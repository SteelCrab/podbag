# Building podbag from source

## Contents

- [What you need](#what-you-need)
- [Linux](#linux)
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

## Check your build

- `podbag --version` prints the version.
- `podbag check` (Linux) lists what the system still needs.
- To run the tests, see [CONTRIBUTING.md](../CONTRIBUTING.md#test).
