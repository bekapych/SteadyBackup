#include "destinationdialog.h"

#include "resourcesdialog.h"
#include "serviceclient.h"
#include "theme.h"
#include "util.h"

#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QDir>
#include <QFileDialog>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QJsonArray>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QSpinBox>
#include <QStackedWidget>
#include <QToolButton>
#include <QVBoxLayout>

// ---------- PasswordEdit ----------

PasswordEdit::PasswordEdit(QWidget *parent) : QWidget(parent) {
    auto *l = new QHBoxLayout(this);
    l->setContentsMargins(0, 0, 0, 0);
    m_edit = new QLineEdit;
    m_edit->setEchoMode(QLineEdit::Password);
    auto *clear = new QToolButton;
    clear->setText(QStringLiteral("×"));
    clear->setToolTip(tr("Удалить сохранённый пароль"));
    l->addWidget(m_edit);
    l->addWidget(clear);
    connect(clear, &QToolButton::clicked, this, [this] {
        m_stored.clear();
        m_edit->clear();
        updatePlaceholder();
    });
    updatePlaceholder();
}

void PasswordEdit::setStored(const QString &value) {
    m_stored = value;
    m_edit->clear();
    updatePlaceholder();
}

void PasswordEdit::updatePlaceholder() {
    m_edit->setPlaceholderText(m_stored.isEmpty() ? tr("не задан") : tr("сохранён — введите, чтобы изменить"));
}

QJsonValue PasswordEdit::value() const {
    if (!m_edit->text().isEmpty()) return m_edit->text();
    if (!m_stored.isEmpty()) return m_stored;
    return QJsonValue::Null;
}

// ---------- DestinationEditor ----------

static QWidget *withBrowse(QLineEdit *edit, QWidget *parent, bool dir = true) {
    auto *w = new QWidget(parent);
    auto *l = new QHBoxLayout(w);
    l->setContentsMargins(0, 0, 0, 0);
    auto *b = new QPushButton(QObject::tr("Обзор…"));
    l->addWidget(edit);
    l->addWidget(b);
    QObject::connect(b, &QPushButton::clicked, w, [edit, w, dir] {
        const QString p = dir ? QFileDialog::getExistingDirectory(w, QString(), edit->text())
                              : QFileDialog::getOpenFileName(w, QString(), edit->text());
        if (!p.isEmpty()) edit->setText(QDir::toNativeSeparators(p));
    });
    return w;
}

static QFormLayout *page(QStackedWidget *pages) {
    auto *w = new QWidget;
    auto *f = new QFormLayout(w);
    f->setContentsMargins(0, 0, 0, 0);
    pages->addWidget(w);
    return f;
}

DestinationEditor::DestinationEditor(QWidget *parent) : QWidget(parent) {
    m_type = new QComboBox;
    m_type->addItem(tr("Локальная папка / диск"), "local");
    m_type->addItem(tr("Сетевая папка (SMB / Samba)"), "smb");
    m_type->addItem(tr("SFTP / SSH"), "sftp");
    m_type->addItem(tr("WebDAV"), "web_dav");
    m_type->addItem(tr("Google Drive"), "google_drive");
    m_pages = new QStackedWidget;

    { // local
        auto *f = page(m_pages);
        m_localPath = new QLineEdit;
        m_localPath->setPlaceholderText(QStringLiteral("E:\\Backup"));
        f->addRow(tr("Корневая папка:"), withBrowse(m_localPath, this));
    }
    { // smb
        auto *f = page(m_pages);
        m_smbPath = new QLineEdit;
        m_smbPath->setPlaceholderText(QStringLiteral("\\\\server\\share\\backup"));
        m_smbUser = new QLineEdit;
        m_smbUser->setPlaceholderText(tr("необязательно, напр. NAS\\backup"));
        m_smbPassword = new PasswordEdit;
        f->addRow(tr("Путь UNC:"), m_smbPath);
        f->addRow(tr("Пользователь:"), m_smbUser);
        f->addRow(tr("Пароль:"), m_smbPassword);
    }
    { // sftp
        auto *f = page(m_pages);
        m_sftpHost = new QLineEdit;
        m_sftpPort = new QSpinBox;
        m_sftpPort->setRange(1, 65535);
        m_sftpPort->setValue(22);
        m_sftpUser = new QLineEdit;
        m_sftpPassword = new PasswordEdit;
        m_sftpKey = new QLineEdit;
        m_sftpKey->setPlaceholderText(tr("необязательно: закрытый ключ OpenSSH"));
        m_sftpKeyPass = new PasswordEdit;
        m_sftpHostKey = new QLineEdit;
        m_sftpHostKey->setPlaceholderText(QStringLiteral("SHA256:…  (рекомендуется)"));
        m_sftpPath = new QLineEdit;
        m_sftpPath->setPlaceholderText(QStringLiteral("/srv/backup"));
        m_sftpPost = new QLineEdit;
        m_sftpPost->setPlaceholderText(tr("необязательно: команда на сервере после копии"));
        auto *hp = new QWidget;
        auto *hl = new QHBoxLayout(hp);
        hl->setContentsMargins(0, 0, 0, 0);
        hl->addWidget(m_sftpHost, 1);
        hl->addWidget(new QLabel(tr("порт")));
        hl->addWidget(m_sftpPort);
        f->addRow(tr("Сервер:"), hp);
        f->addRow(tr("Пользователь:"), m_sftpUser);
        f->addRow(tr("Пароль:"), m_sftpPassword);
        f->addRow(tr("Ключ:"), withBrowse(m_sftpKey, this, false));
        f->addRow(tr("Пароль ключа:"), m_sftpKeyPass);
        f->addRow(tr("Отпечаток сервера:"), m_sftpHostKey);
        f->addRow(tr("Корневая папка:"), m_sftpPath);
        f->addRow(tr("Команда SSH:"), m_sftpPost);
    }
    { // webdav
        auto *f = page(m_pages);
        m_davUrl = new QLineEdit;
        m_davUrl->setPlaceholderText(QStringLiteral("https://webdav.yandex.ru/Backup"));
        m_davUser = new QLineEdit;
        m_davPassword = new PasswordEdit;
        f->addRow(tr("URL:"), m_davUrl);
        auto *httpWarn = new QLabel(tr("Пароль по http:// передаётся открытым текстом. Используйте https://."));
        httpWarn->setWordWrap(true);
        theme::setState(httpWarn, theme::Error);
        httpWarn->hide();
        connect(m_davUrl, &QLineEdit::textChanged, httpWarn, [httpWarn](const QString &t) {
            httpWarn->setVisible(t.trimmed().startsWith(QStringLiteral("http://"), Qt::CaseInsensitive));
        });
        f->addRow(httpWarn);
        f->addRow(tr("Пользователь:"), m_davUser);
        f->addRow(tr("Пароль:"), m_davPassword);
    }
    { // google drive
        auto *f = page(m_pages);
        m_gdFolder = new QLineEdit;
        m_gdFolder->setPlaceholderText(QStringLiteral("SteadyBackup"));
        m_gdState = new QLabel;
        m_gdState->setWordWrap(true);
        m_gdSignIn = new QPushButton(tr("Войти в Google…"));
        connect(m_gdSignIn, &QPushButton::clicked, this, &DestinationEditor::googleSignInRequested);
        auto *sw = new QWidget;
        auto *sl = new QHBoxLayout(sw);
        sl->setContentsMargins(0, 0, 0, 0);
        sl->addWidget(m_gdState, 1);
        sl->addWidget(m_gdSignIn);
        f->addRow(tr("Аккаунт:"), sw);
        f->addRow(tr("Корневая папка:"), m_gdFolder);
        auto *hint = new QLabel(tr("Программа получает доступ только к файлам, которые создаёт сама; "
                                   "остальное содержимое Диска ей не видно."));
        theme::markHint(hint);
        hint->setWordWrap(true);
        f->addRow(hint);

        // Свой OAuth-клиент — для организаций со своим проектом Google Cloud.
        m_gdCustom = new QCheckBox(tr("Использовать свой OAuth-клиент"));
        m_gdClientId = new QLineEdit;
        m_gdClientId->setPlaceholderText(QStringLiteral("…apps.googleusercontent.com"));
        m_gdClientSecret = new PasswordEdit;
        auto *cw = new QWidget;
        auto *cf = new QFormLayout(cw);
        cf->setContentsMargins(20, 0, 0, 0);
        cf->addRow(tr("Client ID:"), m_gdClientId);
        cf->addRow(tr("Client Secret:"), m_gdClientSecret);
        auto *chint = new QLabel(tr("Клиент типа «Desktop app» (Google Auth Platform → Clients). "
                                    "После смены клиента нужно войти заново."));
        theme::markHint(chint);
        chint->setWordWrap(true);
        cf->addRow(chint);
        cw->setVisible(false);
        connect(m_gdCustom, &QCheckBox::toggled, cw, &QWidget::setVisible);
        f->addRow(m_gdCustom);
        f->addRow(cw);
        updateGoogleState();
    }

    connect(m_type, &QComboBox::currentIndexChanged, m_pages, &QStackedWidget::setCurrentIndex);
    auto *top = new QFormLayout;
    top->setContentsMargins(0, 0, 0, 0);
    top->addRow(tr("Тип:"), m_type);
    auto *v = new QVBoxLayout(this);
    v->setContentsMargins(0, 0, 0, 0);
    v->addLayout(top);
    v->addWidget(m_pages);
}

QString DestinationEditor::type() const { return m_type->currentData().toString(); }
QString DestinationEditor::googleClientId() const {
    return m_gdCustom->isChecked() ? m_gdClientId->text().trimmed() : QString();
}
QString DestinationEditor::googleClientSecret() const {
    if (!m_gdCustom->isChecked()) return {};
    const QJsonValue v = m_gdClientSecret->value();
    return v.isString() ? v.toString() : QString();
}

void DestinationEditor::setHostKey(const QString &fingerprint) {
    m_sftpHostKey->setText(fingerprint);
}

bool DestinationEditor::hostKeyEmpty() const {
    return m_sftpHostKey->text().trimmed().isEmpty();
}

void DestinationEditor::setGoogleAccount(const QJsonObject &acc) {
    m_gdAccount = acc;
    if (acc.value("client_secret").isString()) m_gdClientSecret->setStored(acc.value("client_secret").toString());
    updateGoogleState();
}

void DestinationEditor::updateGoogleState() {
    const bool signed_in = !m_gdAccount.value("refresh_token").toString().isEmpty();
    const QString email = m_gdAccount.value("email").toString();
    m_gdState->setText(signed_in ? QStringLiteral("✔ ") + (email.isEmpty() ? tr("вход выполнен") : email) : tr("вход не выполнен"));
    theme::setState(m_gdState, signed_in ? theme::Ok : theme::Error);
    m_gdSignIn->setText(signed_in ? tr("Войти заново…") : tr("Войти в Google…"));
}

void DestinationEditor::load(const QJsonObject &d) {
    const QString t = d.value("type").toString("local");
    m_type->setCurrentIndex(qMax(0, m_type->findData(t)));
    m_pages->setCurrentIndex(m_type->currentIndex());
    if (t == "local") m_localPath->setText(d.value("path").toString());
    if (t == "smb") {
        m_smbPath->setText(d.value("path").toString());
        m_smbUser->setText(d.value("user").toString());
        m_smbPassword->setStored(d.value("password").toString());
    }
    if (t == "sftp") {
        m_sftpHost->setText(d.value("host").toString());
        m_sftpPort->setValue(d.value("port").toInt(22));
        m_sftpUser->setText(d.value("user").toString());
        m_sftpPassword->setStored(d.value("password").toString());
        m_sftpKey->setText(d.value("key_file").toString());
        m_sftpKeyPass->setStored(d.value("key_passphrase").toString());
        m_sftpHostKey->setText(d.value("host_key").toString());
        m_sftpPath->setText(d.value("path").toString());
        m_sftpPost->setText(d.value("post_command").toString());
    }
    if (t == "web_dav") {
        m_davUrl->setText(d.value("url").toString());
        m_davUser->setText(d.value("user").toString());
        m_davPassword->setStored(d.value("password").toString());
    }
    if (t == "google_drive") {
        const QString cid = d.value("client_id").toString();
        m_gdCustom->setChecked(!cid.isEmpty());
        m_gdClientId->setText(cid);
        m_gdFolder->setText(d.value("folder").toString());
        setGoogleAccount(QJsonObject{{"client_secret", d.value("client_secret")},
                                     {"refresh_token", d.value("refresh_token")},
                                     {"email", d.value("email")}});
    }
}

QJsonObject DestinationEditor::value() const {
    using util::optString;
    const QString t = type();
    QJsonObject d{{"type", t}};
    if (t == "local") d["path"] = m_localPath->text().trimmed();
    if (t == "smb") {
        d["path"] = m_smbPath->text().trimmed();
        d["user"] = optString(m_smbUser->text());
        d["password"] = m_smbPassword->value();
    }
    if (t == "sftp") {
        d["host"] = m_sftpHost->text().trimmed();
        d["port"] = m_sftpPort->value();
        d["user"] = m_sftpUser->text().trimmed();
        d["password"] = m_sftpPassword->value();
        d["key_file"] = optString(m_sftpKey->text());
        d["key_passphrase"] = m_sftpKeyPass->value();
        d["host_key"] = optString(m_sftpHostKey->text());
        d["path"] = m_sftpPath->text().trimmed();
        d["post_command"] = optString(m_sftpPost->text());
    }
    if (t == "web_dav") {
        d["url"] = m_davUrl->text().trimmed();
        d["user"] = optString(m_davUser->text());
        d["password"] = m_davPassword->value();
    }
    if (t == "google_drive") {
        const bool custom = m_gdCustom->isChecked();
        d["client_id"] = custom ? optString(m_gdClientId->text()) : QJsonValue(QJsonValue::Null);
        d["client_secret"] = custom ? m_gdClientSecret->value() : QJsonValue(QJsonValue::Null);
        d["refresh_token"] = m_gdAccount.value("refresh_token").toString().isEmpty() ? QJsonValue(QJsonValue::Null)
                                                                                      : m_gdAccount.value("refresh_token");
        d["email"] = m_gdAccount.value("email");
        d["folder"] = m_gdFolder->text().trimmed();
    }
    return d;
}

// ---------- DestinationDialog (назначение в задании) ----------

DestinationDialog::DestinationDialog(ServiceClient *client, const QJsonObject &dest, QWidget *parent)
    : QDialog(parent), m_client(client), m_original(dest) {
    setWindowTitle(tr("Назначение"));
    resize(520, 0);

    m_storage = new SteadyCombo;
    m_storage->addItem(tr("нет хранилищ — нажмите «Хранилища…»"));
    m_storage->setEnabled(true);
    auto *manage = new QPushButton(tr("Хранилища…"));
    connect(manage, &QPushButton::clicked, this, [this] {
        const QString cur = m_storage->currentData().toString();
        // Без родителя: иначе выпадающий список хранилища на Windows не открывается
        // поверх модального задания.
        ResourcesDialog dlg(m_client, nullptr);
        dlg.setWindowModality(Qt::ApplicationModal);
        dlg.exec();
        reloadStorages(cur);
    });
    auto *sw = new QWidget;
    auto *sl = new QHBoxLayout(sw);
    sl->setContentsMargins(0, 0, 0, 0);
    sl->addWidget(m_storage, 1);
    sl->addWidget(manage);

    m_folder = new QLineEdit;
    m_folder->setPlaceholderText(tr("напр. 1C/Бухгалтерия (пусто — корень хранилища)"));
    m_info = new QLabel;
    theme::markHint(m_info);
    m_info->setWordWrap(true);
    connect(m_storage, &QComboBox::currentIndexChanged, this, [this] {
        const QString id = m_storage->currentData().toString();
        for (const auto &s : std::as_const(m_storages))
            if (s.toObject().value("id").toString() == id)
                m_info->setText(util::destinationSummary(s.toObject().value("target").toObject()));
    });

    auto *testBtn = new QPushButton(tr("Проверить"));
    m_testResult = new QLabel;
    m_testResult->setWordWrap(true);
    connect(testBtn, &QPushButton::clicked, this, &DestinationDialog::test);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Готово"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *f = new QFormLayout;
    f->addRow(tr("Хранилище:"), sw);
    f->addRow(QString(), m_info);
    f->addRow(tr("Подпапка:"), m_folder);
    auto *th = new QHBoxLayout;
    th->addWidget(testBtn);
    th->addWidget(m_testResult, 1);
    auto *v = new QVBoxLayout(this);
    v->addLayout(f);
    v->addLayout(th);
    v->addStretch();
    v->addWidget(buttons);

    m_folder->setText(dest.value("folder").toString());
    reloadStorages(dest.value("storage").toString());
    util::keepButtonsOnScreen(this);
}

void DestinationDialog::prime(const QJsonArray &storages, const QString &error) {
    applyStorages(storages, m_original.value("storage").toString(), error);
}

bool DestinationDialog::comboHasStorage() const {
    for (int i = 0; i < m_storage->count(); ++i)
        if (!m_storage->itemData(i).toString().isEmpty()) return true;
    return false;
}

void DestinationDialog::applyStorages(const QJsonArray &list, const QString &select, const QString &err) {
    m_storages = list;
    m_storage->clear();
    m_storage->setEnabled(true);
    if (!err.isEmpty()) {
        m_storage->addItem(tr("ошибка: %1").arg(err));
        m_info->setText(err);
        return;
    }
    for (const auto &s : list) {
        const QJsonObject o = s.toObject();
        const QString name = o.value("name").toString();
        m_storage->addItem(name.isEmpty() ? o.value("id").toString() : name, o.value("id").toString());
    }
    if (m_storage->count() == 0) {
        m_storage->addItem(tr("нет хранилищ — нажмите «Хранилища…»"));
        m_info->setText(tr("Хранилищ пока нет — нажмите «Хранилища…», чтобы добавить."));
    } else if (!select.isEmpty() && m_storage->findData(select) < 0) {
        m_info->setText(tr("Сохранённое хранилище не найдено. Выберите другое или откройте «Хранилища…»."));
    }
    const int i = m_storage->findData(select);
    m_storage->setCurrentIndex(i >= 0 ? i : 0);
    Q_EMIT m_storage->currentIndexChanged(m_storage->currentIndex());
}

void DestinationDialog::reloadStorages(const QString &select) {
    m_client->call({{"cmd", "list_storages"}}, [this, select](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            m_info->setText(err.isEmpty() ? tr("служба не вернула список хранилищ") : err);
            if (!comboHasStorage())
                applyStorages({}, select, m_info->text());
            return;
        }
        const QJsonArray arr = r.isArray() ? r.toArray() : QJsonArray();
        if (!arr.isEmpty()) util::setStorageCache(arr);
        // Пустой ответ не должен стирать уже показанные хранилища.
        if (arr.isEmpty() && comboHasStorage()) {
            m_info->setText(tr("Служба вернула пустой список. Показаны уже известные хранилища."));
            return;
        }
        applyStorages(arr.isEmpty() ? util::storageCache() : arr, select, {});
    }, this);
}

QJsonObject DestinationDialog::value() const {
    return QJsonObject{{"type", "storage"},
                       {"storage", m_storage->currentData().toString()},
                       {"folder", m_folder->text().trimmed().replace('\\', '/')}};
}

void DestinationDialog::accept() {
    if (m_storage->currentData().toString().isEmpty()) {
        QMessageBox::warning(this, windowTitle(), tr("Выберите хранилище."));
        return;
    }
    QDialog::accept();
}

void DestinationDialog::test() {
    theme::setState(m_testResult, theme::None);
    m_testResult->setText(tr("Проверка…"));
    m_client->call({{"cmd", "test_storage"}, {"id", m_storage->currentData().toString()}, {"folder", value().value("folder")}},
                   [this](bool ok, const QJsonValue &r, const QString &err) {
                       if (!ok) {
                           theme::setState(m_testResult, theme::Error);
                           m_testResult->setText(QStringLiteral("✖ ") + err);
                           return;
                       }
                       const QJsonObject o = r.toObject();
                       QString text = tr("✔ Доступно. Объектов в папке: %1").arg(o.value("objects").toInt());
                       if (o.value("free").isDouble())
                           text += tr(", свободно %1").arg(util::formatBytes(o.value("free").toInteger()));
                       theme::setState(m_testResult, theme::Ok);
                       m_testResult->setText(text);
                   }, this);
}
