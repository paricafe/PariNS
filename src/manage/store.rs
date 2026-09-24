//! Private, single-writer management state. Credentials and configuration share
//! one commit point so interrupted setup cannot create an unauthenticated server.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use serde::{Deserialize, Serialize};

const MAX_STATE: usize = 2 * 1024 * 1024;
const MAX_CONFIG: usize = 256 * 1024;
const STATE: &str = "state.json";
const TOKEN: &str = "setup-token";

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Stored {
    pub username: String,
    pub password_hash: String,
    pub toml: String,
    pub previous: Option<String>,
    pub revision: u64,
}

pub(super) struct Store {
    pub dir: PathBuf,
    _lock: File,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(path)
            .context("create management directory")?;
        // systemd DynamicUser StateDirectory may itself be a legitimate symlink.
        // All later access is through its resolved, private directory.
        let dir = path
            .canonicalize()
            .context("resolve management directory")?;
        ensure!(
            dir.parent().is_some(),
            "management directory input '{}' resolves to filesystem root '{}'; use a dedicated private subdirectory",
            path.display(),
            dir.display()
        );
        if let Some(home) = std::env::var_os("HOME")
            && let Ok(home) = Path::new(&home).canonicalize()
        {
            ensure!(
                dir != home,
                "management directory input '{}' resolves to HOME '{}'; use a dedicated private subdirectory, not the account home",
                path.display(),
                dir.display()
            );
        }
        let metadata = fs::metadata(&dir)?;
        ensure!(metadata.is_dir(), "management path must be a directory");
        private_permissions(&metadata, 0o700)?;

        let lock_path = dir.join("lock");
        let lock = match create_private(&lock_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                checked_open(&lock_path, true)?.context("management lock disappeared")?
            }
            Err(error) => return Err(error).context("create management lock"),
        };
        lock.try_lock()
            .context("management directory is already in use")?;
        let store = Self { dir, _lock: lock };
        // Invalid state is an error, never an invitation to run setup again.
        store.read()?;
        Ok(store)
    }

    pub fn read(&self) -> Result<Option<Stored>> {
        let Some(bytes) = read_bounded(&self.dir.join(STATE), MAX_STATE)? else {
            return Ok(None);
        };
        let stored: Stored = serde_json::from_slice(&bytes).context("invalid management state")?;
        validate_stored(&stored)?;
        Ok(Some(stored))
    }

    pub fn save(&self, stored: &Stored) -> Result<()> {
        validate_stored(stored)?;
        let bytes = serde_json::to_vec(stored).context("encode management state")?;
        ensure!(bytes.len() <= MAX_STATE, "management state is too large");
        self.atomic_write(STATE, &bytes)
    }

    pub fn setup_token(&self) -> Result<String> {
        ensure!(self.read()?.is_none(), "management is already initialized");
        if let Some(bytes) = read_bounded(&self.dir.join(TOKEN), 64)? {
            ensure!(
                bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit),
                "invalid setup token"
            );
            return String::from_utf8(bytes).context("invalid setup token");
        }
        let token = secret();
        self.atomic_write(TOKEN, token.as_bytes())?;
        Ok(token)
    }

    pub(super) fn atomic_write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let destination = self.dir.join(name);
        // Do not replace unexpected symlinks, devices or exposed secret files.
        checked_open(&destination, false)?;
        let temporary = Temporary(self.dir.join(format!(".{name}.{}.tmp", secret())));
        let mut file =
            create_private(&temporary.0).context("create private state temporary file")?;
        file.write_all(bytes).context("write management state")?;
        file.sync_all().context("synchronize management state")?;
        fs::rename(&temporary.0, destination).context("publish management state")?;
        // rename is the commit point. A subsequent directory-fsync failure must
        // not tell the runtime to roll back a configuration already published.
        // Atomic visibility still holds, but power-loss durability is uncertain.
        if File::open(&self.dir)
            .and_then(|dir| dir.sync_all())
            .is_err()
        {
            eprintln!(
                "management state committed; directory sync failed, power-loss durability is uncertain"
            );
        }
        Ok(())
    }
}

fn validate_stored(stored: &Stored) -> Result<()> {
    ensure!(
        !stored.username.trim().is_empty() && stored.username.len() <= 64,
        "invalid management username"
    );
    ensure!(stored.revision > 0, "invalid management revision");
    ensure!(
        !stored.toml.is_empty() && stored.toml.len() <= MAX_CONFIG,
        "invalid management configuration size"
    );
    ensure!(
        stored
            .previous
            .as_ref()
            .is_none_or(|toml| !toml.is_empty() && toml.len() <= MAX_CONFIG),
        "invalid previous configuration size"
    );
    ensure!(
        valid_hash(&stored.password_hash).is_some(),
        "invalid management password hash"
    );
    Ok(())
}

pub(crate) fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub(crate) fn checked_open(path: &Path, writable: bool) -> Result<Option<File>> {
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect management file"),
    };
    ensure!(
        before.is_file(),
        "management files must be regular files, not symlinks"
    );
    private_permissions(&before, 0o600)?;
    let file = OpenOptions::new().read(true).write(writable).open(path)?;
    let after = file.metadata()?;
    ensure!(after.is_file(), "management files must be regular files");
    private_permissions(&after, 0o600)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            before.dev() == after.dev() && before.ino() == after.ino() && after.nlink() == 1,
            "management file changed or has multiple links"
        );
    }
    Ok(Some(file))
}

pub(crate) fn private_permissions(metadata: &fs::Metadata, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            metadata.permissions().mode() & 0o7777 == mode,
            "management directory/files require private permissions (0700/0600)"
        );
    }
    #[cfg(not(unix))]
    let _ = (metadata, mode);
    Ok(())
}

pub(super) fn read_bounded(path: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
    let Some(file) = checked_open(path, false)? else {
        return Ok(None);
    };
    ensure!(
        file.metadata()?.len() <= limit as u64,
        "management file is too large"
    );
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= limit, "management file is too large");
    Ok(Some(bytes))
}

struct Temporary(PathBuf);

impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub(super) fn secret() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(super) fn hash_password(password: &str) -> Result<String> {
    ensure!(
        (12..=256).contains(&password.len()),
        "password must contain 12 to 256 bytes"
    );
    let salt = SaltString::encode_b64(&rand::random::<[u8; 16]>())
        .map_err(|_| anyhow::anyhow!("generate password salt"))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| anyhow::anyhow!("hash management password"))
}

fn valid_hash(hash: &str) -> Option<PasswordHash<'_>> {
    if hash.len() > 512 {
        return None;
    }
    let parsed = PasswordHash::new(hash).ok()?;
    // This store writes precisely this profile. Fail closed on corrupt or
    // hostile PHC costs instead of accepting unbounded CPU/memory parameters.
    let params = argon2::Params::try_from(&parsed).ok()?;
    let defaults = argon2::Params::default();
    (parsed.algorithm.as_str() == "argon2id"
        && parsed.version == Some(19)
        && parsed.salt.is_some()
        && parsed.hash.is_some()
        && parsed.params.iter().count() == 3
        && params.m_cost() == defaults.m_cost()
        && params.t_cost() == defaults.t_cost()
        && params.p_cost() == defaults.p_cost()
        && params.output_len() == Some(32))
    .then_some(parsed)
}

pub(super) fn verify_password(hash: &str, password: &str) -> bool {
    if !(12..=256).contains(&password.len()) {
        return false;
    }
    valid_hash(hash).is_some_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_directory() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        }
        directory
    }

    fn stored() -> Stored {
        Stored {
            username: "admin".into(),
            password_hash: hash_password("correct horse battery").unwrap(),
            toml: "listen = '127.0.0.1:5353'".into(),
            previous: None,
            revision: 1,
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_and_home_rejections_identify_input_and_resolved_directory() {
        let error = Store::open(Path::new("/"))
            .err()
            .expect("root must not be used as private state")
            .to_string();
        assert!(
            error.contains("input '/'")
                && error.contains("filesystem root '/'")
                && error.contains("subdirectory")
        );
        if let Some(home) = std::env::var_os("HOME") {
            let path = Path::new(&home);
            if let Ok(resolved) = path.canonicalize() {
                let error = Store::open(path)
                    .err()
                    .expect("HOME must not be used as private state")
                    .to_string();
                assert!(error.contains(&format!("input '{}'", path.display())));
                assert!(error.contains(&format!("HOME '{}'", resolved.display())));
                assert!(error.contains("dedicated private subdirectory"));
            }
        }
    }

    #[test]
    fn state_round_trip_previous_and_exclusive_lock() {
        let directory = private_directory();
        let store = Store::open(directory.path()).unwrap();
        assert!(store.read().unwrap().is_none());
        assert!(Store::open(directory.path()).is_err());
        let mut state = stored();
        store.save(&state).unwrap();
        state.previous = Some(state.toml.clone());
        state.toml = "listen = '127.0.0.1:5354'".into();
        state.revision += 1;
        store.save(&state).unwrap();
        assert!(store.setup_token().is_err());
        drop(store);
        let reopened = Store::open(directory.path()).unwrap();
        let read = reopened.read().unwrap().unwrap();
        assert_eq!(read.toml, state.toml);
        assert_eq!(read.previous, state.previous);
        assert_eq!(read.revision, 2);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn token_is_persistent_and_validated() {
        let directory = private_directory();
        let store = Store::open(directory.path()).unwrap();
        let token = store.setup_token().unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(token, secret());
        assert_eq!(token, store.setup_token().unwrap());
        drop(store);
        let store = Store::open(directory.path()).unwrap();
        assert_eq!(token, store.setup_token().unwrap());
        fs::write(directory.path().join(TOKEN), "invalid").unwrap();
        assert!(store.setup_token().is_err());
        fs::write(directory.path().join(TOKEN), "a".repeat(65)).unwrap();
        assert!(store.setup_token().is_err());
    }

    #[test]
    fn corrupt_and_oversized_state_never_reset_setup() {
        let directory = private_directory();
        let store = Store::open(directory.path()).unwrap();
        store.save(&stored()).unwrap();
        let path = directory.path().join(STATE);
        fs::write(&path, "{broken").unwrap();
        assert!(store.read().is_err());
        assert!(store.setup_token().is_err());
        drop(store);
        assert!(Store::open(directory.path()).is_err());
        fs::write(path, vec![b' '; MAX_STATE + 1]).unwrap();
        assert!(Store::open(directory.path()).is_err());
    }

    #[test]
    fn rejected_update_preserves_published_state() {
        let directory = private_directory();
        let store = Store::open(directory.path()).unwrap();
        let mut state = stored();
        store.save(&state).unwrap();
        state.toml = "x".repeat(MAX_CONFIG + 1);
        assert!(store.save(&state).is_err());
        assert_eq!(store.read().unwrap().unwrap().revision, 1);
        state.toml = "valid = true".into();
        state.revision = 0;
        assert!(store.save(&state).is_err());
        state.revision = 2;
        state.password_hash = "invalid".into();
        assert!(store.save(&state).is_err());
        assert_eq!(store.read().unwrap().unwrap().toml, stored().toml);
    }

    #[test]
    fn password_hash_is_salted_and_bounded() {
        let hash = hash_password("correct horse battery").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$"));
        assert_ne!(hash, hash_password("correct horse battery").unwrap());
        assert!(verify_password(&hash, "correct horse battery"));
        assert!(!verify_password(&hash, "incorrect horse battery"));
        assert!(!verify_password("invalid", "correct horse battery"));
        assert!(hash_password("short").is_err());
        assert!(hash_password(&"a".repeat(257)).is_err());
        assert!(!verify_password(&hash, &"a".repeat(257)));
        assert!(!verify_password(
            &hash.replace("m=19456", "m=999999"),
            "correct horse battery"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn permissions_are_private_and_existing_directories_are_not_chmodded() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/state");
        let store = Store::open(&path).unwrap();
        store.setup_token().unwrap();
        store.save(&stored()).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        for name in [STATE, TOKEN, "lock"] {
            assert_eq!(
                fs::metadata(path.join(name)).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Store::open(directory.path()).is_err());
        assert_eq!(
            fs::metadata(directory.path()).unwrap().permissions().mode() & 0o777,
            0o755
        );
        fs::set_permissions(path.join(STATE), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.read().is_err());
        assert!(store.save(&stored()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_symlink_supported_but_file_links_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        let first = Store::open(&real).unwrap();
        first.save(&stored()).unwrap();
        drop(first);
        let link = directory.path().join("link");
        symlink(&real, &link).unwrap();
        let store = Store::open(&link).unwrap();
        assert_eq!(store.dir, real.canonicalize().unwrap());
        let outside = directory.path().join("outside");
        fs::write(&outside, b"untouched").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o600)).unwrap();
        for name in [STATE, TOKEN] {
            let path = real.join(name);
            let _ = fs::remove_file(&path);
            symlink(&outside, &path).unwrap();
            assert!(if name == STATE {
                store.read().is_err()
            } else {
                store.setup_token().is_err()
            });
            assert!(store.atomic_write(name, b"replacement").is_err());
            fs::remove_file(path).unwrap();
        }
        assert_eq!(fs::read(&outside).unwrap(), b"untouched");
        drop(store);
        fs::remove_file(real.join("lock")).unwrap();
        symlink(&outside, real.join("lock")).unwrap();
        assert!(Store::open(&real).is_err());
        fs::remove_file(real.join("lock")).unwrap();
        fs::hard_link(&outside, real.join("lock")).unwrap();
        assert!(Store::open(&real).is_err());
    }
}
