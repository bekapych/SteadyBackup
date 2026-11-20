use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub name: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    pub source: Source,
    pub destinations: Vec<Destination>,
    #[serde(default)]
    pub archive: ArchiveOptions,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub schedule: crate::schedule::Schedule,
    /// Выполнить пропущенный запуск (служба/компьютер были выключены), как только появится возможность.
    #[serde(default = "yes")]
    pub run_missed: bool,
    /// Предельное время работы внешних программ (1С, sqlcmd, PowerShell для Hyper-V, 7-Zip), минут.
    /// Ограничивает каждый запуск отдельно; по истечении процесс и его потомки завершаются,
    /// задание завершается ошибкой. 0 / нет — значение по умолчанию для типа источника
    /// (`Source::default_timeout`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_minutes: Option<u32>,
    /// Группа (`group::Group::id`). По расписанию группы задание запускается вместе с остальными
    /// её заданиями; собственное расписание при этом тоже действует. Нет — задание вне групп.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<Uuid>,
    /// Сколько раз перезапустить задание целиком, если запуск завершился ошибкой
    /// (не отменой). 0 — не перезапускать. Каждый запуск пишется в журнал отдельно.
    #[serde(default)]
    pub restarts: u32,
    /// Пауза перед перезапуском, минут. 0 — сразу (в конец очереди).
    #[serde(default = "default_restart_delay")]
    pub restart_delay_minutes: u32,
    /// Попыток каждого действия внутри запуска: подготовка источника, упаковка 7-Zip,
    /// отправка в каждое назначение. 0 — по умолчанию (`DEFAULT_*_ATTEMPTS`).
    #[serde(default)]
    pub attempts: u32,
}

fn yes() -> bool {
    true
}

fn default_restart_delay() -> u32 {
    5
}

/// Попыток подготовки источника и упаковки, если в задании не задано.
pub const DEFAULT_STEP_ATTEMPTS: u32 = 1;
/// Попыток отправки в назначение, если в задании не задано (прежнее поведение).
pub const DEFAULT_UPLOAD_ATTEMPTS: u32 = 5;

impl Job {
    /// Попыток подготовки источника и упаковки.
    pub fn step_attempts(&self) -> u32 {
        if self.attempts > 0 { self.attempts } else { DEFAULT_STEP_ATTEMPTS }
    }

    /// Попыток отправки в одно назначение.
    pub fn upload_attempts(&self) -> u32 {
        if self.attempts > 0 { self.attempts } else { DEFAULT_UPLOAD_ATTEMPTS }
    }
}

impl Job {
    /// Тайм-аут внешних программ этого задания.
    pub fn process_timeout(&self) -> std::time::Duration {
        match self.timeout_minutes.filter(|m| *m > 0) {
            Some(m) => std::time::Duration::from_secs(m as u64 * 60),
            None => self.source.default_timeout(),
        }
    }
}

impl Source {
    /// Каждую базу упаковывать в отдельный архив.
    pub fn separate_archives(&self) -> bool {
        match self {
            Source::MsSql { separate_archives, .. }
            | Source::PostgreSql { separate_archives, .. }
            | Source::MySql { separate_archives, .. } => *separate_archives,
            _ => false,
        }
    }

    /// Источник сам повторяет каждую базу (`Job::step_attempts`): общий повтор подготовки
    /// для него не нужен, иначе одна неудачная база заново выгружала бы все.
    pub fn retries_per_item(&self) -> bool {
        matches!(self, Source::MsSql { .. } | Source::PostgreSql { .. } | Source::MySql { .. })
    }

    /// Значение по умолчанию с запасом на большие объёмы: зависание должно прерываться,
    /// а обычная долгая работа — нет.
    pub fn default_timeout(&self) -> std::time::Duration {
        let hours = match self {
            // Выгрузка .dt даже больших файловых баз занимает минуты–десятки минут.
            Source::OneC { .. } => 3,
            Source::MsSql { .. } | Source::Firebird { .. } | Source::PostgreSql { .. } | Source::MySql { .. } => 6,
            // Экспорт ВМ на сотни гигабайт.
            Source::HyperV { .. } => 12,
            // Для файлов внешняя программа — только 7-Zip.
            Source::Files { .. } | Source::SshFiles { .. } => 12,
        };
        std::time::Duration::from_secs(hours * 3600)
    }
}

impl Job {
    /// Шифрует DPAPI все пароли, заданные открытым текстом.
    pub fn protect_secrets(&mut self) -> anyhow::Result<()> {
        fn p(v: &mut Option<String>) -> anyhow::Result<()> {
            if let Some(s) = v.as_mut() {
                if !s.is_empty() && !s.starts_with("dpapi:") {
                    *s = crate::secret::protect_str(s)?;
                }
            }
            Ok(())
        }
        match &mut self.source {
            Source::Files { password, .. }
            | Source::OneC { password, .. }
            | Source::MsSql { password, .. }
            | Source::Firebird { password, .. }
            | Source::PostgreSql { password, .. } => p(password)?,
            Source::MySql { ssh, db_password, .. } => {
                p(db_password)?;
                if let Some(s) = ssh {
                    s.protect_secrets()?;
                }
            }
            Source::SshFiles { ssh, .. } => {
                if let Some(s) = ssh {
                    s.protect_secrets()?;
                }
            }
            Source::HyperV { .. } => {}
        }
        for d in &mut self.destinations {
            d.protect_secrets()?;
        }
        p(&mut self.archive.password)
    }

    /// Расшифровывает пароли задания. Нужно для переноса на другой компьютер:
    /// DPAPI привязан к машине и там не откроется.
    pub fn reveal_secrets(&mut self) -> anyhow::Result<()> {
        match &mut self.source {
            Source::Files { password, .. }
            | Source::OneC { password, .. }
            | Source::MsSql { password, .. }
            | Source::Firebird { password, .. }
            | Source::PostgreSql { password, .. } => reveal_opt(password)?,
            Source::MySql { ssh, db_password, .. } => {
                reveal_opt(db_password)?;
                if let Some(s) = ssh {
                    s.reveal_secrets()?;
                }
            }
            Source::SshFiles { ssh, .. } => {
                if let Some(s) = ssh {
                    s.reveal_secrets()?;
                }
            }
            Source::HyperV { .. } => {}
        }
        for d in &mut self.destinations {
            d.reveal_secrets()?;
        }
        reveal_opt(&mut self.archive.password)
    }
}

pub(crate) fn protect_opt(v: &mut Option<String>) -> anyhow::Result<()> {
    if let Some(s) = v.as_mut() {
        if !s.is_empty() && !s.starts_with("dpapi:") {
            *s = crate::secret::protect_str(s)?;
        }
    }
    Ok(())
}

/// Расшифровывает значение на месте. Маркер GUI и пустую строку не трогает.
/// Текст ошибки не содержит само значение.
pub(crate) fn reveal_opt(v: &mut Option<String>) -> anyhow::Result<()> {
    if let Some(s) = v.as_mut() {
        if !s.is_empty() && s != "dpapi:stored" {
            *s = crate::secret::reveal(s)?;
        }
    }
    Ok(())
}

impl Destination {
    pub fn protect_secrets(&mut self) -> anyhow::Result<()> {
        match self {
            Destination::Smb { password, .. } | Destination::WebDav { password, .. } => protect_opt(password),
            Destination::Sftp { password, key_passphrase, .. } => {
                protect_opt(password)?;
                protect_opt(key_passphrase)
            }
            Destination::GoogleDrive { client_secret, refresh_token, .. } => {
                protect_opt(client_secret)?;
                protect_opt(refresh_token)
            }
            Destination::Local { .. } | Destination::Storage { .. } => Ok(()),
        }
    }

    pub fn reveal_secrets(&mut self) -> anyhow::Result<()> {
        match self {
            Destination::Smb { password, .. } | Destination::WebDav { password, .. } => reveal_opt(password),
            Destination::Sftp { password, key_passphrase, .. } => {
                reveal_opt(password)?;
                reveal_opt(key_passphrase)
            }
            Destination::GoogleDrive { client_secret, refresh_token, .. } => {
                reveal_opt(client_secret)?;
                reveal_opt(refresh_token)
            }
            Destination::Local { .. } | Destination::Storage { .. } => Ok(()),
        }
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Destination::Local { .. } => "local",
            Destination::Smb { .. } => "smb",
            Destination::Sftp { .. } => "sftp",
            Destination::WebDav { .. } => "web_dav",
            Destination::GoogleDrive { .. } => "google_drive",
            Destination::Storage { .. } => "storage",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupKind {
    Full,
    Incremental,
    Differential,
}

impl BackupKind {
    pub fn suffix(self) -> &'static str {
        match self {
            BackupKind::Full => "full",
            BackupKind::Incremental => "inc",
            BackupKind::Differential => "diff",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDateField {
    /// Дата последнего изменения содержимого.
    #[default]
    Modified,
    /// Дата создания файла (не поддерживается протоколом SFTP).
    Created,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDateMode {
    /// Не ограничивать файлы по дате.
    #[default]
    All,
    /// Копировать файлы не старше `within_hours` от начала подготовки копии.
    Within,
    /// Копировать файлы начиная с локальной даты и времени `since`.
    Since,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDateFilter {
    #[serde(default)]
    pub field: FileDateField,
    #[serde(default)]
    pub mode: FileDateMode,
    /// Период для `within`, часов. Должен быть больше нуля; по умолчанию сутки.
    #[serde(default = "default_date_within_hours")]
    pub within_hours: u32,
    /// Порог для `since`: `YYYY-MM-DDTHH:MM:SS` в локальном времени или RFC 3339 со смещением.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
}

fn default_date_within_hours() -> u32 {
    24
}

impl Default for FileDateFilter {
    fn default() -> Self {
        Self { field: FileDateField::Modified, mode: FileDateMode::All, within_hours: default_date_within_hours(), since: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Source {
    Files {
        /// Локальные пути, UNC (\\server\share\...) или WebDAV (https://...).
        paths: Vec<String>,
        /// Подключение из раздела «Подключения» (учётные данные WebDAV).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        /// Учётные данные для WebDAV-источников.
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        /// Дополнительный фильтр по дате изменения или создания файла.
        #[serde(default)]
        date_filter: FileDateFilter,
        kind: BackupKind,
        /// Сделать полную копию, если с последней полной прошло столько копий (0 = никогда автоматически).
        #[serde(default)]
        full_every: u32,
    },
    /// Файловая база 1С.
    OneC {
        /// Каталог информационной базы (где лежит 1Cv8.1CD).
        ib_path: String,
        /// Путь к 1cv8.exe. Если пусто — ищем последнюю установленную платформу.
        #[serde(default)]
        platform_exe: Option<String>,
        /// Подключение (пользователь и пароль 1С).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        mode: OneCMode,
        /// Код разрешения запуска (/UC) при установленной блокировке сеансов.
        #[serde(default)]
        unlock_code: Option<String>,
    },
    MsSql {
        /// Подключение (сервер, логин, пароль). Если задано — поля ниже не используются.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        #[serde(default)]
        server: String,
        /// Одна база (прежний формат). Используется, только если `databases` пуст.
        #[serde(default)]
        database: String,
        /// Базы. Пусто вместе с `database` — все пользовательские базы сервера в сети
        /// (без master, model, msdb, tempdb; для журнала — только с моделью FULL/BULK_LOGGED).
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        databases: Vec<String>,
        /// Каждую базу — в отдельный архив (`<набор>.<база>.7z`). Нет — все базы в одном архиве.
        #[serde(default)]
        separate_archives: bool,
        /// None — Windows-аутентификация.
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        kind: MsSqlKind,
        /// Каталог, доступный и SQL Server'у (запись), и службе (чтение). Обычно UNC.
        backup_dir: String,
        #[serde(default = "yes")]
        verify: bool,
    },
    HyperV {
        vm_name: String,
        /// full — всегда полная; incremental — изменённые блоки (RCT) от предыдущей копии.
        #[serde(default = "full_kind")]
        kind: BackupKind,
        /// Полная копия каждые N копий (для incremental).
        #[serde(default)]
        full_every: u32,
        #[serde(default)]
        consistency: HyperVConsistency,
    },
    /// Базы Firebird через gbak. Версия gbak должна совпадать с версией сервера:
    /// 1.5 и 3.0 — разные задания.
    Firebird {
        /// Полный путь к gbak.exe этой версии Firebird.
        gbak: String,
        /// Подключение (пользователь и пароль). Если задано — поля ниже не используются.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
        /// Строки подключения: `localhost:E:\\Shop\\STANDART.FDB` или локальный путь.
        #[serde(default)]
        databases: Vec<String>,
        /// Каталог, чьи подпапки — имена баз. Вместе с `path_template` дополняет `databases`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        names_from: Option<String>,
        /// Шаблон пути, `{name}` заменяется именем подпапки. Например `E:\\{name}\\STANDART.FDB`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path_template: Option<String>,
        /// Префикс сервера для шаблона, обычно `localhost`. Пусто — путь как есть.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host: Option<String>,
        /// После gbak -b выполнить gbak -c во временный файл и удалить его.
        #[serde(default = "yes")]
        verify: bool,
    },
    /// Базы PostgreSQL через pg_dump в собственном формате (`-Fc`), по файлу `<база>.dump` на базу.
    /// Восстановление — `pg_restore`.
    #[serde(rename = "postgresql")]
    PostgreSql {
        /// Подключение (сервер, порт, пользователь, пароль). Если задано — поля ниже не используются.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        /// Сервер. Пусто — `localhost`.
        #[serde(default)]
        host: Option<String>,
        /// Порт. Нет — 5432.
        #[serde(default)]
        port: Option<u16>,
        /// Пусто — `postgres`.
        #[serde(default)]
        user: Option<String>,
        /// Передаётся pg_dump через PGPASSWORD, не в командной строке.
        #[serde(default)]
        password: Option<String>,
        /// Имена баз. Пусто — все базы сервера, к которым разрешено подключение (кроме шаблонов).
        #[serde(default)]
        databases: Vec<String>,
        /// Полный путь к pg_dump.exe. Пусто — самая новая версия из `Program Files\PostgreSQL`.
        /// pg_dump должен быть не старше сервера. Рядом должны лежать psql.exe, pg_restore.exe и pg_dumpall.exe.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pg_dump: Option<String>,
        /// Дополнительно сохранить роли и табличные пространства (`pg_dumpall --globals-only`
        /// в `globals.sql`). Нужны права суперпользователя.
        #[serde(default)]
        globals: bool,
        /// Проверять каждый файл чтением оглавления (`pg_restore --list`).
        #[serde(default = "yes")]
        verify: bool,
        /// Каждую базу — в отдельный архив (`<набор>.<база>.7z`, роли — `<набор>.globals.7z`).
        /// Нет — все базы в одном архиве.
        #[serde(default)]
        separate_archives: bool,
    },
    /// Базы MySQL/MariaDB на сервере Linux: `mysqldump` запускается на сервере по SSH,
    /// дамп `<база>.sql` передаётся по тому же соединению (со сжатием SSH).
    #[serde(rename = "mysql")]
    MySql {
        /// Подключение SSH из раздела «Подключения».
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        /// Параметры SSH. Заполняются из `connection`; заданные прямо в задании используются без него.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<SshLogin>,
        /// Сервер MySQL, как его видит сервер SSH. Пусто — `localhost`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        db_host: Option<String>,
        /// Порт MySQL. Нет — по умолчанию клиента (3306).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        db_port: Option<u16>,
        /// Пользователь MySQL.
        #[serde(default)]
        db_user: String,
        /// Пароль MySQL (DPAPI). На сервер передаётся во временном файле параметров с правами 0600,
        /// не в командной строке.
        #[serde(default)]
        db_password: Option<String>,
        /// Базы. Пусто — все базы, видимые пользователю (без information_schema,
        /// performance_schema, mysql, sys).
        #[serde(default)]
        databases: Vec<String>,
        /// Каждую базу — в отдельный архив (`<набор>.<база>.7z`). Нет — все базы в одном архиве.
        #[serde(default)]
        separate_archives: bool,
    },
    /// Файлы с сервера по SSH (SFTP): каталоги сайта и т. п. Полная, инкрементная или
    /// дифференциальная копия — как у источника `files` (по размеру и времени изменения).
    SshFiles {
        /// Подключение SSH из раздела «Подключения».
        #[serde(default, skip_serializing_if = "Option::is_none")]
        connection: Option<String>,
        /// Параметры SSH. Заполняются из `connection`; заданные прямо в задании используются без него.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ssh: Option<SshLogin>,
        /// Каталоги или файлы на сервере: `/var/www/site`. Относительный путь — от домашнего каталога.
        paths: Vec<String>,
        #[serde(default)]
        include: Vec<String>,
        #[serde(default)]
        exclude: Vec<String>,
        /// По SFTP доступна дата изменения; протокол не передаёт дату создания.
        #[serde(default)]
        date_filter: FileDateFilter,
        #[serde(default = "full_kind")]
        kind: BackupKind,
        /// Полная копия каждые N копий (0 — никогда автоматически).
        #[serde(default)]
        full_every: u32,
    },
}

/// Вход на сервер SSH. Общий для источников «MySQL по SSH» и «Файлы по SSH».
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SshLogin {
    pub host: String,
    #[serde(default = "ssh_port")]
    pub port: u16,
    pub user: String,
    /// DPAPI.
    #[serde(default)]
    pub password: Option<String>,
    /// Закрытый ключ (OpenSSH/PEM). Пробуется раньше пароля.
    #[serde(default)]
    pub key_file: Option<String>,
    /// Пароль к ключу (DPAPI).
    #[serde(default)]
    pub key_passphrase: Option<String>,
    /// Отпечаток ключа сервера "SHA256:...". Не задан — принимается любой, с предупреждением.
    #[serde(default)]
    pub host_key: Option<String>,
}

impl SshLogin {
    pub fn protect_secrets(&mut self) -> anyhow::Result<()> {
        protect_opt(&mut self.password)?;
        protect_opt(&mut self.key_passphrase)
    }

    pub fn reveal_secrets(&mut self) -> anyhow::Result<()> {
        reveal_opt(&mut self.password)?;
        reveal_opt(&mut self.key_passphrase)
    }
}

fn full_kind() -> BackupKind {
    BackupKind::Full
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HyperVConsistency {
    /// Через VSS внутри гостя (службы интеграции); при неудаче — аварийно-согласованная.
    #[default]
    Application,
    /// Без участия гостя (как при внезапном отключении питания).
    Crash,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OneCMode {
    /// Выгрузка .dt средствами платформы (DESIGNER /DumpIB).
    #[default]
    DumpDt,
    /// Копирование файла 1Cv8.1CD.
    CopyFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MsSqlKind {
    Full,
    Differential,
    Log,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Destination {
    Local {
        path: String,
    },
    Smb {
        path: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    Sftp {
        host: String,
        #[serde(default = "ssh_port")]
        port: u16,
        user: String,
        #[serde(default)]
        password: Option<String>,
        #[serde(default)]
        key_file: Option<String>,
        /// Пароль к ключу.
        #[serde(default)]
        key_passphrase: Option<String>,
        /// Отпечаток ключа сервера "SHA256:...". Если не задан — принимается любой (с записью в лог).
        #[serde(default)]
        host_key: Option<String>,
        path: String,
        /// Команда на сервере после успешной отправки (SSH).
        #[serde(default)]
        post_command: Option<String>,
    },
    WebDav {
        url: String,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        password: Option<String>,
    },
    GoogleDrive {
        /// Устаревшее: имя учётной записи из `<data>\gdrive\<account>.json` (команда `steady gdrive-auth`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        /// Путь к папке на Диске, напр. "SteadyBackup/PayDesk". Создаётся автоматически.
        #[serde(default)]
        folder: String,
        #[serde(default)]
        client_id: Option<String>,
        /// DPAPI.
        #[serde(default)]
        client_secret: Option<String>,
        /// DPAPI.
        #[serde(default)]
        refresh_token: Option<String>,
        /// Адрес аккаунта Google (для отображения).
        #[serde(default)]
        email: Option<String>,
    },
    /// Ссылка на хранилище из раздела «Хранилища» + подпапка внутри него.
    Storage {
        storage: String,
        #[serde(default)]
        folder: String,
    },
}

fn ssh_port() -> u16 {
    22
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveMode {
    /// Один общий архив на весь запуск задания.
    #[default]
    Single,
    /// Отдельный архив на каждый файл файлового источника.
    PerFile,
    /// Отдельный архив на каждый путь (строку) файлового источника.
    PerSource,
    /// На каждую вложенную папку первого уровня каждого пути; файлы корня пути — вместе.
    PerDirectory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveOptions {
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Разбиение файлового источника на архивы. Старые задания — один общий архив.
    #[serde(default)]
    pub mode: ArchiveMode,
    /// 0..9
    #[serde(default = "default_level")]
    pub level: u8,
    #[serde(default)]
    pub password: Option<String>,
    /// Шифровать имена файлов (-mhe=on).
    #[serde(default = "yes")]
    pub encrypt_headers: bool,
    /// Размер тома, напр. "4g". None — один файл.
    #[serde(default)]
    pub volume_size: Option<String>,
    #[serde(default = "yes")]
    pub test_after: bool,
}

fn default_level() -> u8 {
    5
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ArchiveMode::Single,
            level: default_level(),
            password: None,
            encrypt_headers: true,
            volume_size: None,
            test_after: true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Retention {
    /// Сколько последних полных копий хранить вместе с их цепочками: инкрементные и
    /// дифференциальные не старше самой старой из сохранённых полных. 0 — это правило выключено.
    #[serde(default)]
    pub keep_full: u32,
    /// Удалять наборы старше стольких дней. Не трогает цепочку, которую удерживает `keep_full`
    /// (всё не старше самой старой сохранённой полной). Нужен заданиям без полных копий
    /// (журнал и разностные копии MSSQL: под своим именем полных нет, и `keep_full` ничего не удаляет).
    /// 0 — не удалять по сроку.
    #[serde(default)]
    pub keep_days: u32,
    /// Оставлять не больше стольких последних наборов среди тех, что не удерживает `keep_full`.
    /// 0 — не ограничивать число.
    #[serde(default)]
    pub keep_sets: u32,
}

impl Retention {
    /// Есть ли хоть одно включённое правило. Иначе назначение даже не перечисляется.
    pub fn rotates(&self) -> bool {
        self.keep_full > 0 || self.keep_days > 0 || self.keep_sets > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn job(source: &str) -> Job {
        let json = format!(
            r#"{{"id":"11111111-1111-1111-1111-111111111111","name":"t","source":{source},"destinations":[]}}"#
        );
        serde_json::from_str(&json).unwrap()
    }

    /// Старые файлы заданий без поля читаются; тайм-аут берётся по типу источника.
    #[test]
    fn old_job_without_timeout_uses_source_default() {
        let j = job(r#"{"type":"one_c","ib_path":"C:\\ib"}"#);
        assert!(j.timeout_minutes.is_none());
        assert_eq!(j.process_timeout(), Duration::from_secs(3 * 3600));

        let j = job(r#"{"type":"ms_sql","database":"d","kind":"full","backup_dir":"C:\\b"}"#);
        assert_eq!(j.process_timeout(), Duration::from_secs(6 * 3600));

        let j = job(r#"{"type":"hyper_v","vm_name":"vm"}"#);
        assert_eq!(j.process_timeout(), Duration::from_secs(12 * 3600));

        let j = job(r#"{"type":"files","paths":["C:\\x"],"kind":"full"}"#);
        assert_eq!(j.process_timeout(), Duration::from_secs(12 * 3600));
    }

    #[test]
    fn timeout_minutes_overrides_default_zero_does_not() {
        let mut j: Job = serde_json::from_str(
            r#"{"id":"11111111-1111-1111-1111-111111111111","name":"t","timeout_minutes":90,"source":{"type":"one_c","ib_path":"C:\\ib"},"destinations":[]}"#,
        )
        .unwrap();
        assert_eq!(j.process_timeout(), Duration::from_secs(90 * 60));
        j.timeout_minutes = Some(0);
        assert_eq!(j.process_timeout(), Duration::from_secs(3 * 3600));
        j.timeout_minutes = None;
        assert_eq!(j.process_timeout(), Duration::from_secs(3 * 3600));
    }

    #[test]
    fn old_file_job_has_date_filter_disabled() {
        let j = job(r#"{"type":"files","paths":["C:\\x"],"kind":"full"}"#);
        match j.source {
            Source::Files { date_filter, .. } => {
                assert_eq!(date_filter.mode, FileDateMode::All);
                assert_eq!(date_filter.field, FileDateField::Modified);
                assert_eq!(date_filter.within_hours, 24);
                assert!(date_filter.since.is_none());
            }
            _ => panic!("ожидался файловый источник"),
        }
    }

    #[test]
    fn old_archive_options_use_one_common_archive() {
        let j = job(r#"{"type":"files","paths":["C:\\x"],"kind":"full"}"#);
        assert_eq!(j.archive.mode, ArchiveMode::Single);
    }

    /// Старые задания: без перезапусков, прежние 5 попыток отправки и одна — остального.
    #[test]
    fn old_job_keeps_previous_retry_behaviour() {
        let mut j = job(r#"{"type":"files","paths":["C:\\x"],"kind":"full"}"#);
        assert_eq!((j.restarts, j.restart_delay_minutes, j.attempts), (0, 5, 0));
        assert_eq!((j.step_attempts(), j.upload_attempts()), (1, 5));
        j.attempts = 3;
        assert_eq!((j.step_attempts(), j.upload_attempts()), (3, 3));
    }
}
