//! Загрузка обновлений 1С: отдельная ветка конвейера без назначений, staging, 7-Zip
//! и ротации. Скачанное остаётся в постоянных каталогах задания — шаблонах конфигураций
//! и каталоге платформы, поэтому `RunReport` описывает только «сколько и чего скачано».
//!
//! Источники обновлений:
//! * сервис обновлений 1С — обновления конфигураций (цепочка) и платформы;
//! * releases.1c.ru — дистрибутивы последних версий платформы.
//!
//! Всё требует учётной записи ИТС. Пароли берутся из задания (DPAPI) и в журнал не попадают.

mod api;
mod paths;
mod releases;
mod state;
mod unzip;

pub use api::{
    parse_configuration_update, parse_conf_download, parse_platform_update, ConfDownload, ConfUpdate,
    PlatformUpdate,
};
pub use paths::{decode_zip_name, relative_path};
pub use releases::{
    normalize_name, parse_distributions, parse_download_links, parse_releases, Distribution, Release,
};
pub use state::DownloadState;
pub use unzip::unzip;

use crate::job::{
    download_config_title, DownloadConfig, DownloadPlatform, DownloadProxy, DownloadReleases,
    ItsAccount, Job, Source,
};
use crate::pipeline::{RunContext, RunReport};
use anyhow::{anyhow, bail, Context, Result};
use releases::Portal;
use std::path::Path;

/// Что нашлось и что скачано за запуск.
#[derive(Debug, Clone, Default)]
pub struct Progress {
    /// Сколько обновлений и дистрибутивов нашлось (включая уже скачанные).
    pub found: usize,
    /// Сколько файлов реально скачано за этот запуск.
    pub files: u64,
    /// Суммарный размер скачанного за этот запуск, байт.
    pub bytes: u64,
}

/// Отчёт запуска.
///
/// `set_name` описывает результат для журнала и уведомлений вместо имени набора копии:
/// «скачано N файлов», «новых файлов нет», «проверка без скачивания». `skipped` — за запуск
/// не скачано ни одного файла (проверка или нечего скачивать): набор копии не создавался.
pub fn report(
    job: &str,
    progress: &Progress,
    check_only: bool,
    warnings: Vec<String>,
) -> RunReport {
    let set_name = if progress.files > 0 {
        format!("скачано {} файлов", progress.files)
    } else if check_only {
        "проверка без скачивания".to_string()
    } else {
        "новых файлов нет".to_string()
    };
    RunReport {
        job: job.to_string(),
        kind: crate::job::BackupKind::Full,
        set_name,
        items: progress.files,
        bytes: progress.bytes,
        destinations_ok: 0,
        errors: Vec::new(),
        warnings,
        skipped: progress.files == 0,
    }
}

/// Учётная запись ИТС по имени. Имя не задано или не найдено — первая из списка
/// (имя попадает в задание руками, и опечатка не должна оставлять запуск без учётных данных).
pub fn select_account<'a>(accounts: &'a [ItsAccount], name: Option<&str>) -> Option<&'a ItsAccount> {
    let first = accounts.first()?;
    let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) else {
        return Some(first);
    };
    match accounts.iter().find(|a| a.name.as_deref() == Some(name)) {
        Some(account) => Some(account),
        None => {
            tracing::warn!(
                "ИТС: учётная запись «{name}» не найдена, используется первая из списка"
            );
            Some(first)
        }
    }
}

/// Результат запуска: отчёт для журнала и состояние — что уже скачано.
#[derive(Debug)]
pub struct Outcome {
    pub report: RunReport,
    pub state: DownloadState,
}

fn phase(ctx: &RunContext, text: &str) {
    tracing::info!("{text}");
    if let Some(f) = &ctx.on_phase {
        f(text);
    }
}

fn size_text(bytes: u64) -> String {
    format!("{:.1} Мб", bytes as f64 / 1024.0 / 1024.0)
}

/// Запуск загрузки обновлений. Отдельная ветка `pipeline::run_job`: назначения, staging,
/// 7-Zip и ротация не участвуют.
pub fn run(ctx: &RunContext, job: &Job, source: &Source) -> Result<RunReport> {
    Ok(run_download(ctx, job, source)?.report)
}

/// То же, что `run`, но возвращает и состояние: повторный запуск увидит, что уже скачано.
pub fn run_download(ctx: &RunContext, job: &Job, source: &Source) -> Result<Outcome> {
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
    } = source
    else {
        bail!("внутренняя ошибка: источник не является загрузкой обновлений 1С");
    };
    let template_dir = paths::absolute_dir("template_dir", template_dir)?;
    let platform_dir = paths::absolute_dir("platform_dir", platform_dir)?;
    if its_accounts.is_empty() {
        bail!("для загрузки обновлений 1С нужна хотя бы одна учётная запись ИТС (its_accounts)");
    }
    ctx.cancel.check()?;

    let mut state = DownloadState::load(&ctx.data_dir, &job.id)?;
    let mut progress = Progress::default();
    let mut warnings = Vec::new();
    let mut failures: Vec<String> = Vec::new();

    download_configurations(
        ctx,
        job,
        configurations,
        its_accounts,
        proxy.as_ref(),
        *insecure_tls,
        *check_only,
        *unzip_files,
        platform,
        &template_dir,
        &mut state,
        &mut progress,
        &mut failures,
    )?;
    download_platform(
        ctx,
        job,
        platform,
        its_accounts,
        proxy.as_ref(),
        *insecure_tls,
        *check_only,
        &platform_dir,
        &mut state,
        &mut progress,
        &mut warnings,
        &mut failures,
    )?;
    download_releases(
        ctx,
        job,
        releases,
        platform,
        its_accounts,
        proxy.as_ref(),
        *insecure_tls,
        *check_only,
        &platform_dir,
        &mut progress,
        &mut failures,
    )?;

    if !failures.is_empty() {
        // Часть файлов могла скачаться: текст говорит, что сохранено, а что нет.
        let saved = if progress.files > 0 {
            format!(
                "сохранено файлов: {}, объём {}",
                progress.files,
                size_text(progress.bytes)
            )
        } else {
            "ничего не сохранено".into()
        };
        bail!("загрузка обновлений 1С завершилась с ошибками ({saved}): {}", failures.join("; "));
    }
    tracing::info!(
        "1С: найдено обновлений и дистрибутивов: {}, скачано файлов: {} ({})",
        progress.found,
        progress.files,
        size_text(progress.bytes)
    );
    Ok(Outcome {
        report: report(&job.name, &progress, *check_only, warnings),
        state,
    })
}

/// Учётные данные ИТС для одного раздела задания.
fn account_login(accounts: &[ItsAccount], name: Option<&str>) -> Result<(String, String)> {
    let account =
        select_account(accounts, name).context("не задано ни одной учётной записи ИТС")?;
    if account.username.trim().is_empty() {
        bail!("у учётной записи ИТС «{}» не задан логин", account.display_name());
    }
    let password = crate::secret::reveal_opt(account.password.as_deref())?.unwrap_or_default();
    Ok((account.username.clone(), password))
}

fn client_for(
    accounts: &[ItsAccount],
    name: Option<&str>,
    proxy: Option<&DownloadProxy>,
    insecure_tls: bool,
) -> Result<api::ApiClient> {
    let (login, password) = account_login(accounts, name)?;
    api::ApiClient::new(&login, &password, proxy, insecure_tls)
        .context("сервис обновлений 1С")
}

/// Обновления конфигураций: проверка, цепочка звеньев и скачивание в каталог шаблонов.
#[allow(clippy::too_many_arguments)]
fn download_configurations(
    ctx: &RunContext,
    job: &Job,
    configurations: &[DownloadConfig],
    accounts: &[ItsAccount],
    proxy: Option<&DownloadProxy>,
    insecure_tls: bool,
    check_only: bool,
    unzip_files: bool,
    platform: &DownloadPlatform,
    template_dir: &Path,
    state: &mut DownloadState,
    progress: &mut Progress,
    failures: &mut Vec<String>,
) -> Result<()> {
    for conf in configurations {
        ctx.cancel.check()?;
        let title = download_config_title(conf);
        let current = DownloadState::current(
            state.configurations.get(&conf.program_name),
            &conf.start_version,
        )
        .to_string();
        if current.is_empty() {
            failures.push(format!("{title}: не задана начальная версия конфигурации"));
            continue;
        }
        phase(
            ctx,
            &format!("«{}»: проверка обновления {title}", job.name),
        );
        let client = match client_for(accounts, conf.its_account.as_deref(), proxy, insecure_tls) {
            Ok(c) => c,
            Err(e) => {
                failures.push(format!("{title}: {e:#}"));
                continue;
            }
        };
        let platform_version =
            DownloadState::current(state.platform.as_ref(), &platform.start_version).to_string();
        let update =
            match client.check_conf_update(&conf.program_name, &current, &platform_version) {
                Ok(Some(update)) => update,
                Ok(None) => {
                    tracing::info!("1С: для «{title}» обновления нет (версия {current})");
                    continue;
                }
                Err(e) => {
                    failures.push(format!("{title}: проверка обновлений: {e:#}"));
                    continue;
                }
            };
        let Some(version) = update
            .configuration_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty() && *v != current)
        else {
            tracing::info!("1С: «{title}» уже актуальна (версия {current})");
            continue;
        };
        progress.found += 1;
        tracing::info!("1С: для «{title}» доступна версия {version} (сейчас {current})");
        if check_only {
            tracing::info!(
                "1С: «{title}»: проверка без скачивания (check_only) — версия {version} не скачивалась"
            );
            continue;
        }
        let Some(program_uin) = update
            .program_version_uin
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        else {
            failures.push(format!("{title}: в ответе сервиса нет programVersionUin"));
            continue;
        };
        if update.upgrade_sequence.is_empty() {
            // Пустая цепочка — ошибка, а не успех: иначе версия была бы записана без файлов,
            // и следующий запуск счёл бы конфигурацию обновлённой.
            failures.push(format!(
                "1С: «{title}»: сервис не вернул цепочку обновлений до {version}"
            ));
            continue;
        }

        let mut chain_ok = true;
        for (index, uin) in update.upgrade_sequence.iter().enumerate() {
            ctx.cancel.check()?;
            let what = format!("«{title}»: звено {} из {}", index + 1, update.upgrade_sequence.len());
            if let Err(e) = download_chain_item(
                &client,
                uin,
                program_uin,
                &what,
                template_dir,
                unzip_files,
                progress,
            ) {
                tracing::error!("1С: {what}: {e:#}");
                failures.push(format!("{what}: {e:#}"));
                chain_ok = false;
            }
            // Отмена не является ошибкой скачивания: выходим из запуска, а не копим failures.
            ctx.cancel.check()?;
        }
        // Версия запоминается только когда скачана вся цепочка: иначе следующий запуск
        // начал бы с промежуточной версии.
        if chain_ok {
            state
                .configurations
                .insert(conf.program_name.clone(), version.to_string());
            state
                .save(&ctx.data_dir, &job.id)
                .context("сохранение состояния загрузки обновлений")?;
        }
    }
    Ok(())
}

/// Одно звено цепочки: `1cv8.zip` в `<template_dir>\<templatePath>` и, если включено,
/// распаковка в тот же каталог.
fn download_chain_item(
    client: &api::ApiClient,
    uin: &str,
    program_uin: &str,
    what: &str,
    template_dir: &Path,
    unzip_files: bool,
    progress: &mut Progress,
) -> Result<()> {
    let data = client
        .conf_download(uin, program_uin)?
        .ok_or_else(|| anyhow!("сервис не вернул данные для скачивания"))?;
    // Путь пришёл из ответа сервиса: без `..` и абсолютных частей.
    let dir = template_dir.join(paths::relative_path(&data.template_path)?);
    std::fs::create_dir_all(&dir).with_context(|| format!("создание {}", dir.display()))?;
    let dest = dir.join("1cv8.zip");
    tracing::info!(
        "1С: {what}: скачивание {} ({})",
        data.template_path,
        match data.size {
            Some(size) => size_text(size as u64),
            None => "размер неизвестен".to_string(),
        }
    );
    let bytes = client.download(&data.update_file_url, &dest, what)?;
    progress.files += 1;
    progress.bytes += bytes;
    tracing::info!("1С: {what}: сохранено {}", dest.display());
    if unzip_files {
        unzip::unzip(&dest, &dir).with_context(|| format!("распаковка {}", dest.display()))?;
        tracing::info!("1С: {what}: распаковано в {}", dir.display());
    }
    Ok(())
}

/// Обновление платформы через сервис обновлений: `<platform_dir>\<версия>.zip`.
/// Архив платформы не распаковывается (`unzip_files` действует только на конфигурации).
#[allow(clippy::too_many_arguments)]
fn download_platform(
    ctx: &RunContext,
    job: &Job,
    platform: &DownloadPlatform,
    accounts: &[ItsAccount],
    proxy: Option<&DownloadProxy>,
    insecure_tls: bool,
    check_only: bool,
    platform_dir: &Path,
    state: &mut DownloadState,
    progress: &mut Progress,
    warnings: &mut Vec<String>,
    failures: &mut Vec<String>,
) -> Result<()> {
    let current =
        DownloadState::current(state.platform.as_ref(), &platform.start_version).to_string();
    if current.is_empty() {
        // Работать без версии нельзя: сервис ответил бы про пустую версию.
        warnings.push(
            "не задана начальная версия платформы: проверка обновления платформы пропущена".into(),
        );
        tracing::warn!(
            "1С: начальная версия платформы не задана — проверка обновления платформы пропущена"
        );
        return Ok(());
    }
    ctx.cancel.check()?;
    phase(ctx, &format!("«{}»: проверка обновления платформы", job.name));
    let client = match client_for(accounts, platform.its_account.as_deref(), proxy, insecure_tls) {
        Ok(c) => c,
        Err(e) => {
            failures.push(format!("платформа: {e:#}"));
            return Ok(());
        }
    };
    let update = match client.check_platform_update(&current) {
        Ok(Some(update)) => update,
        Ok(None) => {
            tracing::info!("1С: обновления платформы для {current} нет");
            return Ok(());
        }
        Err(e) => {
            failures.push(format!("платформа: проверка обновлений: {e:#}"));
            return Ok(());
        }
    };
    let Some(version) = update
        .platform_version
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty() && *v != current)
    else {
        tracing::info!("1С: платформа {current} актуальна");
        return Ok(());
    };
    progress.found += 1;
    tracing::info!("1С: доступна версия платформы {version} (сейчас {current})");
    if check_only {
        tracing::info!(
            "1С: платформа {version}: проверка без скачивания (check_only)"
        );
        return Ok(());
    }
    let Some(uin) = update
        .distribution_uin
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        failures.push("платформа: в ответе сервиса нет distributionUin".into());
        return Ok(());
    };
    let result = (|| -> Result<u64> {
        let url = client.platform_download_url(uin)?;
        std::fs::create_dir_all(platform_dir)
            .with_context(|| format!("создание {}", platform_dir.display()))?;
        let dest = platform_dir.join(format!("{version}.zip"));
        phase(
            ctx,
            &format!("«{}»: скачивание платформы {version}", job.name),
        );
        client.download(&url, &dest, &format!("платформа {version}"))
    })();
    match result {
        Ok(bytes) => {
            progress.files += 1;
            progress.bytes += bytes;
            // Версия запоминается только после успешной записи архива.
            state.platform = Some(version.to_string());
            state
                .save(&ctx.data_dir, &job.id)
                .context("сохранение состояния загрузки обновлений")?;
            tracing::info!(
                "1С: платформа {version} сохранена ({})",
                size_text(bytes)
            );
        }
        Err(e) => failures.push(format!("платформа {version}: {e:#}")),
    }
    // Отмена во время скачивания не должна выглядеть как ошибка платформы.
    ctx.cancel.check()?;
    Ok(())
}

/// Дистрибутивы последних версий платформы с releases.1c.ru в
/// `<platform_dir>\<версия>\<файл>`. Уже существующие файлы не скачиваются.
#[allow(clippy::too_many_arguments)]
fn download_releases(
    ctx: &RunContext,
    job: &Job,
    releases: &DownloadReleases,
    platform: &DownloadPlatform,
    accounts: &[ItsAccount],
    proxy: Option<&DownloadProxy>,
    insecure_tls: bool,
    check_only: bool,
    platform_dir: &Path,
    progress: &mut Progress,
    failures: &mut Vec<String>,
) -> Result<()> {
    if !releases.enabled {
        return Ok(());
    }
    if releases.count == 0 || releases.projects.is_empty() || releases.distributions.is_empty() {
        tracing::info!("1С: список дистрибутивов платформы пуст — releases.1c.ru не проверяется");
        return Ok(());
    }
    ctx.cancel.check()?;
    phase(
        ctx,
        &format!("«{}»: вход на releases.1c.ru", job.name),
    );
    // Как в исходной программе: своя запись, иначе запись раздела «Платформа».
    let account_name = releases
        .its_account
        .as_deref()
        .or(platform.its_account.as_deref());
    let (login, password) = account_login(accounts, account_name)
        .context("releases.1c.ru: учётные данные ИТС")?;
    let portal = match api::build_agent(proxy, insecure_tls)
        .and_then(|agent| Portal::login(agent, &login, &password))
    {
        Ok(portal) => portal,
        Err(e) => {
            failures.push(format!("releases.1c.ru: {e:#}"));
            return Ok(());
        }
    };

    let mut all = Vec::new();
    for project in &releases.projects {
        match portal.releases(project) {
            Ok(mut list) => all.append(&mut list),
            Err(e) => failures.push(format!("releases.1c.ru: список версий {project}: {e:#}")),
        }
    }
    if all.is_empty() {
        return Ok(());
    }
    all.sort_by_key(|r| std::cmp::Reverse(r.sort_key()));
    all.truncate(releases.count);
    let list: Vec<String> = all
        .iter()
        .map(|r| format!("{} от {}", r.version, r.date_text()))
        .collect();
    tracing::info!("1С: последние версии платформы: {}", list.join(", "));

    let wanted: Vec<String> = releases
        .distributions
        .iter()
        .map(|name| normalize_name(name))
        .collect();
    for release in &all {
        ctx.cancel.check()?;
        let distributions = match portal.distributions(&release.project, &release.version) {
            Ok(list) => list,
            Err(e) => {
                failures.push(format!(
                    "releases.1c.ru: дистрибутивы версии {}: {e:#}",
                    release.version
                ));
                continue;
            }
        };
        let dir = platform_dir.join(&release.version);
        for name in &wanted {
            let Some(dist) = distributions.iter().find(|d| &d.name == name) else {
                tracing::warn!(
                    "1С: в версии {} нет дистрибутива «{name}»",
                    release.version
                );
                continue;
            };
            progress.found += 1;
            // Имя файла тоже пришло с портала: оставляем только имя без каталогов.
            let file_name = match paths::relative_path(&dist.file_name) {
                Ok(path) if path.components().count() == 1 => path,
                _ => {
                    failures.push(format!(
                        "releases.1c.ru: недопустимое имя файла «{}» у дистрибутива {name}",
                        dist.file_name
                    ));
                    continue;
                }
            };
            let dest = dir.join(&file_name);
            if dest.is_file() {
                tracing::info!(
                    "1С: {} «{name}» уже скачан",
                    release.version
                );
                continue;
            }
            if check_only {
                tracing::info!(
                    "1С: {} «{name}» будет скачан в {} (check_only)",
                    release.version,
                    dest.display()
                );
                continue;
            }
            let what = format!("releases.1c.ru: {} «{name}»", release.version);
            match download_distribution(&portal, dist, &dest, &what) {
                Ok(bytes) => {
                    progress.files += 1;
                    progress.bytes += bytes;
                    tracing::info!("1С: сохранено {} ({})", dest.display(), size_text(bytes));
                }
                Err(e) => {
                    tracing::error!("1С: {what}: {e:#}");
                    failures.push(format!("{what}: {e:#}"));
                }
            }
            // Отмена не является ошибкой скачивания дистрибутива.
            ctx.cancel.check()?;
        }
        ctx.cancel.check()?;
    }
    Ok(())
}

/// Скачивание по основной ссылке, при ошибке — по зеркалам.
fn download_distribution(
    portal: &Portal,
    dist: &Distribution,
    dest: &Path,
    what: &str,
) -> Result<u64> {
    let links = portal.download_links(&dist.page_url)?;
    let mut last_error = None;
    for (index, link) in links.iter().enumerate() {
        match portal.download(link, dest, what) {
            Ok(size) => return Ok(size),
            Err(e) => {
                // Ссылка может содержать авторизацию — в журнал её не пишем.
                tracing::warn!("{what}: ссылка {} не сработала: {e:#}", index + 1);
                last_error = Some(e);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("нет ссылок на скачивание")))
}
