//! Paired devices, the Windows counterpart of the Mac Keychain store. Each pairing secret is
//! sealed with DPAPI for the current Windows user before it touches disk.

use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::LocalFree;
use windows_sys::Win32::Security::Cryptography::{
    CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
};

#[derive(Serialize, Deserialize, Clone)]
struct Record {
    id: String,
    name: String,
    /// DPAPI blob, base64.
    secret: String,
}

pub struct Device {
    pub id: String,
    pub secret: [u8; 32],
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    base.join("Beacon").join("paired-devices.json")
}

fn records() -> Vec<Record> {
    std::fs::read(path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn load() -> Vec<Device> {
    records()
        .into_iter()
        .filter_map(|r| {
            let secret = B64.decode(&r.secret).ok().and_then(|b| dpapi(&b, false))?;
            Some(Device { id: r.id, secret: secret.try_into().ok()? })
        })
        .collect()
}

pub fn secret(device_id: &str) -> Option<[u8; 32]> {
    load().into_iter().find(|d| d.id == device_id).map(|d| d.secret)
}

pub fn name(device_id: &str) -> Option<String> {
    records().into_iter().find(|r| r.id == device_id).map(|r| r.name)
}

pub fn add(id: &str, name: &str, secret: &[u8; 32]) -> std::io::Result<()> {
    let sealed = dpapi(secret, true).ok_or_else(|| std::io::Error::other("DPAPI refused to seal the secret"))?;
    let mut all: Vec<Record> = records().into_iter().filter(|r| r.id != id).collect();
    all.push(Record { id: id.into(), name: name.into(), secret: B64.encode(sealed) });
    let p = path();
    std::fs::create_dir_all(p.parent().expect("has a parent"))?;
    std::fs::write(&p, serde_json::to_vec_pretty(&all)?)
}

pub fn forget_all() -> std::io::Result<()> {
    match std::fs::remove_file(path()) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

fn dpapi(data: &[u8], protect: bool) -> Option<Vec<u8>> {
    let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
    // SAFETY: input points at live memory for the call; output is allocated by DPAPI and freed below.
    unsafe {
        let ok = if protect {
            CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        } else {
            CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
        };
        if ok == 0 {
            return None;
        }
        let out = std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec();
        LocalFree(output.pbData as _);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn dpapi_round_trip() {
        let sealed = super::dpapi(&[5u8; 32], true).unwrap();
        assert_ne!(sealed, vec![5u8; 32]);
        assert_eq!(super::dpapi(&sealed, false).unwrap(), vec![5u8; 32]);
    }
}
