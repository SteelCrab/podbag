# Changelog

## Unreleased

### Added

- Build images from a Dockerfile with `podbag build` or the classic Docker builder.
- Save container changes as an image with `podbag commit`, list changed files with
  `podbag diff`, or export the container filesystem as a tar archive with `podbag export`.
- Log in to registries, pull private images, push images, and search Docker Hub.
  Registry commands can use existing Docker credentials.
- Manage containers, images, volumes, and networks with `podbag tui` or
  `podbag-desktop`. The macOS dashboard includes a window toolbar.
- Check stored files for corruption with `podbag system verify`. The command lists
  the images that use each damaged file.
- Install podbag on Linux and macOS with a script that checks the download's SHA-256 sum.
- Build the Android arm64 CLI in CI.

### Fixed

- Preserve file modification times from images and give each crontab its own inode
  so cron can load it. For images stored by v0.1.0, remove the old image and pull
  or load it again to apply the fix.
- Restore truncated files when a pull or load reads them again from an image layer.
- Honor the configured timeout when stopping a container.

## v0.1.0 (2026-11-20)

- Run Docker and OCI images on Linux as root or as an unprivileged user.
  The engine starts containers on demand and shares identical files across images.
- Run containers on Apple silicon Macs in an on-demand Linux VM.
  Windows support uses WSL 2 and is experimental.
- Use the Docker CLI, Compose, and buildx through podbag's Docker API on Linux and macOS.
