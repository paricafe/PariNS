//! Shared low-level private-file and private-directory checks, including the
//! directory lock. Callers retain lifecycle and authority.
use anyhow::{Context, Result, ensure};
use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

/// Open external configuration material without waiting for a FIFO writer.
pub(crate) fn open_regular(path: &Path, what: &str) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)
        .with_context(|| format!("open {what}"))?;
    ensure!(file.metadata()?.is_file(), "{what} must be a regular file");
    Ok(file)
}

pub(crate) fn read_regular(path: &Path, limit: u64, what: &str) -> Result<Vec<u8>> {
    use std::io::Read;
    let file = open_regular(path, what)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "{what} exceeds {limit} bytes"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "{what} exceeds {limit} bytes");
    Ok(bytes)
}

pub(crate) fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
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
    options
        .read(true)
        .write(writable)
        .custom_flags(rustix::fs::OFlags::NOFOLLOW.bits() as i32);
    let file = options.open(path)?;
    let after = file.metadata()?;
    ensure!(after.is_file(), "private files must be regular files");
    private_permissions(&after, 0o600)?;
    ensure!(
        before.dev() == after.dev() && before.ino() == after.ino() && after.nlink() == 1,
        "private file changed or has multiple links"
    );
    Ok(Some(file))
}

pub(crate) fn private_permissions(metadata: &fs::Metadata, mode: u32) -> Result<()> {
    ensure!(
        metadata.permissions().mode() & 0o7777 == mode,
        "private directory/files require private permissions (0700/0600)"
    );
    ensure!(
        metadata.uid() == rustix::process::geteuid().as_raw(),
        "private file owner differs from process owner"
    );
    Ok(())
}

/// Creates `path` with mode 0700 when missing, resolves it, and takes the
/// exclusive lock on its private `lock` file. The input may be a symlink (a
/// systemd DynamicUser state directory is one); the resolved directory must be
/// private and be neither the filesystem root nor HOME. `what` names the
/// directory in errors.
pub(crate) fn lock_private_dir(path: &Path, what: &str) -> Result<(PathBuf, File)> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
        .with_context(|| format!("create {what} directory"))?;
    let dir = path
        .canonicalize()
        .with_context(|| format!("resolve {what} directory"))?;
    ensure!(
        dir.parent().is_some(),
        "{what} directory input '{}' resolves to filesystem root '{}'; use a dedicated private subdirectory",
        path.display(),
        dir.display()
    );
    if let Some(home) = std::env::var_os("HOME")
        && let Ok(home) = Path::new(&home).canonicalize()
    {
        ensure!(
            dir != home,
            "{what} directory input '{}' resolves to HOME '{}'; use a dedicated private subdirectory, not the account home",
            path.display(),
            dir.display()
        );
    }
    let metadata = fs::metadata(&dir)?;
    ensure!(metadata.is_dir(), "{what} path must be a directory");
    private_permissions(&metadata, 0o700)?;
    let lock_path = dir.join("lock");
    let lock = match create_private(&lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            checked_open(&lock_path, true)?.with_context(|| format!("{what} lock disappeared"))?
        }
        Err(error) => return Err(error).with_context(|| format!("create {what} lock")),
    };
    lock.try_lock()
        .with_context(|| format!("{what} directory {} is already in use", dir.display()))?;
    Ok((dir, lock))
}
