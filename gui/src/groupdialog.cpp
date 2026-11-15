#include "groupdialog.h"

#include "theme.h"
#include "util.h"

#include <QCheckBox>
#include <QComboBox>
#include <QDateTimeEdit>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMessageBox>
#include <QPushButton>
#include <QSpinBox>
#include <QStackedWidget>
#include <QTimeEdit>
#include <QUuid>
#include <QVBoxLayout>

static const char *kDays[] = {"Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"};

// ===================== ScheduleEditor =====================

ScheduleEditor::ScheduleEditor(QWidget *parent) : QWidget(parent) {
    m_type = new QComboBox;
    m_type->addItem(tr("Только вручную"), "manual");
    m_type->addItem(tr("Ежедневно"), "daily");
    m_type->addItem(tr("По дням недели"), "weekly");
    m_type->addItem(tr("Ежемесячно"), "monthly");
    m_type->addItem(tr("С интервалом"), "interval");
    m_type->addItem(tr("Однократно"), "once");
    m_times = new QLineEdit(QStringLiteral("23:00"));
    m_times->setPlaceholderText(QStringLiteral("23:00; 13:00"));
    m_pages = new QStackedWidget;
    m_pages->addWidget(new QWidget); // manual
    m_pages->addWidget(new QWidget); // daily
    {
        auto *w = new QWidget;
        auto *h = new QHBoxLayout(w);
        h->setContentsMargins(0, 0, 0, 0);
        for (const auto &n : {tr("Пн"), tr("Вт"), tr("Ср"), tr("Чт"), tr("Пт"), tr("Сб"), tr("Вс")}) {
            auto *cb = new QCheckBox(n);
            m_weekDays << cb;
            h->addWidget(cb);
        }
        h->addStretch();
        m_pages->addWidget(w);
    }
    {
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_monthDays = new QLineEdit(QStringLiteral("1"));
        m_monthDays->setPlaceholderText(tr("напр. 1; 15; 31 (31 = последний день месяца)"));
        f->addRow(tr("Числа месяца:"), m_monthDays);
        m_pages->addWidget(w);
    }
    {
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_intervalMin = new QSpinBox;
        m_intervalMin->setRange(1, 10080);
        m_intervalMin->setValue(60);
        m_intervalMin->setSuffix(tr(" мин"));
        m_intervalWindow = new QCheckBox(tr("Только в интервале времени"));
        m_intervalFrom = new QTimeEdit(QTime(8, 0));
        m_intervalTo = new QTimeEdit(QTime(20, 0));
        auto *hw = new QWidget;
        auto *hl = new QHBoxLayout(hw);
        hl->setContentsMargins(0, 0, 0, 0);
        hl->addWidget(m_intervalWindow);
        hl->addWidget(new QLabel(tr("с")));
        hl->addWidget(m_intervalFrom);
        hl->addWidget(new QLabel(tr("до")));
        hl->addWidget(m_intervalTo);
        hl->addStretch();
        f->addRow(tr("Каждые:"), m_intervalMin);
        f->addRow(hw);
        m_pages->addWidget(w);
    }
    {
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_onceAt = new QDateTimeEdit(QDateTime::currentDateTime().addSecs(3600));
        m_onceAt->setCalendarPopup(true);
        m_onceAt->setDisplayFormat(QStringLiteral("dd.MM.yyyy HH:mm"));
        f->addRow(tr("Дата и время:"), m_onceAt);
        m_pages->addWidget(w);
    }
    auto *timesLabel = new QLabel(tr("Время запуска:"));
    auto update = [this, timesLabel](int i) {
        m_pages->setCurrentIndex(i);
        const QString t = m_type->itemData(i).toString();
        const bool needTimes = t == "daily" || t == "weekly" || t == "monthly";
        m_times->setVisible(needTimes);
        timesLabel->setVisible(needTimes);
    };
    connect(m_type, &QComboBox::currentIndexChanged, this, update);
    auto *f = new QFormLayout;
    f->setContentsMargins(0, 0, 0, 0);
    f->addRow(tr("Запуск:"), m_type);
    f->addRow(timesLabel, m_times);
    auto *v = new QVBoxLayout(this);
    v->setContentsMargins(0, 0, 0, 0);
    v->addLayout(f);
    v->addWidget(m_pages);
    update(0);
}

void ScheduleEditor::load(const QJsonObject &sc) {
    const QString t = sc.value("type").toString("manual");
    m_type->setCurrentIndex(qMax(0, m_type->findData(t)));
    if (sc.contains("times")) {
        QStringList ts;
        for (const auto &x : sc.value("times").toArray()) ts << x.toString().left(5);
        m_times->setText(ts.join("; "));
    }
    if (t == "weekly")
        for (const auto &d : sc.value("days").toArray())
            for (int i = 0; i < 7; ++i)
                if (d.toString() == QLatin1String(kDays[i])) m_weekDays[i]->setChecked(true);
    if (t == "monthly") {
        QStringList l;
        for (const auto &x : sc.value("days").toArray()) l << QString::number(x.toInt());
        m_monthDays->setText(l.join("; "));
    }
    if (t == "interval") {
        m_intervalMin->setValue(sc.value("minutes").toInt(60));
        const bool win = sc.value("from").isString() && sc.value("to").isString();
        m_intervalWindow->setChecked(win);
        if (win) {
            m_intervalFrom->setTime(QTime::fromString(sc.value("from").toString().left(5), "HH:mm"));
            m_intervalTo->setTime(QTime::fromString(sc.value("to").toString().left(5), "HH:mm"));
        }
    }
    if (t == "once") m_onceAt->setDateTime(QDateTime::fromString(sc.value("at").toString().left(19), Qt::ISODate));
}

QJsonObject ScheduleEditor::value() const {
    const QString t = m_type->currentData().toString();
    QJsonObject sc{{"type", t}};
    if (t == "daily" || t == "weekly" || t == "monthly") {
        QJsonArray a;
        for (const auto &s : util::splitList(m_times->text())) {
            const QTime tm = QTime::fromString(s, s.count(':') == 2 ? "H:mm:ss" : "H:mm");
            if (tm.isValid()) a.append(tm.toString("HH:mm:ss"));
        }
        sc["times"] = a;
    }
    if (t == "weekly") {
        QJsonArray d;
        for (int i = 0; i < 7; ++i)
            if (m_weekDays[i]->isChecked()) d.append(QLatin1String(kDays[i]));
        sc["days"] = d;
    }
    if (t == "monthly") {
        QJsonArray d;
        for (const auto &x : util::splitList(m_monthDays->text())) {
            const int n = x.toInt();
            if (n >= 1 && n <= 31) d.append(n);
        }
        sc["days"] = d;
    }
    if (t == "interval") {
        sc["minutes"] = m_intervalMin->value();
        if (m_intervalWindow->isChecked()) {
            sc["from"] = m_intervalFrom->time().toString("HH:mm:ss");
            sc["to"] = m_intervalTo->time().toString("HH:mm:ss");
        }
    }
    if (t == "once") sc["at"] = m_onceAt->dateTime().toString("yyyy-MM-ddTHH:mm:ss");
    return sc;
}

QString ScheduleEditor::validate() const {
    const QJsonObject sc = value();
    const QString t = sc.value("type").toString();
    if ((t == "daily" || t == "weekly" || t == "monthly") && sc.value("times").toArray().isEmpty())
        return tr("Укажите время запуска в формате ЧЧ:ММ.");
    if (t == "weekly" && sc.value("days").toArray().isEmpty()) return tr("Отметьте дни недели.");
    if (t == "monthly" && sc.value("days").toArray().isEmpty()) return tr("Укажите числа месяца.");
    return {};
}

// ===================== RetryEditor =====================

RetryEditor::RetryEditor(bool forGroup, QWidget *parent) : QWidget(parent), m_forGroup(forGroup) {
    const QString same = tr("как в задании");
    m_restarts = new QSpinBox;
    m_restarts->setRange(0, 20);
    m_restarts->setSpecialValueText(forGroup ? same : tr("не перезапускать"));
    m_restarts->setToolTip(tr("Сколько раз запустить задание заново, если оно завершилось ошибкой. "
                              "Отменённое вручную задание не перезапускается."));

    m_delay = new QSpinBox;
    m_delay->setRange(0, 24 * 60);
    m_delay->setSuffix(tr(" мин"));
    m_delay->setSpecialValueText(forGroup ? same : tr("сразу"));
    m_delay->setToolTip(tr("Пауза перед перезапуском. Другие задания в это время выполняются."));

    m_attempts = new QSpinBox;
    m_attempts->setRange(0, 20);
    m_attempts->setSpecialValueText(forGroup ? same : tr("по умолчанию"));
    m_attempts->setToolTip(tr("Попыток каждого действия внутри одного запуска: подготовка источника "
                              "(выгрузка 1С, SQL, PostgreSQL, ВМ), упаковка 7-Zip, отправка в каждое хранилище. "
                              "По умолчанию: отправка — 5 попыток, остальное — 1."));

    auto *f = new QFormLayout(this);
    f->setContentsMargins(0, 0, 0, 0);
    f->addRow(tr("Перезапусков задания при ошибке:"), m_restarts);
    f->addRow(tr("Пауза перед перезапуском:"), m_delay);
    f->addRow(tr("Попыток действия (архив, выгрузка, отправка):"), m_attempts);
    if (!forGroup) m_delay->setValue(5);
}

void RetryEditor::load(const QJsonObject &obj) {
    m_restarts->setValue(obj.value("restarts").toInt(0));
    m_delay->setValue(obj.value("restart_delay_minutes").toInt(m_forGroup ? 0 : 5));
    m_attempts->setValue(obj.value("attempts").toInt(0));
}

void RetryEditor::store(QJsonObject &obj) const {
    obj["restarts"] = m_restarts->value();
    obj["restart_delay_minutes"] = m_delay->value();
    obj["attempts"] = m_attempts->value();
}

// ===================== GroupDialog =====================

GroupDialog::GroupDialog(const QJsonObject &group, const QJsonArray &jobs, QWidget *parent)
    : QDialog(parent), m_original(group) {
    setWindowTitle(group.isEmpty() ? tr("Новая группа") : tr("Группа: %1").arg(group.value("name").toString()));
    resize(560, 560);

    m_name = new QLineEdit(group.value("name").toString());
    m_enabled = new QCheckBox(tr("Включена"));
    m_enabled->setChecked(group.value("enabled").toBool(true));
    auto *head = new QHBoxLayout;
    head->addWidget(new QLabel(tr("Имя:")));
    head->addWidget(m_name, 1);
    head->addWidget(m_enabled);

    // Сначала задания группы в её порядке, затем остальные по имени.
    const QString gid = group.value("id").toString();
    QStringList inGroup;
    for (const auto &m : group.value("jobs").toArray()) inGroup << m.toObject().value("id").toString();
    QList<QJsonObject> rest;
    for (const auto &v : jobs) {
        const QJsonObject j = v.toObject();
        if (!inGroup.contains(j.value("id").toString())) rest << j;
    }
    m_jobs = new QListWidget;
    m_jobs->setSelectionMode(QAbstractItemView::SingleSelection);
    auto add = [this, gid](const QJsonObject &j, bool checked) {
        QString text = j.value("name").toString();
        const QString other = j.value("group").toString();
        if (!checked && !other.isEmpty() && other != gid) text += tr("  (сейчас в другой группе)");
        if (!j.value("enabled").toBool(true)) text += tr("  (отключено — по расписанию группы не запускается)");
        auto *it = new QListWidgetItem(text, m_jobs);
        it->setData(Qt::UserRole, j.value("id").toString());
        it->setFlags(it->flags() | Qt::ItemIsUserCheckable);
        it->setCheckState(checked ? Qt::Checked : Qt::Unchecked);
    };
    for (const auto &m : group.value("jobs").toArray()) {
        // В list_groups у членов группы только id, name, enabled; группа — эта.
        QJsonObject j = m.toObject();
        j["group"] = gid;
        add(j, true);
    }
    for (const auto &j : std::as_const(rest)) add(j, false);

    auto *up = new QPushButton(tr("Выше"));
    auto *down = new QPushButton(tr("Ниже"));
    connect(up, &QPushButton::clicked, this, [this] { move(-1); });
    connect(down, &QPushButton::clicked, this, [this] { move(1); });
    auto *bl = new QVBoxLayout;
    bl->addWidget(up);
    bl->addWidget(down);
    bl->addStretch();
    auto *jobsBox = new QGroupBox(tr("Задания группы и порядок запуска"));
    auto *jl = new QVBoxLayout(jobsBox);
    auto *jh = new QHBoxLayout;
    jh->addWidget(m_jobs, 1);
    jh->addLayout(bl);
    jl->addLayout(jh);
    auto *jobsHint = new QLabel(tr("Отмеченные задания входят в группу и запускаются сверху вниз по одному. Ошибка "
                                   "одного задания не отменяет остальные. Задание может быть только в одной группе: "
                                   "отметка переносит его сюда из другой."));
    jobsHint->setWordWrap(true);
    theme::markHint(jobsHint);
    jl->addWidget(jobsHint);

    m_schedule = new ScheduleEditor;
    m_schedule->load(group.value("schedule").toObject());
    m_runMissed = new QCheckBox(tr("Выполнять пропущенные запуски (если компьютер был выключен)"));
    m_runMissed->setChecked(group.value("run_missed").toBool(true));
    auto *schedBox = new QGroupBox(tr("Расписание группы"));
    auto *sl = new QVBoxLayout(schedBox);
    sl->addWidget(m_schedule);
    sl->addWidget(m_runMissed);
    m_retry = new RetryEditor(true);
    m_retry->load(group);
    sl->addWidget(m_retry);
    auto *schedHint = new QLabel(tr("Собственное расписание каждого задания продолжает действовать. Чтобы задание "
                                    "запускалось только с группой, поставьте у него «Только вручную». Повторы, "
                                    "заданные здесь, при запуске группы заменяют настройки заданий; "
                                    "«как в задании» — берутся из задания."));
    schedHint->setWordWrap(true);
    theme::markHint(schedHint);
    sl->addWidget(schedHint);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *v = new QVBoxLayout(this);
    v->addLayout(head);
    v->addWidget(jobsBox, 1);
    v->addWidget(schedBox);
    v->addWidget(buttons);
    util::keepButtonsOnScreen(this);
}

void GroupDialog::move(int delta) {
    const int r = m_jobs->currentRow();
    const int to = r + delta;
    if (r < 0 || to < 0 || to >= m_jobs->count()) return;
    QListWidgetItem *it = m_jobs->takeItem(r);
    m_jobs->insertItem(to, it);
    m_jobs->setCurrentRow(to);
}

QJsonObject GroupDialog::request() const {
    QJsonObject g{{"id", m_original.value("id").toString(QUuid().toString(QUuid::WithoutBraces))},
                  {"name", m_name->text().trimmed()},
                  {"enabled", m_enabled->isChecked()},
                  {"run_missed", m_runMissed->isChecked()},
                  {"schedule", m_schedule->value()}};
    m_retry->store(g);
    QJsonArray ids;
    for (int i = 0; i < m_jobs->count(); ++i)
        if (m_jobs->item(i)->checkState() == Qt::Checked) ids.append(m_jobs->item(i)->data(Qt::UserRole).toString());
    return QJsonObject{{"group", g}, {"jobs", ids}};
}

void GroupDialog::accept() {
    if (m_name->text().trimmed().isEmpty()) {
        m_name->setFocus();
        return (void)QMessageBox::warning(this, windowTitle(), tr("Укажите имя группы."));
    }
    const QString err = m_schedule->validate();
    if (!err.isEmpty()) return (void)QMessageBox::warning(this, windowTitle(), err);
    QDialog::accept();
}
