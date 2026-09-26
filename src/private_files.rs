//! Shared low-level private-file checks. Callers retain lifecycle and authority.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

pub(crate) fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    options.open(path)
}

pub(crate) fn checked_open(path: &Path, writable: bool) -> Result<Option<File>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect private file"),
    };
    ensure!(
        before.is_file(),
        "private files must be regular files, not symlinks"
    );
    private_permissions(&before, 0o600)?;
    let mut options = OpenOptions::new();
    options.read(true).write(writable);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    }
    let file = options.open(path)?;
    let after = file.metadata()?;
    ensure!(after.is_file(), "private files must be regular files");
    private_permissions(&after, 0o600)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == after.dev() && before.ino() == after.ino() && after.nlink() == 1,
            "private file changed or has multiple links"
        );
    }
    Ok(Some(file))
}

pub(crate) fn private_permissions(metadata: &fs::Metadata, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        ensure!(
            metadata.permissions().mode() & 0o7777 == mode,
            "private directory/files require private permissions (0700/0600)"
        );
        ensure!(
            metadata.uid() == rustix::process::geteuid().as_raw(),
            "private file owner differs from process owner"
        );
    }
    #[cfg(not(unix))]
    let _ = (metadata, mode);
    Ok(())
}
