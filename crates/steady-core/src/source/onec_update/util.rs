//! Номера версий и текстовые форматы дистрибутивов 1С.

use std::cmp::Ordering;
use std::fmt;

/// Номер версии вида `3.0.150.25`. Сравнивается по числам, а не как строка.
#[derive(Clone, Debug)]
pub struct Version(pub Vec<u32>);

impl Version {
    fn significant(&self) -> &[u32] {
        let n = self.0.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1);
        &self.0[..n]
    }

    pub fn parse(s: &str) -> Option<Version> {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        // В каталогах шаблонов версия записана через подчёркивания: 3_0_150_25.
        let parts: Option<Vec<u32>> = s.split(['.', '_']).map(|p| p.trim().parse().ok()).collect();
        parts.filter(|p| !p.is_empty()).map(Version)
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.significant() == other.significant()
    }
}

impl Eq for Version {}

impl std::hash::Hash for Version {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.significant().hash(h);
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        for i in 0..n {
            let a = self.0.get(i).copied().unwrap_or(0);
            let b = other.0.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => {}
                other => return other,
            }
        }
        Ordering::Equal
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: Vec<String> = self.0.iter().map(|n| n.to_string()).collect();
        f.write_str(&s.join("."))
    }
}

/// Текст файла платформы: UTF-8 (с BOM или без), UTF-16LE с BOM, иначе Windows-1251.
pub fn decode_text(raw: &[u8]) -> String {
    if let Some(r) = raw.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        return String::from_utf8_lossy(r).into_owned();
    }
    if raw.starts_with(&[0xFF, 0xFE]) {
        return encoding_rs::UTF_16LE.decode(raw).0.into_owned();
    }
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::WINDOWS_1251.decode(raw).0.into_owned(),
    }
}

/// Пары `ключ=значение` до первой секции `[...]`.
pub fn parse_ini_head(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.starts_with('[') {
            break;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

pub fn ini_get<'a>(kv: &'a [(String, String)], key: &str) -> Option<&'a str> {
    kv.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// Сравнение имён конфигураций без учёта регистра, пробелов и кавычек.
pub fn same_name(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| !c.is_whitespace() && !matches!(c, '"' | '«' | '»' | '\''))
            .flat_map(|c| c.to_lowercase())
            .collect()
    };
    let (a, b) = (norm(a), norm(b));
    !a.is_empty() && a == b
}
