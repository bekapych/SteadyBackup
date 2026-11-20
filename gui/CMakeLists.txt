cmake_minimum_required(VERSION 3.21)
project(SteadyBackupGui VERSION 0.1.11 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_AUTOMOC ON)
set(CMAKE_AUTORCC ON)

find_package(Qt6 REQUIRED COMPONENTS Widgets Network Svg)

# Значок программы в самом exe: его показывают Проводник, ярлык и «Программы и компоненты».
# Путь к .ico подставляется абсолютным: windres ищет файлы относительно каталога сборки.
set(STEADY_ICON "${CMAKE_CURRENT_SOURCE_DIR}/res/steady.ico")
configure_file(res/app.rc.in "${CMAKE_CURRENT_BINARY_DIR}/app.rc" @ONLY)

qt_add_executable(steady-gui WIN32
    src/main.cpp
    src/serviceclient.cpp src/serviceclient.h
    src/mainwindow.cpp src/mainwindow.h
    src/jobdialog.cpp src/jobdialog.h
    src/destinationdialog.cpp src/destinationdialog.h
    src/notifydialog.cpp src/notifydialog.h
    src/resourcesdialog.cpp src/resourcesdialog.h
    src/connectionpicker.cpp src/connectionpicker.h
    src/groupdialog.cpp src/groupdialog.h
    src/updatedialog.cpp src/updatedialog.h
    src/util.cpp src/util.h
    src/theme.cpp src/theme.h
    res/res.qrc
    "${CMAKE_CURRENT_BINARY_DIR}/app.rc"
)

target_link_libraries(steady-gui PRIVATE Qt6::Widgets Qt6::Network Qt6::Svg shell32)
target_compile_definitions(steady-gui PRIVATE QT_NO_KEYWORDS)
