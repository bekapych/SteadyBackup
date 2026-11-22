//! Блокировка сеансов через COM, завершение серверных сеансов через RAC и
//! точечное завершение локальных процессов файловой базы.

use super::platform::{Platform, Target};
use super::util::decode_text;
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Записи `ключ : значение` из текстового вывода RAC. Повтор ключа начинает новую запись.
pub fn parse_rac_records(text: &str) -> Vec<BTreeMap<String, String>> {
    let mut records = Vec::new();
    let mut current = BTreeMap::new();
    let flush = |records: &mut Vec<BTreeMap<String, String>>,
                 current: &mut BTreeMap<String, String>| {
        if !current.is_empty() {
            records.push(std::mem::take(current));
        }
    };
    for line in text.lines() {
        let line = line.trim().trim_start_matches('\u{feff}');
        if line.is_empty() {
            flush(&mut records, &mut current);
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        if current.contains_key(&key) {
            flush(&mut records, &mut current);
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        current.insert(key, value.to_string());
    }
    flush(&mut records, &mut current);
    records
}

pub fn rac_session_ids(records: &[BTreeMap<String, String>], infobase: &str) -> Vec<String> {
    records
        .iter()
        .filter(|r| {
            r.get("infobase")
                .is_some_and(|id| id.eq_ignore_ascii_case(infobase))
        })
        .filter_map(|r| r.get("session").cloned())
        .collect()
}

/// Стандартный порт менеджера кластера в строке ИБ заменяется портом агента RAC.
/// Нестандартные порты не угадываются: для них потребуется отдельная настройка.
fn rac_address(server: &str) -> String {
    server
        .strip_suffix(":1541")
        .map(|host| format!("{host}:1540"))
        .unwrap_or_else(|| server.to_string())
}

/// Найденная серверная база. Подключение к кластеру проверяется до блокировки и .dt.
pub struct RacBase {
    exe: PathBuf,
    server: String,
    cluster: String,
    infobase: String,
    ib_user: String,
    ib_password: String,
    cluster_user: String,
    cluster_password: String,
    timeout: Duration,
}

impl RacBase {
    pub fn discover(
        platform: &Platform,
        cluster_user: &str,
        cluster_password: &str,
    ) -> Result<Self> {
        let Target::Server { server, name } = &platform.target else {
            bail!("RAC применим только к серверной базе");
        };
        let exe = platform
            .exe
            .parent()
            .unwrap_or(Path::new("."))
            .join("rac.exe");
        crate::acl::check_trusted_exe(&exe).context("1С: RAC")?;
        let mut rac = Self {
            exe,
            server: rac_address(server),
            cluster: String::new(),
            infobase: String::new(),
            ib_user: platform.user.clone(),
            ib_password: platform.password.clone(),
            cluster_user: cluster_user.to_string(),
            cluster_password: cluster_password.to_string(),
            timeout: platform.timeout,
        };
        let clusters = rac.run(&["cluster", "list"], false, false)?;
        let records = parse_rac_records(&clusters);
        let ids: Vec<String> = records
            .iter()
            .filter_map(|r| r.get("cluster").cloned())
            .collect();
        if ids.is_empty() {
            bail!("RAC: на сервере {server} не найден кластер 1С");
        }
        for cluster in ids {
            rac.cluster = cluster;
            let text = rac.run(&["infobase", "list"], true, true)?;
            let found = parse_rac_records(&text).into_iter().find(|r| {
                r.get("name").is_some_and(|n| n == name)
                    && r.get("infobase").is_some_and(|id| !id.is_empty())
            });
            if let Some(record) = found {
                rac.infobase = record.get("infobase").cloned().unwrap_or_default();
                return Ok(rac);
            }
        }
        bail!(r"RAC: информационная база «{name}» не найдена на сервере {server}")
    }

    fn command(&self, args: &[&str], cluster: bool, infobase: bool) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args);
        if cluster && !self.cluster.is_empty() {
            cmd.arg(format!("--cluster={}", self.cluster));
        }
        if infobase && !self.infobase.is_empty() {
            cmd.arg(format!("--infobase={}", self.infobase));
        }
        if infobase && !self.ib_user.is_empty() {
            cmd.arg(format!("--infobase-user={}", self.ib_user));
        }
        if infobase && !self.ib_password.is_empty() {
            cmd.arg(format!("--infobase-pwd={}", self.ib_password));
        }
        if !self.cluster_user.is_empty() {
            cmd.arg(format!("--cluster-user={}", self.cluster_user));
        }
        if !self.cluster_password.is_empty() {
            cmd.arg(format!("--cluster-pwd={}", self.cluster_password));
        }
        cmd.arg(&self.server);
        cmd
    }

    fn run(&self, args: &[&str], cluster: bool, infobase: bool) -> Result<String> {
        let out = crate::process::run(
            self.command(args, cluster, infobase),
            self.timeout,
            "1С: RAC",
        )?;
        let stdout = decode_text(&out.stdout);
        let stderr = decode_text(&out.stderr);
        if !out.status.success() {
            let detail = if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            };
            bail!(
                "RAC: команда {} завершилась с ошибкой (код {:?}): {}. Если требуется администратор кластера, задайте cluster_user/cluster_password",
                args.join(" "),
                out.status.code(),
                detail
            );
        }
        Ok(stdout)
    }

    pub fn terminate_all(&self) -> Result<()> {
        let text = self.run(&["session", "list"], true, true)?;
        let ids = rac_session_ids(&parse_rac_records(&text), &self.infobase);
        for id in ids {
            let session = format!("--session={id}");
            self.run(&["session", "terminate", &session], true, false)
                .with_context(|| "RAC: завершение сеанса; до /UpdateCfg база не изменялась")?;
        }
        let left = self.run(&["session", "list"], true, true)?;
        let left = rac_session_ids(&parse_rac_records(&left), &self.infobase);
        if !left.is_empty() {
            bail!(
                "RAC: после завершения остались активные сеансы: {}",
                left.len()
            );
        }
        Ok(())
    }
}

/// Блокировка с исходными параметрами, сохранёнными JScript до изменения.
pub struct ComLock;

fn lock_state_path(platform: &Platform) -> PathBuf {
    platform.work.join("onec-lock.state")
}

impl ComLock {
    pub fn acquire(platform: &Platform) -> Result<Self> {
        let result = (|| -> Result<()> {
            let text = run_session_script(platform, "lock")?;
            text.lines()
                .find_map(|l| l.trim().strip_prefix("WAS "))
                .map(|v| v.trim() == "1")
                .ok_or_else(|| {
                    anyhow!("COM: не удалось прочитать исходное состояние блокировки сеансов")
                })?;
            if !text.lines().any(|l| l.trim() == "LOCKED") {
                bail!("COM: блокировка новых сеансов не установлена");
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(Self),
            Err(error) => Err(Self::restore_after_failed_acquire(platform, error)),
        }
    }

    /// State-файл создаётся и закрывается до первой попытки изменить блокировку.
    /// Без него исходное состояние неизвестно, поэтому автоматически ничего не снимаем.
    fn restore_after_failed_acquire(platform: &Platform, error: anyhow::Error) -> anyhow::Error {
        if !lock_state_path(platform).is_file() {
            return error;
        }
        match run_session_script(platform, "restore") {
            Ok(text) if text.lines().any(|l| l.trim() == "RESTORED") => {
                error.context("COM: после ошибки установки исходные параметры блокировки сеансов восстановлены")
            }
            Ok(_) => anyhow!(
                "{error:#}; COM: восстановление исходных параметров не подтверждено; блокировка могла остаться изменённой"
            ),
            Err(restore_error) => anyhow!(
                "{error:#}; COM: попытка восстановления исходных параметров также завершилась ошибкой: {restore_error:#}; блокировка могла остаться изменённой"
            ),
        }
    }

    /// Восстанавливает исходные признак и код разрешения; ошибка наружу не скрывается.
    pub fn restore(&self, platform: &Platform) -> Result<()> {
        let text = run_session_script(platform, "restore")?;
        if !text.lines().any(|l| l.trim() == "RESTORED") {
            bail!("COM: исходное состояние блокировки сеансов не подтверждено");
        }
        Ok(())
    }
}

pub fn ensure_no_other_com_sessions(platform: &Platform) -> Result<()> {
    let text = run_session_script(platform, "sessions")?;
    let n: usize = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("SESSIONS "))
        .ok_or_else(|| anyhow!("COM: не удалось перечислить сеансы"))?
        .trim()
        .parse()
        .context("COM: неверное число сеансов")?;
    if n > 0 {
        bail!(
            "в базе остались чужие сеансы: {n}. Удалённые сеансы файловой базы по SMB не завершаются; .dt и обновление не начинались"
        );
    }
    Ok(())
}

fn run_session_script(platform: &Platform, action: &str) -> Result<String> {
    run_jscript(platform, SESSIONS_JS, action, "1С: блокировка сеансов")
}

/// Запускает статический UTF-16 JScript двумя разрядностями cscript. Пароль находится
/// только в окружении дочернего процесса; ни сценарий, ни журнал его не содержат.
fn run_jscript(platform: &Platform, source: &str, action: &str, what: &str) -> Result<String> {
    let progid = platform
        .com_progid()
        .context("не удалось определить V83/V85.COMConnector по пути платформы")?;
    std::fs::create_dir_all(&platform.work)?;
    let script = platform
        .work
        .join(format!("onec-{}.js", uuid::Uuid::new_v4().simple()));
    let mut bytes = vec![0xFF, 0xFE];
    for u in source.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&script, bytes)?;
    let windir = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let mut last = String::new();
    for host in [
        windir.join("System32").join("cscript.exe"),
        windir.join("SysWOW64").join("cscript.exe"),
    ] {
        if !host.is_file() {
            continue;
        }
        crate::acl::check_trusted_exe(&host).context("1С: cscript")?;
        let out_path = platform
            .work
            .join(format!("com-{}.out", uuid::Uuid::new_v4().simple()));
        let mut cmd = Command::new(&host);
        cmd.args(["//NoLogo", "//E:jscript"])
            .arg(&script)
            .env("STEADY_ONEC_PROGID", &progid)
            .env("STEADY_ONEC_CONN", platform.connection_string())
            .env("STEADY_ONEC_ACTION", action)
            .env("STEADY_ONEC_USER", &platform.user)
            .env("STEADY_ONEC_UC", &platform.uc)
            .env("STEADY_ONEC_STATE", lock_state_path(platform))
            .env("STEADY_ONEC_OUT", &out_path);
        let out = crate::process::run(cmd, platform.timeout, what)?;
        let text = decode_text(&std::fs::read(&out_path).unwrap_or_default());
        let _ = std::fs::remove_file(&out_path);
        if text.lines().any(|l| l.trim().starts_with("NOCOM")) {
            last = text;
            continue;
        }
        let _ = std::fs::remove_file(&script);
        if !out.status.success() || text.lines().any(|l| l.trim().starts_with("ERROR ")) {
            let detail = text.trim();
            let detail = if detail.is_empty() {
                decode_text(&out.stderr).trim().to_string()
            } else {
                detail.to_string()
            };
            bail!("{what}: {detail}");
        }
        return Ok(text);
    }
    let _ = std::fs::remove_file(&script);
    bail!(
        "{what}: COM-класс {progid} не зарегистрирован или недоступен: {}",
        last.trim()
    )
}

/// Обработчики БСП: сначала COM, при недоступном соединении — штатный ENTERPRISE.
pub fn run_handlers(platform: &Platform) -> Result<()> {
    match run_jscript(
        platform,
        HANDLERS_JS,
        "handlers",
        "1С: обработчики обновления",
    ) {
        Ok(_) => Ok(()),
        Err(e) if format!("{e:#}").contains("CONNECTED") => Err(e),
        Err(e) => {
            tracing::warn!("1С: COM-обработчики недоступны ({e:#}); запуск ENTERPRISE");
            if matches!(platform.target, Target::Server { .. }) {
                tracing::warn!("1С: без COM серверная база может показать форму подтверждения легальности обновления");
            }
            let r = platform.run(
                "ENTERPRISE",
                &[
                    "/AllowExecuteScheduledJobs".as_ref(),
                    "-Off".as_ref(),
                    "/C".as_ref(),
                    "ВыполнитьОбновлениеИЗавершитьРаботу;РегламентныеЗаданияОтключены".as_ref(),
                ],
            )?;
            if !r.ok() {
                bail!(
                    "обработчики обновления завершились с ошибкой ({})",
                    super::platform::explain(&r)
                );
            }
            Ok(())
        }
    }
}

/// Обычное COM-соединение намеренно не вызывает несуществующий «ЗавершитьСеанс»:
/// оно только блокирует новые сеансы, восстанавливает параметры и считает чужие сеансы.
const SESSIONS_JS: &str = r#"var env = new ActiveXObject("WScript.Shell").Environment("PROCESS");
var fso = new ActiveXObject("Scripting.FileSystemObject");
var out = fso.CreateTextFile(env("STEADY_ONEC_OUT"), true, true);
function say(s) { out.WriteLine(s); }
function writeLockState(b) {
  var state = fso.CreateTextFile(env("STEADY_ONEC_STATE"), true, true);
  state.WriteLine("STEADY_LOCK_V1");
  state.WriteLine("WAS " + (b.Установлена ? "1" : "0"));
  state.WriteLine("KEY " + encodeURIComponent(b.КодРазрешения));
  state.Close();
}
function readLockState() {
  var state = fso.OpenTextFile(env("STEADY_ONEC_STATE"), 1, false, -1);
  var text;
  try { text = state.ReadAll(); } finally { state.Close(); }
  var lines = text.split(/\r?\n/);
  if (lines[0] != "STEADY_LOCK_V1") throw new Error("неверная версия состояния блокировки");
  var was = null;
  var key = null;
  for (var i = 1; i < lines.length; i++) {
    if (lines[i].indexOf("WAS ") == 0) was = lines[i].substring(4);
    if (lines[i].indexOf("KEY ") == 0) key = lines[i].substring(4);
  }
  if ((was != "0" && was != "1") || key == null) throw new Error("повреждено состояние блокировки");
  return { was: was == "1", key: decodeURIComponent(key) };
}
function restoreLockFromState(c) {
  var saved = readLockState();
  var current = c.ПолучитьБлокировкуСеансов();
  var empty = c.NewObject("БлокировкаСеансов");
  current.Начало = empty.Начало;
  current.Конец = empty.Конец;
  current.КодРазрешения = saved.key;
  current.Установлена = saved.was;
  c.УстановитьБлокировкуСеансов(current);
}
var cc;
try { cc = new ActiveXObject(env("STEADY_ONEC_PROGID")); }
catch (e) { say("NOCOM " + e.message); out.Close(); WScript.Quit(3); }
var code = 0;
try {
  var c = cc.Connect(env("STEADY_ONEC_CONN"));
  var action = env("STEADY_ONEC_ACTION");
  if (action == "lock") {
    var b = c.ПолучитьБлокировкуСеансов();
    var empty = c.NewObject("БлокировкаСеансов");
    if (String(b.Начало) != String(empty.Начало) || String(b.Конец) != String(empty.Конец)) {
      throw new Error("неподдерживаемое расписание блокировки: Начало/Конец должны быть пустыми; изменения не выполнялись");
    }
    writeLockState(b);
    say("WAS " + (b.Установлена ? "1" : "0"));
    try {
      b.Начало = empty.Начало;
      b.Конец = empty.Конец;
      b.КодРазрешения = env("STEADY_ONEC_UC");
      b.Установлена = true;
      c.УстановитьБлокировкуСеансов(b);
      say("LOCKED");
    } catch (lockError) {
      try { restoreLockFromState(c); }
      catch (restoreError) {
        throw new Error("блокировка могла остаться изменённой: " + lockError.message + "; восстановление: " + restoreError.message);
      }
      throw lockError;
    }
  } else if (action == "restore") {
    restoreLockFromState(c);
    say("RESTORED");
  } else if (action == "sessions") {
    var own = c.НомерСеансаИнформационнойБазы();
    var ss = (new VBArray(c.ПолучитьСеансыИнформационнойБазы())).toArray();
    var n = 0;
    for (var i = 0; i < ss.length; i++) {
      if (ss[i].НомерСеанса != own) n++;
    }
    say("SESSIONS " + n);
  } else {
    throw new Error("неизвестное действие");
  }
  c = null;
} catch (e) { say("ERROR " + e.message); code = 1; }
out.Close();
WScript.Quit(code);
"#;

const HANDLERS_JS: &str = r#"var env = new ActiveXObject("WScript.Shell").Environment("PROCESS");
var fso = new ActiveXObject("Scripting.FileSystemObject");
var out = fso.CreateTextFile(env("STEADY_ONEC_OUT"), true, true);
function say(s) { out.WriteLine(s); }
var cc;
try { cc = new ActiveXObject(env("STEADY_ONEC_PROGID")); }
catch (e) { say("NOCOM " + e.message); out.Close(); WScript.Quit(3); }
var code = 0;
try {
  var c = cc.Connect(env("STEADY_ONEC_CONN"));
  say("CONNECTED");
  var m = c.ОбновлениеИнформационнойБазы;
  if (m == null) { say("NOBSP"); out.Close(); WScript.Quit(0); }
  var r = m.ВыполнитьОбновлениеИнформационнойБазы(false);
  say("UPDATE " + r);
  if (r == "ОшибкаУстановкиМонопольногоРежима") { code = 2; }
  else {
    try {
      c.ОбновлениеКонфигурации.ЗавершитьОбновление(true, "", env("STEADY_ONEC_USER"));
      say("LEGAL ok");
    } catch (e) { say("LEGAL " + e.message); }
  }
  c = null;
} catch (e) { say("ERROR " + e.message); code = 1; }
out.Close();
WScript.Quit(code);
"#;

/// Завершает только локальные 1cv8.exe/1cv8c.exe с аргументом /F, равным базе.
/// Сначала Rust разбирает командные строки, затем PowerShell повторно проверяет /F
/// непосредственно перед Stop-Process (защита от повторного использования PID).
pub fn terminate_local_file_processes(platform: &Platform) -> Result<()> {
    let Target::File(base) = &platform.target else {
        return Ok(());
    };
    let canonical_base = std::fs::canonicalize(base)
        .with_context(|| format!("канонический путь файловой базы {}", base.display()))?;
    let powershell = crate::util::powershell_exe();
    crate::acl::check_trusted_exe(&powershell).context("1С: PowerShell")?;
    std::fs::create_dir_all(&platform.work)?;
    let list_script = platform.work.join("list-1c-processes.ps1");
    let stop_script = platform.work.join("stop-1c-processes.ps1");
    std::fs::write(&list_script, LIST_PROCESSES_PS)?;
    std::fs::write(&stop_script, STOP_PROCESSES_PS)?;
    let mut list = Command::new(&powershell);
    list.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&list_script)
    .arg("-BasePath")
    .arg(&canonical_base);
    let out = crate::process::run(list, platform.timeout, "1С: поиск локальных сеансов")?;
    if !out.status.success() {
        bail!(
            "1С: поиск локальных сеансов: {}",
            decode_text(&out.stderr).trim()
        );
    }
    let text = decode_text(&out.stdout);
    let processes = parse_process_list(&text)?;
    let ids: Vec<String> = processes
        .iter()
        .filter(|p| {
            p.command_line
                .as_deref()
                .is_some_and(|cmd| file_arg_matches(cmd, &canonical_base))
        })
        .map(|p| p.process_id.to_string())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let mut stop = Command::new(&powershell);
    stop.args([
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-File",
    ])
    .arg(&stop_script)
    .arg("-BasePath")
    .arg(&canonical_base)
    .arg("-ProcessIds");
    stop.args(ids);
    let out = crate::process::run(stop, platform.timeout, "1С: завершение локальных сеансов")?;
    if !out.status.success() {
        bail!(
            "1С: завершение локальных сеансов: {}",
            decode_text(&out.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Deserialize)]
struct ProcessInfo {
    #[serde(rename = "ProcessId")]
    process_id: u32,
    #[serde(rename = "CommandLine")]
    command_line: Option<String>,
}

fn parse_process_list(text: &str) -> Result<Vec<ProcessInfo>> {
    let text = text.trim().trim_start_matches('\u{feff}');
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let value: serde_json::Value =
        serde_json::from_str(text).context("PowerShell: список процессов не является JSON")?;
    if value.is_null() {
        return Ok(Vec::new());
    }
    if value.is_array() {
        serde_json::from_value(value).context("PowerShell: неверный список процессов")
    } else {
        Ok(vec![
            serde_json::from_value(value).context("PowerShell: неверный процесс")?
        ])
    }
}

const LIST_PROCESSES_PS: &str = r#"param([Parameter(Mandatory=$true)][string]$BasePath)
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$null = [IO.Path]::GetFullPath($BasePath)
@(Get-CimInstance Win32_Process -Filter "Name='1cv8.exe' OR Name='1cv8c.exe'" |
  Select-Object ProcessId, CommandLine) | ConvertTo-Json -Compress
"#;

const STOP_PROCESSES_PS: &str = r#"param([Parameter(Mandatory=$true)][string]$BasePath, [int[]]$ProcessIds)
$ErrorActionPreference = 'Stop'
function Test-Base([string]$CommandLine, [string]$Base) {
  if ([string]::IsNullOrWhiteSpace($CommandLine)) { return $false }
  $m = [regex]::Match($CommandLine, '(?i)(?:^|\s)/F\s*(?:"(?<q>[^"]+)"|(?<p>\S+))')
  if (-not $m.Success) { return $false }
  $value = $(if ($m.Groups['q'].Success) { $m.Groups['q'].Value } else { $m.Groups['p'].Value })
  try { $full = [IO.Path]::GetFullPath($value).TrimEnd('\') }
  catch { return $false }
  return $full -ieq ([IO.Path]::GetFullPath($Base).TrimEnd('\'))
}
foreach ($id in $ProcessIds) {
  $p = Get-CimInstance Win32_Process -Filter "ProcessId=$id" -ErrorAction SilentlyContinue
  if ($null -eq $p) { continue }
  if ($p.Name -ne '1cv8.exe' -and $p.Name -ne '1cv8c.exe') { continue }
  if (Test-Base $p.CommandLine $BasePath) { Stop-Process -Id $id -Force -ErrorAction Stop }
}
"#;

/// Точное сравнение аргумента `/F` в командной строке Windows с каноническим путём базы.
pub fn file_arg_matches(command_line: &str, canonical_ib: &Path) -> bool {
    let tokens = windows_command_line_tokens(command_line);
    let mut i = 1; // нулевой токен — имя исполняемого файла
    while i < tokens.len() {
        let token = &tokens[i];
        let value = if token.eq_ignore_ascii_case("/F") {
            i += 1;
            tokens.get(i).map(String::as_str)
        } else if token
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("/F"))
            && token.len() > 2
        {
            token.get(2..)
        } else {
            i += 1;
            continue;
        };
        return value.is_some_and(|v| same_file_path(v, canonical_ib));
    }
    false
}

fn same_file_path(value: &str, base: &Path) -> bool {
    fn norm(s: &str) -> String {
        let mut s = s.trim().trim_matches('"').replace('/', "\\");
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            s = format!(r"\\{rest}");
        } else if let Some(rest) = s.strip_prefix(r"\\?\") {
            s = rest.to_string();
        }
        while s.len() > 3 && s.ends_with('\\') {
            s.pop();
        }
        s.to_lowercase()
    }
    norm(value) == norm(&base.to_string_lossy())
}

/// Разбор по правилам CommandLineToArgvW: кавычки и обратные слэши перед ними значимы.
fn windows_command_line_tokens(s: &str) -> Vec<String> {
    let chars: Vec<char> = s.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let mut token = String::new();
        let mut quoted = false;
        while i < chars.len() {
            if chars[i].is_whitespace() && !quoted {
                break;
            }
            if chars[i] == '\\' {
                let start = i;
                while i < chars.len() && chars[i] == '\\' {
                    i += 1;
                }
                let slashes = i - start;
                if i < chars.len() && chars[i] == '"' {
                    token.extend(std::iter::repeat_n('\\', slashes / 2));
                    if slashes % 2 == 0 {
                        quoted = !quoted;
                    } else {
                        token.push('"');
                    }
                    i += 1;
                } else {
                    token.extend(std::iter::repeat_n('\\', slashes));
                }
                continue;
            }
            if chars[i] == '"' {
                quoted = !quoted;
                i += 1;
                continue;
            }
            token.push(chars[i]);
            i += 1;
        }
        tokens.push(token);
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::{rac_address, SESSIONS_JS};

    #[test]
    fn rac_address_uses_agent_port_for_standard_cluster_manager_address() {
        assert_eq!(rac_address("server:1541"), "server:1540");
        assert_eq!(rac_address("server"), "server");
        assert_eq!(rac_address("server:1540"), "server:1540");
        assert_eq!(rac_address("server:2541"), "server:2541");
    }

    #[test]
    fn session_script_excludes_current_session_by_external_connection_api() {
        assert!(SESSIONS_JS.contains("c.НомерСеансаИнформационнойБазы()"));
        assert!(!SESSIONS_JS.contains("c.НомерСеанса()"));
        assert!(!SESSIONS_JS.contains("ЗавершитьСеанс"));
    }

    #[test]
    fn lock_script_saves_closed_state_before_change_and_restores_original_key() {
        let save = SESSIONS_JS.find("writeLockState(b);").expect("state save");
        let change = SESSIONS_JS.find("b.Установлена = true;").expect("lock change");
        assert!(save < change, "исходное состояние должно сохраняться до изменения");
        assert!(SESSIONS_JS.contains("state.Close();"));
        assert!(SESSIONS_JS.contains("encodeURIComponent(b.КодРазрешения)"));
        assert!(SESSIONS_JS.contains("b.КодРазрешения = env(\"STEADY_ONEC_UC\");"));
        assert!(SESSIONS_JS.contains("restoreLockFromState(c);"));
        assert!(SESSIONS_JS.contains("неподдерживаемое расписание блокировки"));
        assert!(!SESSIONS_JS.contains("say(state.ReadLine())"));
    }
}
