# Contributing to podbag

Thank you for helping. Bug reports, fixes and new features are all welcome.

## Contents

- [Report a problem](#report-a-problem)
- [Build](#build)
- [Test](#test)
- [Pull requests](#pull-requests)
- [Translations](#translations)
- [Website](#website)
- [License](#license)

## Report a problem

Open an issue. Include:

- the command you ran,
- what you expected, and what happened,
- the output of `podbag --version` and `podbag check`.

## Build

```sh
cargo build    # the program and the library
```

- Release builds for Linux, macOS and Windows: [docs/build.md](docs/build.md).

## Test

### On any system

```sh
cargo test     # unit tests; tests that need Linux are skipped elsewhere
```

### Linux tests from a Mac

podbag uses Linux kernel features, so most tests run on Linux. On a Mac,
[lima](https://lima-vm.io) runs them in a VM:

```sh
dev/vm.sh up           # create and set up the podbag-dev VM (once)
dev/vm.sh test         # cargo test in the VM, without root
dev/vm.sh test-root    # cargo test in the VM, as root
dev/vm.sh static       # the static release binary, and a check of its size
```

### More tests

- `PODBAG_TEST_NETWORK=1` adds the tests that pull images or use the network.
- `PODBAG_TEST_MACOS_VM=1 cargo test --test macos_vm` tests the macOS VM.

## Pull requests

- One change per pull request, with tests for new behavior.
- Run `cargo fmt`.
- Leave no compiler warnings.
- `cargo test` must pass. CI checks Linux, macOS and Windows.
- Commit messages are one line: `type: what changed`. Types: `feat`, `fix`, `docs`,
  `test`, `refactor`, `chore`.

## Translations

- `README.md` is the source. `README.<language>.md` files follow it.
- When you change `README.md`, say so in the pull request, so that the translations can
  follow.
- The website's words are in `site/i18n/<language>.json`. Every file needs every key that
  `en.json` has: the build stops when one is missing.
- The words in the layers-and-files picture are in `dev/docs/storage.py` (`TEXT`).

## Website

- The site is in `site/`: a page template (`page.html`), one string file per language
  (`i18n/<language>.json`), the style and the script.
- Build it, then look at it:

```sh
python3 dev/docs/site.py _site
python3 -m http.server -d _site 8000    # then open http://localhost:8000
```

- A push to `main` deploys it to https://podbag.dev on Cloudflare
  (`.github/workflows/site.yml`).
- The pictures come from the scripts in `dev/docs/` and from `dev/bench/charts.py`. Run the
  script again after you change one.

## License

By contributing, you agree that your work is licensed under the Apache License 2.0.
