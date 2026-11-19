//! openpod on macOS: commands run in a Linux VM that openpod starts and stops itself
//! (design: `2026-11-19-openpod-macos-design.md`).

pub mod cli;
pub mod embedded;
pub mod host;
pub mod standby;
pub mod vm;
