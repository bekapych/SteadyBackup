#include "resourcesdialog.h"

#include "destinationdialog.h"
#include "serviceclient.h"
#include "theme.h"
#include "util.h"

#include <QApplication>
#include <QComboBox>
#include <QDesktopServices>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QJsonArray>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPointer>
#include <QProgressDialog>
#include <QPushButton>
#include <QSpinBox>
#include <QTabWidget>
#include <QTableWidget>
#include <QUrl>
#include <QVBoxLayout>

static QTableWidget *makeTable(const QStringList &headers) {
    auto *t = new QTableWidget(0, headers.size());
    t->setHorizontalHeaderLabels(headers);
    t->setSelectionBehavior(QAbstractItemView::SelectRows);
    t->setSelectionMode(QAbstractItemView::SingleSelection);
    t->setEditTriggers(QAbstractItemView::NoEditTriggers);
    t->setShowGrid(false);
    t->setWordWrap(false);
    t->verticalHeader()->hide();
    t->verticalHeader()->setDefaultSectionSize(34);
    t->horizontalHeader()->setHighlightSections(false);
    t->horizontalHeader()->setDefaultAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    t->horizontalHeader()->setSectionResizeMode(QHeaderView::ResizeToContents);
    t->horizontalHeader()->setStretchLastSection(true);
    return t;
}

static QString connectionTypeTitle(const QString &t) {
    if (t == "ms_sql") return QObject::tr("SQL Server");
    if (t == "one_c") return QObject::tr("1С (пользователь)");
    if (t == "web_dav") return QObject::tr("WebDAV (источник)");
    if (t == "firebird") return QObject::tr("Firebird");
    if (t == "postgresql") return QObject::tr("PostgreSQL");
    return t;
}

// ===================== ResourcesDialog =====================

ResourcesDialog::ResourcesDialog(ServiceClient *client, QWidget *parent, int tab)
    : QDialog(parent), m_client(client) {
    setWindowTitle(tr("Хранилища и подключения"));
    resize(820, 480);

    auto *tabs = new QTabWidget;

    // --- хранилища ---
    auto *sp = new QWidget;
    m_storages = makeTable({tr("Название"), tr("Тип и адрес"), tr("Используется в заданиях")});
    auto *sAdd = new QPushButton(tr("Добавить…"));
    auto *sEdit = new QPushButton(tr("Изменить…"));
    auto *sDel = new QPushButton(tr("Удалить"));
    auto *sTest = new QPushButton(tr("Проверить"));
    auto *sMigrate = new QPushButton(tr("Перенести из заданий"));
    sMigrate->setToolTip(tr("Создать хранилища из назначений, заданных прямо в заданиях, и переключить задания на них"));
    auto *sResult = new QLabel;
    sResult->setWordWrap(true);
    connect(sAdd, &QPushButton::clicked, this, [this] { editStorage({}); });
    auto editCur = [this] {
        const int r = m_storages->currentRow();
        if (r >= 0) editStorage(m_storageData.at(r).toObject());
    };
    connect(sEdit, &QPushButton::clicked, this, editCur);
    connect(m_storages, &QTableWidget::cellDoubleClicked, this, editCur);
    connect(sDel, &QPushButton::clicked, this, [this] { deleteSelected(m_storages, m_storageData, "delete_storage"); });
    connect(sMigrate, &QPushButton::clicked, this, &ResourcesDialog::migrate);
    connect(sTest, &QPushButton::clicked, this, [this, sResult] {
        const int r = m_storages->currentRow();
        if (r < 0) return;
        theme::setState(sResult, theme::None);
        sResult->setText(tr("Проверка…"));
        m_client->call({{"cmd", "test_storage"}, {"id", m_storageData.at(r).toObject().value("id")}},
                       [sResult](bool ok, const QJsonValue &v, const QString &err) {
                           if (!ok) {
                               theme::setState(sResult, theme::Error);
                               sResult->setText(QStringLiteral("✖ ") + err);
                               return;
                           }
                           const QJsonObject o = v.toObject();
                           QString t = tr("✔ %1 — доступно").arg(o.value("target").toString());
                           if (o.value("free").isDouble())
                               t += tr(", свободно %1 из %2")
                                        .arg(util::formatBytes(o.value("free").toInteger()),
                                             util::formatBytes(o.value("total").toInteger()));
                           theme::setState(sResult, theme::Ok);
                           sResult->setText(t);
                       }, this);
    });
    // Кнопки строки доступны, только когда строка выбрана.
    auto syncStorageButtons = [this, sEdit, sDel, sTest] {
        const bool sel = m_storages->currentRow() >= 0 && m_storages->selectionModel()->hasSelection();
        for (auto *b : {sEdit, sDel, sTest}) b->setEnabled(sel);
    };
    connect(m_storages, &QTableWidget::itemSelectionChanged, this, syncStorageButtons);
    syncStorageButtons();
    auto *sb = new QVBoxLayout;
    for (auto *b : {sAdd, sEdit, sDel, sTest}) sb->addWidget(b);
    sb->addStretch();
    sb->addWidget(sMigrate);
    auto *sh = new QHBoxLayout;
    sh->addWidget(m_storages, 1);
    sh->addLayout(sb);
    auto *sv = new QVBoxLayout(sp);
    auto *sHint = new QLabel(tr("Хранилище настраивается один раз — с паролями и входом в аккаунт. "
                                "В заданиях выбирается хранилище и подпапка."));
    theme::markHint(sHint);
    sHint->setWordWrap(true);
    sv->addWidget(sHint);
    sv->addLayout(sh, 1);
    sv->addWidget(sResult);
    tabs->addTab(sp, tr("Хранилища"));

    // --- подключения ---
    auto *cp = new QWidget;
    m_connections = makeTable({tr("Название"), tr("Тип"), tr("Сервер / пользователь"), tr("Используется в заданиях")});
    auto *cAdd = new QPushButton(tr("Добавить…"));
    auto *cEdit = new QPushButton(tr("Изменить…"));
    auto *cDel = new QPushButton(tr("Удалить"));
    connect(cAdd, &QPushButton::clicked, this, [this] { editConnection({}); });
    auto editConn = [this] {
        const int r = m_connections->currentRow();
        if (r >= 0) editConnection(m_connectionData.at(r).toObject());
    };
    connect(cEdit, &QPushButton::clicked, this, editConn);
    connect(m_connections, &QTableWidget::cellDoubleClicked, this, editConn);
    connect(cDel, &QPushButton::clicked, this, [this] { deleteSelected(m_connections, m_connectionData, "delete_connection"); });
    auto syncConnectionButtons = [this, cEdit, cDel] {
        const bool sel = m_connections->currentRow() >= 0 && m_connections->selectionModel()->hasSelection();
        for (auto *b : {cEdit, cDel}) b->setEnabled(sel);
    };
    connect(m_connections, &QTableWidget::itemSelectionChanged, this, syncConnectionButtons);
    syncConnectionButtons();
    auto *cb = new QVBoxLayout;
    for (auto *b : {cAdd, cEdit, cDel}) cb->addWidget(b);
    cb->addStretch();
    auto *ch = new QHBoxLayout;
    ch->addWidget(m_connections, 1);
    ch->addLayout(cb);
    auto *cv = new QVBoxLayout(cp);
    auto *cHint = new QLabel(tr("Учётные данные источников: SQL Server, пользователь 1С, WebDAV, Firebird. "
                                "Пароль меняется в одном месте для всех заданий."));
    theme::markHint(cHint);
    cHint->setWordWrap(true);
    cv->addWidget(cHint);
    cv->addLayout(ch, 1);
    tabs->addTab(cp, tr("Подключения"));
    tabs->setCurrentIndex(qBound(0, tab, tabs->count() - 1));

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Close);
    buttons->button(QDialogButtonBox::Close)->setText(tr("Закрыть"));
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    auto *v = new QVBoxLayout(this);
    v->addWidget(tabs, 1);
    v->addWidget(buttons);
    util::keepButtonsOnScreen(this);

    refresh();
}

void ResourcesDialog::refresh() {
    m_client->call({{"cmd", "list_storages"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return (void)QMessageBox::warning(this, windowTitle(), err);
        m_storageData = r.toArray();
        util::setStorageCache(m_storageData);
        m_storages->setRowCount(m_storageData.size());
        for (int i = 0; i < m_storageData.size(); ++i) {
            const QJsonObject s = m_storageData.at(i).toObject();
            QStringList used;
            for (const auto &u : s.value("used_by").toArray()) used << u.toString();
            const QStringList cells{s.value("name").toString(), util::destinationSummary(s.value("target").toObject()),
                                    used.isEmpty() ? tr("—") : used.join(", ")};
            for (int c = 0; c < cells.size(); ++c) m_storages->setItem(i, c, new QTableWidgetItem(cells[c]));
        }
    }, this);
    m_client->call({{"cmd", "list_connections"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return (void)QMessageBox::warning(this, windowTitle(), err);
        m_connectionData = r.toArray();
        m_connections->setRowCount(m_connectionData.size());
        for (int i = 0; i < m_connectionData.size(); ++i) {
            const QJsonObject c = m_connectionData.at(i).toObject();
            QStringList used;
            for (const auto &u : c.value("used_by").toArray()) used << u.toString();
            QString who = c.value("user").toString();
            if (c.value("type") == "ms_sql")
                who = c.value("server").toString() + " · " + (who.isEmpty() ? tr("Windows-аутентификация") : who);
            if (c.value("type") == "postgresql")
                who = QStringLiteral("%1:%2 · %3")
                          .arg(c.value("host").toString(QStringLiteral("localhost")))
                          .arg(c.value("port").toInt(5432))
                          .arg(who.isEmpty() ? QStringLiteral("postgres") : who);
            const QStringList cells{c.value("name").toString(), connectionTypeTitle(c.value("type").toString()), who,
                                    used.isEmpty() ? tr("—") : used.join(", ")};
            for (int k = 0; k < cells.size(); ++k) m_connections->setItem(i, k, new QTableWidgetItem(cells[k]));
        }
    }, this);
}

void ResourcesDialog::editStorage(const QJsonObject &s) {
    StorageDialog dlg(m_client, s, this);
    if (dlg.exec() == QDialog::Accepted) refresh();
}

void ResourcesDialog::editConnection(const QJsonObject &c) {
    ConnectionDialog dlg(c, this);
    if (dlg.exec() != QDialog::Accepted) return;
    m_client->call({{"cmd", "save_connection"}, {"connection", dlg.value()}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) QMessageBox::critical(this, windowTitle(), err);
        refresh();
    }, this);
}

void ResourcesDialog::deleteSelected(QTableWidget *table, const QJsonArray &data, const QString &cmd) {
    const int r = table->currentRow();
    if (r < 0) return;
    const QJsonObject o = data.at(r).toObject();
    if (QMessageBox::question(this, windowTitle(), tr("Удалить «%1»?").arg(o.value("name").toString())) != QMessageBox::Yes)
        return;
    m_client->call({{"cmd", cmd}, {"id", o.value("id")}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) QMessageBox::warning(this, windowTitle(), err);
        refresh();
    }, this);
}

void ResourcesDialog::migrate() {
    if (QMessageBox::question(this, windowTitle(),
                              tr("Создать хранилища из назначений, которые заданы прямо в заданиях, и переключить "
                                 "задания на них?\nОдинаковые адреса объединяются в одно хранилище.")) != QMessageBox::Yes)
        return;
    m_client->call({{"cmd", "migrate_jobs"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return (void)QMessageBox::critical(this, windowTitle(), err);
        const QJsonObject o = r.toObject();
        QStringList created, changed;
        for (const auto &x : o.value("storages_created").toArray()) created << x.toString();
        for (const auto &x : o.value("jobs_changed").toArray()) changed << x.toString();
        QMessageBox::information(this, windowTitle(),
                                 changed.isEmpty() ? tr("Все задания уже используют хранилища.")
                                                   : tr("Создано хранилищ: %1\n%2\n\nПереключено заданий: %3\n%4")
                                                         .arg(created.size())
                                                         .arg(created.join("\n"))
                                                         .arg(changed.size())
                                                         .arg(changed.join("\n")));
        refresh();
    }, this);
}

// ===================== StorageDialog =====================

StorageDialog::StorageDialog(ServiceClient *client, const QJsonObject &storage, QWidget *parent)
    : QDialog(parent), m_client(client), m_original(storage) {
    setWindowTitle(storage.isEmpty() ? tr("Новое хранилище") : tr("Хранилище: %1").arg(storage.value("name").toString()));
    resize(580, 0);

    m_name = new QLineEdit(storage.value("name").toString());
    m_name->setPlaceholderText(tr("напр. NAS офис"));
    m_editor = new DestinationEditor;
    m_editor->load(storage.value("target").toObject());
    connect(m_editor, &DestinationEditor::googleSignInRequested, this, &StorageDialog::googleSignIn);

    auto *testBtn = new QPushButton(tr("Проверить подключение"));
    m_result = new QLabel;
    m_result->setWordWrap(true);
    m_result->setTextInteractionFlags(Qt::TextSelectableByMouse);
    connect(testBtn, &QPushButton::clicked, this, &StorageDialog::test);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *f = new QFormLayout;
    f->addRow(tr("Название:"), m_name);
    auto *th = new QHBoxLayout;
    th->addWidget(testBtn);
    th->addWidget(m_result, 1);
    auto *v = new QVBoxLayout(this);
    v->addLayout(f);
    v->addWidget(m_editor);
    v->addLayout(th);
    v->addStretch();
    v->addWidget(buttons);
    util::keepButtonsOnScreen(this);
}

QJsonObject StorageDialog::value() const {
    return QJsonObject{{"id", m_original.value("id").toString()},
                       {"name", m_name->text().trimmed()},
                       {"target", m_editor->value()}};
}

void StorageDialog::test() {
    theme::setState(m_result, theme::None);
    m_result->setText(tr("Проверка…"));
    m_client->call({{"cmd", "test_storage"}, {"storage", value()}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            theme::setState(m_result, theme::Error);
            m_result->setText(QStringLiteral("✖ ") + err);
            return;
        }
        const QJsonObject o = r.toObject();
        QString t = tr("✔ Подключено. Объектов: %1").arg(o.value("objects").toInt());
        if (o.value("free").isDouble())
            t += tr(", свободно %1 из %2")
                     .arg(util::formatBytes(o.value("free").toInteger()), util::formatBytes(o.value("total").toInteger()));
        const QString fp = o.value("host_key").toString();
        if (!fp.isEmpty() && m_editor->hostKeyEmpty()) {
            const auto answer = QMessageBox::question(
                this, windowTitle(),
                tr("Сервер предъявил отпечаток %1.\nСохранить его, чтобы следующие подключения проверяли ключ?").arg(fp));
            if (answer == QMessageBox::Yes) m_editor->setHostKey(fp);
            t += tr("\nОтпечаток: %1").arg(fp);
        }
        const QJsonArray warns = o.value("warnings").toArray();
        for (const auto &w : warns) t += QStringLiteral("\n") + w.toString();
        theme::setState(m_result, theme::Ok);
        m_result->setText(t);
    }, this);
}

void StorageDialog::googleSignIn() {
    const QString id = m_editor->googleClientId();
    const QString secret = m_editor->googleClientSecret();
    // Свой клиент: нужен секрет (введённый или сохранённый в этом хранилище).
    if (!id.isEmpty() && secret.isEmpty()) {
        QMessageBox::information(this, windowTitle(), tr("Укажите Client Secret своего OAuth-клиента."));
        return;
    }
    QJsonObject start{{"cmd", "gdrive_auth_start"}};
    if (!id.isEmpty()) start["client_id"] = id;
    m_client->call(start, [this, secret](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return (void)QMessageBox::critical(this, windowTitle(), err);
        const QJsonObject o = r.toObject();
        QDesktopServices::openUrl(QUrl(o.value("url").toString()));
        auto *wait = new QProgressDialog(tr("Подтвердите доступ в открывшемся окне браузера…"), tr("Отмена"), 0, 0, this);
        wait->setWindowTitle(tr("Вход в Google"));
        wait->setWindowModality(Qt::WindowModal);
        wait->setMinimumDuration(0);
        wait->show();
        // Отдельное соединение: ожидание до 5 минут не должно блокировать остальные запросы GUI.
        // Клиент живёт вместе с диалогом. Удалять его из собственного колбэка нельзя:
        // QMessageBox крутит вложенный цикл событий и уничтожает объект, пока колбэк ещё на стеке.
        auto *waiter = new ServiceClient(this);
        QPointer<QProgressDialog> waitPtr(wait);
        connect(wait, &QProgressDialog::canceled, wait, [waitPtr] {
            if (!waitPtr) return;
            waitPtr->setProperty("canceled", true);
            waitPtr->hide();
        });
        QJsonObject finish{{"cmd", "gdrive_auth_finish"}, {"flow", o.value("flow")}};
        if (!secret.isEmpty()) {
            finish["client_secret"] = secret;
            finish["storage_id"] = m_original.value("id");
        }
        QPointer<StorageDialog> self(this);
        waiter->call(finish, [self, waitPtr](bool ok, const QJsonValue &acc, const QString &err) {
            if (!self || !waitPtr || waitPtr->property("canceled").toBool()) return;
            waitPtr->hide();
            waitPtr->deleteLater();
            if (!ok) return (void)QMessageBox::warning(self, self->windowTitle(), err);
            self->m_editor->setGoogleAccount(acc.toObject());
            theme::setState(self->m_result, theme::Ok);
            QString t = self->tr("✔ Вход выполнен: %1").arg(acc.toObject().value("email").toString());
            if (acc.toObject().value("total").isDouble())
                t += self->tr(", свободно %1").arg(util::formatBytes(acc.toObject().value("total").toInteger() -
                                                                     acc.toObject().value("used").toInteger()));
            self->m_result->setText(t);
            self->raise();
            self->activateWindow();
        });
    }, this);
}

void StorageDialog::accept() {
    const QJsonObject s = value();
    if (s.value("name").toString().isEmpty()) return (void)QMessageBox::warning(this, windowTitle(), tr("Укажите название."));
    const QJsonObject t = s.value("target").toObject();
    if (t.value("type") == "google_drive" && t.value("refresh_token").isNull())
        return (void)QMessageBox::warning(this, windowTitle(), tr("Выполните вход в Google."));
    m_client->call({{"cmd", "save_storage"}, {"storage", s}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (ok) QDialog::accept();
        else QMessageBox::critical(this, windowTitle(), err);
    }, this);
}

// ===================== ConnectionDialog =====================

ConnectionDialog::ConnectionDialog(const QJsonObject &c, QWidget *parent) : QDialog(parent), m_original(c) {
    setWindowTitle(c.isEmpty() ? tr("Новое подключение") : tr("Подключение: %1").arg(c.value("name").toString()));
    resize(460, 0);
    m_name = new QLineEdit(c.value("name").toString());
    m_type = new QComboBox;
    m_type->addItem(tr("SQL Server"), "ms_sql");
    m_type->addItem(tr("Пользователь 1С"), "one_c");
    m_type->addItem(tr("WebDAV (источник файлов)"), "web_dav");
    m_type->addItem(tr("Firebird (SYSDBA)"), "firebird");
    m_type->addItem(tr("PostgreSQL"), "postgresql");
    const QString type = c.value("type").toString("ms_sql");
    m_type->setCurrentIndex(qMax(0, m_type->findData(type)));
    // У PostgreSQL сервер хранится в поле host, у SQL Server — в server.
    m_server = new QLineEdit(c.value(type == "postgresql" ? "host" : "server").toString());
    m_serverLabel = new QLabel(tr("Сервер:"));
    m_port = new QSpinBox;
    m_port->setRange(1, 65535);
    m_port->setValue(c.value("port").toInt(5432));
    m_portLabel = new QLabel(tr("Порт:"));
    m_user = new QLineEdit(c.value("user").toString());
    m_password = new PasswordEdit;
    m_password->setStored(c.value("password").toString());
    auto update = [this] {
        const bool sql = m_type->currentData() == "ms_sql";
        const bool pg = m_type->currentData() == "postgresql";
        m_server->setVisible(sql || pg);
        m_serverLabel->setVisible(sql || pg);
        m_port->setVisible(pg);
        m_portLabel->setVisible(pg);
        m_server->setPlaceholderText(pg ? QStringLiteral("localhost") : QStringLiteral(". или SQL01\\INSTANCE"));
        m_user->setPlaceholderText(sql ? tr("пусто — проверка подлинности Windows")
                                       : (pg ? QStringLiteral("postgres") : QString()));
    };
    connect(m_type, &QComboBox::currentIndexChanged, this, update);
    update();
    if (!c.isEmpty()) m_type->setEnabled(false);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    auto *f = new QFormLayout;
    f->addRow(tr("Название:"), m_name);
    f->addRow(tr("Тип:"), m_type);
    f->addRow(m_serverLabel, m_server);
    f->addRow(m_portLabel, m_port);
    f->addRow(tr("Пользователь:"), m_user);
    f->addRow(tr("Пароль:"), m_password);
    auto *v = new QVBoxLayout(this);
    v->addLayout(f);
    v->addWidget(buttons);
    util::keepButtonsOnScreen(this);
}

QJsonObject ConnectionDialog::value() const {
    QJsonObject c{{"id", m_original.value("id").toString()},
                  {"name", m_name->text().trimmed()},
                  {"type", m_type->currentData().toString()},
                  {"user", util::optString(m_user->text())},
                  {"password", m_password->value()}};
    if (m_type->currentData() == "ms_sql") c["server"] = m_server->text().trimmed();
    if (m_type->currentData() == "postgresql") {
        c["host"] = util::optString(m_server->text());
        c["port"] = m_port->value();
    }
    return c;
}

void ConnectionDialog::accept() {
    if (m_name->text().trimmed().isEmpty()) return (void)QMessageBox::warning(this, windowTitle(), tr("Укажите название."));
    if (m_type->currentData() == "ms_sql" && m_server->text().trimmed().isEmpty())
        return (void)QMessageBox::warning(this, windowTitle(), tr("Укажите сервер."));
    QDialog::accept();
}
