//! SFTP-назначение (чистый Rust: russh + russh-sftp) с выполнением команд по SSH.

use super::{parent_dirs, walk_files, Target};
use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::protocol::OpenFlags;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct SftpParams {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Option<String>,
    pub key_file: Option<String>,
    pub key_passphrase: Option<String>,
    pub host_key: Option<String>,
    pub path: String,
    pub post_command: Option<String>,
}

struct Handler {
    expected: Option<String>,
    host: String,
    seen: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

impl client::Handler for Handler {
    type Error = anyhow::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
            PublicKeyOrCertificate::Certificate(c) => c.public_key().fingerprint(HashAlg::Sha256).to_string(),
        };
        match &self.expected {
            Some(exp) if exp.trim() == fp => Ok(true),
            Some(exp) => bail!("SSH: ключ сервера {} не совпадает: ожидался {exp}, получен {fp}", self.host),
            None => {
                *self.seen.lock().unwrap() = Some(fp.clone());
                tracing::warn!("SSH: ключ сервера {} не проверяется; укажите host_key = \"{fp}\"", self.host);
                Ok(true)
            }
        }
    }
}

pub struct SftpTarget {
    rt: tokio::runtime::Runtime,
    session: client::Handle<Handler>,
    sftp: SftpSession,
    root: String,
    label: String,
    post_command: Option<String>,
    /// Отпечаток, принятый без проверки. Для отчёта и GUI.
    pub unpinned_host_key: Option<String>,
}

impl SftpTarget {
    pub fn connect(p: SftpParams) -> Result<Self> {
        let rt = crate::util::runtime()?;
        let label = format!("sftp://{}@{}:{}{}", p.user, p.host, p.port, p.path);
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_for_handler = seen.clone();
        let (session, sftp) = rt.block_on(async {
            let config = client::Config {
                inactivity_timeout: Some(Duration::from_secs(300)),
                keepalive_interval: Some(Duration::from_secs(30)),
                ..Default::default()
            };
            let handler = Handler { expected: p.host_key.clone(), host: p.host.clone(), seen: seen_for_handler };
            let mut session = tokio::time::timeout(
                Duration::from_secs(30),
                client::connect(Arc::new(config), (p.host.as_str(), p.port), handler),
            )
            .await
            .context("SSH: тайм-аут подключения")?
            .with_context(|| format!("SSH: подключение к {}:{}", p.host, p.port))?;

            let mut ok = false;
            if let Some(kf) = &p.key_file {
                let key = russh::keys::load_secret_key(kf, p.key_passphrase.as_deref())
                    .with_context(|| format!("SSH: чтение ключа {kf}"))?;
                let hash = session.best_supported_rsa_hash().await?.flatten();
                ok = session
                    .authenticate_publickey(&p.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await?
                    .success();
            }
            if !ok {
                if let Some(pw) = &p.password {
                    ok = session.authenticate_password(&p.user, pw).await?.success();
                }
            }
            if !ok {
                bail!("SSH: аутентификация пользователя {} не удалась", p.user);
            }
            let channel = session.channel_open_session().await?;
            channel.request_subsystem(true, "sftp").await?;
            let sftp = SftpSession::new(channel.into_stream()).await.context("SFTP: запуск подсистемы")?;
            Ok::<_, anyhow::Error>((session, sftp))
        })?;

        let mut root = p.path.trim_end_matches('/').to_string();
        if root.is_empty() {
            root = ".".into();
        }
        let unpinned_host_key = seen.lock().unwrap().clone();
        let mut t = Self { rt, session, sftp, root, label, post_command: p.post_command, unpinned_host_key };
        let root = t.root.clone();
        t.mkdirs(&root)?;
        Ok(t)
    }

    fn full(&self, rel: &str) -> String {
        format!("{}/{}", self.root, rel)
    }

    fn mkdirs(&mut self, path: &str) -> Result<()> {
        let sftp = &self.sftp;
        self.rt.block_on(async {
            let absolute = path.starts_with('/');
            let mut cur = String::new();
            for part in path.split('/').filter(|s| !s.is_empty() && *s != ".") {
                if absolute || !cur.is_empty() {
                    cur.push('/');
                }
                cur.push_str(part);
                if !sftp.try_exists(cur.as_str()).await.unwrap_or(false) {
                    sftp.create_dir(cur.as_str()).await.with_context(|| format!("SFTP: mkdir {cur}"))?;
                }
            }
            Ok(())
        })
    }

    fn upload(&mut self, local: &Path, remote: &str) -> Result<()> {
        let sftp = &self.sftp;
        self.rt.block_on(async {
            let mut src = tokio::fs::File::open(local).await?;
            let mut dst = sftp
                .open_with_flags(remote, OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE)
                .await
                .with_context(|| format!("SFTP: создание {remote}"))?;
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let n = src.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                dst.write_all(&buf[..n]).await.with_context(|| format!("SFTP: запись {remote}"))?;
            }
            dst.shutdown().await?;
            Ok::<(), anyhow::Error>(())
        })?;
        let want = std::fs::metadata(local)?.len();
        let got = self.remote_len(remote)?;
        super::expect_size(remote, want, got)
    }

    fn remote_len(&mut self, path: &str) -> Result<u64> {
        let sftp = &self.sftp;
        self.rt.block_on(async { Ok(sftp.metadata(path).await?.len()) })
    }

    fn remove_recursive(&mut self, path: &str) -> Result<()> {
        let sftp = &self.sftp;
        self.rt.block_on(async {
            let md = sftp.metadata(path).await?;
            if !md.is_dir() {
                sftp.remove_file(path).await?;
                return Ok(());
            }
            // Обход в глубину без рекурсии async.
            let mut stack = vec![(path.to_string(), false)];
            while let Some((dir, visited)) = stack.pop() {
                if visited {
                    sftp.remove_dir(dir.as_str()).await?;
                    continue;
                }
                stack.push((dir.clone(), true));
                for e in sftp.read_dir(dir.as_str()).await? {
                    let name = e.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let child = format!("{dir}/{name}");
                    if e.file_type().is_dir() {
                        stack.push((child, false));
                    } else {
                        sftp.remove_file(child.as_str()).await?;
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
    }

    fn exec(&mut self, command: &str) -> Result<()> {
        let session = &self.session;
        self.rt.block_on(async {
            let mut ch = session.channel_open_session().await?;
            ch.exec(true, command).await?;
            let mut out = Vec::new();
            let mut code = None;
            while let Some(msg) = ch.wait().await {
                match msg {
                    russh::ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    russh::ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data),
                    russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    _ => {}
                }
            }
            let text = String::from_utf8_lossy(&out);
            match code {
                Some(0) => {
                    tracing::info!("SSH: {command}: {}", text.trim());
                    Ok(())
                }
                c => bail!("SSH: команда «{command}» завершилась с кодом {c:?}: {}", text.trim()),
            }
        })
    }
}

impl Target for SftpTarget {
    fn describe(&self) -> String {
        self.label.clone()
    }

    fn warnings(&self) -> Vec<String> {
        self.unpinned_host_key
            .as_ref()
            .map(|fp| format!("SSH: ключ сервера не задан, принят {fp}. Сохраните его в поле «Отпечаток сервера»."))
            .into_iter()
            .collect()
    }

    fn host_key(&self) -> Option<String> {
        self.unpinned_host_key.clone()
    }

    fn put(&mut self, local: &Path, name: &str) -> Result<()> {
        // Повторы — в конвейере (`Job::upload_attempts`).
        self.put_once(local, name)
    }

    fn list(&mut self) -> Result<Vec<String>> {
        let sftp = &self.sftp;
        let root = self.root.clone();
        self.rt.block_on(async {
            Ok(sftp
                .read_dir(root.as_str())
                .await?
                .map(|e| e.file_name())
                .filter(|n| n != "." && n != "..")
                .collect())
        })
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        let p = self.full(name);
        self.remove_recursive(&p)
    }

    fn space(&mut self) -> Option<(u64, u64)> {
        let root = self.root.clone();
        let st = self.rt.block_on(self.sftp.fs_info(root.as_str())).ok()??;
        let bs = if st.fragment_size > 0 { st.fragment_size } else { st.block_size };
        Some((st.blocks * bs, st.blocks_avail * bs))
    }

    fn finish(&mut self) -> Result<()> {
        if let Some(cmd) = self.post_command.clone() {
            self.exec(&cmd)?;
        }
        Ok(())
    }
}

impl SftpTarget {
    fn put_once(&mut self, local: &Path, name: &str) -> Result<()> {
        let tmp = format!("{name}.partial");
        let tmp_full = self.full(&tmp);
        if self.rt.block_on(self.sftp.try_exists(tmp_full.as_str())).unwrap_or(false) {
            self.remove_recursive(&tmp_full)?;
        }
        if local.is_dir() {
            self.mkdirs(&tmp_full)?;
            for (file, rel) in walk_files(local)? {
                for d in parent_dirs(&rel) {
                    let dir = format!("{tmp_full}/{d}");
                    self.mkdirs(&dir)?;
                }
                let remote = format!("{tmp_full}/{rel}");
                self.upload(&file, &remote)?;
            }
        } else {
            self.upload(local, &tmp_full)?;
        }
        let dst = self.full(name);
        if self.rt.block_on(self.sftp.try_exists(dst.as_str())).unwrap_or(false) {
            self.remove_recursive(&dst)?;
        }
        let sftp = &self.sftp;
        self.rt
            .block_on(sftp.rename(tmp_full.as_str(), dst.as_str()))
            .with_context(|| format!("SFTP: переименование в {dst}"))?;
        Ok(())
    }
}

impl Drop for SftpTarget {
    fn drop(&mut self) {
        let _ = self.rt.block_on(async {
            let _ = self.sftp.close().await;
            self.session.disconnect(russh::Disconnect::ByApplication, "", "ru").await
        });
    }
}
