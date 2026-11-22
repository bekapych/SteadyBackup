//! Хранилища (куда сохранять) и подключения (учётные данные источников),
//! которые настраиваются один раз и выбираются в заданиях.
//!
//! ```text
//! <data>\storages\<id>.json
//! <data>\connections\<id>.json
//! ```
//! Секреты хранятся в DPAPI; наружу (GUI) отдаются замаскированными — "dpapi:stored".

use crate::job::{protect_opt, Destination, Job, Source};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const STORED: &str = "dpapi:stored";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Storage {
    /// Короткий идентификатор (латиница, цифры, - _), используется в заданиях.
    pub id: String,
    pub name: String,
    /// Параметры подключения; для `local/smb/sftp/web_dav` путь — корень хранилища.
    pub target: Destination,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ConnectionKind {
    MsSql {
        server: String,
        /// None — проверка подлинности Windows.
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    OneC {
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    WebDav {
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    /// Пользователь Firebird (обычно SYSDBA). Пароль передаётся gbak через окружение, не в командной строке.
    Firebird {
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    /// Сервер PostgreSQL. Пароль передаётся pg_dump через окружение (PGPASSWORD).
    #[serde(rename = "postgresql")]
    PostgreSql {
        /// Пусто — `localhost`.
        #[serde(default)]
        host: Option<String>,
        /// Нет — 5432.
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    /// Сервер SSH (источники «MySQL по SSH» и «Файлы по SSH»).
    Ssh {
        host: String,
        #[serde(default = "ssh_port")]
        port: u16,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        /// Закрытый ключ; пробуется раньше пароля.
        #[serde(default)]
        key_file: Option<String>,
        #[serde(default)]
        key_passphrase: Option<String>,
        /// Отпечаток ключа сервера "SHA256:...".
        #[serde(default)]
        host_key: Option<String>,
    },
}

fn ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Connection {
    pub id: String,
    pub name: String,
    #[serde(flatten)]
    pub kind: ConnectionKind,
}

impl Connection {
    pub fn type_name(&self) -> &'static str {
        match self.kind {
            ConnectionKind::MsSql { .. } => "ms_sql",
            ConnectionKind::OneC { .. } => "one_c",
            ConnectionKind::WebDav { .. } => "web_dav",
            ConnectionKind::Firebird { .. } => "firebird",
            ConnectionKind::PostgreSql { .. } => "postgresql",
            ConnectionKind::Ssh { .. } => "ssh",
        }
    }

    /// Все секреты подключения: пароль и, у SSH, пароль ключа.
    fn secrets_mut(&mut self) -> Vec<&mut Option<String>> {
        match &mut self.kind {
            ConnectionKind::Ssh { password, key_passphrase, .. } => vec![password, key_passphrase],
            ConnectionKind::MsSql { password, .. }
            | ConnectionKind::OneC { password, .. }
            | ConnectionKind::WebDav { password, .. }
            | ConnectionKind::Firebird { password, .. }
            | ConnectionKind::PostgreSql { password, .. } => vec![password],
        }
    }

    pub fn reveal_secrets(&mut self) -> Result<()> {
        for s in self.secrets_mut() {
            crate::job::reveal_opt(s)?;
        }
        Ok(())
    }

    pub fn user_password(&self) -> (Option<&str>, Option<&str>) {
        match &self.kind {
            ConnectionKind::MsSql { user, password, .. }
            | ConnectionKind::OneC { user, password }
            | ConnectionKind::WebDav { user, password }
            | ConnectionKind::Firebird { user, password }
            | ConnectionKind::PostgreSql { user, password, .. }
            | ConnectionKind::Ssh { user, password, .. } => (user.as_deref(), password.as_deref()),
        }
    }

    /// Параметры входа SSH (секреты как хранятся: `dpapi:...`).
    pub fn ssh_login(&self) -> Option<crate::job::SshLogin> {
        match &self.kind {
            ConnectionKind::Ssh { host, port, user, password, key_file, key_passphrase, host_key } => {
                Some(crate::job::SshLogin {
                    host: host.clone(),
                    port: *port,
                    user: user.clone().unwrap_or_default(),
                    password: password.clone(),
                    key_file: key_file.clone(),
                    key_passphrase: key_passphrase.clone(),
                    host_key: host_key.clone(),
                })
            }
            _ => None,
        }
    }
}

/// Проверка идентификатора: он попадает в имя файла и в задания.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Идентификатор из имени: «NAS офис» → "nas-ofis" (транслитерация кириллицы).
pub fn slug(name: &str) -> String {
    const TR: &[(&str, &str)] = &[
        ("а", "a"), ("б", "b"), ("в", "v"), ("г", "g"), ("д", "d"), ("е", "e"), ("ё", "e"), ("ж", "zh"),
        ("з", "z"), ("и", "i"), ("й", "y"), ("к", "k"), ("л", "l"), ("м", "m"), ("н", "n"), ("о", "o"),
        ("п", "p"), ("р", "r"), ("с", "s"), ("т", "t"), ("у", "u"), ("ф", "f"), ("х", "h"), ("ц", "c"),
        ("ч", "ch"), ("ш", "sh"), ("щ", "sch"), ("ъ", ""), ("ы", "y"), ("ь", ""), ("э", "e"), ("ю", "yu"),
        ("я", "ya"),
    ];
    let mut out = String::new();
    for ch in name.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if let Some((_, t)) = TR.iter().find(|(c, _)| c.chars().next() == Some(ch)) {
            out.push_str(t);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let s = out.trim_matches('-').to_string();
    if s.is_empty() { "storage".into() } else { s.chars().take(48).collect() }
}

pub struct Resources {
    dir: PathBuf,
}

impl Resources {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.to_path_buf() }
    }

    fn list<T: for<'de> Deserialize<'de>>(&self, sub: &str) -> Result<Vec<T>> {
        let dir = self.dir.join(sub);
        let mut v = Vec::new();
        let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(v) };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) != Some("json") {
                continue;
            }
            match std::fs::read(&p).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice(&b)?)) {
                Ok(x) => v.push(x),
                Err(err) => tracing::error!("{}: {err:#}", p.display()),
            }
        }
        Ok(v)
    }

    fn write<T: Serialize>(&self, sub: &str, id: &str, v: &T) -> Result<()> {
        if !valid_id(id) {
            bail!("недопустимый идентификатор «{id}»: только латиница, цифры, - и _");
        }
        let dir = self.dir.join(sub);
        std::fs::create_dir_all(&dir)?;
        let p = dir.join(format!("{id}.json"));
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(v)?)?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    // ---- хранилища ----

    pub fn storages(&self) -> Result<Vec<Storage>> {
        let mut v: Vec<Storage> = self.list("storages")?;
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(v)
    }

    pub fn storage(&self, id: &str) -> Result<Storage> {
        self.storages()?
            .into_iter()
            .find(|s| s.id == id)
            .with_context(|| format!("хранилище «{id}» не найдено"))
    }

    /// Сохраняет хранилище. Секреты "dpapi:stored" заменяются сохранёнными, открытые — шифруются.
    pub fn save_storage(&self, mut s: Storage) -> Result<Storage> {
        if s.name.trim().is_empty() {
            bail!("не задано имя хранилища");
        }
        if s.id.is_empty() {
            s.id = self.unique_id("storages", &slug(&s.name));
        }
        if matches!(s.target, Destination::Storage { .. }) {
            bail!("хранилище не может ссылаться на другое хранилище");
        }
        if let Ok(old) = self.storage(&s.id) {
            keep_stored_destination(&mut s.target, &old.target);
        }
        s.target.protect_secrets()?;
        self.write("storages", &s.id, &s)?;
        Ok(s)
    }

    pub fn delete_storage(&self, id: &str, jobs: &[Job]) -> Result<()> {
        let used: Vec<&str> = jobs
            .iter()
            .filter(|j| j.destinations.iter().any(|d| matches!(d, Destination::Storage { storage, .. } if storage == id)))
            .map(|j| j.name.as_str())
            .collect();
        if !used.is_empty() {
            bail!("хранилище используется в заданиях: {}", used.join(", "));
        }
        let p = self.dir.join("storages").join(format!("{id}.json"));
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    // ---- подключения ----

    pub fn connections(&self) -> Result<Vec<Connection>> {
        let mut v: Vec<Connection> = self.list("connections")?;
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(v)
    }

    pub fn connection(&self, id: &str) -> Result<Connection> {
        self.connections()?
            .into_iter()
            .find(|c| c.id == id)
            .with_context(|| format!("подключение «{id}» не найдено"))
    }

    pub fn save_connection(&self, mut c: Connection) -> Result<Connection> {
        if c.name.trim().is_empty() {
            bail!("не задано имя подключения");
        }
        if c.id.is_empty() {
            c.id = self.unique_id("connections", &slug(&c.name));
        }
        let mut old = self.connection(&c.id).ok();
        let mut old_secrets = old.as_mut().map(|o| o.secrets_mut()).unwrap_or_default();
        for (i, s) in c.secrets_mut().into_iter().enumerate() {
            if s.as_deref() == Some(STORED) {
                *s = old_secrets.get_mut(i).and_then(|o| o.take());
            }
            protect_opt(s)?;
        }
        self.write("connections", &c.id, &c)?;
        Ok(c)
    }

    pub fn delete_connection(&self, id: &str, jobs: &[Job]) -> Result<()> {
        let used: Vec<&str> = jobs.iter().filter(|j| source_connection(&j.source) == Some(id)).map(|j| j.name.as_str()).collect();
        if !used.is_empty() {
            bail!("подключение используется в заданиях: {}", used.join(", "));
        }
        let p = self.dir.join("connections").join(format!("{id}.json"));
        if p.exists() {
            std::fs::remove_file(p)?;
        }
        Ok(())
    }

    fn unique_id(&self, sub: &str, base: &str) -> String {
        let dir = self.dir.join(sub);
        if !dir.join(format!("{base}.json")).exists() {
            return base.to_string();
        }
        (2..).map(|i| format!("{base}-{i}")).find(|id| !dir.join(format!("{id}.json")).exists()).unwrap()
    }

    // ---- разрешение ссылок ----

    /// Превращает назначение задания в конкретное: ссылка на хранилище → параметры хранилища
    /// с подпапкой; устаревшая учётная запись Google Drive → токены из файла.
    pub fn resolve_destination(&self, d: &Destination) -> Result<Destination> {
        match d {
            Destination::Storage { storage, folder } => {
                let s = self.storage(storage)?;
                Ok(with_subfolder(s.target, folder))
            }
            Destination::GoogleDrive { account: Some(acc), folder, refresh_token: None, .. } => {
                let a = crate::destination::gdrive::TokenStore::new(&self.dir).load(acc)?;
                Ok(Destination::GoogleDrive {
                    account: None,
                    folder: folder.clone(),
                    client_id: Some(a.client_id),
                    client_secret: Some(a.client_secret),
                    refresh_token: Some(a.refresh_token),
                    email: a.email,
                })
            }
            other => Ok(other.clone()),
        }
    }

    /// Подставляет учётные данные подключения в источник.
    pub fn resolve_source(&self, s: &Source) -> Result<Source> {
        let mut s = s.clone();
        match &mut s {
            Source::Files { connection: Some(id), user, password, .. } => {
                let c = self.connection(id)?;
                let (u, p) = c.user_password();
                *user = u.map(String::from);
                *password = p.map(String::from);
            }
            Source::OneC { connection: Some(id), user, password, .. }
            | Source::Firebird { connection: Some(id), user, password, .. } => {
                let c = self.connection(id)?;
                let (u, p) = c.user_password();
                *user = u.map(String::from);
                *password = p.map(String::from);
            }
            Source::OneCUpdate { connection: Some(id), user, password, .. } => {
                let c = self.connection(id)?;
                let ConnectionKind::OneC { user: u, password: p } = c.kind else {
                    bail!("подключение «{id}» не является подключением к 1С");
                };
                *user = u;
                *password = p;
            }
            Source::MsSql { connection: Some(id), server, user, password, .. } => {
                let c = self.connection(id)?;
                let ConnectionKind::MsSql { server: srv, user: u, password: p } = c.kind else {
                    bail!("подключение «{id}» не является подключением к SQL Server");
                };
                *server = srv;
                *user = u;
                *password = p;
            }
            Source::PostgreSql { connection: Some(id), host, port, user, password, .. } => {
                let c = self.connection(id)?;
                let ConnectionKind::PostgreSql { host: h, port: pt, user: u, password: p } = c.kind else {
                    bail!("подключение «{id}» не является подключением к PostgreSQL");
                };
                *host = h;
                *port = pt;
                *user = u;
                *password = p;
            }
            Source::MySql { connection: Some(id), ssh, .. } | Source::SshFiles { connection: Some(id), ssh, .. } => {
                let c = self.connection(id)?;
                *ssh = Some(c.ssh_login().with_context(|| format!("подключение «{id}» не является подключением SSH"))?);
            }
            _ => {}
        }
        Ok(s)
    }

    /// Переводит назначения заданий со встроенными параметрами на хранилища.
    /// Одинаковые подключения (тип, сервер, логин) объединяются в одно хранилище.
    /// Возвращает (созданные хранилища, изменённые задания).
    pub fn migrate_jobs(&self, jobs: &mut [Job]) -> Result<(Vec<String>, Vec<String>)> {
        let mut created = Vec::new();
        let mut changed = Vec::new();
        for job in jobs.iter_mut() {
            let mut touched = false;
            for d in job.destinations.iter_mut() {
                if matches!(d, Destination::Storage { .. }) {
                    continue;
                }
                let resolved = self.resolve_destination(d)?;
                let (root, folder, name) = split_root(&resolved);
                let id = match self.storages()?.into_iter().find(|s| same_root(&s.target, &root)) {
                    Some(s) => s.id,
                    None => {
                        let s = self.save_storage(Storage { id: String::new(), name, target: root, note: None })?;
                        created.push(s.name.clone());
                        s.id
                    }
                };
                *d = Destination::Storage { storage: id, folder };
                touched = true;
            }
            if touched {
                changed.push(job.name.clone());
            }
        }
        Ok((created, changed))
    }

    /// Список хранилищ для GUI: секреты замаскированы.
    pub fn storages_masked(&self) -> Result<Vec<Storage>> {
        Ok(self.storages()?.into_iter().map(|mut s| {
            mask_destination(&mut s.target);
            s
        }).collect())
    }

    pub fn connections_masked(&self) -> Result<Vec<Connection>> {
        Ok(self.connections()?.into_iter().map(|mut c| {
            for s in c.secrets_mut() {
                mask(s);
            }
            c
        }).collect())
    }
}

pub fn source_connection(s: &Source) -> Option<&str> {
    match s {
        Source::Files { connection, .. }
        | Source::OneC { connection, .. }
        | Source::OneCUpdate { connection, .. }
        | Source::MsSql { connection, .. }
        | Source::Firebird { connection, .. }
        | Source::PostgreSql { connection, .. }
        | Source::MySql { connection, .. }
        | Source::SshFiles { connection, .. } => {
            connection.as_deref()
        }
        Source::HyperV { .. } => None,
    }
}

fn mask(v: &mut Option<String>) {
    if v.as_deref().is_some_and(|p| !p.is_empty()) {
        *v = Some(STORED.into());
    }
}

pub fn mask_destination(d: &mut Destination) {
    match d {
        Destination::Smb { password, .. } | Destination::WebDav { password, .. } => mask(password),
        Destination::Sftp { password, key_passphrase, .. } => {
            mask(password);
            mask(key_passphrase);
        }
        Destination::GoogleDrive { client_secret, refresh_token, .. } => {
            mask(client_secret);
            mask(refresh_token);
        }
        Destination::Local { .. } | Destination::Storage { .. } => {}
    }
}

/// Поля "dpapi:stored" из GUI заменяются значениями из сохранённой версии.
pub fn keep_stored_destination(new: &mut Destination, old: &Destination) {
    fn keep(n: &mut Option<String>, o: &Option<String>) {
        if n.as_deref() == Some(STORED) {
            *n = o.clone();
        }
    }
    match (new, old) {
        (Destination::Smb { password: n, .. }, Destination::Smb { password: o, .. })
        | (Destination::WebDav { password: n, .. }, Destination::WebDav { password: o, .. }) => keep(n, o),
        (
            Destination::Sftp { password: n, key_passphrase: nk, .. },
            Destination::Sftp { password: o, key_passphrase: ok, .. },
        ) => {
            keep(n, o);
            keep(nk, ok);
        }
        (
            Destination::GoogleDrive { client_secret: ns, refresh_token: nr, .. },
            Destination::GoogleDrive { client_secret: os, refresh_token: or, .. },
        ) => {
            keep(ns, os);
            keep(nr, or);
        }
        (n, _) => {
            // Тип сменился — замаскированные значения недействительны.
            if let Destination::Smb { password, .. } | Destination::WebDav { password, .. } = n {
                if password.as_deref() == Some(STORED) {
                    *password = None;
                }
            }
        }
    }
}

fn join_path(root: &str, sub: &str, sep: char) -> String {
    let sub = sub.trim_matches(['/', '\\']);
    if sub.is_empty() {
        return root.to_string();
    }
    let sub = sub.replace(['/', '\\'], &sep.to_string());
    format!("{}{sep}{sub}", root.trim_end_matches(['/', '\\']))
}

/// Корень хранилища + подпапка задания.
pub fn with_subfolder(mut d: Destination, folder: &str) -> Destination {
    match &mut d {
        Destination::Local { path } | Destination::Smb { path, .. } => *path = join_path(path, folder, '\\'),
        Destination::Sftp { path, .. } => *path = join_path(path, folder, '/'),
        Destination::WebDav { url, .. } => *url = join_path(url, folder, '/'),
        Destination::GoogleDrive { folder: f, .. } => *f = join_path(f, folder, '/'),
        Destination::Storage { .. } => {}
    }
    d
}

/// Разбивает конкретное назначение на корень хранилища, подпапку и предлагаемое имя.
/// Корень — первый уровень пути (сетевой ресурс, диск, первая папка), остальное — подпапка.
fn split_root(d: &Destination) -> (Destination, String, String) {
    fn split(path: &str, keep: usize, sep: char) -> (String, String) {
        let parts: Vec<&str> = path.split(['/', '\\']).filter(|s| !s.is_empty()).collect();
        let k = keep.min(parts.len());
        (parts[..k].join(&sep.to_string()), parts[k..].join("/"))
    }
    let mut root = d.clone();
    let (folder, name) = match &mut root {
        Destination::Local { path } => {
            let (r, f) = split(path, 2, '\\');
            *path = r.clone();
            (f, format!("Папка {r}"))
        }
        Destination::Smb { path, .. } => {
            let (r, f) = split(path, 2, '\\');
            *path = format!(r"\\{r}");
            (f, format!(r"Сеть \\{r}"))
        }
        Destination::Sftp { path, host, .. } => {
            let abs = path.starts_with('/');
            let (r, f) = split(path, 1, '/');
            *path = if abs { format!("/{r}") } else { r };
            (f, format!("SFTP {host}"))
        }
        Destination::WebDav { url, .. } => {
            let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or("webdav").to_string();
            (String::new(), format!("WebDAV {host}"))
        }
        Destination::GoogleDrive { folder, email, .. } => {
            let (r, f) = split(folder, 1, '/');
            *folder = r;
            (f, format!("Google Drive{}", email.as_ref().map(|e| format!(" {e}")).unwrap_or_default()))
        }
        Destination::Storage { .. } => (String::new(), String::new()),
    };
    (root, folder, name)
}

/// Одно и то же хранилище (для объединения при миграции).
fn same_root(a: &Destination, b: &Destination) -> bool {
    match (a, b) {
        (Destination::Local { path: x }, Destination::Local { path: y }) => x.eq_ignore_ascii_case(y),
        (Destination::Smb { path: x, user: u1, .. }, Destination::Smb { path: y, user: u2, .. }) => {
            x.eq_ignore_ascii_case(y) && u1 == u2
        }
        (
            Destination::Sftp { host: h1, port: p1, user: u1, path: x, .. },
            Destination::Sftp { host: h2, port: p2, user: u2, path: y, .. },
        ) => h1 == h2 && p1 == p2 && u1 == u2 && x == y,
        (Destination::WebDav { url: x, user: u1, .. }, Destination::WebDav { url: y, user: u2, .. }) => x == y && u1 == u2,
        (
            Destination::GoogleDrive { folder: f1, client_id: c1, refresh_token: r1, .. },
            Destination::GoogleDrive { folder: f2, client_id: c2, refresh_token: r2, .. },
        ) => f1 == f2 && c1 == c2 && r1 == r2,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("NAS офис"), "nas-ofis");
        assert_eq!(slug("Google Drive (бухгалтерия)"), "google-drive-buhgalteriya");
        assert_eq!(slug("!!!"), "storage");
        assert!(valid_id("nas-ofis_2"));
        assert!(!valid_id("../x"));
    }

    #[cfg(windows)]
    #[test]
    fn stored_marker_keeps_password() {
        let d = std::env::temp_dir().join(format!(
            "steady-res-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        let res = Resources::new(&d);
        let c = Connection {
            id: "fb".into(),
            name: "Firebird".into(),
            kind: ConnectionKind::Firebird {
                user: Some("SYSDBA".into()),
                password: Some(crate::secret::protect_str("masterkey").unwrap()),
            },
        };
        res.save_connection(c).unwrap();

        // Так подключение выглядит в ответе IPC: пароль заменён маркером.
        let mut masked = res.connections_masked().unwrap().remove(0);
        assert_eq!(masked.user_password().1, Some(STORED));
        masked.name = "Firebird 2".into();
        res.save_connection(masked).unwrap();

        let mut loaded = res.connections().unwrap().remove(0);
        loaded.reveal_secrets().unwrap();
        assert_eq!(loaded.user_password(), (Some("SYSDBA"), Some("masterkey")));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Оба секрета SSH маскируются для GUI и сохраняются по маркеру; источник получает вход из подключения.
    #[cfg(windows)]
    #[test]
    fn ssh_connection_keeps_both_secrets() {
        let d = std::env::temp_dir().join(format!("steady-res-ssh-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        let res = Resources::new(&d);
        let c: Connection = serde_json::from_str(
            r#"{"id":"web","name":"Сайт","type":"ssh","host":"example.ru","user":"deploy","password":"pw","key_passphrase":"kp"}"#,
        )
        .unwrap();
        res.save_connection(c).unwrap();
        let mut masked = res.connections_masked().unwrap().remove(0);
        let ConnectionKind::Ssh { password, key_passphrase, port, .. } = &masked.kind else { panic!("не SSH") };
        assert_eq!((password.as_deref(), key_passphrase.as_deref(), *port), (Some(STORED), Some(STORED), 22));
        masked.name = "Сайт 2".into();
        res.save_connection(masked).unwrap();

        let job: Source = serde_json::from_str(r#"{"type":"ssh_files","connection":"web","paths":["/var/www"]}"#).unwrap();
        let Source::SshFiles { ssh: Some(mut login), .. } = res.resolve_source(&job).unwrap() else { panic!("нет входа") };
        login.reveal_secrets().unwrap();
        assert_eq!((login.host.as_str(), login.user.as_str()), ("example.ru", "deploy"));
        assert_eq!((login.password.as_deref(), login.key_passphrase.as_deref()), (Some("pw"), Some("kp")));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn subfolders() {
        let d = with_subfolder(Destination::Smb { path: r"\\nas\backup\".into(), user: None, password: None }, "1C/PayDesk");
        assert!(matches!(d, Destination::Smb { ref path, .. } if path == r"\\nas\backup\1C\PayDesk"));
        let d = with_subfolder(Destination::Local { path: "E:\\Backup".into() }, "");
        assert!(matches!(d, Destination::Local { ref path } if path == "E:\\Backup"));
    }

    #[test]
    fn split() {
        let (r, f, _) = split_root(&Destination::Local { path: r"E:\SteadyBackupTest\PayDesk".into() });
        assert!(matches!(r, Destination::Local { ref path } if path == r"E:\SteadyBackupTest"));
        assert_eq!(f, "PayDesk");
        let (r, f, _) = split_root(&Destination::Smb { path: r"\\nas\backup\1c\buh".into(), user: None, password: None });
        assert!(matches!(r, Destination::Smb { ref path, .. } if path == r"\\nas\backup"));
        assert_eq!(f, "1c/buh");
    }
}
