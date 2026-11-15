//! Google Drive: OAuth 2.0 для установленных приложений (loopback + PKCE), resumable upload API v3.
//! Область доступа drive.file: программа видит только созданные ею файлы и папки.

use super::{parent_dirs, walk_files, Target};
use crate::secret;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3/files";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
/// Встроенный OAuth-клиент SteadyBackup (проект Google Cloud «steadybackup», тип Desktop app).
/// Для настольных приложений Google не считает client secret секретом: защиту входа обеспечивают
/// согласие пользователя и PKCE; секретом является только refresh-токен (хранится в DPAPI).
pub const BUILTIN_CLIENT_ID: &str = "854340264669-k30660ita57nhj74kto18c6fitk3s7f2.apps.googleusercontent.com";
///
/// Секрет в исходники не кладётся: его подставляет сборка из переменной окружения
/// `STEADY_GDRIVE_SECRET` или из файла `gdrive-secret.txt` в корне (он в `.gitignore`),
/// см. `build.rs`. Без него встроенный вход в Google Drive не работает — нужен свой OAuth-клиент.
pub const BUILTIN_CLIENT_SECRET: &str = env!("STEADY_GDRIVE_SECRET");

/// Кратно 256 КБ, как требует API.
const CHUNK: usize = 32 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
pub struct Account {
    pub client_id: String,
    /// Зашифровано DPAPI.
    pub client_secret: String,
    /// Зашифровано DPAPI.
    pub refresh_token: String,
    #[serde(default)]
    pub email: Option<String>,
}

pub struct TokenStore {
    dir: PathBuf,
}

impl TokenStore {
    pub fn new(data_dir: &Path) -> Self {
        Self { dir: data_dir.join("gdrive") }
    }

    fn path(&self, account: &str) -> PathBuf {
        self.dir.join(format!("{}.json", crate::pipeline::safe_name(account)))
    }

    pub fn load(&self, account: &str) -> Result<Account> {
        let p = self.path(account);
        let data = std::fs::read(&p)
            .with_context(|| format!("Google Drive: учётная запись «{account}» не найдена, выполните steady gdrive-auth"))?;
        Ok(serde_json::from_slice(&data)?)
    }

    pub fn save(&self, account: &str, a: &Account) -> Result<PathBuf> {
        let p = self.path(account);
        crate::util::write_atomic(&p, &serde_json::to_vec_pretty(a)?)?;
        Ok(p)
    }
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // 308 в resumable upload означает «часть принята», а не редирект.
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .build()
        .into()
}

fn read_json(resp: &mut ureq::http::Response<ureq::Body>, what: &str) -> Result<Value> {
    let status = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap_or_default();
    if !(200..300).contains(&status) {
        let msg = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| {
                v["error"]["message"].as_str().or(v["error_description"].as_str()).or(v["error"].as_str()).map(String::from)
            })
            .unwrap_or(text);
        bail!("Google Drive {what}: HTTP {status}: {msg}");
    }
    if text.is_empty() {
        return Ok(Value::Null);
    }
    Ok(serde_json::from_str(&text)?)
}

fn b64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn enc(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string()
}

/// Интерактивная авторизация (CLI): сохраняет учётную запись в `<data>\gdrive\<account>.json`.
pub fn authorize(store: &TokenStore, account: &str, client_id: &str, client_secret: &str) -> Result<PathBuf> {
    let flow = AuthFlow::start(client_id)?;
    println!("Откройте в браузере и разрешите доступ:\n\n{}\n", flow.url);
    #[cfg(windows)]
    let _ = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", &flow.url]).spawn();
    let acc = flow.finish(client_id, client_secret, Duration::from_secs(600))?;
    store.save(account, &acc)
}

/// Вход в Google в два шага: `start` даёт URL для браузера, `finish` ждёт перенаправления
/// на 127.0.0.1 и обменивает код на refresh-токен. Браузер открывает тот, кто запускает вход
/// (GUI на компьютере пользователя), поэтому вход возможен и для службы.
pub struct AuthFlow {
    pub url: String,
    listener: std::net::TcpListener,
    redirect: String,
    verifier: String,
    state: String,
}

/// Встроенный клиент собран без секрета (сборка из исходников без `gdrive-secret.txt`).
pub fn builtin_available() -> bool {
    !BUILTIN_CLIENT_SECRET.is_empty()
}

impl AuthFlow {
    pub fn start(client_id: &str) -> Result<Self> {
        if client_id == BUILTIN_CLIENT_ID && !builtin_available() {
            bail!(
                "эта сборка без встроенного OAuth-клиента Google: укажите в хранилище свой Client ID и Client Secret \
                 (Google Cloud Console, тип Desktop app)"
            );
        }
        use sha2::Digest;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let redirect = format!("http://127.0.0.1:{}/", listener.local_addr()?.port());
        let verifier = b64url(&rand::random::<[u8; 32]>());
        let challenge = b64url(&sha2::Sha256::digest(verifier.as_bytes()));
        let state = b64url(&rand::random::<[u8; 16]>());
        let url = format!(
            "{AUTH_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&code_challenge={challenge}&code_challenge_method=S256&state={state}&access_type=offline&prompt=consent",
            enc(client_id),
            enc(&redirect),
            enc(SCOPE),
        );
        Ok(Self { url, listener, redirect, verifier, state })
    }

    /// Возвращает учётную запись с зашифрованными (DPAPI) секретами.
    pub fn finish(self, client_id: &str, client_secret: &str, timeout: Duration) -> Result<Account> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + timeout;
        let code = loop {
            let mut stream = match self.listener.accept() {
                Ok((s, _)) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if Instant::now() > deadline {
                        bail!("Google: вход не завершён за отведённое время");
                    }
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                Err(e) => return Err(e.into()),
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(10)))?;
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let target = req.split_whitespace().nth(1).unwrap_or("/");
            let query = target.split_once('?').map(|x| x.1).unwrap_or("");
            let params: std::collections::HashMap<String, String> = query
                .split('&')
                .filter_map(|kv| kv.split_once('='))
                .map(|(k, v)| (k.to_string(), percent_encoding::percent_decode_str(v).decode_utf8_lossy().into_owned()))
                .collect();
            let reply = |s: &mut std::net::TcpStream, msg: &str| {
                let body = format!("<html><meta charset=utf-8><body style='font-family:sans-serif'><h2>{msg}</h2>Окно можно закрыть.</body></html>");
                let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            };
            if let Some(err) = params.get("error") {
                reply(&mut stream, "Доступ не предоставлен");
                bail!("Google: {err}");
            }
            if let Some(code) = params.get("code") {
                if params.get("state") != Some(&self.state) {
                    reply(&mut stream, "Ошибка: неверный state");
                    bail!("Google: неверный параметр state");
                }
                reply(&mut stream, "SteadyBackup: доступ к Google Drive получен");
                break code.clone();
            }
            let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        };

        let mut resp = agent().post(TOKEN_URL).send_form([
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", self.redirect.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ])?;
        let tok = read_json(&mut resp, "получение токена")?;
        let refresh = tok["refresh_token"].as_str().context("Google не вернул refresh_token")?;
        let mut acc = Account {
            client_id: client_id.to_string(),
            client_secret: secret::protect_str(client_secret)?,
            refresh_token: secret::protect_str(refresh)?,
            email: None,
        };
        if let Ok(info) = account_info(&acc) {
            acc.email = info.email;
        }
        Ok(acc)
    }
}

/// Адрес аккаунта и квота Диска.
#[derive(Debug, Default, Serialize)]
pub struct AccountInfo {
    pub email: Option<String>,
    pub total: Option<u64>,
    pub used: Option<u64>,
}

pub fn account_info(acc: &Account) -> Result<AccountInfo> {
    let mut s = Session::new(Account {
        client_id: acc.client_id.clone(),
        client_secret: acc.client_secret.clone(),
        refresh_token: acc.refresh_token.clone(),
        email: None,
    })?;
    s.about()
}

struct Session {
    agent: ureq::Agent,
    account: Account,
    token: String,
    expires: Instant,
}

impl Session {
    fn new(account: Account) -> Result<Self> {
        let mut s = Self { agent: agent(), account, token: String::new(), expires: Instant::now() };
        s.refresh()?;
        Ok(s)
    }

    fn refresh(&mut self) -> Result<()> {
        let secret = secret::reveal(&self.account.client_secret)?;
        let refresh = secret::reveal(&self.account.refresh_token)?;
        let mut resp = self.agent.post(TOKEN_URL).send_form([
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh.as_str()),
            ("client_id", self.account.client_id.as_str()),
            ("client_secret", secret.as_str()),
        ])?;
        let v = read_json(&mut resp, "обновление токена")
            .map_err(|e| anyhow!("{e:#}. Если доступ отозван — повторите steady gdrive-auth"))?;
        self.token = v["access_token"].as_str().context("нет access_token")?.to_string();
        let ttl = v["expires_in"].as_u64().unwrap_or(3600);
        self.expires = Instant::now() + Duration::from_secs(ttl.saturating_sub(120));
        Ok(())
    }

    fn bearer(&mut self) -> Result<String> {
        if Instant::now() >= self.expires {
            self.refresh()?;
        }
        Ok(format!("Bearer {}", self.token))
    }

    fn get(&mut self, url: &str, what: &str) -> Result<Value> {
        let auth = self.bearer()?;
        let mut r = self.agent.get(url).header("Authorization", &auth).call()?;
        read_json(&mut r, what)
    }

    /// Ищет дочерний объект по имени.
    fn find(&mut self, parent: &str, name: &str, folder: bool) -> Result<Option<String>> {
        let mut q = format!(
            "name = '{}' and '{}' in parents and trashed = false",
            name.replace('\\', "\\\\").replace('\'', "\\'"),
            parent
        );
        if folder {
            q.push_str(&format!(" and mimeType = '{FOLDER_MIME}'"));
        }
        let url = format!("{API}/files?q={}&fields=files(id)&pageSize=10&spaces=drive", enc(&q));
        let v = self.get(&url, "поиск")?;
        Ok(v["files"].get(0).and_then(|f| f["id"].as_str()).map(String::from))
    }

    fn about(&mut self) -> Result<AccountInfo> {
        let v = self.get(&format!("{API}/about?fields=user(emailAddress),storageQuota(limit,usage)"), "сведения об аккаунте")?;
        let num = |x: &Value| x.as_str().and_then(|s| s.parse().ok());
        Ok(AccountInfo {
            email: v["user"]["emailAddress"].as_str().map(String::from),
            total: num(&v["storageQuota"]["limit"]),
            used: num(&v["storageQuota"]["usage"]),
        })
    }

    fn create_folder(&mut self, parent: &str, name: &str) -> Result<String> {
        let auth = self.bearer()?;
        let mut r = self
            .agent
            .post(&format!("{API}/files?fields=id"))
            .header("Authorization", &auth)
            .send_json(json!({ "name": name, "mimeType": FOLDER_MIME, "parents": [parent] }))?;
        let v = read_json(&mut r, "создание папки")?;
        Ok(v["id"].as_str().context("нет id папки")?.to_string())
    }

    fn ensure_folder(&mut self, parent: &str, name: &str) -> Result<String> {
        match self.find(parent, name, true)? {
            Some(id) => Ok(id),
            None => self.create_folder(parent, name),
        }
    }

    fn rename(&mut self, id: &str, name: &str) -> Result<()> {
        let auth = self.bearer()?;
        let mut r = self
            .agent
            .patch(&format!("{API}/files/{id}"))
            .header("Authorization", &auth)
            .send_json(json!({ "name": name }))?;
        read_json(&mut r, "переименование")?;
        Ok(())
    }

    fn delete(&mut self, id: &str) -> Result<()> {
        let auth = self.bearer()?;
        let mut r = self.agent.delete(&format!("{API}/files/{id}")).header("Authorization", &auth).call()?;
        if r.status().as_u16() == 404 {
            return Ok(());
        }
        read_json(&mut r, "удаление")?;
        Ok(())
    }

    fn children(&mut self, parent: &str) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let q = format!("'{parent}' in parents and trashed = false");
            let mut url = format!("{API}/files?q={}&fields=nextPageToken,files(id,name)&pageSize=1000&spaces=drive", enc(&q));
            if let Some(p) = &page {
                url.push_str(&format!("&pageToken={}", enc(p)));
            }
            let v = self.get(&url, "список файлов")?;
            for f in v["files"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (f["id"].as_str(), f["name"].as_str()) {
                    out.push((id.to_string(), name.to_string()));
                }
            }
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => break,
            }
        }
        Ok(out)
    }

    /// Resumable upload с докачкой после сбоев.
    fn upload(&mut self, local: &Path, parent: &str, name: &str) -> Result<String> {
        let mut file = std::fs::File::open(local)?;
        let total = file.metadata()?.len();
        let auth = self.bearer()?;
        let mut r = self
            .agent
            .post(&format!("{UPLOAD}?uploadType=resumable&fields=id"))
            .header("Authorization", &auth)
            .header("X-Upload-Content-Type", "application/octet-stream")
            .header("X-Upload-Content-Length", &total.to_string())
            .send_json(json!({ "name": name, "parents": [parent] }))?;
        if !r.status().is_success() {
            read_json(&mut r, "начало загрузки")?;
        }
        let session = r
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .context("Google Drive: нет адреса сессии загрузки")?
            .to_string();

        if total == 0 {
            let auth = self.bearer()?;
            let mut r = self.agent.put(&session).header("Authorization", &auth).send(&[][..])?;
            let v = read_json(&mut r, "загрузка")?;
            return Ok(v["id"].as_str().unwrap_or_default().to_string());
        }

        let mut buf = vec![0u8; CHUNK];
        let mut offset: u64 = 0;
        let mut failures = 0;
        loop {
            use std::io::Seek;
            file.seek(std::io::SeekFrom::Start(offset))?;
            let want = CHUNK.min((total - offset) as usize);
            file.read_exact(&mut buf[..want])?;
            let end = offset + want as u64 - 1;
            let auth = self.bearer()?;
            let res = self
                .agent
                .put(&session)
                .header("Authorization", &auth)
                .header("Content-Range", &format!("bytes {offset}-{end}/{total}"))
                .send(&buf[..want]);
            match res {
                Ok(mut r) if r.status().is_success() => {
                    let v = read_json(&mut r, "загрузка")?;
                    return Ok(v["id"].as_str().unwrap_or_default().to_string());
                }
                Ok(r) if r.status().as_u16() == 308 => {
                    offset = range_end(&r).map(|e| e + 1).unwrap_or(0);
                    failures = 0;
                }
                Ok(mut r) if !matches!(r.status().as_u16(), 401 | 429 | 500..=599) => {
                    read_json(&mut r, "загрузка")?;
                    bail!("Google Drive: неожиданный ответ {}", r.status());
                }
                other => {
                    failures += 1;
                    let why = match other {
                        Ok(r) => format!("HTTP {}", r.status()),
                        Err(e) => e.to_string(),
                    };
                    if failures > 6 {
                        bail!("Google Drive: загрузка {name} прервана: {why}");
                    }
                    tracing::warn!("Google Drive: {why}, повтор через {}с", 2u64.pow(failures));
                    std::thread::sleep(Duration::from_secs(2u64.pow(failures)));
                    if why.contains("401") {
                        self.refresh()?;
                    }
                    offset = self.query_offset(&session, total)?;
                }
            }
        }
    }

    fn query_offset(&mut self, session: &str, total: u64) -> Result<u64> {
        let auth = self.bearer()?;
        let r = self
            .agent
            .put(session)
            .header("Authorization", &auth)
            .header("Content-Range", &format!("bytes */{total}"))
            .send(&[][..])?;
        Ok(if r.status().as_u16() == 308 { range_end(&r).map(|e| e + 1).unwrap_or(0) } else { 0 })
    }
}

fn range_end(r: &ureq::http::Response<ureq::Body>) -> Option<u64> {
    // "Range: bytes=0-12345"
    r.headers().get("range")?.to_str().ok()?.rsplit('-').next()?.parse().ok()
}

pub struct DriveTarget {
    s: Session,
    folder_id: String,
    label: String,
}

impl DriveTarget {
    pub fn open_account(account: Account, folder: &str) -> Result<Self> {
        let who = account.email.clone().unwrap_or_else(|| "google".into());
        let mut s = Session::new(account)?;
        let mut id = "root".to_string();
        for part in folder.split(['/', '\\']).filter(|p| !p.is_empty()) {
            id = s.ensure_folder(&id, part)?;
        }
        Ok(Self { s, folder_id: id, label: format!("gdrive://{who}/{}", folder.trim_matches('/')) })
    }
}

impl Target for DriveTarget {
    fn describe(&self) -> String {
        self.label.clone()
    }

    fn put(&mut self, local: &Path, name: &str) -> Result<()> {
        // Повторный запуск не должен оставлять дубликатов (на Диске имена не уникальны).
        while let Some(old) = self.s.find(&self.folder_id, name, false)? {
            self.s.delete(&old)?;
        }
        if local.is_dir() {
            let tmp_name = format!("{name}.partial");
            let tmp = self.s.create_folder(&self.folder_id, &tmp_name)?;
            let mut dirs = std::collections::HashMap::new();
            for (file, rel) in walk_files(local)? {
                let mut parent = tmp.clone();
                for d in parent_dirs(&rel) {
                    parent = match dirs.get(&d) {
                        Some(id) => String::clone(id),
                        None => {
                            let leaf = d.rsplit('/').next().unwrap();
                            let id = self.s.create_folder(&parent, leaf)?;
                            dirs.insert(d.clone(), id.clone());
                            id
                        }
                    };
                }
                let fname = rel.rsplit('/').next().unwrap();
                self.s.upload(&file, &parent, fname)?;
            }
            self.s.rename(&tmp, name)
        } else {
            // Файл появляется на Диске только после завершения resumable-загрузки.
            self.s.upload(local, &self.folder_id, name).map(|_| ())
        }
    }

    fn list(&mut self) -> Result<Vec<String>> {
        let id = self.folder_id.clone();
        Ok(self.s.children(&id)?.into_iter().map(|(_, n)| n).collect())
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        let id = self.folder_id.clone();
        for (fid, n) in self.s.children(&id)? {
            if n == name {
                self.s.delete(&fid)?;
            }
        }
        Ok(())
    }

    fn space(&mut self) -> Option<(u64, u64)> {
        let a = self.s.about().ok()?;
        let total = a.total?;
        Some((total, total.saturating_sub(a.used.unwrap_or(0))))
    }
}
