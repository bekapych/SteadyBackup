use std::path::{Path, PathBuf};
use steady_core::source::onec_update::{
    backup_file_name, file_arg_matches, missing_intermediate, no_op_report, parse_rac_records,
    parse_update_info, plan, rac_session_ids, read_cfu_sources, scan, select, ConfigInfo, Package,
    PackageKind, Target, Version,
};
use steady_core::{Job, RunReport, Source};

fn v(s: &str) -> Version {
    Version::parse(s).expect("корректная версия в тесте")
}

fn package(to: &str, from: &[&str], kind: PackageKind) -> Package {
    Package {
        file: PathBuf::from(format!(
            "{to}.{}",
            if kind == PackageKind::Cfu {
                "cfu"
            } else {
                "cf"
            }
        )),
        kind,
        version: v(to),
        from: from.iter().map(|s| v(s)).collect(),
        vendor: String::new(),
        name: "УНФ".into(),
        sources: Vec::new(),
    }
}

fn versions(chain: &[Package]) -> Vec<String> {
    chain.iter().map(|p| p.version.to_string()).collect()
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "steady-one-c-update-{name}-{}",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&dir).expect("временный каталог");
    dir
}

#[test]
fn json_one_c_update_defaults_and_roundtrip() {
    let source: Source = serde_json::from_str(
        r#"{"type":"one_c_update","ib_path":"C:\\ib","updates_dir":"D:\\updates","backup_dir":"E:\\backup"}"#,
    )
    .expect("новый источник читается");
    let Source::OneCUpdate {
        ib_path,
        updates_dir,
        backup_dir,
        platform_exe,
        connection,
        user,
        password,
        unlock_code,
        force,
        server_side,
        run_handlers,
        cluster_user,
        cluster_password,
    } = &source
    else {
        panic!("ожидался OneCUpdate");
    };
    assert_eq!(
        (ib_path.as_str(), updates_dir.as_str(), backup_dir.as_str()),
        ("C:\\ib", "D:\\updates", "E:\\backup")
    );
    assert!(platform_exe.is_none() && connection.is_none() && user.is_none() && password.is_none());
    assert!(unlock_code.is_none() && cluster_user.is_none() && cluster_password.is_none());
    assert!(!*force && !*server_side && !*run_handlers);
    assert_eq!(
        source.default_timeout(),
        std::time::Duration::from_secs(3 * 3600)
    );
    assert_eq!(
        serde_json::to_value(&source).expect("JSON")["type"],
        "one_c_update"
    );
}

#[cfg(windows)]
#[test]
fn one_c_update_passwords_are_protected_and_revealed() {
    let mut job: Job = serde_json::from_str(
        r#"{"id":"11111111-1111-1111-1111-111111111111","name":"u","source":{"type":"one_c_update","ib_path":"C:\\ib","updates_dir":"D:\\u","backup_dir":"E:\\b","password":"ib-secret","cluster_password":"cluster-secret"},"destinations":[]}"#,
    )
    .expect("задание");
    job.protect_secrets().expect("DPAPI");
    let Source::OneCUpdate {
        password,
        cluster_password,
        ..
    } = &job.source
    else {
        panic!("источник")
    };
    assert!(password
        .as_deref()
        .is_some_and(|s| s.starts_with("dpapi:") && !s.contains("ib-secret")));
    assert!(cluster_password
        .as_deref()
        .is_some_and(|s| s.starts_with("dpapi:") && !s.contains("cluster-secret")));
    job.reveal_secrets().expect("раскрытие DPAPI");
    let Source::OneCUpdate {
        password,
        cluster_password,
        ..
    } = &job.source
    else {
        panic!("источник")
    };
    assert_eq!(
        (password.as_deref(), cluster_password.as_deref()),
        (Some("ib-secret"), Some("cluster-secret"))
    );
}

#[cfg(windows)]
#[test]
fn one_c_update_resolves_one_c_connection() {
    use steady_core::resources::{Connection, ConnectionKind, Resources};
    let dir = temp_dir("connection");
    let res = Resources::new(&dir);
    res.save_connection(Connection {
        id: "onec".into(),
        name: "1С".into(),
        kind: ConnectionKind::OneC {
            user: Some("Updater".into()),
            password: Some("pw".into()),
        },
    })
    .expect("подключение");
    let source: Source = serde_json::from_str(
        r#"{"type":"one_c_update","connection":"onec","ib_path":"srv\\ref","updates_dir":"D:\\u","backup_dir":"E:\\b"}"#,
    )
    .expect("источник");
    assert_eq!(
        steady_core::resources::source_connection(&source),
        Some("onec")
    );
    let resolved = res.resolve_source(&source).expect("разрешение подключения");
    let Source::OneCUpdate { user, password, .. } = resolved else {
        panic!("источник")
    };
    assert_eq!(user.as_deref(), Some("Updater"));
    assert_eq!(
        steady_core::secret::reveal_opt(password.as_deref())
            .expect("пароль")
            .as_deref(),
        Some("pw")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn one_c_update_rejects_wrong_connection_type() {
    use steady_core::resources::{Connection, ConnectionKind, Resources};
    let dir = temp_dir("wrong-connection");
    let res = Resources::new(&dir);
    res.save_connection(Connection {
        id: "sql".into(),
        name: "SQL".into(),
        kind: ConnectionKind::MsSql {
            server: "localhost".into(),
            user: None,
            password: None,
        },
    })
    .expect("подключение");
    let source: Source = serde_json::from_str(
        r#"{"type":"one_c_update","connection":"sql","ib_path":"srv\\ref","updates_dir":"D:\\u","backup_dir":"E:\\b"}"#,
    )
    .expect("источник");
    let error = res
        .resolve_source(&source)
        .expect_err("тип подключения должен проверяться");
    assert!(
        format!("{error:#}").contains("не является подключением к 1С"),
        "{error:#}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn version_order_is_numeric_and_ignores_trailing_zeroes() {
    assert!(v("3.0.10.1") > v("3.0.9.200"));
    assert_eq!(v("3.0.1"), v("3.0.1.0"));
    assert_eq!(v("3_0_150_25"), v("3.0.150.25"));
    assert!(Version::parse("3.0.x").is_none());
    assert!(Version::parse("").is_none());
}

#[test]
fn target_distinguishes_file_paths_and_server_refs() {
    let dir = temp_dir("target");
    assert!(matches!(
        Target::parse(&dir.display().to_string()),
        Target::File(_)
    ));
    let missing = dir.join("missing").join("ib");
    assert!(
        matches!(Target::parse(&missing.display().to_string()), Target::File(p) if p == missing)
    );
    assert!(
        matches!(Target::parse(r"srv1c:1541\unf"), Target::Server { server, name } if server == "srv1c:1541" && name == "unf")
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn planner_prefers_cfu_chain_then_fewer_steps() {
    let packages = vec![
        package("1.0.2", &["1.0.1"], PackageKind::Cfu),
        package("1.0.3", &["1.0.2"], PackageKind::Cfu),
        package("1.0.3", &[], PackageKind::Cf),
        package("1.0.4", &["1.0.3"], PackageKind::Cfu),
    ];
    let chain = plan(&packages, &v("1.0.1"), None);
    assert_eq!(versions(&chain), ["1.0.2", "1.0.3", "1.0.4"]);
    assert!(chain.iter().all(|p| p.kind == PackageKind::Cfu));
    assert!(plan(&packages, &v("1.0.4"), None).is_empty());
    let cfu_only = vec![package("1.0.2", &["1.0.1"], PackageKind::Cfu)];
    assert!(plan(&cfu_only, &v("0.9"), None).is_empty());
}

#[test]
fn planner_prefers_cfu_over_cf_of_same_version() {
    let packages = vec![
        package("2.0", &[], PackageKind::Cf),
        package("2.0", &["1.0"], PackageKind::Cfu),
    ];
    let chain = plan(&packages, &v("1.0"), None);
    assert_eq!(chain.len(), 1);
    assert_eq!(chain[0].kind, PackageKind::Cfu);
    assert_eq!(versions(&chain), ["2.0"]);
}

#[test]
fn planner_chains_cf_then_cfu() {
    let packages = vec![
        package("1.2", &[], PackageKind::Cf),
        package("1.3", &["1.2"], PackageKind::Cfu),
    ];
    let chain = plan(&packages, &v("1.0"), None);
    assert_eq!(versions(&chain), ["1.2", "1.3"]);
    assert_eq!(
        chain.iter().map(|p| p.kind).collect::<Vec<_>>(),
        [PackageKind::Cf, PackageKind::Cfu]
    );
}

#[test]
fn planner_stops_at_requested_target() {
    let packages = vec![
        package("1.2", &["1.1"], PackageKind::Cfu),
        package("1.3", &["1.2"], PackageKind::Cfu),
    ];
    assert_eq!(
        versions(&plan(&packages, &v("1.1"), Some(&v("1.2")))),
        ["1.2"]
    );
    assert!(plan(&packages, &v("1.1"), Some(&v("1.0.9"))).is_empty());
}

#[test]
fn unreachable_newer_package_requires_intermediate_update() {
    let own = vec![package("1.0.3", &["1.0.2"], PackageKind::Cfu)];
    let chain = plan(&own, &v("1.0.1"), None);
    assert!(chain.is_empty());
    assert_eq!(
        missing_intermediate(&own, &v("1.0.1"), &chain),
        Some(v("1.0.3"))
    );
    assert_eq!(missing_intermediate(&[], &v("1.0.1"), &[]), None);
    assert_eq!(missing_intermediate(&own, &v("1.0.3"), &[]), None);
    assert_eq!(missing_intermediate(&own, &v("1.0.1"), &own), None);
}

#[test]
fn planner_uses_cf_when_no_cfu_chain_exists() {
    let packages = vec![
        package("1.2", &["1.1"], PackageKind::Cfu),
        package("1.3", &[], PackageKind::Cf),
    ];
    let chain = plan(&packages, &v("1.0"), None);
    assert_eq!(versions(&chain), ["1.3"]);
    assert_eq!(chain[0].kind, PackageKind::Cf);
}

#[test]
fn scan_reads_mft_updinfo_and_reports_missing_root() {
    let root = temp_dir("scan");
    let package_dir = root.join("1c").join("Trade").join("11_5_2_10");
    std::fs::create_dir_all(&package_dir).expect("каталог пакета");
    std::fs::write(
        package_dir.join("1cv8.mft"),
        "Vendor=Фирма \"1С\"\r\nName=УправлениеТорговлей\r\nVersion=11.5.2.10\r\n[Config1]\r\nSource=1cv8.cf\r\n",
    )
    .expect("mft");
    std::fs::write(
        package_dir.join("UpdInfo.txt"),
        "Version=11.5.2.10\r\nFromVersions=;11.5.1.20;11.5.1.25;\r\n",
    )
    .expect("UpdInfo");
    std::fs::write(package_dir.join("1cv8.cfu"), b"not a real cfu").expect("cfu");
    std::fs::write(package_dir.join("1cv8.cf"), b"cf").expect("cf");

    let scanned = scan(std::slice::from_ref(&root)).expect("сканирование");
    assert_eq!(scanned.packages.len(), 2, "{:?}", scanned.warnings);
    let cfu = scanned
        .packages
        .iter()
        .find(|p| p.kind == PackageKind::Cfu)
        .expect("cfu");
    let selected = select(
        cfu,
        "Фирма «1С»",
        &["УправлениеТорговлей", "Управление торговлей"],
    )
    .expect("читаемые исходные версии")
    .expect("свой пакет");
    assert_eq!(selected.from, vec![v("11.5.1.20"), v("11.5.1.25")]);
    assert!(select(cfu, "Фирма «1С»", &["БухгалтерияПредприятия"])
        .expect("чужой пакет")
        .is_none());

    let missing = root.join("missing-root");
    assert!(
        scan(&[missing]).is_err(),
        "ошибка чтения корня не должна становиться no-op"
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn update_info_parser_extracts_only_real_sources() {
    let text = "\u{FEFF}{3,2,\"УНФ\",\"Фирма \"\"1С\"\"\",\"3.0.10.1\",4877eb92-bafd-4182-ab12-f087cb458bbd,\
                \"УНФБазовая\",\"Фирма \"\"1С\"\"\",\"3.0.10.2\",83e93837-6632-4a9d-b3fc-3c6857f0bab9,21,\"x\",{216,0,{1,0}}}";
    let sources = parse_update_info(text).expect("UpdateInfo.inf");
    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].name, "УНФ");
    assert_eq!(sources[0].vendor, "Фирма \"1С\"");
    assert_eq!(sources[1].version, v("3.0.10.2"));
    assert!(parse_update_info("{2,1}").is_err());
    assert!(parse_update_info("not an update").is_err());
}

#[test]
fn binary_cfu_fixture_is_read_streaming() {
    fn push_block(out: &mut Vec<u8>, data: &[u8]) {
        let header = format!(
            "\r\n{:08X} {:08X} {:08X} \r\n",
            data.len(),
            data.len(),
            0x7FFF_FFFFu32
        );
        assert_eq!(header.len(), 31);
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(data);
    }

    let text =
        "{3,1,\"УНФ\",\"Фирма \"\"1С\"\"\",\"3.0.10.1\",4877eb92-bafd-4182-ab12-f087cb458bbd}";
    let mut header_data = vec![0u8; 20];
    for unit in "UpdateInfo.inf".encode_utf16().chain(std::iter::once(0)) {
        header_data.extend_from_slice(&unit.to_le_bytes());
    }
    let header_offset = 16 + 31 + 12;
    let data_offset = header_offset + 31 + header_data.len();
    let mut toc = Vec::new();
    toc.extend_from_slice(&(header_offset as u32).to_le_bytes());
    toc.extend_from_slice(&(data_offset as u32).to_le_bytes());
    toc.extend_from_slice(&0u32.to_le_bytes());

    let mut bytes = vec![0u8; 16];
    bytes[..4].copy_from_slice(&[0xFF, 0xFF, 0xFF, 0x7F]);
    push_block(&mut bytes, &toc);
    assert_eq!(bytes.len(), header_offset);
    push_block(&mut bytes, &header_data);
    assert_eq!(bytes.len(), data_offset);
    push_block(&mut bytes, text.as_bytes());

    let dir = temp_dir("cfu-container");
    let file = dir.join("synthetic.cfu");
    std::fs::write(&file, bytes).expect("синтетический CFU");
    let sources = read_cfu_sources(&file).expect("UpdateInfo.inf из бинарного контейнера");
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].name, "УНФ");
    assert_eq!(sources[0].version, v("3.0.10.1"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn config_xml_parser_reads_main_properties() {
    let xml = r#"<Configuration><Properties><Name>УправлениеНебольшойФирмой</Name>
<Synonym><v8:item><v8:lang>ru</v8:lang><v8:content>Управление нашей фирмой</v8:content></v8:item></Synonym>
<Comment/><Vendor>Фирма &quot;1С&quot;</Vendor><Version>3.0.4.123</Version></Properties>
<ChildObjects><Catalog>X</Catalog></ChildObjects></Configuration>"#;
    let cfg = ConfigInfo::from_xml(xml).expect("Configuration.xml");
    assert_eq!(cfg.name, "УправлениеНебольшойФирмой");
    assert_eq!(cfg.synonym, "Управление нашей фирмой");
    assert_eq!(cfg.vendor, "Фирма \"1С\"");
    assert_eq!(cfg.version, "3.0.4.123");
}

#[test]
fn backup_name_is_safe_and_contains_target_version_and_stamp() {
    let target = Target::parse(r"srv\Бухгалтерия/рабочая");
    let name = backup_file_name(&target, &v("3.0.10.1"), "20261121_233000");
    assert_eq!(name, "srv_Бухгалтерия_рабочая_3.0.10.1_20261121_233000.dt");
    assert!(!name.contains(['\\', '/', ':']));
}

#[test]
fn rac_output_parser_handles_records_quotes_and_session_filter() {
    let text = "cluster : c-1\r\nname : \"Основной кластер\"\r\n\r\ninfobase : ib-1\r\nname : \"УНФ\"\r\n\r\nsession : s-1\r\ninfobase : ib-1\r\nsession : s-2\r\ninfobase : other\r\nsession : s-3\r\ninfobase : ib-1\r\n";
    let records = parse_rac_records(text);
    assert_eq!(records.len(), 5);
    assert_eq!(records[0].get("cluster").map(String::as_str), Some("c-1"));
    assert_eq!(records[1].get("name").map(String::as_str), Some("УНФ"));
    assert_eq!(rac_session_ids(&records, "ib-1"), vec!["s-1", "s-3"]);
    assert!(rac_session_ids(&records, "missing").is_empty());
}

#[test]
fn file_argument_matches_only_exact_canonical_base() {
    let base = Path::new(r"C:\Bases\УНФ");
    assert!(file_arg_matches(
        r#""C:\Program Files\1cv8\8.3.25.1000\bin\1cv8.exe" DESIGNER /F"C:\Bases\УНФ""#,
        base
    ));
    assert!(file_arg_matches(
        r"1cv8c.exe DESIGNER /F C:\bases\унф",
        base
    ));
    assert!(!file_arg_matches(
        r"1cv8.exe DESIGNER /FC:\Bases\УНФ2",
        base
    ));
    assert!(!file_arg_matches(r"1cv8.exe DESIGNER /FC:\Bases", base));
    assert!(!file_arg_matches(r"1cv8.exe DESIGNER /Ssrv\УНФ", base));
    assert!(!file_arg_matches(r"1cv8.exe DESIGNER /F", base));
}

#[test]
fn no_op_report_is_explicitly_skipped_and_has_no_backup() {
    let report = no_op_report("Обновление УНФ", vec!["предупреждение".into()]);
    assert!(report.skipped);
    assert_eq!(report.job, "Обновление УНФ");
    assert_eq!(report.set_name, "");
    assert_eq!(
        (report.items, report.bytes, report.destinations_ok),
        (0, 0, 0)
    );
    assert!(report.errors.is_empty());
    assert_eq!(report.warnings, vec!["предупреждение"]);
}

#[test]
fn old_report_json_defaults_skipped_to_false() {
    let report: RunReport = serde_json::from_str(
        r#"{"job":"old","kind":"full","set_name":"old_20260101_000000_full","items":1,"bytes":2,"destinations_ok":1,"errors":[]}"#,
    )
    .expect("старый отчёт");
    assert!(!report.skipped);
}
