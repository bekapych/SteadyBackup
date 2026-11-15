# SteadyBackup: задания и командная строка

## Команды

```
steady install                      установить и запустить службу (от администратора)
steady uninstall                    удалить службу
steady import job.json              добавить/обновить задание (в %ProgramData%\SteadyBackup\jobs)
steady export backup.json           все задания, хранилища, подключения и настройки в один файл
steady import-settings backup.json  загрузить такой файл (--replace удаляет то, чего в файле нет)
steady jobs                         список заданий и ближайший запуск
steady ipc "{\"cmd\":\"run_job\",\"job_id\":\"...\"}"   команда работающей службе
steady run job.json                 выполнить задание сразу, без службы
steady test-dest job.json           проверить подключение к назначениям
steady protect "пароль"             зашифровать пароль (DPAPI) → dpapi:...
steady gdrive-auth <имя> --client-id ... --client-secret ...   доступ к Google Drive
steady console                      служба в консоли (отладка)
steady restore a.7z b.7z --target D:\Restore   файлы: полная, затем инкременты; --password при необходимости
```

Любой пароль в задании можно указать открытым текстом или как `dpapi:...` из `steady protect`.
Значение DPAPI привязано к компьютеру: расшифровать его можно только на той же машине.

`steady export` пишет пароли открытым текстом: иначе файл не откроется на другом компьютере.
После `import-settings` они снова шифруются DPAPI. Журнал и каталоги инкрементных копий не переносятся,
следующая инкрементная копия начинается с полной. Ключ SFTP, если файл был прочитан, кладётся в
`<data>\keys`, путь в задании меняется. Команда доступна только администратору; содержимое файла
в журнал не пишется.

IPC-команды: `status`, `list_jobs`, `get_job {job_id}`, `save_job {job}`, `delete_job {job_id}`,
`copy_job {job_id}`, `export_settings`, `import_settings {text, replace}`,
`run_job {job_id}`, `cancel_job {job_id}`, `history {limit}`, `protect {secret}`, `get_settings`, `save_settings {work_dir}`,
Снимки окон без вашей службы: `steady --data-dir <временный каталог> console` с переменной
`STEADY_PIPE=<имя>` и GUI с той же переменной (`steady-gui --shots <каталог>`). В консольном режиме GUI
того же пользователя получает права администратора, а задания этого пользователя загружаются;
служба Windows так не делает.

`list_databases {source}` — базы сервера для источника `ms_sql`, `postgresql` или `mysql` (как в задании: `connection`
или встроенные поля); заодно проверяет подключение. Только администраторам, тайм-аут 60 с.

### Обновление программы

`settings.json`, поле `update` (по умолчанию `{"check": true, "auto_install": false}`):

* `check` — служба раз в сутки (первый раз через 2 минуты после старта) запрашивает
  `api.github.com/repos/bekapych/SteadyBackup/releases/latest`. Предварительные выпуски и черновики пропускаются.
* `auto_install` — найденную новую версию служба скачивает и ставит сама, когда нет выполняющихся,
  стоящих в очереди и ждущих перезапуска заданий (проверка каждые 10 минут).

Установщик `SteadyBackup-<версия>.msi` скачивается в `<data>\updates` (только SYSTEM и Администраторы),
сверяется по размеру и SHA-256 из выпуска (`digest` вложения; без суммы установка запрещена) и запускается
`msiexec /i … /qn /norestart /l*v <data>\updates\install-<версия>.log`. Пока идёт установка, новые задания
в очередь не встают, планировщик метки не двигает (подошедший запуск новая служба выполнит как пропущенный).
Перед запуском пишется `updates\pending.json`; новая служба сверяет версию, записывает итог
(`last_result`) и запускает окно в трее в активных сеансах. Не удавшуюся версию автоматически
повторно не ставит. Состояние — `updates\state.json`. В режиме `steady console` установка запрещена.

IPC: `update_status` (доступна всем: версии, настройки, фаза `idle|checking|downloading|installing`, итог),
`check_update`, `install_update` (в фоне), `save_update_settings {settings}` — только администраторам.
`status` содержит `update: {available, phase}`.

`work_dir` — каталог staging и архива. Пусто — `%ProgramData%\SteadyBackup\work`. Свой путь должен быть
абсолютным и не может быть корнем диска или каталогом данных: при старте служба удаляет оттуда только
недоделанные каталоги заданий (`<id>_<32 hex>`), чужие файлы не трогает. На свой каталог ставятся те же
права, что на каталог данных.

## Группы заданий

Группа — общее расписание для нескольких заданий: `<data>\groups\<id>.json`.

```json
{ "id": "7a000000-0000-0000-0000-000000000001", "name": "Ночные копии", "enabled": true,
  "schedule": { "type": "daily", "times": ["01:00:00"] }, "run_missed": true,
  "order": ["6f1c0000-0000-0000-0000-000000000002", "6f1c0000-0000-0000-0000-000000000001"] }
```

Задание входит не больше чем в одну группу — поле задания `"group": "<id группы>"` (нет — без группы).
По расписанию группы её **включённые** задания ставятся в очередь по порядку `order` (не упомянутые там —
после, по имени) и выполняются по одному; ошибка одного не отменяет остальные. Собственное расписание
задания продолжает действовать: чтобы задание шло только с группой, поставьте ему `"type": "manual"`.
Группа может задать `"restarts"`, `"restart_delay_minutes"`, `"attempts"` (как у задания): при запуске
группы ненулевые значения заменяют значения её заданий, `0` — «как в задании».
Удаление группы задания не удаляет — у них снимается ссылка. Группы переносятся экспортом настроек.

IPC: `list_groups` (состав, расписание, `next_run`), `save_group {group, jobs}` (`jobs` — состав и порядок,
нулевой `id` — новая группа), `delete_group {group_id}`, `run_group {group_id}`.

## Расписание

```json
"schedule": { "type": "daily",    "times": ["01:00:00", "13:00:00"] }
"schedule": { "type": "weekly",   "days": ["Mon","Wed","Fri"], "times": ["22:00:00"] }
"schedule": { "type": "monthly",  "days": [1, 15, 31], "times": ["03:00:00"] }      // 31 = последний день
"schedule": { "type": "interval", "minutes": 60, "from": "08:00:00", "to": "20:00:00" }
"schedule": { "type": "once",     "at": "2026-12-31T23:00:00" }
"schedule": { "type": "manual" }
```
`"run_missed": true` (по умолчанию): если компьютер был выключен во время запуска, задание выполнится после включения.

Повторы при ошибке (страница расписания, блок «При ошибке»):

* `"restarts"` — сколько раз перезапустить задание целиком после ошибки (по умолчанию `0`). Отменённое вручную не перезапускается. Каждый запуск — отдельная запись журнала; уведомление — по последнему.
* `"restart_delay_minutes"` — пауза перед перезапуском (по умолчанию `5`, `0` — сразу). Перезапуски ждут в памяти службы: её перезапуск их сбрасывает.
* `"attempts"` — попыток каждого действия в одном запуске: подготовка источника (выгрузка 1С, BACKUP SQL, pg_dump, экспорт ВМ), упаковка 7-Zip, отправка в каждое назначение. `0` — по умолчанию: отправка 5 попыток, остальное 1.

```json
"restarts": 2, "restart_delay_minutes": 10, "attempts": 3
```

`"timeout_minutes"`: предельное время одного запуска внешней программы (1С, sqlcmd, PowerShell для Hyper-V, упаковка 7-Zip), в минутах. Нет или `0` — значение по умолчанию для типа источника: 1С — 3 часа, MS SQL, Firebird, PostgreSQL и MySQL (каждый `mysqldump`) — 6 часов, файлы (в т. ч. по SSH) и Hyper-V — 12 часов. По истечении процесс и его потомки завершаются, задание завершается ошибкой с указанием этапа. В GUI — поле «Прервать, если длится дольше» на странице расписания.

## Файловая база 1С (выгрузка .dt)
```json
{
  "id": "6f1c0000-0000-0000-0000-000000000001",
  "name": "PayDesk",
  "source": { "type": "one_c", "ib_path": "C:\\1C\\PayDesk", "user": "", "password": "", "mode": "dump_dt" },
  "destinations": [ { "type": "smb", "path": "\\\\nas\\backup\\1c", "user": "nas\\backup", "password": "dpapi:..." } ],
  "archive": { "password": "dpapi:...", "level": 5 },
  "retention": { "keep_full": 14 },
  "schedule": { "type": "daily", "times": ["23:30:00"] }
}
```
`platform_exe` можно не указывать: берётся самая новая установленная платформа.
Если в базе включена блокировка сеансов, код разрешения указывается в `unlock_code`.

## MS SQL
```json
{
  "id": "6f1c0000-0000-0000-0000-000000000002",
  "name": "ERP_SQL",
  "source": { "type": "ms_sql", "server": "SQL01", "database": "ERP", "kind": "full", "backup_dir": "\\\\SQL01\\Backup$" },
  "destinations": [ { "type": "local", "path": "E:\\Backup\\SQL" } ],
  "archive": { "password": "dpapi:...", "level": 3, "volume_size": "4g" },
  "retention": { "keep_full": 7 },
  "schedule": { "type": "daily", "times": ["02:00:00"] }
}
```
`kind`: `full` | `differential` | `log`.

Несколько баз — поле `databases` (тогда `database` не используется):

```json
"source": { "type": "ms_sql", "connection": "sql01", "databases": ["ERP", "CRM"], "separate_archives": true,
            "kind": "full", "backup_dir": "\\\\SQL01\\Backup$" }
```

* `databases` и `database` пусты — все пользовательские базы сервера в сети (без `master`, `model`, `msdb`,
  `tempdb`, снимков; для `log` — без баз с моделью `SIMPLE`). Новые базы подхватываются сами.
* `separate_archives` (по умолчанию `false`) — каждая база в своём архиве
  `<задание>_<дата>_<время>_<вид>.<база>.7z`; иначе все `.bak` в одном архиве. То же поле есть у PostgreSQL
  (роли — `<…>.globals.7z`). Ротация считает архивы одного запуска одним набором.
* Если часть баз не сохранилась, остальные отправляются, а запуск завершается ошибкой (с перезапусками,
  если они заданы). `attempts` задания — попыток на каждую базу.

## Firebird (gbak)

Версия `gbak.exe` должна совпадать с сервером. Firebird 1.5 и 3.0 — два задания. В архив попадает `.fbk`
(не восстановленный файл базы и не RAR). Пароль задаётся подключением и передаётся gbak через
`ISC_PASSWORD`, не аргументом командной строки. Выгрузка на WebDAV — обычное назначение задания.

```json
{
  "id": "6f1c0000-0000-0000-0000-000000000004",
  "name": "IP2 Firebird 3",
  "source": {
    "type": "firebird",
    "gbak": "C:\\Program Files (x86)\\Firebird\\Firebird_3_0\\gbak.exe",
    "connection": "firebird-sysdba",
    "names_from": "E:\\Backup\\ip2",
    "path_template": "E:\\{name}\\STANDART.FD3",
    "host": "localhost",
    "verify": true
  },
  "destinations": [ { "type": "web_dav", "url": "https://webdav.cloud.mail.ru/SaveBackup", "user": "user@mail.ru", "password": "dpapi:..." } ],
  "retention": { "keep_full": 14 },
  "schedule": { "type": "daily", "times": ["02:30:00"] }
}
```

Для 1.5 тот же шаблон, другой `gbak` (`Firebird_1_5\\bin\\gbak.exe`) и `STANDART.FDB`.
`{name}` — имя подпапки в `names_from`. Явный список — поле `databases`, по одной строке
`localhost:E:\\Shop\\STANDART.FDB`.

## PostgreSQL (pg_dump)

Каждая база — отдельный файл `<база>.dump` в формате `pg_dump -Fc` (уже сжат, 7-Zip только упаковывает).
Восстановление: `pg_restore -d <база> <файл>.dump`. Пароль задаётся подключением (тип `postgresql`:
`host`, `port`, `user`, `password`) и передаётся через `PGPASSWORD`, не аргументом командной строки.

```json
{
  "id": "6f1c0000-0000-0000-0000-000000000005",
  "name": "PG_ERP",
  "source": {
    "type": "postgresql",
    "connection": "pg-local",
    "databases": ["erp", "crm"],
    "globals": true,
    "verify": true
  },
  "destinations": [ { "type": "local", "path": "E:\\Backup\\PG" } ],
  "retention": { "keep_full": 14 },
  "schedule": { "type": "daily", "times": ["03:00:00"] }
}
```

* `databases` пусто — все базы сервера, к которым разрешено подключение (кроме шаблонов).
* `pg_dump` — полный путь к `pg_dump.exe`; пусто — самая новая версия в `Program Files\PostgreSQL\<версия>\bin`.
  Версия должна быть не старше сервера. `psql.exe`, `pg_restore.exe`, `pg_dumpall.exe` берутся из того же
  каталога; все проверяются на права записи (запуск от SYSTEM).
* `separate_archives` — каждая база в отдельном архиве (см. раздел MS SQL).
* `globals` — роли и табличные пространства (`pg_dumpall --globals-only` → `globals.sql`), нужен суперпользователь.
* `verify` (по умолчанию `true`) — `pg_restore --list` по каждому файлу.
* Вместо `connection` можно указать в источнике `host`, `port`, `user`, `password`. По умолчанию
  `localhost`, `5432`, `postgres`. Тайм-аут по умолчанию — 6 часов на каждый запуск утилиты.

`retention.keep_full` удаляет только лишние полные цепочки. У задания с `kind` `differential` или `log` полных копий под своим именем нет, поэтому одно это поле ничего не удаляет. Для них (и для хвоста старше сохранённой полной) — `keep_days` (удалять наборы старше стольких дней) и `keep_sets` (оставить не больше стольких последних наборов). `0` — правило выключено. Цепочку, которую удерживает `keep_full`, срок и число не трогают. Старый JSON без этих полей читается как прежде.

## Сайт на сервере Linux: MySQL и файлы по SSH

Сервер SSH задаётся подключением типа `ssh` (раздел «Подключения»):

```json
{ "id": "site-ru", "name": "site.ru", "type": "ssh", "host": "site.ru", "port": 22, "user": "deploy",
  "password": "dpapi:...", "key_file": null, "key_passphrase": null, "host_key": "SHA256:..." }
```

`key_file` — закрытый ключ OpenSSH/PEM (пробуется раньше пароля). `host_key` — отпечаток сервера; не задан —
принимается любой, а в итоге запуска появляется предупреждение с отпечатком. Вместо `connection` можно
указать в источнике объект `"ssh": { host, port, user, password, key_file, key_passphrase, host_key }`.

**Базы MySQL / MariaDB** (`mysqldump` или `mariadb-dump` на сервере; дамп `<база>.sql` идёт по SSH со сжатием):

```json
{
  "id": "6f1c0000-0000-0000-0000-000000000006",
  "name": "site.ru БД",
  "source": { "type": "mysql", "connection": "site-ru", "db_user": "site", "db_password": "dpapi:...",
              "databases": ["site"], "separate_archives": false },
  "destinations": [ { "type": "storage", "storage": "nas", "folder": "site.ru" } ],
  "retention": { "keep_full": 14 },
  "schedule": { "type": "daily", "times": ["03:00:00"] }
}
```

* `db_host` (пусто — `localhost`, как его видит сервер SSH), `db_port` (нет — 3306; с `localhost` включает TCP).
* `databases` пусто — все базы, видимые пользователю, кроме `information_schema`, `performance_schema`,
  `mysql`, `sys`. `separate_archives` — как у MS SQL.
* Ключи `mysqldump`: `--single-transaction --quick --routines --triggers --hex-blob --no-tablespaces
  --databases`. Восстановление: `mysql -u … -p < база.sql` (в дампе есть `CREATE DATABASE`/`USE`).
* Пароль БД передаётся команде через stdin и на время дампа лежит на сервере во временном файле
  с правами 0600 (`--defaults-extra-file`); в командной строке сервера его нет.
* Дамп без последней строки `-- Dump completed` считается оборванным. Каждая база повторяется
  `attempts` раз через новое соединение; несохранённые базы — ошибка запуска, остальные отправляются.

**Файлы по SSH** (SFTP; полная, инкрементная, дифференциальная — как у `files`):

```json
"source": { "type": "ssh_files", "connection": "site-ru", "paths": ["/var/www/site.ru"],
            "exclude": ["cache/**", "*.log"], "kind": "incremental", "full_every": 7 }
```

Относительный путь — от домашнего каталога пользователя SSH. Символические ссылки, сокеты и пустые
каталоги не копируются. Обрыв соединения — ошибка подготовки (повтор по `attempts`); ошибка чтения
отдельного файла — предупреждение, файл попадёт в следующую копию.

Полная копия сайта — два задания (базы и файлы) в одной группе с общим расписанием.

IPC: `list_databases {source}` работает и для `mysql` (нужен `db_user`); `check_ssh_paths {source}` —
вход на сервер и наличие каждого пути источника `ssh_files` (`{target, host_key, paths:[{path, ok, dir|error}]}`),
только администраторам.

## Файлы: локальные, сетевые, WebDAV (инкрементная, полная каждые 7 копий)
```json
{
  "id": "6f1c0000-0000-0000-0000-000000000003",
  "name": "Docs",
  "source": { "type": "files",
              "paths": ["D:\\Docs", "\\\\srv\\share\\Projects", "https://cloud.example.ru/remote.php/dav/files/user/Work"],
              "user": "user", "password": "dpapi:...",
              "exclude": ["*.tmp", "~$*", "**/node_modules/**"], "kind": "incremental", "full_every": 7 },
  "destinations": [ { "type": "local", "path": "E:\\Backup\\Docs" } ],
  "archive": { "password": "dpapi:..." },
  "retention": { "keep_full": 4 },
  "schedule": { "type": "interval", "minutes": 120, "from": "08:00:00", "to": "20:00:00" }
}
```
`user`/`password` в источнике нужны только для WebDAV-путей.

## Hyper-V
```json
{
  "id": "6f1c0000-0000-0000-0000-000000000004",
  "name": "VM-DC01",
  "source": { "type": "hyper_v", "vm_name": "DC01" },
  "destinations": [ { "type": "local", "path": "F:\\Backup\\VM" } ],
  "archive": { "level": 1, "password": "dpapi:..." },
  "retention": { "keep_full": 3 },
  "schedule": { "type": "weekly", "days": ["Sun"], "times": ["01:00:00"] }
}
```

## Назначения
```json
{ "type": "local", "path": "E:\\Backup" }
{ "type": "smb", "path": "\\\\nas\\backup\\1c", "user": "NAS\\backup", "password": "dpapi:..." }
{ "type": "sftp", "host": "backup.example.ru", "port": 22, "user": "backup",
  "password": "dpapi:...",                       // или ключ:
  "key_file": "C:\\ProgramData\\SteadyBackup\\id_ed25519", "key_passphrase": "dpapi:...",
  "host_key": "SHA256:IkSXHIdKpBRlX0qPtS8nvdFa/gE+G7SVJf13GQIUTaY",
  "path": "/srv/backup/paydesk",
  "post_command": "/usr/local/bin/notify-backup.sh" }
{ "type": "web_dav", "url": "https://webdav.yandex.ru/Backup", "user": "...", "password": "dpapi:..." }
{ "type": "google_drive", "account": "office", "folder": "SteadyBackup/PayDesk" }
```

* **SFTP**: без `host_key` подключение работает, но в лог пишется отпечаток сервера. Его стоит вписать в задание.
* **Google Drive**: создайте OAuth-клиент типа «Desktop app» в Google Cloud Console (Drive API),
  затем выполните `steady gdrive-auth office --client-id ... --client-secret ...` на той же машине.
  Программа получает доступ только к созданным ею файлам (область `drive.file`).
