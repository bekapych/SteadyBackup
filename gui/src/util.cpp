#include "util.h"

#include <QAbstractItemView>
#include <QBoxLayout>
#include <QDateTime>
#include <QDialog>
#include <QDialogButtonBox>
#include <QEvent>
#include <QGuiApplication>
#include <QJsonArray>
#include <QPainter>
#include <QPainterPath>
#include <QPalette>
#include <QPixmap>
#include <QRegularExpression>
#include <QScreen>
#include <QScrollArea>
#include <QTabWidget>
#include <QTimer>
#include <QVBoxLayout>

void SteadyCombo::showPopup() {
    setEnabled(true);
    QComboBox::showPopup();
    QWidget *popup = view() ? view()->window() : nullptr;
    if (!popup) return;
    if (popup->isVisible()) {
        popup->raise();
        return;
    }
    // Список, созданный до показа окна, на Windows остаётся за модальным заданием.
    popup->setWindowFlags(Qt::Popup | Qt::FramelessWindowHint | Qt::NoDropShadowWindowHint);
    popup->move(mapToGlobal(QPoint(0, height())));
    const int rows = qMax(1, count());
    const int rowH = qMax(24, fontMetrics().height() + 10);
    popup->resize(qMax(width(), 240), qMin(320, rowH * rows + 8));
    popup->show();
    popup->raise();
}

namespace util {

QIcon appIcon(const QColor &accent) {
    QIcon icon;
    for (int size : {16, 24, 32, 48, 64, 256}) {
        QPixmap pm(size, size);
        pm.fill(Qt::transparent);
        QPainter p(&pm);
        p.setRenderHint(QPainter::Antialiasing);
        const qreal s = size;
        // Щит
        QPainterPath shield;
        shield.moveTo(s * 0.5, s * 0.04);
        shield.lineTo(s * 0.9, s * 0.18);
        shield.cubicTo(s * 0.9, s * 0.62, s * 0.72, s * 0.84, s * 0.5, s * 0.97);
        shield.cubicTo(s * 0.28, s * 0.84, s * 0.1, s * 0.62, s * 0.1, s * 0.18);
        shield.closeSubpath();
        p.setPen(Qt::NoPen);
        p.setBrush(accent);
        p.drawPath(shield);
        // Стрелка по кругу
        QPen pen(Qt::white, qMax(1.5, s * 0.08), Qt::SolidLine, Qt::RoundCap);
        p.setPen(pen);
        p.setBrush(Qt::NoBrush);
        const QRectF r(s * 0.3, s * 0.28, s * 0.4, s * 0.4);
        p.drawArc(r, 60 * 16, 280 * 16);
        QPainterPath head;
        head.moveTo(s * 0.62, s * 0.22);
        head.lineTo(s * 0.68, s * 0.33);
        head.lineTo(s * 0.56, s * 0.34);
        p.setBrush(Qt::white);
        p.setPen(Qt::NoPen);
        p.drawPath(head);
        p.end();
        icon.addPixmap(pm);
    }
    return icon;
}

QString sourceTitle(const QString &type) {
    if (type == QLatin1String("files")) return QObject::tr("Файлы");
    if (type == QLatin1String("one_c")) return QObject::tr("1С (файловая)");
    if (type == QLatin1String("one_c_update")) return QObject::tr("Обновление конфигурации 1С");
    if (type == QLatin1String("ms_sql")) return QObject::tr("MS SQL");
    if (type == QLatin1String("hyper_v")) return QObject::tr("Hyper-V");
    if (type == QLatin1String("firebird")) return QObject::tr("Firebird");
    if (type == QLatin1String("postgresql")) return QObject::tr("PostgreSQL");
    if (type == QLatin1String("mysql")) return QObject::tr("MySQL (SSH)");
    if (type == QLatin1String("ssh_files")) return QObject::tr("Файлы (SSH)");
    return type;
}

QString destinationSummary(const QJsonObject &d) {
    const QString t = d.value("type").toString();
    if (t == "local") return QObject::tr("Папка: %1").arg(d.value("path").toString());
    if (t == "smb") return QObject::tr("Сеть: %1").arg(d.value("path").toString());
    if (t == "sftp")
        return QObject::tr("SFTP: %1@%2:%3%4")
            .arg(d.value("user").toString(), d.value("host").toString())
            .arg(d.value("port").toInt(22))
            .arg(d.value("path").toString());
    if (t == "web_dav") return QObject::tr("WebDAV: %1").arg(d.value("url").toString());
    if (t == "google_drive") {
        QString who = d.value("email").toString();
        if (who.isEmpty()) who = d.value("account").toString();
        return QObject::tr("Google Drive%1: /%2").arg(who.isEmpty() ? QString() : " (" + who + ")", d.value("folder").toString());
    }
    if (t == "storage") {
        const QString name = d.value("storage_name").toString();
        const QString folder = d.value("folder").toString();
        return (name.isEmpty() ? d.value("storage").toString() : name) + (folder.isEmpty() ? QString() : " / " + folder);
    }
    return t;
}

static QString times(const QJsonObject &s) {
    QStringList v;
    for (const auto &t : s.value("times").toArray())
        v << t.toString().left(5);
    return v.join(", ");
}

QString scheduleSummary(const QJsonObject &s) {
    const QString t = s.value("type").toString();
    if (t.isEmpty() || t == "manual") return QObject::tr("вручную");
    if (t == "once") return QObject::tr("однократно %1").arg(formatDateTime(s.value("at").toString()));
    if (t == "daily") return QObject::tr("ежедневно в %1").arg(times(s));
    if (t == "weekly") {
        static const QHash<QString, QString> names{{"Mon", "пн"}, {"Tue", "вт"}, {"Wed", "ср"}, {"Thu", "чт"},
                                                   {"Fri", "пт"}, {"Sat", "сб"}, {"Sun", "вс"}};
        QStringList d;
        for (const auto &x : s.value("days").toArray())
            d << names.value(x.toString(), x.toString());
        return QObject::tr("по %1 в %2").arg(d.join(", "), times(s));
    }
    if (t == "monthly") {
        QStringList d;
        for (const auto &x : s.value("days").toArray())
            d << QString::number(x.toInt());
        return QObject::tr("ежемесячно %1 числа в %2").arg(d.join(", "), times(s));
    }
    if (t == "interval") {
        QString r = QObject::tr("каждые %1 мин").arg(s.value("minutes").toInt());
        if (s.value("from").isString() && s.value("to").isString())
            r += QObject::tr(" с %1 до %2").arg(s.value("from").toString().left(5), s.value("to").toString().left(5));
        return r;
    }
    return t;
}

QString formatBytes(qint64 b) {
    const char *units[] = {"Б", "КБ", "МБ", "ГБ", "ТБ"};
    double v = b;
    int i = 0;
    while (v >= 1024 && i < 4) {
        v /= 1024;
        ++i;
    }
    return QString::number(v, 'f', i ? 1 : 0) + ' ' + QString::fromUtf8(units[i]);
}

QDateTime parseIso(const QString &iso) {
    if (iso.isEmpty()) return {};
    QDateTime dt = QDateTime::fromString(iso, Qt::ISODateWithMs);
    if (!dt.isValid()) dt = QDateTime::fromString(iso.left(19), Qt::ISODate);
    if (!dt.isValid()) return {};
    if (iso.contains('+') || iso.endsWith('Z')) dt = dt.toLocalTime();
    return dt;
}

QString formatDateTime(const QString &iso) {
    if (iso.isEmpty()) return QStringLiteral("—");
    const QDateTime dt = parseIso(iso);
    if (!dt.isValid()) return iso;
    return dt.toString(QStringLiteral("dd.MM.yyyy HH:mm"));
}

QString formatWhen(const QString &iso) {
    if (iso.isEmpty()) return QStringLiteral("—");
    const QDateTime dt = parseIso(iso);
    if (!dt.isValid()) return iso;
    const qint64 days = QDate::currentDate().daysTo(dt.date());
    const QString time = dt.toString(QStringLiteral("HH:mm"));
    if (days == 0) return QObject::tr("сегодня, %1").arg(time);
    if (days == -1) return QObject::tr("вчера, %1").arg(time);
    if (days == 1) return QObject::tr("завтра, %1").arg(time);
    return dt.toString(QStringLiteral("dd.MM.yyyy HH:mm"));
}

QString formatDuration(qint64 secs) {
    if (secs < 0) secs = 0;
    const QLatin1Char z('0');
    if (secs >= 3600)
        return QStringLiteral("%1:%2:%3").arg(secs / 3600).arg(secs / 60 % 60, 2, 10, z).arg(secs % 60, 2, 10, z);
    return QStringLiteral("%1:%2").arg(secs / 60).arg(secs % 60, 2, 10, z);
}

QStringList splitList(const QString &text) {
    QStringList out;
    for (const auto &p : text.split(QRegularExpression(QStringLiteral("[;,\\n]")), Qt::SkipEmptyParts)) {
        const QString t = p.trimmed();
        if (!t.isEmpty()) out << t;
    }
    return out;
}

static QJsonArray g_storages;

void setStorageCache(const QJsonArray &list) {
    if (!list.isEmpty()) g_storages = list;
}

QJsonArray storageCache() { return g_storages; }

QJsonValue optString(const QString &s) {
    const QString t = s.trimmed();
    return t.isEmpty() ? QJsonValue(QJsonValue::Null) : QJsonValue(t);
}

static QRect availableFor(QWidget *w) {
    QScreen *scr = w->screen();
    if (!scr) scr = QGuiApplication::primaryScreen();
    if (!scr) return {};
    return scr->availableGeometry().adjusted(8, 8, -8, -8);
}

void fitToScreen(QWidget *w) {
    const QRect avail = availableFor(w);
    if (!avail.isValid() || avail.height() < 120) return;
    // Запас на заголовок окна: availableGeometry его не включает.
    const int frameH = qMax(32, w->isVisible() ? w->frameGeometry().height() - w->height() : 40);
    const int frameW = qMax(8, w->isVisible() ? w->frameGeometry().width() - w->width() : 16);
    const QSize cap(qMax(200, avail.width() - frameW), qMax(120, avail.height() - frameH));
    w->setMaximumSize(cap);
    const QSize minSize(qMin(360, cap.width()), qMin(220, cap.height()));
    w->setMinimumSize(minSize);
    QSize want = w->size();
    if (want.width() < 200 || want.height() < 100) want = w->sizeHint();
    want = want.boundedTo(cap).expandedTo(minSize);
    if (w->size() != want) w->resize(want);
    if (!w->isWindow()) return;
    const int x = qBound(avail.left(), w->x(), qMax(avail.left(), avail.right() - w->width()));
    const int y = qBound(avail.top(), w->y(), qMax(avail.top(), avail.bottom() - w->height() - frameH));
    if (w->pos() != QPoint(x, y)) w->move(x, y);
}

namespace {

class ScreenFit : public QObject {
public:
    explicit ScreenFit(QWidget *w) : QObject(w) { w->installEventFilter(this); }
    bool eventFilter(QObject *o, QEvent *e) override {
        if (e->type() == QEvent::Show) {
            // exec() центрирует окно уже после Show. Подгоняем, когда позиция окончательная.
            auto *w = static_cast<QWidget *>(o);
            QTimer::singleShot(0, w, [w] { fitToScreen(w); });
        }
        return false;
    }
};

void placeItem(QVBoxLayout *to, QLayoutItem *item) {
    if (QWidget *w = item->widget()) {
        const int stretch = qobject_cast<QTabWidget *>(w) ? 1 : 0;
        to->addWidget(w, stretch);
        delete item;
        return;
    }
    if (QLayout *l = item->layout()) {
        to->addLayout(l);
        if (item != l) delete item;
        return;
    }
    if (QSpacerItem *s = item->spacerItem()) {
        to->addSpacerItem(s);
        return;
    }
    to->addItem(item);
}

} // namespace

void keepButtonsOnScreen(QDialog *dlg) {
    auto *root = qobject_cast<QBoxLayout *>(dlg->layout());
    QDialogButtonBox *buttons = nullptr;
    if (root) {
        QList<QLayoutItem *> items;
        while (root->count() > 0) items.append(root->takeAt(0));
        QLayoutItem *buttonItem = nullptr;
        for (auto *item : items) {
            if (auto *w = item->widget()) {
                if (auto *b = qobject_cast<QDialogButtonBox *>(w)) {
                    buttons = b;
                    buttonItem = item;
                }
            }
        }
        if (buttons) {
            auto *content = new QWidget(dlg);
            auto *cv = new QVBoxLayout(content);
            cv->setContentsMargins(0, 0, 0, 0);
            for (auto *item : items) {
                if (item != buttonItem) placeItem(cv, item);
            }
            auto *scroll = new QScrollArea(dlg);
            scroll->setWidgetResizable(true);
            scroll->setFrameShape(QFrame::NoFrame);
            scroll->setHorizontalScrollBarPolicy(Qt::ScrollBarAsNeeded);
            scroll->setVerticalScrollBarPolicy(Qt::ScrollBarAsNeeded);
            scroll->viewport()->setBackgroundRole(QPalette::Window);
            content->setAutoFillBackground(false);
            scroll->setWidget(content);
            scroll->setMinimumHeight(80);
            root->addWidget(scroll, 1);
            root->addWidget(buttons);
            delete buttonItem;
            // Иначе раскладка не даст окну стать ниже суммы полей, и кнопки уедут за экран.
            root->setSizeConstraint(QLayout::SetNoConstraint);
        } else {
            for (auto *item : items) root->addItem(item);
        }
    }
    new ScreenFit(dlg);
    fitToScreen(dlg);
}

} // namespace util
