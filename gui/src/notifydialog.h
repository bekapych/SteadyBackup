#pragma once

#include <QDialog>
#include <QJsonObject>

class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QSpinBox;
class PasswordEdit;
class ServiceClient;

// Настройки уведомлений службы: e-mail и Telegram.
class NotifyDialog : public QDialog {
    Q_OBJECT
public:
    explicit NotifyDialog(ServiceClient *client, QWidget *parent = nullptr);

protected:
    void accept() override;

private:
    void load(const QJsonObject &s);
    QJsonObject value() const;
    void test(const QString &channel, QLabel *result);
    void findChats();

    ServiceClient *m_client;
    QLineEdit *m_computer;

    QCheckBox *m_mailOn;
    QComboBox *m_mailWhen;
    QLineEdit *m_mailServer, *m_mailUser, *m_mailFrom, *m_mailTo;
    QSpinBox *m_mailPort;
    QComboBox *m_mailSecurity;
    PasswordEdit *m_mailPassword;
    QLabel *m_mailResult;

    QCheckBox *m_tgOn;
    QComboBox *m_tgWhen;
    PasswordEdit *m_tgToken;
    QLineEdit *m_tgChat;
    QLabel *m_tgResult;
};
