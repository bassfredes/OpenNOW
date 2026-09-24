//! Windows Credential Manager limits blobs to 2,560 bytes (and keyring passwords
//! are UTF-16). OAuth sessions can be larger. Store bounded, atomic DPAPI blobs
//! in the profile, protected by the current Windows user, never machine scope.
use super::{OsSecretStore, SERVICE_NAME, SecretStore};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows_sys::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};

const MAX_SECRET: usize = 4 * 1024 * 1024;
const MAX_ENCRYPTED: usize = MAX_SECRET + 4096;

pub(super) struct WindowsSecretStore {
    directory: PathBuf,
}

impl WindowsSecretStore {
    pub(super) fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn path(&self, user_id: &str) -> PathBuf {
        self.directory
            .join(format!("{:x}.dpapi", Sha256::digest(user_id.as_bytes())))
    }
}

impl SecretStore for WindowsSecretStore {
    fn get(&self, user_id: &str) -> Result<Option<String>, String> {
        let file = match fs::File::open(self.path(user_id)) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return OsSecretStore.get(user_id);
            }
            Err(_) => return Err("Windows protected session could not be opened".into()),
        };
        let mut encrypted = Vec::new();
        file.take(MAX_ENCRYPTED as u64 + 1)
            .read_to_end(&mut encrypted)
            .map_err(|_| "Windows protected session could not be read")?;
        if encrypted.is_empty() || encrypted.len() > MAX_ENCRYPTED {
            return Err("Windows protected session exceeds size limits".into());
        }
        // A present but invalid blob must fail closed, not resurrect an old grant.
        let clear = protect(&encrypted, user_id, false)?;
        if clear.len() > MAX_SECRET {
            return Err("Saved credential exceeds size limits".into());
        }
        String::from_utf8(clear)
            .map(Some)
            .map_err(|_| "Windows protected session is invalid".into())
    }

    fn set(&self, user_id: &str, encoded: &str) -> Result<(), String> {
        if user_id.trim().is_empty() || encoded.len() > MAX_SECRET {
            return Err("Saved credential exceeds size or identity limits".into());
        }
        let encrypted = protect(encoded.as_bytes(), user_id, true)?;
        if encrypted.len() > MAX_ENCRYPTED {
            return Err("Windows protected session exceeds size limits".into());
        }
        fs::create_dir_all(&self.directory)
            .map_err(|_| "Windows protected session directory is unavailable")?;
        let target = self.path(user_id);
        let temporary = target.with_extension(format!("{:016x}.tmp", rand::random::<u64>()));
        let mut created = false;
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| "Windows protected session could not be saved")?;
            created = true;
            file.write_all(&encrypted)
                .and_then(|_| file.sync_all())
                .map_err(|_| "Windows protected session could not be saved")?;
            drop(file);
            let source = wide(&temporary);
            let destination = wide(&target);
            if unsafe {
                MoveFileExW(
                    source.as_ptr(),
                    destination.as_ptr(),
                    MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
                )
            } == 0
            {
                return Err("Windows protected session could not be committed".into());
            }
            Ok(())
        })();
        if result.is_err() && created {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    fn delete(&self, user_id: &str) -> Result<(), String> {
        match fs::remove_file(self.path(user_id)) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err("Windows protected session could not be removed".into()),
        }
        OsSecretStore.delete(user_id)
    }
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn protect(bytes: &[u8], user_id: &str, encrypt: bool) -> Result<Vec<u8>, String> {
    let entropy_bytes = format!("{SERVICE_NAME}:session:{user_id}");
    let input = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr().cast_mut(),
    };
    let entropy = CRYPT_INTEGER_BLOB {
        cbData: entropy_bytes.len() as u32,
        pbData: entropy_bytes.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let success = unsafe {
        if encrypt {
            CryptProtectData(
                &input,
                std::ptr::null(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        } else {
            CryptUnprotectData(
                &input,
                std::ptr::null_mut(),
                &entropy,
                std::ptr::null(),
                std::ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output,
            )
        }
    };
    if success == 0 {
        return Err("Windows user-protected session encryption/decryption failed".into());
    }
    let result = if output.cbData == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() }
    };
    unsafe {
        // Avoid retaining a second decrypted copy in the OS allocation.
        if !encrypt {
            for offset in 0..output.cbData as usize {
                output.pbData.add(offset).write_volatile(0);
            }
        }
        LocalFree(output.pbData.cast());
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_oauth_session_survives_store_recreation_and_rotation() {
        let directory = tempfile::tempdir().unwrap();
        let user = format!("opennow-synthetic-{:016x}", rand::random::<u64>());
        let session = "synthetic-token-世界".repeat(1024);
        let store = WindowsSecretStore::new(directory.path().into());
        store.set(&user, &session).unwrap();
        let encrypted = fs::read(store.path(&user)).unwrap();
        assert!(!encrypted.windows(15).any(|part| part == b"synthetic-token"));
        drop(store);
        let restored = WindowsSecretStore::new(directory.path().into());
        assert_eq!(
            restored.get(&user).unwrap().as_deref(),
            Some(session.as_str())
        );
        let renewed = format!("renewed:{session}");
        restored.set(&user, &renewed).unwrap();
        assert_eq!(
            restored.get(&user).unwrap().as_deref(),
            Some(renewed.as_str())
        );
        restored.delete(&user).unwrap();
        assert_eq!(restored.get(&user).unwrap(), None);
    }

    #[test]
    fn tampering_and_account_substitution_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let store = WindowsSecretStore::new(directory.path().into());
        store.set("synthetic-a", "test-grant").unwrap();
        fs::copy(store.path("synthetic-a"), store.path("synthetic-b")).unwrap();
        assert!(store.get("synthetic-b").is_err());
        fs::write(store.path("synthetic-a"), b"invalid-blob").unwrap();
        assert!(store.get("synthetic-a").is_err());
    }

    #[test]
    fn failed_replace_retains_the_previous_encrypted_session() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = tempfile::tempdir().unwrap();
        let store = WindowsSecretStore::new(directory.path().into());
        store.set("synthetic-user", "old-grant").unwrap();
        let locked = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(store.path("synthetic-user"))
            .unwrap();
        assert!(store.set("synthetic-user", "replacement-grant").is_err());
        drop(locked);
        assert_eq!(
            fs::read_dir(directory.path()).unwrap().count(),
            1,
            "temporary ciphertext cleaned up"
        );
        assert!(
            store
                .set("synthetic-user", &"a".repeat(MAX_SECRET + 1))
                .is_err()
        );
        assert_eq!(
            store.get("synthetic-user").unwrap().as_deref(),
            Some("old-grant")
        );
    }
}
