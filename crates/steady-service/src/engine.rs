//! Движок: очередь заданий, исполнитель и планировщик.

use chrono::{Local, NaiveDateTime};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use steady_core::state::StateStore;
use steady_core::group::RetryOverride;
use steady_core::store::{HistoryEntry, Store};
use steady_core::{run_job, RunContext};
use uuid::Uuid;

fn fmt_bytes(b: u64) -> String {
    let units = ["Б", "КБ", "МБ", "ГБ", "ТБ"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} Б") } else { format!("{v:.1} {}", units[i]) }
}

/// Пропущенный запуск старше этого срока считается неактуальным, если run_missed = false.
const MISSED_GRACE_MIN: i64 = 5;

#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub job_id: Uuid,
    pub job: String,
    pub started: chrono::DateTime<Local>,
    pub phase: String,
}

/// Запуск в очереди: задание, повторы (свои или группы) и номер перезапуска.
#[derive(Debug, Clone, Copy)]
struct Queued {
    id: Uuid,
    retry: RetryOverride,
    /// 0 — первый запуск, N — N-й перезапуск после ошибки.
    restart: u32,
}

#[derive(Default)]
struct Inner {
    queue: VecDeque<Queued>,
    /// Перезапуски после ошибки, ждущие своей паузы. В памяти: остановка службы их сбрасывает.
    delayed: Vec<(std::time::Instant, Queued)>,
    running: Option<Running>,
    stop: bool,
    /// Идёт установка обновления: исполнитель не берёт задания, установщик вот-вот остановит службу.
    updating: bool,
    /// Флаг отмены текущего запуска. Новый на каждое задание: однажды выставленный флаг
    /// не сбрасывается, и общий флаг после одной отмены прерывал бы все следующие задания.
    cancel: steady_core::process::CancelFlag,
}

pub struct Engine {
    pub store: Store,
    pub data_dir: PathBuf,
    pub seven_zip: Option<PathBuf>,
    pub updater: crate::updater::Updater,
    worker_done: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    inner: Mutex<Inner>,
    cv: Condvar,
}

impl Engine {
    /// `console` — `steady console` от имени пользователя: задания этого пользователя загружаются
    /// (у службы — только файлы, принадлежащие SYSTEM или «Администраторам»).
    pub fn new(data_dir: PathBuf, seven_zip: Option<PathBuf>, console: bool) -> Arc<Self> {
        Arc::new(Self {
            store: if console { Store::new(&data_dir) } else { Store::for_service(&data_dir) },
            updater: crate::updater::Updater::new(data_dir.clone(), console),
            data_dir,
            seven_zip,
            worker_done: Mutex::new(None),
            inner: Mutex::default(),
            cv: Condvar::new(),
        })
    }

    pub fn start(self: &Arc<Self>) {
        // Авария или отключение питания оставляют staging. Чужие файлы в каталоге не удаляются.
        match steady_core::settings::work_dir(&self.data_dir) {
            Ok(work) => match steady_core::settings::clean_work_dir(&work) {
                Ok(0) => {}
                Ok(n) => tracing::info!("удалены недоделанные каталоги заданий: {n} ({})", work.display()),
                Err(e) => tracing::error!("очистка рабочего каталога {}: {e:#}", work.display()),
            },
            Err(e) => tracing::error!("рабочий каталог не очищен: {e:#}"),
        }
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        *self.worker_done.lock().unwrap() = Some(done_rx);
        let e = self.clone();
        std::thread::Builder::new().name("worker".into()).spawn(move || {
            e.worker();
            let _ = done_tx.send(());
        }).unwrap();
        let e = self.clone();
        std::thread::Builder::new().name("scheduler".into()).spawn(move || e.scheduler()).unwrap();
        crate::updater::spawn(self.clone());
    }

    /// Есть выполняющееся, стоящее в очереди или ждущее перезапуска задание.
    pub fn is_busy(&self) -> bool {
        let g = self.inner.lock().unwrap();
        g.running.is_some() || !g.queue.is_empty() || !g.delayed.is_empty()
    }

    /// Начать установку обновления: только если служба свободна. Пока признак стоит,
    /// исполнитель не берёт задания из очереди.
    pub fn begin_update(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        if g.stop || g.running.is_some() || !g.queue.is_empty() || !g.delayed.is_empty() {
            return false;
        }
        g.updating = true;
        true
    }

    /// Установка не состоялась: задания снова выполняются.
    pub fn end_update(&self) {
        self.inner.lock().unwrap().updating = false;
        self.cv.notify_all();
    }

    /// Пауза, которую прерывает остановка службы. false — служба останавливается.
    pub fn sleep_unless_stopped(&self, d: Duration) -> bool {
        let end = std::time::Instant::now() + d;
        let mut g = self.inner.lock().unwrap();
        loop {
            if g.stop {
                return false;
            }
            let now = std::time::Instant::now();
            if now >= end {
                return true;
            }
            g = self.cv.wait_timeout(g, end - now).unwrap().0;
        }
    }

    pub fn stop(&self) {
        let mut g = self.inner.lock().unwrap();
        g.stop = true;
        g.cancel.request("прервано остановкой службы");
        drop(g);
        self.cv.notify_all();
    }

    /// Снимает выполняющееся задание или убирает его из очереди.
    pub fn cancel_job(&self, id: Uuid) -> Result<(), &'static str> {
        let mut g = self.inner.lock().unwrap();
        if g.running.as_ref().is_some_and(|r| r.job_id == id) {
            g.cancel.request("задание отменено");
            return Ok(());
        }
        let before = g.queue.len() + g.delayed.len();
        g.queue.retain(|q| q.id != id);
        g.delayed.retain(|(_, q)| q.id != id);
        if g.queue.len() + g.delayed.len() < before {
            return Ok(());
        }
        Err("задание не выполняется")
    }

    /// Ждёт, пока исполнитель выйдет из текущего задания. false — не успел.
    pub fn wait_worker(&self, timeout: Duration) -> bool {
        let rx = self.worker_done.lock().unwrap().take();
        match rx {
            Some(rx) => rx.recv_timeout(timeout).is_ok(),
            None => true,
        }
    }

    /// Ставит задание в очередь. false — уже в очереди или выполняется.
    pub fn enqueue(&self, id: Uuid) -> bool {
        self.enqueue_with(id, RetryOverride::default())
    }

    /// Ставит задание в очередь с повторами группы. Ждущий перезапуск заменяется новым запуском.
    fn enqueue_with(&self, id: Uuid, retry: RetryOverride) -> bool {
        let mut g = self.inner.lock().unwrap();
        // Во время установки обновления служба вот-вот остановится, очередь пропадёт.
        if g.updating || g.queue.iter().any(|q| q.id == id) || g.running.as_ref().is_some_and(|r| r.job_id == id) {
            return false;
        }
        g.delayed.retain(|(_, q)| q.id != id);
        g.queue.push_back(Queued { id, retry, restart: 0 });
        self.cv.notify_all();
        true
    }

    /// Выполняющееся задание и очередь; ждущие перезапуска идут в конце очереди.
    pub fn snapshot(&self) -> (Option<Running>, Vec<Uuid>) {
        let g = self.inner.lock().unwrap();
        let mut ids: Vec<Uuid> = g.queue.iter().map(|q| q.id).collect();
        ids.extend(g.delayed.iter().map(|(_, q)| q.id));
        (g.running.clone(), ids)
    }

    fn worker(self: Arc<Self>) {
        loop {
            let item = {
                let mut g = self.inner.lock().unwrap();
                loop {
                    if g.stop {
                        return;
                    }
                    let now = std::time::Instant::now();
                    let (due, wait): (Vec<_>, Vec<_>) =
                        std::mem::take(&mut g.delayed).into_iter().partition(|(at, _)| *at <= now);
                    g.delayed = wait;
                    g.queue.extend(due.into_iter().map(|(_, q)| q));
                    if !g.updating {
                        if let Some(q) = g.queue.pop_front() {
                            break q;
                        }
                    }
                    g = match g.delayed.iter().map(|(at, _)| *at).min() {
                        Some(at) => self.cv.wait_timeout(g, at.saturating_duration_since(now)).unwrap().0,
                        None => self.cv.wait(g).unwrap(),
                    };
                }
            };
            self.execute_item(item);
        }
    }

    #[cfg(test)]
    fn execute(self: &Arc<Self>, id: Uuid) {
        self.execute_item(Queued { id, retry: RetryOverride::default(), restart: 0 });
    }

    fn execute_item(self: &Arc<Self>, item: Queued) {
        let id = item.id;
        let mut job = match self.store.load_jobs().map(|v| v.into_iter().find(|j| j.id == id)) {
            Ok(Some(j)) => j,
            Ok(None) => return,
            Err(e) => {
                tracing::error!("загрузка заданий: {e:#}");
                return;
            }
        };
        item.retry.apply(&mut job);
        let started = Local::now();
        // Флаг и `running` меняются под одной блокировкой: отмена или остановка между ними
        // не попадёт в флаг прошлого задания.
        let cancel = {
            let mut g = self.inner.lock().unwrap();
            let flag = steady_core::process::CancelFlag::new();
            if g.stop {
                flag.request("прервано остановкой службы");
            }
            g.cancel = flag.clone();
            let phase =
                if item.restart > 0 { format!("перезапуск {} из {}", item.restart, job.restarts) } else { "запуск".into() };
            g.running = Some(Running { job_id: id, job: job.name.clone(), started, phase });
            flag
        };
        let cancel_seen = cancel.clone();

        let me = self.clone();
        let work_dir = steady_core::settings::work_dir(&self.data_dir);
        let ctx = work_dir.as_ref().ok().map(|work_dir| RunContext {
            data_dir: self.data_dir.clone(),
            work_dir: work_dir.clone(),
            state: StateStore::new(self.data_dir.join("state")),
            seven_zip: self.seven_zip.clone(),
            on_phase: Some(Box::new(move |p: &str| {
                if let Some(r) = me.inner.lock().unwrap().running.as_mut() {
                    r.phase = p.to_string();
                }
            })),
            cancel,
        });
        tracing::info!("старт задания «{}»", job.name);
        // Паника в задании не должна ронять службу.
        let result = match (work_dir, ctx) {
            (Err(e), _) => Err(e),
            (_, None) => unreachable!(),
            (Ok(_), Some(ctx)) => std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&ctx, &job)))
                .unwrap_or_else(|p| {
                    let msg = p.downcast_ref::<String>().cloned().or(p.downcast_ref::<&str>().map(|s| s.to_string()));
                    Err(anyhow::anyhow!("внутренняя ошибка: {}", msg.unwrap_or_default()))
                }),
        };

        // Отмену и остановку службы не перезапускаем.
        let restart_left = item.restart < job.restarts && !cancel_seen.is_set();
        let mut entry = match result {
            Ok(r) => {
                tracing::info!("задание «{}» выполнено", job.name);
                HistoryEntry {
                    job_id: id,
                    job: job.name.clone(),
                    started,
                    finished: Local::now(),
                    ok: true,
                    warnings: r.errors.iter().chain(&r.warnings).cloned().collect(),
                    error: None,
                    report: Some(r),
                }
            }
            Err(e) => {
                tracing::error!("задание «{}»: {e:#}", job.name);
                HistoryEntry {
                    job_id: id,
                    job: job.name.clone(),
                    started,
                    finished: Local::now(),
                    ok: false,
                    warnings: vec![],
                    error: Some(format!("{e:#}")),
                    report: None,
                }
            }
        };
        let mut retry_at = None;
        if !entry.ok && restart_left {
            let n = item.restart + 1;
            let delay = Duration::from_secs(u64::from(job.restart_delay_minutes) * 60);
            let note = format!("будет перезапуск {n} из {} через {} мин", job.restarts, job.restart_delay_minutes);
            tracing::warn!("задание «{}»: {note}", job.name);
            if let Some(e) = entry.error.as_mut() {
                e.push_str(&format!(" ({note})"));
            }
            retry_at = Some((std::time::Instant::now() + delay, Queued { restart: n, ..item }));
        }
        if let Err(e) = self.store.append_history(&entry) {
            tracing::error!("запись журнала: {e:#}");
        }
        {
            let mut g = self.inner.lock().unwrap();
            g.running = None;
            if let Some(r) = retry_at {
                if !g.stop {
                    g.delayed.push(r);
                }
            }
        }
        // Об ошибке, после которой будет перезапуск, не уведомляем: придёт итог последнего запуска.
        if retry_at.is_none() {
            self.notify(&entry);
        }
    }

    /// Уведомление в фоне: медленный SMTP не должен задерживать следующее задание.
    fn notify(&self, h: &HistoryEntry) {
        use steady_core::notify::{self, Event, NotifySettings, Outcome};
        let settings = NotifySettings::load(&self.data_dir);
        if settings.email.as_ref().is_none_or(|e| !e.enabled) && settings.telegram.as_ref().is_none_or(|t| !t.enabled)
        {
            return;
        }
        let outcome = if !h.ok {
            Outcome::Error
        } else if h.warnings.is_empty() {
            Outcome::Ok
        } else {
            Outcome::Warning
        };
        let mut details = Vec::new();
        if let Some(e) = &h.error {
            details.push(e.clone());
        }
        if let Some(r) = &h.report {
            let kind = match r.kind {
                steady_core::BackupKind::Full => "полная",
                steady_core::BackupKind::Incremental => "инкрементная",
                steady_core::BackupKind::Differential => "дифференциальная",
            };
            details.push(format!("Копия: {} ({kind})", r.set_name));
            details.push(format!("Объём данных: {}, файлов: {}", fmt_bytes(r.bytes), r.items));
            details.push(format!("Сохранено в назначений: {}", r.destinations_ok));
        }
        details.extend(h.warnings.iter().map(|w| format!("⚠ {w}")));
        let ev = Event { job: h.job.clone(), outcome, started: h.started, finished: h.finished, details };
        std::thread::spawn(move || {
            for err in notify::send(&settings, &ev) {
                tracing::error!("уведомление: {err}");
            }
        });
    }

    fn scheduler(self: Arc<Self>) {
        loop {
            if let Err(e) = self.tick(Local::now().naive_local()) {
                tracing::error!("планировщик: {e:#}");
            }
            let g = self.inner.lock().unwrap();
            let (g, _) = self.cv.wait_timeout(g, Duration::from_secs(15)).unwrap();
            if g.stop {
                return;
            }
        }
    }

    /// Одна проверка расписания. Метка задания — момент, до которого расписание уже обработано.
    fn tick(&self, now: NaiveDateTime) -> anyhow::Result<()> {
        // Во время обновления метки не двигаем: подошедший запуск новая служба выполнит
        // как пропущенный (по правилам run_missed), а не потеряет.
        if self.inner.lock().unwrap().updating {
            return Ok(());
        }
        let jobs = self.store.load_jobs()?;
        let mut marks = self.store.load_marks();
        let before = marks.clone();
        // Отключённые задания метку не хранят: иначе включение спустя дни станет «пропущенным» запуском.
        marks.retain(|id, _| jobs.iter().any(|j| j.id == *id && j.enabled));

        for job in jobs.iter().filter(|j| j.enabled) {
            let mark = *marks.entry(job.id).or_insert(now);
            let Some(next) = job.schedule.next_after(mark) else { continue };
            if next > now {
                continue;
            }
            marks.insert(job.id, now);
            let late = (now - next).num_minutes();
            if late > MISSED_GRACE_MIN && !job.run_missed {
                tracing::info!("«{}»: пропущенный запуск {next} не выполняется (run_missed = false)", job.name);
                continue;
            }
            tracing::info!("«{}»: запуск по расписанию ({next})", job.name);
            self.enqueue(job.id);
        }
        if marks != before {
            self.store.save_marks(&marks)?;
        }
        self.tick_groups(&jobs, now)
    }

    /// Расписания групп: те же правила меток и пропущенных запусков, что у заданий.
    fn tick_groups(&self, jobs: &[steady_core::Job], now: NaiveDateTime) -> anyhow::Result<()> {
        let groups = self.store.load_groups()?;
        let mut marks = self.store.load_group_marks();
        let before = marks.clone();
        marks.retain(|id, _| groups.iter().any(|g| g.id == *id && g.enabled));
        for g in groups.iter().filter(|g| g.enabled) {
            let mark = *marks.entry(g.id).or_insert(now);
            let Some(next) = g.schedule.next_after(mark) else { continue };
            if next > now {
                continue;
            }
            marks.insert(g.id, now);
            let late = (now - next).num_minutes();
            if late > MISSED_GRACE_MIN && !g.run_missed {
                tracing::info!("группа «{}»: пропущенный запуск {next} не выполняется (run_missed = false)", g.name);
                continue;
            }
            let n = self.enqueue_group(g, jobs);
            tracing::info!("группа «{}»: запуск по расписанию ({next}), заданий в очередь: {n}", g.name);
        }
        if marks != before {
            self.store.save_group_marks(&marks)?;
        }
        Ok(())
    }

    /// Ставит в очередь включённые задания группы по порядку. Уже стоящие в очереди или
    /// выполняющиеся пропускаются. Возвращает число поставленных.
    pub fn enqueue_group(&self, g: &steady_core::group::Group, jobs: &[steady_core::Job]) -> usize {
        let retry = RetryOverride::from_group(g);
        steady_core::group::members(g, jobs)
            .into_iter()
            .filter(|j| j.enabled)
            .filter(|j| self.enqueue_with(j.id, retry))
            .count()
    }

    /// После сохранения группы — как `on_job_saved`.
    pub fn on_group_saved(
        &self,
        previous: Option<&steady_core::group::Group>,
        g: &steady_core::group::Group,
        now: NaiveDateTime,
    ) {
        let mut marks = self.store.load_group_marks();
        let changed = previous.is_none_or(|p| p.schedule != g.schedule || p.enabled != g.enabled);
        if !g.enabled {
            marks.remove(&g.id);
        } else if changed {
            marks.insert(g.id, now);
        }
        if let Err(e) = self.store.save_group_marks(&marks) {
            tracing::error!("метка расписания группы: {e:#}");
        }
    }

    pub fn next_group_run(&self, g: &steady_core::group::Group) -> Option<NaiveDateTime> {
        if !g.enabled {
            return None;
        }
        let from = self.store.load_group_marks().get(&g.id).copied().unwrap_or_else(|| Local::now().naive_local());
        g.schedule.next_after(from)
    }

    pub fn next_run(&self, job: &steady_core::Job) -> Option<NaiveDateTime> {
        if !job.enabled {
            return None;
        }
        let marks = self.store.load_marks();
        let from = marks.get(&job.id).copied().unwrap_or_else(|| Local::now().naive_local());
        job.schedule.next_after(from)
    }

    pub fn wake(&self) {
        self.cv.notify_all();
    }

    /// После сохранения задания. Смена расписания или включения не должна тут же считаться пропущенным запуском.
    pub fn on_job_saved(&self, previous: Option<&steady_core::Job>, job: &steady_core::Job, now: NaiveDateTime) {
        let mut marks = self.store.load_marks();
        let changed = previous.is_none_or(|p| p.schedule != job.schedule || p.enabled != job.enabled);
        if !job.enabled {
            marks.remove(&job.id);
        } else if changed {
            marks.insert(job.id, now);
        }
        if let Err(e) = self.store.save_marks(&marks) {
            tracing::error!("метка расписания: {e:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn engine() -> (std::path::PathBuf, Arc<Engine>) {
        let dir = std::env::temp_dir().join(format!("steady-tick-{}", Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let e = Arc::new(Engine {
            store: Store::new(&dir),
            updater: crate::updater::Updater::new(dir.clone(), true),
            data_dir: dir.clone(),
            seven_zip: None,
            worker_done: Mutex::new(None),
            inner: Mutex::default(),
            cv: Condvar::new(),
        });
        (dir, e)
    }

    fn job(id: Uuid, time: &str) -> steady_core::Job {
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"t","source":{{"type":"files","paths":["C:\\x"],"kind":"full"}},"destinations":[{{"type":"local","path":"C:\\d"}}],"schedule":{{"type":"daily","times":["{time}"]}}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn reschedule_to_earlier_time_does_not_run_immediately() {
        let (dir, e) = engine();
        let id = Uuid::new_v4();
        let old = job(id, "23:00:00");
        e.store.save_job(&old).unwrap();
        let now = Local::now().naive_local();
        let mut marks = std::collections::HashMap::new();
        marks.insert(id, now - Duration::days(2));
        e.store.save_marks(&marks).unwrap();

        e.tick(now).unwrap();
        assert!(e.snapshot().1.contains(&id), "старая метка должна дать пропущенный запуск");
        e.inner.lock().unwrap().queue.clear();

        let new = job(id, "01:00:00");
        e.store.save_job(&new).unwrap();
        e.on_job_saved(Some(&old), &new, now);
        e.tick(now).unwrap();
        assert!(e.snapshot().1.is_empty(), "правка расписания не запускает задание сразу");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn group(id: Uuid, time: &str, order: Vec<Uuid>) -> steady_core::group::Group {
        serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"g","schedule":{{"type":"daily","times":["{time}"]}},"order":{}}}"#,
            serde_json::to_string(&order).unwrap()
        ))
        .unwrap()
    }

    /// Расписание группы ставит её включённые задания в очередь по порядку группы.
    #[test]
    fn group_schedule_enqueues_members_in_order() {
        let (dir, e) = engine();
        let gid = Uuid::new_v4();
        let (a, b, off, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let member = |id: Uuid, enabled: bool, group: Option<Uuid>| {
            let mut j = job(id, "23:59:59");
            j.name = id.to_string();
            j.schedule = steady_core::schedule::Schedule::Manual;
            j.enabled = enabled;
            j.group = group;
            e.store.save_job(&j).unwrap();
        };
        member(a, true, Some(gid));
        member(b, true, Some(gid));
        member(off, false, Some(gid));
        member(other, true, None);
        e.store.save_group(&group(gid, "23:00:00", vec![b, a])).unwrap();

        let now = Local::now().naive_local();
        let mut marks = std::collections::HashMap::new();
        marks.insert(gid, now - Duration::days(2));
        e.store.save_group_marks(&marks).unwrap();

        e.tick(now).unwrap();
        assert_eq!(e.snapshot().1, vec![b, a], "порядок группы, без отключённых и чужих");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Правка расписания группы на прошедшее время не запускает её сразу.
    #[test]
    fn group_reschedule_does_not_run_immediately() {
        let (dir, e) = engine();
        let gid = Uuid::new_v4();
        let a = Uuid::new_v4();
        let mut j = job(a, "23:59:59");
        j.schedule = steady_core::schedule::Schedule::Manual;
        j.group = Some(gid);
        e.store.save_job(&j).unwrap();
        let old = group(gid, "23:00:00", vec![]);
        e.store.save_group(&old).unwrap();
        let now = Local::now().naive_local();
        let mut marks = std::collections::HashMap::new();
        marks.insert(gid, now - Duration::days(2));
        e.store.save_group_marks(&marks).unwrap();

        let new = group(gid, "01:00:00", vec![]);
        e.store.save_group(&new).unwrap();
        e.on_group_saved(Some(&old), &new, now);
        e.tick(now).unwrap();
        assert!(e.snapshot().1.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Задание без назначений всегда падает: бьёт перезапуски до исчерпания.
    fn failing_job(e: &Engine, restarts: u32, group: Option<Uuid>) -> Uuid {
        let id = Uuid::new_v4();
        let mut j: steady_core::Job = serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"t","source":{{"type":"files","paths":["C:\\x"],"kind":"full"}},"destinations":[]}}"#
        ))
        .unwrap();
        j.restarts = restarts;
        j.restart_delay_minutes = 0;
        j.group = group;
        e.store.save_job(&j).unwrap();
        id
    }

    /// Выполняет очередь, включая ждущие перезапуски с нулевой паузой.
    fn drain(e: &Arc<Engine>) -> usize {
        let mut runs = 0;
        loop {
            let item = {
                let mut g = e.inner.lock().unwrap();
                let delayed = std::mem::take(&mut g.delayed);
                g.queue.extend(delayed.into_iter().map(|(_, q)| q));
                g.queue.pop_front()
            };
            let Some(item) = item else { return runs };
            e.execute_item(item);
            runs += 1;
            assert!(runs < 20, "перезапуски не кончаются");
        }
    }

    #[test]
    fn failed_job_restarts_given_times() {
        let (dir, e) = engine();
        let id = failing_job(&e, 2, None);
        e.enqueue(id);
        assert_eq!(drain(&e), 3, "первый запуск и два перезапуска");
        let h = e.store.history(10).unwrap();
        assert_eq!(h.iter().filter(|x| x.job_id == id && !x.ok).count(), 3);
        assert!(h.iter().any(|x| x.error.as_deref().is_some_and(|s| s.contains("перезапуск 1 из 2"))));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Запуск группы берёт перезапуски группы; ручной запуск задания — свои.
    #[test]
    fn group_restarts_override_job() {
        let (dir, e) = engine();
        let gid = Uuid::new_v4();
        let id = failing_job(&e, 0, Some(gid));
        let mut g = group(gid, "23:00:00", vec![id]);
        g.restarts = 1;
        e.store.save_group(&g).unwrap();
        assert_eq!(e.enqueue_group(&g, &e.store.load_jobs().unwrap()), 1);
        assert_eq!(drain(&e), 2);
        e.enqueue(id);
        assert_eq!(drain(&e), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Отменённое задание не перезапускается, а отмена снимает ждущий перезапуск.
    #[test]
    fn cancel_removes_pending_restart() {
        let (dir, e) = engine();
        let id = failing_job(&e, 3, None);
        e.execute(id);
        assert_eq!(e.snapshot().1, vec![id], "перезапуск ждёт");
        e.cancel_job(id).unwrap();
        assert!(e.snapshot().1.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Отмена одного задания не должна прерывать следующие запуски.
    #[test]
    fn cancel_does_not_affect_next_run() {
        let (dir, e) = engine();
        let src = dir.join("src");
        let dst = dir.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.txt"), b"x").unwrap();
        let id = Uuid::new_v4();
        let esc = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
        let job: steady_core::Job = serde_json::from_str(&format!(
            r#"{{"id":"{id}","name":"t","source":{{"type":"files","paths":["{}"],"kind":"full","vss":false}},"destinations":[{{"type":"local","path":"{}"}}],"archive":{{"enabled":false}}}}"#,
            esc(&src),
            esc(&dst)
        ))
        .unwrap();
        e.store.save_job(&job).unwrap();

        // Отмена выполняющегося задания (как кнопка «Отменить» в GUI).
        e.inner.lock().unwrap().running =
            Some(Running { job_id: id, job: "t".into(), started: Local::now(), phase: String::new() });
        e.cancel_job(id).unwrap();
        e.inner.lock().unwrap().running = None;

        e.execute(id);
        let last = e.store.last(&id).expect("запись журнала");
        assert!(last.ok, "следующий запуск прерван старой отменой: {:?}", last.error);
        std::fs::remove_dir_all(&dir).ok();
    }
}
