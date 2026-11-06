// Copyright (c) 2024-present, fjall-rs
// This source code is licensed under both the Apache 2.0 and MIT License
// (found in the LICENSE-* files in the repository)

use std::{
    fs::{File, OpenOptions, TryLockError},
    path::Path,
    sync::Arc,
};

/// Takes an exclusive lock on `file` without blocking.
///
/// The standard library does not lock files on every target (Android among
/// them, where it reports `Unsupported`); there, `flock` takes the same
/// advisory lock.
fn try_lock(file: &File) -> Result<(), TryLockError> {
    match file.try_lock() {
        Err(TryLockError::Error(e)) if e.kind() == std::io::ErrorKind::Unsupported => {
            flock::try_lock(file)
        }
        result => result,
    }
}

/// Releases the lock [`try_lock`] took.
fn unlock(file: &File) -> std::io::Result<()> {
    match file.unlock() {
        Err(e) if e.kind() == std::io::ErrorKind::Unsupported => flock::unlock(file),
        result => result,
    }
}

#[cfg(unix)]
mod flock {
    use rustix::fs::FlockOperation;
    use std::{fs::File, fs::TryLockError, io};

    pub fn try_lock(file: &File) -> Result<(), TryLockError> {
        match rustix::fs::flock(file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(()),
            Err(e) if e == rustix::io::Errno::WOULDBLOCK => Err(TryLockError::WouldBlock),
            Err(e) => Err(TryLockError::Error(e.into())),
        }
    }

    pub fn unlock(file: &File) -> io::Result<()> {
        Ok(rustix::fs::flock(file, FlockOperation::Unlock)?)
    }
}

#[cfg(not(unix))]
mod flock {
    use std::{fs::File, fs::TryLockError, io};

    pub fn try_lock(_: &File) -> Result<(), TryLockError> {
        Err(TryLockError::Error(io::ErrorKind::Unsupported.into()))
    }

    pub fn unlock(_: &File) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

struct LockedFileGuardInner(File);

impl Drop for LockedFileGuardInner {
    fn drop(&mut self) {
        log::debug!("Unlocking database lock");

        unlock(&self.0)
            .inspect_err(|e| {
                log::warn!("Failed to unlock database lock: {e:?}");
            })
            .ok();
    }
}

#[derive(Clone)]
#[expect(unused)]
pub struct LockedFileGuard(Arc<LockedFileGuardInner>);

impl LockedFileGuard {
    pub fn create_new(path: &Path) -> crate::Result<Self> {
        log::debug!("Acquiring database lock at {}", path.display());

        let file = match File::create_new(path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                OpenOptions::new().read(true).write(true).open(path)?
            }
            e => e?,
        };

        try_lock(&file).map_err(|e| match e {
            TryLockError::Error(e) => {
                log::error!("Failed to acquire database lock - if this is expected, you can try opening again (maybe wait a little)");
                crate::Error::Io(e)
            }
            TryLockError::WouldBlock => crate::Error::Locked,
        })?;

        Ok(Self(Arc::new(LockedFileGuardInner(file))))
    }

    pub fn try_acquire(path: &Path) -> crate::Result<Self> {
        const RETRIES: usize = 3;

        log::debug!("Acquiring database lock at {}", path.display());

        let file = OpenOptions::new().read(true).write(true).open(path)?;

        for i in 1..=RETRIES {
            if let Err(e) = try_lock(&file) {
                match e {
                    TryLockError::Error(e) => {
                        log::error!("Failed to acquire database lock - if this is expected, you can try opening again (maybe wait a little)");
                        return Err(crate::Error::Io(e));
                    }
                    TryLockError::WouldBlock => {
                        if i == RETRIES {
                            return Err(crate::Error::Locked);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(100));
                    }
                }
            } else {
                // Success
                break;
            }
        }

        Ok(Self(Arc::new(LockedFileGuardInner(file))))
    }
}

#[cfg(test)]
mod tests {
    use super::LockedFileGuard;
    use std::fs::File;

    #[test]
    fn create_new_acquires_lock_when_file_already_exists() -> crate::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lock");

        File::create(&path)?;

        let _guard = LockedFileGuard::create_new(&path)?;

        Ok(())
    }

    #[test]
    fn try_acquire_acquires_lock_on_existing_file() -> crate::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lock");

        File::create(&path)?;

        let _guard = LockedFileGuard::try_acquire(&path)?;

        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn the_flock_fallback_excludes_a_second_holder() -> crate::Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("lock");
        let (first, second) = (File::create(&path)?, File::open(&path)?);

        super::flock::try_lock(&first).map_err(|_| std::io::Error::other("first lock"))?;
        assert!(matches!(
            super::flock::try_lock(&second),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        super::flock::unlock(&first)?;
        super::flock::try_lock(&second).map_err(|_| std::io::Error::other("second lock"))?;

        Ok(())
    }
}
