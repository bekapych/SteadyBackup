#include "mainwindow.h"
#include "theme.h"
#include "util.h"

#include <QApplication>
#include <QLocalServer>
#include <QLibraryInfo>
#include <QLocalSocket>
#include <QTranslator>

int main(int argc, char *argv[]) {
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("SteadyBackup"));
    QApplication::setOrganizationName(QStringLiteral("SteadyBackup"));
    QApplication::setQuitOnLastWindowClosed(false);
    QApplication::setWindowIcon(util::appIcon());
    // Значки в меню не используются: пункты выровнены по тексту.
    QApplication::setAttribute(Qt::AA_DontShowIconsInMenus);
    theme::apply();

    // Стандартные надписи Qt (Да/Нет, диалоги выбора файлов) по-русски, если переводы есть рядом.
    QTranslator qtTr;
    if (qtTr.load(QLocale(QLocale::Russian), QStringLiteral("qtbase"), QStringLiteral("_"),
                  QApplication::applicationDirPath() + QStringLiteral("/translations")))
        app.installTranslator(&qtTr);

    // Один экземпляр на пользователя: второй запуск активирует первое окно.
    // Экземпляр с повышенными правами (--elevated) — отдельный: неповышенный процесс не может
    // подключиться к его локальному серверу, и окно просмотра не должно мешать ему запуститься.
    const bool elevated = app.arguments().contains(QStringLiteral("--elevated"));
    const QString key = QStringLiteral("SteadyBackupGui-") + qEnvironmentVariable("USERNAME") +
                        (elevated ? QStringLiteral("-admin") : QString());
    {
        QLocalSocket probe;
        probe.connectToServer(key);
        if (probe.waitForConnected(300)) {
            probe.write("show\n");
            probe.waitForBytesWritten(300);
            return 0;
        }
    }
    QLocalServer::removeServer(key);
    QLocalServer single;
    single.listen(key);

    MainWindow w;
    QObject::connect(&single, &QLocalServer::newConnection, &w, [&] {
        while (auto *c = single.nextPendingConnection()) {
            c->deleteLater();
            w.showNormal();
            w.raise();
            w.activateWindow();
        }
    });

    const int shots = app.arguments().indexOf(QStringLiteral("--shots"));
    if (shots > 0 && shots + 1 < app.arguments().size()) {
        w.show();
        w.takeShots(app.arguments().at(shots + 1));
    } else if (!app.arguments().contains(QStringLiteral("--tray"))) {
        w.show();
    }
    return app.exec();
}
