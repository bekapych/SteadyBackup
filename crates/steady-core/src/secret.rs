//! Хранение паролей: значения вида `dpapi:<base64>` расшифровываются через Windows DPAPI
//! (область LocalMachine — чтобы служба под SYSTEM могла прочитать то, что ввёл администратор).
//! Обычная строка без префикса используется как есть.

use anyhow::{bail, Result};
use base64::Engine;

const PREFIX: &str = "dpapi:";
const ENTROPY: &[u8] = b"SteadyBackup/v1";

pub fn reveal(value: &str) -> Result<String> {
    match value.strip_prefix(PREFIX) {
        Some(b64) => {
            let data = base64::engine::general_purpose::STANDARD.decode(b64.trim())?;
            Ok(String::from_utf8(unprotect(&data)?)?)
        }
        None => Ok(value.to_string()),
    }
}

pub fn reveal_opt(value: Option<&str>) -> Result<Option<String>> {
    value.filter(|s| !s.is_empty()).map(reveal).transpose()
}

pub fn protect_str(plain: &str) -> Result<String> {
    let enc = protect(plain.as_bytes())?;
    Ok(format!("{PREFIX}{}", base64::engine::general_purpose::STANDARD.encode(enc)))
}

#[cfg(windows)]
fn protect(data: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::*;
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptProtectData(
            &input,
            std::ptr::null(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            bail!("DPAPI: шифрование не удалось: {}", std::io::Error::last_os_error());
        }
        Ok(take_blob(out))
    }
}

#[cfg(windows)]
fn unprotect(data: &[u8]) -> Result<Vec<u8>> {
    use windows_sys::Win32::Security::Cryptography::*;
    unsafe {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let entropy = CRYPT_INTEGER_BLOB { cbData: ENTROPY.len() as u32, pbData: ENTROPY.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        let ok = CryptUnprotectData(
            &input,
            std::ptr::null_mut(),
            &entropy,
            std::ptr::null(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out,
        );
        if ok == 0 {
            bail!(
                "DPAPI: не удалось расшифровать (зашифровано на другом компьютере?): {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(take_blob(out))
    }
}

#[cfg(windows)]
unsafe fn take_blob(b: windows_sys::Win32::Security::Cryptography::CRYPT_INTEGER_BLOB) -> Vec<u8> {
    let v = std::slice::from_raw_parts(b.pbData, b.cbData as usize).to_vec();
    windows_sys::Win32::Foundation::LocalFree(b.pbData as _);
    v
}

#[cfg(not(windows))]
fn protect(_: &[u8]) -> Result<Vec<u8>> {
    bail!("DPAPI доступен только в Windows")
}

#[cfg(not(windows))]
fn unprotect(_: &[u8]) -> Result<Vec<u8>> {
    bail!("DPAPI доступен только в Windows")
}

#[cfg(all(test, windows))]
mod tests {
    #[test]
    fn roundtrip() {
        let p = super::protect_str("Пароль 123").unwrap();
        assert!(p.starts_with("dpapi:"));
        assert_eq!(super::reveal(&p).unwrap(), "Пароль 123");
        assert_eq!(super::reveal("plain").unwrap(), "plain");
    }
}
