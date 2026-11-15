//! Проверка и загрузка новых версий с GitHub (`releases/latest`).
//!
//! Доверие к установщику: файл скачивается по HTTPS, его размер и SHA-256 сверяются с данными
//! выпуска из API GitHub (поле `digest` вложения). Файл кладётся в `<data>\updates`, куда пишут
//! только SYSTEM и «Администраторы»: между проверкой и запуском msiexec его не подменить.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Репозиторий выпусков.
pub const REPO: &str = "bekapych/SteadyBackup";

/// Настройки обновления (`settings.json`, поле `update`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateSettings {
    /// Проверять раз в сутки, есть ли новая версия.
    #[serde(default = "yes")]
    pub check: bool,
    /// Найденную версию скачивать и устанавливать без участия пользователя — когда служба
    /// свободна (нет выполняющихся и ждущих заданий).
    #[serde(default)]
    pub auto_install: bool,
}

fn yes() -> bool {
    true
}

impl Default for UpdateSettings {
    fn default() -> Self {
        Self { check: true, auto_install: false }
    }
}

/// Выпуск с установщиком MSI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Release {
    /// "0.1.10" — без "v".
    pub version: String,
    /// Страница выпуска.
    pub url: String,
    /// Описание выпуска (Markdown).
    #[serde(default)]
    pub notes: String,
    pub published: Option<String>,
    pub msi_name: String,
    pub msi_url: String,
    pub size: u64,
    /// SHA-256 в нижнем регистре.
    pub sha256: String,
}

/// Числовые части версии: "v0.1.10" → [0, 1, 10]. Хвост вида "-beta" не допускается.
pub fn parse_version(s: &str) -> Option<Vec<u64>> {
    let s = s.trim().trim_start_matches(['v', 'V']);
    let v: Vec<u64> = s.split('.').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    (!v.is_empty()).then_some(v)
}

/// `candidate` новее `current`. Неразборчивая версия новой не считается.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_version(candidate), parse_version(current)) {
        (Some(mut a), Some(mut b)) => {
            let n = a.len().max(b.len());
            a.resize(n, 0);
            b.resize(n, 0);
            a > b
        }
        _ => false,
    }
}

/// Разбор ответа `GET /repos/{REPO}/releases/latest`.
pub fn parse_release(v: &serde_json::Value) -> Result<Release> {
    if v["draft"].as_bool() == Some(true) || v["prerelease"].as_bool() == Some(true) {
        bail!("последний выпуск — черновик или предварительный");
    }
    let tag = v["tag_name"].as_str().context("в выпуске нет tag_name")?;
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    if parse_version(&version).is_none() {
        bail!("непонятный номер версии выпуска «{tag}»");
    }
    let want = format!("SteadyBackup-{version}.msi");
    let asset = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|a| a["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&want)))
        .with_context(|| format!("в выпуске {tag} нет файла {want}"))?;
    let sha256 = asset["digest"]
        .as_str()
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(|d| d.to_ascii_lowercase())
        .filter(|d| d.len() == 64 && d.chars().all(|c| c.is_ascii_hexdigit()))
        .context("у установщика в выпуске нет контрольной суммы SHA-256 — установка без проверки запрещена")?;
    let msi_url = asset["browser_download_url"].as_str().context("нет ссылки на установщик")?.to_string();
    if !msi_url.starts_with(&format!("https://github.com/{REPO}/releases/download/")) {
        bail!("ссылка на установщик ведёт не на выпуски {REPO}: {msi_url}");
    }
    Ok(Release {
        version,
        url: v["html_url"].as_str().unwrap_or_default().to_string(),
        notes: v["body"].as_str().unwrap_or_default().to_string(),
        published: v["published_at"].as_str().map(String::from),
        msi_name: want,
        msi_url,
        size: asset["size"].as_u64().context("нет размера установщика")?,
        sha256,
    })
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .user_agent(concat!("SteadyBackup/", env!("CARGO_PKG_VERSION")))
        .build()
        .into()
}

/// Последний выпуск на GitHub.
pub fn fetch_latest() -> Result<Release> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let mut r = agent(Duration::from_secs(30))
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .call()
        .context("проверка обновлений: GitHub недоступен")?;
    let status = r.status().as_u16();
    let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
    match status {
        200 => parse_release(&v),
        404 => bail!("на GitHub ещё нет выпусков"),
        403 | 429 => bail!("GitHub временно ограничил запросы, проверка будет позже"),
        s => bail!("проверка обновлений: GitHub ответил {s}"),
    }
}

/// Скачивает установщик в `dir` и проверяет размер и SHA-256. Готовый проверенный файл
/// повторно не скачивается.
pub fn download(rel: &Release, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let dest = dir.join(&rel.msi_name);
    if dest.is_file() && file_sha256(&dest).ok().as_deref() == Some(rel.sha256.as_str()) {
        return Ok(dest);
    }
    let part = dir.join(format!("{}.partial", rel.msi_name));
    let mut r = agent(Duration::from_secs(30 * 60))
        .get(&rel.msi_url)
        .call()
        .context("загрузка обновления")?;
    if r.status().as_u16() != 200 {
        bail!("загрузка обновления: ответ {}", r.status());
    }
    let mut body = r.body_mut().with_config().limit(rel.size + 1).reader();
    let mut f = std::fs::File::create(&part).with_context(|| format!("создание {}", part.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut total = 0u64;
    loop {
        let n = body.read(&mut buf).context("загрузка обновления")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        f.write_all(&buf[..n])?;
        total += n as u64;
    }
    f.sync_all()?;
    drop(f);
    let got = hex(&hasher.finalize());
    if total != rel.size || got != rel.sha256 {
        let _ = std::fs::remove_file(&part);
        bail!("скачанный установщик не совпал с выпуском (размер {total} из {}, SHA-256 {got}) — не устанавливаю", rel.size);
    }
    let _ = std::fs::remove_file(&dest);
    std::fs::rename(&part, &dest).with_context(|| format!("запись {}", dest.display()))?;
    Ok(dest)
}

pub fn file_sha256(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex(&h.finalize()))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Удаляет скачанные установщики, кроме `keep`.
pub fn clean_downloads(dir: &Path, keep: Option<&Path>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_lowercase();
        if (name.ends_with(".msi") || name.ends_with(".msi.partial")) && Some(p.as_path()) != keep {
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Запись о начатой установке: после перезапуска служба сравнивает версии и сообщает итог.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub from: String,
    pub to: String,
    pub started: chrono::DateTime<chrono::Local>,
    pub log: PathBuf,
}

impl Pending {
    pub fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("updates").join("pending.json")
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        crate::util::write_atomic(&Self::path(data_dir), &serde_json::to_vec_pretty(self)?)
    }

    /// Читает и удаляет запись.
    pub fn take(data_dir: &Path) -> Option<Self> {
        let p = Self::path(data_dir);
        let v = std::fs::read(&p).ok().and_then(|b| serde_json::from_slice(&b).ok());
        let _ = std::fs::remove_file(&p);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn release_json(tag: &str, digest: Option<&str>) -> serde_json::Value {
        let ver = tag.trim_start_matches('v');
        let mut asset = json!({
            "name": format!("SteadyBackup-{ver}.msi"),
            "size": 42,
            "browser_download_url": format!("https://github.com/{REPO}/releases/download/{tag}/SteadyBackup-{ver}.msi"),
        });
        if let Some(d) = digest {
            asset["digest"] = json!(d);
        }
        json!({ "tag_name": tag, "draft": false, "prerelease": false, "html_url": "https://x", "body": "notes",
                "assets": [ { "name": "other.zip", "size": 1 }, asset ] })
    }

    const D: &str = "sha256:2A376F2313BD2A090B636F226D4168DDC0A092554F07BC1B825EE30A38B7BD7C";

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(is_newer("v0.2", "0.1.9"));
        assert!(!is_newer("0.1.9", "0.1.9"));
        assert!(!is_newer("0.1.9.0", "0.1.9"));
        assert!(!is_newer("0.1.8", "0.1.9"));
        assert!(!is_newer("1.0-beta", "0.1.9"));
    }

    #[test]
    fn release_is_parsed_with_digest() {
        let r = parse_release(&release_json("v0.1.10", Some(D))).unwrap();
        assert_eq!(r.version, "0.1.10");
        assert_eq!(r.msi_name, "SteadyBackup-0.1.10.msi");
        assert_eq!(r.sha256, D.trim_start_matches("sha256:").to_lowercase());
        assert_eq!(r.size, 42);
    }

    #[test]
    fn release_without_digest_or_from_elsewhere_is_rejected() {
        let e = parse_release(&release_json("v0.1.10", None)).unwrap_err().to_string();
        assert!(e.contains("SHA-256"), "{e}");
        let mut v = release_json("v0.1.10", Some(D));
        v["assets"][1]["browser_download_url"] = json!("https://evil.example/SteadyBackup-0.1.10.msi");
        assert!(parse_release(&v).is_err());
        let mut v = release_json("v0.1.10", Some(D));
        v["prerelease"] = json!(true);
        assert!(parse_release(&v).is_err());
    }

    #[test]
    fn old_settings_enable_check_only() {
        let s: crate::settings::ServiceSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.update, UpdateSettings { check: true, auto_install: false });
    }

    /// Сеть: настоящий выпуск с GitHub, загрузка и сверка суммы; испорченная сумма — отказ.
    /// `cargo test -p steady-core -- --ignored network_download`
    #[test]
    #[ignore]
    fn network_download_checks_sha256() {
        let d = std::env::temp_dir().join(format!("steady-upd-net-{}", uuid::Uuid::new_v4().simple()));
        let rel = fetch_latest().unwrap();
        let p = download(&rel, &d).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), rel.size);
        let mut bad = rel.clone();
        bad.sha256 = "0".repeat(64);
        bad.msi_name = "bad.msi".into();
        let e = download(&bad, &d).unwrap_err().to_string();
        assert!(e.contains("не совпал") && !d.join("bad.msi").exists(), "{e}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn clean_keeps_only_requested_file() {
        let d = std::env::temp_dir().join(format!("steady-upd-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        for n in ["SteadyBackup-0.1.9.msi", "SteadyBackup-0.2.0.msi", "x.msi.partial", "pending.json"] {
            std::fs::write(d.join(n), b"x").unwrap();
        }
        let keep = d.join("SteadyBackup-0.2.0.msi");
        clean_downloads(&d, Some(&keep));
        let mut left: Vec<String> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into()).collect();
        left.sort();
        assert_eq!(left, vec!["SteadyBackup-0.2.0.msi", "pending.json"]);
        let _ = std::fs::remove_dir_all(&d);
    }
}
