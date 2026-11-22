//! Платформа 1С: пакетный DESIGNER, свойства конфигурации, .dt и шаги обновления.

use super::util::{decode_text, Version};
use crate::pipeline::safe_name;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Информационная база: каталог файловой базы или `сервер\имя_базы`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    File(PathBuf),
    Server { server: String, name: String },
}

impl Target {
    pub fn parse(s: &str) -> Target {
        let s = s.trim();
        let path = Path::new(s);
        // Абсолютный путь остаётся файловым даже если каталог ещё не существует/опечатан:
        // иначе `C:\missing\ib` ошибочно превращался бы в серверную базу.
        if path.is_absolute() || path.is_dir() || path.join("1Cv8.1CD").is_file() {
            return Target::File(path.to_path_buf());
        }
        let (server, name) = s.rsplit_once(['\\', '/']).unwrap_or((s, ""));
        Target::Server {
            server: server.to_string(),
            name: name.to_string(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self {
            Target::File(p) if !p.join("1Cv8.1CD").is_file() => {
                bail!("в каталоге {} нет файловой базы 1Cv8.1CD", p.display());
            }
            Target::Server { server, name }
                if server.trim().is_empty() || name.trim().is_empty() =>
            {
                bail!("база 1С должна быть каталогом файловой базы или строкой сервер\\имя");
            }
            _ => Ok(()),
        }
    }

    pub fn short_name(&self) -> String {
        match self {
            Target::File(p) => p
                .file_name()
                .map(|n| safe_name(&n.to_string_lossy()))
                .unwrap_or_else(|| "base".into()),
            Target::Server { server, name } => safe_name(&format!(r"{server}\{name}")),
        }
    }

    pub fn server_string(&self) -> Option<String> {
        match self {
            Target::Server { server, name } => Some(format!(r"{server}\{name}")),
            Target::File(_) => None,
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::File(p) => write!(f, "файловая база {}", p.display()),
            Target::Server { server, name } => write!(f, r"серверная база {server}\{name}"),
        }
    }
}

/// Имя постоянной копии: безопасная база, исходная версия и штамп времени.
pub fn backup_file_name(target: &Target, current: &Version, stamp: &str) -> String {
    format!(
        "{}_{}_{stamp}.dt",
        target.short_name(),
        safe_name(&current.to_string())
    )
}

/// Свойства основной конфигурации из `Configuration.xml`.
#[derive(Clone, Debug, Default)]
pub struct ConfigInfo {
    pub name: String,
    pub synonym: String,
    pub vendor: String,
    pub version: String,
}

impl ConfigInfo {
    pub fn from_xml(xml: &str) -> Result<ConfigInfo> {
        let props = between(xml, "<Properties>", "</Properties>")
            .ok_or_else(|| anyhow!("в Configuration.xml нет раздела Properties"))?;
        let synonym = between(props, "<Synonym>", "</Synonym>")
            .and_then(|s| between(s, "<v8:content>", "</v8:content>"))
            .unwrap_or("");
        Ok(ConfigInfo {
            name: xml_unescape(tag(props, "Name")),
            synonym: xml_unescape(synonym),
            vendor: xml_unescape(tag(props, "Vendor")),
            version: xml_unescape(tag(props, "Version")),
        })
    }

    pub fn title(&self) -> &str {
        if self.synonym.is_empty() {
            &self.name
        } else {
            &self.synonym
        }
    }
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let a = s.find(open)? + open.len();
    let b = s[a..].find(close)? + a;
    Some(&s[a..b])
}

fn tag<'a>(s: &'a str, name: &str) -> &'a str {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let empty = format!("<{name}/>");
    match (s.find(&open), s.find(&empty)) {
        (Some(a), Some(e)) if e < a => "",
        (Some(_), _) => between(s, &open, &close).unwrap_or(""),
        _ => "",
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .trim()
        .to_string()
}

/// Подключение к базе через конкретную платформу.
pub struct Platform {
    pub exe: PathBuf,
    pub target: Target,
    pub user: String,
    pub password: String,
    pub uc: String,
    pub work: PathBuf,
    pub timeout: Duration,
}

pub struct RunResult {
    pub code: Option<i32>,
    pub log: String,
}

impl RunResult {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }
}

/// Разрешает и обязательно проверяет доверенный 1cv8.exe до любого запуска.
pub fn resolve_platform(spec: Option<&str>) -> Result<PathBuf> {
    let exe = match spec.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => PathBuf::from(s),
        None => super::super::onec::find_platform()
            .context("не найдена платформа 1С (1cv8.exe); укажите путь в задании")?,
    };
    crate::acl::check_trusted_exe(&exe).context("1С: платформа обновления")?;
    Ok(exe)
}

impl Platform {
    fn command(&self, mode: &str) -> Command {
        let mut cmd = Command::new(&self.exe);
        cmd.arg(mode);
        match &self.target {
            Target::File(p) => cmd.arg("/F").arg(p),
            Target::Server { .. } => cmd
                .arg("/S")
                .arg(self.target.server_string().unwrap_or_default()),
        };
        if !self.user.is_empty() {
            cmd.arg("/N").arg(&self.user);
        }
        // Платформа принимает пароль и код только аргументами /P и /UC — известное ограничение.
        if !self.password.is_empty() {
            cmd.arg("/P").arg(&self.password);
        }
        if !self.uc.is_empty() {
            cmd.arg("/UC").arg(&self.uc);
        }
        cmd.args(["/DisableStartupDialogs", "/DisableStartupMessages"]);
        cmd
    }

    pub fn run(&self, mode: &str, args: &[&std::ffi::OsStr]) -> Result<RunResult> {
        std::fs::create_dir_all(&self.work)?;
        let log = self
            .work
            .join(format!("1cv8-{}.log", uuid::Uuid::new_v4().simple()));
        let mut cmd = self.command(mode);
        cmd.args(args).arg("/Out").arg(&log);
        let out = crate::process::run(cmd, self.timeout, "1С: обновление")?;
        let raw = std::fs::read(&log).unwrap_or_default();
        let _ = std::fs::remove_file(&log);
        Ok(RunResult {
            code: out.status.code(),
            log: decode_text(&raw).trim().to_string(),
        })
    }

    fn designer(&self, args: &[&std::ffi::OsStr]) -> Result<RunResult> {
        self.run("DESIGNER", args)
    }

    /// Свойства основной конфигурации: выгружается только `Configuration.xml`.
    pub fn read_config(&self) -> Result<ConfigInfo> {
        let dir = self
            .work
            .join(format!("cfg-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir)?;
        let list = self
            .work
            .join(format!("list-{}.txt", uuid::Uuid::new_v4().simple()));
        std::fs::write(&list, "Configuration\r\n")?;
        let result = self.designer(&[
            "/DumpConfigToFiles".as_ref(),
            dir.as_os_str(),
            "-listFile".as_ref(),
            list.as_os_str(),
        ]);
        let xml = dir.join("Configuration.xml");
        let r = result?;
        if !r.ok() || !xml.is_file() {
            let _ = std::fs::remove_dir_all(&dir);
            let _ = std::fs::remove_file(&list);
            bail!(
                "не удалось прочитать свойства конфигурации ({})",
                explain(&r)
            );
        }
        let raw = std::fs::read(&xml)?;
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&list);
        ConfigInfo::from_xml(&decode_text(&raw))
    }

    pub fn dump_ib(&self, out: &Path) -> Result<RunResult> {
        let r = self.designer(&["/DumpIB".as_ref(), out.as_os_str()])?;
        let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
        if !r.ok() || size == 0 {
            bail!("выгрузка в .dt не выполнена ({})", explain(&r));
        }
        Ok(r)
    }

    pub fn update(&self, package: &Path, force: bool, server_side: bool) -> Result<RunResult> {
        let mut args: Vec<&std::ffi::OsStr> = vec!["/UpdateCfg".as_ref(), package.as_os_str()];
        if force {
            args.push("-force".as_ref());
        }
        args.push("/UpdateDBCfg".as_ref());
        if server_side {
            args.push("-Server".as_ref());
        }
        let r = self.designer(&args)?;
        if !r.ok() {
            bail!(
                "обновление из {} не выполнено ({})",
                package.display(),
                explain(&r)
            );
        }
        Ok(r)
    }

    /// Имя COM-класса выбранной платформы: V83.COMConnector или V85.COMConnector.
    pub fn com_progid(&self) -> Option<String> {
        let ver = self
            .exe
            .parent()
            .and_then(Path::parent)
            .and_then(|d| d.file_name())
            .and_then(|n| Version::parse(&n.to_string_lossy()))?;
        Some(format!("V{}{}.COMConnector", ver.0.first()?, ver.0.get(1)?))
    }

    /// Пароль передаётся JScript через окружение процесса, а не записывается в сценарий.
    pub fn connection_string(&self) -> String {
        let q = |s: &str| s.replace('"', "\"\"");
        let mut c = match &self.target {
            Target::File(p) => format!("File=\"{}\";", q(&p.display().to_string())),
            Target::Server { server, name } => {
                format!("Srvr=\"{}\";Ref=\"{}\";", q(server), q(name))
            }
        };
        if !self.user.is_empty() {
            c.push_str(&format!("Usr=\"{}\";", q(&self.user)));
        }
        if !self.password.is_empty() {
            c.push_str(&format!("Pwd=\"{}\";", q(&self.password)));
        }
        if !self.uc.is_empty() {
            c.push_str(&format!("UC=\"{}\";", q(&self.uc)));
        }
        c
    }
}

pub fn explain(r: &RunResult) -> String {
    let code = r.code.map(|c| c.to_string()).unwrap_or_else(|| "—".into());
    if r.log.is_empty() {
        format!("код {code}, журнал пуст")
    } else {
        format!("код {code}: {}", r.log)
    }
}
