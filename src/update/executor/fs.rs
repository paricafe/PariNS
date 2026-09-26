//! Root files use held directory descriptors. No app-owned path is a write target.
use anyhow::{Result, ensure};
use rustix::fs::{self, AtFlags, Mode, OFlags};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    path::Path,
};

pub struct Dir {
    pub fd: OwnedFd,
}

/// Adopt the trusted installer's held lock without creating an inheritable
/// duplicate. The caller has already validated the expected lock file.
pub(super) fn adopt_installer_lock(handoff: impl AsFd, expected: &fs::Stat) -> Result<OwnedFd> {
    rustix::io::fcntl_setfd(handoff.as_fd(), rustix::io::FdFlags::CLOEXEC)?;
    let inherited = rustix::io::fcntl_dupfd_cloexec(handoff.as_fd(), 0)?;
    let actual = fs::fstat(&inherited)?;
    ensure!(
        actual.st_ino == expected.st_ino && actual.st_dev == expected.st_dev,
        "installer lock was not handed off"
    );
    Ok(inherited)
}
impl Dir {
    pub fn exists(&self, name: &str) -> Result<bool> {
        match fs::statat(&self.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
    pub fn open(path: &Path, root_owned: bool) -> Result<Self> {
        let fd = fs::open(
            path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&fd)?;
        if root_owned {
            ensure!(
                stat.st_uid == 0 && stat.st_mode & 0o022 == 0,
                "untrusted root directory"
            );
        }
        Ok(Self { fd })
    }
    pub fn child(&self, name: &str) -> Result<Self> {
        let fd = fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&fd)?;
        ensure!(
            stat.st_uid == 0 && stat.st_mode & 0o022 == 0,
            "untrusted child directory"
        );
        Ok(Self { fd })
    }
    pub fn create_dir(&self, name: &str, mode: u32) -> Result<Self> {
        let created = match fs::mkdirat(&self.fd, name, Mode::from_raw_mode(mode as _)) {
            Ok(()) => true,
            Err(rustix::io::Errno::EXIST) => false,
            Err(error) => return Err(error.into()),
        };
        let dir = self.child(name)?;
        if created {
            fs::fchmod(&dir.fd, Mode::from_raw_mode(mode as _))?;
            dir.sync()?;
            self.sync()?;
        }
        Ok(dir)
    }
    pub fn file(&self, name: &str, limit: u64, root_owned: bool) -> Result<File> {
        let fd = fs::openat(
            &self.fd,
            name,
            OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = fs::fstat(&fd)?;
        ensure!(
            fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::RegularFile
                && stat.st_nlink == 1
                && stat.st_size >= 0
                && stat.st_size as u64 <= limit,
            "invalid bounded file"
        );
        if root_owned {
            ensure!(
                stat.st_uid == 0 && stat.st_mode & 0o022 == 0,
                "untrusted root file"
            );
        }
        Ok(fd.into())
    }
    pub fn read(&self, name: &str, limit: usize, root_owned: bool) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.file(name, limit as u64, root_owned)?
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "oversized file");
        Ok(bytes)
    }
    pub fn create(&self, name: &str, mode: u32) -> Result<File> {
        let fd = fs::openat(
            &self.fd,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(mode as _),
        )?;
        fs::fchmod(&fd, Mode::from_raw_mode(mode as _))?;
        Ok(fd.into())
    }
    pub fn remove(&self, name: &str) -> Result<()> {
        match fs::unlinkat(&self.fd, name, AtFlags::empty()) {
            Ok(()) => self.sync(),
            Err(rustix::io::Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    pub fn remove_dir(&self, name: &str) -> Result<()> {
        fs::unlinkat(&self.fd, name, AtFlags::REMOVEDIR)?;
        self.sync()
    }
    pub fn replace(&self, name: &str, bytes: &[u8], mode: u32) -> Result<()> {
        // One reserved temporary per destination, protected by the root lock.
        // A crash cannot accumulate random orphan files across operations.
        let temp = format!(".{name}.tmp");
        self.remove(&temp)?;
        let mut file = self.create(&temp, mode)?;
        file.write_all(bytes)?;
        boundary()?;
        sync_file(&file)?;
        fs::renameat(&self.fd, &temp, &self.fd, name)?;
        boundary()?;
        self.sync()
    }
    pub fn sync(&self) -> Result<()> {
        fs::fsync(&self.fd)?;
        boundary()?;
        Ok(())
    }
    pub fn rename_to(&self, name: &str, other: &Self, target: &str) -> Result<()> {
        fs::renameat(&self.fd, name, &other.fd, target)?;
        boundary()?;
        other.sync()?;
        self.sync()
    }
    pub fn space(&self, required: u64) -> Result<()> {
        let stat = fs::fstatvfs(&self.fd)?;
        ensure!(
            stat.f_bavail.saturating_mul(stat.f_frsize) >= required,
            "insufficient_space"
        );
        Ok(())
    }
}

pub fn sync_file(file: &File) -> Result<()> {
    file.sync_all()?;
    boundary()
}

#[cfg(not(test))]
fn boundary() -> Result<()> {
    Ok(())
}
#[cfg(test)]
thread_local! {static FAIL_AFTER:std::cell::Cell<Option<usize>>=const {std::cell::Cell::new(None)};}
#[cfg(test)]
fn boundary() -> Result<()> {
    FAIL_AFTER.with(|slot| match slot.get() {
        None => Ok(()),
        Some(0) => {
            slot.set(None);
            anyhow::bail!("injected durable boundary interruption")
        }
        Some(left) => {
            slot.set(Some(left - 1));
            Ok(())
        }
    })
}

pub fn sha256(mut file: File) -> Result<String> {
    let mut digest = Sha256::new();
    let mut chunk = [0; 65536];
    loop {
        let size = file.read(&mut chunk)?;
        if size == 0 {
            break;
        }
        digest.update(&chunk[..size]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

pub fn validate_elf(mut file: File, target: &str) -> Result<()> {
    use std::io::{Seek, SeekFrom};
    let mut h = [0; 64];
    file.read_exact(&mut h)?;
    ensure!(&h[..7] == b"\x7fELF\x02\x01\x01", "invalid ELF header");
    let machine = u16::from_le_bytes([h[18], h[19]]);
    ensure!(
        matches!(
            (target, machine),
            ("x86_64-unknown-linux-musl", 62) | ("aarch64-unknown-linux-musl", 183)
        ),
        "invalid ELF target"
    );
    let kind = u16::from_le_bytes([h[16], h[17]]);
    ensure!(kind == 2 || kind == 3, "invalid ELF kind");
    let offset = u64::from_le_bytes(h[32..40].try_into()?);
    let stride = u16::from_le_bytes([h[54], h[55]]);
    let count = u16::from_le_bytes([h[56], h[57]]);
    ensure!(
        stride == 56 && count > 0 && count <= 128,
        "invalid ELF program table"
    );
    let length = file.metadata()?.len();
    ensure!(
        offset
            .checked_add(u64::from(stride) * u64::from(count))
            .is_some_and(|end| end <= length),
        "truncated ELF program table"
    );
    for index in 0..count {
        file.seek(SeekFrom::Start(offset + u64::from(index) * 56))?;
        let mut entry = [0; 56];
        file.read_exact(&mut entry)?;
        let kind = u32::from_le_bytes(entry[..4].try_into()?);
        ensure!(kind != 3, "dynamic ELF interpreter is unsupported");
        if kind == 2 {
            let at = u64::from_le_bytes(entry[8..16].try_into()?);
            let len = u64::from_le_bytes(entry[32..40].try_into()?);
            ensure!(
                len <= 1024 * 1024 && at.checked_add(len).is_some_and(|end| end <= length),
                "invalid ELF dynamic table"
            );
            file.seek(SeekFrom::Start(at))?;
            for _ in 0..len / 16 {
                let mut pair = [0; 16];
                file.read_exact(&mut pair)?;
                let tag = u64::from_le_bytes(pair[..8].try_into()?);
                if tag == 0 {
                    break;
                }
                ensure!(tag != 1, "ELF shared dependency is unsupported");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "fs_tests.rs"]
mod tests;
