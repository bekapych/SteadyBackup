//! Группы заданий: общее расписание для нескольких заданий.
//!
//! Задание входит не больше чем в одну группу (`Job::group`). По расписанию группы её
//! включённые задания ставятся в общую очередь по порядку `order`; ошибка одного задания
//! не отменяет остальные. Собственное расписание задания при этом продолжает действовать.

use crate::job::Job;
use crate::schedule::Schedule;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
    /// Отключённая группа по расписанию не запускается; вручную — можно.
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub schedule: Schedule,
    /// Выполнить пропущенный запуск группы, как только появится возможность.
    #[serde(default = "yes")]
    pub run_missed: bool,
    /// Порядок запуска: id заданий. Задания группы, которых нет в списке, идут после — по имени.
    #[serde(default)]
    pub order: Vec<Uuid>,
    /// При запуске группы заменяет `Job::restarts` её заданий. 0 — как в задании.
    #[serde(default)]
    pub restarts: u32,
    /// При запуске группы заменяет `Job::restart_delay_minutes`. 0 — как в задании.
    #[serde(default)]
    pub restart_delay_minutes: u32,
    /// При запуске группы заменяет `Job::attempts`. 0 — как в задании.
    #[serde(default)]
    pub attempts: u32,
}

fn yes() -> bool {
    true
}

/// Повторы для одного запуска задания: собственные или заданные группой.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetryOverride {
    pub restarts: Option<u32>,
    pub restart_delay_minutes: Option<u32>,
    pub attempts: Option<u32>,
}

impl RetryOverride {
    /// Ненулевые значения группы.
    pub fn from_group(g: &Group) -> Self {
        let nz = |v: u32| (v > 0).then_some(v);
        Self { restarts: nz(g.restarts), restart_delay_minutes: nz(g.restart_delay_minutes), attempts: nz(g.attempts) }
    }

    /// Задание с подставленными значениями группы.
    pub fn apply(&self, job: &mut Job) {
        if let Some(v) = self.restarts {
            job.restarts = v;
        }
        if let Some(v) = self.restart_delay_minutes {
            job.restart_delay_minutes = v;
        }
        if let Some(v) = self.attempts {
            job.attempts = v;
        }
    }
}

/// Задания группы в порядке запуска (включая отключённые — их отсеивает вызывающий).
pub fn members<'a>(group: &Group, jobs: &'a [Job]) -> Vec<&'a Job> {
    let mut v: Vec<&Job> = jobs.iter().filter(|j| j.group == Some(group.id)).collect();
    let pos = |j: &Job| group.order.iter().position(|id| *id == j.id).unwrap_or(usize::MAX);
    v.sort_by(|a, b| pos(a).cmp(&pos(b)).then_with(|| a.name.cmp(&b.name)));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: u128, name: &str, group: Option<Uuid>) -> Job {
        let mut j: Job = serde_json::from_str(&format!(
            r#"{{"id":"{}","name":"{name}","source":{{"type":"files","paths":["C:\\x"],"kind":"full"}},"destinations":[]}}"#,
            Uuid::from_u128(id)
        ))
        .unwrap();
        j.group = group;
        j
    }

    #[test]
    fn members_follow_order_then_name() {
        let g = Uuid::from_u128(100);
        let group = Group {
            id: g,
            name: "Ночь".into(),
            enabled: true,
            schedule: Schedule::Manual,
            run_missed: true,
            order: vec![Uuid::from_u128(3), Uuid::from_u128(1)],
            restarts: 0,
            restart_delay_minutes: 0,
            attempts: 0,
        };
        let jobs = vec![
            job(1, "B", Some(g)),
            job(2, "Z", Some(g)),
            job(3, "C", Some(g)),
            job(4, "A", Some(g)),
            job(5, "Чужое", None),
        ];
        let names: Vec<_> = members(&group, &jobs).into_iter().map(|j| j.name.as_str()).collect();
        assert_eq!(names, vec!["C", "B", "A", "Z"]);
    }

    #[test]
    fn old_job_has_no_group_and_minimal_group_json_reads() {
        let j = job(1, "t", None);
        assert!(j.group.is_none());
        assert!(!serde_json::to_string(&j).unwrap().contains("\"group\""));
        let g: Group = serde_json::from_str(r#"{"id":"00000000-0000-0000-0000-000000000064","name":"g"}"#).unwrap();
        assert!(g.enabled && g.run_missed && g.order.is_empty() && g.schedule == Schedule::Manual);
        assert_eq!(RetryOverride::from_group(&g), RetryOverride::default());
    }

    /// Ненулевые значения группы заменяют значения задания, нулевые — нет.
    #[test]
    fn group_override_replaces_only_set_values() {
        let mut g: Group = serde_json::from_str(r#"{"id":"00000000-0000-0000-0000-000000000064","name":"g"}"#).unwrap();
        g.restarts = 2;
        g.attempts = 4;
        let mut j = job(1, "t", None);
        j.restarts = 7;
        j.restart_delay_minutes = 9;
        j.attempts = 1;
        RetryOverride::from_group(&g).apply(&mut j);
        assert_eq!((j.restarts, j.restart_delay_minutes, j.attempts), (2, 9, 4));
    }
}
