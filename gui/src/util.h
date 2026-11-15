#pragma once

#include <QComboBox>
#include <QDateTime>
#include <QIcon>
#include <QJsonArray>
#include <QJsonObject>
#include <QString>
#include <QStringList>

// Обычный QComboBox в модальном окне, особенно если его заполнили до показа,
// на Windows не открывает список. Этот открывает его явно поверх окна.
class QDialog;

class SteadyCombo : public QComboBox {
public:
    using QComboBox::QComboBox;
    void showPopup() override;
};

namespace util {

QIcon appIcon(const QColor &accent = QColor(0x2e, 0x7d, 0x32));
QString sourceTitle(const QString &type);
QString destinationSummary(const QJsonObject &d);
QString scheduleSummary(const QJsonObject &s);
QString formatBytes(qint64 bytes);
QString formatDateTime(const QString &iso);
// Время службы → местное. Невалидное значение, если строка не разобрана.
QDateTime parseIso(const QString &iso);
// «сегодня, 23:30», «вчера, 22:57», иначе полная дата.
QString formatWhen(const QString &iso);
// 5:07 или 1:02:15.
QString formatDuration(qint64 secs);
QStringList splitList(const QString &text);
// Значение для JSON: пустая строка → null (Option::None в службе).
QJsonValue optString(const QString &s);
// Последний успешный список хранилищ. Окно задания и окно назначения читают его сразу,
// не дожидаясь ещё одного ответа службы.
void setStorageCache(const QJsonArray &list);
QJsonArray storageCache();
// Окно не выше экрана: иначе на низком разрешении кнопки уезжают под панель задач.
void fitToScreen(QWidget *w);
// То же для диалога: «Сохранить» остаётся внизу, длинная форма прокручивается.
void keepButtonsOnScreen(QDialog *dlg);

} // namespace util
