#pragma once

#include <QDialog>
#include <QJsonArray>
#include <QJsonObject>

class QCheckBox;
class QComboBox;
class QDateTimeEdit;
class QLineEdit;
class QListWidget;
class QSpinBox;
class QStackedWidget;
class QTimeEdit;
class ScheduleEditor;
class ServiceClient;

// Расписание: тип, время, дни, интервал. Тот же JSON, что у задания ("schedule").
class ScheduleEditor : public QWidget {
    Q_OBJECT
public:
    explicit ScheduleEditor(QWidget *parent = nullptr);
    void load(const QJsonObject &schedule);
    QJsonObject value() const;
    // Пусто — расписание заполнено верно.
    QString validate() const;

private:
    QComboBox *m_type;
    QStackedWidget *m_pages;
    QLineEdit *m_times;
    QList<QCheckBox *> m_weekDays;
    QLineEdit *m_monthDays;
    QSpinBox *m_intervalMin;
    QCheckBox *m_intervalWindow;
    QTimeEdit *m_intervalFrom, *m_intervalTo;
    QDateTimeEdit *m_onceAt;
};

// Повторы при ошибке: перезапуски задания, пауза между ними и попытки действий.
// Поля JSON: restarts, restart_delay_minutes, attempts. В группе 0 значит «как в задании».
class RetryEditor : public QWidget {
    Q_OBJECT
public:
    explicit RetryEditor(bool forGroup, QWidget *parent = nullptr);
    void load(const QJsonObject &obj);
    void store(QJsonObject &obj) const;

private:
    bool m_forGroup;
    QSpinBox *m_restarts;
    QSpinBox *m_delay;
    QSpinBox *m_attempts;
};

// Группа заданий: имя, состав и порядок, общее расписание.
class GroupDialog : public QDialog {
    Q_OBJECT
public:
    // group — объект из list_groups (пустой — новая группа); jobs — список list_jobs.
    GroupDialog(const QJsonObject &group, const QJsonArray &jobs, QWidget *parent = nullptr);
    // {"group": {...}, "jobs": [id, ...]} — параметры команды save_group.
    QJsonObject request() const;

protected:
    void accept() override;

private:
    void move(int delta);

    QJsonObject m_original;
    QLineEdit *m_name;
    QCheckBox *m_enabled;
    QCheckBox *m_runMissed;
    QListWidget *m_jobs; // отмеченные — в группе; порядок строк — порядок запуска
    ScheduleEditor *m_schedule;
    RetryEditor *m_retry;
};
