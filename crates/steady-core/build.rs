//! Секрет встроенного OAuth-клиента Google не хранится в репозитории. Берётся из переменной
//! окружения `STEADY_GDRIVE_SECRET` или из файла `gdrive-secret.txt` в корне рабочей области.
//! Если нет ни того, ни другого, сборка проходит, но встроенный вход в Google Drive выключен.

use std::path::PathBuf;

fn main() {
    let file = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default()).join("../../gdrive-secret.txt");
    println!("cargo:rerun-if-env-changed=STEADY_GDRIVE_SECRET");
    println!("cargo:rerun-if-changed={}", file.display());
    let secret = std::env::var("STEADY_GDRIVE_SECRET")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::fs::read_to_string(&file).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if secret.is_empty() {
        println!("cargo:warning=секрет OAuth-клиента Google не задан: встроенный вход в Google Drive выключен (STEADY_GDRIVE_SECRET или gdrive-secret.txt)");
    }
    println!("cargo:rustc-env=STEADY_GDRIVE_SECRET={secret}");
}
