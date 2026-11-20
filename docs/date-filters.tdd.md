# Фильтры файлов по дате — проверка TDD

Дата: 2026-11-20. Требования получены непосредственно от пользователя.

## Пользовательские сценарии

* Копировать файлы, изменённые или созданные за последние N часов (по умолчанию 24).
* Копировать файлы начиная с выбранной локальной даты и времени.
* Не считать удалёнными файлы, которые просто перестали соответствовать фильтру.
* Не терять последующее реальное удаление такого файла из инкрементного манифеста.

## RED → GREEN

* RED: `cargo test -p steady-core date_filter -- --nocapture` — ожидаемая ошибка компиляции:
  отсутствовали `FileDateFilter`, `FileDateField`, `FileDateMode`, `Filter::accepts_date` и
  `DavEntry::created`.
* GREEN: та же команда — 5 тестов успешно; затем `source::files::tests` и разбор WebDAV прошли.
* Дополнительный RED: `cargo test -p steady-core date_filtered_file_stays_in_state_catalog` выявил
  потерю отфильтрованного файла из каталога состояния. Перенос прежнего штампа в новое состояние
  исправил ошибку; тест и весь набор `source::files::tests` стали зелёными.
* Полный набор: `cargo test --workspace` — 127 успешно, 1 сетевой тест пропущен по `#[ignore]`.

| Гарантия | Тест | Результат |
|---|---|---|
| Старый JSON имеет выключенный фильтр | `job::tests::old_file_job_has_date_filter_disabled` | PASS |
| Относительный фильтр включает границу 24 часов и исключает старые файлы | `relative_date_filter_accepts_only_last_24_hours` | PASS |
| Можно использовать дату создания вместо изменения | `date_filter_can_use_creation_instead_of_modification` | PASS |
| Фиксированный момент `since` разбирается и применяется | `fixed_date_filter_accepts_files_since_selected_moment` | PASS |
| Нулевой период и отсутствие `since` отклоняются | `invalid_date_filter_is_rejected_before_scanning` | PASS |
| Фильтрация не создаёт ложное удаление | `date_filtered_file_is_not_reported_as_deleted` | PASS |
| Отфильтрованный файл остаётся в каталоге состояния | `date_filtered_file_stays_in_state_catalog` | PASS |
| Подготовка копирует только подходящие по дате файлы | `prepare_copies_only_files_matching_modified_date` | PASS |
| WebDAV читает `creationdate` | `destination::webdav::tests::propfind` | PASS |

## Сквозная проверка

На временных локальных каталогах выполнены настоящие `steady run` с разными датами создания и изменения:

* `within` по изменению выбрал только файл с недавним `LastWriteTime`;
* `within` по созданию на том же источнике выбрал другой файл — с недавним `CreationTime`;
* `since` выбрал только файл после заданного момента;
* следующий инкремент не записал старый отфильтрованный файл в `deleted`.

`cargo clippy --workspace --all-targets` завершился успешно; 6 замечаний относятся к существующему коду.
`cmake --build build-gui` собрал GUI с блоком «Фильтр по дате».

## Непроверенное

Реальные WebDAV и SFTP не запускались; их обработчики проверены разбором ответа и общей логикой фильтра.
Окно GUI интерактивно не открывалось. Численное покрытие не измерялось. Установка MSI и автоматическое
обновление службы отдельно не проверяются.
