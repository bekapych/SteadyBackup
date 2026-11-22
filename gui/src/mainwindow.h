#pragma once

#include <QJsonArray>
#include <QJsonObject>
#include <QList>
#include <QMainWindow>
#include <QSystemTrayIcon>

class QAction;
class QCheckBox;
class QComboBox;
class QFrame;
class QLabel;
class QPushButton;
class QStackedWidget;
class QTableWidget;
class QTimer;
class QToolButton;
class ServiceClient;

class MainWindow : public QMainWindow {
    Q_OBJECT
public:
    explicit MainWindow(QWidget *parent = nullptr);
    // Снимает главное окно и диалоги в PNG и завершает программу.
    void takeShots(const QString &dir);

protected:
    void closeEvent(QCloseEvent *e) override;
    // Щелчок и клавиши ←/→/пробел по заголовку группы в списке заданий — свернуть/развернуть.
    bool eventFilter(QObject *obj, QEvent *e) override;

private:
    // Сводка над списком заданий: значок, значение, пояснение.
    struct Card {
        QLabel *icon = nullptr;
        QLabel *value = nullptr;
        QLabel *sub = nullptr;
    };
    enum CardId { CardJobs, CardHealth, CardNext, CardNow, CardCount };

    QWidget *buildHeader();
    QWidget *buildCards();
    QWidget *buildJobs();
    QWidget *buildHistory();
    void buildTray();
    // Значки зависят от темы: вызывается при запуске и при смене темы Windows.
    void applyIcons();

    void refresh();
    void applyJobs(const QJsonArray &jobs);
    void applyHistory(const QJsonArray &history);
    void applyStatus(const QJsonObject &status);
    void serviceDown(const QString &error);
    void renderHistory();
    void updateCards();
    void updateBanner();
    void updateActions();
    void updateTray();
    void setCard(CardId id, const QString &iconName, int state, const QString &value, const QString &sub);
    void showHistoryDetails(int row);
    QString selectedJobId() const;
    // Выбрана строка группы (заголовок в списке заданий).
    QString selectedGroupId() const;
    QJsonObject groupById(const QString &id) const;
    // Ключ раздела в строке-заголовке (id группы или "__ungrouped"); пусто — строка задания.
    QString sectionAt(int row) const;
    void setSectionCollapsed(const QString &key, bool collapsed);
    void setAllCollapsed(bool collapsed);
    void newGroup();
    void editGroup(const QJsonObject &group);
    void newJob();
    void editJob();
    void copyJob();
    void jobMenu(const QPoint &pos);
    void exportSettings();
    void importSettings();
    void deleteJob();
    void runJob();
    void cancelJob();
    void clearHistory();
    void saveJob(const QJsonObject &job);
    void showError(const QString &text);
    // Права администратора нужны для изменений; предлагает перезапуск с повышением (UAC).
    bool requireAdmin();
    void restartElevated();
    void editWorkDir();
    // Окно «Обновление программы».
    void showUpdates();
    void notifyFinished(const QJsonArray &history);

    ServiceClient *m_client;
    QTableWidget *m_jobs;
    QTableWidget *m_history;
    QStackedWidget *m_jobsStack;
    QLabel *m_logo;
    QLabel *m_statePill;
    QFrame *m_banner;
    QLabel *m_bannerIcon;
    QLabel *m_bannerText;
    QPushButton *m_bannerButton;
    QPushButton *m_newButton;
    QPushButton *m_emptyButton;
    QLabel *m_emptyIcon;
    QToolButton *m_settingsButton;
    QToolButton *m_refreshButton;
    QPushButton *m_updateButton; // «Доступна версия …» в заголовке
    QString m_updateNotified;    // версия, о которой уже сообщили в трее
    QComboBox *m_historyFilter;
    QCheckBox *m_historyOnlyJob = nullptr;
    Card m_cards[CardCount];
    QTimer *m_timer;
    QSystemTrayIcon *m_tray;
    QAction *m_actEdit, *m_actCopy, *m_actDelete, *m_actRun, *m_actCancel, *m_actNewGroup;
    QString m_runningId;
    QJsonArray m_jobsData;
    QJsonArray m_groupsData;
    QStringList m_collapsed; // свёрнутые разделы списка заданий (ключи разделов)
    QJsonArray m_historyData;
    QList<QJsonObject> m_historyRows; // строки журнала после фильтра, в порядке показа
    QJsonObject m_statusData;
    QString m_serviceError;
    QString m_lastHistoryKey;
    int m_tick = 0;
    bool m_serviceOk = false;
    bool m_anyFailed = false;
    bool m_historyLoaded = false;
    bool m_admin = false;
    bool m_quitting = false;
    bool m_trayHintShown = false;
};
