#pragma once

#include <QJsonObject>
#include <QWidget>

class QComboBox;
class ServiceClient;

// Выбор подключения (учётных данных источника) из раздела «Подключения».
// Для заданий старого формата показывает пункт «задано в задании» и сохраняет поля как есть.
class ConnectionPicker : public QWidget {
    Q_OBJECT
public:
    // type: "ms_sql" | "one_c" | "web_dav" | "firebird" | "postgresql" | "ssh"; noneText — подпись пункта «без подключения» (пусто — пункта нет).
    ConnectionPicker(ServiceClient *client, const QString &type, const QString &noneText, QWidget *parent = nullptr);
    void setSource(const QJsonObject &source);
    // Записывает в источник connection (и сохраняет встроенные поля, если выбран «старый» вариант).
    void apply(QJsonObject &source) const;

Q_SIGNALS:
    // Пользователь выбрал другой пункт или список подключений загружен.
    void changed();

private:
    void reload(const QString &select);
    // Заполнить список тем, что уже известно, не дожидаясь ответа службы.
    void fill(const QString &select);

    ServiceClient *m_client;
    QString m_type;
    QString m_noneText;
    class SteadyCombo *m_combo;
    QJsonObject m_inline; // user/password/server из старого задания
    QString m_savedConnection; // ссылка из задания: не терять, пока список не загрузился
    bool m_ready = false;      // список получен (или получена ошибка)
    bool m_userPicked = false; // пользователь сам менял пункт
    int m_seq = 0;             // ответы на устаревшие запросы списка игнорируются
};
