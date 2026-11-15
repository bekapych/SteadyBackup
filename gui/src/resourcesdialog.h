#pragma once

#include <QDialog>
#include <QJsonArray>
#include <QJsonObject>

class QLabel;
class QLineEdit;
class QComboBox;
class QStackedWidget;
class QTableWidget;
class QPushButton;
class PasswordEdit;
class ServiceClient;

// Раздел «Хранилища и подключения»: список + добавление/изменение/удаление/проверка.
class ResourcesDialog : public QDialog {
    Q_OBJECT
public:
    // tab: 0 — хранилища, 1 — подключения.
    explicit ResourcesDialog(ServiceClient *client, QWidget *parent = nullptr, int tab = 0);

private:
    void refresh();
    void editStorage(const QJsonObject &s);
    void editConnection(const QJsonObject &c);
    void deleteSelected(QTableWidget *table, const QJsonArray &data, const QString &cmd);
    void migrate();

    ServiceClient *m_client;
    QTableWidget *m_storages;
    QTableWidget *m_connections;
    QJsonArray m_storageData;
    QJsonArray m_connectionData;
};

// Редактор хранилища: тип, параметры, вход в Google, проверка.
class StorageDialog : public QDialog {
    Q_OBJECT
public:
    StorageDialog(ServiceClient *client, const QJsonObject &storage, QWidget *parent = nullptr);
    QJsonObject value() const;

protected:
    void accept() override;

private:
    void test();
    void googleSignIn();

    ServiceClient *m_client;
    QJsonObject m_original;
    QJsonObject m_target;
    QLineEdit *m_name;
    class DestinationEditor *m_editor;
    QLabel *m_result;
};

// Редактор подключения: SQL Server, 1С, WebDAV, Firebird, PostgreSQL.
class ConnectionDialog : public QDialog {
    Q_OBJECT
public:
    ConnectionDialog(const QJsonObject &connection, QWidget *parent = nullptr);
    QJsonObject value() const;

protected:
    void accept() override;

private:
    QJsonObject m_original;
    QLineEdit *m_name;
    QComboBox *m_type;
    QLineEdit *m_server;
    QLineEdit *m_user;
    PasswordEdit *m_password;
    QLabel *m_serverLabel;
    class QSpinBox *m_port;
    QLabel *m_portLabel;
    // SSH: ключ, пароль ключа, отпечаток сервера.
    QLineEdit *m_keyFile, *m_hostKey;
    PasswordEdit *m_keyPass;
    QWidget *m_keyRow;
    QLabel *m_keyLabel, *m_keyPassLabel, *m_hostKeyLabel;
};
