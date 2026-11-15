//! Перенос настроек между компьютерами.
//!
//! DPAPI привязан к машине, поэтому в файле экспорта пароли — открытым текстом.
//! Файл нельзя писать в журнал. При импорте секреты снова шифруются DPAPI.
//! Журнал запусков и каталоги инкрементных копий не переносятся: следующее
//! инкрементное задание начнётся с полной копии.

use crate::destination::gdrive::{self, TokenStore};
use crate::job::{Destination, Job};
use crate::notify::NotifySettings;
use crate::pipeline::safe_name;
use crate::resources::{source_connection, valid_id, Connection, Resources, Storage};
use crate::settings::{self, ServiceSettings};
use crate::store::Store;
use anyhow::{bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const FORMAT: &str = "steadybackup";
pub const VERSION: u32 = 1;
const KEY_LIMIT: u64 = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Bundle {
    pub format: String,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exported: Option<String>,
    #[serde(default)]
    pub jobs: Vec<Job>,
    #[serde(default)]
    pub storages: Vec<Storage>,
    #[serde(default)]
    pub connections: Vec<Connection>,
    /// Группы заданий. Нет в старых файлах экспорта.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<crate::group::Group>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifySettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settings: Option<ServiceSettings>,
    /// Метки планировщика. Без них после переноса пропущенные запуски не догоняются,
    /// но и не стартуют все сразу: первая проверка ставит метку «сейчас».
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub scheduler: HashMap<Uuid, chrono::NaiveDateTime>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gdrive: Vec<GdriveAccount>,
    /// Содержимое файлов ключей SFTP. При импорте пишутся в `<data>\keys`, путь в задании меняется.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key_files: Vec<ExportedKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GdriveAccount {
    pub name: String,
    pub client_id: String,
    /// Пусто — встроенный клиент SteadyBackup. Его секрет в файл переноса не кладётся.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub client_secret: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedKey {
    pub path: String,
    pub content_b64: String,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct TransferReport {
    pub jobs: usize,
    pub storages: usize,
    pub connections: usize,
    pub gdrive: usize,
    pub keys: usize,
    pub deleted_jobs: usize,
    pub deleted_storages: usize,
    pub deleted_connections: usize,
    pub warnings: Vec<String>,
}

/// Имя копии, не совпадающее с уже занятыми ни как есть, ни после `safe_name`.
pub fn copy_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    let stem = copy_stem(name.trim());
    let first = format!("{stem} (копия)");
    if !taken(&first) {
        return first;
    }
    for n in 2..10_000 {
        let candidate = format!("{stem} (копия {n})");
        if !taken(&candidate) {
            return candidate;
        }
    }
    format!("{stem} (копия {})", Uuid::new_v4().simple())
}

fn copy_stem(name: &str) -> &str {
    let Some(rest) = name.strip_suffix(')') else { return name };
    let Some(i) = rest.rfind(" (копия") else { return name };
    let suffix = rest[i + " (копия".len()..].trim();
    if suffix.is_empty() || suffix.chars().all(|c| c.is_ascii_digit()) {
        let stem = name[..i].trim();
        if !stem.is_empty() {
            return stem;
        }
    }
    name
}

pub fn export_dir(data_dir: &Path) -> Result<(Bundle, Vec<String>)> {
    let mut warnings = Vec::new();
    let store = Store::new(data_dir);
    let mut jobs = store.load_jobs().context("чтение заданий")?;
    for job in &mut jobs {
        job.reveal_secrets().with_context(|| format!("задание «{}»", job.name))?;
    }
    let res = Resources::new(data_dir);
    let mut storages = res.storages().context("чтение хранилищ")?;
    for s in &mut storages {
        s.target.reveal_secrets().with_context(|| format!("хранилище «{}»", s.name))?;
    }
    let mut connections = res.connections().context("чтение подключений")?;
    for c in &mut connections {
        c.reveal_secrets().with_context(|| format!("подключение «{}»", c.name))?;
    }
    // Всегда снимок, даже если файлов ещё нет: иначе на новом компьютере останутся чужие уведомления.
    let mut notify = NotifySettings::load(data_dir);
    notify.reveal_secrets().context("уведомления")?;
    let settings = ServiceSettings::load(data_dir);
    let gdrive = export_gdrive(data_dir).context("учётные записи Google Drive")?;
    let mut key_files = Vec::new();
    let mut seen_keys = HashSet::new();
    for job in &jobs {
        collect_dest_keys(&job.destinations, &mut key_files, &mut warnings, &mut seen_keys);
    }
    for s in &storages {
        collect_dest_keys(std::slice::from_ref(&s.target), &mut key_files, &mut warnings, &mut seen_keys);
    }
    for c in &connections {
        if let crate::resources::ConnectionKind::Ssh { key_file: Some(path), .. } = &c.kind {
            collect_key(path, &mut key_files, &mut warnings, &mut seen_keys);
        }
    }
    for job in &jobs {
        if let crate::Source::MySql { ssh: Some(l), .. } | crate::Source::SshFiles { ssh: Some(l), .. } = &job.source {
            if let Some(path) = &l.key_file {
                collect_key(path, &mut key_files, &mut warnings, &mut seen_keys);
            }
        }
    }
    let bundle = Bundle {
        format: FORMAT.into(),
        version: VERSION,
        exported: Some(chrono::Local::now().to_rfc3339()),
        jobs,
        storages,
        connections,
        groups: store.load_groups().context("чтение групп")?,
        notify: Some(notify),
        settings: Some(settings),
        scheduler: store.load_marks(),
        gdrive,
        key_files,
    };
    Ok((bundle, warnings))
}

fn export_gdrive(data_dir: &Path) -> Result<Vec<GdriveAccount>> {
    let dir = data_dir.join("gdrive");
    let Ok(rd) = std::fs::read_dir(&dir) else { return Ok(Vec::new()) };
    let store = TokenStore::new(data_dir);
    let mut out = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("json") {
            continue;
        }
        let Some(name) = p.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
        let mut acc = store.load(&name).with_context(|| format!("учётная запись «{name}»"))?;
        reveal_string(&mut acc.client_secret).with_context(|| format!("секрет клиента «{name}»"))?;
        reveal_string(&mut acc.refresh_token).with_context(|| format!("токен «{name}»"))?;
        // Встроенный секрет и так в программе. В файл переноса его не кладём.
        if acc.client_id == gdrive::BUILTIN_CLIENT_ID && acc.client_secret == gdrive::BUILTIN_CLIENT_SECRET {
            acc.client_secret.clear();
        }
        out.push(GdriveAccount {
            name,
            client_id: acc.client_id,
            client_secret: acc.client_secret,
            refresh_token: acc.refresh_token,
            email: acc.email,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn reveal_string(v: &mut String) -> Result<()> {
    if !v.is_empty() && v != "dpapi:stored" {
        *v = crate::secret::reveal(v)?;
    }
    Ok(())
}

fn collect_dest_keys(dest: &[Destination], out: &mut Vec<ExportedKey>, warnings: &mut Vec<String>, seen: &mut HashSet<String>) {
    for d in dest {
        let Destination::Sftp { key_file: Some(path), .. } = d else { continue };
        collect_key(path, out, warnings, seen);
    }
}

fn collect_key(path: &str, out: &mut Vec<ExportedKey>, warnings: &mut Vec<String>, seen: &mut HashSet<String>) {
    if path.is_empty() || !seen.insert(path.to_string()) {
        return;
    }
    match read_key(path) {
        Ok(Some(content_b64)) => out.push(ExportedKey { path: path.to_string(), content_b64 }),
        Ok(None) => warnings.push(format!("ключ «{path}» больше 256 КБ и не включён в файл")),
        Err(e) => warnings.push(format!("ключ «{path}» не прочитан: {e}")),
    }
}

fn read_key(path: &str) -> Result<Option<String>> {
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        bail!("это не файл");
    }
    if meta.len() > KEY_LIMIT {
        return Ok(None);
    }
    let bytes = std::fs::read(path)?;
    Ok(Some(base64::engine::general_purpose::STANDARD.encode(bytes)))
}

pub fn parse(text: &str) -> Result<Bundle> {
    let bundle: Bundle = serde_json::from_str(text.trim().trim_start_matches('\u{feff}'))
        .context("это не файл экспорта SteadyBackup")?;
    if bundle.format != FORMAT {
        bail!("это не файл экспорта SteadyBackup");
    }
    if bundle.version != VERSION {
        bail!("файл экспорта версии {} не поддерживается (нужна {VERSION})", bundle.version);
    }
    Ok(bundle)
}

/// `replace` — удалить задания, хранилища и подключения, которых нет в файле.
/// Уведомления, рабочий каталог и метки планировщика заменяются всегда, если они есть в файле.
pub fn import_dir(data_dir: &Path, bundle: &Bundle, replace: bool) -> Result<TransferReport> {
    if bundle.format != FORMAT || bundle.version != VERSION {
        bail!("это не файл экспорта SteadyBackup");
    }
    let mut bundle = bundle.clone();
    let mut warnings = Vec::new();
    let (key_map, key_warnings) = place_keys(data_dir, &bundle.key_files)?;
    warnings.extend(key_warnings);
    rewrite_keys(&mut bundle, &key_map);

    let store = Store::new(data_dir);
    let existing = store.load_jobs().context("чтение заданий")?;
    let final_jobs = merged_jobs(&existing, &bundle.jobs, replace);
    ensure_unique_names(&final_jobs)?;
    ensure_ids(&bundle)?;
    if replace {
        check_refs(&bundle)?;
    }

    let res = Resources::new(data_dir);
    for s in &bundle.storages {
        res.save_storage(s.clone()).with_context(|| format!("хранилище «{}»", s.name))?;
    }
    for c in &bundle.connections {
        res.save_connection(c.clone()).with_context(|| format!("подключение «{}»", c.name))?;
    }
    let mut groups_now: HashSet<Uuid> =
        if replace { HashSet::new() } else { store.load_groups()?.iter().map(|g| g.id).collect() };
    for g in &bundle.groups {
        store.save_group(g).with_context(|| format!("группа «{}»", g.name))?;
        groups_now.insert(g.id);
    }
    if replace {
        for g in store.load_groups()? {
            if !groups_now.contains(&g.id) {
                store.delete_group(&g.id).with_context(|| format!("удаление группы «{}»", g.name))?;
            }
        }
    }
    for mut job in bundle.jobs.clone() {
        let name = job.name.clone();
        // Группы нет ни в файле, ни на этом компьютере: задание остаётся без группы.
        if job.group.is_some_and(|g| !groups_now.contains(&g)) {
            warnings.push(format!("задание «{name}»: группа не найдена, задание перенесено без группы"));
            job.group = None;
        }
        job.protect_secrets().with_context(|| format!("задание «{name}»"))?;
        store.save_job(&job).with_context(|| format!("задание «{name}»"))?;
    }
    let gdrive_n = import_gdrive(data_dir, &bundle.gdrive, replace)?;
    if let Some(n) = &bundle.notify {
        n.save(data_dir).context("уведомления")?;
    }
    if let Some(s) = &bundle.settings {
        match settings::resolve_work_dir(data_dir, s) {
            Ok(_) => s.save(data_dir).context("рабочий каталог")?,
            Err(e) => warnings.push(format!("рабочий каталог не перенесён: {e:#}")),
        }
    }
    save_marks(&store, &bundle, replace)?;

    let mut deleted_jobs = 0;
    let mut deleted_storages = 0;
    let mut deleted_connections = 0;
    if replace {
        let keep_jobs: HashSet<_> = bundle.jobs.iter().map(|j| j.id).collect();
        for j in &existing {
            if !keep_jobs.contains(&j.id) {
                store.delete_job(&j.id).with_context(|| format!("удаление задания «{}»", j.name))?;
                deleted_jobs += 1;
            }
        }
        let jobs_now = store.load_jobs()?;
        let keep_s: HashSet<_> = bundle.storages.iter().map(|s| s.id.as_str()).collect();
        for s in res.storages()? {
            if !keep_s.contains(s.id.as_str()) {
                res.delete_storage(&s.id, &jobs_now).with_context(|| format!("удаление хранилища «{}»", s.name))?;
                deleted_storages += 1;
            }
        }
        let keep_c: HashSet<_> = bundle.connections.iter().map(|c| c.id.as_str()).collect();
        for c in res.connections()? {
            if !keep_c.contains(c.id.as_str()) {
                res.delete_connection(&c.id, &jobs_now).with_context(|| format!("удаление подключения «{}»", c.name))?;
                deleted_connections += 1;
            }
        }
    }

    Ok(TransferReport {
        jobs: bundle.jobs.len(),
        storages: bundle.storages.len(),
        connections: bundle.connections.len(),
        gdrive: gdrive_n,
        keys: key_map.len(),
        deleted_jobs,
        deleted_storages,
        deleted_connections,
        warnings,
    })
}

fn merged_jobs(existing: &[Job], incoming: &[Job], replace: bool) -> Vec<Job> {
    if replace {
        return incoming.to_vec();
    }
    let ids: HashSet<_> = incoming.iter().map(|j| j.id).collect();
    let mut v: Vec<Job> = existing.iter().filter(|j| !ids.contains(&j.id)).cloned().collect();
    v.extend(incoming.iter().cloned());
    v
}

fn ensure_unique_names(jobs: &[Job]) -> Result<()> {
    let mut seen: HashMap<String, String> = HashMap::new();
    for j in jobs {
        if j.name.trim().is_empty() {
            bail!("в файле есть задание без имени");
        }
        if j.id.is_nil() {
            bail!("задание «{}» без идентификатора", j.name);
        }
        let safe = safe_name(&j.name);
        if let Some(other) = seen.insert(safe, j.name.clone()) {
            bail!(
                "имена «{other}» и «{}» после очистки совпадают — в одном назначении они сотрут копиям друг друга",
                j.name
            );
        }
    }
    Ok(())
}

fn ensure_ids(bundle: &Bundle) -> Result<()> {
    let mut ids = HashSet::new();
    for s in &bundle.storages {
        if !valid_id(&s.id) {
            bail!("недопустимый идентификатор хранилища «{}»", s.id);
        }
        if s.name.trim().is_empty() {
            bail!("хранилище «{}» без имени", s.id);
        }
        if !ids.insert(s.id.clone()) {
            bail!("хранилище «{}» повторяется в файле", s.id);
        }
    }
    ids.clear();
    for c in &bundle.connections {
        if !valid_id(&c.id) {
            bail!("недопустимый идентификатор подключения «{}»", c.id);
        }
        if c.name.trim().is_empty() {
            bail!("подключение «{}» без имени", c.id);
        }
        if !ids.insert(c.id.clone()) {
            bail!("подключение «{}» повторяется в файле", c.id);
        }
    }
    let mut job_ids = HashSet::new();
    for j in &bundle.jobs {
        if !job_ids.insert(j.id) {
            bail!("задание «{}» повторяется в файле", j.name);
        }
    }
    Ok(())
}

fn check_refs(bundle: &Bundle) -> Result<()> {
    let storages: HashSet<_> = bundle.storages.iter().map(|s| s.id.as_str()).collect();
    let connections: HashSet<_> = bundle.connections.iter().map(|c| c.id.as_str()).collect();
    for j in &bundle.jobs {
        for d in &j.destinations {
            if let Destination::Storage { storage, .. } = d {
                if !storages.contains(storage.as_str()) {
                    bail!("задание «{}» ссылается на хранилище «{storage}», которого нет в файле", j.name);
                }
            }
        }
        if let Some(id) = source_connection(&j.source) {
            if !connections.contains(id) {
                bail!("задание «{}» ссылается на подключение «{id}», которого нет в файле", j.name);
            }
        }
    }
    Ok(())
}

fn place_keys(data_dir: &Path, keys: &[ExportedKey]) -> Result<(HashMap<String, String>, Vec<String>)> {
    if keys.is_empty() {
        return Ok((HashMap::new(), Vec::new()));
    }
    let dir = data_dir.join("keys");
    std::fs::create_dir_all(&dir)?;
    let mut map = HashMap::new();
    let mut warnings = Vec::new();
    for k in keys {
        let bytes = match base64::engine::general_purpose::STANDARD.decode(k.content_b64.trim()) {
            Ok(b) if b.len() as u64 <= KEY_LIMIT => b,
            Ok(_) => {
                warnings.push(format!("ключ «{}» слишком большой и пропущен", k.path));
                continue;
            }
            Err(_) => {
                warnings.push(format!("ключ «{}» в файле повреждён и пропущен", k.path));
                continue;
            }
        };
        let name = key_filename(&k.path);
        let dest = unique_key_path(&dir, &name, &bytes);
        if std::fs::read(&dest).ok().as_deref() != Some(bytes.as_slice()) {
            crate::util::write_atomic(&dest, &bytes).with_context(|| format!("запись ключа «{}»", k.path))?;
        }
        map.insert(k.path.clone(), dest.display().to_string());
    }
    Ok((map, warnings))
}

fn key_filename(path: &str) -> String {
    let name = Path::new(path).file_name().and_then(|s| s.to_str()).unwrap_or("key");
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    if ok { name.to_string() } else { "key".into() }
}

fn unique_key_path(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let candidate = dir.join(name);
    if std::fs::read(&candidate).ok().as_deref() == Some(bytes) || !candidate.exists() {
        return candidate;
    }
    let stem = Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("key");
    let ext = Path::new(name).extension().and_then(|s| s.to_str()).unwrap_or("");
    for i in 2..10_000 {
        let n = if ext.is_empty() { format!("{stem}-{i}") } else { format!("{stem}-{i}.{ext}") };
        let p = dir.join(&n);
        if std::fs::read(&p).ok().as_deref() == Some(bytes) || !p.exists() {
            return p;
        }
    }
    dir.join(format!("key-{}", Uuid::new_v4().simple()))
}

fn rewrite_keys(bundle: &mut Bundle, map: &HashMap<String, String>) {
    if map.is_empty() {
        return;
    }
    for job in &mut bundle.jobs {
        rewrite_dests(&mut job.destinations, map);
    }
    for s in &mut bundle.storages {
        rewrite_dests(std::slice::from_mut(&mut s.target), map);
    }
    let fix = |path: &mut String| {
        if let Some(new) = map.get(path.as_str()) {
            *path = new.clone();
        }
    };
    for c in &mut bundle.connections {
        if let crate::resources::ConnectionKind::Ssh { key_file: Some(path), .. } = &mut c.kind {
            fix(path);
        }
    }
    for job in &mut bundle.jobs {
        if let crate::Source::MySql { ssh: Some(l), .. } | crate::Source::SshFiles { ssh: Some(l), .. } = &mut job.source {
            if let Some(path) = &mut l.key_file {
                fix(path);
            }
        }
    }
}

fn rewrite_dests(dest: &mut [Destination], map: &HashMap<String, String>) {
    for d in dest {
        if let Destination::Sftp { key_file: Some(path), .. } = d {
            if let Some(new) = map.get(path) {
                *path = new.clone();
            }
        }
    }
}

fn import_gdrive(data_dir: &Path, accounts: &[GdriveAccount], replace: bool) -> Result<usize> {
    let store = TokenStore::new(data_dir);
    let mut keep = HashSet::new();
    for a in accounts {
        if !valid_account_name(&a.name) {
            bail!("недопустимое имя учётной записи Google");
        }
        let client_secret = if a.client_secret.is_empty() && a.client_id == gdrive::BUILTIN_CLIENT_ID {
            gdrive::BUILTIN_CLIENT_SECRET.to_string()
        } else {
            a.client_secret.clone()
        };
        let mut acc = gdrive::Account {
            client_id: a.client_id.clone(),
            client_secret,
            refresh_token: a.refresh_token.clone(),
            email: a.email.clone(),
        };
        if !acc.client_secret.is_empty() && !acc.client_secret.starts_with("dpapi:") {
            acc.client_secret = crate::secret::protect_str(&acc.client_secret)?;
        }
        if !acc.refresh_token.is_empty() && !acc.refresh_token.starts_with("dpapi:") {
            acc.refresh_token = crate::secret::protect_str(&acc.refresh_token)?;
        }
        store.save(&a.name, &acc).with_context(|| format!("учётная запись «{}»", a.name))?;
        keep.insert(safe_name(&a.name));
    }
    if replace {
        let dir = data_dir.join("gdrive");
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                if !keep.contains(stem) {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
    Ok(accounts.len())
}

fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
        && name.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
}

fn save_marks(store: &Store, bundle: &Bundle, replace: bool) -> Result<()> {
    let mut marks = if replace { HashMap::new() } else { store.load_marks() };
    for (id, t) in &bundle.scheduler {
        marks.insert(*id, *t);
    }
    if replace {
        let ids: HashSet<_> = bundle.jobs.iter().map(|j| j.id).collect();
        marks.retain(|id, _| ids.contains(id));
    }
    store.save_marks(&marks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("steady-bundle-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn job(id: &str, name: &str) -> Job {
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"{name}","source":{{"type":"files","paths":["C:\\x"],"kind":"full"}},"destinations":[{{"type":"local","path":"C:\\d"}}]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn copy_name_adds_suffix_and_number() {
        assert_eq!(copy_name("База", |_| false), "База (копия)");
        let taken = |n: &str| n == "База (копия)" || safe_name(n) == safe_name("База (копия)");
        assert_eq!(copy_name("База", taken), "База (копия 2)");
        assert_eq!(copy_name("База (копия)", |_| false), "База (копия)");
        assert_eq!(copy_name("База (копия 2)", |_| false), "База (копия)");
    }

    #[test]
    fn roundtrip_keeps_jobs_and_does_not_touch_strangers_unless_replaced() {
        let src = tmp();
        let store = Store::new(&src);
        store.save_job(&job("11111111-1111-1111-1111-111111111111", "База")).unwrap();
        std::fs::write(src.join("settings.json"), r#"{"work_dir":"D:\\Backup\\work"}"#).unwrap();
        let (bundle, warnings) = export_dir(&src).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(bundle.jobs.len(), 1);
        assert_eq!(bundle.settings.as_ref().unwrap().work_dir.as_deref(), Some(r"D:\Backup\work"));

        let dst = tmp();
        let other = job("22222222-2222-2222-2222-222222222222", "Чужое");
        Store::new(&dst).save_job(&other).unwrap();
        let report = import_dir(&dst, &bundle, false).unwrap();
        assert_eq!(report.jobs, 1);
        assert_eq!(report.deleted_jobs, 0);
        let names: Vec<_> = Store::new(&dst).load_jobs().unwrap().into_iter().map(|j| j.name).collect();
        assert!(names.contains(&"База".into()) && names.contains(&"Чужое".into()));

        import_dir(&dst, &bundle, true).unwrap();
        let left: Vec<_> = Store::new(&dst).load_jobs().unwrap().into_iter().map(|j| j.name).collect();
        assert_eq!(left, vec!["База".to_string()]);
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn missing_key_is_a_warning_not_a_failure() {
        let src = tmp();
        let mut j = job("11111111-1111-1111-1111-111111111111", "Sftp");
        j.destinations = vec![serde_json::from_str(
            r#"{"type":"sftp","host":"h","user":"u","path":"/b","key_file":"C:\\no\\such\\key"}"#,
        )
        .unwrap()];
        Store::new(&src).save_job(&j).unwrap();
        let (bundle, warnings) = export_dir(&src).unwrap();
        assert!(bundle.key_files.is_empty());
        assert!(warnings.iter().any(|w| w.contains("не прочитан")));
        assert!(warnings.iter().all(|w| !w.contains("dpapi:")));
        let _ = std::fs::remove_dir_all(&src);
    }

    #[test]
    fn key_file_is_rewritten_under_data_dir() {
        let src = tmp();
        let key = src.join("id_rsa");
        std::fs::write(&key, b"not-a-real-key").unwrap();
        let mut j = job("11111111-1111-1111-1111-111111111111", "Sftp");
        let path = key.display().to_string().replace('\\', "\\\\");
        j.destinations = vec![serde_json::from_str(&format!(
            r#"{{"type":"sftp","host":"h","user":"u","path":"/b","key_file":"{path}"}}"#
        ))
        .unwrap()];
        Store::new(&src).save_job(&j).unwrap();
        let (bundle, warnings) = export_dir(&src).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(bundle.key_files.len(), 1);

        let dst = tmp();
        import_dir(&dst, &bundle, false).unwrap();
        let loaded = Store::new(&dst).load_jobs().unwrap();
        let Destination::Sftp { key_file: Some(p), .. } = &loaded[0].destinations[0] else { panic!("нет ключа") };
        assert!(p.contains("keys"), "{p}");
        assert_eq!(std::fs::read(p).unwrap(), b"not-a-real-key");
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }

    #[test]
    fn groups_travel_with_jobs() {
        let src = tmp();
        let store = Store::new(&src);
        let gid = Uuid::from_u128(9);
        store
            .save_group(&serde_json::from_str(&format!(r#"{{"id":"{gid}","name":"Ночь"}}"#)).unwrap())
            .unwrap();
        let mut j = job("11111111-1111-1111-1111-111111111111", "База");
        j.group = Some(gid);
        store.save_job(&j).unwrap();
        let (bundle, _) = export_dir(&src).unwrap();
        assert_eq!(bundle.groups.len(), 1);

        let dst = tmp();
        let report = import_dir(&dst, &bundle, false).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        assert_eq!(Store::new(&dst).load_groups().unwrap().len(), 1);
        assert_eq!(Store::new(&dst).load_jobs().unwrap()[0].group, Some(gid));

        // Старый файл без групп: ссылка на неизвестную группу снимается с предупреждением.
        let mut old = bundle.clone();
        old.groups.clear();
        let dst2 = tmp();
        let report = import_dir(&dst2, &old, false).unwrap();
        assert!(report.warnings.iter().any(|w| w.contains("группа не найдена")));
        assert!(Store::new(&dst2).load_jobs().unwrap()[0].group.is_none());
        for d in [src, dst, dst2] {
            let _ = std::fs::remove_dir_all(d);
        }
    }

    #[test]
    fn rejects_a_plain_job_file() {
        let err = parse(r#"{"id":"11111111-1111-1111-1111-111111111111","name":"t"}"#).unwrap_err();
        assert!(err.to_string().contains("не файл экспорта"));
    }

    #[cfg(windows)]
    #[test]
    fn password_is_plain_in_export_and_protected_after_import() {
        let src = tmp();
        let mut j = job("11111111-1111-1111-1111-111111111111", "Секрет");
        j.archive.password = Some("test-secret".into());
        j.protect_secrets().unwrap();
        Store::new(&src).save_job(&j).unwrap();
        let (bundle, warnings) = export_dir(&src).unwrap();
        assert!(warnings.iter().all(|w| !w.contains("test-secret")));
        assert_eq!(bundle.jobs[0].archive.password.as_deref(), Some("test-secret"));
        let text = serde_json::to_string(&bundle).unwrap();
        assert!(text.contains("test-secret"));

        let dst = tmp();
        import_dir(&dst, &bundle, false).unwrap();
        let loaded = Store::new(&dst).load_jobs().unwrap();
        let stored = loaded[0].archive.password.clone().unwrap();
        assert!(stored.starts_with("dpapi:"), "{stored}");
        assert!(!stored.contains("test-secret"));
        let mut again = loaded[0].clone();
        again.reveal_secrets().unwrap();
        assert_eq!(again.archive.password.as_deref(), Some("test-secret"));
        let _ = std::fs::remove_dir_all(&src);
        let _ = std::fs::remove_dir_all(&dst);
    }
}
