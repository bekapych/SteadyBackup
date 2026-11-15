#pragma once

#include <QDialog>
#include <QJsonObject>

class QCheckBox;
class QLabel;
class QPushButton;
class QTextBrowser;
class QTimer;
class ServiceClient;

// «Обновление программы»: текущая и последняя версии, проверка, установка,
// настройки «проверять автоматически» и «устанавливать автоматически».
class UpdateDialog : public QDialog {
    Q_OBJECT
public:
    UpdateDialog(ServiceClient *client, bool admin, QWidget *parent = nullptr);

private:
    void apply(const QJsonObject &st);
    void reload();
    void check();
    void install();
    void saveSettings();

    ServiceClient *m_client;
    bool m_admin;
    QLabel *m_current, *m_latest, *m_state, *m_last;
    QTextBrowser *m_notes;
    QCheckBox *m_check, *m_auto;
    QPushButton *m_checkButton, *m_installButton;
    QTimer *m_poll;
    QJsonObject m_status;
    bool m_loading = false;
};
