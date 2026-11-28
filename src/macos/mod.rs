//! podbag on macOS: commands run in a Linux VM that podbag starts and stops itself.

pub mod cli;
pub mod cpu;
mod disk;
pub mod embedded;
pub mod host;
mod resources;
pub mod settings;
pub mod standby;
pub mod vm;
