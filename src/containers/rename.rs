//! `rename`: a new name for a container, also while it runs. The name lives in its
//! settings (where name lookups read it) and, while it runs, in the records of its
//! networks, which write the hosts files of its peers.

use super::ContainerHandle;
use crate::paths;
use crate::state;
use crate::{Error, ErrorKind, Result};

impl ContainerHandle {
    /// Give the container the name `new_name` (`docker rename`). A name in use by another
    /// container is a conflict.
    pub fn rename(&self, new_name: &str) -> Result<()> {
        let dirs = paths::dirs()?;
        let new_name = new_name.strip_prefix('/').unwrap_or(new_name);
        if new_name.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot rename the container",
                "Neither old nor new names may be empty",
                "give the new name",
            ));
        }
        crate::container::check_name(new_name)?;
        {
            let _names = state::lock_names(&dirs)?;
            let mut spec = state::read_config(&dirs, &self.id)?;
            if spec.name == new_name {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    format!("cannot rename container {}", spec.name),
                    "Renaming a container with the same name as its current name",
                    "give another name",
                ));
            }
            if let Some(other) = state::named(&dirs, new_name, &self.id)? {
                return Err(name_in_use(new_name, &other));
            }
            spec.name = new_name.to_string();
            state::write_config(&dirs, &spec)?;
        }
        // Its peers find it by the new name. A network that cannot be read is skipped:
        // the name is changed, and the next start writes the records again.
        if let Ok(networks) = crate::network::rename_member(&dirs, &self.id, new_name) {
            for network in networks {
                crate::network::refresh_hosts(&dirs, &network);
            }
        }
        Ok(())
    }
}

/// Docker's words for a name in use.
fn name_in_use(name: &str, other: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!(
            "Conflict. The container name \"/{name}\" is already in use by container \"{other}\""
        ),
        "You have to remove (or rename) that container to be able to reuse that name",
        "choose another name, or remove that container",
    )
}
