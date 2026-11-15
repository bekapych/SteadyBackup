#pragma once

#include <QHash>
#include <QJsonObject>
#include <QJsonValue>
#include <QLocalSocket>
#include <QObject>
#include <functional>

// Клиент службы SteadyBackup: построчный JSON по именованному каналу \\.\pipe\SteadyBackup.
class ServiceClient : public QObject {
    Q_OBJECT
public:
    using Callback = std::function<void(bool ok, const QJsonValue &result, const QString &error)>;

    explicit ServiceClient(QObject *parent = nullptr);
    ~ServiceClient() override;

    // guard — окно, которому принадлежит колбэк. Если его уже закрыли, ответ службы
    // не вызывается: иначе GUI падает вместо сообщения об ошибке.
    void call(QJsonObject request, Callback cb = {}, QObject *guard = nullptr);
    bool isConnected() const { return m_socket.state() == QLocalSocket::ConnectedState; }

Q_SIGNALS:
    void connectionChanged(bool connected);

private:
    void ensureConnected();
    void onReadyRead();
    void failAll(const QString &error);

    QLocalSocket m_socket;
    QByteArray m_buffer;
    QList<QByteArray> m_outbox;
    QHash<qint64, Callback> m_pending;
    qint64 m_nextId = 1;
    bool m_wasConnected = false;
};
