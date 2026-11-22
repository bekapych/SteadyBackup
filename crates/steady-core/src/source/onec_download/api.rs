//! Сервис обновлений 1С (`update-api.1c.ru`): проверка обновлений платформы и конфигураций,
//! ссылки на файлы и скачивание с basic-авторизацией учётной записи ИТС.

use crate::job::DownloadProxy;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const API_URL: &str = "https://update-api.1c.ru/update-platform/programs";
const USER_AGENT: &str = "1C+Enterprise/8.3";
/// Ответ сервиса — JSON/HTML: в память читаем с ограничением, чтобы не уйти в гигабайты.
pub const TEXT_LIMIT: u64 = 16 * 1024 * 1024;

/// Ответ на проверку обновления платформы (`platformUpdateResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformUpdate {
    /// Версия, до которой предлагается обновиться.
    pub platform_version: Option<String>,
    /// Идентификатор дистрибутива для запроса ссылки (`update/`).
    pub distribution_uin: Option<String>,
    /// Размер файла в байтах (для журнала).
    pub size: Option<f64>,
}

/// Ответ на проверку обновления конфигурации (`configurationUpdateResponse`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfUpdate {
    /// Версия, до которой предлагается обновиться.
    pub configuration_version: Option<String>,
    /// Цепочка обновлений: идентификаторы звеньев по порядку. Сервис присылает `null`,
    /// когда обновления нет, поэтому здесь `Option`, а не `Vec` с одним лишь `default`.
    #[serde(default)]
    pub upgrade_sequence: Option<Vec<String>>,
    /// Идентификатор версии программы для запроса данных звена.
    pub program_version_uin: Option<String>,
}

impl ConfUpdate {
    /// Звенья цепочки; нет поля или `null` — пустая цепочка.
    pub fn sequence(&self) -> Vec<String> {
        self.upgrade_sequence.clone().unwrap_or_default()
    }
}

/// Данные одного звена цепочки (`configurationUpdateDataList[0]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfDownload {
    /// Путь шаблона на сервисе, например `1c\Accounting\3_0_52_32`.
    pub template_path: String,
    /// Ссылка на файл обновления. Может содержать авторизацию — в журнал не попадает.
    pub update_file_url: String,
    /// Размер файла в байтах.
    pub size: Option<f64>,
}

/// Разбор ответа проверки платформы. `None` — сервис не предложил обновление.
pub fn parse_platform_update(text: &str) -> Result<Option<PlatformUpdate>> {
    parse_field(text, "platformUpdateResponse")
}

/// Разбор ответа проверки конфигурации. `None` — сервис не предложил обновление.
pub fn parse_configuration_update(text: &str) -> Result<Option<ConfUpdate>> {
    parse_field(text, "configurationUpdateResponse")
}

/// Разбор данных звена цепочки: берётся первый элемент списка.
pub fn parse_conf_download(text: &str) -> Result<Option<ConfDownload>> {
    let value = parse_json(text)?;
    service_error(&value)?;
    let first = value
        .get("configurationUpdateDataList")
        .and_then(Value::as_array)
        .and_then(|list| list.first())
        .cloned();
    match first {
        Some(v) => Ok(Some(
            serde_json::from_value(v).context("неожиданный формат configurationUpdateDataList")?,
        )),
        None => Ok(None),
    }
}

fn parse_json(text: &str) -> Result<Value> {
    serde_json::from_str(text)
        .with_context(|| format!("ответ сервиса обновлений не JSON: {}", truncate(text, 300)))
}

/// Ошибка сервиса приходит с HTTP 200: её текст в `errorName`/`errorMessage`.
fn service_error(value: &Value) -> Result<()> {
    if let Some(name) = value.get("errorName").and_then(Value::as_str) {
        let message = value
            .get("errorMessage")
            .and_then(Value::as_str)
            .unwrap_or("");
        bail!("сервис обновлений 1С вернул ошибку {name}: {message}");
    }
    Ok(())
}

fn parse_field<T: for<'de> Deserialize<'de>>(text: &str, field: &str) -> Result<Option<T>> {
    let mut value = parse_json(text)?;
    service_error(&value)?;
    match value.get_mut(field).map(Value::take) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => Ok(Some(
            serde_json::from_value(v).with_context(|| format!("неожиданный формат {field}"))?,
        )),
    }
}

pub(crate) fn truncate(s: &str, max_chars: usize) -> &str {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// HTTP-агент сервисов 1С: общие TLS, прокси и тайм-ауты.
///
/// Cookies сохраняются между запросами агента (feature `cookies`) — на них держится вход
/// на портал releases.1c.ru. Коды состояния не превращаются в ошибки: тело ответа нужно
/// и у 4xx, чтобы показать причину. Только https: учётные данные ИТС идут по сети, и
/// downgrade на http недопустим. Заголовок `Authorization` при перенаправлении сохраняется
/// лишь для того же хоста.
pub fn build_agent(proxy: Option<&DownloadProxy>, insecure_tls: bool) -> Result<ureq::Agent> {
    // Корни берём из хранилища Windows: webpki-набор в native-tls не всегда содержит
    // корпоративные УЦ, а без него Windows сам строит цепочку через SChannel.
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::NativeTls)
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .disable_verification(insecure_tls)
        .build();
    let mut config = ureq::Agent::config_builder()
        .tls_config(tls)
        .user_agent(USER_AGENT)
        .http_status_as_error(false)
        .https_only(true)
        .redirect_auth_headers(ureq::config::RedirectAuthHeaders::SameHost)
        .max_redirects(10)
        // Соединение — 30 с, чтение порции данных — 300 с. Общего предела нет:
        // дистрибутив платформы может весить гигабайты, пока данные идут — это нормально.
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(300)))
        .timeout_recv_response(Some(Duration::from_secs(300)));
    // Пароль прокси расшифровывается внутри `url()` и percent-кодируется.
    let proxy_url = match proxy {
        Some(proxy) => proxy.url()?,
        None => None,
    };
    if let Some(url) = proxy_url {
        // В тексте ошибки URL прокси не показываем: он содержит пароль.
        config = config.proxy(Some(
            ureq::Proxy::new(&url).context("неверные настройки прокси")?,
        ));
    }
    Ok(config.build().into())
}

/// Адрес, по которому можно передавать учётные данные: только https.
pub fn require_https(url: &str, what: &str) -> Result<()> {
    if !url.trim().to_ascii_lowercase().starts_with("https://") {
        bail!("{what}: ссылка не https — учётные данные ИТС по ней не передаются");
    }
    Ok(())
}

/// Тело ответа как строка с ограничением размера (JSON сервиса, страница портала).
pub fn read_text(resp: &mut ureq::http::Response<ureq::Body>, what: &str) -> Result<String> {
    resp.body_mut()
        .with_config()
        .limit(TEXT_LIMIT)
        .read_to_string()
        .with_context(|| format!("{what}: чтение ответа"))
}

/// Скачивает тело подготовленного ответа в `dest` через `<dest>*.part` с проверкой отмены.
/// Файл появляется под итоговым именем только целиком; размер сверяется с `Content-Length`.
pub fn save_response(
    mut resp: ureq::http::Response<ureq::Body>,
    dest: &Path,
    what: &str,
) -> Result<u64> {
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        let text = read_text(&mut resp, what).unwrap_or_default();
        bail!("{what}: HTTP {status} {}", truncate(text.trim(), 300));
    }
    let total = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("создание {}", parent.display()))?;
    }
    let part = part_path(dest);
    let result = copy_body(&mut resp, &part, what);
    let done = match result {
        Ok(done) => done,
        Err(e) => {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
    };
    if let Some(total) = total {
        if done != total {
            let _ = std::fs::remove_file(&part);
            bail!("{what}: скачано {done} байт из {total}");
        }
    }
    std::fs::rename(&part, dest)
        .with_context(|| format!("переименование {} в {}", part.display(), dest.display()))?;
    Ok(done)
}

fn copy_body(
    resp: &mut ureq::http::Response<ureq::Body>,
    part: &Path,
    what: &str,
) -> Result<u64> {
    let mut reader = resp.body_mut().as_reader();
    let file = std::fs::File::create(part)
        .with_context(|| format!("создание {}", part.display()))?;
    let mut out = std::io::BufWriter::with_capacity(1 << 20, file);
    let mut buf = vec![0u8; 256 * 1024];
    let mut done: u64 = 0;
    loop {
        crate::process::check_cancel()?;
        let n = reader
            .read(&mut buf)
            .with_context(|| format!("{what}: обрыв при скачивании"))?;
        if n == 0 {
            break;
        }
        out.write_all(&buf[..n])
            .with_context(|| format!("{what}: запись {}", part.display()))?;
        done += n as u64;
    }
    out.flush()
        .with_context(|| format!("{what}: запись {}", part.display()))?;
    Ok(done)
}

/// Временный файл рядом с итоговым: чужие остатки не мешают, имя уникально.
fn part_path(dest: &Path) -> PathBuf {
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    dest.with_file_name(format!(
        ".{name}.{}.part",
        uuid::Uuid::new_v4().simple()
    ))
}

/// Клиент сервиса обновлений с учётной записью ИТС.
pub struct ApiClient {
    login: String,
    password: String,
    agent: ureq::Agent,
}

impl ApiClient {
    pub fn new(
        login: &str,
        password: &str,
        proxy: Option<&DownloadProxy>,
        insecure_tls: bool,
    ) -> Result<Self> {
        Ok(Self {
            login: login.to_string(),
            password: password.to_string(),
            agent: build_agent(proxy, insecure_tls)?,
        })
    }

    /// `POST` JSON → текстовое тело. Тело запроса содержит логин и пароль — в журнал не попадает.
    fn post(&self, url: &str, body: &Value) -> Result<String> {
        let mut resp = self
            .agent
            .post(url)
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| anyhow!("сервис обновлений 1С: {e}"))?;
        let status = resp.status().as_u16();
        let text = read_text(&mut resp, "сервис обновлений 1С")?;
        if !(200..300).contains(&status) {
            bail!(
                "сервис обновлений 1С: HTTP {status} {}",
                truncate(text.trim(), 300)
            );
        }
        Ok(text)
    }

    fn info_body(program: &str, version: &str, platform: &str, update_type: &str) -> Value {
        json!({
            "programName": program,
            "versionNumber": version,
            "platformVersion": platform,
            "updateType": update_type,
        })
    }

    /// Доступное обновление платформы; `None` — обновления нет.
    pub fn check_platform_update(&self, current: &str) -> Result<Option<PlatformUpdate>> {
        // Сервису нужна какая-нибудь конфигурация: как в исходной программе, ЗУП 3.1.
        let body = Self::info_body("HRM", "3.1", current, "NewPlatform");
        let text = self.post(&format!("{API_URL}/update/info"), &body)?;
        parse_platform_update(&text)
    }

    /// Доступное обновление конфигурации; `None` — обновления нет.
    pub fn check_conf_update(
        &self,
        program: &str,
        version: &str,
        platform: &str,
    ) -> Result<Option<ConfUpdate>> {
        let body = Self::info_body(program, version, platform, "NewProgramOrRedaction");
        let text = self.post(&format!("{API_URL}/update/info"), &body)?;
        parse_configuration_update(&text)
    }

    /// Прямая ссылка на дистрибутив платформы.
    pub fn platform_download_url(&self, distribution_uin: &str) -> Result<String> {
        let body = json!({
            "upgradeSequence": null,
            "programVersionUin": null,
            "platformDistributionUin": distribution_uin,
            "login": self.login,
            "password": self.password,
        });
        let text = self.post(&format!("{API_URL}/update/"), &body)?;
        let value = parse_json(&text)?;
        service_error(&value)?;
        value
            .get("platformDistributionUrl")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| anyhow!("в ответе сервиса нет platformDistributionUrl"))
    }

    /// Данные для скачивания одного звена цепочки обновлений конфигурации.
    pub fn conf_download(&self, uin: &str, program_uin: &str) -> Result<Option<ConfDownload>> {
        let body = json!({
            "upgradeSequence": [uin],
            "programVersionUin": program_uin,
            "platformDistributionUin": null,
            "login": self.login,
            "password": self.password,
        });
        let text = self.post(&format!("{API_URL}/update/"), &body)?;
        parse_conf_download(&text)
    }

    /// Скачивание файла с basic-авторизацией ИТС. `what` — описание для журнала и ошибок.
    pub fn download(&self, url: &str, dest: &Path, what: &str) -> Result<u64> {
        // До подстановки Authorization проверяем схему: по http пароль не уходит.
        require_https(url, what)?;
        let auth = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.login, self.password));
        let resp = self
            .agent
            .get(url)
            .header("Authorization", &format!("Basic {auth}"))
            .call()
            .map_err(|e| anyhow!("{what}: ошибка соединения при скачивании: {e}"))?;
        save_response(resp, dest, what)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn part_file_is_next_to_destination() {
        let dest = Path::new(r"C:\Upd\templates\1c\Accounting\3_0_52_32\1cv8.zip");
        let part = part_path(dest);
        assert_eq!(part.parent(), dest.parent());
        let name = part.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.starts_with(".1cv8.zip."), "{name}");
        assert!(name.ends_with(".part"), "{name}");
    }
}
