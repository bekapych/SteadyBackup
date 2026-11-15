#include "theme.h"

#include <QApplication>
#include <QFontMetrics>
#include <QGuiApplication>
#include <QHash>
#include <QLabel>
#include <QList>
#include <QPair>
#include <QPainter>
#include <QPalette>
#include <QStyle>
#include <QStyleFactory>
#include <QStyleHints>
#include <QStyleOptionViewItem>
#include <QSvgRenderer>

namespace {

struct Colors {
    const char *bg, *surface, *field, *border, *hover, *text, *muted, *accent, *accentHover, *sel;
    const char *ok, *okBg, *warn, *warnBg, *err, *errBg, *info, *infoBg, *mutedBg;
};

const Colors kLight{"#F3F5F8", "#FFFFFF", "#FFFFFF", "#D5DAE1", "#EBEEF2", "#1B2330", "#667085", "#1F8A4C", "#1A7541", "#DDF0E5",
                    "#1B7F3B", "#E1F4E7", "#A45A00", "#FCEFD6", "#C62828", "#FBE4E4", "#1565C0", "#E2EEFB", "#ECEFF3"};
const Colors kDark{"#16191E", "#1F232A", "#191C22", "#363C47", "#2A2F38", "#E6E9EE", "#9AA4B2", "#2F9E63", "#38B273", "#244233",
                   "#62CE90", "#1C3527", "#F0B35A", "#3B2E17", "#F2857F", "#3F2222", "#74B4F5", "#1B2E45", "#2A2F38"};

const Colors &pal() { return theme::isDark() ? kDark : kLight; }

QColor col(const char *hex) { return QColor(QString::fromLatin1(hex)); }

const char *kQss = R"(
QMainWindow, QDialog { background: @bg; }
QToolTip { background: @surface; color: @text; border: 1px solid @border; padding: 4px 6px; }
QLabel[hint="true"] { color: @muted; }

#header { background: @surface; border-bottom: 1px solid @border; }
#appTitle { font-size: 13pt; font-weight: 600; }
#sectionTitle { font-size: 11pt; font-weight: 600; }
#emptyTitle { font-size: 12pt; font-weight: 600; }

QFrame#card { background: @surface; border: 1px solid @border; border-radius: 10px; }
#cardTitle { color: @muted; }
#cardValue { font-size: 13pt; font-weight: 600; }
#cardSub { color: @muted; }

QFrame#banner { background: @infoBg; border: 1px solid @info; border-radius: 8px; }
QFrame#banner[kind="error"] { background: @errBg; border: 1px solid @err; }

QPushButton { background: @surface; border: 1px solid @border; border-radius: 6px; padding: 5px 14px; min-height: 20px; }
QPushButton:hover { background: @hover; }
QPushButton:pressed { background: @border; }
QPushButton:focus { border: 1px solid @accent; }
QPushButton:disabled { color: @muted; background: @bg; }
QPushButton#primary { background: @accent; border: 1px solid @accent; color: #FFFFFF; font-weight: 600; }
QPushButton#primary:hover { background: @accentHover; border: 1px solid @accentHover; }
QPushButton#primary:disabled { background: @border; border: 1px solid @border; color: @muted; }

QToolButton { background: transparent; border: 1px solid transparent; border-radius: 6px; padding: 5px 10px; }
QToolButton:hover { background: @hover; border: 1px solid @border; }
QToolButton:pressed { background: @border; }
QToolButton:disabled { color: @muted; }
QToolButton::menu-indicator { image: none; }

QLineEdit, QPlainTextEdit, QSpinBox, QTimeEdit, QDateTimeEdit, QComboBox {
    background: @field; border: 1px solid @border; border-radius: 6px; padding: 4px 8px; min-height: 20px;
    selection-background-color: @accent; selection-color: #FFFFFF;
}
QPlainTextEdit { padding: 4px; }
QLineEdit:focus, QPlainTextEdit:focus, QSpinBox:focus, QTimeEdit:focus, QDateTimeEdit:focus, QComboBox:focus { border: 1px solid @accent; }
QLineEdit:disabled, QPlainTextEdit:disabled, QSpinBox:disabled, QTimeEdit:disabled, QDateTimeEdit:disabled, QComboBox:disabled {
    color: @muted; background: @bg;
}
QComboBox::drop-down, QDateTimeEdit::drop-down { subcontrol-origin: padding; subcontrol-position: center right; width: 24px; border: none; }
QComboBox::down-arrow, QDateTimeEdit::down-arrow { image: url(:/res/chevron-down-@mode.svg); width: 12px; height: 12px; }
QComboBox QAbstractItemView {
    background: @surface; border: 1px solid @border; outline: 0; padding: 2px;
    selection-background-color: @sel; selection-color: @text;
}
QSpinBox::up-button, QTimeEdit::up-button, QDateTimeEdit::up-button {
    subcontrol-origin: border; subcontrol-position: top right; width: 20px; border: none; margin: 2px 2px 0 0;
}
QSpinBox::down-button, QTimeEdit::down-button, QDateTimeEdit::down-button {
    subcontrol-origin: border; subcontrol-position: bottom right; width: 20px; border: none; margin: 0 2px 2px 0;
}
QSpinBox::up-arrow, QTimeEdit::up-arrow, QDateTimeEdit::up-arrow { image: url(:/res/chevron-up-@mode.svg); width: 9px; height: 9px; }
QSpinBox::down-arrow, QTimeEdit::down-arrow { image: url(:/res/chevron-down-@mode.svg); width: 9px; height: 9px; }
QCheckBox { spacing: 8px; }

QTabWidget::pane { border: none; border-top: 1px solid @border; top: -1px; }
QTabBar::tab { background: transparent; border: none; border-bottom: 2px solid transparent; padding: 8px 14px; margin-right: 2px; color: @muted; }
QTabBar::tab:hover { color: @text; }
QTabBar::tab:selected { color: @text; border-bottom: 2px solid @accent; }

QGroupBox { border: 1px solid @border; border-radius: 8px; margin-top: 10px; padding: 12px 8px 8px 8px; }
QGroupBox::title { subcontrol-origin: margin; subcontrol-position: top left; left: 10px; padding: 0 4px; }

QTableView, QListWidget {
    background: @surface; border: 1px solid @border; border-radius: 6px; gridline-color: transparent; outline: 0;
    selection-background-color: @sel; selection-color: @text;
}
QTableView::item { padding: 0 8px; border: none; border-bottom: 1px solid @hover; }
QTableView::item:selected { background: @sel; color: @text; }
QListWidget::item { padding: 7px 8px; border-bottom: 1px solid @hover; }
QListWidget::item:selected { background: @sel; color: @text; }
QHeaderView { background: transparent; }
QHeaderView::section {
    background: @surface; color: @muted; border: none; border-bottom: 1px solid @border; padding: 7px 8px; font-weight: 600;
}
QTableCornerButton::section { background: @surface; border: none; }

QScrollBar:vertical { background: transparent; width: 12px; margin: 2px; }
QScrollBar::handle:vertical { background: @border; border-radius: 4px; min-height: 28px; }
QScrollBar::handle:vertical:hover { background: @muted; }
QScrollBar:horizontal { background: transparent; height: 12px; margin: 2px; }
QScrollBar::handle:horizontal { background: @border; border-radius: 4px; min-width: 28px; }
QScrollBar::handle:horizontal:hover { background: @muted; }
QScrollBar::add-line, QScrollBar::sub-line { width: 0; height: 0; }
QScrollBar::add-page, QScrollBar::sub-page { background: transparent; }

QMenu { background: @surface; border: 1px solid @border; padding: 5px; }
QMenu::item { padding: 7px 28px 7px 14px; border-radius: 5px; }
QMenu::item:selected { background: @sel; color: @text; }
QMenu::item:disabled { color: @muted; }
QMenu::separator { height: 1px; background: @border; margin: 5px 8px; }

QStatusBar { background: @surface; border-top: 1px solid @border; }
QStatusBar::item { border: none; }
QSplitter::handle:vertical { height: 10px; background: transparent; }
)";

const QHash<QString, QString> &iconPaths() {
    static const QHash<QString, QString> h{
        {QStringLiteral("plus"), QStringLiteral("<path d='M12 5v14M5 12h14'/>")},
        {QStringLiteral("play"), QStringLiteral("<path d='M8 5.5v13l10.5-6.5z' fill='{c}'/>")},
        {QStringLiteral("stop"), QStringLiteral("<rect x='6.5' y='6.5' width='11' height='11' rx='2' fill='{c}'/>")},
        {QStringLiteral("edit"), QStringLiteral("<path d='M4 20h4L19 9l-4-4L4 16z'/><path d='M13 7l4 4'/>")},
        {QStringLiteral("copy"), QStringLiteral("<rect x='9' y='9' width='11' height='11' rx='2'/><path d='M15 5H7a2 2 0 0 0-2 2v8'/>")},
        {QStringLiteral("trash"), QStringLiteral("<path d='M4 7h16'/><path d='M9 7V4h6v3'/><path d='M6 7l1 13h10l1-13'/><path d='M10 11v6M14 11v6'/>")},
        {QStringLiteral("storage"), QStringLiteral("<ellipse cx='12' cy='6' rx='7' ry='3'/><path d='M5 6v12c0 1.7 3.1 3 7 3s7-1.3 7-3V6'/><path d='M5 12c0 1.7 3.1 3 7 3s7-1.3 7-3'/>")},
        {QStringLiteral("bell"), QStringLiteral("<path d='M6 16v-5a6 6 0 0 1 12 0v5l2 2H4z'/><path d='M10 21h4'/>")},
        {QStringLiteral("folder"), QStringLiteral("<path d='M3 7a2 2 0 0 1 2-2h4l2 2h8a2 2 0 0 1 2 2v8a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2z'/>")},
        {QStringLiteral("refresh"), QStringLiteral("<path d='M20 12a8 8 0 1 1-2.34-5.66'/><path d='M20 4v5h-5'/>")},
        {QStringLiteral("export"), QStringLiteral("<path d='M12 15V4'/><path d='M8 8l4-4 4 4'/><path d='M5 14v4a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-4'/>")},
        {QStringLiteral("import"), QStringLiteral("<path d='M12 4v11'/><path d='M8 11l4 4 4-4'/><path d='M5 14v4a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2v-4'/>")},
        {QStringLiteral("shield"), QStringLiteral("<path d='M12 3l7 3v5c0 4.5-3 8.3-7 10-4-1.7-7-5.5-7-10V6z'/>")},
        {QStringLiteral("settings"), QStringLiteral("<path d='M4 7h9M19 7h1M4 17h1M11 17h9'/><circle cx='16' cy='7' r='2.5'/><circle cx='8' cy='17' r='2.5'/>")},
        {QStringLiteral("ok"), QStringLiteral("<circle cx='12' cy='12' r='9'/><path d='M8 12.5l3 3 5-6'/>")},
        {QStringLiteral("warn"), QStringLiteral("<path d='M12 4l9 16H3z'/><path d='M12 10v4M12 17v.5'/>")},
        {QStringLiteral("error"), QStringLiteral("<circle cx='12' cy='12' r='9'/><path d='M9 9l6 6M15 9l-6 6'/>")},
        {QStringLiteral("clock"), QStringLiteral("<circle cx='12' cy='12' r='9'/><path d='M12 7v5l3 2'/>")},
        {QStringLiteral("list"), QStringLiteral("<path d='M9 6h11M9 12h11M9 18h11M4.5 6h.5M4.5 12h.5M4.5 18h.5'/>")},
        {QStringLiteral("calendar"), QStringLiteral("<rect x='4' y='5' width='16' height='15' rx='2'/><path d='M4 10h16M8 3v4M16 3v4'/>")},
        {QStringLiteral("activity"), QStringLiteral("<path d='M3 12h4l3-7 4 14 3-7h4'/>")},
    };
    return h;
}

} // namespace

namespace theme {

bool isDark() {
    return QGuiApplication::styleHints()->colorScheme() == Qt::ColorScheme::Dark;
}

void apply() {
    // Fusion рисует одинаково в светлой и тёмной палитре; системный стиль Windows часть цветов не берёт из палитры.
    static bool styled = false;
    if (!styled) {
        QApplication::setStyle(QStyleFactory::create(QStringLiteral("Fusion")));
        styled = true;
    }
    const Colors &k = pal();
    QPalette p;
    p.setColor(QPalette::Window, col(k.bg));
    p.setColor(QPalette::WindowText, col(k.text));
    p.setColor(QPalette::Base, col(k.field));
    p.setColor(QPalette::AlternateBase, col(k.surface));
    p.setColor(QPalette::Text, col(k.text));
    p.setColor(QPalette::Button, col(k.surface));
    p.setColor(QPalette::ButtonText, col(k.text));
    p.setColor(QPalette::BrightText, col("#FFFFFF"));
    p.setColor(QPalette::ToolTipBase, col(k.surface));
    p.setColor(QPalette::ToolTipText, col(k.text));
    p.setColor(QPalette::PlaceholderText, col(k.muted));
    p.setColor(QPalette::Highlight, col(k.accent));
    p.setColor(QPalette::HighlightedText, col("#FFFFFF"));
    p.setColor(QPalette::Link, col(k.info));
    p.setColor(QPalette::Light, col(k.surface));
    p.setColor(QPalette::Midlight, col(k.hover));
    p.setColor(QPalette::Mid, col(k.border));
    p.setColor(QPalette::Dark, col(k.border));
    p.setColor(QPalette::Shadow, col("#000000"));
    for (QPalette::ColorRole r : {QPalette::WindowText, QPalette::Text, QPalette::ButtonText})
        p.setColor(QPalette::Disabled, r, col(k.muted));
    QApplication::setPalette(p);

    QString qss = QString::fromUtf8(kQss);
    // Длинные имена раньше коротких: «@accentHover» содержит «@accent».
    const QList<QPair<QString, QString>> tokens{
        {QStringLiteral("@accentHover"), QString::fromLatin1(k.accentHover)},
        {QStringLiteral("@accent"), QString::fromLatin1(k.accent)},
        {QStringLiteral("@surface"), QString::fromLatin1(k.surface)},
        {QStringLiteral("@field"), QString::fromLatin1(k.field)},
        {QStringLiteral("@border"), QString::fromLatin1(k.border)},
        {QStringLiteral("@hover"), QString::fromLatin1(k.hover)},
        {QStringLiteral("@text"), QString::fromLatin1(k.text)},
        {QStringLiteral("@muted"), QString::fromLatin1(k.muted)},
        {QStringLiteral("@sel"), QString::fromLatin1(k.sel)},
        {QStringLiteral("@infoBg"), QString::fromLatin1(k.infoBg)},
        {QStringLiteral("@info"), QString::fromLatin1(k.info)},
        {QStringLiteral("@errBg"), QString::fromLatin1(k.errBg)},
        {QStringLiteral("@err"), QString::fromLatin1(k.err)},
        {QStringLiteral("@bg"), QString::fromLatin1(k.bg)},
        {QStringLiteral("@mode"), isDark() ? QStringLiteral("dark") : QStringLiteral("light")},
    };
    for (const auto &t : tokens)
        qss.replace(t.first, t.second);
    qApp->setStyleSheet(qss);
}

QColor color(State s) {
    const Colors &k = pal();
    switch (s) {
    case Ok: return col(k.ok);
    case Warn: return col(k.warn);
    case Error: return col(k.err);
    case Info: return col(k.info);
    case Muted: return col(k.muted);
    case None: break;
    }
    return col(k.text);
}

QColor tint(State s) {
    const Colors &k = pal();
    switch (s) {
    case Ok: return col(k.okBg);
    case Warn: return col(k.warnBg);
    case Error: return col(k.errBg);
    case Info: return col(k.infoBg);
    case Muted:
    case None: break;
    }
    return col(k.mutedBg);
}

QColor textColor() { return col(pal().text); }
QColor mutedColor() { return col(pal().muted); }

QPixmap pixmap(const QString &name, const QColor &c, int size) {
    QString body = iconPaths().value(name);
    body.replace(QStringLiteral("{c}"), c.name());
    const QString svg = QStringLiteral("<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 24 24'>"
                                       "<g fill='none' stroke='") +
                        c.name() +
                        QStringLiteral("' stroke-width='1.8' stroke-linecap='round' stroke-linejoin='round'>") + body +
                        QStringLiteral("</g></svg>");
    const qreal dpr = qApp->devicePixelRatio();
    const int px = qMax(1, qRound(size * dpr));
    QPixmap pm(px, px);
    pm.fill(Qt::transparent);
    QSvgRenderer renderer(svg.toUtf8());
    QPainter p(&pm);
    renderer.render(&p, QRectF(0, 0, px, px));
    p.end();
    pm.setDevicePixelRatio(dpr);
    return pm;
}

QIcon icon(const QString &name, State s, int size) {
    QIcon ic;
    ic.addPixmap(pixmap(name, color(s), size), QIcon::Normal);
    ic.addPixmap(pixmap(name, mutedColor(), size), QIcon::Disabled);
    return ic;
}

void markHint(QLabel *label) {
    label->setProperty("hint", true);
}

void setState(QLabel *label, State s) {
    if (s == None) {
        label->setPalette(QPalette());
        return;
    }
    QPalette p = label->palette();
    p.setColor(QPalette::WindowText, color(s));
    label->setPalette(p);
}

void PillDelegate::paint(QPainter *p, const QStyleOptionViewItem &option, const QModelIndex &index) const {
    // Без состояния (заголовок группы в списке заданий) — обычный текст без плашки.
    if (!index.data(StateRole).isValid()) return QStyledItemDelegate::paint(p, option, index);
    QStyleOptionViewItem opt = option;
    initStyleOption(&opt, index);
    const QString text = opt.text;
    opt.text.clear();
    const QWidget *w = opt.widget;
    QStyle *st = w ? w->style() : QApplication::style();
    st->drawControl(QStyle::CE_ItemViewItem, &opt, p, w);
    if (text.isEmpty()) return;

    const State s = static_cast<State>(index.data(StateRole).toInt());
    const QFontMetrics fm(opt.font);
    const int h = qMin(opt.rect.height() - 8, fm.height() + 8);
    const int width = qMin(opt.rect.width() - 14, fm.horizontalAdvance(text) + 30);
    if (h < 8 || width < 24) return;
    const QRectF r(opt.rect.left() + 8, opt.rect.center().y() - h / 2.0 + 0.5, width, h);
    p->save();
    p->setRenderHint(QPainter::Antialiasing);
    p->setPen(Qt::NoPen);
    p->setBrush(tint(s));
    p->drawRoundedRect(r, h / 2.0, h / 2.0);
    p->setBrush(color(s));
    p->drawEllipse(QPointF(r.left() + 11, r.center().y()), 3.5, 3.5);
    p->setPen(color(s));
    p->setFont(opt.font);
    const QRectF textRect = r.adjusted(20, 0, -8, 0);
    p->drawText(textRect, Qt::AlignVCenter | Qt::AlignLeft, fm.elidedText(text, Qt::ElideRight, int(textRect.width()) + 1));
    p->restore();
}

QSize PillDelegate::sizeHint(const QStyleOptionViewItem &option, const QModelIndex &index) const {
    QSize s = QStyledItemDelegate::sizeHint(option, index);
    // Заголовок растянут на всю строку: его ширина не должна раздувать первую колонку.
    if (!index.data(StateRole).isValid()) return QSize(0, s.height());
    const QFontMetrics fm(option.font);
    s.setWidth(fm.horizontalAdvance(index.data(Qt::DisplayRole).toString()) + 48);
    return s;
}

} // namespace theme
