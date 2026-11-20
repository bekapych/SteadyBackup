//! Файлы с сервера по SSH (SFTP): полная, инкрементная, дифференциальная копия.
//! Каталог изменений, манифест и правила «удалён / не прочитан» — общие с источником `files`.

use super::files::{effective_kind, finish, root_label, Filter, Run};
use super::Prepared;
use crate::job::{BackupKind, FileDateField, FileDateFilter, FileDateMode, SshLogin};
use crate::ssh::SshSession;
use crate::state::{FileStamp, JobState};
use anyhow::{bail, Context, Result};
use russh_sftp::client::SftpSession;
use std::path::Path;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct SshFilesParams<'a> {
    pub ssh: &'a SshLogin,
    pub paths: &'a [String],
    pub include: &'a [String],
    pub exclude: &'a [String],
    pub date_filter: &'a FileDateFilter,
    pub requested: BackupKind,
    pub full_every: u32,
}

/// Файл на сервере, найденный обходом.
struct RemoteFile {
    /// Полный путь на сервере.
    path: String,
    /// Относительно корня источника, через '/'.
    rel: String,
    stamp: FileStamp,
}

#[derive(Default)]
struct Walk {
    files: Vec<RemoteFile>,
    links: u32,
    empty_dirs: u32,
    /// Относительные пути каталогов, которые не удалось прочитать ("" — сам корень).
    unreadable: Vec<String>,
}

fn stamp(md: &russh_sftp::protocol::FileAttributes) -> FileStamp {
    FileStamp { size: md.len(), mtime: md.mtime.unwrap_or(0) as i128 * 1_000_000_000 }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('/') { format!("{dir}{name}") } else { format!("{dir}/{name}") }
}

async fn walk(sftp: &SftpSession, root: &str) -> Result<Walk> {
    let md = sftp.metadata(root).await.with_context(|| format!("источник не найден на сервере: {root}"))?;
    let mut w = Walk::default();
    if !md.is_dir() {
        let name = root.rsplit('/').next().unwrap_or(root).to_string();
        w.files.push(RemoteFile { path: root.to_string(), rel: name, stamp: stamp(&md) });
        return Ok(w);
    }
    // Обход в ширину без рекурсии async: (полный путь, относительный).
    let mut queue = std::collections::VecDeque::from([(root.to_string(), String::new())]);
    while let Some((dir, rel_dir)) = queue.pop_front() {
        crate::process::check_cancel()?;
        let entries = match sftp.read_dir(dir.as_str()).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("SFTP: каталог {dir} не прочитан: {e}");
                w.unreadable.push(rel_dir);
                continue;
            }
        };
        let mut any = false;
        for e in entries {
            let name = e.file_name();
            if name == "." || name == ".." {
                continue;
            }
            any = true;
            let path = join(&dir, &name);
            let rel = if rel_dir.is_empty() { name.clone() } else { format!("{rel_dir}/{name}") };
            let t = e.file_type();
            if t.is_symlink() {
                w.links += 1;
            } else if t.is_dir() {
                queue.push_back((path, rel));
            } else if t.is_file() {
                w.files.push(RemoteFile { path, rel, stamp: stamp(&e.metadata()) });
            }
            // Сокеты, каналы, устройства не копируются.
        }
        if !any && !rel_dir.is_empty() {
            w.empty_dirs += 1;
        }
    }
    Ok(w)
}

async fn download(sftp: &SftpSession, remote: &str, local: &Path) -> Result<u64> {
    let mut src = sftp.open(remote).await.with_context(|| format!("SFTP: открытие {remote}"))?;
    let mut dst = tokio::fs::File::create(local).await.with_context(|| format!("создание {}", local.display()))?;
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = src.read(&mut buf).await.with_context(|| format!("SFTP: чтение {remote}"))?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await?;
        total += n as u64;
    }
    dst.flush().await?;
    Ok(total)
}

pub fn prepare(p: &SshFilesParams, state: &JobState, staging: &Path) -> Result<Prepared> {
    let paths: Vec<String> = p.paths.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    if paths.is_empty() {
        bail!("не указаны каталоги на сервере");
    }
    if p.date_filter.mode != FileDateMode::All && p.date_filter.field == FileDateField::Created {
        bail!("SFTP не передаёт дату создания файла: выберите фильтр по дате изменения");
    }
    let filter = Filter::new(p.include, p.exclude, p.date_filter, chrono::Local::now())?;
    let kind = effective_kind(p.requested, p.full_every, state);
    let mut run = Run::start(kind, state, staging);
    // Повтор всей подготовки (и подключения) — в конвейере (`Job::step_attempts`).
    let mut sess = SshSession::open(p.ssh, false)?;
    let host_warning = sess.host_key_warning();

    for (idx, root) in paths.iter().enumerate() {
        let label = root_label(idx, root);
        let w = {
            let (rt, sftp) = sess.sftp()?;
            rt.block_on(walk(sftp, root))?
        };
        run.skipped_links += w.links;
        run.empty_dirs += w.empty_dirs;
        for d in w.unreadable {
            run.unseen_prefixes.push(if d.is_empty() { label.clone() } else { format!("{label}/{d}") });
        }
        for f in w.files {
            if !filter.accepts(&f.rel) {
                continue;
            }
            crate::process::check_cancel()?;
            let key = format!("{label}/{}", f.rel);
            if !filter.accepts_date(&f.stamp, None).with_context(|| format!("фильтр даты SFTP: {key}"))? {
                run.date_filtered(key);
                continue;
            }
            if !run.changed(&key, &f.stamp) {
                run.record(key, f.stamp, None);
                continue;
            }
            crate::settings::ensure_free(&run.data_root, f.stamp.size)?;
            let dst = run.dst(&key)?;
            let res = {
                let (rt, sftp) = sess.sftp()?;
                rt.block_on(download(sftp, &f.path, &dst))
            };
            match res {
                Ok(n) => run.record(key, f.stamp, Some(n)),
                Err(e) => {
                    let _ = std::fs::remove_file(&dst);
                    // Обрыв соединения — ошибка всего запуска (с повтором подготовки),
                    // а не тысячи «непрочитанных» файлов.
                    if sess.is_closed() {
                        return Err(e.context(format!("SSH: соединение с {} разорвано", sess.label())));
                    }
                    run.fail(key, format!("{e:#}"));
                }
            }
        }
    }
    let mut prepared = finish(run, kind, state, staging, &paths)?;
    prepared.warnings.extend(host_warning);
    Ok(prepared)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_paths() {
        assert_eq!(join("/var/www", "a"), "/var/www/a");
        assert_eq!(join("/", "etc"), "/etc");
        assert_eq!(join("site", "a"), "site/a");
    }

    #[test]
    fn labels_for_remote_roots() {
        assert_eq!(root_label(0, "/var/www/site.ru/"), "0_site.ru");
        assert_eq!(root_label(1, "public_html"), "1_public_html");
    }

    #[test]
    fn job_json_defaults() {
        let j: crate::Job = serde_json::from_str(
            r#"{"id":"11111111-1111-1111-1111-111111111111","name":"s","source":{"type":"ssh_files","ssh":{"host":"h","user":"u"},"paths":["/var/www"]},"destinations":[]}"#,
        )
        .unwrap();
        match j.source {
            crate::Source::SshFiles { ssh: Some(s), kind, .. } => {
                assert_eq!((s.port, kind), (22, BackupKind::Full));
            }
            _ => panic!("ожидался источник SSH"),
        }
    }
}
