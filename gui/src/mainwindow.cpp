#include "mainwindow.h"

#include "destinationdialog.h"
#include "groupdialog.h"
#include "jobdialog.h"
#include "notifydialog.h"
#include "resourcesdialog.h"
#include "serviceclient.h"
#include "theme.h"
#include "updatedialog.h"
#include "util.h"

#include <QAction>
#include <QApplication>
#include <QCheckBox>
#include <QCloseEvent>
#include <QComboBox>
#include <QDialog>
#include <QDialogButtonBox>
#include <QDir>
#include <QFile>
#include <QFileDialog>
#include <QFormLayout>
#include <QFrame>
#include <QGuiApplication>
#include <QHBoxLayout>
#include <QHash>
#include <QHeaderView>
#include <QSet>
#include <QJsonDocument>
#include <QKeySequence>
#include <QLabel>
#include <QLineEdit>
#include <QMenu>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QSaveFile>
#include <QSettings>
#include <QSplitter>
#include <QStackedWidget>
#include <QStatusBar>
#include <QStyle>
#include <QStyleHints>
#include <QTableWidget>
#include <QTimer>
#include <QToolButton>
#include <QVBoxLayout>
#include <windows.h>
#include <shellapi.h>

enum JobCol { ColState, ColName, ColSource, ColSchedule, ColNext, ColLast, ColMessage, ColCount };
enum HistCol { HistResult, HistStart, HistDuration, HistJob, HistDetails, HistCount };
// Строка группы хранит её id в ColName под этой ролью; у строки задания — id задания в Qt::UserRole.
enum { GroupIdRole = Qt::UserRole + 1 };
// Строка-заголовок раздела хранит в ColState ключ раздела: id группы или "__ungrouped".
enum { SectionRole = Qt::UserRole + 2 };

static QTableWidget *makeTable(const QStringList &headers) {
    auto *t = new QTableWidget(0, headers.size());
    t->setHorizontalHeaderLabels(headers);
    t->setSelectionBehavior(QAbstractItemView::SelectRows);
    t->setSelectionMode(QAbstractItemView::SingleSelection);
    t->setEditTriggers(QAbstractItemView::NoEditTriggers);
    t->setShowGrid(false);
    t->setWordWrap(false);
    t->verticalHeader()->hide();
    t->verticalHeader()->setDefaultSectionSize(36);
    t->horizontalHeader()->setHighlightSections(false);
    t->horizontalHeader()->setDefaultAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    t->horizontalHeader()->setSectionResizeMode(QHeaderView::ResizeToContents);
    t->horizontalHeader()->setStretchLastSection(true);
    // Первая колонка обеих таблиц — состояние в виде цветной плашки.
    t->setItemDelegateForColumn(0, new theme::PillDelegate(t));
    return t;
}

static QTableWidgetItem *cell(QTableWidget *t, int row, int col) {
    auto *it = t->item(row, col);
    if (!it) {
        it = new QTableWidgetItem;
        t->setItem(row, col, it);
    }
    return it;
}

// Длинное имя задания не должно раздвигать карточку сводки.
static QString shortened(const QString &s, int max) {
    return s.size() > max ? s.left(max - 1) + QStringLiteral("…") : s;
}

MainWindow::MainWindow(QWidget *parent) : QMainWindow(parent), m_client(new ServiceClient(this)) {
    setWindowTitle(tr("SteadyBackup"));
    setWindowIcon(util::appIcon());
    resize(1120, 720);
    util::fitToScreen(this);

    // --- действия: кнопки над списком, контекстное меню и клавиши ---
    auto action = [this](const QString &text, const QKeySequence &key, void (MainWindow::*slot)()) {
        auto *a = new QAction(text, this);
        if (!key.isEmpty()) {
            a->setShortcut(key);
            a->setToolTip(QStringLiteral("%1 (%2)").arg(text, key.toString(QKeySequence::NativeText)));
        }
        connect(a, &QAction::triggered, this, slot);
        addAction(a);
        return a;
    };
    m_actRun = action(tr("Запустить"), QKeySequence(Qt::CTRL | Qt::Key_R), &MainWindow::runJob);
    m_actCancel = action(tr("Отменить"), QKeySequence(), &MainWindow::cancelJob);
    m_actEdit = action(tr("Изменить"), QKeySequence(Qt::Key_F2), &MainWindow::editJob);
    m_actCopy = action(tr("Копировать"), QKeySequence(Qt::CTRL | Qt::Key_D), &MainWindow::copyJob);
    m_actDelete = action(tr("Удалить"), QKeySequence(Qt::Key_Delete), &MainWindow::deleteJob);
    action(tr("Новое задание"), QKeySequence(Qt::CTRL | Qt::Key_N), &MainWindow::newJob);
    m_actNewGroup = action(tr("Новая группа"), QKeySequence(Qt::CTRL | Qt::Key_G), &MainWindow::newGroup);
    action(tr("Обновить"), QKeySequence(Qt::Key_F5), &MainWindow::refresh);

    // --- полоса сообщения: служба недоступна или режим просмотра ---
    m_banner = new QFrame;
    m_banner->setObjectName(QStringLiteral("banner"));
    auto *bl = new QHBoxLayout(m_banner);
    bl->setContentsMargins(12, 8, 8, 8);
    bl->setSpacing(10);
    m_bannerIcon = new QLabel;
    m_bannerText = new QLabel;
    m_bannerText->setWordWrap(true);
    m_bannerButton = new QPushButton(tr("Перезапустить от имени администратора"));
    connect(m_bannerButton, &QPushButton::clicked, this, &MainWindow::restartElevated);
    bl->addWidget(m_bannerIcon);
    bl->addWidget(m_bannerText, 1);
    bl->addWidget(m_bannerButton);
    m_banner->hide();

    auto *split = new QSplitter(Qt::Vertical);
    split->addWidget(buildJobs());
    split->addWidget(buildHistory());
    split->setStretchFactor(0, 3);
    split->setStretchFactor(1, 2);
    split->setChildrenCollapsible(false);

    auto *body = new QWidget;
    auto *bv = new QVBoxLayout(body);
    bv->setContentsMargins(16, 14, 16, 10);
    bv->setSpacing(12);
    bv->addWidget(m_banner);
    bv->addWidget(buildCards());
    bv->addWidget(split, 1);

    auto *central = new QWidget;
    auto *outer = new QVBoxLayout(central);
    outer->setContentsMargins(0, 0, 0, 0);
    outer->setSpacing(0);
    outer->addWidget(buildHeader());
    outer->addWidget(body, 1);
    setCentralWidget(central);
    statusBar();

    buildTray();
    applyIcons();
    updateActions();
    updateCards();

    connect(QGuiApplication::styleHints(), &QStyleHints::colorSchemeChanged, this, [this] {
        theme::apply();
        applyIcons();
        // Полоса сообщения красится по свойству: сбрасываем его, чтобы стиль применился заново.
        m_banner->setProperty("kind", QVariant());
        updateBanner();
        updateCards();
        applyJobs(m_jobsData);
        renderHistory();
    });

    connect(m_client, &ServiceClient::connectionChanged, this, [this](bool) {
        m_tick = 0;
        refresh();
    });

    m_timer = new QTimer(this);
    connect(m_timer, &QTimer::timeout, this, &MainWindow::refresh);
    m_timer->start(2000);
    refresh();
}

QWidget *MainWindow::buildHeader() {
    auto *header = new QFrame;
    header->setObjectName(QStringLiteral("header"));
    auto *h = new QHBoxLayout(header);
    h->setContentsMargins(16, 10, 16, 10);
    h->setSpacing(10);

    m_logo = new QLabel;
    m_logo->setPixmap(util::appIcon().pixmap(28, 28));
    auto *title = new QLabel(tr("SteadyBackup"));
    title->setObjectName(QStringLiteral("appTitle"));
    m_statePill = new QLabel(tr("подключение к службе…"));
    theme::markHint(m_statePill);

    m_refreshButton = new QToolButton;
    m_refreshButton->setToolTip(tr("Обновить (F5)"));
    connect(m_refreshButton, &QToolButton::clicked, this, &MainWindow::refresh);

    m_settingsButton = new QToolButton;
    m_settingsButton->setText(tr("Настройки"));
    m_settingsButton->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
    m_settingsButton->setPopupMode(QToolButton::InstantPopup);
    auto *menu = new QMenu(m_settingsButton);
    menu->addAction(tr("Хранилища и подключения…"), this, [this] {
        if (!requireAdmin()) return;
        ResourcesDialog dlg(m_client, this);
        dlg.exec();
    });
    menu->addAction(tr("Уведомления…"), this, [this] {
        if (!requireAdmin()) return;
        NotifyDialog dlg(m_client, this);
        dlg.exec();
    });
    menu->addAction(tr("Рабочий каталог…"), this, [this] { editWorkDir(); });
    menu->addSeparator();
    menu->addAction(tr("Экспорт настроек…"), this, [this] { exportSettings(); });
    menu->addAction(tr("Импорт настроек…"), this, [this] { importSettings(); });
    menu->addSeparator();
    menu->addAction(tr("Обновление программы…"), this, [this] { showUpdates(); });
    m_settingsButton->setMenu(menu);

    m_updateButton = new QPushButton;
    m_updateButton->hide();
    connect(m_updateButton, &QPushButton::clicked, this, [this] { showUpdates(); });

    m_newButton = new QPushButton(tr("Новое задание"));
    m_newButton->setObjectName(QStringLiteral("primary"));
    m_newButton->setToolTip(tr("Новое задание (Ctrl+N)"));
    connect(m_newButton, &QPushButton::clicked, this, &MainWindow::newJob);

    h->addWidget(m_logo);
    h->addWidget(title);
    h->addSpacing(12);
    h->addWidget(m_statePill);
    h->addStretch(1);
    h->addWidget(m_updateButton);
    h->addWidget(m_refreshButton);
    h->addWidget(m_settingsButton);
    h->addSpacing(4);
    h->addWidget(m_newButton);
    return header;
}

QWidget *MainWindow::buildCards() {
    auto *row = new QWidget;
    auto *h = new QHBoxLayout(row);
    h->setContentsMargins(0, 0, 0, 0);
    h->setSpacing(12);
    const QStringList titles{tr("Задания"), tr("Состояние"), tr("Следующий запуск"), tr("Сейчас")};
    for (int i = 0; i < CardCount; ++i) {
        auto *card = new QFrame;
        card->setObjectName(QStringLiteral("card"));
        auto *g = new QHBoxLayout(card);
        g->setContentsMargins(14, 12, 14, 12);
        g->setSpacing(12);
        Card &c = m_cards[i];
        c.icon = new QLabel;
        c.icon->setFixedSize(26, 26);
        auto *title = new QLabel(titles.at(i));
        title->setObjectName(QStringLiteral("cardTitle"));
        c.value = new QLabel(QStringLiteral("—"));
        c.value->setObjectName(QStringLiteral("cardValue"));
        c.sub = new QLabel;
        c.sub->setObjectName(QStringLiteral("cardSub"));
        // Текст карточки не задаёт её ширину: все четыре делят строку поровну.
        for (QLabel *l : {title, c.value, c.sub})
            l->setSizePolicy(QSizePolicy::Ignored, QSizePolicy::Preferred);
        auto *v = new QVBoxLayout;
        v->setSpacing(2);
        v->addWidget(title);
        v->addWidget(c.value);
        v->addWidget(c.sub);
        g->addWidget(c.icon, 0, Qt::AlignTop);
        g->addLayout(v, 1);
        h->addWidget(card, 1);
    }
    return row;
}

QWidget *MainWindow::buildJobs() {
    auto *box = new QWidget;
    auto *v = new QVBoxLayout(box);
    v->setContentsMargins(0, 0, 0, 0);
    v->setSpacing(8);

    auto *bar = new QHBoxLayout;
    bar->setSpacing(4);
    auto *title = new QLabel(tr("Задания"));
    title->setObjectName(QStringLiteral("sectionTitle"));
    bar->addWidget(title);
    bar->addStretch(1);
    for (QAction *a : {m_actRun, m_actCancel, m_actEdit, m_actCopy, m_actDelete, m_actNewGroup}) {
        auto *b = new QToolButton;
        b->setDefaultAction(a);
        b->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
        bar->addWidget(b);
    }
    v->addLayout(bar);

    m_jobs = makeTable({tr("Состояние"), tr("Задание"), tr("Источник"), tr("Расписание"), tr("Следующий запуск"),
                        tr("Последний запуск"), tr("Сообщение")});
    m_jobs->setContextMenuPolicy(Qt::CustomContextMenu);
    connect(m_jobs, &QTableWidget::customContextMenuRequested, this, &MainWindow::jobMenu);
    connect(m_jobs, &QTableWidget::cellDoubleClicked, this, [this] { editJob(); });
    // Заголовки групп сворачиваются щелчком и клавишами; см. eventFilter.
    m_jobs->viewport()->installEventFilter(this);
    m_jobs->installEventFilter(this);
    m_collapsed = QSettings().value(QStringLiteral("jobs/collapsed")).toStringList();
    connect(m_jobs, &QTableWidget::itemSelectionChanged, this, [this] {
        updateActions();
        if (m_historyOnlyJob && m_historyOnlyJob->isChecked()) renderHistory();
    });

    // Пустой список: вместо голой таблицы — что это и с чего начать.
    auto *empty = new QFrame;
    empty->setObjectName(QStringLiteral("card"));
    auto *ev = new QVBoxLayout(empty);
    ev->setSpacing(8);
    m_emptyIcon = new QLabel;
    m_emptyIcon->setAlignment(Qt::AlignCenter);
    auto *emptyTitle = new QLabel(tr("Заданий пока нет"));
    emptyTitle->setObjectName(QStringLiteral("emptyTitle"));
    emptyTitle->setAlignment(Qt::AlignCenter);
    auto *emptyHint = new QLabel(tr("Задание описывает, что копировать, куда и по какому расписанию."));
    emptyHint->setAlignment(Qt::AlignCenter);
    theme::markHint(emptyHint);
    m_emptyButton = new QPushButton(tr("Создать первое задание"));
    m_emptyButton->setObjectName(QStringLiteral("primary"));
    connect(m_emptyButton, &QPushButton::clicked, this, &MainWindow::newJob);
    ev->addStretch(1);
    ev->addWidget(m_emptyIcon);
    ev->addWidget(emptyTitle);
    ev->addWidget(emptyHint);
    ev->addSpacing(6);
    ev->addWidget(m_emptyButton, 0, Qt::AlignHCenter);
    ev->addStretch(1);

    m_jobsStack = new QStackedWidget;
    m_jobsStack->addWidget(m_jobs);
    m_jobsStack->addWidget(empty);
    v->addWidget(m_jobsStack, 1);
    return box;
}

QWidget *MainWindow::buildHistory() {
    auto *box = new QWidget;
    auto *v = new QVBoxLayout(box);
    v->setContentsMargins(0, 0, 0, 0);
    v->setSpacing(8);

    auto *bar = new QHBoxLayout;
    bar->setSpacing(12);
    auto *title = new QLabel(tr("Журнал"));
    title->setObjectName(QStringLiteral("sectionTitle"));
    m_historyOnlyJob = new QCheckBox(tr("Только выбранное задание"));
    m_historyFilter = new QComboBox;
    m_historyFilter->addItem(tr("Все записи"));
    m_historyFilter->addItem(tr("Только ошибки и предупреждения"));
    connect(m_historyFilter, &QComboBox::currentIndexChanged, this, [this] { renderHistory(); });
    connect(m_historyOnlyJob, &QCheckBox::toggled, this, [this] { renderHistory(); });
    bar->addWidget(title);
    bar->addStretch(1);
    bar->addWidget(m_historyOnlyJob);
    bar->addWidget(m_historyFilter);
    v->addLayout(bar);

    m_history = makeTable({tr("Итог"), tr("Начало"), tr("Длительность"), tr("Задание"), tr("Подробности")});
    m_history->setToolTip(tr("Двойной щелчок — подробности запуска"));
    connect(m_history, &QTableWidget::cellDoubleClicked, this, [this](int row, int) { showHistoryDetails(row); });
    v->addWidget(m_history, 1);
    return box;
}

void MainWindow::buildTray() {
    m_tray = new QSystemTrayIcon(util::appIcon(), this);
    auto *menu = new QMenu(this);
    menu->addAction(tr("Открыть SteadyBackup"), this, [this] {
        showNormal();
        raise();
        activateWindow();
    });
    menu->addSeparator();
    menu->addAction(tr("Выход"), this, [this] {
        m_quitting = true;
        qApp->quit();
    });
    m_tray->setContextMenu(menu);
    m_tray->setToolTip(tr("SteadyBackup"));
    connect(m_tray, &QSystemTrayIcon::activated, this, [this](QSystemTrayIcon::ActivationReason r) {
        if (r == QSystemTrayIcon::Trigger || r == QSystemTrayIcon::DoubleClick) {
            if (isVisible() && !isMinimized()) {
                hide();
            } else {
                showNormal();
                raise();
                activateWindow();
            }
        }
    });
    m_tray->show();
}

void MainWindow::applyIcons() {
    m_actRun->setIcon(theme::icon(QStringLiteral("play"), theme::Ok));
    m_actCancel->setIcon(theme::icon(QStringLiteral("stop"), theme::Error));
    m_actEdit->setIcon(theme::icon(QStringLiteral("edit")));
    m_actCopy->setIcon(theme::icon(QStringLiteral("copy")));
    m_actDelete->setIcon(theme::icon(QStringLiteral("trash")));
    m_actNewGroup->setIcon(theme::icon(QStringLiteral("list")));
    m_refreshButton->setIcon(theme::icon(QStringLiteral("refresh")));
    m_settingsButton->setIcon(theme::icon(QStringLiteral("settings")));
    const QIcon plus(theme::pixmap(QStringLiteral("plus"), QColor(Qt::white), 16));
    m_newButton->setIcon(plus);
    m_emptyButton->setIcon(plus);
    m_emptyIcon->setPixmap(theme::pixmap(QStringLiteral("list"), theme::mutedColor(), 40));
}

void MainWindow::closeEvent(QCloseEvent *e) {
    if (!m_quitting && m_tray->isVisible()) {
        hide();
        if (!m_trayHintShown) {
            m_tray->showMessage(tr("SteadyBackup"),
                                tr("Программа продолжает работать в области уведомлений. Резервное копирование "
                                   "выполняет служба, даже если это окно закрыто."));
            m_trayHintShown = true;
        }
        e->ignore();
        return;
    }
    e->accept();
}

void MainWindow::refresh() {
    m_client->call({{"cmd", "status"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return serviceDown(err);
        applyStatus(r.toObject());
    });
    m_client->call({{"cmd", "list_groups"}}, [this](bool ok, const QJsonValue &r, const QString &) {
        if (ok) m_groupsData = r.toArray();
    });
    // Ответы приходят по порядку: к ответу list_jobs группы уже обновлены.
    m_client->call({{"cmd", "list_jobs"}}, [this](bool ok, const QJsonValue &r, const QString &) {
        if (ok) applyJobs(r.toArray());
    });
    m_client->call({{"cmd", "history"}, {"limit", 300}}, [this](bool ok, const QJsonValue &r, const QString &) {
        if (ok) applyHistory(r.toArray());
    });
    // Список хранилищ нужен только диалогам и меняется редко: раз в полминуты, а не каждый опрос.
    if (m_tick++ % 15 == 0) {
        m_client->call({{"cmd", "list_storages"}}, [](bool ok, const QJsonValue &r, const QString &) {
            if (ok && r.isArray()) util::setStorageCache(r.toArray());
        });
    }
}

void MainWindow::serviceDown(const QString &error) {
    m_serviceOk = false;
    m_serviceError = error.isEmpty() ? tr("Служба SteadyBackup недоступна") : error;
    m_statusData = QJsonObject();
    m_runningId.clear();
    m_statePill->setText(tr("●  Служба недоступна"));
    theme::setState(m_statePill, theme::Error);
    updateBanner();
    updateCards();
    updateActions();
    updateTray();
}

void MainWindow::applyStatus(const QJsonObject &st) {
    m_serviceOk = true;
    m_serviceError.clear();
    m_statusData = st;
    m_admin = st.value("admin").toBool();
    m_statePill->setText(tr("●  Служба работает · версия %1").arg(st.value("version").toString()));
    theme::setState(m_statePill, theme::Ok);
    const QJsonValue running = st.value("running");
    m_runningId = running.isObject() ? running.toObject().value("job_id").toString() : QString();
    // Новая версия: кнопка в заголовке и одно сообщение в трее за сеанс.
    const QJsonObject upd = st.value("update").toObject();
    const QString avail = upd.value("available").toString();
    const QString phase = upd.value("phase").toString();
    if (phase == QLatin1String("downloading") || phase == QLatin1String("installing")) {
        m_updateButton->setText(tr("Устанавливается обновление…"));
        m_updateButton->show();
    } else if (!avail.isEmpty()) {
        m_updateButton->setText(tr("Доступна версия %1").arg(avail));
        m_updateButton->show();
        if (m_updateNotified != avail) {
            m_updateNotified = avail;
            m_tray->showMessage(tr("SteadyBackup"), tr("Доступна новая версия %1. Настройки → Обновление программы.").arg(avail),
                                QSystemTrayIcon::Information, 8000);
        }
    } else {
        m_updateButton->hide();
    }
    updateBanner();
    updateCards();
    updateActions();
    updateTray();
}

void MainWindow::updateTray() {
    if (!m_serviceOk) {
        m_tray->setIcon(util::appIcon(QColor(0x9e, 0x9e, 0x9e)));
        m_tray->setToolTip(tr("SteadyBackup — служба недоступна"));
        return;
    }
    const QJsonValue running = m_statusData.value("running");
    if (running.isObject()) {
        m_tray->setIcon(util::appIcon(QColor(0x15, 0x65, 0xc0)));
        m_tray->setToolTip(tr("SteadyBackup — выполняется «%1»").arg(running.toObject().value("job").toString()));
        return;
    }
    m_tray->setIcon(util::appIcon(m_anyFailed ? QColor(0xc6, 0x28, 0x28) : QColor(0x2e, 0x7d, 0x32)));
    m_tray->setToolTip(m_anyFailed ? tr("SteadyBackup — есть задания с ошибкой") : tr("SteadyBackup"));
}

void MainWindow::updateBanner() {
    QString kind;
    if (!m_serviceOk) {
        // До первого ответа службы сообщать не о чем.
        if (m_serviceError.isEmpty()) {
            m_banner->hide();
            return;
        }
        kind = QStringLiteral("error");
        m_bannerIcon->setPixmap(theme::pixmap(QStringLiteral("error"), theme::color(theme::Error), 20));
        m_bannerText->setText(tr("%1\nПроверьте, что служба SteadyBackup запущена: оснастка «Службы» Windows → SteadyBackup.")
                                  .arg(m_serviceError));
        m_bannerButton->hide();
    } else if (!m_admin) {
        kind = QStringLiteral("info");
        m_bannerIcon->setPixmap(theme::pixmap(QStringLiteral("shield"), theme::color(theme::Info), 20));
        m_bannerText->setText(tr("Режим просмотра. Чтобы менять задания и настройки, нужны права администратора."));
        m_bannerButton->show();
    } else {
        m_banner->hide();
        return;
    }
    if (m_banner->property("kind").toString() != kind) {
        m_banner->setProperty("kind", kind);
        m_banner->style()->unpolish(m_banner);
        m_banner->style()->polish(m_banner);
    }
    m_banner->show();
}

void MainWindow::updateActions() {
    const bool job = !selectedJobId().isEmpty();
    const bool sel = job || !selectedGroupId().isEmpty();
    m_actEdit->setEnabled(sel);
    m_actCopy->setEnabled(job && m_admin);
    m_actRun->setEnabled(sel && m_admin);
    m_actDelete->setEnabled(sel && m_admin);
    m_actNewGroup->setEnabled(m_admin && m_serviceOk);
    m_actCancel->setEnabled(m_admin && !m_runningId.isEmpty());
}

void MainWindow::setCard(CardId id, const QString &iconName, int state, const QString &value, const QString &sub) {
    const Card &c = m_cards[id];
    const auto st = static_cast<theme::State>(state);
    c.icon->setPixmap(theme::pixmap(iconName, theme::color(st == theme::None ? theme::Muted : st), 24));
    c.value->setText(shortened(value, 32));
    c.value->setToolTip(value);
    theme::setState(c.value, st);
    c.sub->setText(shortened(sub, 48));
    c.sub->setToolTip(sub);
}

void MainWindow::updateCards() {
    const QString listIcon = QStringLiteral("list");
    const QString okIcon = QStringLiteral("ok");
    const QString calendarIcon = QStringLiteral("calendar");
    const QString clockIcon = QStringLiteral("clock");
    if (!m_serviceOk) {
        const QString dash = QStringLiteral("—");
        setCard(CardJobs, listIcon, theme::Muted, dash, {});
        setCard(CardHealth, okIcon, theme::Muted, dash, {});
        setCard(CardNext, calendarIcon, theme::Muted, dash, {});
        setCard(CardNow, clockIcon, theme::Muted, dash, {});
        return;
    }
    const int total = m_jobsData.size();
    int enabled = 0, errors = 0, warns = 0, never = 0;
    QDateTime next;
    QString nextIso, nextJob;
    for (const auto &v : std::as_const(m_jobsData)) {
        const QJsonObject j = v.toObject();
        if (!j.value("enabled").toBool()) continue;
        ++enabled;
        const QString st = j.value("last_status").toString();
        if (st == QLatin1String("error")) ++errors;
        else if (st == QLatin1String("warning")) ++warns;
        else if (st.isEmpty()) ++never;
        const QString iso = j.value("next_run").toString();
        const QDateTime dt = util::parseIso(iso);
        if (dt.isValid() && (!next.isValid() || dt < next)) {
            next = dt;
            nextIso = iso;
            nextJob = j.value("name").toString();
        }
    }

    setCard(CardJobs, listIcon, theme::None, QString::number(total),
            total == 0 ? tr("заданий пока нет") : tr("активных: %1 · отключено: %2").arg(enabled).arg(total - enabled));

    if (enabled == 0)
        setCard(CardHealth, okIcon, theme::Muted, tr("Нет данных"), tr("нет активных заданий"));
    else if (errors > 0)
        setCard(CardHealth, QStringLiteral("error"), theme::Error, tr("Ошибок: %1").arg(errors),
                warns > 0 ? tr("предупреждений: %1 · по последним запускам").arg(warns) : tr("по последнему запуску каждого задания"));
    else if (warns > 0)
        setCard(CardHealth, QStringLiteral("warn"), theme::Warn, tr("Предупреждений: %1").arg(warns),
                tr("по последнему запуску каждого задания"));
    else if (never == enabled)
        setCard(CardHealth, okIcon, theme::Muted, tr("Запусков ещё не было"), tr("задания ждут расписания или ручного запуска"));
    else
        setCard(CardHealth, okIcon, theme::Ok, tr("Всё в порядке"),
                never > 0 ? tr("ещё не запускались: %1").arg(never) : tr("последние запуски без ошибок"));

    if (nextIso.isEmpty())
        setCard(CardNext, calendarIcon, theme::None, QStringLiteral("—"), tr("нет заданий по расписанию"));
    else
        setCard(CardNext, calendarIcon, theme::None, util::formatWhen(nextIso), nextJob);

    const QJsonValue running = m_statusData.value("running");
    const int queued = m_statusData.value("queue").toArray().size();
    if (running.isObject()) {
        const QJsonObject r = running.toObject();
        QString sub = r.value("phase").toString();
        const QDateTime started = util::parseIso(r.value("started").toString());
        if (started.isValid()) sub += tr(" · с %1").arg(started.toString(QStringLiteral("HH:mm")));
        if (queued > 0) sub += tr(" · в очереди: %1").arg(queued);
        setCard(CardNow, QStringLiteral("activity"), theme::Info, r.value("job").toString(), sub);
    } else {
        setCard(CardNow, clockIcon, theme::None, tr("Ожидание"), tr("сейчас ничего не выполняется"));
    }
}

void MainWindow::applyJobs(const QJsonArray &jobs) {
    const QString sel = selectedJobId();
    const QString selGroup = selectedGroupId();
    m_jobsData = jobs;
    m_jobsStack->setCurrentIndex(jobs.isEmpty() && m_groupsData.isEmpty() ? 1 : 0);
    m_anyFailed = false;

    // Порядок строк: группы по имени (заголовок, затем задания в порядке группы), потом задания без группы.
    // Заголовок — QJsonObject с полем "__group"; задание — объект из list_jobs.
    QHash<QString, QJsonObject> byId;
    for (const auto &v : jobs) byId.insert(v.toObject().value("id").toString(), v.toObject());
    QList<QJsonObject> rows;
    QSet<QString> placed;
    // Сводка по заданиям раздела: в свёрнутом заголовке видно, что внутри идёт работа или есть ошибки.
    struct Summary {
        int running = 0, queued = 0, errors = 0, warnings = 0;
    };
    QHash<QString, Summary> summary;
    auto count = [&summary](const QString &key, const QJsonObject &j) {
        Summary &s = summary[key];
        if (j.value("running").toBool()) ++s.running;
        else if (j.value("queued").toBool()) ++s.queued;
        if (!j.value("enabled").toBool()) return;
        const QString last = j.value("last_status").toString();
        if (last == QLatin1String("error")) ++s.errors;
        else if (last == QLatin1String("warning")) ++s.warnings;
    };
    const QString ungroupedKey = QStringLiteral("__ungrouped");
    for (const auto &gv : std::as_const(m_groupsData)) {
        const QJsonObject g = gv.toObject();
        const QString key = g.value("id").toString();
        rows << QJsonObject{{"__group", g}};
        for (const auto &m : g.value("jobs").toArray()) {
            const QString id = m.toObject().value("id").toString();
            if (byId.contains(id) && !placed.contains(id)) {
                rows << byId.value(id);
                placed.insert(id);
                count(key, byId.value(id));
            }
        }
    }
    bool headerForRest = !m_groupsData.isEmpty();
    for (const auto &v : jobs) {
        const QString id = v.toObject().value("id").toString();
        if (placed.contains(id)) continue;
        if (headerForRest) {
            rows << QJsonObject{{"__group", QJsonObject{{"__ungrouped", true}}}};
            headerForRest = false;
        }
        rows << v.toObject();
        count(ungroupedKey, v.toObject());
    }

    m_jobs->clearSpans();
    m_jobs->setRowCount(rows.size());
    QString sectionKey; // раздел, к которому относятся следующие строки заданий
    for (int i = 0; i < rows.size(); ++i) {
        if (rows.at(i).contains("__group")) {
            const QJsonObject g = rows.at(i).value("__group").toObject();
            const bool ungrouped = g.value("__ungrouped").toBool();
            sectionKey = ungrouped ? ungroupedKey : g.value("id").toString();
            const bool collapsed = m_collapsed.contains(sectionKey);
            m_jobs->setRowHidden(i, false);
            for (int c = 0; c < ColCount; ++c) {
                auto *it = cell(m_jobs, i, c);
                it->setText({});
                it->setToolTip({});
                it->setData(Qt::UserRole, QVariant());
                it->setData(GroupIdRole, QVariant());
                it->setData(SectionRole, QVariant());
                it->setData(theme::StateRole, QVariant());
                it->setForeground(QBrush(theme::mutedColor()));
            }
            // Стрелка — признак, что раздел сворачивается щелчком.
            QString title = collapsed ? QStringLiteral("▸  ") : QStringLiteral("▾  ");
            if (ungrouped) {
                title += tr("Без группы");
            } else {
                const bool on = g.value("enabled").toBool(true);
                title += tr("Группа «%1» · заданий: %2 · %3")
                             .arg(g.value("name").toString())
                             .arg(g.value("jobs").toArray().size())
                             .arg(on ? util::scheduleSummary(g.value("schedule").toObject()) : tr("отключена"));
                const QString next = g.value("next_run").toString();
                if (on && !next.isEmpty()) title += tr(" · следующий запуск: %1").arg(util::formatWhen(next));
                m_jobs->item(i, ColState)->setData(GroupIdRole, g.value("id").toString());
            }
            if (collapsed) {
                const Summary s = summary.value(sectionKey);
                QStringList parts;
                if (s.running) parts << tr("выполняется");
                if (s.queued) parts << tr("в очереди: %1").arg(s.queued);
                if (s.errors) parts << tr("ошибок: %1").arg(s.errors);
                if (s.warnings) parts << tr("предупреждений: %1").arg(s.warnings);
                if (!parts.isEmpty()) title += QStringLiteral(" · ") + parts.join(QStringLiteral(" · "));
            }
            auto *head = m_jobs->item(i, ColState);
            head->setText(title);
            head->setData(SectionRole, sectionKey);
            head->setToolTip(ungrouped ? tr("Щелчок — свернуть или развернуть")
                                       : tr("Щелчок — свернуть или развернуть. Изменить группу — F2 или правая кнопка"));
            QFont f;
            f.setWeight(QFont::Bold);
            head->setFont(f);
            // Без StateRole делегат первой колонки рисует обычный текст, без плашки.
            m_jobs->setSpan(i, 0, 1, ColCount);
            if (!ungrouped && g.value("id").toString() == selGroup) m_jobs->selectRow(i);
            continue;
        }
        m_jobs->setRowHidden(i, !sectionKey.isEmpty() && m_collapsed.contains(sectionKey));
        cell(m_jobs, i, ColState)->setData(SectionRole, QVariant());
        const QJsonObject j = rows.at(i);
        const bool on = j.value("enabled").toBool();
        const QString last = j.value("last_status").toString();
        const QString lastMessage = j.value("last_error").toString();

        theme::State state = theme::Muted;
        QString stateText = tr("Нет запусков");
        QString message;
        if (j.value("running").toBool()) {
            state = theme::Info;
            stateText = tr("Выполняется");
        } else if (j.value("queued").toBool()) {
            state = theme::Info;
            stateText = tr("В очереди");
        } else if (!on) {
            stateText = tr("Отключено");
            message = lastMessage;
        } else if (last == QLatin1String("ok")) {
            state = theme::Ok;
            stateText = tr("Успешно");
        } else if (last == QLatin1String("warning")) {
            state = theme::Warn;
            stateText = tr("Предупреждение");
            message = lastMessage;
        } else if (!last.isEmpty()) {
            state = theme::Error;
            stateText = tr("Ошибка");
            message = lastMessage;
        }
        if (on && last == QLatin1String("error")) m_anyFailed = true;

        const QStringList cells{stateText,
                                j.value("name").toString(),
                                util::sourceTitle(j.value("source").toString()),
                                on ? util::scheduleSummary(j.value("schedule").toObject()) : QStringLiteral("—"),
                                util::formatWhen(j.value("next_run").toString()),
                                util::formatWhen(j.value("last_run").toString()),
                                message};
        for (int c = 0; c < cells.size(); ++c) {
            auto *it = cell(m_jobs, i, c);
            it->setText(cells.at(c));
            it->setForeground(on ? QBrush() : QBrush(theme::mutedColor()));
            it->setData(GroupIdRole, QVariant());
            it->setFont(QFont());
            it->setToolTip({});
        }
        m_jobs->item(i, ColState)->setData(theme::StateRole, int(state));
        auto *name = m_jobs->item(i, ColName);
        name->setData(Qt::UserRole, j.value("id").toString());
        QFont f;
        f.setWeight(QFont::DemiBold);
        name->setFont(f);
        m_jobs->item(i, ColMessage)->setToolTip(message);
        if (j.value("id").toString() == sel && !m_jobs->isRowHidden(i)) m_jobs->selectRow(i);
    }
    updateCards();
    updateActions();
    updateTray();
}

void MainWindow::applyHistory(const QJsonArray &history) {
    m_historyData = history;
    renderHistory();
    notifyFinished(history);
}

void MainWindow::renderHistory() {
    const bool problemsOnly = m_historyFilter->currentIndex() == 1;
    const QString onlyJob = m_historyOnlyJob->isChecked() ? selectedJobId() : QString();
    m_historyRows.clear();
    // Служба отдаёт старые записи первыми; в таблице новые сверху.
    for (int i = m_historyData.size() - 1; i >= 0; --i) {
        const QJsonObject h = m_historyData.at(i).toObject();
        const bool clean = h.value("ok").toBool() && h.value("warnings").toArray().isEmpty();
        if (problemsOnly && clean) continue;
        if (!onlyJob.isEmpty() && h.value("job_id").toString() != onlyJob) continue;
        m_historyRows.append(h);
    }
    m_history->setRowCount(m_historyRows.size());
    for (int i = 0; i < m_historyRows.size(); ++i) {
        const QJsonObject &h = m_historyRows.at(i);
        const bool ok = h.value("ok").toBool();
        const QJsonArray warns = h.value("warnings").toArray();
        const QDateTime s = util::parseIso(h.value("started").toString());
        const QDateTime f = util::parseIso(h.value("finished").toString());
        QString details;
        if (!ok) {
            details = h.value("error").toString();
        } else {
            const QJsonObject r = h.value("report").toObject();
            details = tr("%1 · объектов: %2 · %3 · назначений: %4")
                          .arg(r.value("set_name").toString())
                          .arg(r.value("items").toInteger())
                          .arg(util::formatBytes(r.value("bytes").toInteger()))
                          .arg(r.value("destinations_ok").toInt());
            for (const auto &w : warns)
                details += QStringLiteral(" · ") + w.toString();
        }
        const theme::State state = !ok ? theme::Error : warns.isEmpty() ? theme::Ok : theme::Warn;
        const QStringList cells{!ok ? tr("Ошибка") : warns.isEmpty() ? tr("Успешно") : tr("Предупреждение"),
                                util::formatWhen(h.value("started").toString()),
                                s.isValid() && f.isValid() ? util::formatDuration(s.secsTo(f)) : QStringLiteral("—"),
                                h.value("job").toString(), details};
        for (int c = 0; c < cells.size(); ++c)
            cell(m_history, i, c)->setText(cells.at(c));
        m_history->item(i, HistResult)->setData(theme::StateRole, int(state));
        m_history->item(i, HistDetails)->setToolTip(details);
    }
}

void MainWindow::showHistoryDetails(int row) {
    if (row < 0 || row >= m_historyRows.size()) return;
    const QJsonObject h = m_historyRows.at(row);
    const bool ok = h.value("ok").toBool();
    const QJsonArray warns = h.value("warnings").toArray();
    const QDateTime s = util::parseIso(h.value("started").toString());
    const QDateTime f = util::parseIso(h.value("finished").toString());
    QStringList lines{tr("Задание: %1").arg(h.value("job").toString()),
                      tr("Начало: %1").arg(util::formatDateTime(h.value("started").toString())),
                      tr("Окончание: %1").arg(util::formatDateTime(h.value("finished").toString())),
                      tr("Длительность: %1").arg(s.isValid() && f.isValid() ? util::formatDuration(s.secsTo(f)) : QStringLiteral("—")),
                      tr("Итог: %1").arg(!ok ? tr("ошибка") : warns.isEmpty() ? tr("успешно") : tr("успешно с предупреждениями"))};
    if (!ok) {
        lines << QString() << tr("Ошибка:") << h.value("error").toString();
    } else {
        const QJsonObject r = h.value("report").toObject();
        lines << QString() << tr("Набор: %1").arg(r.value("set_name").toString())
              << tr("Объектов: %1").arg(r.value("items").toInteger())
              << tr("Объём: %1").arg(util::formatBytes(r.value("bytes").toInteger()))
              << tr("Назначений: %1").arg(r.value("destinations_ok").toInt());
    }
    if (!warns.isEmpty()) {
        lines << QString() << tr("Предупреждения:");
        for (const auto &w : warns)
            lines << QStringLiteral("• ") + w.toString();
    }

    QDialog dlg(this);
    dlg.setWindowTitle(tr("Запуск задания «%1»").arg(h.value("job").toString()));
    dlg.resize(640, 420);
    auto *text = new QPlainTextEdit(lines.join(QLatin1Char('\n')));
    text->setReadOnly(true);
    auto *box = new QDialogButtonBox(QDialogButtonBox::Close);
    box->button(QDialogButtonBox::Close)->setText(tr("Закрыть"));
    connect(box, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
    auto *v = new QVBoxLayout(&dlg);
    v->addWidget(text, 1);
    v->addWidget(box);
    dlg.exec();
}

void MainWindow::notifyFinished(const QJsonArray &history) {
    if (history.isEmpty()) return;
    const QJsonObject last = history.last().toObject();
    const QString key = last.value("job_id").toString() + last.value("finished").toString();
    if (m_historyLoaded && key != m_lastHistoryKey) {
        const bool ok = last.value("ok").toBool();
        m_tray->showMessage(last.value("job").toString(),
                            ok ? tr("Резервная копия создана") : tr("Ошибка: %1").arg(last.value("error").toString()),
                            ok ? QSystemTrayIcon::Information : QSystemTrayIcon::Critical);
    }
    m_lastHistoryKey = key;
    m_historyLoaded = true;
}

void MainWindow::takeShots(const QString &dir) {
    QTimer::singleShot(2500, this, [this, dir] {
        grab().save(dir + "/main.png");
        JobDialog empty(m_client, {}, this);
        empty.show();
        QApplication::processEvents();
        empty.saveShots(dir, "new");
        empty.hide();

        const QString id = m_jobsData.isEmpty() ? QString() : m_jobsData.first().toObject().value("id").toString();
        {
            NotifyDialog nd(m_client, this);
            nd.show();
            QTimer::singleShot(800, &nd, [&nd, dir] {
                nd.grab().save(dir + "/notify.png");
                nd.reject();
            });
            nd.exec();
        }
        auto finish = [this, dir] {
            auto snap = [](QDialog &d, const QString &file) {
                d.show();
                QTimer::singleShot(1000, &d, [&d, file] {
                    d.grab().save(file);
                    d.reject();
                });
                d.exec();
            };
            {
                ResourcesDialog rd(m_client, this);
                snap(rd, dir + "/storages.png");
            }
            {
                QJsonObject sftp{{"name", "SFTP Selectel"},
                                 {"target", QJsonObject{{"type", "sftp"}, {"host", "backup.example.ru"}, {"port", 22},
                                                        {"user", "backup"}, {"password", "dpapi:stored"}, {"path", "/srv/backup"}}}};
                StorageDialog sd(m_client, sftp, this);
                snap(sd, dir + "/storage-sftp.png");
            }
            {
                QJsonObject gd{{"name", "Google Drive"},
                               {"target", QJsonObject{{"type", "google_drive"}, {"refresh_token", "dpapi:stored"},
                                                      {"email", "user@gmail.com"}, {"folder", "SteadyBackup"}}}};
                StorageDialog sd(m_client, gd, this);
                snap(sd, dir + "/storage-gdrive.png");
            }
            {
                DestinationDialog dd(m_client, QJsonObject{{"type", "storage"}, {"folder", "PayDesk"}}, this);
                snap(dd, dir + "/destination.png");
            }
            {
                UpdateDialog ud(m_client, m_admin, this);
                snap(ud, dir + "/update.png");
            }
            m_quitting = true;
            qApp->quit();
        };
        if (id.isEmpty()) return finish();
        m_client->call({{"cmd", "get_job"}, {"job_id", id}}, [this, dir, finish](bool ok, const QJsonValue &r, const QString &) {
            if (ok) {
                JobDialog dlg(m_client, r.toObject(), this);
                dlg.show();
                QApplication::processEvents();
                dlg.saveShots(dir, "edit");
            }
            finish();
        });
    });
}

QString MainWindow::selectedJobId() const {
    const auto rows = m_jobs->selectionModel()->selectedRows();
    if (rows.isEmpty()) return {};
    auto *it = m_jobs->item(rows.first().row(), ColName);
    return it ? it->data(Qt::UserRole).toString() : QString();
}

QString MainWindow::sectionAt(int row) const {
    auto *it = row >= 0 ? m_jobs->item(row, ColState) : nullptr;
    return it ? it->data(SectionRole).toString() : QString();
}

void MainWindow::setSectionCollapsed(const QString &key, bool collapsed) {
    if (key.isEmpty() || m_collapsed.contains(key) == collapsed) return;
    if (collapsed)
        m_collapsed << key;
    else
        m_collapsed.removeAll(key);
    // Состояние запоминается для этого пользователя между запусками программы.
    QSettings().setValue(QStringLiteral("jobs/collapsed"), m_collapsed);
    applyJobs(m_jobsData);
}

void MainWindow::setAllCollapsed(bool collapsed) {
    QStringList keys;
    for (int r = 0; r < m_jobs->rowCount(); ++r)
        if (!sectionAt(r).isEmpty()) keys << sectionAt(r);
    m_collapsed = collapsed ? keys : QStringList();
    QSettings().setValue(QStringLiteral("jobs/collapsed"), m_collapsed);
    applyJobs(m_jobsData);
}

bool MainWindow::eventFilter(QObject *obj, QEvent *e) {
    if (obj == m_jobs->viewport() && (e->type() == QEvent::MouseButtonPress || e->type() == QEvent::MouseButtonDblClick)) {
        auto *me = static_cast<QMouseEvent *>(e);
        if (me->button() != Qt::LeftButton) return QMainWindow::eventFilter(obj, e);
        const int row = m_jobs->rowAt(me->position().toPoint().y());
        const QString key = sectionAt(row);
        if (!key.isEmpty()) {
            // Второе нажатие двойного щелчка игнорируется: иначе раздел свернулся бы и сразу развернулся.
            if (e->type() == QEvent::MouseButtonDblClick) return true;
            // Щелчок по заголовку выделяет группу (для «Запустить», «Изменить») и сворачивает её.
            m_jobs->selectRow(row);
            setSectionCollapsed(key, !m_collapsed.contains(key));
            return true;
        }
    }
    if (obj == m_jobs && e->type() == QEvent::KeyPress) {
        const int key = static_cast<QKeyEvent *>(e)->key();
        const QString section = sectionAt(m_jobs->currentRow());
        if (!section.isEmpty()) {
            if (key == Qt::Key_Left) return setSectionCollapsed(section, true), true;
            if (key == Qt::Key_Right) return setSectionCollapsed(section, false), true;
            if (key == Qt::Key_Space) return setSectionCollapsed(section, !m_collapsed.contains(section)), true;
        }
    }
    return QMainWindow::eventFilter(obj, e);
}

QString MainWindow::selectedGroupId() const {
    const auto rows = m_jobs->selectionModel()->selectedRows();
    if (rows.isEmpty()) return {};
    auto *it = m_jobs->item(rows.first().row(), ColState);
    return it ? it->data(GroupIdRole).toString() : QString();
}

QJsonObject MainWindow::groupById(const QString &id) const {
    for (const auto &v : m_groupsData)
        if (v.toObject().value("id").toString() == id) return v.toObject();
    return {};
}

void MainWindow::newGroup() {
    if (!requireAdmin()) return;
    editGroup({});
}

void MainWindow::editGroup(const QJsonObject &group) {
    if (!requireAdmin()) return;
    GroupDialog dlg(group, m_jobsData, this);
    if (dlg.exec() != QDialog::Accepted) return;
    QJsonObject req = dlg.request();
    req["cmd"] = QStringLiteral("save_group");
    m_client->call(req, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) showError(tr("Не удалось сохранить группу:\n%1").arg(err));
        refresh();
    });
}

bool MainWindow::requireAdmin() {
    if (m_admin) return true;
    const auto answer = QMessageBox::question(
        this, tr("SteadyBackup"),
        tr("Изменять задания и настройки можно только с правами администратора.\n"
           "Перезапустить программу с правами администратора?"));
    if (answer == QMessageBox::Yes) restartElevated();
    return false;
}

void MainWindow::editWorkDir() {
    if (!requireAdmin()) return;
    m_client->call({{"cmd", "get_settings"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return showError(err);
        const QString effective = r.toObject().value("effective").toString();
        QDialog dlg(this);
        dlg.setWindowTitle(tr("Рабочий каталог"));
        auto *edit = new QLineEdit(r.toObject().value("work_dir").toString());
        edit->setPlaceholderText(tr("по умолчанию — %1").arg(effective));
        edit->setMinimumWidth(420);
        auto *browse = new QPushButton(tr("Обзор…"));
        QObject::connect(browse, &QPushButton::clicked, &dlg, [edit, &dlg] {
            const QString d = QFileDialog::getExistingDirectory(&dlg, dlg.tr("Рабочий каталог"), edit->text());
            if (!d.isEmpty()) edit->setText(QDir::toNativeSeparators(d));
        });
        auto *row = new QHBoxLayout;
        row->addWidget(edit, 1);
        row->addWidget(browse);
        auto *hint = new QLabel(tr("Сюда пишутся временные файлы копии и архив. Для больших баз и ВМ укажите диск, "
                                   "отличный от системного. Пусто — папка work в каталоге данных. "
                                   "Права на выбранную папку будут только у SYSTEM и Администраторов."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        auto *form = new QFormLayout;
        form->addRow(tr("Папка:"), row);
        auto *box = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
        box->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
        box->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
        box->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
        QObject::connect(box, &QDialogButtonBox::accepted, &dlg, &QDialog::accept);
        QObject::connect(box, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
        auto *v = new QVBoxLayout(&dlg);
        v->addWidget(hint);
        v->addLayout(form);
        v->addWidget(box);
        util::keepButtonsOnScreen(&dlg);
        if (dlg.exec() != QDialog::Accepted) return;
        m_client->call({{"cmd", "save_settings"}, {"work_dir", edit->text().trimmed()}}, [this](bool ok, const QJsonValue &, const QString &err) {
            if (!ok) showError(err);
        });
    });
}

void MainWindow::showUpdates() {
    UpdateDialog dlg(m_client, m_admin, this);
    dlg.exec();
    refresh();
}

void MainWindow::restartElevated() {
    // ShellExecute «runas» показывает запрос UAC. Новый экземпляр работает в другом уровне
    // целостности и не видит защиту от второго запуска текущего, поэтому текущий закрываем.
    const std::wstring exe = QCoreApplication::applicationFilePath().toStdWString();
    const HINSTANCE r = ShellExecuteW(reinterpret_cast<HWND>(winId()), L"runas", exe.c_str(), L"--elevated", nullptr, SW_SHOWNORMAL);
    if (reinterpret_cast<INT_PTR>(r) <= 32) return; // отказ в UAC — остаёмся в режиме просмотра
    m_quitting = true;
    qApp->quit();
}

void MainWindow::showError(const QString &text) {
    QMessageBox::critical(this, tr("SteadyBackup"), text);
}

void MainWindow::jobMenu(const QPoint &pos) {
    if (auto *it = m_jobs->itemAt(pos)) m_jobs->selectRow(it->row());
    QMenu menu(this);
    menu.addAction(m_actRun);
    menu.addAction(m_actEdit);
    if (selectedGroupId().isEmpty()) menu.addAction(m_actCopy);
    menu.addSeparator();
    menu.addAction(m_actDelete);
    menu.addSeparator();
    menu.addAction(m_actNewGroup);
    if (!m_groupsData.isEmpty()) {
        menu.addAction(tr("Свернуть все группы"), this, [this] { setAllCollapsed(true); });
        menu.addAction(tr("Развернуть все группы"), this, [this] { setAllCollapsed(false); });
    }
    menu.exec(m_jobs->viewport()->mapToGlobal(pos));
}

void MainWindow::copyJob() {
    const QString id = selectedJobId();
    if (id.isEmpty() || !requireAdmin()) return;
    m_client->call({{"cmd", "copy_job"}, {"job_id", id}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return showError(err);
        statusBar()->showMessage(
            tr("Создана копия «%1». Она включена и пойдёт по тому же расписанию.").arg(r.toObject().value("name").toString()),
            6000);
        refresh();
    });
}

void MainWindow::exportSettings() {
    if (!requireAdmin()) return;
    if (QMessageBox::question(
            this, tr("Экспорт настроек"),
            tr("В файле будут все задания, хранилища, подключения и уведомления.\n"
               "Пароли и токены запишутся открытым текстом — иначе на другом компьютере их не прочитать.\n\n"
               "Храните файл как секрет и удалите после переноса.")) != QMessageBox::Yes)
        return;
    const QString path = QFileDialog::getSaveFileName(this, tr("Экспорт настроек"),
                                                      QDir::home().filePath(QStringLiteral("SteadyBackup.json")),
                                                      tr("Файл настроек (*.json)"));
    if (path.isEmpty()) return;
    m_client->call({{"cmd", "export_settings"}}, [this, path](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return showError(err);
        const QJsonObject o = r.toObject();
        const QString text = o.value(QStringLiteral("text")).toString();
        if (text.isEmpty()) return showError(tr("Служба вернула пустой файл экспорта."));
        QSaveFile f(path);
        if (!f.open(QIODevice::WriteOnly)) return showError(tr("Не удалось записать файл:\n%1").arg(path));
        f.write(text.toUtf8());
        if (!f.commit()) return showError(tr("Не удалось записать файл:\n%1").arg(path));
        QStringList lines{tr("Записано заданий: %1, хранилищ: %2, подключений: %3.")
                              .arg(o.value(QStringLiteral("jobs")).toInt())
                              .arg(o.value(QStringLiteral("storages")).toInt())
                              .arg(o.value(QStringLiteral("connections")).toInt()),
                          tr("Журнал и состояние инкрементных копий не переносятся.")};
        for (const auto &w : o.value(QStringLiteral("warnings")).toArray()) lines << w.toString();
        QMessageBox::information(this, tr("Экспорт настроек"), lines.join('\n'));
    });
}

void MainWindow::importSettings() {
    if (!requireAdmin()) return;
    const QString path = QFileDialog::getOpenFileName(this, tr("Импорт настроек"), QDir::homePath(),
                                                      tr("Файл настроек (*.json)"));
    if (path.isEmpty()) return;
    QFile f(path);
    if (!f.open(QIODevice::ReadOnly)) return showError(tr("Не удалось прочитать файл:\n%1").arg(path));
    const QByteArray bytes = f.readAll();
    const QJsonObject preview = QJsonDocument::fromJson(bytes).object();
    if (preview.value(QStringLiteral("format")).toString() != QStringLiteral("steadybackup"))
        return showError(tr("Это не файл экспорта SteadyBackup."));

    QMessageBox box(this);
    box.setWindowTitle(tr("Импорт настроек"));
    box.setIcon(QMessageBox::Question);
    box.setText(tr("Импортировать настройки?"));
    box.setInformativeText(tr("В файле: заданий %1, хранилищ %2, подключений %3.\n"
                              "Записи с теми же идентификаторами будут обновлены. Пароли запишутся в зашифрованном виде.\n"
                              "Пути к базам, gbak и папкам останутся как на исходном компьютере.\n"
                              "Журнал не переносится. Следующая инкрементная копия начнётся с полной.\n"
                              "Пропущенные по расписанию задания могут запуститься сразу после импорта.")
                               .arg(preview.value(QStringLiteral("jobs")).toArray().size())
                               .arg(preview.value(QStringLiteral("storages")).toArray().size())
                               .arg(preview.value(QStringLiteral("connections")).toArray().size()));
    auto *replace = new QCheckBox(tr("Удалить задания, хранилища и подключения, которых нет в файле"), &box);
    box.setCheckBox(replace);
    box.setStandardButtons(QMessageBox::Ok | QMessageBox::Cancel);
    box.setDefaultButton(QMessageBox::Cancel);
    if (box.exec() != QMessageBox::Ok) return;
    const bool wipe = replace->isChecked();

    m_client->call({{"cmd", "import_settings"}, {"text", QString::fromUtf8(bytes)}, {"replace", wipe}},
                   [this](bool ok, const QJsonValue &r, const QString &err) {
                       if (!ok) return showError(err);
                       const QJsonObject o = r.toObject();
                       QStringList lines{tr("Импортировано заданий: %1, хранилищ: %2, подключений: %3.")
                                             .arg(o.value(QStringLiteral("jobs")).toInt())
                                             .arg(o.value(QStringLiteral("storages")).toInt())
                                             .arg(o.value(QStringLiteral("connections")).toInt())};
                       if (o.value(QStringLiteral("deleted_jobs")).toInt() || o.value(QStringLiteral("deleted_storages")).toInt()
                           || o.value(QStringLiteral("deleted_connections")).toInt())
                           lines << tr("Удалено заданий: %1, хранилищ: %2, подключений: %3.")
                                        .arg(o.value(QStringLiteral("deleted_jobs")).toInt())
                                        .arg(o.value(QStringLiteral("deleted_storages")).toInt())
                                        .arg(o.value(QStringLiteral("deleted_connections")).toInt());
                       for (const auto &w : o.value(QStringLiteral("warnings")).toArray()) lines << w.toString();
                       QMessageBox::information(this, tr("Импорт настроек"), lines.join('\n'));
                       refresh();
                   });
}

void MainWindow::saveJob(const QJsonObject &job) {
    m_client->call({{"cmd", "save_job"}, {"job", job}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) showError(tr("Не удалось сохранить задание:\n%1").arg(err));
        refresh();
    });
}

void MainWindow::newJob() {
    if (!requireAdmin()) return;
    JobDialog dlg(m_client, {}, this);
    if (dlg.exec() == QDialog::Accepted) saveJob(dlg.value());
}

void MainWindow::editJob() {
    const QString gid = selectedGroupId();
    if (!gid.isEmpty()) return editGroup(groupById(gid));
    const QString id = selectedJobId();
    if (id.isEmpty()) return;
    if (!requireAdmin()) return;
    m_client->call({{"cmd", "get_job"}, {"job_id", id}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return showError(err);
        JobDialog dlg(m_client, r.toObject(), this);
        if (dlg.exec() == QDialog::Accepted) saveJob(dlg.value());
    });
}

void MainWindow::deleteJob() {
    const QString gid = selectedGroupId();
    if (!gid.isEmpty()) {
        if (!requireAdmin()) return;
        const QString name = groupById(gid).value("name").toString();
        if (QMessageBox::question(this, tr("Удаление"),
                                  tr("Удалить группу «%1»?\nЗадания группы не удаляются, они останутся без группы.")
                                      .arg(name)) != QMessageBox::Yes)
            return;
        m_client->call({{"cmd", "delete_group"}, {"group_id", gid}}, [this](bool ok, const QJsonValue &, const QString &err) {
            if (!ok) showError(err);
            refresh();
        });
        return;
    }
    const QString id = selectedJobId();
    if (id.isEmpty() || !requireAdmin()) return;
    const auto rows = m_jobs->selectionModel()->selectedRows();
    const QString name = rows.isEmpty() ? QString() : m_jobs->item(rows.first().row(), ColName)->text();
    if (QMessageBox::question(this, tr("Удаление"),
                              tr("Удалить задание «%1»?\nСозданные резервные копии не удаляются.").arg(name)) !=
        QMessageBox::Yes)
        return;
    m_client->call({{"cmd", "delete_job"}, {"job_id", id}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) showError(err);
        refresh();
    });
}

void MainWindow::cancelJob() {
    if (m_runningId.isEmpty() || !requireAdmin()) return;
    const QString id = m_runningId;
    m_client->call({{"cmd", "cancel_job"}, {"job_id", id}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) return showError(err);
        statusBar()->showMessage(tr("Задание отменяется"), 4000);
        refresh();
    });
}

void MainWindow::runJob() {
    const QString gid = selectedGroupId();
    if (!gid.isEmpty()) {
        m_client->call({{"cmd", "run_group"}, {"group_id", gid}}, [this](bool ok, const QJsonValue &r, const QString &err) {
            if (!ok) return showError(err);
            const int n = r.toObject().value("queued").toInt();
            statusBar()->showMessage(n > 0 ? tr("Заданий группы поставлено в очередь: %1").arg(n)
                                           : tr("Нечего запускать: задания группы отключены, уже в очереди или выполняются"),
                                     5000);
            refresh();
        });
        return;
    }
    const QString id = selectedJobId();
    if (id.isEmpty()) return;
    m_client->call({{"cmd", "run_job"}, {"job_id", id}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return showError(err);
        if (!r.toObject().value("queued").toBool())
            statusBar()->showMessage(tr("Задание уже выполняется или стоит в очереди"), 4000);
        refresh();
    });
}
