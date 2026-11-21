#pragma once

#include <QDialog>
#include <QHash>
#include <QJsonArray>
#include <QJsonObject>

class QCheckBox;
class QComboBox;
class QDateTimeEdit;
class QGroupBox;
class QLabel;
class QLineEdit;
class QListWidget;
class QPlainTextEdit;
class QSpinBox;
class QStackedWidget;
class QTimeEdit;
class ServiceClient;
class PasswordEdit;
class RetryEditor;

class JobDialog : public QDialog {
    Q_OBJECT
public:
    JobDialog(ServiceClient *client, const QJsonObject &job, QWidget *parent = nullptr);
    QJsonObject value() const;
    // Снимки всех вкладок (режим --shots для документации и проверки).
    void saveShots(const QString &dir, const QString &prefix);

protected:
    void accept() override;

private:
    QWidget *buildSourcePage();
    QWidget *buildDestinationsPage();
    QWidget *buildArchivePage();
    QWidget *buildSchedulePage();
    void load(const QJsonObject &job);
    void refreshDestinations();
    QJsonArray storagesForDialog() const;
    // tab — вкладка с незаполненным полем (−1 — поле вне вкладок).
    QString validate(int *tab = nullptr) const;
    // Запрашивает у службы базы сервера (MS SQL или PostgreSQL) и этим проверяет подключение.
    // automatic — вызов при смене подключения: без выбранного подключения ничего не делает.
    void loadDatabases(const QString &type, bool automatic);
    // Список баз с отметками (MS SQL и PostgreSQL): отмеченные, которых нет на сервере, остаются.
    static void setDbList(QListWidget *list, const QStringList &checked, const QStringList &available);
    static QStringList dbChecked(const QListWidget *list);
    // Строка кнопок «Проверить и обновить», «Добавить вручную…», «Отметить все/снять».
    QWidget *dbButtons(QListWidget *list, const QString &type);
    QComboBox *makeArchiveMode();
    // «Файлы по SSH»: вход на сервер и наличие каталогов.
    void checkSshPaths();

    ServiceClient *m_client;
    QJsonObject m_original;
    QJsonArray m_destinations;
    QJsonArray m_storageList;
    QHash<QString, QString> m_storageNames; // id → название хранилища

    class QTabWidget *m_tabs;
    QLineEdit *m_name;
    QCheckBox *m_enabled;

    // источник
    QComboBox *m_srcType;
    QStackedWidget *m_srcPages;
    QPlainTextEdit *m_filesPaths;
    QLineEdit *m_filesInclude, *m_filesExclude;
    class ConnectionPicker *m_filesConn;
    QComboBox *m_filesKind;
    QSpinBox *m_filesFullEvery;
    QCheckBox *m_filesVss;
    QLineEdit *m_onecPath, *m_onecExe, *m_onecUnlock;
    ConnectionPicker *m_onecConn;
    QComboBox *m_onecMode;
    QListWidget *m_sqlDatabases; // отмеченные базы; ничего не отмечено — все пользовательские базы
    QComboBox *m_sqlArchives, *m_pgArchives; // данные: false — один архив, true — по архиву на базу
    QLineEdit *m_sqlDir;
    QLabel *m_sqlStatus;
    ConnectionPicker *m_sqlConn;
    QComboBox *m_sqlKind;
    QCheckBox *m_sqlVerify;
    QLineEdit *m_fbGbak, *m_fbNamesFrom, *m_fbTemplate, *m_fbHost;
    QPlainTextEdit *m_fbDatabases;
    ConnectionPicker *m_fbConn;
    QCheckBox *m_fbVerify;
    ConnectionPicker *m_pgConn;
    QListWidget *m_pgDatabases; // отмеченные базы; ничего не отмечено — все базы сервера
    QLabel *m_pgStatus;
    QLineEdit *m_pgDump;
    QCheckBox *m_pgGlobals, *m_pgVerify;
    ConnectionPicker *m_myConn, *m_sshConn; // подключения SSH
    QLineEdit *m_myHost, *m_myUser;
    QSpinBox *m_myPort; // 0 — порт по умолчанию
    PasswordEdit *m_myPassword;
    QListWidget *m_myDatabases;
    QComboBox *m_myArchives;
    QLabel *m_myStatus, *m_sshStatus;
    QPlainTextEdit *m_sshPaths;
    QLineEdit *m_sshInclude, *m_sshExclude;
    QComboBox *m_sshKind;
    QSpinBox *m_sshFullEvery;
    QComboBox *m_vmName;
    QComboBox *m_vmKind;
    QSpinBox *m_vmFullEvery;
    QComboBox *m_vmConsistency;

    // Общий для файловых источников фильтр даты.
    QGroupBox *m_dateBox;
    QComboBox *m_dateField, *m_dateMode;
    QSpinBox *m_dateHours;
    QDateTimeEdit *m_dateSince;

    // назначения
    QListWidget *m_destList;

    // архив
    QCheckBox *m_arcEnabled, *m_arcHeaders, *m_arcTest;
    QComboBox *m_arcMode;
    QSpinBox *m_arcLevel, *m_keepFull, *m_keepDays, *m_keepSets;
    PasswordEdit *m_arcPassword;
    QLineEdit *m_arcVolume;

    // расписание
    QComboBox *m_schedType;
    QStackedWidget *m_schedPages;
    QLineEdit *m_schedTimes;
    QList<QCheckBox *> m_weekDays;
    QLineEdit *m_monthDays;
    QSpinBox *m_intervalMin;
    QCheckBox *m_intervalWindow;
    QTimeEdit *m_intervalFrom, *m_intervalTo;
    QDateTimeEdit *m_onceAt;
    QCheckBox *m_runMissed;
    QSpinBox *m_timeoutMin;
    RetryEditor *m_retry;
    QComboBox *m_group; // данные пункта — id группы, пусто — без группы
    int m_dbSeq = 0; // ответы на устаревшие запросы списка баз игнорируются
};
