#pragma once

#include <QDialog>
#include <QJsonArray>
#include <QJsonObject>
#include <QWidget>

class QComboBox;
class QLineEdit;
class QSpinBox;
class QStackedWidget;
class QLabel;
class QPushButton;
class ServiceClient;
class PasswordEdit;

// Поле пароля, которое не показывает сохранённое (зашифрованное) значение.
// Пустое поле = оставить прежний пароль; кнопка «×» очищает пароль.
class PasswordEdit : public QWidget {
    Q_OBJECT
public:
    explicit PasswordEdit(QWidget *parent = nullptr);
    void setStored(const QString &value);
    QJsonValue value() const;

private:
    void updatePlaceholder();
    QLineEdit *m_edit;
    QString m_stored;
};

// Параметры подключения к хранилищу (корень): тип + поля. Используется в редакторе хранилища.
class DestinationEditor : public QWidget {
    Q_OBJECT
public:
    explicit DestinationEditor(QWidget *parent = nullptr);
    void load(const QJsonObject &d);
    QJsonObject value() const;
    QString type() const;
    // Google Drive: результат входа (client_id, client_secret, refresh_token, email — секреты в DPAPI).
    void setGoogleAccount(const QJsonObject &acc);
    QString googleClientId() const;
    QString googleClientSecret() const;
    void setHostKey(const QString &fingerprint);
    bool hostKeyEmpty() const;

Q_SIGNALS:
    void googleSignInRequested();

private:
    void updateGoogleState();

    QComboBox *m_type;
    QStackedWidget *m_pages;
    QLineEdit *m_localPath;
    QLineEdit *m_smbPath, *m_smbUser;
    PasswordEdit *m_smbPassword;
    QLineEdit *m_sftpHost, *m_sftpUser, *m_sftpKey, *m_sftpHostKey, *m_sftpPath, *m_sftpPost;
    QSpinBox *m_sftpPort;
    PasswordEdit *m_sftpPassword, *m_sftpKeyPass;
    QLineEdit *m_davUrl, *m_davUser;
    PasswordEdit *m_davPassword;
    class QCheckBox *m_gdCustom;
    QLineEdit *m_gdClientId, *m_gdFolder;
    PasswordEdit *m_gdClientSecret;
    QLabel *m_gdState;
    QPushButton *m_gdSignIn;
    QJsonObject m_gdAccount; // client_secret / refresh_token (DPAPI или "dpapi:stored"), email
};

// Назначение в задании: хранилище из раздела «Хранилища» + подпапка.
class DestinationDialog : public QDialog {
    Q_OBJECT
public:
    DestinationDialog(ServiceClient *client, const QJsonObject &dest, QWidget *parent = nullptr);
    // Уже загруженный список, чтобы поле не было пустым, пока служба отвечает ещё раз.
    void prime(const QJsonArray &storages, const QString &error = {});
    QJsonObject value() const;

protected:
    void accept() override;

private:
    void reloadStorages(const QString &select);
    void applyStorages(const QJsonArray &list, const QString &select, const QString &err);
    bool comboHasStorage() const;
    void test();

    ServiceClient *m_client;
    QJsonObject m_original;
    class SteadyCombo *m_storage;
    QLineEdit *m_folder;
    QLabel *m_info;
    QLabel *m_testResult;
    QJsonArray m_storages;
};
