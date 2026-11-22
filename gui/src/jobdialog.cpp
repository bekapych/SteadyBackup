#include "jobdialog.h"

#include "connectionpicker.h"
#include "destinationdialog.h"
#include "groupdialog.h"
#include "serviceclient.h"
#include "theme.h"
#include "util.h"

#include <QApplication>
#include <QCheckBox>
#include <QComboBox>
#include <QDateTimeEdit>
#include <QDialogButtonBox>
#include <QFileDialog>
#include <QFormLayout>
#include <QGroupBox>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QInputDialog>
#include <QLabel>
#include <QLineEdit>
#include <QListWidget>
#include <QMessageBox>
#include <QPlainTextEdit>
#include <QPushButton>
#include <QRegularExpression>
#include <QSignalBlocker>
#include <QSpinBox>
#include <QStackedWidget>
#include <QTabWidget>
#include <QTableWidget>
#include <QTimeEdit>
#include <QUuid>
#include <QVBoxLayout>

static const char *kDays[] = {"Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"};

// Значения по умолчанию источника one_c_download повторяют формат Autoupdate:
// служба получает те же проекты и дистрибутивы, что и прежняя утилита.
static QStringList defaultDlProjects() {
    return {QStringLiteral("Platform83"), QStringLiteral("Platform85")};
}

static QStringList defaultDlDistributions() {
    return {QStringLiteral("Технологическая платформа 1С:Предприятия (64-bit) для Windows"),
            QStringLiteral("Технологическая платформа 1С:Предприятия для Windows"),
            QStringLiteral("Тонкий клиент 1С:Предприятия для Windows")};
}

// Учётная запись ИТС: имя, логин, пароль. Сохранённый пароль возвращается маркером dpapi:stored,
// если поле не трогали: служба по маркеру понимает, что пароль менять не нужно.
static bool itsAccountDialog(QWidget *parent, QJsonObject &account) {
    QDialog dlg(parent);
    dlg.setWindowTitle(QObject::tr("Учётная запись ИТС"));
    auto *name = new QLineEdit(account.value("name").toString());
    name->setPlaceholderText(QObject::tr("напр. основная"));
    auto *user = new QLineEdit(account.value("username").toString());
    user->setPlaceholderText(QObject::tr("логин на releases.1c.ru"));
    auto *password = new PasswordEdit;
    password->setStored(account.value("password").toString());
    auto *f = new QFormLayout;
    f->addRow(QObject::tr("Имя:"), name);
    f->addRow(QObject::tr("Логин:"), user);
    f->addRow(QObject::tr("Пароль:"), password);
    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(QObject::tr("Сохранить"));
    buttons->button(QDialogButtonBox::Cancel)->setText(QObject::tr("Отмена"));
    QObject::connect(buttons, &QDialogButtonBox::accepted, &dlg, &QDialog::accept);
    QObject::connect(buttons, &QDialogButtonBox::rejected, &dlg, &QDialog::reject);
    auto *v = new QVBoxLayout(&dlg);
    v->addLayout(f);
    v->addWidget(buttons);
    util::keepButtonsOnScreen(&dlg);
    if (dlg.exec() != QDialog::Accepted) return false;
    const QJsonValue loadedPassword = account.value("password");
    account["name"] = name->text().trimmed();
    account["username"] = user->text().trimmed();
    const QJsonValue p = password->value();
    // Маркер dpapi:stored означает «пароль не менялся»: возвращаем значение из задания как есть,
    // чтобы служба подставила сохранённый пароль, а заглушка не попала в файл.
    if (p.toString() == QLatin1String("dpapi:stored") || p == loadedPassword) {
        if (loadedPassword.isString() && !loadedPassword.toString().isEmpty())
            account["password"] = loadedPassword;
        else
            account.remove(QStringLiteral("password"));
    } else if (p.isNull()) {
        account.remove(QStringLiteral("password")); // «×» — пароль удалён
    } else {
        account["password"] = p;
    }
    return true;
}

// Текст строки таблицы конфигураций (ячейка может быть ещё не создана).
static QString tableCell(const QTableWidget *t, int row, int col) {
    const QTableWidgetItem *it = t->item(row, col);
    return it ? it->text().trimmed() : QString();
}

// Роли ячейки «Конфигурация» в окне выбора из ИТС: текста с пометкой «уже в задании» мало,
// в задание нужно писать исходные значения с портала.
enum ItsCellRole {
    ProgramRole = Qt::UserRole, // program_name (в том виде, в каком его ждёт служба)
    HumanNameRole = Qt::UserRole + 1,
    VersionRole = Qt::UserRole + 2,
};

// Выбор конфигураций, доступных учётной записи ИТС. Список берётся у службы
// (list_its_configurations), запрос асинхронный: окно задания не ждёт сеть.
// Пароль учётки уходит в запрос как есть: сохранённый в задании (dpapi:stored) служба
// отклонит понятной ошибкой «введите пароль учётной записи ИТС» — её показываем в диалоге.
class ItsConfigurationsDialog : public QDialog {
public:
    ItsConfigurationsDialog(ServiceClient *client, const QJsonArray &accounts, const QStringList &present,
                            const QJsonObject &proxy, bool insecureTls, bool proxyPasswordHidden,
                            QWidget *parent = nullptr);

    // Отмеченные строки в формате задания: human_name, program_name, start_version, its_account.
    QJsonArray selected() const;

private:
    void fetch();
    void fill(const QJsonArray &list);
    void filterRows(const QString &text);
    void updateState();
    int currentAccountIndex() const;
    QString currentAccountName() const;

    ServiceClient *m_client;
    QJsonArray m_accounts;
    QStringList m_present; // program_name уже добавленных в задание
    QJsonObject m_proxy;
    bool m_insecure = false;
    QComboBox *m_account = nullptr;
    QPushButton *m_fetch = nullptr;
    QLineEdit *m_filter = nullptr;
    QTableWidget *m_table = nullptr;
    QLabel *m_status = nullptr;
    QPushButton *m_add = nullptr;
    bool m_loading = false;
    int m_seq = 0; // ответы на устаревшие запросы игнорируются
};

ItsConfigurationsDialog::ItsConfigurationsDialog(ServiceClient *client, const QJsonArray &accounts,
                                                 const QStringList &present, const QJsonObject &proxy,
                                                 bool insecureTls, bool proxyPasswordHidden, QWidget *parent)
    : QDialog(parent), m_client(client), m_accounts(accounts), m_present(present), m_proxy(proxy),
      m_insecure(insecureTls) {
    setWindowTitle(QObject::tr("Конфигурации учётной записи ИТС"));
    resize(880, 580);

    m_account = new SteadyCombo;
    for (int i = 0; i < m_accounts.size(); ++i) {
        const QJsonObject a = m_accounts.at(i).toObject();
        QString text = a.value("name").toString().trimmed();
        if (text.isEmpty()) text = QObject::tr("без имени");
        const QString user = a.value("username").toString().trimmed();
        text += QStringLiteral(" — ") + (user.isEmpty() ? QObject::tr("без логина") : user);
        m_account->addItem(text, i);
    }
    m_account->setToolTip(QObject::tr("Учётные записи ИТС из задания. Пароль тоже берётся из задания."));
    m_fetch = new QPushButton(QObject::tr("Получить список"));
    m_fetch->setToolTip(QObject::tr("Войти на releases.1c.ru и получить конфигурации, доступные этой учётке."));
    connect(m_fetch, &QPushButton::clicked, this, [this] { fetch(); });
    // Список относится к своей учётке: при смене учётки прежние строки не показываем.
    connect(m_account, &QComboBox::currentIndexChanged, this, [this](int) {
        ++m_seq;
        m_loading = false;
        m_table->setRowCount(0);
        m_status->setText(QObject::tr("Нажмите «Получить список»: при открытии окна список не запрашивается."));
        theme::setState(m_status, theme::Muted);
        updateState();
    });

    m_filter = new QLineEdit;
    m_filter->setPlaceholderText(QObject::tr("по названию или группе"));
    m_filter->setClearButtonEnabled(true);
    connect(m_filter, &QLineEdit::textChanged, this, [this](const QString &t) { filterRows(t); });

    m_table = new QTableWidget(0, 3);
    m_table->setHorizontalHeaderLabels(
        {QObject::tr("Группа"), QObject::tr("Конфигурация"), QObject::tr("Последняя версия")});
    m_table->setSelectionBehavior(QAbstractItemView::SelectRows);
    m_table->setShowGrid(false);
    m_table->verticalHeader()->hide();
    m_table->horizontalHeader()->setHighlightSections(false);
    m_table->horizontalHeader()->setDefaultAlignment(Qt::AlignLeft | Qt::AlignVCenter);
    // Название занимает свободное место, служебные колонки — по содержимому:
    // иначе пометка «уже в задании» обрезается многоточием.
    m_table->horizontalHeader()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
    m_table->horizontalHeader()->setSectionResizeMode(1, QHeaderView::Stretch);
    m_table->horizontalHeader()->setSectionResizeMode(2, QHeaderView::ResizeToContents);
    m_table->horizontalHeaderItem(2)->setToolTip(QObject::tr("Версия на портале — не версия вашей базы."));
    m_table->setToolTip(QObject::tr("Отметьте нужные конфигурации, затем «Добавить отмеченные»."));
    connect(m_table, &QTableWidget::itemChanged, this, [this](QTableWidgetItem *) { updateState(); });

    m_status = new QLabel(QObject::tr("Нажмите «Получить список»: при открытии окна список не запрашивается."));
    m_status->setWordWrap(true);
    m_status->setTextInteractionFlags(Qt::TextSelectableByMouse);
    theme::setState(m_status, theme::Muted);

    auto *hint = new QLabel(QObject::tr(
        "«Последняя версия» — то, что сейчас опубликовано на портале. В задание она попадает как начальная "
        "(start_version), поэтому после добавления замените её на текущую версию вашей базы: иначе "
        "обновление не будет найдено. Строки, которые уже есть в задании, отмечены и повторно не добавятся. "
        "Прокси и проверка TLS-сертификата берутся из формы задания."));
    hint->setWordWrap(true);
    theme::markHint(hint);
    QLabel *proxyHint = nullptr;
    if (proxyPasswordHidden) {
        proxyHint = new QLabel(QObject::tr(
            "Пароль прокси хранится в задании и форме недоступен: если прокси требует вход, запрос уйдёт "
            "без пароля — введите его в поле «Прокси» формы задания и повторите."));
        proxyHint->setWordWrap(true);
        theme::setState(proxyHint, theme::Warn);
    }

    m_add = new QPushButton(QObject::tr("Добавить отмеченные"));
    m_add->setObjectName(QStringLiteral("primary"));
    m_add->setToolTip(QObject::tr("Добавить отмеченные конфигурации в задание."));
    auto *cancel = new QPushButton(QObject::tr("Отмена"));
    connect(m_add, &QPushButton::clicked, this, &QDialog::accept);
    connect(cancel, &QPushButton::clicked, this, &QDialog::reject);

    auto *top = new QHBoxLayout;
    top->addWidget(new QLabel(QObject::tr("Учётная запись:")));
    top->addWidget(m_account, 1);
    top->addWidget(m_fetch);
    auto *filterRow = new QHBoxLayout;
    filterRow->addWidget(new QLabel(QObject::tr("Фильтр:")));
    filterRow->addWidget(m_filter, 1);
    auto *buttons = new QHBoxLayout;
    buttons->addStretch();
    buttons->addWidget(m_add);
    buttons->addWidget(cancel);

    auto *v = new QVBoxLayout(this);
    v->addLayout(top);
    v->addLayout(filterRow);
    v->addWidget(m_table, 1);
    v->addWidget(hint);
    if (proxyHint) v->addWidget(proxyHint);
    v->addWidget(m_status);
    v->addLayout(buttons);
    util::keepButtonsOnScreen(this);
    updateState();
}

int ItsConfigurationsDialog::currentAccountIndex() const {
    // Данные пункта — номер учётки в m_accounts; пустой список даёт 0 при size 0.
    const int i = m_account->currentData().toInt();
    return i >= 0 && i < m_accounts.size() ? i : -1;
}

QString ItsConfigurationsDialog::currentAccountName() const {
    const int i = currentAccountIndex();
    return i < 0 ? QString() : m_accounts.at(i).toObject().value("name").toString().trimmed();
}

void ItsConfigurationsDialog::fetch() {
    const int i = currentAccountIndex();
    if (i < 0) return;
    const QJsonObject account = m_accounts.at(i).toObject();
    const QString user = account.value("username").toString().trimmed();
    if (user.isEmpty()) {
        m_status->setText(QObject::tr("У выбранной учётной записи нет логина: добавьте его в списке "
                                      "учётных записей ИТС."));
        theme::setState(m_status, theme::Warn);
        return;
    }
    const QString password = account.value("password").toString();
    if (password.isEmpty()) {
        m_status->setText(QObject::tr("У выбранной учётной записи нет пароля: введите его в списке "
                                      "учётных записей ИТС."));
        theme::setState(m_status, theme::Warn);
        return;
    }
    QJsonObject request{
        {QStringLiteral("cmd"), QStringLiteral("list_its_configurations")},
        {QStringLiteral("username"), user},
        // Маркер dpapi:stored уходит в службу как есть: она ответит «введите пароль учётной записи ИТС».
        {QStringLiteral("password"), password},
        {QStringLiteral("insecure_tls"), m_insecure},
    };
    if (m_proxy.isEmpty())
        request[QStringLiteral("proxy")] = QJsonValue(QJsonValue::Null);
    else
        request[QStringLiteral("proxy")] = m_proxy;
    const int seq = ++m_seq;
    m_loading = true;
    m_status->setText(QObject::tr("Запрос списка конфигураций на releases.1c.ru…"));
    theme::setState(m_status, theme::Muted);
    updateState();
    m_client->call(request, [this, seq](bool ok, const QJsonValue &r, const QString &err) {
        if (seq != m_seq) return; // учётку успели сменить — ответ не нужен
        m_loading = false;
        if (!ok) {
            m_table->setRowCount(0);
            m_status->setText(QObject::tr("Ошибка: %1").arg(err));
            theme::setState(m_status, theme::Error);
            updateState();
            return;
        }
        const QJsonArray list = r.toArray();
        fill(list);
        if (list.isEmpty()) {
            m_status->setText(QObject::tr("Служба вернула пустой список конфигураций."));
            theme::setState(m_status, theme::Warn);
        } else {
            m_status->setText(QObject::tr("Конфигураций: %1. Отметьте нужные и нажмите «Добавить отмеченные».")
                                  .arg(list.size()));
            theme::setState(m_status, theme::Ok);
        }
        updateState();
    }, this);
}

void ItsConfigurationsDialog::fill(const QJsonArray &list) {
    // Отметки ставим до показа строк: onChanged не должен пересчитывать кнопки на каждой строке.
    const QSignalBlocker block(m_table);
    m_table->setRowCount(list.size());
    for (int r = 0; r < list.size(); ++r) {
        const QJsonObject c = list.at(r).toObject();
        const QString program = c.value("program_name").toString().trimmed();
        const QString human = c.value("name").toString().trimmed();
        const QString version = c.value("version").toString().trimmed();
        const QString nick = c.value("nick").toString().trimmed();
        auto *groupItem = new QTableWidgetItem(c.value("group").toString().trimmed());
        auto *nameItem = new QTableWidgetItem(human.isEmpty() ? program : human);
        auto *versionItem = new QTableWidgetItem(version);
        nameItem->setData(ProgramRole, program);
        nameItem->setData(HumanNameRole, human);
        nameItem->setData(VersionRole, version);
        QString tip = program.isEmpty() ? nick : QObject::tr("program_name: %1").arg(program);
        if (!nick.isEmpty() && !program.isEmpty()) tip += QStringLiteral(" · ") + nick;
        if (m_present.contains(program)) {
            // Уже в задании: строка видна и отмечена, но повторно не добавляется.
            nameItem->setCheckState(Qt::Checked);
            nameItem->setFlags(nameItem->flags() & ~Qt::ItemIsUserCheckable);
            nameItem->setText(nameItem->text() + QObject::tr("  ·  уже в задании"));
            nameItem->setToolTip(QObject::tr("Эта конфигурация уже есть в задании."));
        } else {
            nameItem->setFlags(nameItem->flags() | Qt::ItemIsUserCheckable);
            nameItem->setCheckState(Qt::Unchecked);
            nameItem->setToolTip(QObject::tr("Отметьте, чтобы добавить в задание (%1).").arg(tip));
        }
        groupItem->setToolTip(QObject::tr("Группа на портале ИТС (условный раздел программы)."));
        versionItem->setToolTip(QObject::tr("Опубликованная версия, а не версия вашей базы: после добавления "
                                            "замените её на текущую версию базы."));
        m_table->setItem(r, 0, groupItem);
        m_table->setItem(r, 1, nameItem);
        m_table->setItem(r, 2, versionItem);
    }
}

void ItsConfigurationsDialog::filterRows(const QString &text) {
    const QString needle = text.trimmed().toLower();
    for (int r = 0; r < m_table->rowCount(); ++r) {
        bool match = needle.isEmpty();
        for (int c = 0; c < 2 && !match; ++c) { // группа и название
            const QTableWidgetItem *it = m_table->item(r, c);
            if (it && it->text().toLower().contains(needle)) match = true;
        }
        m_table->setRowHidden(r, !match);
    }
    updateState();
}

void ItsConfigurationsDialog::updateState() {
    m_fetch->setEnabled(!m_loading && currentAccountIndex() >= 0);
    m_account->setEnabled(!m_loading);
    m_filter->setEnabled(!m_loading);
    bool anyChecked = false;
    if (!m_loading) {
        for (int r = 0; r < m_table->rowCount() && !anyChecked; ++r) {
            const QTableWidgetItem *it = m_table->item(r, 1);
            // Строки, которые уже есть в задании, отмечены, но добавлять их нечего.
            anyChecked = it && it->checkState() == Qt::Checked && (it->flags() & Qt::ItemIsUserCheckable) &&
                         !m_table->isRowHidden(r);
        }
    }
    m_add->setEnabled(anyChecked);
}

QJsonArray ItsConfigurationsDialog::selected() const {
    QJsonArray out;
    const QString account = currentAccountName();
    for (int r = 0; r < m_table->rowCount(); ++r) {
        const QTableWidgetItem *it = m_table->item(r, 1);
        if (!it || it->checkState() != Qt::Checked || !(it->flags() & Qt::ItemIsUserCheckable)) continue;
        const QString program = it->data(ProgramRole).toString().trimmed();
        if (program.isEmpty()) continue;
        out.append(QJsonObject{
            {QStringLiteral("human_name"), it->data(HumanNameRole).toString()},
            {QStringLiteral("program_name"), program},
            {QStringLiteral("start_version"), it->data(VersionRole).toString()},
            {QStringLiteral("its_account"), account},
        });
    }
    return out;
}

JobDialog::JobDialog(ServiceClient *client, const QJsonObject &job, QWidget *parent)
    : QDialog(parent), m_client(client), m_original(job) {
    setWindowTitle(job.isEmpty() ? tr("Новое задание") : tr("Задание: %1").arg(job.value("name").toString()));
    resize(720, 600);

    m_name = new QLineEdit;
    m_enabled = new QCheckBox(tr("Включено"));
    m_enabled->setChecked(true);

    auto *head = new QHBoxLayout;
    head->addWidget(new QLabel(tr("Имя:")));
    head->addWidget(m_name, 1);
    head->addWidget(m_enabled);

    auto *tabs = m_tabs = new QTabWidget;
    tabs->addTab(buildSourcePage(), tr("Источник"));
    tabs->addTab(buildDestinationsPage(), tr("Назначения"));
    tabs->addTab(buildArchivePage(), tr("Архив и хранение"));
    tabs->addTab(buildSchedulePage(), tr("Расписание"));
    const auto updateSourceTabs = [this] {
        const QString source = m_srcType->currentData().toString();
        // У этих источников нет назначений и архива: они сами раскладывают файлы по папкам.
        const bool local = source == QLatin1String("one_c_update") || source == QLatin1String("one_c_download");
        m_tabs->setTabEnabled(1, !local);
        m_tabs->setTabEnabled(2, !local);
    };
    connect(m_srcType, &QComboBox::currentIndexChanged, this, [updateSourceTabs](int) { updateSourceTabs(); });
    updateSourceTabs();

    auto *buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    buttons->button(QDialogButtonBox::Ok)->setText(tr("Сохранить"));
    buttons->button(QDialogButtonBox::Ok)->setObjectName(QStringLiteral("primary"));
    buttons->button(QDialogButtonBox::Cancel)->setText(tr("Отмена"));
    connect(buttons, &QDialogButtonBox::accepted, this, &QDialog::accept);
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);

    auto *v = new QVBoxLayout(this);
    v->addLayout(head);
    v->addWidget(tabs, 1);
    v->addWidget(buttons);

    load(job);
    // На низком разрешении форма выше экрана: кнопки остаются внизу, поля прокручиваются.
    util::keepButtonsOnScreen(this);
}

// ---------------- Источник ----------------

QWidget *JobDialog::buildSourcePage() {
    auto *page = new QWidget;
    m_srcType = new QComboBox;
    m_srcType->addItem(tr("Файлы и папки (локальные, сетевые, WebDAV)"), "files");
    m_srcType->addItem(tr("Файловая база 1С"), "one_c");
    m_srcType->addItem(tr("Обновление конфигурации 1С"), "one_c_update");
    m_srcType->addItem(tr("Загрузка обновлений 1С"), "one_c_download");
    m_srcType->addItem(tr("База MS SQL Server"), "ms_sql");
    m_srcType->addItem(tr("База Firebird"), "firebird");
    m_srcType->addItem(tr("Базы PostgreSQL"), "postgresql");
    m_srcType->addItem(tr("Базы MySQL / MariaDB по SSH (сайт)"), "mysql");
    m_srcType->addItem(tr("Файлы по SSH (сайт)"), "ssh_files");
    m_srcType->addItem(tr("Виртуальная машина Hyper-V"), "hyper_v");
    m_srcPages = new QStackedWidget;

    { // files
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_filesPaths = new QPlainTextEdit;
        m_filesPaths->setMinimumHeight(64);
        m_filesPaths->setMaximumHeight(140);
        m_filesPaths->setPlaceholderText(tr("По одному пути на строку:\nD:\\Документы\n\\\\server\\share\\Проекты\n"
                                            "https://cloud.example.ru/remote.php/dav/files/user/Work"));
        auto *add = new QPushButton(tr("Добавить папку…"));
        connect(add, &QPushButton::clicked, this, [this] {
            const QString p = QFileDialog::getExistingDirectory(this);
            if (!p.isEmpty()) m_filesPaths->appendPlainText(QDir::toNativeSeparators(p));
        });
        m_filesKind = new QComboBox;
        m_filesKind->addItem(tr("Полная"), "full");
        m_filesKind->addItem(tr("Инкрементная (изменения с прошлой копии)"), "incremental");
        m_filesKind->addItem(tr("Дифференциальная (изменения с последней полной)"), "differential");
        m_filesFullEvery = new QSpinBox;
        m_filesFullEvery->setRange(0, 1000);
        m_filesFullEvery->setSpecialValueText(tr("никогда"));
        m_filesFullEvery->setSuffix(tr(" копий"));
        // Для полной копии счётчик «полная каждые» не имеет смысла.
        connect(m_filesKind, &QComboBox::currentIndexChanged, this,
                [this] { m_filesFullEvery->setEnabled(m_filesKind->currentData().toString() != QLatin1String("full")); });
        m_filesFullEvery->setEnabled(false);
        m_filesInclude = new QLineEdit;
        m_filesInclude->setPlaceholderText(tr("все файлы; или напр. *.docx; *.xlsx"));
        m_filesExclude = new QLineEdit;
        m_filesExclude->setPlaceholderText(QStringLiteral("*.tmp; ~$*; **/node_modules/**"));
        m_filesConn = new ConnectionPicker(m_client, "web_dav", tr("без учётных данных"));
        m_filesVss = new QCheckBox(tr("Теневое копирование (VSS) для локальных дисков"));
        m_filesVss->setChecked(true);
        m_filesVss->setToolTip(tr("Позволяет копировать открытые файлы из снимка на один момент времени. "
                                  "Сетевые и WebDAV-пути читаются напрямую. Нужны права администратора."));
        f->addRow(tr("Пути:"), m_filesPaths);
        f->addRow(QString(), add);
        f->addRow(tr("Тип копии:"), m_filesKind);
        f->addRow(tr("Полная каждые:"), m_filesFullEvery);
        f->addRow(tr("Включать:"), m_filesInclude);
        f->addRow(tr("Исключать:"), m_filesExclude);
        f->addRow(tr("Вход WebDAV:"), m_filesConn);
        f->addRow(m_filesVss);
        m_srcPages->addWidget(w);
    }
    { // 1C
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_onecPath = new QLineEdit;
        m_onecPath->setPlaceholderText(QStringLiteral("C:\\1C\\Бухгалтерия"));
        auto *browse = new QPushButton(tr("Обзор…"));
        connect(browse, &QPushButton::clicked, this, [this] {
            const QString p = QFileDialog::getExistingDirectory(this, tr("Каталог базы (где лежит 1Cv8.1CD)"), m_onecPath->text());
            if (!p.isEmpty()) m_onecPath->setText(QDir::toNativeSeparators(p));
        });
        auto *pw = new QWidget;
        auto *pl = new QHBoxLayout(pw);
        pl->setContentsMargins(0, 0, 0, 0);
        pl->addWidget(m_onecPath);
        pl->addWidget(browse);
        m_onecMode = new QComboBox;
        m_onecMode->addItem(tr("Выгрузка .dt средствами платформы (рекомендуется)"), "dump_dt");
        m_onecMode->addItem(tr("Копия файла 1Cv8.1CD"), "copy_file");
        m_onecConn = new ConnectionPicker(m_client, "one_c", tr("без пароля (пользователь по умолчанию)"));
        m_onecExe = new QLineEdit;
        m_onecExe->setPlaceholderText(tr("автоматически"));
        m_client->call({{"cmd", "find1c"}}, [this](bool ok, const QJsonValue &r, const QString &) {
            if (ok && r.isString()) m_onecExe->setPlaceholderText(tr("автоматически: %1").arg(r.toString()));
        }, this);
        m_onecUnlock = new QLineEdit;
        m_onecUnlock->setPlaceholderText(tr("если включена блокировка сеансов"));
        f->addRow(tr("Каталог базы:"), pw);
        f->addRow(tr("Способ:"), m_onecMode);
        f->addRow(tr("Пользователь 1С:"), m_onecConn);
        f->addRow(tr("1cv8.exe:"), m_onecExe);
        f->addRow(tr("Код разрешения (/UC):"), m_onecUnlock);
        auto *hint = new QLabel(tr("Выгрузка .dt выполняется в режиме Конфигуратора. В это время в базе не должно быть "
                                   "активных пользователей, иначе выгрузка завершится ошибкой."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // Обновление конфигурации 1С
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        const auto directoryRow = [this](QLineEdit *edit, const QString &title) {
            auto *browse = new QPushButton(tr("Обзор…"));
            connect(browse, &QPushButton::clicked, this, [this, edit, title] {
                const QString p = QFileDialog::getExistingDirectory(this, title, edit->text());
                if (!p.isEmpty()) edit->setText(QDir::toNativeSeparators(p));
            });
            auto *row = new QWidget;
            auto *l = new QHBoxLayout(row);
            l->setContentsMargins(0, 0, 0, 0);
            l->addWidget(edit, 1);
            l->addWidget(browse);
            return row;
        };
        m_onecUpdatePath = new QLineEdit;
        m_onecUpdatePath->setPlaceholderText(QStringLiteral("C:\\1C\\Бухгалтерия  или  сервер\\имя"));
        m_onecUpdateUpdatesDir = new QLineEdit;
        m_onecUpdateUpdatesDir->setPlaceholderText(QStringLiteral("C:\\1C\\Updates"));
        m_onecUpdateBackupDir = new QLineEdit;
        m_onecUpdateBackupDir->setPlaceholderText(QStringLiteral("C:\\1C\\Backup"));
        f->addRow(tr("База 1С:"), directoryRow(m_onecUpdatePath, tr("Каталог файловой базы 1С")));
        f->addRow(tr("Папка обновлений:"), directoryRow(m_onecUpdateUpdatesDir, tr("Папка с обновлениями конфигурации")));
        f->addRow(tr("Папка для .dt:"), directoryRow(m_onecUpdateBackupDir, tr("Папка для резервной копии .dt")));

        m_onecUpdateExe = new QLineEdit;
        m_onecUpdateExe->setPlaceholderText(tr("автоматически"));
        m_client->call({{"cmd", "find1c"}}, [this](bool ok, const QJsonValue &r, const QString &) {
            if (ok && r.isString()) m_onecUpdateExe->setPlaceholderText(tr("автоматически: %1").arg(r.toString()));
        }, this);
        auto *exeBrowse = new QPushButton(tr("Обзор…"));
        connect(exeBrowse, &QPushButton::clicked, this, [this] {
            const QString p = QFileDialog::getOpenFileName(this, tr("1cv8.exe"), m_onecUpdateExe->text(), tr("Программы (*.exe)"));
            if (!p.isEmpty()) m_onecUpdateExe->setText(QDir::toNativeSeparators(p));
        });
        auto *exeRow = new QWidget;
        auto *exeLayout = new QHBoxLayout(exeRow);
        exeLayout->setContentsMargins(0, 0, 0, 0);
        exeLayout->addWidget(m_onecUpdateExe, 1);
        exeLayout->addWidget(exeBrowse);
        m_onecUpdateConn = new ConnectionPicker(m_client, "one_c", tr("без пароля (пользователь по умолчанию)"));
        m_onecUpdateUnlock = new QLineEdit;
        m_onecUpdateUnlock->setPlaceholderText(tr("если включена блокировка сеансов"));
        m_onecUpdateClusterUser = new QLineEdit;
        m_onecUpdateClusterPassword = new PasswordEdit;
        m_onecUpdateForce = new QCheckBox(tr("Принудительное обновление"));
        m_onecUpdateServerSide = new QCheckBox(tr("Реструктуризация на сервере 1С"));
        m_onecUpdateRunHandlers = new QCheckBox(tr("Выполнять обработчики обновления"));
        f->addRow(tr("Пользователь 1С:"), m_onecUpdateConn);
        f->addRow(tr("1cv8.exe:"), exeRow);
        f->addRow(tr("Код разрешения (/UC):"), m_onecUpdateUnlock);
        f->addRow(tr("Администратор кластера:"), m_onecUpdateClusterUser);
        f->addRow(tr("Пароль кластера:"), m_onecUpdateClusterPassword);
        auto *clusterHint = new QLabel(tr("Нужны только для серверных баз, если кластер требует отдельную аутентификацию администратора RAC."));
        clusterHint->setWordWrap(true);
        theme::markHint(clusterHint);
        f->addRow(clusterHint);
        f->addRow(m_onecUpdateForce);
        f->addRow(m_onecUpdateServerSide);
        f->addRow(m_onecUpdateRunHandlers);
        auto *hint = new QLabel(tr("Сначала проверяется наличие обновления. Если оно найдено, новые сеансы блокируются через COM, "
                                   "активные завершаются через RAC (серверная база) или завершением локальных процессов этой базы, "
                                   "резервная копия .dt сохраняется в указанную папку, затем обновляется конфигурация. "
                                   "Назначения и архивирование для этого типа задания не используются."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // Загрузка обновлений 1С
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        const auto directoryRow = [this](QLineEdit *edit, const QString &title) {
            auto *browse = new QPushButton(tr("Обзор…"));
            connect(browse, &QPushButton::clicked, this, [this, edit, title] {
                const QString p = QFileDialog::getExistingDirectory(this, title, edit->text());
                if (!p.isEmpty()) edit->setText(QDir::toNativeSeparators(p));
            });
            auto *row = new QWidget;
            auto *l = new QHBoxLayout(row);
            l->setContentsMargins(0, 0, 0, 0);
            l->addWidget(edit, 1);
            l->addWidget(browse);
            return row;
        };
        m_dlTemplateDir = new QLineEdit;
        m_dlTemplateDir->setPlaceholderText(QStringLiteral("C:\\1C\\Updates"));
        m_dlPlatformDir = new QLineEdit;
        m_dlPlatformDir->setPlaceholderText(QStringLiteral("C:\\1C\\Platform"));
        f->addRow(tr("Обновления конфигураций:"),
                  directoryRow(m_dlTemplateDir, tr("Папка для обновлений конфигураций")));
        f->addRow(tr("Платформа и дистрибутивы:"),
                  directoryRow(m_dlPlatformDir, tr("Папка для платформы и дистрибутивов")));
        m_dlTemplateDir->setToolTip(tr("Эту папку сможет использовать задание «Обновление конфигурации 1С»."));
        m_dlUnzip = new QCheckBox(tr("Распаковывать архивы"));
        m_dlInsecure = new QCheckBox(tr("Не проверять TLS-сертификат"));
        m_dlCheckOnly = new QCheckBox(tr("Только проверять, не скачивать"));
        f->addRow(m_dlUnzip);
        f->addRow(m_dlInsecure);
        f->addRow(m_dlCheckOnly);

        auto *proxy = new QGroupBox(tr("Прокси"));
        auto *pf = new QFormLayout(proxy);
        m_dlProxyHost = new QLineEdit;
        m_dlProxyHost->setPlaceholderText(tr("без прокси; напр. proxy.example.ru"));
        m_dlProxyPort = new QSpinBox;
        m_dlProxyPort->setRange(0, 65535);
        m_dlProxyPort->setSpecialValueText(tr("по умолчанию"));
        auto *hostRow = new QWidget;
        auto *hl = new QHBoxLayout(hostRow);
        hl->setContentsMargins(0, 0, 0, 0);
        hl->addWidget(m_dlProxyHost, 1);
        hl->addWidget(new QLabel(tr("порт:")));
        hl->addWidget(m_dlProxyPort);
        m_dlProxyUser = new QLineEdit;
        m_dlProxyUser->setPlaceholderText(tr("если прокси требует вход"));
        m_dlProxyPassword = new PasswordEdit;
        pf->addRow(tr("Сервер:"), hostRow);
        pf->addRow(tr("Пользователь:"), m_dlProxyUser);
        pf->addRow(tr("Пароль:"), m_dlProxyPassword);
        f->addRow(proxy);

        auto *its = new QGroupBox(tr("Учётные записи ИТС"));
        auto *il = new QHBoxLayout(its);
        m_dlAccounts = new QListWidget;
        m_dlAccounts->setMinimumHeight(80);
        m_dlAccounts->setMaximumHeight(140);
        m_dlAccounts->setToolTip(tr("Учётные записи для releases.1c.ru. Пароли на диске хранятся "
                                    "зашифрованными (DPAPI), в списке пароль не показывается."));
        auto *addAcc = new QPushButton(tr("Добавить…"));
        auto *editAcc = new QPushButton(tr("Изменить…"));
        auto *delAcc = new QPushButton(tr("Удалить"));
        const auto editAccount = [this](int row) {
            QJsonObject account = row >= 0 && row < m_dlAccountList.size() ? m_dlAccountList.at(row).toObject()
                                                                          : QJsonObject{};
            if (!itsAccountDialog(this, account)) return;
            if (row >= 0 && row < m_dlAccountList.size())
                m_dlAccountList.replace(row, account);
            else
                m_dlAccountList.append(account);
            refreshItsAccounts();
        };
        connect(addAcc, &QPushButton::clicked, this, [editAccount] { editAccount(-1); });
        connect(editAcc, &QPushButton::clicked, this, [this, editAccount] { editAccount(m_dlAccounts->currentRow()); });
        connect(m_dlAccounts, &QListWidget::itemDoubleClicked, this,
                [this, editAccount](QListWidgetItem *) { editAccount(m_dlAccounts->currentRow()); });
        connect(delAcc, &QPushButton::clicked, this, [this] {
            const int r = m_dlAccounts->currentRow();
            if (r < 0 || r >= m_dlAccountList.size()) return;
            m_dlAccountList.removeAt(r);
            refreshItsAccounts();
        });
        auto *accButtons = new QVBoxLayout;
        accButtons->addWidget(addAcc);
        accButtons->addWidget(editAcc);
        accButtons->addWidget(delAcc);
        accButtons->addStretch();
        il->addWidget(m_dlAccounts, 1);
        il->addLayout(accButtons);
        f->addRow(its);

        auto *platformBox = new QGroupBox(tr("Платформа 1С"));
        auto *plf = new QFormLayout(platformBox);
        m_dlPlatformVersion = new QLineEdit;
        m_dlPlatformVersion->setPlaceholderText(tr("напр. 8.3.24.1548 или пусто — установленная"));
        m_dlPlatformVersion->setToolTip(
            tr("Нужна сервису обновлений для проверки конфигураций. Пусто — берётся версия "
               "установленной платформы 1С."));
        m_dlPlatformAccount = new SteadyCombo;
        m_dlPlatformAccount->setToolTip(tr("Пусто — первая учётная запись ИТС из списка выше."));
        plf->addRow(tr("Начальная версия:"), m_dlPlatformVersion);
        plf->addRow(tr("Учётная запись:"), m_dlPlatformAccount);
        f->addRow(platformBox);

        auto *rel = new QGroupBox(tr("Выпуски и дистрибутивы с releases.1c.ru"));
        auto *rf = new QFormLayout(rel);
        m_dlReleasesEnabled = new QCheckBox(tr("Скачивать выпуски и дистрибутивы"));
        m_dlReleasesAccount = new SteadyCombo;
        m_dlReleasesAccount->setToolTip(tr("Пусто — первая учётная запись ИТС из списка выше."));
        m_dlReleasesCount = new QSpinBox;
        m_dlReleasesCount->setRange(1, 10);
        m_dlReleasesCount->setValue(2);
        m_dlReleasesCount->setSuffix(tr(" версий"));
        m_dlReleasesCount->setToolTip(tr("Сколько последних версий скачивать за один запуск."));
        m_dlReleasesProjects = new QLineEdit;
        m_dlReleasesProjects->setPlaceholderText(QStringLiteral("Platform83,Platform85"));
        m_dlDistributions = new QPlainTextEdit;
        m_dlDistributions->setMinimumHeight(64);
        m_dlDistributions->setMaximumHeight(120);
        m_dlDistributions->setPlaceholderText(tr("По одному названию дистрибутива на строку"));
        rf->addRow(m_dlReleasesEnabled);
        rf->addRow(tr("Учётная запись:"), m_dlReleasesAccount);
        rf->addRow(tr("Сколько последних версий:"), m_dlReleasesCount);
        rf->addRow(tr("Проекты:"), m_dlReleasesProjects);
        rf->addRow(tr("Дистрибутивы:"), m_dlDistributions);
        for (QWidget *x : std::initializer_list<QWidget *>{m_dlReleasesAccount, m_dlReleasesCount, m_dlReleasesProjects,
                                                           m_dlDistributions})
            connect(m_dlReleasesEnabled, &QCheckBox::toggled, x, &QWidget::setEnabled);
        m_dlReleasesEnabled->setChecked(true);
        f->addRow(rel);

        auto *cfg = new QGroupBox(tr("Конфигурации"));
        auto *cl = new QHBoxLayout(cfg);
        m_dlConfigurations = new QTableWidget(0, 4);
        m_dlConfigurations->setHorizontalHeaderLabels(
            {tr("Название"), tr("Программа"), tr("Начальная версия"), tr("Учётка")});
        m_dlConfigurations->setSelectionBehavior(QAbstractItemView::SelectRows);
        m_dlConfigurations->setShowGrid(false);
        m_dlConfigurations->verticalHeader()->hide();
        m_dlConfigurations->horizontalHeader()->setHighlightSections(false);
        m_dlConfigurations->horizontalHeader()->setDefaultAlignment(Qt::AlignLeft | Qt::AlignVCenter);
        m_dlConfigurations->horizontalHeader()->setSectionResizeMode(QHeaderView::Stretch);
        m_dlConfigurations->setToolTip(tr("Учётка — имя из списка учётных записей ИТС; пусто — первая учётная запись."));
        m_dlConfigurations->setMinimumHeight(110);
        auto *addCfg = new QPushButton(tr("Добавить"));
        m_dlItsPick = new QPushButton(tr("Выбрать из ИТС…"));
        auto *delCfg = new QPushButton(tr("Удалить"));
        const auto addConfigRow = [this] {
            const int r = m_dlConfigurations->rowCount();
            m_dlConfigurations->insertRow(r);
            for (int c = 0; c < 4; ++c) m_dlConfigurations->setItem(r, c, new QTableWidgetItem);
            m_dlConfigurations->setCurrentCell(r, 0);
            m_dlConfigurations->editItem(m_dlConfigurations->item(r, 0));
        };
        connect(addCfg, &QPushButton::clicked, this, addConfigRow);
        connect(m_dlItsPick, &QPushButton::clicked, this, &JobDialog::chooseItsConfigurations);
        connect(delCfg, &QPushButton::clicked, this, [this] {
            const int r = m_dlConfigurations->currentRow();
            if (r < 0) return;
            m_dlConfigurations->removeRow(r);
        });
        auto *cfgButtons = new QVBoxLayout;
        cfgButtons->addWidget(addCfg);
        cfgButtons->addWidget(m_dlItsPick);
        cfgButtons->addWidget(delCfg);
        cfgButtons->addStretch();
        cl->addWidget(m_dlConfigurations, 1);
        cl->addLayout(cfgButtons);
        f->addRow(cfg);

        auto *hint = new QLabel(tr("Нужна учётная запись ИТС. Задание скачивает обновления конфигураций в папку "
                                   "шаблонов (её сможет использовать задание «Обновление конфигурации 1С»), архив "
                                   "платформы и дистрибутивы с releases.1c.ru. Состояние «последняя скачанная "
                                   "версия» хранится службой, поэтому повторно те же файлы не скачиваются. "
                                   "Назначения и архивирование для этого типа задания не используются."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        refreshItsAccounts();
        m_srcPages->addWidget(w);
    }
    { // MSSQL
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_sqlConn = new ConnectionPicker(m_client, "ms_sql", QString());
        // Список приходит от службы; имя можно добавить и вручную (база ещё не создана, сервер недоступен).
        m_sqlDatabases = new QListWidget;
        m_sqlDatabases->setMinimumHeight(80);
        m_sqlDatabases->setMaximumHeight(160);
        m_sqlDatabases->setToolTip(tr("Отметьте нужные базы. Ничего не отмечено — копируются все пользовательские "
                                      "базы сервера (без master, model, msdb, tempdb)."));
        connect(m_sqlConn, &ConnectionPicker::changed, this, [this] { loadDatabases("ms_sql", true); });
        QWidget *sqlButtons = dbButtons(m_sqlDatabases, "ms_sql");
        m_sqlArchives = makeArchiveMode();
        m_sqlStatus = new QLabel;
        m_sqlStatus->setWordWrap(true);
        m_sqlKind = new QComboBox;
        m_sqlKind->addItem(tr("Полная"), "full");
        m_sqlKind->addItem(tr("Дифференциальная"), "differential");
        m_sqlKind->addItem(tr("Журнал транзакций"), "log");
        m_sqlDir = new QLineEdit;
        m_sqlDir->setPlaceholderText(QStringLiteral("C:\\MSSQL\\Backup  или  \\\\SQL01\\Backup$"));
        m_sqlVerify = new QCheckBox(tr("Проверять копию (RESTORE VERIFYONLY)"));
        m_sqlVerify->setChecked(true);
        f->addRow(tr("Сервер:"), m_sqlConn);
        f->addRow(tr("Базы:"), m_sqlDatabases);
        f->addRow(QString(), sqlButtons);
        f->addRow(QString(), m_sqlStatus);
        f->addRow(tr("Архивы:"), m_sqlArchives);
        f->addRow(tr("Тип копии:"), m_sqlKind);
        f->addRow(tr("Папка для .bak:"), m_sqlDir);
        f->addRow(m_sqlVerify);
        auto *hint = new QLabel(tr("SQL Server должен иметь право записи в папку для .bak, а служба SteadyBackup — "
                                   "право чтения. Для журнала транзакций нужна модель восстановления FULL (при выборе "
                                   "«все базы» базы с моделью SIMPLE пропускаются). Если одна база не сохранилась, "
                                   "остальные всё равно отправляются, а задание завершается ошибкой."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // Firebird
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_fbGbak = new QLineEdit;
        m_fbGbak->setPlaceholderText(QStringLiteral("C:\\Program Files (x86)\\Firebird\\Firebird_3_0\\gbak.exe"));
        auto *browse = new QPushButton(tr("Обзор…"));
        connect(browse, &QPushButton::clicked, this, [this] {
            const QString p = QFileDialog::getOpenFileName(this, tr("gbak.exe"), m_fbGbak->text(), tr("Программы (*.exe)"));
            if (!p.isEmpty()) m_fbGbak->setText(QDir::toNativeSeparators(p));
        });
        auto *gb = new QWidget;
        auto *gl = new QHBoxLayout(gb);
        gl->setContentsMargins(0, 0, 0, 0);
        gl->addWidget(m_fbGbak, 1);
        gl->addWidget(browse);
        m_fbConn = new ConnectionPicker(m_client, "firebird", tr("не выбрано"));
        m_fbDatabases = new QPlainTextEdit;
        m_fbDatabases->setMinimumHeight(64);
        m_fbDatabases->setMaximumHeight(120);
        m_fbDatabases->setPlaceholderText(tr("По одной базе на строку:\nlocalhost:E:\\Shop\\STANDART.FDB"));
        m_fbNamesFrom = new QLineEdit;
        m_fbNamesFrom->setPlaceholderText(tr("необязательно: папка, чьи подпапки — имена баз"));
        m_fbTemplate = new QLineEdit;
        m_fbTemplate->setPlaceholderText(QStringLiteral("E:\\{name}\\STANDART.FDB"));
        m_fbHost = new QLineEdit;
        m_fbHost->setPlaceholderText(QStringLiteral("localhost"));
        m_fbVerify = new QCheckBox(tr("Проверять копию восстановлением (gbak -c) и удалять временную базу"));
        m_fbVerify->setChecked(true);
        f->addRow(tr("gbak.exe:"), gb);
        f->addRow(tr("Пользователь:"), m_fbConn);
        f->addRow(tr("Базы:"), m_fbDatabases);
        f->addRow(tr("Папка имён:"), m_fbNamesFrom);
        f->addRow(tr("Шаблон пути:"), m_fbTemplate);
        f->addRow(tr("Сервер:"), m_fbHost);
        f->addRow(m_fbVerify);
        auto *hint = new QLabel(tr("Версия gbak должна совпадать с сервером: Firebird 1.5 и 3.0 — разные задания. "
                                   "В архив попадает файл .fbk, не восстановленная база. Пароль gbak не виден в командной строке. "
                                   "Выгрузку на WebDAV добавьте назначением, а не отдельной командой."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // PostgreSQL
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_pgConn = new ConnectionPicker(m_client, "postgresql", tr("не выбрано"));
        m_pgDatabases = new QListWidget;
        m_pgDatabases->setMinimumHeight(80);
        m_pgDatabases->setMaximumHeight(160);
        m_pgDatabases->setToolTip(tr("Отметьте нужные базы. Ничего не отмечено — копируются все базы сервера."));
        connect(m_pgConn, &ConnectionPicker::changed, this, [this] { loadDatabases("postgresql", true); });
        QWidget *pgButtons = dbButtons(m_pgDatabases, "postgresql");
        m_pgArchives = makeArchiveMode();
        m_pgFormat = new QComboBox;
        m_pgFormat->addItem(tr("Dump (custom pg_dump, .dump)"), QStringLiteral("custom"));
        m_pgFormat->addItem(tr("SQL-скрипт (plain pg_dump, .sql)"), QStringLiteral("plain"));
        m_pgFormat->setToolTip(tr("Dump восстанавливается через pg_restore. SQL-скрипт — через psql; "
                                  "текстовый дамп сжимается при упаковке 7-Zip."));
        m_pgStatus = new QLabel;
        m_pgStatus->setWordWrap(true);
        m_pgDump = new QLineEdit;
        m_pgDump->setPlaceholderText(tr("автоматически: самая новая версия в Program Files\\PostgreSQL"));
        auto *browse = new QPushButton(tr("Обзор…"));
        connect(browse, &QPushButton::clicked, this, [this] {
            const QString p = QFileDialog::getOpenFileName(this, tr("pg_dump.exe"), m_pgDump->text(), tr("Программы (*.exe)"));
            if (!p.isEmpty()) m_pgDump->setText(QDir::toNativeSeparators(p));
        });
        auto *db = new QWidget;
        auto *dl = new QHBoxLayout(db);
        dl->setContentsMargins(0, 0, 0, 0);
        dl->addWidget(m_pgDump, 1);
        dl->addWidget(browse);
        m_pgGlobals = new QCheckBox(tr("Сохранять роли и табличные пространства (pg_dumpall --globals-only)"));
        m_pgVerify = new QCheckBox(tr("Проверять каждую копию Dump (pg_restore --list)"));
        m_pgVerify->setChecked(true);
        const auto updatePgVerify = [this] {
            const bool custom = m_pgFormat->currentData().toString() == QLatin1String("custom");
            m_pgVerify->setEnabled(custom);
            m_pgVerify->setToolTip(custom ? QString()
                                          : tr("pg_restore не читает SQL-скрипты. Для plain-дампа эта проверка не выполняется."));
        };
        connect(m_pgFormat, &QComboBox::currentIndexChanged, this, [updatePgVerify](int) { updatePgVerify(); });
        updatePgVerify();
        f->addRow(tr("Сервер:"), m_pgConn);
        f->addRow(tr("Базы:"), m_pgDatabases);
        f->addRow(QString(), pgButtons);
        f->addRow(QString(), m_pgStatus);
        f->addRow(tr("Архивы:"), m_pgArchives);
        f->addRow(tr("Формат дампа:"), m_pgFormat);
        f->addRow(tr("pg_dump.exe:"), db);
        f->addRow(m_pgGlobals);
        f->addRow(m_pgVerify);
        auto *hint = new QLabel(tr("Отмеченные базы копируются; ничего не отмечено — все базы сервера, включая новые. "
                                   "Каждая база сохраняется отдельным файлом: Dump (.dump) восстанавливается через "
                                   "pg_restore, SQL-скрипт (.sql) — через psql. Версия pg_dump должна быть не старше сервера. Пароль передаётся "
                                   "через окружение и не виден в командной строке. Роли сохраняются только под "
                                   "суперпользователем. Если одна база не сохранилась, остальные всё равно "
                                   "отправляются, а задание завершается ошибкой."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // MySQL по SSH
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_myConn = new ConnectionPicker(m_client, "ssh", tr("не выбрано"));
        connect(m_myConn, &ConnectionPicker::changed, this, [this] { loadDatabases("mysql", true); });
        m_myHost = new QLineEdit;
        m_myHost->setPlaceholderText(tr("localhost (как его видит сервер SSH)"));
        m_myPort = new QSpinBox;
        m_myPort->setRange(0, 65535);
        m_myPort->setSpecialValueText(tr("по умолчанию (3306)"));
        m_myUser = new QLineEdit;
        m_myPassword = new PasswordEdit;
        m_myDatabases = new QListWidget;
        m_myDatabases->setMinimumHeight(80);
        m_myDatabases->setMaximumHeight(160);
        m_myDatabases->setToolTip(tr("Отметьте нужные базы. Ничего не отмечено — копируются все базы, доступные "
                                     "пользователю (без information_schema, performance_schema, mysql, sys)."));
        QWidget *myButtons = dbButtons(m_myDatabases, "mysql");
        m_myArchives = makeArchiveMode();
        m_myStatus = new QLabel;
        m_myStatus->setWordWrap(true);
        auto *db = new QWidget;
        auto *dl = new QHBoxLayout(db);
        dl->setContentsMargins(0, 0, 0, 0);
        dl->addWidget(m_myHost, 1);
        dl->addWidget(new QLabel(tr("порт:")));
        dl->addWidget(m_myPort);
        f->addRow(tr("Сервер SSH:"), m_myConn);
        f->addRow(tr("Сервер MySQL:"), db);
        f->addRow(tr("Пользователь БД:"), m_myUser);
        f->addRow(tr("Пароль БД:"), m_myPassword);
        f->addRow(tr("Базы:"), m_myDatabases);
        f->addRow(QString(), myButtons);
        f->addRow(QString(), m_myStatus);
        f->addRow(tr("Архивы:"), m_myArchives);
        auto *hint = new QLabel(tr("mysqldump (или mariadb-dump) запускается на сервере по SSH, дамп .sql передаётся "
                                   "по тому же соединению со сжатием. Копия согласованная (--single-transaction), сайт "
                                   "не останавливается. Пароль БД не попадает в командную строку сервера. Если одна база "
                                   "не сохранилась, остальные всё равно отправляются, а задание завершается ошибкой. "
                                   "Восстановление: mysql < база.sql."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // Файлы по SSH
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_sshConn = new ConnectionPicker(m_client, "ssh", tr("не выбрано"));
        m_sshPaths = new QPlainTextEdit;
        m_sshPaths->setMinimumHeight(64);
        m_sshPaths->setMaximumHeight(140);
        m_sshPaths->setPlaceholderText(tr("По одному каталогу на строку:\n/var/www/site.ru\npublic_html"));
        auto *check = new QPushButton(tr("Проверить"));
        m_sshStatus = new QLabel;
        m_sshStatus->setWordWrap(true);
        connect(check, &QPushButton::clicked, this, &JobDialog::checkSshPaths);
        auto *cw = new QWidget;
        auto *cl = new QHBoxLayout(cw);
        cl->setContentsMargins(0, 0, 0, 0);
        cl->addWidget(check);
        cl->addWidget(m_sshStatus, 1);
        m_sshKind = new QComboBox;
        m_sshKind->addItem(tr("Полная"), "full");
        m_sshKind->addItem(tr("Инкрементная (изменения с прошлой копии)"), "incremental");
        m_sshKind->addItem(tr("Дифференциальная (изменения с последней полной)"), "differential");
        m_sshFullEvery = new QSpinBox;
        m_sshFullEvery->setRange(0, 1000);
        m_sshFullEvery->setSpecialValueText(tr("никогда"));
        m_sshFullEvery->setSuffix(tr(" копий"));
        connect(m_sshKind, &QComboBox::currentIndexChanged, this,
                [this] { m_sshFullEvery->setEnabled(m_sshKind->currentData().toString() != QLatin1String("full")); });
        m_sshFullEvery->setEnabled(false);
        m_sshInclude = new QLineEdit;
        m_sshInclude->setPlaceholderText(tr("все файлы; или напр. *.php; *.jpg"));
        m_sshExclude = new QLineEdit;
        m_sshExclude->setPlaceholderText(QStringLiteral("cache/**; *.log; **/node_modules/**"));
        f->addRow(tr("Сервер SSH:"), m_sshConn);
        f->addRow(tr("Каталоги:"), m_sshPaths);
        f->addRow(QString(), cw);
        f->addRow(tr("Тип копии:"), m_sshKind);
        f->addRow(tr("Полная каждые:"), m_sshFullEvery);
        f->addRow(tr("Включать:"), m_sshInclude);
        f->addRow(tr("Исключать:"), m_sshExclude);
        auto *hint = new QLabel(tr("Файлы скачиваются по SFTP. Инкрементная копия забирает только изменённые файлы "
                                   "(по размеру и времени изменения). Символические ссылки и пустые каталоги не "
                                   "копируются. Для полной копии сайта добавьте второе задание «Базы MySQL по SSH» "
                                   "и объедините их в группу с общим расписанием."));
        hint->setWordWrap(true);
        theme::markHint(hint);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }
    { // Hyper-V
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        m_vmName = new QComboBox;
        m_vmName->setEditable(true);
        m_client->call({{"cmd", "list_vms"}}, [this](bool ok, const QJsonValue &r, const QString &) {
            if (!ok) return;
            const QString cur = m_vmName->currentText();
            for (const auto &v : r.toArray()) {
                const QString n = v.toObject().value("Name").toString();
                if (!n.isEmpty() && m_vmName->findText(n) < 0) m_vmName->addItem(n);
            }
            m_vmName->setCurrentText(cur);
        }, this);
        m_vmKind = new QComboBox;
        m_vmKind->addItem(tr("Полная (весь диск каждый раз)"), "full");
        m_vmKind->addItem(tr("Инкрементная (только изменённые блоки, RCT)"), "incremental");
        m_vmFullEvery = new QSpinBox;
        m_vmFullEvery->setRange(0, 1000);
        m_vmFullEvery->setValue(7);
        m_vmFullEvery->setSpecialValueText(tr("никогда"));
        m_vmFullEvery->setSuffix(tr(" копий"));
        connect(m_vmKind, &QComboBox::currentIndexChanged, this,
                [this] { m_vmFullEvery->setEnabled(m_vmKind->currentData() == "incremental"); });
        m_vmFullEvery->setEnabled(false);
        m_vmConsistency = new QComboBox;
        m_vmConsistency->addItem(tr("Согласованная (VSS внутри ВМ) — рекомендуется"), "application");
        m_vmConsistency->addItem(tr("Аварийно-согласованная (без участия ВМ)"), "crash");
        f->addRow(tr("Виртуальная машина:"), m_vmName);
        f->addRow(tr("Тип копии:"), m_vmKind);
        f->addRow(tr("Полная каждые:"), m_vmFullEvery);
        f->addRow(tr("Согласованность:"), m_vmConsistency);
        auto *hint = new QLabel(tr("Копия делается без остановки ВМ. Временная контрольная точка после копирования "
                                   "сливается с диском — лишних .avhdx и контрольных точек не остаётся.\n"
                                   "Согласованная копия требует служб интеграции Hyper-V в гостевой ОС; если они "
                                   "недоступны, будет сделана аварийно-согласованная копия с предупреждением.\n"
                                   "Восстановление цепочки: steady restore-vm <архивы> --target <папка>"));
        theme::markHint(hint);
        hint->setWordWrap(true);
        f->addRow(hint);
        m_srcPages->addWidget(w);
    }

    connect(m_srcType, &QComboBox::currentIndexChanged, m_srcPages, &QStackedWidget::setCurrentIndex);
    auto *v = new QVBoxLayout(page);
    auto *f = new QFormLayout;
    f->addRow(tr("Что копировать:"), m_srcType);
    v->addLayout(f);
    v->addWidget(m_srcPages, 1);

    m_dateBox = new QGroupBox(tr("Фильтр по дате"));
    auto *df = new QFormLayout(m_dateBox);
    m_dateField = new QComboBox;
    m_dateField->addItem(tr("Дата изменения"), "modified");
    m_dateField->addItem(tr("Дата создания"), "created");
    m_dateMode = new QComboBox;
    m_dateMode->addItem(tr("Все файлы, без ограничения по дате"), "all");
    m_dateMode->addItem(tr("Не старше указанного периода от начала копирования"), "within");
    m_dateMode->addItem(tr("Начиная с указанной даты и времени"), "since");
    m_dateHours = new QSpinBox;
    m_dateHours->setRange(1, 100000);
    m_dateHours->setValue(24);
    m_dateHours->setSuffix(tr(" ч."));
    m_dateHours->setToolTip(tr("24 часа — сутки, 168 часов — неделя. Момент отсчёта фиксируется при начале подготовки копии."));
    m_dateSince = new QDateTimeEdit(QDateTime::currentDateTime().addDays(-1));
    m_dateSince->setCalendarPopup(true);
    m_dateSince->setDisplayFormat(QStringLiteral("dd.MM.yyyy HH:mm"));
    m_dateSince->setToolTip(tr("Копируются файлы с выбранной датой не раньше этого момента (включая границу)."));
    auto *dateHint = new QLabel(tr("Для WebDAV дата создания доступна, если сервер возвращает creationdate. "
                                   "Протокол SFTP передаёт только дату изменения."));
    dateHint->setWordWrap(true);
    theme::markHint(dateHint);
    df->addRow(tr("Дата:"), m_dateField);
    df->addRow(tr("Условие:"), m_dateMode);
    df->addRow(tr("Не старше:"), m_dateHours);
    df->addRow(tr("Начиная с:"), m_dateSince);
    df->addRow(dateHint);
    v->addWidget(m_dateBox);
    const auto updateDateFilter = [this] {
        const QString source = m_srcType->currentData().toString();
        const bool localFiles = source == QLatin1String("files");
        const bool sshFiles = source == QLatin1String("ssh_files");
        const bool active = localFiles || sshFiles;
        const QString mode = m_dateMode->currentData().toString();
        m_dateBox->setVisible(active);
        if (sshFiles && m_dateField->currentData().toString() == QLatin1String("created"))
            m_dateField->setCurrentIndex(0);
        m_dateField->setEnabled(localFiles && mode != QLatin1String("all"));
        m_dateHours->setEnabled(active && mode == QLatin1String("within"));
        m_dateSince->setEnabled(active && mode == QLatin1String("since"));
    };
    connect(m_dateMode, &QComboBox::currentIndexChanged, this, [updateDateFilter](int) { updateDateFilter(); });
    connect(m_srcType, &QComboBox::currentIndexChanged, this, [updateDateFilter](int) { updateDateFilter(); });
    updateDateFilter();
    return page;
}

// ---------------- Назначения ----------------

QWidget *JobDialog::buildDestinationsPage() {
    auto *page = new QWidget;
    m_destList = new QListWidget;
    auto *add = new QPushButton(tr("Добавить…"));
    auto *edit = new QPushButton(tr("Изменить…"));
    auto *del = new QPushButton(tr("Удалить"));
    auto editRow = [this](int row) {
        if (row < 0 || row >= m_destinations.size()) return;
        if (m_destinations.at(row).toObject().value("type") != "storage") {
            QMessageBox::information(this, windowTitle(),
                                     tr("Это назначение задано прямо в задании (старый формат). Чтобы изменить его, "
                                        "откройте «Хранилища…» → «Перенести из заданий» или удалите и добавьте "
                                        "назначение заново."));
            return;
        }
        DestinationDialog dlg(m_client, m_destinations.at(row).toObject(), this);
        dlg.prime(storagesForDialog());
        if (dlg.exec() == QDialog::Accepted) {
            m_destinations.replace(row, dlg.value());
            refreshDestinations();
        }
    };
    connect(add, &QPushButton::clicked, this, [this] {
        DestinationDialog dlg(m_client, {}, this);
        dlg.prime(storagesForDialog());
        if (dlg.exec() == QDialog::Accepted) {
            m_destinations.append(dlg.value());
            refreshDestinations();
        }
    });
    connect(edit, &QPushButton::clicked, this, [this, editRow] { editRow(m_destList->currentRow()); });
    connect(m_destList, &QListWidget::itemDoubleClicked, this, [this, editRow] { editRow(m_destList->currentRow()); });
    connect(del, &QPushButton::clicked, this, [this] {
        const int r = m_destList->currentRow();
        if (r >= 0) {
            m_destinations.removeAt(r);
            refreshDestinations();
        }
    });
    auto *bl = new QVBoxLayout;
    bl->addWidget(add);
    bl->addWidget(edit);
    bl->addWidget(del);
    bl->addStretch();
    auto *h = new QHBoxLayout(page);
    h->addWidget(m_destList, 1);
    h->addLayout(bl);
    return page;
}

QJsonArray JobDialog::storagesForDialog() const {
    QJsonArray all = m_storageList.isEmpty() ? util::storageCache() : m_storageList;
    auto known = [&](const QString &id) {
        if (id.isEmpty()) return;
        for (const auto &s : all)
            if (s.toObject().value("id").toString() == id) return;
        const QString name = m_storageNames.value(id);
        all.append(QJsonObject{{"id", id}, {"name", name.isEmpty() ? id : name}});
    };
    for (const auto &d : m_destinations) {
        const QJsonObject o = d.toObject();
        if (o.value("type").toString() == "storage") known(o.value("storage").toString());
    }
    return all;
}

void JobDialog::refreshDestinations() {
    m_destList->clear();
    for (const auto &d : std::as_const(m_destinations)) {
        QJsonObject o = d.toObject();
        if (o.value("type") == "storage") o["storage_name"] = m_storageNames.value(o.value("storage").toString());
        QString text = util::destinationSummary(o);
        if (d.toObject().value("type") != "storage") text += tr("  (задано в задании)");
        m_destList->addItem(text);
    }
}

// ---------------- Архив ----------------

QWidget *JobDialog::buildArchivePage() {
    auto *page = new QWidget;
    auto *v = new QVBoxLayout(page);

    auto *arc = new QGroupBox(tr("Упаковка 7-Zip"));
    auto *f = new QFormLayout(arc);
    m_arcEnabled = new QCheckBox(tr("Упаковывать в архив .7z"));
    m_arcMode = new QComboBox;
    m_arcMode->addItem(tr("Один общий архив"), "single");
    m_arcMode->addItem(tr("Каждый файл — в отдельный архив"), "per_file");
    m_arcMode->addItem(tr("Каждая строка источника — в отдельный архив"), "per_source");
    m_arcMode->addItem(tr("Каждая вложенная папка 1-го уровня — в отдельный архив"), "per_directory");
    m_arcMode->setToolTip(tr("Вариант применяется к файловым источникам, в том числе по SSH. При разбиении по папкам "
                             "файлы непосредственно в корне строки источника собираются в отдельный архив этой строки. "
                             "Архивы одного запуска образуют один набор и ротируются вместе."));
    m_arcLevel = new QSpinBox;
    m_arcLevel->setRange(0, 9);
    m_arcPassword = new PasswordEdit;
    m_arcHeaders = new QCheckBox(tr("Шифровать имена файлов"));
    m_arcVolume = new QLineEdit;
    m_arcVolume->setPlaceholderText(tr("не делить; напр. 4g или 700m"));
    m_arcTest = new QCheckBox(tr("Проверять архив после упаковки"));
    f->addRow(m_arcEnabled);
    f->addRow(tr("Вариант архивирования:"), m_arcMode);
    f->addRow(tr("Степень сжатия (0–9):"), m_arcLevel);
    f->addRow(tr("Пароль (AES-256):"), m_arcPassword);
    f->addRow(m_arcHeaders);
    f->addRow(tr("Размер тома:"), m_arcVolume);
    f->addRow(m_arcTest);
    for (QWidget *w : std::initializer_list<QWidget *>{m_arcLevel, m_arcPassword, m_arcHeaders, m_arcVolume, m_arcTest})
        connect(m_arcEnabled, &QCheckBox::toggled, w, &QWidget::setEnabled);
    const auto updateArchiveMode = [this] {
        const QString source = m_srcType->currentData().toString();
        const bool files = source == QLatin1String("files") || source == QLatin1String("ssh_files");
        m_arcMode->setEnabled(m_arcEnabled->isChecked() && files);
    };
    connect(m_arcEnabled, &QCheckBox::toggled, this, [updateArchiveMode](bool) { updateArchiveMode(); });
    connect(m_srcType, &QComboBox::currentIndexChanged, this, [updateArchiveMode](int) { updateArchiveMode(); });
    updateArchiveMode();

    auto *ret = new QGroupBox(tr("Хранение копий"));
    auto *rf = new QFormLayout(ret);
    m_keepFull = new QSpinBox;
    m_keepFull->setRange(0, 10000);
    m_keepFull->setSpecialValueText(tr("хранить все"));
    m_keepDays = new QSpinBox;
    m_keepDays->setRange(0, 36500);
    m_keepDays->setSpecialValueText(tr("не ограничивать"));
    m_keepDays->setSuffix(tr(" дн."));
    m_keepSets = new QSpinBox;
    m_keepSets->setRange(0, 100000);
    m_keepSets->setSpecialValueText(tr("не ограничивать"));
    rf->addRow(tr("Хранить полных копий:"), m_keepFull);
    rf->addRow(tr("Удалять наборы старше:"), m_keepDays);
    rf->addRow(tr("Хранить наборов не больше:"), m_keepSets);
    auto *hint = new QLabel(tr("Вместе с удаляемой полной копией удаляются и зависящие от неё "
                               "инкрементные и дифференциальные копии. Срок и число наборов — для копий "
                               "без своей полной (журнал и разностные MSSQL) и для хвоста старше сохранённой полной. "
                               "Цепочку сохранённой полной они не удаляют."));
    hint->setWordWrap(true);
    theme::markHint(hint);
    rf->addRow(hint);

    v->addWidget(arc);
    v->addWidget(ret);
    v->addStretch();
    return page;
}

// ---------------- Расписание ----------------

QWidget *JobDialog::buildSchedulePage() {
    auto *page = new QWidget;
    m_schedType = new QComboBox;
    m_schedType->addItem(tr("Только вручную"), "manual");
    m_schedType->addItem(tr("Ежедневно"), "daily");
    m_schedType->addItem(tr("По дням недели"), "weekly");
    m_schedType->addItem(tr("Ежемесячно"), "monthly");
    m_schedType->addItem(tr("С интервалом"), "interval");
    m_schedType->addItem(tr("Однократно"), "once");

    m_schedTimes = new QLineEdit(QStringLiteral("23:00"));
    m_schedTimes->setPlaceholderText(QStringLiteral("23:00; 13:00"));
    m_schedPages = new QStackedWidget;

    m_schedPages->addWidget(new QWidget); // manual
    m_schedPages->addWidget(new QWidget); // daily — только время
    { // weekly
        auto *w = new QWidget;
        auto *h = new QHBoxLayout(w);
        h->setContentsMargins(0, 0, 0, 0);
        const QStringList names{tr("Пн"), tr("Вт"), tr("Ср"), tr("Чт"), tr("Пт"), tr("Сб"), tr("Вс")};
        for (const auto &n : names) {
            auto *cb = new QCheckBox(n);
            m_weekDays << cb;
            h->addWidget(cb);
        }
        h->addStretch();
        m_schedPages->addWidget(w);
    }
    { // monthly
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_monthDays = new QLineEdit(QStringLiteral("1"));
        m_monthDays->setPlaceholderText(tr("напр. 1; 15; 31 (31 = последний день месяца)"));
        f->addRow(tr("Числа месяца:"), m_monthDays);
        m_schedPages->addWidget(w);
    }
    { // interval
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_intervalMin = new QSpinBox;
        m_intervalMin->setRange(1, 10080);
        m_intervalMin->setValue(60);
        m_intervalMin->setSuffix(tr(" мин"));
        m_intervalWindow = new QCheckBox(tr("Только в интервале времени"));
        m_intervalFrom = new QTimeEdit(QTime(8, 0));
        m_intervalTo = new QTimeEdit(QTime(20, 0));
        auto *hw = new QWidget;
        auto *hl = new QHBoxLayout(hw);
        hl->setContentsMargins(0, 0, 0, 0);
        hl->addWidget(m_intervalWindow);
        hl->addWidget(new QLabel(tr("с")));
        hl->addWidget(m_intervalFrom);
        hl->addWidget(new QLabel(tr("до")));
        hl->addWidget(m_intervalTo);
        hl->addStretch();
        f->addRow(tr("Каждые:"), m_intervalMin);
        f->addRow(hw);
        m_schedPages->addWidget(w);
    }
    { // once
        auto *w = new QWidget;
        auto *f = new QFormLayout(w);
        f->setContentsMargins(0, 0, 0, 0);
        m_onceAt = new QDateTimeEdit(QDateTime::currentDateTime().addSecs(3600));
        m_onceAt->setCalendarPopup(true);
        m_onceAt->setDisplayFormat(QStringLiteral("dd.MM.yyyy HH:mm"));
        f->addRow(tr("Дата и время:"), m_onceAt);
        m_schedPages->addWidget(w);
    }

    auto *timesLabel = new QLabel(tr("Время запуска:"));
    auto updateVisibility = [this, timesLabel](int i) {
        m_schedPages->setCurrentIndex(i);
        const QString t = m_schedType->itemData(i).toString();
        const bool needTimes = t == "daily" || t == "weekly" || t == "monthly";
        m_schedTimes->setVisible(needTimes);
        timesLabel->setVisible(needTimes);
    };
    connect(m_schedType, &QComboBox::currentIndexChanged, this, updateVisibility);

    m_runMissed = new QCheckBox(tr("Выполнять пропущенные запуски (если компьютер был выключен)"));
    m_runMissed->setChecked(true);

    // 0 — значение по умолчанию для типа источника (1С 3 ч, SQL 6 ч, файлы и Hyper-V 12 ч).
    m_timeoutMin = new QSpinBox;
    m_timeoutMin->setRange(0, 7 * 24 * 60);
    m_timeoutMin->setSpecialValueText(tr("по умолчанию"));
    m_timeoutMin->setSuffix(tr(" мин"));
    m_timeoutMin->setToolTip(tr("Предельное время одного запуска внешней программы (1С, SQL, Hyper-V, 7-Zip). "
                                "По истечении процесс завершается, задание — с ошибкой."));

    // Список групп приходит от службы; ссылку из задания показываем сразу, чтобы сохранение
    // до ответа не сняло задание с группы.
    m_group = new SteadyCombo;
    m_group->addItem(tr("без группы"), QString());
    m_group->setToolTip(tr("По расписанию группы задание запускается вместе с остальными её заданиями. "
                           "Собственное расписание задания тоже действует. Группы — в главном окне."));
    m_client->call({{"cmd", "list_groups"}}, [this](bool ok, const QJsonValue &r, const QString &) {
        if (!ok) return;
        const QString cur = m_group->currentData().toString();
        m_group->clear();
        m_group->addItem(tr("без группы"), QString());
        for (const auto &v : r.toArray()) {
            const QJsonObject g = v.toObject();
            m_group->addItem(g.value("name").toString(), g.value("id").toString());
        }
        const int i = m_group->findData(cur);
        if (i >= 0) m_group->setCurrentIndex(i);
        else if (!cur.isEmpty()) {
            m_group->addItem(tr("группа удалена"), cur);
            m_group->setCurrentIndex(m_group->count() - 1);
        }
    }, this);

    auto *f = new QFormLayout;
    f->addRow(tr("Запуск:"), m_schedType);
    f->addRow(timesLabel, m_schedTimes);
    f->addRow(tr("Группа:"), m_group);
    f->addRow(tr("Прервать, если длится дольше:"), m_timeoutMin);
    auto *v = new QVBoxLayout(page);
    v->addLayout(f);
    v->addWidget(m_schedPages);
    v->addWidget(m_runMissed);
    auto *retryBox = new QGroupBox(tr("При ошибке"));
    auto *rl = new QVBoxLayout(retryBox);
    m_retry = new RetryEditor(false);
    rl->addWidget(m_retry);
    auto *retryHint = new QLabel(tr("При запуске по расписанию группы действуют повторы группы, если они в ней заданы."));
    retryHint->setWordWrap(true);
    theme::markHint(retryHint);
    rl->addWidget(retryHint);
    v->addWidget(retryBox);
    v->addStretch();
    updateVisibility(0);
    return page;
}

// ---------------- Загрузка / сохранение ----------------

static QString joinArray(const QJsonValue &v, const QString &sep) {
    QStringList l;
    for (const auto &x : v.toArray())
        l << (x.isDouble() ? QString::number(x.toInt()) : x.toString());
    return l.join(sep);
}

static int indexOfData(QComboBox *c, const QString &d) {
    return qMax(0, c->findData(d));
}

void JobDialog::load(const QJsonObject &job) {
    m_name->setText(job.value("name").toString());
    m_enabled->setChecked(job.value("enabled").toBool(true));

    const QJsonObject s = job.value("source").toObject();
    const QString st = s.value("type").toString("files");
    m_srcType->setCurrentIndex(indexOfData(m_srcType, st));
    m_srcPages->setCurrentIndex(m_srcType->currentIndex());
    if (st == "files") {
        m_filesPaths->setPlainText(joinArray(s.value("paths"), "\n"));
        m_filesInclude->setText(joinArray(s.value("include"), "; "));
        m_filesExclude->setText(joinArray(s.value("exclude"), "; "));
        m_filesKind->setCurrentIndex(indexOfData(m_filesKind, s.value("kind").toString("full")));
        m_filesFullEvery->setValue(s.value("full_every").toInt());
        m_filesFullEvery->setEnabled(m_filesKind->currentData().toString() != QLatin1String("full"));
        m_filesConn->setSource(s);
        m_filesVss->setChecked(s.value("vss").toBool(true));
    } else if (st == "one_c") {
        m_onecPath->setText(s.value("ib_path").toString());
        m_onecExe->setText(s.value("platform_exe").toString());
        m_onecConn->setSource(s);
        m_onecMode->setCurrentIndex(indexOfData(m_onecMode, s.value("mode").toString("dump_dt")));
        m_onecUnlock->setText(s.value("unlock_code").toString());
    } else if (st == "one_c_update") {
        m_onecUpdatePath->setText(s.value("ib_path").toString());
        m_onecUpdateUpdatesDir->setText(s.value("updates_dir").toString());
        m_onecUpdateBackupDir->setText(s.value("backup_dir").toString());
        m_onecUpdateExe->setText(s.value("platform_exe").toString());
        m_onecUpdateConn->setSource(s);
        m_onecUpdateUnlock->setText(s.value("unlock_code").toString());
        m_onecUpdateClusterUser->setText(s.value("cluster_user").toString());
        m_onecUpdateClusterPassword->setStored(s.value("cluster_password").toString());
        m_onecUpdateForce->setChecked(s.value("force").toBool(false));
        m_onecUpdateServerSide->setChecked(s.value("server_side").toBool(false));
        m_onecUpdateRunHandlers->setChecked(s.value("run_handlers").toBool(false));
    } else if (st == "one_c_download") {
        m_dlTemplateDir->setText(s.value("template_dir").toString());
        m_dlPlatformDir->setText(s.value("platform_dir").toString());
        m_dlUnzip->setChecked(s.value("unzip_files").toBool(false));
        m_dlInsecure->setChecked(s.value("insecure_tls").toBool(false));
        m_dlCheckOnly->setChecked(s.value("check_only").toBool(false));
        const QJsonObject proxy = s.value("proxy").toObject();
        m_dlProxyHost->setText(proxy.value("host").toString());
        m_dlProxyPort->setValue(proxy.value("port").toInt(0));
        m_dlProxyUser->setText(proxy.value("username").toString());
        m_dlProxyPassword->setStored(proxy.value("password").toString());
        m_dlAccountList = s.value("its_accounts").toArray();
        refreshItsAccounts();
        // Учётки могло не оказаться в списке (переименована): показываем её, чтобы не потерять при сохранении.
        const auto selectAccount = [](QComboBox *combo, const QString &name) {
            if (combo->findData(name) < 0) combo->addItem(name, name);
            combo->setCurrentIndex(indexOfData(combo, name));
        };
        const QJsonObject platform = s.value("platform").toObject();
        const QJsonObject releases = s.value("releases").toObject();
        m_dlPlatformVersion->setText(platform.value("start_version").toString());
        selectAccount(m_dlPlatformAccount, platform.value("its_account").toString());
        m_dlReleasesEnabled->setChecked(releases.value("enabled").toBool(true));
        selectAccount(m_dlReleasesAccount, releases.value("its_account").toString());
        m_dlReleasesCount->setValue(releases.value("count").toInt(2));
        m_dlReleasesProjects->setText(releases.contains("projects")
                                          ? joinArray(releases.value("projects"), ", ")
                                          : defaultDlProjects().join(QStringLiteral(", ")));
        m_dlDistributions->setPlainText(releases.contains("distributions")
                                            ? joinArray(releases.value("distributions"), "\n")
                                            : defaultDlDistributions().join(QStringLiteral("\n")));
        m_dlConfigurations->setRowCount(0);
        for (const auto &v : s.value("configurations").toArray()) {
            const QJsonObject c = v.toObject();
            const int r = m_dlConfigurations->rowCount();
            m_dlConfigurations->insertRow(r);
            m_dlConfigurations->setItem(r, 0, new QTableWidgetItem(c.value("human_name").toString()));
            m_dlConfigurations->setItem(r, 1, new QTableWidgetItem(c.value("program_name").toString()));
            m_dlConfigurations->setItem(r, 2, new QTableWidgetItem(c.value("start_version").toString()));
            m_dlConfigurations->setItem(r, 3, new QTableWidgetItem(c.value("its_account").toString()));
        }
    } else if (st == "ms_sql") {
        m_sqlConn->setSource(s);
        QStringList dbs;
        for (const auto &d : s.value("databases").toArray()) dbs << d.toString();
        // Прежние задания хранят одну базу в "database".
        if (dbs.isEmpty() && !s.value("database").toString().isEmpty()) dbs << s.value("database").toString();
        setDbList(m_sqlDatabases, dbs, {});
        m_sqlArchives->setCurrentIndex(s.value("separate_archives").toBool(false) ? 1 : 0);
        m_sqlKind->setCurrentIndex(indexOfData(m_sqlKind, s.value("kind").toString("full")));
        m_sqlDir->setText(s.value("backup_dir").toString());
        m_sqlVerify->setChecked(s.value("verify").toBool(true));
    } else if (st == "firebird") {
        m_fbGbak->setText(s.value("gbak").toString());
        m_fbConn->setSource(s);
        m_fbDatabases->setPlainText(joinArray(s.value("databases"), "\n"));
        m_fbNamesFrom->setText(s.value("names_from").toString());
        m_fbTemplate->setText(s.value("path_template").toString());
        m_fbHost->setText(s.value("host").toString());
        m_fbVerify->setChecked(s.value("verify").toBool(true));
    } else if (st == "postgresql") {
        m_pgConn->setSource(s);
        QStringList dbs;
        for (const auto &d : s.value("databases").toArray()) dbs << d.toString();
        setDbList(m_pgDatabases, dbs, {});
        m_pgArchives->setCurrentIndex(s.value("separate_archives").toBool(false) ? 1 : 0);
        m_pgFormat->setCurrentIndex(indexOfData(m_pgFormat, s.value(QStringLiteral("format")).toString("custom")));
        m_pgDump->setText(s.value("pg_dump").toString());
        m_pgGlobals->setChecked(s.value("globals").toBool(false));
        m_pgVerify->setChecked(s.value("verify").toBool(true));
    } else if (st == "mysql") {
        m_myConn->setSource(s);
        m_myHost->setText(s.value("db_host").toString());
        m_myPort->setValue(s.value("db_port").toInt(0));
        m_myUser->setText(s.value("db_user").toString());
        m_myPassword->setStored(s.value("db_password").toString());
        QStringList dbs;
        for (const auto &d : s.value("databases").toArray()) dbs << d.toString();
        setDbList(m_myDatabases, dbs, {});
        m_myArchives->setCurrentIndex(s.value("separate_archives").toBool(false) ? 1 : 0);
    } else if (st == "ssh_files") {
        m_sshConn->setSource(s);
        m_sshPaths->setPlainText(joinArray(s.value("paths"), "\n"));
        m_sshInclude->setText(joinArray(s.value("include"), "; "));
        m_sshExclude->setText(joinArray(s.value("exclude"), "; "));
        m_sshKind->setCurrentIndex(indexOfData(m_sshKind, s.value("kind").toString("full")));
        m_sshFullEvery->setValue(s.value("full_every").toInt());
        m_sshFullEvery->setEnabled(m_sshKind->currentData().toString() != QLatin1String("full"));
    } else if (st == "hyper_v") {
        m_vmName->setCurrentText(s.value("vm_name").toString());
        m_vmKind->setCurrentIndex(indexOfData(m_vmKind, s.value("kind").toString("full")));
        m_vmFullEvery->setValue(s.value("full_every").toInt(7));
        m_vmFullEvery->setEnabled(m_vmKind->currentData() == "incremental");
        m_vmConsistency->setCurrentIndex(indexOfData(m_vmConsistency, s.value("consistency").toString("application")));
    }

    const QJsonObject dateFilter = (st == QLatin1String("files") || st == QLatin1String("ssh_files"))
        ? s.value("date_filter").toObject()
        : QJsonObject{};
    m_dateField->setCurrentIndex(indexOfData(m_dateField, dateFilter.value("field").toString("modified")));
    m_dateMode->setCurrentIndex(indexOfData(m_dateMode, dateFilter.value("mode").toString("all")));
    m_dateHours->setValue(dateFilter.value("within_hours").toInt(24));
    const QDateTime since = QDateTime::fromString(dateFilter.value("since").toString().left(19), Qt::ISODate);
    if (since.isValid()) m_dateSince->setDateTime(since);

    m_destinations = job.value("destinations").toArray();
    refreshDestinations();
    m_client->call({{"cmd", "list_storages"}}, [this](bool ok, const QJsonValue &r, const QString &) {
        if (!ok) return;
        m_storageList = r.toArray();
        util::setStorageCache(m_storageList);
        m_storageNames.clear();
        for (const auto &s : std::as_const(m_storageList))
            m_storageNames.insert(s.toObject().value("id").toString(), s.toObject().value("name").toString());
        refreshDestinations();
    }, this);

    const QJsonObject a = job.value("archive").toObject();
    m_arcEnabled->setChecked(a.value("enabled").toBool(true));
    m_arcMode->setCurrentIndex(indexOfData(m_arcMode, a.value("mode").toString("single")));
    m_arcLevel->setValue(a.value("level").toInt(5));
    m_arcPassword->setStored(a.value("password").toString());
    m_arcHeaders->setChecked(a.value("encrypt_headers").toBool(true));
    m_arcVolume->setText(a.value("volume_size").toString());
    m_arcTest->setChecked(a.value("test_after").toBool(true));
    const QJsonObject ret = job.value("retention").toObject();
    m_keepFull->setValue(ret.value("keep_full").toInt(0));
    m_keepDays->setValue(ret.value("keep_days").toInt(0));
    m_keepSets->setValue(ret.value("keep_sets").toInt(0));

    const QJsonObject sc = job.value("schedule").toObject();
    const QString t = sc.value("type").toString("manual");
    m_schedType->setCurrentIndex(indexOfData(m_schedType, t));
    if (sc.contains("times")) {
        QStringList ts;
        for (const auto &x : sc.value("times").toArray())
            ts << x.toString().left(5);
        m_schedTimes->setText(ts.join("; "));
    }
    if (t == "weekly")
        for (const auto &d : sc.value("days").toArray())
            for (int i = 0; i < 7; ++i)
                if (d.toString() == QLatin1String(kDays[i])) m_weekDays[i]->setChecked(true);
    if (t == "monthly") m_monthDays->setText(joinArray(sc.value("days"), "; "));
    if (t == "interval") {
        m_intervalMin->setValue(sc.value("minutes").toInt(60));
        const bool win = sc.value("from").isString() && sc.value("to").isString();
        m_intervalWindow->setChecked(win);
        if (win) {
            m_intervalFrom->setTime(QTime::fromString(sc.value("from").toString().left(5), "HH:mm"));
            m_intervalTo->setTime(QTime::fromString(sc.value("to").toString().left(5), "HH:mm"));
        }
    }
    if (t == "once") m_onceAt->setDateTime(QDateTime::fromString(sc.value("at").toString().left(19), Qt::ISODate));
    m_runMissed->setChecked(job.value("run_missed").toBool(true));
    m_timeoutMin->setValue(job.value("timeout_minutes").toInt(0));
    m_retry->load(job);
    const QString group = job.value("group").toString();
    if (!group.isEmpty()) {
        int i = m_group->findData(group);
        if (i < 0) {
            m_group->addItem(tr("группа (загрузка…)"), group);
            i = m_group->count() - 1;
        }
        m_group->setCurrentIndex(i);
    }
}

static QJsonArray toArray(const QStringList &l) {
    QJsonArray a;
    for (const auto &s : l)
        a.append(s);
    return a;
}

static QJsonArray parseTimes(const QString &text) {
    QJsonArray a;
    for (const auto &s : util::splitList(text)) {
        const QTime t = QTime::fromString(s, s.count(':') == 2 ? "H:mm:ss" : "H:mm");
        if (t.isValid()) a.append(t.toString("HH:mm:ss"));
    }
    return a;
}

QJsonObject JobDialog::value() const {
    using util::optString;
    QJsonObject job = m_original;
    if (!job.contains("id")) job["id"] = QUuid::createUuid().toString(QUuid::WithoutBraces);
    job["name"] = m_name->text().trimmed();
    job["enabled"] = m_enabled->isChecked();

    const QString st = m_srcType->currentData().toString();
    // Поля исходного задания сохраняем, пока тип источника не меняли: так не теряются
    // учётные данные и поля, которых нет в этой версии окна.
    QJsonObject s = m_original.value("source").toObject();
    if (s.value("type").toString() != st) s = QJsonObject{};
    s["type"] = st;
    if (st == "files") {
        QStringList paths;
        for (const auto &p : m_filesPaths->toPlainText().split('\n', Qt::SkipEmptyParts))
            if (!p.trimmed().isEmpty()) paths << p.trimmed();
        s["paths"] = toArray(paths);
        s["include"] = toArray(util::splitList(m_filesInclude->text()));
        s["exclude"] = toArray(util::splitList(m_filesExclude->text()));
        s["kind"] = m_filesKind->currentData().toString();
        s["full_every"] = m_filesFullEvery->value();
        s["vss"] = m_filesVss->isChecked();
        m_filesConn->apply(s);
    } else if (st == "one_c") {
        s["ib_path"] = m_onecPath->text().trimmed();
        s["platform_exe"] = optString(m_onecExe->text());
        m_onecConn->apply(s);
        s["mode"] = m_onecMode->currentData().toString();
        s["unlock_code"] = optString(m_onecUnlock->text());
    } else if (st == "one_c_update") {
        s[QStringLiteral("ib_path")] = m_onecUpdatePath->text().trimmed();
        s[QStringLiteral("updates_dir")] = m_onecUpdateUpdatesDir->text().trimmed();
        s[QStringLiteral("backup_dir")] = m_onecUpdateBackupDir->text().trimmed();
        s[QStringLiteral("platform_exe")] = optString(m_onecUpdateExe->text());
        m_onecUpdateConn->apply(s);
        s[QStringLiteral("unlock_code")] = optString(m_onecUpdateUnlock->text());
        s[QStringLiteral("cluster_user")] = optString(m_onecUpdateClusterUser->text());
        s[QStringLiteral("cluster_password")] = optString(m_onecUpdateClusterPassword->value().toString());
        s[QStringLiteral("force")] = m_onecUpdateForce->isChecked();
        s[QStringLiteral("server_side")] = m_onecUpdateServerSide->isChecked();
        s[QStringLiteral("run_handlers")] = m_onecUpdateRunHandlers->isChecked();
    } else if (st == "one_c_download") {
        s[QStringLiteral("template_dir")] = m_dlTemplateDir->text().trimmed();
        s[QStringLiteral("platform_dir")] = m_dlPlatformDir->text().trimmed();
        s[QStringLiteral("unzip_files")] = m_dlUnzip->isChecked();
        s[QStringLiteral("insecure_tls")] = m_dlInsecure->isChecked();
        s[QStringLiteral("check_only")] = m_dlCheckOnly->isChecked();
        // Пустой сервер — прокси не задан: ключ proxy в задании не создаём.
        if (m_dlProxyHost->text().trimmed().isEmpty()) {
            s.remove(QStringLiteral("proxy"));
        } else {
            QJsonObject proxy{
                {QStringLiteral("host"), m_dlProxyHost->text().trimmed()},
                {QStringLiteral("username"), m_dlProxyUser->text().trimmed()},
                {QStringLiteral("password"), m_dlProxyPassword->value()},
            };
            // 0 в поле порта — порт по умолчанию: ключ не пишем, как у db_port других источников.
            if (m_dlProxyPort->value() > 0) proxy[QStringLiteral("port")] = m_dlProxyPort->value();
            s[QStringLiteral("proxy")] = proxy;
        }
        QJsonArray accounts;
        for (const auto &v : std::as_const(m_dlAccountList)) accounts.append(v);
        s[QStringLiteral("its_accounts")] = accounts;
        s[QStringLiteral("platform")] = QJsonObject{
            {QStringLiteral("start_version"), m_dlPlatformVersion->text().trimmed()},
            {QStringLiteral("its_account"), m_dlPlatformAccount->currentData().toString()},
        };
        QStringList distributions;
        for (const auto &d : m_dlDistributions->toPlainText().split('\n', Qt::SkipEmptyParts))
            if (!d.trimmed().isEmpty()) distributions << d.trimmed();
        s[QStringLiteral("releases")] = QJsonObject{
            {QStringLiteral("enabled"), m_dlReleasesEnabled->isChecked()},
            {QStringLiteral("its_account"), m_dlReleasesAccount->currentData().toString()},
            {QStringLiteral("count"), m_dlReleasesCount->value()},
            {QStringLiteral("projects"), toArray(util::splitList(m_dlReleasesProjects->text()))},
            {QStringLiteral("distributions"), toArray(distributions)},
        };
        QJsonArray configurations;
        for (int r = 0; r < m_dlConfigurations->rowCount(); ++r) {
            const QString human = tableCell(m_dlConfigurations, r, 0);
            const QString program = tableCell(m_dlConfigurations, r, 1);
            const QString version = tableCell(m_dlConfigurations, r, 2);
            const QString account = tableCell(m_dlConfigurations, r, 3);
            // Пустая строка таблицы — не конфигурация: в JSON её не пишем.
            if (human.isEmpty() && program.isEmpty() && version.isEmpty() && account.isEmpty()) continue;
            configurations.append(QJsonObject{
                {QStringLiteral("human_name"), human},
                {QStringLiteral("program_name"), program},
                {QStringLiteral("start_version"), version},
                {QStringLiteral("its_account"), account},
            });
        }
        s[QStringLiteral("configurations")] = configurations;
    } else if (st == "ms_sql") {
        m_sqlConn->apply(s);
        const QStringList dbs = dbChecked(m_sqlDatabases);
        // Одна база пишется и в прежнее поле: такое задание прочтёт и старая версия службы.
        s["database"] = dbs.size() == 1 ? dbs.first() : QString();
        s["databases"] = toArray(dbs);
        s["separate_archives"] = m_sqlArchives->currentData().toBool();
        s["kind"] = m_sqlKind->currentData().toString();
        s["backup_dir"] = m_sqlDir->text().trimmed();
        s["verify"] = m_sqlVerify->isChecked();
    } else if (st == "firebird") {
        s["gbak"] = m_fbGbak->text().trimmed();
        m_fbConn->apply(s);
        QStringList dbs;
        for (const auto &p : m_fbDatabases->toPlainText().split('\n', Qt::SkipEmptyParts))
            if (!p.trimmed().isEmpty()) dbs << p.trimmed();
        s["databases"] = toArray(dbs);
        s["names_from"] = optString(m_fbNamesFrom->text());
        s["path_template"] = optString(m_fbTemplate->text());
        s["host"] = optString(m_fbHost->text());
        s["verify"] = m_fbVerify->isChecked();
    } else if (st == "postgresql") {
        m_pgConn->apply(s);
        s["databases"] = toArray(dbChecked(m_pgDatabases));
        s["separate_archives"] = m_pgArchives->currentData().toBool();
        s[QStringLiteral("format")] = m_pgFormat->currentData().toString();
        s["pg_dump"] = optString(m_pgDump->text());
        s["globals"] = m_pgGlobals->isChecked();
        s["verify"] = m_pgVerify->isChecked();
    } else if (st == "mysql") {
        m_myConn->apply(s);
        s["db_host"] = optString(m_myHost->text());
        if (m_myPort->value() > 0)
            s["db_port"] = m_myPort->value();
        else
            s.remove(QStringLiteral("db_port"));
        s["db_user"] = m_myUser->text().trimmed();
        s["db_password"] = m_myPassword->value();
        s["databases"] = toArray(dbChecked(m_myDatabases));
        s["separate_archives"] = m_myArchives->currentData().toBool();
    } else if (st == "ssh_files") {
        m_sshConn->apply(s);
        QStringList paths;
        for (const auto &p : m_sshPaths->toPlainText().split('\n', Qt::SkipEmptyParts))
            if (!p.trimmed().isEmpty()) paths << p.trimmed();
        s["paths"] = toArray(paths);
        s["include"] = toArray(util::splitList(m_sshInclude->text()));
        s["exclude"] = toArray(util::splitList(m_sshExclude->text()));
        s["kind"] = m_sshKind->currentData().toString();
        s["full_every"] = m_sshFullEvery->value();
    } else if (st == "hyper_v") {
        s["vm_name"] = m_vmName->currentText().trimmed();
        s["kind"] = m_vmKind->currentData().toString();
        s["full_every"] = m_vmFullEvery->value();
        s["consistency"] = m_vmConsistency->currentData().toString();
    }
    if (st == QLatin1String("files") || st == QLatin1String("ssh_files")) {
        const QString mode = m_dateMode->currentData().toString();
        QJsonObject filter{
            {"field", m_dateField->currentData().toString()},
            {"mode", mode},
            {"within_hours", m_dateHours->value()},
        };
        if (mode == QLatin1String("since"))
            filter["since"] = m_dateSince->dateTime().toString("yyyy-MM-ddTHH:mm:ss");
        s["date_filter"] = filter;
    }
    job["source"] = s;
    job["destinations"] = m_destinations;

    job["archive"] = QJsonObject{
        {"enabled", m_arcEnabled->isChecked()},
        {"mode", m_arcMode->currentData().toString()},
        {"level", m_arcLevel->value()},
        {"password", m_arcPassword->value()},
        {"encrypt_headers", m_arcHeaders->isChecked()},
        {"volume_size", optString(m_arcVolume->text())},
        {"test_after", m_arcTest->isChecked()},
    };
    job["retention"] = QJsonObject{
        {"keep_full", m_keepFull->value()},
        {"keep_days", m_keepDays->value()},
        {"keep_sets", m_keepSets->value()},
    };

    const QString t = m_schedType->currentData().toString();
    QJsonObject sc{{"type", t}};
    if (t == "daily" || t == "weekly" || t == "monthly") sc["times"] = parseTimes(m_schedTimes->text());
    if (t == "weekly") {
        QJsonArray d;
        for (int i = 0; i < 7; ++i)
            if (m_weekDays[i]->isChecked()) d.append(QLatin1String(kDays[i]));
        sc["days"] = d;
    }
    if (t == "monthly") {
        QJsonArray d;
        for (const auto &x : util::splitList(m_monthDays->text())) {
            const int n = x.toInt();
            if (n >= 1 && n <= 31) d.append(n);
        }
        sc["days"] = d;
    }
    if (t == "interval") {
        sc["minutes"] = m_intervalMin->value();
        if (m_intervalWindow->isChecked()) {
            sc["from"] = m_intervalFrom->time().toString("HH:mm:ss");
            sc["to"] = m_intervalTo->time().toString("HH:mm:ss");
        }
    }
    if (t == "once") sc["at"] = m_onceAt->dateTime().toString("yyyy-MM-ddTHH:mm:ss");
    job["schedule"] = sc;
    job["run_missed"] = m_runMissed->isChecked();
    m_retry->store(job);
    if (m_timeoutMin->value() > 0)
        job["timeout_minutes"] = m_timeoutMin->value();
    else
        job.remove(QStringLiteral("timeout_minutes"));
    const QString group = m_group->currentData().toString();
    if (group.isEmpty())
        job.remove(QStringLiteral("group"));
    else
        job["group"] = group;
    return job;
}

QString JobDialog::validate(int *tab) const {
    auto at = [tab](int index, const QString &text) {
        if (tab) *tab = index;
        return text;
    };
    const QJsonObject j = value();
    if (j.value("name").toString().isEmpty()) return at(-1, tr("Укажите имя задания."));
    const QJsonObject s = j.value("source").toObject();
    const QString st = s.value("type").toString();
    if (st == "files" && s.value("paths").toArray().isEmpty()) return at(0, tr("Укажите хотя бы одну папку-источник."));
    if (st == "one_c" && s.value("ib_path").toString().isEmpty()) return at(0, tr("Укажите каталог базы 1С."));
    if (st == "one_c_update" && s.value("ib_path").toString().isEmpty())
        return at(0, tr("Укажите каталог файловой базы 1С или сервер и имя базы."));
    if (st == "one_c_update" && s.value("updates_dir").toString().isEmpty())
        return at(0, tr("Укажите папку с обновлениями конфигурации 1С."));
    if (st == "one_c_update" && s.value("backup_dir").toString().isEmpty())
        return at(0, tr("Укажите папку для резервной копии .dt."));
    if (st == "one_c_download" && s.value("template_dir").toString().isEmpty())
        return at(0, tr("Укажите папку для обновлений конфигураций 1С."));
    if (st == "one_c_download" && s.value("platform_dir").toString().isEmpty())
        return at(0, tr("Укажите папку для платформы и дистрибутивов 1С."));
    if (st == "one_c_download") {
        // Без логина ИТС скачивать нечего: служба не сможет войти на releases.1c.ru.
        bool hasLogin = false;
        for (const auto &v : s.value("its_accounts").toArray())
            if (!v.toObject().value("username").toString().trimmed().isEmpty()) {
                hasLogin = true;
                break;
            }
        if (!hasLogin)
            return at(0, tr("Добавьте хотя бы одну учётную запись ИТС с логином — без неё обновления "
                            "с releases.1c.ru не скачиваются."));
    }
    if (st == "ms_sql" && ((s.value("server").toString().isEmpty() && s.value("connection").toString().isEmpty()) ||
                           s.value("backup_dir").toString().isEmpty()))
        return at(0, tr("Укажите сервер и папку для .bak."));
    if (st == "firebird" && (s.value("gbak").toString().isEmpty() ||
                             (s.value("databases").toArray().isEmpty() && s.value("names_from").toString().isEmpty())))
        return at(0, tr("Укажите gbak.exe и хотя бы одну базу или папку имён."));
    if (st == "firebird" && s.value("connection").toString().isEmpty() && s.value("user").toString().isEmpty())
        return at(0, tr("Выберите подключение Firebird. Если списка нет — «Подключения…», тип «Firebird (SYSDBA)»."));
    if (st == "postgresql" && s.value("connection").toString().isEmpty() && s.value("user").toString().isEmpty() &&
        s.value("host").toString().isEmpty())
        return at(0, tr("Выберите подключение PostgreSQL. Если списка нет — «Подключения…», тип «PostgreSQL»."));
    const bool noSsh = s.value("connection").toString().isEmpty() && !s.value("ssh").isObject();
    if ((st == "mysql" || st == "ssh_files") && noSsh)
        return at(0, tr("Выберите сервер SSH. Если списка нет — «Подключения…», тип «SSH (сервер сайта)»."));
    if (st == "mysql" && s.value("db_user").toString().isEmpty()) return at(0, tr("Укажите пользователя базы данных."));
    if (st == "ssh_files" && s.value("paths").toArray().isEmpty())
        return at(0, tr("Укажите хотя бы один каталог на сервере."));
    if (st == "hyper_v" && s.value("vm_name").toString().isEmpty()) return at(0, tr("Укажите имя виртуальной машины."));
    if (st != "one_c_update" && st != "one_c_download" && j.value("destinations").toArray().isEmpty())
        return at(1, tr("Добавьте хотя бы одно назначение."));
    const QJsonObject sc = j.value("schedule").toObject();
    const QString t = sc.value("type").toString();
    if ((t == "daily" || t == "weekly" || t == "monthly") && sc.value("times").toArray().isEmpty())
        return at(3, tr("Укажите время запуска в формате ЧЧ:ММ."));
    if (t == "weekly" && sc.value("days").toArray().isEmpty()) return at(3, tr("Отметьте дни недели."));
    if (t == "monthly" && sc.value("days").toArray().isEmpty()) return at(3, tr("Укажите числа месяца."));
    return {};
}

// ---------------- Список баз ----------------

QStringList JobDialog::dbChecked(const QListWidget *list) {
    QStringList l;
    for (int i = 0; i < list->count(); ++i)
        if (list->item(i)->checkState() == Qt::Checked) l << list->item(i)->text();
    return l;
}

void JobDialog::setDbList(QListWidget *list, const QStringList &checked, const QStringList &available) {
    QStringList all = available;
    // Отмеченные базы, которых нет на сервере (удалена, переименована), не теряем: их видно и можно снять.
    for (const auto &c : checked)
        if (!all.contains(c)) all << c;
    list->clear();
    for (const auto &n : std::as_const(all)) {
        auto *it = new QListWidgetItem(n, list);
        it->setFlags(it->flags() | Qt::ItemIsUserCheckable);
        it->setCheckState(checked.contains(n) ? Qt::Checked : Qt::Unchecked);
        if (!available.isEmpty() && !available.contains(n)) it->setToolTip(tr("нет на сервере"));
    }
}

QWidget *JobDialog::dbButtons(QListWidget *list, const QString &type) {
    auto *refresh = new QPushButton(tr("Проверить и обновить"));
    connect(refresh, &QPushButton::clicked, this, [this, type] { loadDatabases(type, false); });
    auto *add = new QPushButton(tr("Добавить вручную…"));
    connect(add, &QPushButton::clicked, this, [this, list] {
        bool ok = false;
        const QString n =
            QInputDialog::getText(this, tr("База данных"), tr("Имя базы:"), QLineEdit::Normal, {}, &ok).trimmed();
        if (!ok || n.isEmpty()) return;
        QStringList checked = dbChecked(list);
        if (!checked.contains(n)) checked << n;
        QStringList all;
        for (int i = 0; i < list->count(); ++i) all << list->item(i)->text();
        setDbList(list, checked, all);
    });
    auto *toggle = new QPushButton(tr("Отметить все / снять"));
    toggle->setToolTip(tr("Ничего не отмечено — копируются все базы сервера, включая новые."));
    connect(toggle, &QPushButton::clicked, this, [list] {
        const bool anyUnchecked = [&] {
            for (int i = 0; i < list->count(); ++i)
                if (list->item(i)->checkState() != Qt::Checked) return true;
            return false;
        }();
        for (int i = 0; i < list->count(); ++i)
            list->item(i)->setCheckState(anyUnchecked ? Qt::Checked : Qt::Unchecked);
    });
    auto *w = new QWidget;
    auto *l = new QHBoxLayout(w);
    l->setContentsMargins(0, 0, 0, 0);
    l->addWidget(refresh);
    l->addWidget(add);
    l->addWidget(toggle);
    l->addStretch();
    return w;
}

QComboBox *JobDialog::makeArchiveMode() {
    auto *c = new QComboBox;
    c->addItem(tr("Все базы в одном архиве"), false);
    c->addItem(tr("Каждая база в отдельном архиве"), true);
    c->setToolTip(tr("Отдельные архивы называются <задание>_<дата>_<время>_<вид>.<база>.7z; удобно восстанавливать "
                     "одну базу. Ротация удаляет архивы одного запуска вместе."));
    return c;
}

void JobDialog::loadDatabases(const QString &type, bool automatic) {
    const bool sql = type == QLatin1String("ms_sql");
    const bool my = type == QLatin1String("mysql");
    QLabel *status = sql ? m_sqlStatus : (my ? m_myStatus : m_pgStatus);
    QListWidget *list = sql ? m_sqlDatabases : (my ? m_myDatabases : m_pgDatabases);
    // Источник из текущего состояния формы: служба сама подставит учётные данные подключения.
    QJsonObject s = value().value("source").toObject();
    if (s.value("type").toString() != type) return;
    const bool hasConn = !s.value("connection").toString().isEmpty();
    const bool hasInline = my ? s.value("ssh").isObject() : !s.value(sql ? "server" : "host").toString().isEmpty();
    if (!hasConn && !hasInline) {
        if (!automatic) {
            status->setText(tr("Выберите подключение."));
            theme::setState(status, theme::Warn);
        }
        return;
    }
    // MySQL: без пользователя БД проверять нечего; при смене SSH-подключения молча ждём ввода.
    if (my && s.value("db_user").toString().isEmpty()) {
        if (!automatic) {
            status->setText(tr("Укажите пользователя базы данных."));
            theme::setState(status, theme::Warn);
        }
        return;
    }
    const int seq = ++m_dbSeq;
    status->setText(tr("Подключение к серверу…"));
    theme::setState(status, theme::Muted);
    m_client->call({{"cmd", "list_databases"}, {"source", s}},
                   [this, list, status, seq](bool ok, const QJsonValue &r, const QString &err) {
        if (seq != m_dbSeq) return;
        if (!ok) {
            status->setText(tr("Ошибка подключения: %1").arg(err));
            theme::setState(status, theme::Error);
            return;
        }
        QStringList names;
        for (const auto &v : r.toArray()) names << v.toString();
        setDbList(list, dbChecked(list), names);
        QStringList missing;
        for (const auto &c : dbChecked(list))
            if (!names.contains(c)) missing << c;
        if (!missing.isEmpty()) {
            status->setText(tr("Подключение работает, но на сервере нет баз: %1.").arg(missing.join(", ")));
            theme::setState(status, theme::Warn);
            return;
        }
        status->setText(tr("Подключение работает, баз на сервере: %1.").arg(names.size()));
        theme::setState(status, theme::Ok);
    }, this);
}

void JobDialog::checkSshPaths() {
    const QJsonObject s = value().value("source").toObject();
    if (s.value("connection").toString().isEmpty() && !s.value("ssh").isObject()) {
        m_sshStatus->setText(tr("Выберите сервер SSH."));
        theme::setState(m_sshStatus, theme::Warn);
        return;
    }
    m_sshStatus->setText(tr("Подключение к серверу…"));
    theme::setState(m_sshStatus, theme::Muted);
    m_client->call({{"cmd", "check_ssh_paths"}, {"source", s}}, [this](bool ok, const QJsonValue &r, const QString &err) {
        if (!ok) {
            m_sshStatus->setText(tr("Ошибка: %1").arg(err));
            theme::setState(m_sshStatus, theme::Error);
            return;
        }
        const QJsonObject o = r.toObject();
        QStringList bad;
        int found = 0;
        for (const auto &v : o.value("paths").toArray()) {
            const QJsonObject p = v.toObject();
            if (p.value("ok").toBool()) ++found;
            else bad << p.value("path").toString();
        }
        QString t = bad.isEmpty() ? tr("✔ %1: найдено каталогов %2.").arg(o.value("target").toString()).arg(found)
                                  : tr("Подключение работает, но не найдено: %1.").arg(bad.join(", "));
        const QString fp = o.value("host_key").toString();
        if (!fp.isEmpty())
            t += tr("\nОтпечаток сервера не закреплён: %1 — впишите его в подключение SSH.").arg(fp);
        m_sshStatus->setText(t);
        m_sshStatus->setTextInteractionFlags(Qt::TextSelectableByMouse);
        theme::setState(m_sshStatus, bad.isEmpty() ? theme::Ok : theme::Warn);
    }, this);
}

void JobDialog::refreshItsAccounts() {
    // Выбранные учётки сохраняем: список мог измениться из-за правки другой записи.
    const QString prevPlatform = m_dlPlatformAccount->currentData().toString();
    const QString prevReleases = m_dlReleasesAccount->currentData().toString();
    m_dlAccounts->clear();
    for (const auto &v : std::as_const(m_dlAccountList)) {
        const QJsonObject a = v.toObject();
        QString text = a.value("name").toString();
        if (text.isEmpty()) text = tr("без имени");
        const QString user = a.value("username").toString();
        if (!user.isEmpty()) text += QStringLiteral("  ·  ") + user;
        text += a.value("password").toString().isEmpty() ? tr("  ·  пароль не задан")
                                                         : tr("  ·  пароль задан");
        m_dlAccounts->addItem(text);
    }
    // Пустой пункт — «первая учётка в списке»: служба берёт её сама.
    const auto fill = [this](QComboBox *combo, const QString &noneText, const QString &prev) {
        combo->clear();
        combo->addItem(noneText, QString());
        for (const auto &v : std::as_const(m_dlAccountList)) {
            const QString name = v.toObject().value("name").toString();
            if (!name.isEmpty() && combo->findData(name) < 0) combo->addItem(name, name);
        }
        const int i = combo->findData(prev);
        combo->setCurrentIndex(i >= 0 ? i : 0);
    };
    fill(m_dlPlatformAccount, tr("первая учётка"), prevPlatform);
    fill(m_dlReleasesAccount, tr("первая из списка"), prevReleases);
    updateItsPickButton();
}

void JobDialog::updateItsPickButton() {
    if (!m_dlItsPick) return;
    // Без логина служба не войдёт на releases.1c.ru и список запрашивать нечего.
    bool hasLogin = false;
    for (const auto &v : std::as_const(m_dlAccountList))
        if (!v.toObject().value("username").toString().trimmed().isEmpty()) {
            hasLogin = true;
            break;
        }
    m_dlItsPick->setEnabled(hasLogin);
    m_dlItsPick->setToolTip(hasLogin
                                ? tr("Запросить у releases.1c.ru конфигурации, доступные учётной записи ИТС, "
                                     "и добавить отмеченные в задание.")
                                : tr("Сначала добавьте учётную запись ИТС"));
}

void JobDialog::chooseItsConfigurations() {
    // Учётки, прокси и insecure_tls — из текущего состояния формы задания.
    const QJsonObject source = value().value("source").toObject();
    if (source.value("type").toString() != QLatin1String("one_c_download")) return;
    QStringList present;
    for (int r = 0; r < m_dlConfigurations->rowCount(); ++r) {
        const QString program = tableCell(m_dlConfigurations, r, 1);
        if (!program.isEmpty()) present << program;
    }
    QJsonObject proxy;
    bool proxyPasswordHidden = false;
    const QJsonValue proxyValue = source.value("proxy");
    if (proxyValue.isObject()) {
        proxy = proxyValue.toObject();
        const QString password = proxy.value("password").toString();
        // Пароль прокси в задании — только маркер: форме он недоступен, в запрос не отправляем
        // (иначе служба ругалась бы на base64). Диалог предупредит об этом.
        if (password == QLatin1String("dpapi:stored")) {
            proxy.remove(QStringLiteral("password"));
            proxyPasswordHidden = true;
        } else if (password.isEmpty()) {
            proxy.remove(QStringLiteral("password"));
        }
    }
    ItsConfigurationsDialog dlg(m_client, source.value("its_accounts").toArray(), present, proxy,
                                m_dlInsecure->isChecked(), proxyPasswordHidden, this);
    if (dlg.exec() != QDialog::Accepted) return;
    const QJsonArray picked = dlg.selected();
    int firstRow = -1;
    for (const auto &v : picked) {
        const QJsonObject c = v.toObject();
        const QString program = c.value("program_name").toString();
        if (program.isEmpty() || present.contains(program)) continue; // дубликатов в задании не бывает
        const int r = m_dlConfigurations->rowCount();
        m_dlConfigurations->insertRow(r);
        auto *humanItem = new QTableWidgetItem(c.value("human_name").toString());
        humanItem->setToolTip(QStringLiteral("program_name: %1").arg(program));
        auto *versionItem = new QTableWidgetItem(c.value("start_version").toString());
        versionItem->setToolTip(tr("Версия на портале. Замените её на текущую версию базы — иначе обновление "
                                   "не будет найдено."));
        m_dlConfigurations->setItem(r, 0, humanItem);
        m_dlConfigurations->setItem(r, 1, new QTableWidgetItem(program));
        m_dlConfigurations->setItem(r, 2, versionItem);
        m_dlConfigurations->setItem(r, 3, new QTableWidgetItem(c.value("its_account").toString()));
        if (firstRow < 0) firstRow = r;
        present << program;
    }
    // Курсор — в колонку версии первой добавленной строки: её и нужно заменить на версию базы.
    if (firstRow >= 0) m_dlConfigurations->setCurrentCell(firstRow, 2);
}

void JobDialog::saveShots(const QString &dir, const QString &prefix) {
    for (int i = 0; i < m_tabs->count(); ++i) {
        m_tabs->setCurrentIndex(i);
        QApplication::processEvents();
        grab().save(QStringLiteral("%1/%2-%3.png").arg(dir, prefix).arg(i + 1));
    }
    m_tabs->setCurrentIndex(0);
}

void JobDialog::accept() {
    int tab = -1;
    const QString err = validate(&tab);
    if (!err.isEmpty()) {
        // Показываем вкладку, где не хватает данных: иначе сообщение есть, а поля не видно.
        if (tab >= 0) m_tabs->setCurrentIndex(tab);
        else m_name->setFocus();
        QMessageBox::warning(this, windowTitle(), err);
        return;
    }
    QDialog::accept();
}
