#include "updatedialog.h"

#include "serviceclient.h"
#include "theme.h"
#include "util.h"

#include <QCheckBox>
#include <QDialogButtonBox>
#include <QFormLayout>
#include <QHBoxLayout>
#include <QLabel>
#include <QMessageBox>
#include <QPushButton>
#include <QTextBrowser>
#include <QTimer>
#include <QVBoxLayout>

UpdateDialog::UpdateDialog(ServiceClient *client, bool admin, QWidget *parent)
    : QDialog(parent), m_client(client), m_admin(admin) {
    setWindowTitle(tr("Обновление SteadyBackup"));
    resize(560, 460);

    m_current = new QLabel(tr("—"));
    m_latest = new QLabel(tr("—"));
    m_latest->setTextFormat(Qt::RichText);
    m_latest->setOpenExternalLinks(true);
    m_state = new QLabel;
    m_state->setWordWrap(true);
    m_state->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_last = new QLabel;
    m_last->setWordWrap(true);
    m_last->setTextInteractionFlags(Qt::TextSelectableByMouse);
    m_notes = new QTextBrowser;
    m_notes->setOpenExternalLinks(true);
    m_notes->setMinimumHeight(140);

    m_check = new QCheckBox(tr("Проверять обновления автоматически (раз в сутки)"));
    m_auto = new QCheckBox(tr("Устанавливать обновления автоматически, когда нет выполняющихся заданий"));
    connect(m_check, &QCheckBox::toggled, this, [this](bool on) {
        m_auto->setEnabled(on && m_admin);
        saveSettings();
    });
    connect(m_auto, &QCheckBox::toggled, this, [this] { saveSettings(); });

    m_checkButton = new QPushButton(tr("Проверить сейчас"));
    connect(m_checkButton, &QPushButton::clicked, this, &UpdateDialog::check);
    m_installButton = new QPushButton(tr("Установить"));
    m_installButton->setObjectName(QStringLiteral("primary"));
    connect(m_installButton, &QPushButton::clicked, this, &UpdateDialog::install);

    auto *f = new QFormLayout;
    f->addRow(tr("Установлена версия:"), m_current);
    f->addRow(tr("Последняя версия:"), m_latest);
    auto *row = new QHBoxLayout;
    row->addWidget(m_checkButton);
    row->addWidget(m_installButton);
    row->addStretch();

    auto *hint = new QLabel(tr("Обновления берутся из выпусков на GitHub (bekapych/SteadyBackup). Установщик проверяется "
                               "по контрольной сумме SHA-256 из выпуска. Во время установки служба перезапускается "
                               "(около минуты); задания и настройки сохраняются."));
    hint->setWordWrap(true);
    theme::markHint(hint);

    auto *box = new QDialogButtonBox(QDialogButtonBox::Close);
    box->button(QDialogButtonBox::Close)->setText(tr("Закрыть"));
    connect(box, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *v = new QVBoxLayout(this);
    v->addLayout(f);
    v->addWidget(m_state);
    v->addLayout(row);
    v->addWidget(new QLabel(tr("Что нового:")));
    v->addWidget(m_notes, 1);
    v->addWidget(m_last);
    v->addWidget(m_check);
    v->addWidget(m_auto);
    v->addWidget(hint);
    v->addWidget(box);
    util::keepButtonsOnScreen(this);

    if (!m_admin) {
        for (QWidget *w : std::initializer_list<QWidget *>{m_check, m_auto, m_checkButton, m_installButton})
            w->setEnabled(false);
        m_state->setText(tr("Проверять и устанавливать обновления может только администратор."));
        theme::setState(m_state, theme::Info);
    }

    // Пока идёт загрузка или установка, состояние обновляется само.
    m_poll = new QTimer(this);
    m_poll->setInterval(2000);
    connect(m_poll, &QTimer::timeout, this, &UpdateDialog::reload);
    reload();
}

void UpdateDialog::reload() {
    m_client->call({{"cmd", "update_status"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            // Во время установки служба перезапускается: это ожидаемо.
            if (m_status.value("phase").toString() == "installing") {
                m_state->setText(tr("Служба перезапускается после установки…"));
                theme::setState(m_state, theme::Muted);
                return;
            }
            m_state->setText(tr("Служба недоступна: %1").arg(err));
            theme::setState(m_state, theme::Error);
            return;
        }
        apply(r.toObject());
    }, this);
}

void UpdateDialog::apply(const QJsonObject &st) {
    const QString prevVersion = m_status.value("current").toString();
    m_status = st;
    m_loading = true;
    m_check->setChecked(st.value("check").toBool(true));
    m_auto->setChecked(st.value("auto_install").toBool(false));
    m_auto->setEnabled(m_admin && m_check->isChecked() && st.value("can_install").toBool());
    m_loading = false;

    m_current->setText(st.value("current").toString());
    const QJsonObject latest = st.value("latest").toObject();
    const QString checked = st.value("checked").toString();
    if (latest.isEmpty()) {
        m_latest->setText(checked.isEmpty() ? tr("ещё не проверялось") : tr("неизвестно"));
        m_notes->clear();
    } else {
        QString text = QStringLiteral("<a href=\"%1\">%2</a>").arg(latest.value("url").toString().toHtmlEscaped(),
                                                                    latest.value("version").toString().toHtmlEscaped());
        if (!checked.isEmpty()) text += tr(" · проверено %1").arg(util::formatWhen(checked).toHtmlEscaped());
        m_latest->setText(text);
        m_notes->setMarkdown(latest.value("notes").toString());
    }

    const QString phase = st.value("phase").toString();
    const bool busy = phase != QLatin1String("idle");
    const bool available = st.value("available").toBool();
    m_poll->setInterval(busy ? 2000 : 0);
    if (busy) m_poll->start();
    else m_poll->stop();
    m_checkButton->setEnabled(m_admin && !busy);
    m_installButton->setVisible(available);
    m_installButton->setEnabled(m_admin && !busy && st.value("can_install").toBool());
    if (available) m_installButton->setText(tr("Установить %1").arg(latest.value("version").toString()));

    if (!m_admin) {
        // Текст про права уже показан.
    } else if (phase == QLatin1String("checking")) {
        m_state->setText(tr("Проверка…"));
        theme::setState(m_state, theme::Muted);
    } else if (phase == QLatin1String("downloading")) {
        m_state->setText(tr("Загрузка установщика (%1)…").arg(util::formatBytes(latest.value("size").toInteger())));
        theme::setState(m_state, theme::Muted);
    } else if (phase == QLatin1String("installing")) {
        m_state->setText(tr("Установка… Служба и окно в трее перезапустятся."));
        theme::setState(m_state, theme::Info);
    } else if (!st.value("error").toString().isEmpty()) {
        m_state->setText(tr("Ошибка: %1").arg(st.value("error").toString()));
        theme::setState(m_state, theme::Error);
    } else if (available) {
        m_state->setText(tr("Доступна новая версия %1.").arg(latest.value("version").toString()));
        theme::setState(m_state, theme::Warn);
        if (!st.value("can_install").toBool())
            m_state->setText(m_state->text() + tr(" Установка возможна только из службы, не в консольном режиме."));
    } else if (!latest.isEmpty()) {
        m_state->setText(tr("Установлена последняя версия."));
        theme::setState(m_state, theme::Ok);
    } else {
        m_state->clear();
    }

    const QString last = st.value("last_result").toString();
    m_last->setText(last);
    m_last->setVisible(!last.isEmpty());
    theme::setState(m_last, st.value("last_result_ok").toBool() ? theme::Ok : theme::Error);

    if (!prevVersion.isEmpty() && prevVersion != st.value("current").toString())
        QMessageBox::information(this, windowTitle(), tr("SteadyBackup обновлён до версии %1.").arg(st.value("current").toString()));
}

void UpdateDialog::check() {
    m_checkButton->setEnabled(false);
    m_state->setText(tr("Проверка…"));
    theme::setState(m_state, theme::Muted);
    m_client->call({{"cmd", "check_update"}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            m_checkButton->setEnabled(m_admin);
            m_state->setText(tr("Ошибка: %1").arg(err));
            theme::setState(m_state, theme::Error);
            return;
        }
        apply(r.toObject());
    }, this);
}

void UpdateDialog::install() {
    const QString ver = m_status.value("latest").toObject().value("version").toString();
    if (QMessageBox::question(this, windowTitle(),
                              tr("Установить SteadyBackup %1?\nСлужба перезапустится, окно программы закроется и "
                                 "откроется снова.").arg(ver)) != QMessageBox::Yes)
        return;
    m_installButton->setEnabled(false);
    m_client->call({{"cmd", "install_update"}}, [this](bool ok, const QJsonValue &, const QString &err) {
        if (!ok) {
            m_state->setText(tr("Ошибка: %1").arg(err));
            theme::setState(m_state, theme::Error);
            m_installButton->setEnabled(m_admin);
            return;
        }
        m_status["phase"] = QStringLiteral("downloading");
        m_poll->start(2000);
        reload();
    }, this);
}

void UpdateDialog::saveSettings() {
    if (m_loading || !m_admin) return;
    QJsonObject s{{"check", m_check->isChecked()}, {"auto_install", m_check->isChecked() && m_auto->isChecked()}};
    m_client->call({{"cmd", "save_update_settings"}, {"settings", s}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) return (void)QMessageBox::warning(this, windowTitle(), err);
        apply(r.toObject());
    }, this);
}
