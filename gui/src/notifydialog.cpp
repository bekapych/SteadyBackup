#include "notifydialog.h"

#include "destinationdialog.h"
#include "serviceclient.h"
#include "theme.h"
#include "util.h"

#include <QCheckBox>
#include <QComboBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QInputDialog>
#include <QJsonArray>
#include <QLabel>
#include <QLineEdit>
#include <QMessageBox>
#include <QPushButton>
#include <QSpinBox>
#include <QVBoxLayout>

static QComboBox *whenCombo() {
    auto *c = new QComboBox;
    c->addItem(QObject::tr("Ошибки и предупреждения"), "problems");
    c->addItem(QObject::tr("Только ошибки"), "errors");
    c->addItem(QObject::tr("Каждый запуск"), "always");
    return c;
}

static QLabel *resultLabel() {
    auto *l = new QLabel;
    l->setWordWrap(true);
    l->setTextInteractionFlags(Qt::TextSelectableByMouse);
    return l;
}

NotifyDialog::NotifyDialog(ServiceClient *client, QWidget *parent) : QDialog(parent), m_client(client) {
    setWindowTitle(tr("Уведомления"));
    resize(560, 0);

    m_computer = new QLineEdit;
    m_computer->setPlaceholderText(qEnvironmentVariable("COMPUTERNAME"));

    // --- e-mail ---
    auto *mail = new QGroupBox(tr("Электронная почта"));
    m_mailOn = new QCheckBox(tr("Отправлять письма"));
    m_mailWhen = whenCombo();
    m_mailServer = new QLineEdit;
    m_mailServer->setPlaceholderText(QStringLiteral("smtp.yandex.ru"));
    m_mailPort = new QSpinBox;
    m_mailPort->setRange(1, 65535);
    m_mailPort->setValue(465);
    m_mailSecurity = new QComboBox;
    m_mailSecurity->addItem(QStringLiteral("SSL/TLS"), "tls");
    m_mailSecurity->addItem(QStringLiteral("STARTTLS"), "start_tls");
    m_mailSecurity->addItem(tr("Без шифрования"), "none");
    connect(m_mailSecurity, &QComboBox::currentIndexChanged, this, [this] {
        const QString s = m_mailSecurity->currentData().toString();
        if (m_mailPort->value() == 465 || m_mailPort->value() == 587 || m_mailPort->value() == 25)
            m_mailPort->setValue(s == "tls" ? 465 : s == "start_tls" ? 587 : 25);
    });
    m_mailUser = new QLineEdit;
    m_mailPassword = new PasswordEdit;
    m_mailFrom = new QLineEdit;
    m_mailFrom->setPlaceholderText(QStringLiteral("SteadyBackup <backup@company.ru>"));
    m_mailTo = new QLineEdit;
    m_mailTo->setPlaceholderText(QStringLiteral("admin@company.ru, it@company.ru"));
    m_mailResult = resultLabel();
    auto *mailTest = new QPushButton(tr("Отправить тестовое письмо"));
    connect(mailTest, &QPushButton::clicked, this, [this] { test("email", m_mailResult); });

    auto *sp = new QWidget;
    auto *sl = new QHBoxLayout(sp);
    sl->setContentsMargins(0, 0, 0, 0);
    sl->addWidget(m_mailServer, 1);
    sl->addWidget(new QLabel(tr("порт")));
    sl->addWidget(m_mailPort);
    sl->addWidget(m_mailSecurity);

    auto *mf = new QFormLayout(mail);
    mf->addRow(m_mailOn);
    mf->addRow(tr("Когда:"), m_mailWhen);
    mf->addRow(tr("SMTP-сервер:"), sp);
    mf->addRow(tr("Логин:"), m_mailUser);
    mf->addRow(tr("Пароль:"), m_mailPassword);
    mf->addRow(tr("От кого:"), m_mailFrom);
    mf->addRow(tr("Кому:"), m_mailTo);
    auto *mh = new QHBoxLayout;
    mh->addWidget(mailTest);
    mh->addWidget(m_mailResult, 1);
    mf->addRow(mh);
    auto *mailHint = new QLabel(tr("Для Яндекс и Gmail используйте пароль приложения, а не основной пароль."));
    theme::markHint(mailHint);
    mailHint->setWordWrap(true);
    mf->addRow(mailHint);

    // --- Telegram ---
    auto *tg = new QGroupBox(QStringLiteral("Telegram"));
    m_tgOn = new QCheckBox(tr("Отправлять сообщения"));
    m_tgWhen = whenCombo();
    m_tgToken = new PasswordEdit;
    m_tgChat = new QLineEdit;
    m_tgChat->setPlaceholderText(tr("chat_id; несколько — через запятую"));
    auto *find = new QPushButton(tr("Найти…"));
    connect(find, &QPushButton::clicked, this, &NotifyDialog::findChats);
    auto *cw = new QWidget;
    auto *cl = new QHBoxLayout(cw);
    cl->setContentsMargins(0, 0, 0, 0);
    cl->addWidget(m_tgChat, 1);
    cl->addWidget(find);
    m_tgResult = resultLabel();
    auto *tgTest = new QPushButton(tr("Отправить тестовое сообщение"));
    connect(tgTest, &QPushButton::clicked, this, [this] { test("telegram", m_tgResult); });

    auto *tf = new QFormLayout(tg);
    tf->addRow(m_tgOn);
    tf->addRow(tr("Когда:"), m_tgWhen);
    tf->addRow(tr("Токен бота:"), m_tgToken);
    tf->addRow(tr("Чат:"), cw);
    auto *th = new QHBoxLayout;
    th->addWidget(tgTest);
    th->addWidget(m_tgResult, 1);
    tf->addRow(th);
    auto *tgHint = new QLabel(tr("Создайте бота у @BotFather и получите токен. Затем напишите боту любое сообщение "
                                 "(или добавьте его в группу) и нажмите «Найти…»."));
    theme::markHint(tgHint);
    tgHint->setWordWrap(true);
    tf->addRow(tgHint);

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *top = new QFormLayout;
    top->addRow(tr("Имя компьютера в сообщениях:"), m_computer);
    auto *v = new QVBoxLayout(this);
    v->addLayout(top);
    v->addWidget(mail);
    v->addWidget(tg);
    v->addStretch();
    v->addWidget(buttons);
    util::keepButtonsOnScreen(this);

    m_client->call({{"cmd", "get_notify"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (ok) load(r.toObject());
        else QMessageBox::warning(this, windowTitle(), err);
    }, this);
}

void NotifyDialog::load(const QJsonObject &s) {
    m_computer->setText(s.value("computer_name").toString());
    const QJsonObject e = s.value("email").toObject();
    m_mailOn->setChecked(e.value("enabled").toBool());
    m_mailWhen->setCurrentIndex(qMax(0, m_mailWhen->findData(e.value("when").toString("problems"))));
    m_mailServer->setText(e.value("server").toString());
    m_mailSecurity->setCurrentIndex(qMax(0, m_mailSecurity->findData(e.value("security").toString("tls"))));
    m_mailPort->setValue(e.value("port").toInt(465));
    m_mailUser->setText(e.value("user").toString());
    m_mailPassword->setStored(e.value("password").toString());
    m_mailFrom->setText(e.value("from").toString());
    m_mailTo->setText(e.value("to").toString());
    const QJsonObject t = s.value("telegram").toObject();
    m_tgOn->setChecked(t.value("enabled").toBool());
    m_tgWhen->setCurrentIndex(qMax(0, m_tgWhen->findData(t.value("when").toString("problems"))));
    m_tgToken->setStored(t.value("bot_token").toString());
    m_tgChat->setText(t.value("chat_id").toString());
}

QJsonObject NotifyDialog::value() const {
    QJsonObject s;
    s["computer_name"] = util::optString(m_computer->text());
    s["email"] = QJsonObject{
        {"enabled", m_mailOn->isChecked()},
        {"when", m_mailWhen->currentData().toString()},
        {"server", m_mailServer->text().trimmed()},
        {"port", m_mailPort->value()},
        {"security", m_mailSecurity->currentData().toString()},
        {"user", util::optString(m_mailUser->text())},
        {"password", m_mailPassword->value()},
        {"from", m_mailFrom->text().trimmed()},
        {"to", m_mailTo->text().trimmed()},
    };
    const QJsonValue token = m_tgToken->value();
    s["telegram"] = QJsonObject{
        {"enabled", m_tgOn->isChecked()},
        {"when", m_tgWhen->currentData().toString()},
        {"bot_token", token.isString() ? token : QJsonValue("")},
        {"chat_id", m_tgChat->text().trimmed()},
    };
    return s;
}

void NotifyDialog::test(const QString &channel, QLabel *result) {
    theme::setState(result, theme::None);
    result->setText(tr("Отправка…"));
    m_client->call({{"cmd", "test_notify"}, {"channel", channel}, {"settings", value()}},
                   [result](bool ok, const QJsonValue &, const QString &err) {
                       theme::setState(result, ok ? theme::Ok : theme::Error);
                       result->setText(ok ? tr("✔ Отправлено") : QStringLiteral("✖ ") + err);
                   }, this);
}

void NotifyDialog::findChats() {
    const QJsonValue token = m_tgToken->value();
    if (!token.isString()) {
        QMessageBox::information(this, windowTitle(), tr("Сначала укажите токен бота."));
        return;
    }
    m_client->call({{"cmd", "telegram_chats"}, {"bot_token", token}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            QMessageBox::warning(this, windowTitle(), err);
            return;
        }
        const QJsonArray chats = r.toArray();
        if (chats.isEmpty()) {
            QMessageBox::information(this, windowTitle(),
                                     tr("Бот пока не получал сообщений. Напишите ему что-нибудь в Telegram "
                                        "(или добавьте в группу) и нажмите «Найти…» ещё раз."));
            return;
        }
        QStringList items;
        for (const auto &c : chats) {
            const QJsonObject o = c.toObject();
            items << QStringLiteral("%1 — %2").arg(o.value("id").toString(), o.value("title").toString());
        }
        bool picked = false;
        const QString sel = QInputDialog::getItem(this, windowTitle(), tr("Куда отправлять уведомления:"), items, 0, false, &picked);
        if (!picked) return;
        const QString id = sel.section(QStringLiteral(" — "), 0, 0);
        QStringList cur = util::splitList(m_tgChat->text());
        if (!cur.contains(id)) cur << id;
        m_tgChat->setText(cur.join(", "));
    }, this);
}

void NotifyDialog::accept() {
    m_client->call({{"cmd", "save_notify"}, {"settings", value()}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (ok) QDialog::accept();
        else QMessageBox::critical(this, windowTitle(), tr("Не удалось сохранить:\n%1").arg(err));
    }, this);
}
