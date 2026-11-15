//! Уведомления о результатах заданий: e-mail (SMTP) и Telegram.
//! Настройки — `<data>\notify.json`; пароль SMTP и токен бота хранятся в DPAPI.

use crate::secret::{protect_str, reveal};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum When {
    /// Каждый запуск.
    Always,
    /// Ошибки и предупреждения.
    #[default]
    Problems,
    /// Только ошибки.
    Errors,
    Never,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmtpSecurity {
    /// Сразу TLS (порт 465).
    Tls,
    /// STARTTLS (порт 587).
    #[default]
    StartTls,
    /// Без шифрования (только внутренние серверы).
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub when: When,
    pub server: String,
    #[serde(default = "smtp_port")]
    pub port: u16,
    #[serde(default)]
    pub security: SmtpSecurity,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    pub from: String,
    /// Адреса через запятую или точку с запятой.
    pub to: String,
}

fn smtp_port() -> u16 {
    587
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TelegramSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub when: When,
    pub bot_token: String,
    /// Один или несколько chat_id через запятую.
    pub chat_id: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotifySettings {
    #[serde(default)]
    pub computer_name: Option<String>,
    #[serde(default)]
    pub email: Option<EmailSettings>,
    #[serde(default)]
    pub telegram: Option<TelegramSettings>,
}

impl NotifySettings {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("notify.json")
    }

    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(Self::path(data_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Расшифровывает пароль SMTP и токен бота для переноса на другой компьютер.
    pub fn reveal_secrets(&mut self) -> Result<()> {
        if let Some(e) = self.email.as_mut() {
            crate::job::reveal_opt(&mut e.password).context("пароль SMTP")?;
        }
        if let Some(t) = self.telegram.as_mut() {
            if !t.bot_token.is_empty() && t.bot_token != "dpapi:stored" {
                t.bot_token = reveal(&t.bot_token).context("токен Telegram")?;
            }
        }
        Ok(())
    }

    pub fn save(&self, data_dir: &Path) -> Result<()> {
        let mut s = self.clone();
        if let Some(e) = s.email.as_mut() {
            if let Some(p) = e.password.as_mut() {
                if !p.is_empty() && !p.starts_with("dpapi:") {
                    *p = protect_str(p)?;
                }
            }
        }
        if let Some(t) = s.telegram.as_mut() {
            if !t.bot_token.is_empty() && !t.bot_token.starts_with("dpapi:") {
                t.bot_token = protect_str(&t.bot_token)?;
            }
        }
        crate::util::write_atomic(&Self::path(data_dir), &serde_json::to_vec_pretty(&s)?)
    }

    fn computer(&self) -> String {
        self.computer_name
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| std::env::var("COMPUTERNAME").ok())
            .unwrap_or_else(|| "?".into())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    Warning,
    Error,
}

/// Событие для уведомления.
pub struct Event {
    pub job: String,
    pub outcome: Outcome,
    pub started: chrono::DateTime<chrono::Local>,
    pub finished: chrono::DateTime<chrono::Local>,
    /// Строки с подробностями (имя набора, объём, назначения, ошибки).
    pub details: Vec<String>,
}

impl Event {
    fn status_text(&self) -> &'static str {
        match self.outcome {
            Outcome::Ok => "успешно",
            Outcome::Warning => "с предупреждениями",
            Outcome::Error => "ОШИБКА",
        }
    }

    fn icon(&self) -> &'static str {
        match self.outcome {
            Outcome::Ok => "✅",
            Outcome::Warning => "⚠️",
            Outcome::Error => "❌",
        }
    }

    fn duration(&self) -> String {
        let s = (self.finished - self.started).num_seconds().max(0);
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    }
}

fn wanted(when: When, o: Outcome) -> bool {
    match when {
        When::Always => true,
        When::Problems => o != Outcome::Ok,
        When::Errors => o == Outcome::Error,
        When::Never => false,
    }
}

/// Отправляет уведомление по всем включённым каналам. Ошибки каналов возвращаются списком,
/// чтобы сбой почты не мешал Telegram и наоборот.
pub fn send(settings: &NotifySettings, ev: &Event) -> Vec<String> {
    let mut errors = Vec::new();
    if let Some(e) = settings.email.as_ref().filter(|e| e.enabled && wanted(e.when, ev.outcome)) {
        if let Err(err) = send_email(e, &settings.computer(), ev) {
            errors.push(format!("e-mail: {err:#}"));
        }
    }
    if let Some(t) = settings.telegram.as_ref().filter(|t| t.enabled && wanted(t.when, ev.outcome)) {
        if let Err(err) = send_telegram(t, &settings.computer(), ev) {
            errors.push(format!("Telegram: {err:#}"));
        }
    }
    errors
}

/// Тестовое сообщение по одному каналу ("email" | "telegram") независимо от условия `when`.
pub fn send_test(settings: &NotifySettings, channel: &str) -> Result<()> {
    let now = chrono::Local::now();
    let ev = Event {
        job: "Проверка уведомлений".into(),
        outcome: Outcome::Ok,
        started: now,
        finished: now,
        details: vec!["Это тестовое сообщение SteadyBackup.".into()],
    };
    match channel {
        "email" => send_email(settings.email.as_ref().context("e-mail не настроен")?, &settings.computer(), &ev),
        "telegram" => {
            send_telegram(settings.telegram.as_ref().context("Telegram не настроен")?, &settings.computer(), &ev)
        }
        _ => bail!("неизвестный канал {channel}"),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn send_email(s: &EmailSettings, computer: &str, ev: &Event) -> Result<()> {
    use lettre::message::{header::ContentType, Mailbox, MultiPart, SinglePart};
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{SmtpTransport, Transport};

    let from: Mailbox = s.from.trim().parse().with_context(|| format!("адрес отправителя «{}»", s.from))?;
    let mut builder = lettre::Message::builder()
        .from(from)
        .subject(format!("[SteadyBackup] {} — {} ({computer})", ev.job, ev.status_text()));
    let mut any = false;
    for addr in s.to.split([',', ';']).map(str::trim).filter(|a| !a.is_empty()) {
        builder = builder.to(addr.parse().with_context(|| format!("адрес получателя «{addr}»"))?);
        any = true;
    }
    if !any {
        bail!("не указаны получатели");
    }

    let plain = format!(
        "Задание: {}\nРезультат: {}\nКомпьютер: {computer}\nНачало: {}\nДлительность: {}\n\n{}\n",
        ev.job,
        ev.status_text(),
        ev.started.format("%d.%m.%Y %H:%M:%S"),
        ev.duration(),
        ev.details.join("\n")
    );
    let color = match ev.outcome {
        Outcome::Ok => "#2e7d32",
        Outcome::Warning => "#ef6c00",
        Outcome::Error => "#c62828",
    };
    let rows: String = ev.details.iter().map(|d| format!("<li>{}</li>", html_escape(d))).collect();
    let html = format!(
        "<div style='font-family:Segoe UI,Arial,sans-serif;font-size:14px'>\
         <h2 style='color:{color};margin:0 0 8px'>{} {}</h2>\
         <table style='border-collapse:collapse'>\
         <tr><td style='color:#666;padding-right:12px'>Задание</td><td><b>{}</b></td></tr>\
         <tr><td style='color:#666;padding-right:12px'>Компьютер</td><td>{}</td></tr>\
         <tr><td style='color:#666;padding-right:12px'>Начало</td><td>{}</td></tr>\
         <tr><td style='color:#666;padding-right:12px'>Длительность</td><td>{}</td></tr></table>\
         <ul>{rows}</ul><p style='color:#999;font-size:12px'>SteadyBackup</p></div>",
        ev.icon(),
        ev.status_text(),
        html_escape(&ev.job),
        html_escape(computer),
        ev.started.format("%d.%m.%Y %H:%M:%S"),
        ev.duration(),
    );
    let msg = builder.multipart(
        MultiPart::alternative()
            .singlepart(SinglePart::builder().header(ContentType::TEXT_PLAIN).body(plain))
            .singlepart(SinglePart::builder().header(ContentType::TEXT_HTML).body(html)),
    )?;

    let mut t = match s.security {
        SmtpSecurity::Tls => SmtpTransport::relay(&s.server)?,
        SmtpSecurity::StartTls => SmtpTransport::starttls_relay(&s.server)?,
        SmtpSecurity::None => SmtpTransport::builder_dangerous(&s.server),
    }
    .port(s.port)
    .timeout(Some(Duration::from_secs(30)));
    if let Some(user) = s.user.as_deref().filter(|u| !u.is_empty()) {
        let pw = s.password.as_deref().map(reveal).transpose()?.unwrap_or_default();
        t = t.credentials(Credentials::new(user.to_string(), pw));
    }
    t.build().send(&msg).with_context(|| format!("SMTP {}:{}", s.server, s.port))?;
    Ok(())
}

fn send_telegram(s: &TelegramSettings, computer: &str, ev: &Event) -> Result<()> {
    let token = reveal(&s.bot_token)?;
    if token.is_empty() {
        bail!("не указан токен бота");
    }
    let mut text = format!(
        "{} <b>{}</b> — {}\n🖥 {}\n🕒 {} · {}",
        ev.icon(),
        html_escape(&ev.job),
        ev.status_text(),
        html_escape(computer),
        ev.started.format("%d.%m.%Y %H:%M"),
        ev.duration()
    );
    for d in &ev.details {
        text.push_str("\n• ");
        text.push_str(&html_escape(d));
    }
    // Лимит Telegram — 4096 символов.
    if text.chars().count() > 4000 {
        text = text.chars().take(4000).collect::<String>() + "…";
    }
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    for chat in s.chat_id.split([',', ';']).map(str::trim).filter(|c| !c.is_empty()) {
        let mut r = agent.post(&url).send_json(serde_json::json!({
            "chat_id": chat,
            "text": text,
            "parse_mode": "HTML",
            "disable_web_page_preview": true,
        }))?;
        let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
        if v["ok"].as_bool() != Some(true) {
            let d = v["description"].as_str().unwrap_or("неизвестная ошибка");
            let hint = if d.contains("Unauthorized") {
                " (неверный токен бота)"
            } else if d.contains("chat not found") {
                " (напишите боту /start или добавьте его в группу)"
            } else {
                ""
            };
            bail!("chat {chat}: {d}{hint}");
        }
    }
    Ok(())
}

/// Последние чаты, писавшие боту, — чтобы пользователю не искать chat_id вручную.
pub fn telegram_recent_chats(bot_token: &str) -> Result<Vec<(String, String)>> {
    let token = reveal(bot_token)?;
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut r = agent.get(&format!("https://api.telegram.org/bot{token}/getUpdates")).call()?;
    let v: serde_json::Value = r.body_mut().read_json().unwrap_or_default();
    if v["ok"].as_bool() != Some(true) {
        let d = v["description"].as_str().unwrap_or("ошибка Telegram");
        bail!("{d}{}", if d.contains("Unauthorized") { " (неверный токен бота)" } else { "" });
    }
    let mut out: Vec<(String, String)> = Vec::new();
    for u in v["result"].as_array().into_iter().flatten() {
        let chat = u["message"]["chat"].clone();
        let chat = if chat.is_null() { u["my_chat_member"]["chat"].clone() } else { chat };
        if let Some(id) = chat["id"].as_i64() {
            let title = chat["title"]
                .as_str()
                .map(String::from)
                .or_else(|| {
                    let f = chat["first_name"].as_str().unwrap_or("");
                    let l = chat["last_name"].as_str().unwrap_or("");
                    let u = chat["username"].as_str().map(|u| format!(" @{u}")).unwrap_or_default();
                    Some(format!("{f} {l}{u}").trim().to_string())
                })
                .unwrap_or_default();
            if !out.iter().any(|(i, _)| *i == id.to_string()) {
                out.push((id.to_string(), title));
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter() {
        assert!(wanted(When::Always, Outcome::Ok));
        assert!(!wanted(When::Problems, Outcome::Ok));
        assert!(wanted(When::Problems, Outcome::Warning));
        assert!(!wanted(When::Errors, Outcome::Warning));
        assert!(wanted(When::Errors, Outcome::Error));
        assert!(!wanted(When::Never, Outcome::Error));
    }
}
