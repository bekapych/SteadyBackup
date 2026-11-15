//! Сеанс SSH для источников «MySQL по SSH» и «Файлы по SSH»: вход, выполнение команд
//! с потоковым выводом, SFTP. Синхронный интерфейс поверх однопоточного tokio, как в
//! SFTP-назначении.

use crate::job::SshLogin;
use anyhow::{bail, Context, Result};
use russh::client;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use std::borrow::Cow;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct Handler {
    pub expected: Option<String>,
    pub host: String,
    pub seen: Arc<Mutex<Option<String>>>,
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
                if let Ok(mut s) = self.seen.lock() {
                    *s = Some(fp.clone());
                }
                tracing::warn!("SSH: ключ сервера {} не проверяется; укажите отпечаток \"{fp}\"", self.host);
                Ok(true)
            }
        }
    }
}

/// Параметры входа с уже расшифрованными секретами.
pub(crate) struct ConnectParams<'a> {
    pub host: &'a str,
    pub port: u16,
    pub user: &'a str,
    pub password: Option<&'a str>,
    pub key_file: Option<&'a str>,
    pub key_passphrase: Option<&'a str>,
    pub host_key: Option<&'a str>,
    /// Сжатие zlib, если сервер его поддерживает: дампы SQL и тексты сайтов сжимаются в разы.
    pub compress: bool,
}

/// Подключение и вход. Возвращает сеанс и отпечаток, принятый без проверки.
pub(crate) async fn connect(p: &ConnectParams<'_>) -> Result<(client::Handle<Handler>, Option<String>)> {
    if p.host.trim().is_empty() {
        bail!("SSH: не указан сервер");
    }
    if p.user.trim().is_empty() {
        bail!("SSH: не указан пользователь");
    }
    let mut config = client::Config {
        inactivity_timeout: Some(Duration::from_secs(300)),
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    };
    if p.compress {
        const ORDER: &[russh::compression::Name] =
            &[russh::compression::ZLIB_LEGACY, russh::compression::ZLIB, russh::compression::NONE];
        config.preferred = russh::Preferred { compression: Cow::Borrowed(ORDER), ..russh::Preferred::DEFAULT };
    }
    let seen = Arc::new(Mutex::new(None));
    let handler = Handler {
        expected: p.host_key.map(str::trim).filter(|s| !s.is_empty()).map(String::from),
        host: p.host.to_string(),
        seen: seen.clone(),
    };
    let host = p.host.trim();
    let mut session = tokio::time::timeout(Duration::from_secs(30), client::connect(Arc::new(config), (host, p.port), handler))
        .await
        .context("SSH: тайм-аут подключения")?
        .with_context(|| format!("SSH: подключение к {host}:{}", p.port))?;

    let mut ok = false;
    if let Some(kf) = p.key_file.filter(|s| !s.trim().is_empty()) {
        let key = russh::keys::load_secret_key(kf, p.key_passphrase.filter(|s| !s.is_empty()))
            .with_context(|| format!("SSH: чтение ключа {kf}"))?;
        let hash = session.best_supported_rsa_hash().await?.flatten();
        ok = session
            .authenticate_publickey(p.user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await?
            .success();
    }
    if !ok {
        if let Some(pw) = p.password.filter(|s| !s.is_empty()) {
            ok = session.authenticate_password(p.user, pw).await?.success();
        }
    }
    if !ok {
        bail!("SSH: вход пользователя {} на {host} не выполнен (неверный пароль или ключ)", p.user);
    }
    let unpinned = seen.lock().ok().and_then(|s| s.clone());
    Ok((session, unpinned))
}

/// Итог команды: код выхода и (обрезанный) поток ошибок.
pub struct ExecResult {
    pub code: Option<u32>,
    pub stderr: String,
}

pub struct SshSession {
    rt: tokio::runtime::Runtime,
    session: client::Handle<Handler>,
    sftp: Option<SftpSession>,
    label: String,
    /// Отпечаток, принятый без проверки.
    pub unpinned_host_key: Option<String>,
}

/// Предел текста ошибок команды, который держим в памяти.
const STDERR_LIMIT: usize = 64 * 1024;

impl SshSession {
    /// `login` — секреты в виде `dpapi:...` или открытым текстом.
    pub fn open(login: &SshLogin, compress: bool) -> Result<Self> {
        let password = crate::secret::reveal_opt(login.password.as_deref())?;
        let passphrase = crate::secret::reveal_opt(login.key_passphrase.as_deref())?;
        let p = ConnectParams {
            host: &login.host,
            port: if login.port == 0 { 22 } else { login.port },
            user: login.user.trim(),
            password: password.as_deref(),
            key_file: login.key_file.as_deref(),
            key_passphrase: passphrase.as_deref(),
            host_key: login.host_key.as_deref(),
            compress,
        };
        let rt = crate::util::runtime()?;
        let (session, unpinned_host_key) = rt.block_on(connect(&p))?;
        let label = format!("{}@{}:{}", p.user, login.host.trim(), p.port);
        Ok(Self { rt, session, sftp: None, label, unpinned_host_key })
    }

    /// Соединение разорвано: дальнейшие операции бессмысленны.
    pub fn is_closed(&self) -> bool {
        self.session.is_closed()
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Предупреждение для отчёта, если ключ сервера не закреплён.
    pub fn host_key_warning(&self) -> Option<String> {
        self.unpinned_host_key.as_ref().map(|fp| {
            format!("SSH {}: отпечаток сервера не задан, принят {fp}. Сохраните его в подключении.", self.label)
        })
    }

    /// Выполняет асинхронную операцию SFTP поверх того же соединения
    /// (подсистема открывается при первом вызове).
    /// Возвращает runtime и сеанс SFTP: `rt.block_on(операция(sftp))`.
    pub fn sftp(&mut self) -> Result<(&tokio::runtime::Runtime, &SftpSession)> {
        self.open_sftp()?;
        let sftp = self.sftp.as_ref().context("SFTP не открыт")?;
        Ok((&self.rt, sftp))
    }

    fn open_sftp(&mut self) -> Result<()> {
        if self.sftp.is_none() {
            let session = &self.session;
            let sftp = self.rt.block_on(async {
                let channel = session.channel_open_session().await?;
                channel.request_subsystem(true, "sftp").await?;
                let s = SftpSession::new(channel.into_stream()).await.context("SFTP: запуск подсистемы")?;
                // Большие каталоги и медленные диски хостинга: 10 с по умолчанию мало.
                s.set_timeout(120);
                Ok::<_, anyhow::Error>(s)
            })?;
            self.sftp = Some(sftp);
        }
        Ok(())
    }

    /// Выполняет команду, передавая её стандартный вывод в `out`. Прерывается отменой задания
    /// и по `timeout`. Ошибку записи в `out` возвращает как ошибку.
    /// `stdin` передаётся команде целиком, затем поток закрывается (так передаются пароли:
    /// не в командной строке, которую видят другие пользователи сервера).
    pub fn exec_to(&mut self, command: &str, stdin: &[u8], out: &mut dyn Write, timeout: Duration) -> Result<ExecResult> {
        let session = &self.session;
        self.rt.block_on(async {
            let mut ch = session.channel_open_session().await.context("SSH: открытие канала")?;
            ch.exec(true, command).await.context("SSH: запуск команды")?;
            if !stdin.is_empty() {
                ch.data(stdin).await.context("SSH: передача данных команде")?;
            }
            ch.eof().await.context("SSH: передача данных команде")?;
            let deadline = Instant::now() + timeout;
            let mut code = None;
            let mut err = Vec::new();
            loop {
                if Instant::now() >= deadline {
                    let _ = ch.close().await;
                    bail!("SSH: команда не завершилась за {}", crate::process::fmt_duration(timeout));
                }
                if let Err(e) = crate::process::check_cancel() {
                    let _ = ch.close().await;
                    return Err(e);
                }
                let msg = match tokio::time::timeout(Duration::from_millis(500), ch.wait()).await {
                    Err(_) => continue,
                    Ok(None) => break,
                    Ok(Some(m)) => m,
                };
                match msg {
                    russh::ChannelMsg::Data { data } => out.write_all(&data).context("запись вывода команды")?,
                    russh::ChannelMsg::ExtendedData { data, .. } => {
                        if err.len() < STDERR_LIMIT {
                            err.extend_from_slice(&data[..data.len().min(STDERR_LIMIT - err.len())]);
                        }
                    }
                    russh::ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
                    russh::ChannelMsg::ExitSignal { signal_name, .. } => {
                        bail!("SSH: команда прервана сигналом {signal_name:?}")
                    }
                    _ => {}
                }
            }
            Ok(ExecResult { code, stderr: String::from_utf8_lossy(&err).trim().to_string() })
        })
    }

    /// Команда с выводом в память (короткие ответы: список баз и т. п.).
    pub fn exec_capture(&mut self, command: &str, stdin: &[u8], timeout: Duration) -> Result<(ExecResult, Vec<u8>)> {
        let mut buf = Vec::new();
        let r = self.exec_to(command, stdin, &mut buf, timeout)?;
        Ok((r, buf))
    }
}

impl Drop for SshSession {
    fn drop(&mut self) {
        let _ = self.rt.block_on(async {
            if let Some(s) = &self.sftp {
                let _ = s.close().await;
            }
            self.session.disconnect(russh::Disconnect::ByApplication, "", "ru").await
        });
    }
}

/// Аргумент для POSIX-оболочки в одинарных кавычках.
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_quoting() {
        assert_eq!(sh_quote("site"), "'site'");
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        assert_eq!(sh_quote("$(rm -rf /)"), "'$(rm -rf /)'");
    }
}
