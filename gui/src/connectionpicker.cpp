#include "connectionpicker.h"

#include "resourcesdialog.h"
#include "serviceclient.h"
#include "util.h"

#include <QHBoxLayout>
#include <QJsonArray>
#include <QPushButton>

static const char *kInline = "__inline__";

// Подпись пункта «задано в задании».
static QString inlineWho(const QJsonObject &in) {
    const QJsonObject o = in.value("ssh").isObject() ? in.value("ssh").toObject() : in;
    QString who = o.value("server").toString(o.value("host").toString());
    const QString u = o.value("user").toString();
    if (!u.isEmpty()) who += (who.isEmpty() ? "" : " · ") + u;
    return who;
}

ConnectionPicker::ConnectionPicker(ServiceClient *client, const QString &type, const QString &noneText, QWidget *parent)
    : QWidget(parent), m_client(client), m_type(type), m_noneText(noneText) {
    m_combo = new SteadyCombo;
    m_combo->addItem(m_noneText.isEmpty() ? tr("не выбрано") : m_noneText, QString());
    m_combo->setEnabled(true);
    // Только выбор пользователя, а не программное заполнение списка.
    connect(m_combo, qOverload<int>(&QComboBox::activated), this, [this](int) {
        m_userPicked = true;
        Q_EMIT changed();
    });
    auto *manage = new QPushButton(tr("Подключения…"));
    connect(manage, &QPushButton::clicked, this, [this] {
        const QString cur = m_combo->currentData().toString();
        ResourcesDialog dlg(m_client, window(), 1);
        dlg.exec();
        reload(cur);
    });
    auto *l = new QHBoxLayout(this);
    l->setContentsMargins(0, 0, 0, 0);
    l->addWidget(m_combo, 1);
    l->addWidget(manage);
    reload({});
}

void ConnectionPicker::fill(const QString &select) {
    m_combo->clear();
    m_combo->setToolTip({});
    if (!m_noneText.isEmpty()) m_combo->addItem(m_noneText, QString());
    if (!m_inline.isEmpty()) {
        const QString who = inlineWho(m_inline);
        m_combo->addItem(tr("задано в задании: %1").arg(who.isEmpty() ? tr("без логина") : who), kInline);
    }
    // Сохранённую ссылку показываем сразу, не дожидаясь списка: иначе сохранение
    // до ответа службы записало бы пустое подключение.
    if (!m_savedConnection.isEmpty() && m_combo->findData(m_savedConnection) < 0)
        m_combo->addItem(tr("сохранённое подключение (%1)").arg(m_savedConnection), m_savedConnection);
    const int i = m_combo->findData(select);
    m_combo->setCurrentIndex(i >= 0 ? i : 0);
    m_combo->setEnabled(true);
}

void ConnectionPicker::reload(const QString &select) {
    const int seq = ++m_seq;
    m_ready = false;
    fill(select);
    m_client->call({{"cmd", "list_connections"}}, [this, select, seq](bool ok, const QJsonValue &r, const QString &err) {
        // Пока шёл запрос, список мог быть запрошен заново (например, после «Подключения…»).
        if (seq != m_seq) return;
        m_combo->clear();
        m_combo->setToolTip({});
        if (!m_noneText.isEmpty()) m_combo->addItem(m_noneText, QString());
        if (!m_inline.isEmpty()) {
            const QString who = inlineWho(m_inline);
            m_combo->addItem(tr("задано в задании: %1").arg(who.isEmpty() ? tr("без логина") : who), kInline);
        }
        int found = 0;
        if (!ok) {
            m_combo->addItem(tr("ошибка списка: %1").arg(err));
            m_combo->setToolTip(err);
        } else {
            for (const auto &c : r.toArray()) {
                const QJsonObject o = c.toObject();
                if (o.value("type").toString() != m_type) continue;
                QString label = o.value("name").toString();
                if (m_type == "ms_sql") label += "  (" + o.value("server").toString() + ")";
                if (m_type == "postgresql")
                    label += QStringLiteral("  (%1:%2)")
                                 .arg(o.value("host").toString(QStringLiteral("localhost")))
                                 .arg(o.value("port").toInt(5432));
                if (m_type == "ssh")
                    label += QStringLiteral("  (%1@%2)").arg(o.value("user").toString(), o.value("host").toString());
                m_combo->addItem(label, o.value("id").toString());
                ++found;
            }
        }
        // Подключение могло быть переименовано, удалено или ещё не попало в список.
        // Ссылку из задания всё равно оставляем, чтобы сохранение не стёрло учётные данные.
        if (!m_savedConnection.isEmpty() && m_combo->findData(m_savedConnection) < 0) {
            m_combo->addItem(tr("сохранённое подключение (%1)").arg(m_savedConnection), m_savedConnection);
            ++found;
        }
        if (found == 0 && m_savedConnection.isEmpty() && m_inline.isEmpty())
            m_combo->addItem(tr("нет подключений — нажмите «Подключения…»"));
        const int i = m_combo->findData(select);
        m_combo->setCurrentIndex(i >= 0 ? i : 0);
        m_combo->setEnabled(true);
        m_ready = true;
        Q_EMIT changed();
    }, this);
}

void ConnectionPicker::setSource(const QJsonObject &s) {
    m_savedConnection = s.value("connection").toString();
    m_inline = {};
    // Встроенные поля читаем и при наличии connection: если подключение удалят,
    // данные старого задания останутся.
    if (m_type == "ssh") {
        // У источников по SSH вход задания — вложенный объект "ssh". Пока выбрано подключение,
        // служба заполняет его сама, поэтому «заданным в задании» считаем его только без ссылки.
        if (s.value("ssh").isObject() && m_savedConnection.isEmpty()) m_inline["ssh"] = s.value("ssh");
    } else {
        for (const char *k : {"server", "host", "user", "password"})
            if (s.value(k).isString() && !s.value(k).toString().isEmpty()) m_inline[k] = s.value(k);
    }
    m_userPicked = false;
    const QString select = !m_savedConnection.isEmpty() ? m_savedConnection
                                                       : (m_inline.isEmpty() ? QString() : QString(kInline));
    reload(select);
}

void ConnectionPicker::apply(QJsonObject &s) const {
    const QString v = m_combo->currentData().toString();
    if (v == kInline) {
        for (auto it = m_inline.begin(); it != m_inline.end(); ++it) s[it.key()] = it.value();
        s.remove("connection");
        return;
    }
    if (!v.isEmpty()) {
        // Пункт не меняли — не трогаем поля задания: в них могут быть учётные
        // данные старого формата, которые служба использует, если подключение пропадёт.
        if (v == m_savedConnection) return;
        s["connection"] = v;
        if (m_type == "ssh") {
            s.remove("ssh");
            return;
        }
        s["user"] = QJsonValue(QJsonValue::Null);
        s["password"] = QJsonValue(QJsonValue::Null);
        if (m_type == "ms_sql") s["server"] = QString();
        if (m_type == "postgresql") {
            s["host"] = QJsonValue(QJsonValue::Null);
            s["port"] = QJsonValue(QJsonValue::Null);
        }
        return;
    }
    // Пустой пункт — «без подключения». Пока список не пришёл, сохраняем как было:
    // иначе «Сохранить» до ответа службы стирает ссылку и пароль.
    if (!m_ready) return;
    if (!m_savedConnection.isEmpty() && !m_userPicked) return;
    s["connection"] = QJsonValue(QJsonValue::Null);
    if (m_type == "ssh") {
        if (m_inline.isEmpty()) s.remove("ssh");
        else s["ssh"] = m_inline.value("ssh");
        return;
    }
    if (m_inline.isEmpty()) {
        s["user"] = QJsonValue(QJsonValue::Null);
        s["password"] = QJsonValue(QJsonValue::Null);
        if (m_type == "ms_sql") s["server"] = QString();
    } else {
        for (auto it = m_inline.begin(); it != m_inline.end(); ++it) s[it.key()] = it.value();
    }
}
