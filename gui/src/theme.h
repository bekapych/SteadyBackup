#pragma once

#include <QColor>
#include <QIcon>
#include <QPixmap>
#include <QString>
#include <QStyledItemDelegate>

class QLabel;

// Оформление программы: палитра (светлая и тёмная, по настройке Windows), таблица стилей,
// значки и цвета состояний. Цвета в остальном коде напрямую не задаются — только отсюда.
namespace theme {

enum State { None = 0, Ok, Warn, Error, Info, Muted };

// Роль элемента таблицы с состоянием (State) для PillDelegate.
enum { StateRole = Qt::UserRole + 20 };

bool isDark();
// Стиль, палитра и таблица стилей приложения. Вызывается при запуске и при смене темы Windows.
void apply();

QColor color(State s); // цвет текста и значка состояния
QColor tint(State s);  // фон плашки состояния
QColor textColor();
QColor mutedColor();

// Значок из встроенного набора, в цвете текста или состояния.
QIcon icon(const QString &name, State s = None, int size = 18);
QPixmap pixmap(const QString &name, const QColor &color, int size);

// Пояснение под полем: приглушённый цвет, следует за темой.
void markHint(QLabel *label);
// Результат проверки: цвет состояния. None возвращает обычный цвет.
void setState(QLabel *label, State s);

// Ячейка таблицы в виде цветной плашки с точкой: «Успешно», «Ошибка» и т. п.
class PillDelegate : public QStyledItemDelegate {
public:
    using QStyledItemDelegate::QStyledItemDelegate;
    void paint(QPainter *p, const QStyleOptionViewItem &option, const QModelIndex &index) const override;
    QSize sizeHint(const QStyleOptionViewItem &option, const QModelIndex &index) const override;
};

} // namespace theme
