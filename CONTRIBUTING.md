# Contributing to podbag

Thank you for helping. Bug reports, fixes and new features are all welcome.

## Report a problem

Open an issue with the command you ran, what you expected, what happened, and the output
of `podbag --version` and `podbag check`.

## Build and test

```sh
cargo build    # the CLI and the library
cargo test     # unit tests; tests that need Linux are skipped elsewhere
```

podbag uses Linux kernel features, so most tests run on Linux. On a Mac,
[lima](https://lima-vm.io) runs them in a VM:

```sh
dev/vm.sh up           # create and set up the podbag-dev VM (once)
dev/vm.sh test         # cargo test in the VM, rootless
dev/vm.sh test-root    # cargo test in the VM, as root
dev/vm.sh static       # the static release binary, and a check of its size
```

`PODBAG_TEST_NETWORK=1` adds the tests that pull images or use the network.
`PODBAG_TEST_MACOS_VM=1 cargo test --test macos_vm` tests the macOS VM.

Release builds:

- Linux, a static binary (needs `musl-tools`):
  `CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --target x86_64-unknown-linux-musl`
  (or `aarch64` for arm64).
- macOS: `dev/vm.sh run dev/macos/build-guest.sh`, then
  `cargo build --release --features embed-guest`, then `dev/macos/sign.sh target/release/podbag`.
- Windows: build the static Linux binary for the same CPU first, then
  `PODBAG_LINUX_BIN=<that file> cargo build --release --features embed-linux`.

## Pull requests

- One change per pull request, with tests for new behavior.
- Run `cargo fmt`; leave no compiler warnings; `cargo test` must pass. CI checks Linux,
  macOS and Windows.
- Commit messages are one line: `type: what changed`, with `feat`, `fix`, `docs`, `test`,
  `refactor` or `chore`.

By contributing, you agree that your work is licensed under the Apache License 2.0.
