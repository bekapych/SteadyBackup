//! Загрузка обновлений 1С (`one_c_download`): модель задания, разбор ответов сервиса
//! и портала, безопасные пути, распаковка и состояние. Сеть не используется.

use std::io::Write;
use std::path::PathBuf;

use steady_core::source::onec_download::{
    decode_zip_name, list_available, parse_configuration_update, parse_conf_download,
    parse_distributions, parse_download_links, parse_platform_update, parse_releases,
    parse_total_programs, program_name_from_nick, relative_path, report, run_download,
    select_account, unzip, AvailableConfiguration, DownloadState, Progress,
};
use steady_core::{DownloadProxy, DownloadReleases, ItsAccount, Job, Source};

fn job_from(source: &str) -> Job {
    serde_json::from_str(&format!(
        r#"{{"id":"11111111-1111-1111-1111-111111111111","name":"Обновления","source":{source},"destinations":[]}}"#
    ))
    .expect("задание должно читаться")
}

fn download_source() -> Source {
    job_from(
        r#"{"type":"one_c_download","template_dir":"C:\\Upd\\templates","platform_dir":"C:\\Upd\\platform"}"#,
    )
    .source
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "steady-one-c-download-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Старое или частичное задание читается: все необязательные поля получают значения по умолчанию.
#[test]
fn minimal_job_gets_defaults() {
    let Source::OneCDownload {
        template_dir,
        platform_dir,
        unzip_files,
        insecure_tls,
        check_only,
        proxy,
        its_accounts,
        platform,
        releases,
        configurations,
    } = download_source()
    else {
        panic!("ожидался источник one_c_download");
    };
    assert_eq!(template_dir, r"C:\Upd\templates");
    assert_eq!(platform_dir, r"C:\Upd\platform");
    assert!(!unzip_files);
    assert!(!insecure_tls);
    assert!(!check_only);
    assert!(proxy.is_none());
    assert!(its_accounts.is_empty());
    assert_eq!(platform.start_version, "");
    assert!(platform.its_account.is_none());
    assert!(releases.enabled);
    assert_eq!(releases.count, 2);
    assert_eq!(releases.projects, vec!["Platform83", "Platform85"]);
    assert_eq!(releases.distributions.len(), 3);
    assert!(releases
        .distributions
        .iter()
        .any(|d| d == "Тонкий клиент 1С:Предприятия для Windows"));
    assert!(configurations.is_empty());
}

/// Задание с полными данными переживает сериализацию без потерь.
#[test]
fn full_job_roundtrip() {
    let raw = r#"{
        "type":"one_c_download",
        "template_dir":"D:\\1c\\templates",
        "platform_dir":"D:\\1c\\platform",
        "unzip_files":true,
        "insecure_tls":true,
        "check_only":true,
        "proxy":{"host":"10.1.2.3","port":3128,"username":"proxy-user","password":"dpapi:x"},
        "its_accounts":[
            {"name":"main","username":"its-login","password":"dpapi:y"},
            {"username":"second","password":"dpapi:z"}
        ],
        "platform":{"start_version":"8.3.24.1234","its_account":"main"},
        "releases":{"enabled":false,"its_account":"second","count":3,"projects":["Platform83"],"distributions":["Тонкий клиент 1С:Предприятия для Windows"]},
        "configurations":[{"human_name":"Бухгалтерия","program_name":"Accounting","start_version":"3.0.50.20","its_account":"main"}]
    }"#;
    let job = job_from(raw);
    let text = serde_json::to_string(&job).unwrap();
    let back: Job = serde_json::from_str(&text).unwrap();
    let Source::OneCDownload {
        unzip_files,
        insecure_tls,
        check_only,
        proxy,
        its_accounts,
        platform,
        releases,
        configurations,
        ..
    } = back.source
    else {
        panic!("ожидался источник one_c_download");
    };
    assert!(unzip_files && insecure_tls && check_only);
    let proxy = proxy.expect("прокси");
    assert_eq!((proxy.host.as_str(), proxy.port), ("10.1.2.3", Some(3128)));
    assert_eq!(proxy.username.as_deref(), Some("proxy-user"));
    assert_eq!(its_accounts.len(), 2);
    assert_eq!(its_accounts[0].name.as_deref(), Some("main"));
    assert!(its_accounts[1].name.is_none());
    assert_eq!(platform.start_version, "8.3.24.1234");
    assert_eq!(platform.its_account.as_deref(), Some("main"));
    assert!(!releases.enabled);
    assert_eq!(releases.count, 3);
    assert_eq!(releases.projects, vec!["Platform83"]);
    assert_eq!(configurations.len(), 1);
    assert_eq!(configurations[0].program_name, "Accounting");
    assert_eq!(configurations[0].its_account.as_deref(), Some("main"));
}

/// Пароли уходят в DPAPI: в JSON задания открытого текста нет, обратное преобразование их возвращает.
#[cfg(windows)]
#[test]
fn passwords_are_protected() {
    let mut job = job_from(
        r#"{"type":"one_c_download","template_dir":"C:\\t","platform_dir":"C:\\p",
            "proxy":{"host":"proxy.local","password":"proxy-pass"},
            "its_accounts":[{"name":"main","username":"its-user","password":"its-pass"}]}"#,
    );
    job.protect_secrets().unwrap();
    let text = serde_json::to_string(&job).unwrap();
    assert!(!text.contains("its-pass"), "пароль ИТС попал в JSON задания");
    assert!(!text.contains("proxy-pass"), "пароль прокси попал в JSON задания");
    job.reveal_secrets().unwrap();
    match &job.source {
        Source::OneCDownload { proxy, its_accounts, .. } => {
            assert_eq!(its_accounts[0].password.as_deref(), Some("its-pass"));
            assert_eq!(
                proxy.as_ref().and_then(|p| p.password.as_deref()),
                Some("proxy-pass")
            );
        }
        _ => panic!("ожидался источник one_c_download"),
    }
}

/// Пароль прокси расшифровывается, логин и пароль percent-кодируются.
#[test]
fn proxy_url_encodes_credentials() {
    let full = DownloadProxy {
        host: "10.0.0.5".into(),
        port: Some(3128),
        username: Some("user@corp".into()),
        password: Some("p@ss word".into()),
    };
    assert_eq!(
        full.url().unwrap().as_deref(),
        Some("http://user%40corp:p%40ss%20word@10.0.0.5:3128")
    );
    let anon = DownloadProxy {
        host: "proxy.local".into(),
        port: None,
        username: None,
        password: None,
    };
    assert_eq!(anon.url().unwrap().as_deref(), Some("http://proxy.local"));
    let empty = DownloadProxy::default();
    assert!(empty.url().unwrap().is_none());
}

/// В сеть уходит открытый пароль, а не шифротекст DPAPI.
#[cfg(windows)]
#[test]
fn proxy_url_decrypts_dpapi_password() {
    let proxy = DownloadProxy {
        host: "proxy.local".into(),
        port: Some(8080),
        username: Some("its".into()),
        password: Some(steady_core::secret::protect_str("p@ss word").unwrap()),
    };
    let url = proxy.url().unwrap().expect("прокси задан");
    assert!(!url.contains("dpapi:"), "в URL попал шифротекст: {url}");
    assert_eq!(url, "http://its:p%40ss%20word@proxy.local:8080");
}

/// Учётная запись выбирается по имени; неизвестное имя — первая из списка.
#[test]
fn account_selected_by_name_with_fallback() {
    let accounts = vec![
        ItsAccount {
            name: Some("main".into()),
            username: "u1".into(),
            password: None,
        },
        ItsAccount {
            name: Some("billing".into()),
            username: "u2".into(),
            password: None,
        },
        ItsAccount {
            name: None,
            username: "u3".into(),
            password: None,
        },
    ];
    assert_eq!(
        select_account(&accounts, Some("billing")).map(|a| a.username.as_str()),
        Some("u2")
    );
    assert_eq!(
        select_account(&accounts, None).map(|a| a.username.as_str()),
        Some("u1")
    );
    assert_eq!(
        select_account(&accounts, Some("")).map(|a| a.username.as_str()),
        Some("u1")
    );
    assert_eq!(
        select_account(&accounts, Some("нет такой")).map(|a| a.username.as_str()),
        Some("u1")
    );
    assert!(select_account(&[], Some("main")).is_none());
}

/// Путь из ответа сервиса и из zip не выходит за пределы целевого каталога.
#[test]
fn relative_path_rejects_escapes() {
    assert_eq!(
        relative_path(r"1c\Accounting/3_0_52_32")
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/"),
        "1c/Accounting/3_0_52_32"
    );
    assert!(relative_path("").unwrap().as_os_str().is_empty());
    assert!(relative_path("./a/./b").unwrap().to_string_lossy().ends_with("b"));
    for bad in [
        r"..\evil",
        "a/../../b",
        "/etc/passwd",
        r"\\server\share",
        r"C:\Windows",
        "1c:stream",
        "..",
    ] {
        assert!(
            relative_path(bad).is_err(),
            "путь «{bad}» должен быть отклонён"
        );
    }
}

/// Имена файлов в zip: UTF-8 как есть, иначе cp866 (так сохраняют старые архивы 1С).
#[test]
fn zip_names_decoded_as_utf8_or_cp866() {
    assert_eq!(decode_zip_name("config/1cv8.cfe".as_bytes()), "config/1cv8.cfe");
    // «конфиг» в cp866.
    assert_eq!(decode_zip_name(b"\xaa\xae\xad\xe4\xa8\xa3"), "конфиг");
}

fn make_zip(path: &std::path::Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut w = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default();
    for (name, data) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn unzip_extracts_into_directory() {
    let dir = temp_dir("unzip");
    let zip_path = dir.join("1cv8.zip");
    make_zip(
        &zip_path,
        &[
            ("config/1cv8.cfe", b"hello"),
            ("config/Ext/Form.dat", b"form"),
        ],
    );
    let out = dir.join("out");
    unzip(&zip_path, &out).unwrap();
    assert_eq!(
        std::fs::read_to_string(out.join("config").join("1cv8.cfe")).unwrap(),
        "hello"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("config").join("Ext").join("Form.dat")).unwrap(),
        "form"
    );
    // Временный каталог распаковки не остаётся.
    assert!(!has_unpack_dir(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Временный подкаталог `.unpack-*` не остаётся в целевом каталоге.
fn has_unpack_dir(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".unpack-")
            })
        })
        .unwrap_or(false)
}

/// Прежние файлы шаблона не пропадают: распаковка дополняет каталог.
#[test]
fn unzip_keeps_previous_files() {
    let dir = temp_dir("unzip-keep");
    let zip_path = dir.join("1cv8.zip");
    make_zip(&zip_path, &[("config/new.cfe", b"new")]);
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("config")).unwrap();
    std::fs::write(out.join("config").join("old.cfe"), b"old").unwrap();
    unzip(&zip_path, &out).unwrap();
    assert_eq!(
        std::fs::read_to_string(out.join("config").join("new.cfe")).unwrap(),
        "new"
    );
    assert_eq!(
        std::fs::read_to_string(out.join("config").join("old.cfe")).unwrap(),
        "old"
    );
    assert!(!has_unpack_dir(&out));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Архив с выходом за пределы каталога не распаковывается, файл вне каталога не создаётся,
/// временный каталог удаляется.
#[test]
fn unzip_rejects_paths_outside_directory() {
    let dir = temp_dir("unzip-escape");
    let zip_path = dir.join("evil.zip");
    make_zip(&zip_path, &[("../escaped.txt", b"x")]);
    let out = dir.join("out");
    std::fs::create_dir_all(&out).unwrap();
    assert!(unzip(&zip_path, &out).is_err());
    assert!(!dir.join("escaped.txt").exists(), "файл создан вне каталога");
    assert!(!has_unpack_dir(&out), "остался временный каталог распаковки");
    let _ = std::fs::remove_dir_all(&dir);
}

const RELEASES_PAGE: &str = r#"<html><body>
<table class="list" id="versionsTable">
<tr><th>Номер версии</th><th class="dateColumn">Дата выхода</th></tr>
<tr><td><a href="/project/Platform85/ver=8.3.25.1394">8.3.25.1394</a></td><td class="dateColumn">14.11.2025</td></tr>
<tr><td><a href="/project/Platform85/ver=8.3.24.1234">8.3.24.1234</a></td><td class="dateColumn">01.09.25</td></tr>
<tr><td><a href="/project/Platform85/ver=8.3.22.1921">8.3.22.1921</a></td><td class="dateColumn">12.03.2025</td></tr>
</table>
</body></html>"#;

/// Таблица версий проекта: номер версии и дата выхода.
#[test]
fn releases_table_parsed() {
    let list = parse_releases("Platform85", RELEASES_PAGE).unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].version, "8.3.25.1394");
    assert_eq!(list[0].project, "Platform85");
    assert_eq!(list[0].date, (2025, 11, 14));
    assert_eq!(list[0].date_text(), "14.11.2025");
    // Двухзначный год — 20xx.
    assert_eq!(list[1].date, (2025, 9, 1));
    assert!(parse_releases("Platform85", "<html><body>нет таблицы</body></html>").is_err());
}

/// Последние версии по дате выхода, затем по номеру.
#[test]
fn releases_sorted_and_truncated() {
    let mut list = parse_releases("Platform85", RELEASES_PAGE).unwrap();
    list.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
    let versions: Vec<String> = list.iter().take(2).map(|r| r.version.clone()).collect();
    assert_eq!(versions, vec!["8.3.25.1394", "8.3.24.1234"]);
}

const FILES_PAGE: &str = r#"<html><body><table>
<tr><td><a href="/version_file?nick=Platform85&amp;ver=8.3.25.1394&amp;path=Platform85%5C8_3_25_1394%5Csetup.exe">Технологическая
   платформа 1С:Предприятия (64-bit) для Windows</a></td></tr>
<tr><td><a href="/version_file?nick=Platform85&amp;ver=8.3.25.1394&amp;path=Platform85%5C8_3_25_1394%5Csetuptc.exe">Тонкий клиент 1С:Предприятия для Windows</a></td></tr>
</table></body></html>"#;

/// Список дистрибутивов версии: название, страница файла и имя файла из `path`.
#[test]
fn distributions_parsed() {
    let list = parse_distributions(FILES_PAGE).unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(
        list[0].name,
        "Технологическая платформа 1С:Предприятия (64-bit) для Windows"
    );
    assert_eq!(list[0].file_name, "setup.exe");
    assert_eq!(
        list[0].page_url,
        "https://releases.1c.ru/version_file?nick=Platform85&ver=8.3.25.1394&path=Platform85%5C8_3_25_1394%5Csetup.exe"
    );
    assert_eq!(list[1].file_name, "setuptc.exe");
}

const FILE_PAGE: &str = r#"<html><body>
<table class="downloadDist">
<tr><td><a href="https://releases.1c.ru/total/8_3_25_1394/setup.exe">Скачать дистрибутив</a></td></tr>
<tr><td><a href="https://mirror.1c.ru/total/8_3_25_1394/setup.exe">Зеркало</a></td></tr>
</table>
</body></html>"#;

/// Ссылки на скачивание: основная первая, зеркала следом.
#[test]
fn download_links_parsed() {
    let links = parse_download_links(FILE_PAGE).unwrap();
    assert_eq!(
        links,
        vec![
            "https://releases.1c.ru/total/8_3_25_1394/setup.exe".to_string(),
            "https://mirror.1c.ru/total/8_3_25_1394/setup.exe".to_string(),
        ]
    );
    assert!(parse_download_links("<html><body>нет ссылок</body></html>").is_err());
}

/// Ответ сервиса: обновление платформы.
#[test]
fn platform_update_response_parsed() {
    let text = r#"{"platformUpdateResponse":{"platformVersion":"8.3.25.1394","distributionUin":"uin-1","size":123456}}"#;
    let upd = parse_platform_update(text).unwrap().expect("обновление");
    assert_eq!(upd.platform_version.as_deref(), Some("8.3.25.1394"));
    assert_eq!(upd.distribution_uin.as_deref(), Some("uin-1"));
    assert_eq!(upd.size, Some(123456.0));
    // Нет раздела — обновления нет.
    assert!(parse_platform_update(r#"{"platformUpdateResponse":null}"#)
        .unwrap()
        .is_none());
    assert!(parse_platform_update("{}").unwrap().is_none());
}

/// Ответ сервиса: обновление конфигурации с цепочкой.
#[test]
fn configuration_update_response_parsed() {
    let text = r#"{"configurationUpdateResponse":{"configurationVersion":"3.0.52.32","upgradeSequence":["a","b"],"programVersionUin":"prog-1"}}"#;
    let upd = parse_configuration_update(text).unwrap().expect("обновление");
    assert_eq!(upd.configuration_version.as_deref(), Some("3.0.52.32"));
    assert_eq!(upd.upgrade_sequence, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(upd.program_version_uin.as_deref(), Some("prog-1"));
    // Пустой ответ: полей нет — значение по умолчанию, ошибки нет.
    let upd = parse_configuration_update(r#"{"configurationUpdateResponse":{}}"#)
        .unwrap()
        .expect("раздел есть");
    assert!(upd.configuration_version.is_none());
    assert!(upd.upgrade_sequence.is_empty());
    assert!(upd.program_version_uin.is_none());
}

/// Данные звена цепочки и ошибка сервиса в `errorName`.
#[test]
fn conf_download_data_parsed_and_service_error() {
    let text = r#"{"configurationUpdateDataList":[{"templatePath":"1c\\Accounting\\3_0_52_32","updateFileUrl":"https://update-api.1c.ru/file","size":2048}]}"#;
    let data = parse_conf_download(text).unwrap().expect("звено");
    assert_eq!(data.template_path, r"1c\Accounting\3_0_52_32");
    assert_eq!(data.update_file_url, "https://update-api.1c.ru/file");
    assert!(parse_conf_download(r#"{"configurationUpdateDataList":[]}"#)
        .unwrap()
        .is_none());

    let err = parse_conf_download(
        r#"{"errorName":"NOT_FOUND","errorMessage":"программа не найдена"}"#,
    )
    .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("NOT_FOUND"), "{text}");
    assert!(text.contains("программа не найдена"), "{text}");
    assert!(parse_conf_download("<html>не JSON</html>").is_err());
}

/// Состояние загрузки пишется отдельным файлом атомарно и читается обратно.
#[test]
fn download_state_saved_and_loaded() {
    let dir = temp_dir("state");
    let id = uuid::Uuid::new_v4();
    let mut st = DownloadState {
        platform: Some("8.3.25.1394".into()),
        ..Default::default()
    };
    st.configurations
        .insert("Accounting".into(), "3.0.52.32".into());
    st.save(&dir, &id).unwrap();
    let path = DownloadState::path(&dir, &id);
    assert_eq!(
        path,
        dir.join("state").join(format!("{id}.download.json"))
    );
    assert!(path.is_file());

    let back = DownloadState::load(&dir, &id).unwrap();
    assert_eq!(back.platform.as_deref(), Some("8.3.25.1394"));
    assert_eq!(
        back.configurations.get("Accounting").map(String::as_str),
        Some("3.0.52.32")
    );

    // Повторная запись заменяет файл, а не дополняет.
    let st2 = DownloadState {
        platform: Some("8.3.26.1000".into()),
        ..Default::default()
    };
    st2.save(&dir, &id).unwrap();
    let back = DownloadState::load(&dir, &id).unwrap();
    assert_eq!(back.platform.as_deref(), Some("8.3.26.1000"));
    assert!(back.configurations.is_empty());

    // Нет файла — пустое состояние, не ошибка.
    let empty = DownloadState::load(&dir, &uuid::Uuid::new_v4()).unwrap();
    assert!(empty.platform.is_none() && empty.configurations.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Частичный или старый файл состояния читается.
#[test]
fn download_state_compatible_with_partial_json() {
    let dir = temp_dir("state-partial");
    let id = uuid::Uuid::new_v4();
    let path = DownloadState::path(&dir, &id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, br#"{"platform":"8.3.24.1234"}"#).unwrap();
    let st = DownloadState::load(&dir, &id).unwrap();
    assert_eq!(st.platform.as_deref(), Some("8.3.24.1234"));
    assert!(st.configurations.is_empty());
    std::fs::write(&path, br#"{}"#).unwrap();
    let st = DownloadState::load(&dir, &id).unwrap();
    assert!(st.platform.is_none() && st.configurations.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

/// Нечего скачивать и нечего скачать — пропуск без файлов и назначений, но с понятным
/// описанием результата вместо пустого «Набора».
#[test]
fn nothing_to_download_is_skipped() {
    let r = report("Обновления", &Progress::default(), false, Vec::new());
    assert!(r.skipped);
    assert_eq!(r.items, 0);
    assert_eq!(r.bytes, 0);
    assert_eq!(r.destinations_ok, 0);
    assert_eq!(r.set_name, "новых файлов нет");

    // check_only: файлов нет — это тоже пропуск копии, но текст другой.
    let found = Progress {
        found: 2,
        files: 0,
        bytes: 0,
    };
    let r = report("Обновления", &found, true, Vec::new());
    assert!(r.skipped);
    assert_eq!(r.items, 0);
    assert_eq!(r.set_name, "проверка без скачивания");

    // Найдено и скачано — обычный результат.
    let downloaded = Progress {
        found: 1,
        files: 3,
        bytes: 4096,
    };
    let r = report("Обновления", &downloaded, false, Vec::new());
    assert!(!r.skipped);
    assert_eq!(r.items, 3);
    assert_eq!(r.bytes, 4096);
    assert_eq!(r.destinations_ok, 0);
    assert_eq!(r.set_name, "скачано 3 файлов");
}

/// Значения по умолчанию раздела releases совпадают с исходной программой.
#[test]
fn releases_defaults_match_original() {
    let d = DownloadReleases::default();
    assert!(d.enabled);
    assert_eq!(d.count, 2);
    assert_eq!(d.projects, vec!["Platform83", "Platform85"]);
    assert!(d.distributions.iter().any(|n| n
        == "Технологическая платформа 1С:Предприятия (64-bit) для Windows"));
    assert!(d
        .distributions
        .iter()
        .any(|n| n == "Технологическая платформа 1С:Предприятия для Windows"));
}

fn context(dir: &std::path::Path) -> steady_core::RunContext {
    steady_core::RunContext {
        data_dir: dir.join("data"),
        work_dir: dir.join("work"),
        state: steady_core::state::StateStore::new(dir.join("data").join("state")),
        seven_zip: None,
        on_phase: None,
        cancel: steady_core::process::CancelFlag::new(),
    }
}

/// Обязательные каталоги и учётные данные проверяются до первого запроса в сеть.
#[test]
fn mandatory_fields_checked_before_requests() {
    let dir = temp_dir("validate");
    let ctx = context(&dir);

    let job = job_from(
        r#"{"type":"one_c_download","template_dir":"Upd","platform_dir":"C:\\p","its_accounts":[{"username":"u"}]}"#,
    );
    let err = run_download(&ctx, &job, &job.source).unwrap_err();
    assert!(format!("{err:#}").contains("абсолютным"), "{err:#}");

    let job = job_from(
        r#"{"type":"one_c_download","template_dir":"C:\\t","platform_dir":"C:\\p"}"#,
    );
    let err = run_download(&ctx, &job, &job.source).unwrap_err();
    assert!(format!("{err:#}").contains("учётная запись ИТС"), "{err:#}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Страница `/total` портала releases.1c.ru (серверный HTML с BOM): группы, строки
/// конфигураций, версия из `version_files`, строки без `/project` пропускаются,
/// группа у первой строки — пустая, версия может отсутствовать.
const TOTAL_PAGE: &str = "\u{feff}<html><body><table class=\"totalTable\">
<tr parent-group=\"481\"><td class=\"nameColumn\"><a href=\"/project/Accounting30\">Бухгалтерия предприятия, редакция 3.0</a></td><td class=\"versionColumn actualVersionColumn\"><a href=\"/version_files?nick=Accounting30&amp;ver=3.0.212.9\">3.0.212.9</a></td></tr>
<tr group=\"accounting\"><td colspan=\"3\"><span class=\"group-name\">Бухгалтерский и налоговый учёт</span></td></tr>
<tr parent-group=\"481\"><td class=\"nameColumn\"><a href=\"/project/AccountingCorp30\">Бухгалтерия предприятия КОРП, редакция 3.0</a></td><td class=\"versionColumn actualVersionColumn\"><a href=\"/version_files?nick=AccountingCorp30&amp;ver=3.0.212.9\">3.0.212.9</a></td></tr>
<tr group=\"hr\"><td colspan=\"3\"><span class=\"group-name\">Кадровый учёт</span></td></tr>
<tr parent-group=\"13\"><td class=\"nameColumn\"><a href=\"/project/HRM30\">Заработная плата и управление персоналом, редакция 3.1</a></td><td class=\"versionColumn actualVersionColumn\"><a href=\"/version_files?nick=HRM30&amp;ver=3.1.36.244\">3.1.36.244</a></td></tr>
<tr parent-group=\"13\"><td class=\"nameColumn\"><a href=\"/project/HRMCorp30\">Заработная плата и управление персоналом КОРП, редакция 3.1</a></td><td class=\"versionColumn actualVersionColumn\"><a href=\"/version_files?nick=HRMCorp30&amp;ver=3.1.36.244\">3.1.36.244</a></td></tr>
<tr group=\"branch\"><td colspan=\"3\"><span class=\"group-name\">Отраслевые решения</span></td></tr>
<tr parent-group=\"77\"><td class=\"nameColumn\"><a href=\"/project/SmallBusiness30\">Малый бизнес &amp; &quot;Предпринимательство&quot;</a></td><td class=\"versionColumn actualVersionColumn\"></td></tr>
<tr parent-group=\"77\"><td class=\"nameColumn\"><a href=\"/project/Trade11\">Управление торговлей, редакция 11</a></td><td class=\"versionColumn actualVersionColumn\"><a href=\"/version_files?nick=Trade11&amp;ver=11.5.19.111\">11.5.19.111</a></td></tr>
<tr group=\"tech\"><td colspan=\"3\"><span class=\"group-name\">Технологические дистрибутивы</span></td></tr>
<tr parent-group=\"9\"><td class=\"nameColumn\"><a href=\"/project/MobilePlatform\">Мобильная платформа 1С:Предприятия</a></td><td class=\"versionColumn actualVersionColumn\"></td></tr>
<tr parent-group=\"9\"><td class=\"nameColumn\"><a href=\"/version_files?nick=Platform85&amp;ver=8.3.25.1394\">Технологическая платформа 8.3</a></td><td class=\"versionColumn actualVersionColumn\">8.3.25.1394</td></tr>
</table></body></html>";

/// Список конфигураций со страницы `/total`: группа, nick, имя и версия.
#[test]
fn total_page_parsed() {
    let list = parse_total_programs(TOTAL_PAGE).unwrap();
    let short: Vec<(&str, &str, &str)> = list
        .iter()
        .map(|p| (p.group.as_str(), p.nick.as_str(), p.version.as_str()))
        .collect();
    assert_eq!(
        short,
        vec![
            ("", "Accounting30", "3.0.212.9"),
            ("Бухгалтерский и налоговый учёт", "AccountingCorp30", "3.0.212.9"),
            ("Кадровый учёт", "HRM30", "3.1.36.244"),
            ("Кадровый учёт", "HRMCorp30", "3.1.36.244"),
            ("Отраслевые решения", "SmallBusiness30", ""),
            ("Отраслевые решения", "Trade11", "11.5.19.111"),
            ("Технологические дистрибутивы", "MobilePlatform", ""),
        ]
    );
    // Названия из HTML-сущностей раскрываются, пробелы схлопываются.
    assert_eq!(list[0].name, "Бухгалтерия предприятия, редакция 3.0");
    assert_eq!(list[4].name, "Малый бизнес & \"Предпринимательство\"");
    // Строка без `/project/<nick>` в список не попадает.
    assert!(list.iter().all(|p| p.nick != "Platform85"));
}

/// Пустая или битая страница — понятная ошибка вместо паники или пустого ответа.
#[test]
fn total_page_without_rows_is_error() {
    for page in [
        "",
        "<html><body>нет таблицы</body></html>",
        "<table class=\"totalTable\"></table>",
        "<tr><td>строка без проекта</td></tr>",
    ] {
        let err = parse_total_programs(page).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("конфигурац"), "«{page}»: {text}");
    }
}

/// Имя программы для сервиса обновлений: nick без суффикса редакции.
#[test]
fn program_name_from_nick_strips_edition() {
    assert_eq!(program_name_from_nick("Accounting30"), "Accounting");
    assert_eq!(program_name_from_nick("HRM30"), "HRM");
    assert_eq!(program_name_from_nick("AccountingCorp30"), "AccountingCorp");
    assert_eq!(program_name_from_nick("SmallBusiness30"), "SmallBusiness");
    // Суффикса редакции нет — nick как есть.
    assert_eq!(program_name_from_nick("MobilePlatform"), "MobilePlatform");
    assert_eq!(program_name_from_nick("Trade11"), "Trade");
    // Вырожденные значения не дают пустого имени.
    assert_eq!(program_name_from_nick(""), "");
    assert_eq!(program_name_from_nick("11"), "11");
}

/// Конфигурация для выбора в GUI: имена полей JSON и производное имя программы.
#[test]
fn available_configuration_serialized() {
    let c = AvailableConfiguration {
        group: "Бухгалтерский и налоговый учёт".into(),
        nick: "Accounting30".into(),
        name: "Бухгалтерия предприятия, редакция 3.0".into(),
        program_name: program_name_from_nick("Accounting30"),
        version: "3.0.212.9".into(),
    };
    let v = serde_json::to_value(&c).unwrap();
    assert_eq!(v["group"], "Бухгалтерский и налоговый учёт");
    assert_eq!(v["nick"], "Accounting30");
    assert_eq!(v["name"], "Бухгалтерия предприятия, редакция 3.0");
    assert_eq!(v["program_name"], "Accounting");
    assert_eq!(v["version"], "3.0.212.9");
    // Пароля в ответе нет: структура о нём не знает.
    assert!(v.get("password").is_none());
}

/// Пустой логин отсекается до первого сетевого запроса.
#[test]
fn empty_its_login_rejected_without_network() {
    let err = list_available("   ", "pass", None, false).unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("логин"), "{text}");
}

/// Живая проверка списка конфигураций ИТС (нужна сеть и действующая учётная запись):
///
/// ```text
/// $env:STEADY_ITS_USER = "..."; $env:STEADY_ITS_PASSWORD = "..."
/// cargo test -p steady-core --test one_c_download -- --ignored live_its_list --nocapture
/// ```
///
/// Учётные данные в тест не вписываются: только переменные окружения.
#[test]
#[ignore = "нужны сеть и действующая учётная запись ИТС"]
fn live_its_list() {
    let user = std::env::var("STEADY_ITS_USER").expect("STEADY_ITS_USER");
    let password = std::env::var("STEADY_ITS_PASSWORD").expect("STEADY_ITS_PASSWORD");
    let list = list_available(&user, &password, None, false).expect("список конфигураций ИТС");
    assert!(!list.is_empty(), "список не должен быть пустым");
    println!("конфигураций: {}", list.len());
    for c in list.iter().take(8) {
        println!("{} | {} | {} | {}", c.group, c.name, c.program_name, c.version);
    }
}

/// Проверять нечего: запуск успешно пропускается, не обращаясь к сервисам.
#[test]
fn empty_job_is_skipped_without_requests() {
    let dir = temp_dir("run-empty");
    let ctx = context(&dir);
    let job = job_from(
        r#"{"type":"one_c_download","template_dir":"C:\\t","platform_dir":"C:\\p",
            "its_accounts":[{"username":"u","password":"p"}],"releases":{"enabled":false}}"#,
    );
    let out = run_download(&ctx, &job, &job.source).unwrap();
    assert!(out.report.skipped);
    assert_eq!(out.report.items, 0);
    assert_eq!(out.report.destinations_ok, 0);
    assert_eq!(out.report.set_name, "новых файлов нет");
    assert!(
        out.report.warnings.iter().any(|w| w.contains("платформы")),
        "{:?}",
        out.report.warnings
    );
    assert_eq!(out.state, DownloadState::default());
    let _ = std::fs::remove_dir_all(&dir);
}
