//! Портал releases.1c.ru: вход через login.1c.ru, список версий платформы и ссылки
//! на дистрибутивы.

use super::api::{read_text, require_https, save_response, truncate};
use anyhow::{anyhow, bail, Result};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;
use ureq::ResponseExt;

pub const LOGIN_URL: &str = "https://login.1c.ru/login";
pub const RELEASES_URL: &str = "https://releases.1c.ru";
const SECURITY_CHECK_URL: &str = "https://releases.1c.ru/public/security_check";

/// Версия из таблицы релизов проекта.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Лента портала, из которой взята версия (`Platform83`, `Platform85`).
    pub project: String,
    pub version: String,
    /// Дата выхода (год, месяц, день) — для сортировки.
    pub date: (u32, u32, u32),
}

impl Release {
    /// Дата для журнала: `14.11.2025`.
    pub fn date_text(&self) -> String {
        format!("{:02}.{:02}.{}", self.date.2, self.date.1, self.date.0)
    }

    /// Ключ сортировки: сначала дата выхода, затем номер версии.
    pub fn sort_key(&self) -> ((u32, u32, u32), Vec<u32>) {
        (
            self.date,
            self.version
                .split('.')
                .filter_map(|p| p.parse().ok())
                .collect(),
        )
    }
}

/// Дистрибутив на странице версии.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distribution {
    /// Название как на портале: «Тонкий клиент 1С:Предприятия для Windows».
    pub name: String,
    /// Страница файла со ссылками на скачивание.
    pub page_url: String,
    /// Имя файла, например `setuptc_8_5_1_1529.rar`.
    pub file_name: String,
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("регулярное выражение"))
}

/// Версии проекта из таблицы «Номер версии / Дата выхода» (`versionsTable`).
pub fn parse_releases(project: &str, page: &str) -> Result<Vec<Release>> {
    static ROW: OnceLock<Regex> = OnceLock::new();
    let start = page
        .find("id=\"versionsTable\"")
        .ok_or_else(|| anyhow!("на странице проекта {project} нет таблицы версий"))?;
    let rest = &page[start..];
    let table = &rest[..rest.find("</table>").unwrap_or(rest.len())];
    let row = re(
        &ROW,
        r#"ver=([0-9.]+)"[^>]*>\s*[0-9.]+\s*</a>\s*</td>\s*<td class="dateColumn">\s*(\d{2})\.(\d{2})\.(\d{2,4})"#,
    );
    let releases: Vec<Release> = row
        .captures_iter(table)
        .map(|c| {
            let year: u32 = c[4].parse().unwrap_or(0);
            Release {
                project: project.to_string(),
                version: c[1].to_string(),
                date: (
                    if year < 100 { 2000 + year } else { year },
                    c[3].parse().unwrap_or(0),
                    c[2].parse().unwrap_or(0),
                ),
            }
        })
        .collect();
    if releases.is_empty() {
        bail!("в таблице версий проекта {project} ничего не найдено (изменилась вёрстка портала?)");
    }
    Ok(releases)
}

/// Дистрибутивы со страницы версии (`version_files`).
pub fn parse_distributions(page: &str) -> Result<Vec<Distribution>> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    let link = re(&LINK, r#"<a href="(/version_file\?[^"]+)"[^>]*>([^<]+)</a>"#);
    let list: Vec<Distribution> = link
        .captures_iter(page)
        .filter_map(|c| {
            let href = html_unescape(&c[1]);
            let path = href.split('&').find_map(|kv| kv.strip_prefix("path="))?;
            let path = url_decode(path);
            let file_name = path.rsplit(['\\', '/']).next()?.to_string();
            if file_name.trim().is_empty() {
                return None;
            }
            Some(Distribution {
                name: normalize_name(&html_unescape(&c[2])),
                page_url: format!("{RELEASES_URL}{href}"),
                file_name,
            })
        })
        .collect();
    if list.is_empty() {
        bail!("на странице версии не найдено ни одного дистрибутива (изменилась вёрстка портала?)");
    }
    Ok(list)
}

/// Ссылки «Скачать дистрибутив» (основная и зеркала) со страницы файла.
pub fn parse_download_links(page: &str) -> Result<Vec<String>> {
    static HREF: OnceLock<Regex> = OnceLock::new();
    let start = page
        .find("class=\"downloadDist\"")
        .ok_or_else(|| anyhow!("на странице файла нет ссылок на скачивание"))?;
    let rest = &page[start..];
    let block = &rest[..rest.find("</table>").unwrap_or(rest.len())];
    let links: Vec<String> = re(&HREF, r#"<a href="(https?://[^"]+)""#)
        .captures_iter(block)
        .map(|c| html_unescape(&c[1]))
        .collect();
    if links.is_empty() {
        bail!("на странице файла нет ссылок на скачивание");
    }
    Ok(links)
}

/// Схлопывает пробелы и переводы строк в названии дистрибутива.
pub fn normalize_name(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(v) => {
                        out.push(v);
                        i += 3;
                        continue;
                    }
                    None => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Сеанс портала с cookies учётной записи ИТС.
pub struct Portal {
    agent: ureq::Agent,
}

impl Portal {
    /// Вход на портал. `agent` должен хранить cookies (feature `cookies`).
    pub fn login(agent: ureq::Agent, username: &str, password: &str) -> Result<Self> {
        static EXECUTION: OnceLock<Regex> = OnceLock::new();
        let url = format!("{LOGIN_URL}?service={}", url_encode(SECURITY_CHECK_URL));
        require_https(&url, "login.1c.ru")?;
        let page = {
            let mut resp = agent
                .get(&url)
                .call()
                .map_err(|e| anyhow!("не открывается login.1c.ru: {e}"))?;
            read_text(&mut resp, "login.1c.ru")?
        };
        let execution = re(&EXECUTION, r#"name="execution"\s+value="([^"]+)""#)
            .captures(&page)
            .map(|c| c[1].to_string())
            .ok_or_else(|| anyhow!("на странице входа login.1c.ru не найдена форма"))?;

        let resp = agent
            .post(&url)
            .send_form(vec![
                ("username", username),
                ("password", password),
                ("execution", execution.as_str()),
                ("_eventId", "submit"),
                ("inviteCode", ""),
            ])
            .map_err(|e| anyhow!("ошибка соединения с login.1c.ru: {e}"))?;
        let final_uri = resp.get_uri().to_string();
        if !final_uri.starts_with(RELEASES_URL) {
            bail!(
                "не удалось войти на releases.1c.ru — проверьте логин и пароль ИТС (остановились на {final_uri})"
            );
        }
        Ok(Self { agent })
    }

    fn get(&self, url: &str) -> Result<String> {
        require_https(url, "releases.1c.ru")?;
        let mut resp = self
            .agent
            .get(url)
            .call()
            .map_err(|e| anyhow!("ошибка соединения с порталом releases.1c.ru: {e}"))?;
        let status = resp.status().as_u16();
        let final_uri = resp.get_uri().to_string();
        if !final_uri.starts_with(RELEASES_URL) {
            bail!("портал перенаправил на вход — сессия ИТС не авторизована");
        }
        if !(200..300).contains(&status) {
            let text = read_text(&mut resp, "releases.1c.ru").unwrap_or_default();
            bail!(
                "releases.1c.ru: HTTP {status} {}",
                truncate(text.trim(), 300)
            );
        }
        read_text(&mut resp, "releases.1c.ru")
    }

    /// Выпущенные версии проекта.
    pub fn releases(&self, project: &str) -> Result<Vec<Release>> {
        let page = self.get(&format!("{RELEASES_URL}/project/{project}"))?;
        parse_releases(project, &page)
    }

    /// Дистрибутивы версии.
    pub fn distributions(&self, project: &str, version: &str) -> Result<Vec<Distribution>> {
        let page = self.get(&format!(
            "{RELEASES_URL}/version_files?nick={project}&ver={version}"
        ))?;
        parse_distributions(&page)
    }

    /// Ссылки на скачивание со страницы файла.
    pub fn download_links(&self, page_url: &str) -> Result<Vec<String>> {
        let page = self.get(page_url)?;
        parse_download_links(&page)
    }

    /// Скачивание по ссылке портала (cookies уже в агенте).
    pub fn download(&self, link: &str, dest: &Path, what: &str) -> Result<u64> {
        require_https(link, what)?;
        let resp = self
            .agent
            .get(link)
            .call()
            .map_err(|e| anyhow!("{what}: ошибка соединения при скачивании: {e}"))?;
        save_response(resp, dest, what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_normalized() {
        assert_eq!(normalize_name("  Тонкий\n клиент  "), "Тонкий клиент");
    }

    #[test]
    fn url_decoding() {
        assert_eq!(url_decode("Platform85%5C8_3_25%5Csetup.exe"), r"Platform85\8_3_25\setup.exe");
        assert_eq!(url_decode("a+b"), "a b");
    }
}
