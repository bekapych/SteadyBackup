//! Назначения резервных копий.

mod fs;
pub mod gdrive;
mod sftp;
pub mod webdav;

use crate::job::Destination;
use crate::secret::reveal_opt;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub use fs::FsTarget;

pub trait Target {
    fn describe(&self) -> String;
    /// Копирует локальный файл или каталог под именем `name` (атомарно: через временное имя).
    fn put(&mut self, local: &Path, name: &str) -> Result<()>;
    /// Имена объектов верхнего уровня.
    fn list(&mut self) -> Result<Vec<String>>;
    fn delete(&mut self, name: &str) -> Result<()>;
    /// Вызывается после успешной отправки всех файлов.
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }
    /// (всего, свободно) байт, если хранилище это сообщает.
    fn space(&mut self) -> Option<(u64, u64)> {
        None
    }
    /// Предупреждения подключения (отпечаток SSH не задан, пароль по http).
    fn warnings(&self) -> Vec<String> {
        Vec::new()
    }
    /// Отпечаток SSH, принятый без проверки. GUI предлагает сохранить его.
    fn host_key(&self) -> Option<String> {
        None
    }
}

/// Открывает назначение; ссылки на хранилища и старые учётные записи Google разрешаются
/// через раздел «Хранилища» в `data_dir`.
pub fn open(dest: &Destination, data_dir: &Path) -> Result<Box<dyn Target>> {
    let dest = crate::resources::Resources::new(data_dir).resolve_destination(dest)?;
    Ok(match &dest {
        Destination::Local { path } => Box::new(FsTarget::new(path)),
        Destination::Smb { path, user, password } => {
            if let Some(user) = user.as_deref().filter(|s| !s.is_empty()) {
                fs::smb_connect(path, user, reveal_opt(password.as_deref())?.as_deref().unwrap_or(""))?;
            }
            Box::new(FsTarget::new(path))
        }
        Destination::Sftp { host, port, user, password, key_file, key_passphrase, host_key, path, post_command } => {
            Box::new(sftp::SftpTarget::connect(sftp::SftpParams {
                host: host.clone(),
                port: *port,
                user: user.clone(),
                password: reveal_opt(password.as_deref())?,
                key_file: key_file.clone().filter(|s| !s.is_empty()),
                key_passphrase: reveal_opt(key_passphrase.as_deref())?,
                host_key: host_key.clone().filter(|s| !s.is_empty()),
                path: path.clone(),
                post_command: post_command.clone().filter(|s| !s.is_empty()),
            })?)
        }
        Destination::WebDav { url, user, password } => Box::new(webdav::WebDavTarget::new(
            url,
            user.as_deref(),
            reveal_opt(password.as_deref())?.as_deref(),
        )?),
        Destination::GoogleDrive { folder, client_id, client_secret, refresh_token, email, .. } => {
            // Без своего OAuth-клиента используется встроенный клиент SteadyBackup.
            let custom = client_id.as_deref().filter(|s| !s.is_empty());
            let (id, secret) = match custom {
                Some(id) => (
                    id.to_string(),
                    client_secret.clone().context("Google Drive: не задан Client Secret своего OAuth-клиента")?,
                ),
                None => (gdrive::BUILTIN_CLIENT_ID.to_string(), gdrive::BUILTIN_CLIENT_SECRET.to_string()),
            };
            let account = gdrive::Account {
                client_id: id,
                client_secret: secret,
                refresh_token: refresh_token.clone().context("Google Drive: вход в аккаунт не выполнен")?,
                email: email.clone(),
            };
            Box::new(gdrive::DriveTarget::open_account(account, folder)?)
        }
        Destination::Storage { .. } => unreachable!("разрешено выше"),
    })
}

pub(crate) fn expect_size(where_: &str, local: u64, remote: u64) -> Result<()> {
    if local != remote {
        bail!("{where_}: размер не совпал (локально {local}, на назначении {remote})");
    }
    Ok(())
}

/// Список файлов каталога (относительные пути с '/') — для рекурсивной отправки.
pub(crate) fn walk_files(dir: &Path) -> Result<Vec<(PathBuf, String)>> {
    let mut v = Vec::new();
    for e in walkdir::WalkDir::new(dir).sort_by_file_name() {
        let e = e?;
        if e.file_type().is_file() {
            let rel = e.path().strip_prefix(dir)?.to_string_lossy().replace('\\', "/");
            v.push((e.path().to_path_buf(), rel));
        }
    }
    Ok(v)
}

/// Родительские каталоги относительного пути: "a/b/c.txt" → ["a", "a/b"].
pub(crate) fn parent_dirs(rel: &str) -> Vec<String> {
    let parts: Vec<&str> = rel.split('/').collect();
    (1..parts.len()).map(|i| parts[..i].join("/")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_mismatch_is_an_error() {
        let err = expect_size("архив.7z", 10, 9).unwrap_err().to_string();
        assert!(err.contains("размер не совпал") && err.contains("архив.7z"), "{err}");
        assert!(expect_size("a", 10, 10).is_ok());
    }
}
