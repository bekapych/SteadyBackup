//! Безопасные пути загрузки обновлений: значения из ответа сервиса (`templatePath`)
//! и имена записей внутри zip не должны выходить за пределы целевого каталога.

use anyhow::{bail, Result};
use std::path::{Component, Path, PathBuf};

/// Относительный путь из `a\b/c` без `..`, без абсолютных компонентов и без `:` в имени.
/// Пустая строка даёт пустой путь (файл ляжет прямо в целевой каталог).
pub fn relative_path(name: &str) -> Result<PathBuf> {
    let name = name.trim();
    // Ведующий разделитель или буква диска сделали бы путь абсолютным.
    if name.starts_with(['/', '\\']) {
        bail!("недопустимый абсолютный путь в ответе сервиса или в архиве: {name}");
    }
    let mut out = PathBuf::new();
    for part in name.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        if part == ".." || part.contains(':') {
            bail!("недопустимый путь в ответе сервиса или в архиве: {name}");
        }
        if !matches!(Path::new(part).components().next(), Some(Component::Normal(_))) {
            bail!("недопустимый путь в ответе сервиса или в архиве: {name}");
        }
        out.push(part);
    }
    Ok(out)
}

/// Имя записи zip: байты без UTF-8 (так сохраняли архивы старых версий 1С) — это cp866.
pub fn decode_zip_name(raw: &[u8]) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::IBM866.decode(raw).0.into_owned(),
    }
}

/// Обязательный абсолютный каталог из задания: служба работает от SYSTEM, поэтому
/// относительный путь означал бы «неизвестно где» — его не угадываем.
pub fn absolute_dir(what: &str, value: &str) -> Result<PathBuf> {
    let value = value.trim();
    if value.is_empty() {
        bail!("для загрузки обновлений 1С обязателен {what}");
    }
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        bail!("{what} должен быть абсолютным путём, а не «{value}»");
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_dir_requires_absolute_path() {
        let ok = absolute_dir("template_dir", r" C:\Upd\templates ").unwrap();
        assert_eq!(ok, PathBuf::from(r"C:\Upd\templates"));
        assert!(absolute_dir("template_dir", "").is_err());
        assert!(absolute_dir("template_dir", "Upd\\templates").is_err());
    }
}
