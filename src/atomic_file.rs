//! Locked read/modify/rename for local state shared by concurrent commands.
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::error::{os_error, Result};

pub(crate) fn update(path: &Path, change: impl FnOnce(&[u8]) -> Result<Vec<u8>>) -> Result<()> {
    // Follow an existing user symlink rather than replacing it with a regular file.
    let target = if path.is_symlink() {
        fs::canonicalize(path).map_err(|e| os_error(&e, path))?
    } else {
        path.to_path_buf()
    };
    let parent = target.parent().expect("file parent");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| os_error(&e, parent))?;
    let lock_path = parent.join(format!(
        ".{}.lock",
        target.file_name().unwrap().to_string_lossy()
    ));
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(&lock_path)
        .map_err(|e| os_error(&e, &lock_path))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)
        .map_err(|e| os_error(&e.into(), &lock_path))?;
    let original = match fs::read(&target) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(os_error(&e, &target)),
    };
    let updated = change(&original)?;
    if updated == original {
        return Ok(());
    }
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| os_error(&e, parent))?;
    temp.as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temp.write_all(&updated)
        .map_err(|e| os_error(&e, &target))?;
    temp.as_file()
        .sync_all()
        .map_err(|e| os_error(&e, &target))?;
    temp.persist(&target)
        .map_err(|e| os_error(&e.error, &target))?;
    Ok(())
}
