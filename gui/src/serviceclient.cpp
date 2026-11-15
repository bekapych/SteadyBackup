#include "serviceclient.h"

#include <QJsonDocument>
#include <QPointer>
#include <QRegularExpression>

// Имя канала службы. STEADY_PIPE — как у `steady console`: вторая копия для проверок и снимков.
static QString pipeName() {
    const QString n = qEnvironmentVariable("STEADY_PIPE");
    static const QRegularExpression ok(QStringLiteral("^[A-Za-z0-9_-]+$"));
    return ok.match(n).hasMatch() ? n : QStringLiteral("SteadyBackup");
}

ServiceClient::~ServiceClient() {
    // Сокет при уничтожении шлёт disconnected. Колбэк в этот момент видит уже
    // разрушаемый клиент и часто сам его удаляет — отсюда падение при отмене входа.
    m_socket.disconnect(this);
    m_pending.clear();
    m_socket.abort();
}

ServiceClient::ServiceClient(QObject *parent) : QObject(parent) {
    connect(&m_socket, &QLocalSocket::connected, this, [this] {
        for (const auto &line : std::as_const(m_outbox))
            m_socket.write(line);
        m_outbox.clear();
        if (!m_wasConnected) {
            m_wasConnected = true;
            Q_EMIT connectionChanged(true);
        }
    });
    connect(&m_socket, &QLocalSocket::readyRead, this, &ServiceClient::onReadyRead);
    connect(&m_socket, &QLocalSocket::errorOccurred, this, [this](QLocalSocket::LocalSocketError) {
        QString err = tr("Служба SteadyBackup недоступна: %1").arg(m_socket.errorString());
        m_socket.abort();
        failAll(err);
        if (m_wasConnected) {
            m_wasConnected = false;
            Q_EMIT connectionChanged(false);
        }
    });
    connect(&m_socket, &QLocalSocket::disconnected, this, [this] {
        failAll(tr("Соединение со службой разорвано"));
        if (m_wasConnected) {
            m_wasConnected = false;
            Q_EMIT connectionChanged(false);
        }
    });
}

void ServiceClient::ensureConnected() {
    if (m_socket.state() == QLocalSocket::UnconnectedState)
        m_socket.connectToServer(pipeName());
}

void ServiceClient::call(QJsonObject request, Callback cb, QObject *guard) {
    const qint64 id = m_nextId++;
    // Строковый id — идентификатор хранилища или подключения. Номер запроса тогда
    // в req_id: иначе служба примет число 356 за этот идентификатор и отвергнет команду.
    if (request.value(QStringLiteral("id")).isString())
        request.insert(QStringLiteral("req_id"), id);
    else
        request.insert(QStringLiteral("id"), id);
    if (cb) {
        if (guard) {
            // Не подписываемся на destroyed: этот сигнал в Qt иногда снимает колбэк
            // раньше ответа, и список навсегда остаётся на «загрузка…».
            QPointer<QObject> alive(guard);
            m_pending.insert(id, [alive, cb = std::move(cb)](bool ok, const QJsonValue &r, const QString &err) {
                if (alive) cb(ok, r, err);
            });
        } else {
            m_pending.insert(id, std::move(cb));
        }
    }
    QByteArray line = QJsonDocument(request).toJson(QJsonDocument::Compact);
    line.append('\n');
    if (isConnected()) {
        m_socket.write(line);
    } else {
        m_outbox.append(line);
        ensureConnected();
    }
}

void ServiceClient::onReadyRead() {
    m_buffer.append(m_socket.readAll());
    int nl;
    while ((nl = m_buffer.indexOf('\n')) >= 0) {
        const QByteArray line = m_buffer.left(nl);
        m_buffer.remove(0, nl + 1);
        const QJsonObject reply = QJsonDocument::fromJson(line).object();
        const qint64 id = reply.value(QStringLiteral("id")).toInteger();
        auto cb = m_pending.take(id);
        if (!cb)
            continue;
        // Колбэк вызывается после выхода из обработчика readyRead. Колбэки открывают модальные
        // окна (exec): если окно открыто прямо отсюда, QLocalSocket не шлёт readyRead повторно,
        // пока этот обработчик не вернётся, и окно не получает ни одного ответа службы
        // (список подключений, проверка подключения).
        QMetaObject::invokeMethod(this, [cb = std::move(cb), reply] {
            if (reply.value(QStringLiteral("ok")).toBool())
                cb(true, reply.value(QStringLiteral("result")), {});
            else
                cb(false, {}, reply.value(QStringLiteral("error")).toString());
        }, Qt::QueuedConnection);
    }
}

void ServiceClient::failAll(const QString &error) {
    m_outbox.clear();
    m_buffer.clear();
    const auto pending = std::exchange(m_pending, {});
    for (const auto &cb : pending)
        cb(false, {}, error);
}
