//! WebDAV-клиент: назначение и источник файлов.

use super::{parent_dirs, walk_files, Target};
use anyhow::{bail, Context, Result};
use base64::Engine;
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS};
use std::path::Path;
use std::time::Duration;
use ureq::http::{Request, Response};

/// Символы, кодируемые в сегменте пути.
const SEGMENT: &AsciiSet = &CONTROLS
    .add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?')
    .add(b'`').add(b'{').add(b'}').add(b'/').add(b'[').add(b']').add(b'^').add(b'|').add(b'\\');

pub struct DavEntry {
    /// Путь относительно базового URL, сегменты через '/'.
    pub rel: String,
    pub is_dir: bool,
    pub size: u64,
    /// mtime в наносекундах от UNIX-эпохи.
    pub mtime: i128,
}

pub struct WebDavClient {
    agent: ureq::Agent,
    /// "https://host:port"
    origin: String,
    /// Декодированный путь базового URL с завершающим '/'.
    base_path: String,
    auth: Option<String>,
}

impl WebDavClient {
    pub fn new(url: &str, user: Option<&str>, password: Option<&str>) -> Result<Self> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("WebDAV: URL должен начинаться с http:// или https://");
        }
        let scheme_end = url.find("://").unwrap() + 3;
        let (origin, path) = match url[scheme_end..].find('/') {
            Some(i) => (&url[..scheme_end + i], &url[scheme_end + i..]),
            None => (url, "/"),
        };
        let base_path = format!("{}/", decode(path).trim_end_matches('/'));
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .timeout_connect(Some(Duration::from_secs(30)))
            .build()
            .into();
        let auth = user.filter(|u| !u.is_empty()).map(|u| {
            let raw = format!("{u}:{}", password.unwrap_or(""));
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(raw))
        });
        Ok(Self { agent, origin: origin.to_string(), base_path, auth })
    }

    /// URL объекта; `rel` относительно базового каталога.
    pub fn url(&self, rel: &str) -> String {
        self.abs_url(&format!("{}{rel}", self.base_path))
    }

    fn abs_url(&self, path: &str) -> String {
        let enc: Vec<String> = path
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| utf8_percent_encode(s, SEGMENT).to_string())
            .collect();
        format!("{}/{}", self.origin, enc.join("/"))
    }

    pub fn base_url(&self) -> String {
        format!("{}/", self.url("").trim_end_matches('/'))
    }

    fn request(&self, method: &str, url: &str) -> ureq::http::request::Builder {
        let mut b = Request::builder().method(method).uri(url);
        if let Some(a) = &self.auth {
            b = b.header("Authorization", a);
        }
        b
    }

    fn check(&self, what: &str, resp: Response<ureq::Body>, ok: &[u16]) -> Result<Response<ureq::Body>> {
        let code = resp.status().as_u16();
        if (200..300).contains(&code) || ok.contains(&code) {
            return Ok(resp);
        }
        let mut resp = resp;
        let body = resp.body_mut().read_to_string().unwrap_or_default();
        let hint = match code {
            401 => " (неверный логин/пароль)",
            403 => " (нет прав)",
            404 | 409 => " (нет родительского каталога)",
            507 => " (на сервере закончилось место)",
            _ => "",
        };
        bail!("WebDAV {what}: HTTP {code}{hint} {}", body.chars().take(300).collect::<String>().trim())
    }

    pub fn mkcol(&self, rel: &str) -> Result<()> {
        let url = format!("{}/", self.url(rel).trim_end_matches('/'));
        let resp = self.agent.run(self.request("MKCOL", &url).body(())?)?;
        // 405 — каталог уже существует.
        self.check(&format!("MKCOL {url}"), resp, &[405])?;
        Ok(())
    }

    /// Создаёт базовый каталог и все промежуточные (если сервер их не имеет).
    pub fn ensure_base(&self) -> Result<()> {
        let base = self.base_url();
        let resp = self.agent.run(self.request("PROPFIND", &base).header("Depth", "0").body(())?)?;
        if resp.status().is_success() {
            return Ok(());
        }
        // Создаём путь от корня сервера; на промежуточных уровнях 403/405 допустимы (нет прав/уже есть).
        let segs: Vec<&str> = self.base_path.split('/').filter(|s| !s.is_empty()).collect();
        for i in 1..=segs.len() {
            let url = format!("{}/", self.abs_url(&segs[..i].join("/")));
            let resp = self.agent.run(self.request("MKCOL", &url).body(())?)?;
            let last = i == segs.len();
            self.check(&format!("MKCOL {url}"), resp, if last { &[405] } else { &[403, 405, 409] })?;
        }
        Ok(())
    }

    pub fn put_file(&self, local: &Path, rel: &str) -> Result<()> {
        let url = self.url(rel);
        let file = std::fs::File::open(local)?;
        let resp = self
            .agent
            .run(self.request("PUT", &url).header("Content-Type", "application/octet-stream").body(file)?)?;
        self.check(&format!("PUT {url}"), resp, &[])?;
        let want = std::fs::metadata(local)?.len();
        let got = self.file_len(rel)?;
        crate::destination::expect_size(&url, want, got)
    }

    /// Размер файла по PROPFIND Depth: 0.
    pub fn file_len(&self, rel: &str) -> Result<u64> {
        let url = self.url(rel);
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:getcontentlength/></d:prop></d:propfind>"#;
        let resp = self.agent.run(
            self.request("PROPFIND", &url)
                .header("Depth", "0")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body)?,
        )?;
        let mut resp = self.check(&format!("PROPFIND {url}"), resp, &[])?;
        let xml = resp.body_mut().read_to_string()?;
        let doc = roxmltree::Document::parse(&xml).context("WebDAV: разбор размера")?;
        doc.descendants()
            .find(|n| n.tag_name().name() == "getcontentlength")
            .and_then(|n| n.text())
            .and_then(|t| t.trim().parse().ok())
            .with_context(|| format!("WebDAV: сервер не сообщил размер {url}"))
    }

    pub fn get_file(&self, rel: &str, local: &Path) -> Result<u64> {
        let url = self.url(rel);
        let resp = self.agent.run(self.request("GET", &url).body(())?)?;
        let mut resp = self.check(&format!("GET {url}"), resp, &[])?;
        let mut out = std::fs::File::create(local)?;
        Ok(std::io::copy(&mut resp.body_mut().as_reader(), &mut out)?)
    }

    pub fn move_to(&self, from: &str, to: &str, is_dir: bool) -> Result<()> {
        let slash = if is_dir { "/" } else { "" };
        let src = format!("{}{slash}", self.url(from));
        let dst = format!("{}{slash}", self.url(to));
        let resp = self
            .agent
            .run(self.request("MOVE", &src).header("Destination", &dst).header("Overwrite", "T").body(())?)?;
        self.check(&format!("MOVE {src}"), resp, &[])?;
        Ok(())
    }

    pub fn delete(&self, rel: &str) -> Result<()> {
        let url = self.url(rel);
        let resp = self.agent.run(self.request("DELETE", &url).body(())?)?;
        self.check(&format!("DELETE {url}"), resp, &[404])?;
        Ok(())
    }

    /// PROPFIND Depth: 1 для каталога `rel` ("" — базовый).
    pub fn list_dir(&self, rel: &str) -> Result<Vec<DavEntry>> {
        let url = format!("{}/", self.url(rel).trim_end_matches('/'));
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;
        let resp = self.agent.run(
            self.request("PROPFIND", &url)
                .header("Depth", "1")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body)?,
        )?;
        let mut resp = self.check(&format!("PROPFIND {url}"), resp, &[])?;
        let xml = resp.body_mut().with_config().limit(64 * 1024 * 1024).read_to_string()?;
        let dir_path = format!("{}{}", self.base_path, rel.trim_matches('/'));
        let dir_path = if dir_path.starts_with('/') { dir_path } else { format!("/{dir_path}") };
        let dir_path = dir_path.trim_end_matches('/');
        parse_propfind(&xml, dir_path, rel)
    }

    /// RFC 4331: (всего, свободно), если сервер сообщает квоту.
    pub fn quota(&self) -> Result<Option<(u64, u64)>> {
        let body = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:quota-available-bytes/><d:quota-used-bytes/></d:prop></d:propfind>"#;
        let resp = self.agent.run(
            self.request("PROPFIND", &self.base_url())
                .header("Depth", "0")
                .header("Content-Type", "application/xml; charset=utf-8")
                .body(body)?,
        )?;
        let mut resp = self.check("PROPFIND quota", resp, &[])?;
        let xml = resp.body_mut().read_to_string()?;
        let doc = roxmltree::Document::parse(&xml)?;
        let num = |tag: &str| {
            doc.descendants()
                .find(|n| n.tag_name().name() == tag)
                .and_then(|n| n.text())
                .and_then(|t| t.trim().parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .map(|v| v as u64)
        };
        Ok(match (num("quota-available-bytes"), num("quota-used-bytes")) {
            (Some(a), Some(u)) => Some((a + u, a)),
            _ => None,
        })
    }

    /// Рекурсивный обход (только файлы).
    pub fn walk(&self) -> Result<Vec<DavEntry>> {
        let mut files = Vec::new();
        let mut stack = vec![String::new()];
        while let Some(dir) = stack.pop() {
            for e in self.list_dir(&dir)? {
                if e.is_dir {
                    stack.push(e.rel);
                } else {
                    files.push(e);
                }
            }
        }
        Ok(files)
    }
}

fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

fn parse_propfind(xml: &str, dir_path: &str, dir_rel: &str) -> Result<Vec<DavEntry>> {
    let doc = roxmltree::Document::parse(xml).context("WebDAV: разбор ответа PROPFIND")?;
    let mut out = Vec::new();
    for resp in doc.descendants().filter(|n| n.tag_name().name() == "response") {
        let Some(href) = resp.descendants().find(|n| n.tag_name().name() == "href").and_then(|n| n.text()) else {
            continue;
        };
        let mut path = decode(href.trim());
        // href может быть абсолютным URL.
        if let Some(i) = path.find("://") {
            path = path[i + 3..].find('/').map(|j| path[i + 3 + j..].to_string()).unwrap_or_else(|| "/".into());
        }
        let path = path.trim_end_matches('/');
        if path == dir_path {
            continue;
        }
        let Some(name) = path.rsplit('/').next().filter(|s| !s.is_empty()) else { continue };
        let is_dir = resp.descendants().any(|n| n.tag_name().name() == "collection");
        let text = |tag: &str| resp.descendants().find(|n| n.tag_name().name() == tag).and_then(|n| n.text());
        let size = text("getcontentlength").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let mtime = text("getlastmodified")
            .and_then(|s| chrono::DateTime::parse_from_rfc2822(s.trim()).ok())
            .and_then(|d| d.timestamp_nanos_opt())
            .map(|n| n as i128)
            .unwrap_or(0);
        let rel = if dir_rel.is_empty() { name.to_string() } else { format!("{}/{name}", dir_rel.trim_matches('/')) };
        out.push(DavEntry { rel, is_dir, size, mtime });
    }
    Ok(out)
}

pub struct WebDavTarget {
    client: WebDavClient,
    ready: bool,
}

impl WebDavTarget {
    pub fn new(url: &str, user: Option<&str>, password: Option<&str>) -> Result<Self> {
        Ok(Self { client: WebDavClient::new(url, user, password)?, ready: false })
    }

    fn prepare(&mut self) -> Result<()> {
        if !self.ready {
            self.client.ensure_base()?;
            self.ready = true;
        }
        Ok(())
    }
}

impl Target for WebDavTarget {
    fn describe(&self) -> String {
        self.client.base_url()
    }

    fn warnings(&self) -> Vec<String> {
        let url = self.client.base_url();
        if url.starts_with("http://") {
            vec!["WebDAV: пароль передаётся открытым текстом по http://. Используйте https://.".into()]
        } else {
            vec![]
        }
    }

    fn put(&mut self, local: &Path, name: &str) -> Result<()> {
        // Повторы — в конвейере (`Job::upload_attempts`).
        self.put_once(local, name)
    }

    fn list(&mut self) -> Result<Vec<String>> {
        self.prepare()?;
        Ok(self.client.list_dir("")?.into_iter().map(|e| e.rel).collect())
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        self.client.delete(name)
    }

    fn space(&mut self) -> Option<(u64, u64)> {
        self.client.quota().ok().flatten()
    }
}

impl WebDavTarget {
    fn put_once(&mut self, local: &Path, name: &str) -> Result<()> {
        self.prepare()?;
        let tmp = format!("{name}.partial");
        self.client.delete(&tmp)?;
        if local.is_dir() {
            self.client.mkcol(&tmp)?;
            let mut made = std::collections::HashSet::new();
            for (file, rel) in walk_files(local)? {
                for d in parent_dirs(&rel) {
                    if made.insert(d.clone()) {
                        self.client.mkcol(&format!("{tmp}/{d}"))?;
                    }
                }
                self.client.put_file(&file, &format!("{tmp}/{rel}"))?;
            }
            self.client.move_to(&tmp, name, true)
        } else {
            self.client.put_file(local, &tmp)?;
            self.client.move_to(&tmp, name, false)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn propfind() {
        let xml = r#"<?xml version="1.0"?>
<d:multistatus xmlns:d="DAV:">
 <d:response><d:href>/dav/backup/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
 <d:response><d:href>http://srv/dav/backup/%D0%9A%D0%BE%D0%BF%D0%B8%D1%8F%201.7z</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>42</d:getcontentlength><d:getlastmodified>Mon, 09 Nov 2026 12:00:00 GMT</d:getlastmodified><d:creationdate>2026-11-08T10:30:00Z</d:creationdate></d:prop></d:propstat></d:response>
 <d:response><d:href>/dav/backup/sub/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let v = parse_propfind(xml, "/dav/backup", "").unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].rel, "Копия 1.7z");
        assert_eq!(v[0].size, 42);
        assert!(v[0].mtime > 0);
        assert!(v[0].created.is_some_and(|t| t > 0));
        assert!(v[1].is_dir);
        assert_eq!(v[1].rel, "sub");
    }

    #[test]
    fn url_encoding() {
        let c = WebDavClient::new("https://h/dav/%D0%91%D1%8D%D0%BA%D0%B0%D0%BF", None, None).unwrap();
        assert_eq!(c.base_path, "/dav/Бэкап/");
        assert_eq!(c.url("a b/c#.7z"), "https://h/dav/%D0%91%D1%8D%D0%BA%D0%B0%D0%BF/a%20b/c%23.7z");
        let c = WebDavClient::new("http://h:8080", None, None).unwrap();
        assert_eq!(c.base_url(), "http://h:8080/");
        assert_eq!(c.url("x"), "http://h:8080/x");
    }
}
