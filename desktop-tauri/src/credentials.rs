//! OS keyring access exposed to the renderer.
//!
//! Uses the `keyring` crate which abstracts over libsecret (Linux),
//! Keychain (macOS), and the Windows Credential Manager. Same service id
//! as the Electron shell (`com.beam.desktop`) so credentials roam between
//! the two if a user installs both — they share the system keyring entry.

use anyhow::Result;
use keyring::Entry;

const SERVICE: &str = "com.beam.desktop";

fn entry(host: &str, user: &str) -> Result<Entry> {
    let key = format!("{host}|{user}");
    Ok(Entry::new(SERVICE, &key)?)
}

#[tauri::command]
pub async fn credentials_save(host: String, user: String, token: String) -> Result<(), String> {
    let e = entry(&host, &user).map_err(|e| e.to_string())?;
    e.set_password(&token).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn credentials_load(host: String, user: String) -> Result<Option<String>, String> {
    let e = entry(&host, &user).map_err(|e| e.to_string())?;
    match e.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
pub async fn credentials_clear(host: String, user: String) -> Result<(), String> {
    let e = entry(&host, &user).map_err(|e| e.to_string())?;
    match e.delete_credential() {
        Ok(()) => Ok(()),
        Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}
