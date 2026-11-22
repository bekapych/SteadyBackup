//! Распаковка скачанного обновления конфигурации (`1cv8.zip`).

use super::paths::{decode_zip_name, relative_path};
use anyhow::{bail, Context, Result};
use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

/// Распаковывает zip в `dir` через временный подкаталог: сначала архив целиком разбирается
/// в `<dir>\.unpack-<uuid>`, затем файлы переносятся на место.
///
/// Так ошибка разбора архива не затрагивает рабочий каталог вовсе: временный каталог
/// удаляется, прежние файлы остаются как были. Ошибка уже во время переноса может оставить
/// часть файлов заменёнными — их пишет сам архив, и повторная распаковка их перезапишет.
/// Имена без UTF-8 (старые архивы 1С) декодируются как cp866, записи с выходом за пределы
/// каталога отклоняются.
pub fn unzip(zip_path: &Path, dir: &Path) -> Result<()> {
    let temp = dir.join(format!(".unpack-{}", uuid::Uuid::new_v4().simple()));
    let result = unpack_into(zip_path, &temp).and_then(|()| move_into_place(&temp, &temp, dir));
    // Временный каталог не остаётся ни при успехе, ни при ошибке.
    let _ = std::fs::remove_dir_all(&temp);
    result
}

fn unpack_into(zip_path: &Path, temp: &Path) -> Result<()> {
    std::fs::create_dir_all(temp)
        .with_context(|| format!("создание {}", temp.display()))?;
    let file = File::open(zip_path)
        .with_context(|| format!("открытие {}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(BufReader::new(file))
        .with_context(|| format!("{} — не zip-архив", zip_path.display()))?;
    let mut failed: Vec<String> = Vec::new();
    for i in 0..archive.len() {
        crate::process::check_cancel()?;
        let result: Result<()> = (|| {
            let mut entry = archive.by_index(i)?;
            let name = decode_zip_name(entry.name_raw());
            let out = temp.join(relative_path(&name)?);
            if entry.is_dir() {
                std::fs::create_dir_all(&out)
                    .with_context(|| format!("создание {}", out.display()))?;
                return Ok(());
            }
            if let Some(parent) = out.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("создание {}", parent.display()))?;
                }
            }
            let mut f = File::create(&out)
                .with_context(|| format!("создание {}", out.display()))?;
            io::copy(&mut entry, &mut f)
                .with_context(|| format!("запись {}", out.display()))?;
            Ok(())
        })();
        if let Err(e) = result {
            failed.push(format!("{e:#}"));
        }
    }
    if !failed.is_empty() {
        bail!(
            "не распаковано файлов: {} ({}): {}",
            failed.len(),
            zip_path.display(),
            failed.join("; ")
        );
    }
    Ok(())
}

/// Переносит содержимое `from` в `to`. Каталоги создаются, существующие файлы заменяются
/// целиком (rename в пределах одного тома). Обходит рекурсивно.
fn move_into_place(root: &Path, from: &Path, to: &Path) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("чтение {}", from.display()))? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| anyhow::anyhow!("{e:#}"))?
            .to_path_buf();
        let target: PathBuf = to.join(&relative);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&target)
                .with_context(|| format!("создание {}", target.display()))?;
            move_into_place(root, &path, to)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("создание {}", parent.display()))?;
        }
        std::fs::rename(&path, &target).with_context(|| {
            format!(
                "перенос {} в {}",
                path.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}
