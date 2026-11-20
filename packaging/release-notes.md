> [!IMPORTANT]
> Before publishing: build and sign `podbag-aarch64-macos` on a Mac
> ([docs/build.md](https://github.com/SteelCrab/podbag/blob/main/docs/build.md#releases)),
> upload it, add its line to `SHA256SUMS`, and remove this note.

## Install

```sh
curl -fsSL https://podbag.dev/install.sh | sh
```

- Debian and Ubuntu: `sudo apt install ./podbag_@VERSION@_amd64.deb` (arm64: `podbag_@VERSION@_arm64.deb`)
- Fedora, RHEL and openSUSE: `sudo dnf install ./podbag-@VERSION@-1.x86_64.rpm` (arm64: `podbag-@VERSION@-1.aarch64.rpm`)

## Files

| System | File |
|---|---|
| Debian, Ubuntu | `podbag_@VERSION@_amd64.deb`, `podbag_@VERSION@_arm64.deb` |
| Fedora, RHEL, openSUSE | `podbag-@VERSION@-1.x86_64.rpm`, `podbag-@VERSION@-1.aarch64.rpm` |
| Linux, static (any distribution) | `podbag-x86_64-linux-musl`, `podbag-aarch64-linux-musl` |
| Linux, glibc 2.28 or later | `podbag-x86_64-linux-gnu`, `podbag-aarch64-linux-gnu` |
| macOS, Apple silicon | `podbag-aarch64-macos` |
| Windows (experimental) | `podbag-x86_64-windows.exe`, `podbag-aarch64-windows.exe` |

- The packages hold the static program.
- `podbag-<arch>-linux` is the same file as `podbag-<arch>-linux-musl`, for older install scripts.
- Check the files with `SHA256SUMS`.
