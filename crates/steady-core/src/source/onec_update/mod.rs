//! Обновление конфигурации 1С: отдельный конвейер без назначений, staging,
//! 7-Zip и ротации. Постоянная копия `.dt` создаётся только при найденном плане.

mod cfu;
mod packages;
mod platform;
mod sessions;
mod util;

pub use cfu::{parse_update_info, read_sources as read_cfu_sources, UpdateSource};
pub use packages::{missing_intermediate, plan, scan, select, Package, PackageKind, Scan};
pub use platform::{backup_file_name, ConfigInfo, Target};
pub use sessions::{file_arg_matches, parse_rac_records, rac_session_ids};
pub use util::Version;

use crate::job::{BackupKind, Job, Source};
use crate::pipeline::{RunContext, RunReport};
use anyhow::{anyhow, bail, Context, Result};
use platform::Platform;
use sessions::{ComLock, RacBase};
use std::path::{Path, PathBuf};

pub fn no_op_report(job: &str, warnings: Vec<String>) -> RunReport {
    RunReport {
        job: job.to_string(),
        kind: BackupKind::Full,
        set_name: String::new(),
        items: 0,
        bytes: 0,
        destinations_ok: 0,
        errors: Vec::new(),
        warnings,
        skipped: true,
    }
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn phase(ctx: &RunContext, text: &str) {
    tracing::info!("{text}");
    if let Some(f) = &ctx.on_phase {
        f(text);
    }
}

/// Один запуск без внутренних автоматических повторов. Повторный запуск задания заново
/// читает фактическую версию и строит план от неё.
pub fn run(ctx: &RunContext, job: &Job, source: &Source) -> Result<RunReport> {
    let Source::OneCUpdate {
        ib_path,
        updates_dir,
        backup_dir,
        platform_exe,
        user,
        password,
        unlock_code,
        force,
        server_side,
        run_handlers,
        cluster_user,
        cluster_password,
        ..
    } = source
    else {
        bail!("внутренняя ошибка: источник не является обновлением 1С");
    };
    if ib_path.trim().is_empty() || updates_dir.trim().is_empty() || backup_dir.trim().is_empty() {
        bail!("для обновления 1С обязательны ib_path, updates_dir и backup_dir");
    }
    ctx.cancel.check()?;
    let target = Target::parse(ib_path);
    target.validate()?;
    let exe = platform::resolve_platform(platform_exe.as_deref())?;
    let work = ctx
        .work_dir
        .join(format!("{}_{}", job.id, uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&work)?;
    let _cleanup = Cleanup(work.clone());
    let password = crate::secret::reveal_opt(password.as_deref())?.unwrap_or_default();
    let cluster_password =
        crate::secret::reveal_opt(cluster_password.as_deref())?.unwrap_or_default();
    let platform = Platform {
        exe,
        target,
        user: user.clone().unwrap_or_default(),
        password,
        uc: unlock_code.clone().unwrap_or_default(),
        work,
        timeout: job.process_timeout(),
    };

    phase(ctx, &format!("«{}»: чтение конфигурации 1С", job.name));
    let cfg = platform.read_config()?;
    let current = Version::parse(&cfg.version).ok_or_else(|| {
        anyhow!(
            "у конфигурации «{}» не задана версия — обновлять нечем",
            cfg.title()
        )
    })?;
    tracing::info!(
        "1С: конфигурация «{}» ({}), поставщик «{}», версия {}",
        cfg.title(),
        cfg.name,
        cfg.vendor,
        current
    );

    phase(ctx, &format!("«{}»: поиск пакетов обновления", job.name));
    let roots = vec![PathBuf::from(updates_dir)];
    let scanned = packages::scan(&roots)?;
    let names = [cfg.name.as_str(), cfg.synonym.as_str()];
    let mut own = Vec::new();
    let mut warnings = scanned.warnings;
    for p in &scanned.packages {
        match packages::select(p, &cfg.vendor, &names) {
            Ok(Some(p)) => own.push(p),
            Ok(None) => {}
            Err(e) => warnings.push(e),
        }
    }
    for warning in &warnings {
        tracing::warn!("1С: {warning}");
    }
    let chain = packages::plan(&own, &current, None);
    if chain.is_empty() {
        if let Some(newest) = packages::missing_intermediate(&own, &current, &chain) {
            bail!(
                "есть обновление до {newest}, но ни один пакет не подходит для текущей версии {current} — нужны промежуточные обновления"
            );
        }
        tracing::info!(
            "1С: подходящей цепочки обновления нет; сеансы не блокируются, .dt не создаётся"
        );
        return Ok(no_op_report(&job.name, warnings));
    }
    let newest = chain
        .last()
        .map(|p| p.version.to_string())
        .unwrap_or_else(|| current.to_string());
    tracing::info!(
        "1С: план обновления {} → {}, шагов {}",
        current,
        newest,
        chain.len()
    );

    let backup_dir = PathBuf::from(backup_dir);
    std::fs::create_dir_all(&backup_dir).with_context(|| {
        format!(
            "создание каталога постоянных копий {}",
            backup_dir.display()
        )
    })?;
    let stamp = chrono::Local::now().format("%Y%m%d_%H%M%S").to_string();
    let final_dt = unique_backup_path(
        &backup_dir,
        &backup_file_name(&platform.target, &current, &stamp),
    );

    // Для серверной базы сначала проверяем RAC и отдельные права администратора кластера.
    // Ошибка прав возвращается до блокировки, .dt и любого изменения конфигурации.
    let rac = if matches!(platform.target, Target::Server { .. }) {
        Some(RacBase::discover(
            &platform,
            cluster_user.as_deref().unwrap_or(""),
            &cluster_password,
        )?)
    } else {
        None
    };

    phase(
        ctx,
        &format!("«{}»: блокировка и завершение сеансов 1С", job.name),
    );
    let lock = ComLock::acquire(&platform)?;
    let mut update_started = false;
    let mut published_dt: Option<PathBuf> = None;
    let operation = (|| -> Result<()> {
        if let Some(rac) = &rac {
            rac.terminate_all()?;
        } else {
            sessions::terminate_local_file_processes(&platform)?;
        }
        sessions::ensure_no_other_com_sessions(&platform)?;
        ctx.cancel.check()?;

        phase(
            ctx,
            &format!("«{}»: выгрузка .dt перед обновлением", job.name),
        );
        let temp_dt = temporary_backup_path(&final_dt);
        if let Err(e) = platform.dump_ib(&temp_dt) {
            let _ = std::fs::remove_file(&temp_dt);
            return Err(e).context("резервная копия не создана, обновление не начиналось");
        }
        let size = std::fs::metadata(&temp_dt).map(|m| m.len()).unwrap_or(0);
        if size == 0 {
            let _ = std::fs::remove_file(&temp_dt);
            bail!("резервная копия пуста, обновление не начиналось");
        }
        if let Err(e) = std::fs::rename(&temp_dt, &final_dt) {
            let _ = std::fs::remove_file(&temp_dt);
            return Err(anyhow::Error::new(e)).context(format!(
                "публикация постоянной копии {}",
                final_dt.display()
            ));
        }
        published_dt = Some(final_dt.clone());
        tracing::info!(
            "1С: постоянная копия {} ({} байт)",
            final_dt.display(),
            size
        );

        for (index, package) in chain.iter().enumerate() {
            ctx.cancel.check()?;
            phase(
                ctx,
                &format!(
                    "«{}»: шаг {}/{}, обновление до {}",
                    job.name,
                    index + 1,
                    chain.len(),
                    package.version
                ),
            );
            // Даже ошибка запуска/неизвестный код после этой точки означает, что база могла
            // измениться: пользователей не впускаем до ручной проверки и восстановления.
            update_started = true;
            let result = platform.update(&package.file, *force, *server_side)?;
            if !result.log.trim().is_empty() {
                tracing::info!("1С: {}", result.log.trim());
            }
            let actual = platform.read_config()?;
            let actual_version = Version::parse(&actual.version);
            if actual_version.as_ref() != Some(&package.version) {
                bail!(
                    "после шага обновления версия конфигурации {} вместо {}",
                    if actual.version.is_empty() {
                        "не задана".into()
                    } else {
                        actual.version
                    },
                    package.version
                );
            }
        }
        if *run_handlers {
            phase(ctx, &format!("«{}»: обработчики обновления 1С", job.name));
            sessions::run_handlers(&platform)?;
        }
        Ok(())
    })();

    let keep_locked = update_started && operation.is_err();
    let restore = if keep_locked {
        Ok(())
    } else {
        lock.restore(&platform)
    };
    if let Err(restore_error) = restore {
        let backup = published_dt
            .as_deref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "не создана".into());
        let combined = match operation {
            Ok(()) => anyhow!(
                "обновление выполнено, но исходная блокировка сеансов не восстановлена: {restore_error:#}. Постоянная копия: {backup}"
            ),
            Err(e) => anyhow!("{e:#}; дополнительно не удалось восстановить блокировку сеансов: {restore_error:#}. Постоянная копия: {backup}"),
        };
        return Err(combined);
    }
    if let Err(e) = operation {
        if keep_locked {
            let backup = published_dt
                .as_deref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "не найдена".into());
            return Err(e).context(format!(
                "изменение конфигурации уже началось; блокировка сеансов оставлена включённой. Разберитесь с состоянием базы и при необходимости восстановите её из {backup}; не впускайте пользователей в промежуточное состояние"
            ));
        }
        return Err(e);
    }

    let dt = published_dt.context("обновление завершено без подтверждённой постоянной копии")?;
    let bytes = std::fs::metadata(&dt)
        .with_context(|| format!("чтение размера {}", dt.display()))?
        .len();
    Ok(RunReport {
        job: job.name.clone(),
        kind: BackupKind::Full,
        set_name: dt.display().to_string(),
        items: 1,
        bytes,
        destinations_ok: 0,
        errors: Vec::new(),
        warnings,
        skipped: false,
    })
}

fn unique_backup_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("backup");
    for i in 2..10_000 {
        let candidate = dir.join(format!("{stem}_{i}.dt"));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{stem}_{}.dt", uuid::Uuid::new_v4().simple()))
}

fn temporary_backup_path(final_dt: &Path) -> PathBuf {
    let name = final_dt
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("backup.dt");
    final_dt.with_file_name(format!(".{name}.{}.partial", uuid::Uuid::new_v4().simple()))
}
