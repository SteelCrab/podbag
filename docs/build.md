# Building podbag from source

## Contents

- [What you need](#what-you-need)
- [Linux](#linux)
- [macOS](#macos)
- [Windows](#windows)
- [Releases](#releases)
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

### glibc build

The release's `-gnu` files. They use the system's glibc, version 2.28 or later (for
example RHEL 8, Debian 10, Ubuntu 20.04).

```sh
pip install cargo-zigbuild                       # with zig, its linker
rustup target add x86_64-unknown-linux-gnu       # on arm64: aarch64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
```

- The program is `target/x86_64-unknown-linux-gnu/release/podbag`.
- `.2.28` links against glibc 2.28, so the program also runs on older systems than the
  one that built it.

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
   - download `podbag-x86_64-linux-musl` or `podbag-aarch64-linux-musl` from a
     [release](https://github.com/SteelCrab/podbag/releases).

2. **Build podbag.exe** with it:

   ```powershell
   $env:PODBAG_LINUX_BIN = "C:\path\to\podbag-x86_64-linux-musl"
   cargo build --release --features embed-linux
   ```

- On a Mac or on Linux, `--target x86_64-pc-windows-gnu` cross-compiles. It needs
  `mingw-w64`.

## Releases

A `v*` tag starts [release.yml](../.github/workflows/release.yml). It makes a draft
release; a maintainer checks it and publishes it.

### Steps

1. Set the version in `Cargo.toml` and merge it to `main`.
2. Tag it: `git tag v0.2.0 && git push origin v0.2.0`. The tag must match `Cargo.toml`.
3. The workflow builds and tests the files below, writes `SHA256SUMS` and makes the draft.
4. Build and sign `podbag-aarch64-macos` on a Mac (see [macOS](#macos)). Upload it, and
   add its line to `SHA256SUMS`:

   ```sh
   gh release download v0.2.0 -p SHA256SUMS
   shasum -a 256 podbag-aarch64-macos >> SHA256SUMS
   gh release upload v0.2.0 podbag-aarch64-macos SHA256SUMS --clobber
   ```

5. Remove the note at the top of the draft, and publish it.

### Files

| File | What it is |
|---|---|
| `podbag-<arch>-linux-musl` | the [static build](#static-build) |
| `podbag-<arch>-linux` | the same file, for older install scripts |
| `podbag-<arch>-linux-gnu` | the [glibc build](#glibc-build) |
| `podbag_<ver>_amd64.deb`, `podbag_<ver>_arm64.deb` | the static build as a Debian package |
| `podbag-<ver>-1.x86_64.rpm`, `podbag-<ver>-1.aarch64.rpm` | the static build as an RPM package |
| `podbag-<arch>-windows.exe` | see [Windows](#windows) |
| `podbag-aarch64-macos` | built and uploaded by hand |
| `SHA256SUMS` | the SHA-256 sums of all files |

- `<arch>` is `x86_64` or `aarch64`.

### Packages

- [nfpm](https://nfpm.goreleaser.com) builds the .deb and the .rpm from
  [packaging/nfpm.yaml](../packaging/nfpm.yaml).
- They install `/usr/bin/podbag`, and the license and README in `/usr/share/doc/podbag`.
- The .deb also installs `/etc/apparmor.d/podbag`, a configuration file:
  - The package loads it where AppArmor restricts user namespaces (Ubuntu 23.10 and later).
  - AppArmor before 4.0 cannot read it. There, the package adds a link in
    `/etc/apparmor.d/disable`, so that AppArmor skips it.
  - Removing the package unloads the profile. Purging it removes the file.

### Tests

The workflow also runs on pull requests that change the packaging, and by hand. Then it
builds and tests, but publishes nothing.

- The .deb on Ubuntu 26.04: install, a rootless container with AppArmor's restriction on,
  purge.
- The .rpm in a Fedora 44 container: install, run, remove.
- The glibc build and the packages on AlmaLinux 8 (glibc 2.28) and Debian 11.
- `install.sh` with the new and the older file names.
- All on x86_64 and arm64.

## Check your build

- `podbag --version` prints the version.
- `podbag check` (Linux) lists what the system still needs.
- To run the tests, see [CONTRIBUTING.md](../CONTRIBUTING.md#test).
