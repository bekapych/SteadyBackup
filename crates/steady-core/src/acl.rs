//! Права доступа к каталогу данных и проверка владельца файлов.
//!
//! Служба работает от SYSTEM и выполняет задания из файлов каталога данных, а секреты там
//! зашифрованы DPAPI в области LocalMachine (расшифровать может любой локальный процесс).
//! Поэтому каталог должен быть доступен только SYSTEM и «Администраторам», без наследования
//! прав от `%ProgramData%`, где обычные пользователи могут создавать файлы.

use anyhow::Result;
use std::path::Path;

/// Защищённый DACL: SYSTEM и Администраторы — полный доступ с наследованием, владелец — Администраторы.
/// `P` — наследование от родителя отключено.
#[cfg(windows)]
const DATA_DIR_SDDL: &str = "O:BAD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)";

/// Создаёт каталог (если нужно) и выставляет защищённый DACL на него и на всё содержимое.
/// Для уже существующих файлов права и владелец переустанавливаются: каталог мог быть
/// создан раньше с унаследованными от `%ProgramData%` правами.
#[cfg(windows)]
pub fn secure_data_dir(dir: &Path) -> Result<()> {
    use anyhow::{bail, Context};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::*;

    std::fs::create_dir_all(dir).with_context(|| format!("создание {}", dir.display()))?;
    let sddl: Vec<u16> = DATA_DIR_SDDL.encode_utf16().chain(std::iter::once(0)).collect();
    let path: Vec<u16> = wide(dir);
    unsafe {
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            bail!("SDDL каталога данных: {}", std::io::Error::last_os_error());
        }
        let mut present = 0;
        let mut defaulted = 0;
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut owner: PSID = std::ptr::null_mut();
        let ok = GetSecurityDescriptorDacl(sd, &mut present, &mut dacl, &mut defaulted) != 0
            && GetSecurityDescriptorOwner(sd, &mut owner, &mut defaulted) != 0;
        if !ok {
            let e = std::io::Error::last_os_error();
            LocalFree(sd as _);
            bail!("разбор SDDL каталога данных: {e}");
        }
        // TREE_SEC_INFO_RESET: дочерние объекты получают права только наследованием от каталога,
        // явные права (в т.ч. оставшиеся от прежних версий) удаляются.
        let rc = TreeSetNamedSecurityInfoW(
            path.as_ptr() as _,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            owner,
            std::ptr::null_mut(),
            dacl,
            std::ptr::null(),
            TREE_SEC_INFO_RESET,
            None,
            ProgressInvokeNever,
            std::ptr::null(),
        );
        LocalFree(sd as _);
        if rc != 0 {
            bail!(
                "установка прав на {}: {} (нужны права администратора)",
                dir.display(),
                std::io::Error::from_raw_os_error(rc as i32)
            );
        }
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn secure_data_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    Ok(())
}

/// Владелец файла — SYSTEM или «Администраторы». Файлы, созданные службой или администратором
/// через GUI, всегда удовлетворяют этому условию; подложенные обычным пользователем — нет.
#[cfg(windows)]
pub fn owned_by_admins(path: &Path) -> Result<bool> {
    use anyhow::bail;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::*;

    let p = wide(path);
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            p.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            bail!("владелец {}: {}", path.display(), std::io::Error::from_raw_os_error(rc as i32));
        }
        let ok = !owner.is_null()
            && (IsWellKnownSid(owner, WinLocalSystemSid) != 0 || IsWellKnownSid(owner, WinBuiltinAdministratorsSid) != 0);
        LocalFree(sd as _);
        Ok(ok)
    }
}

#[cfg(not(windows))]
pub fn owned_by_admins(_: &Path) -> Result<bool> {
    Ok(true)
}

/// Проверяет, что исполняемый файл можно запускать от SYSTEM: путь абсолютный, файл существует,
/// и ни сам файл, ни один каталог над ним не доступны для изменения никому, кроме SYSTEM,
/// «Администраторов» и TrustedInstaller. Иначе пользователь, способный подменить файл,
/// получил бы выполнение кода с правами службы.
pub fn check_trusted_exe(path: &Path) -> Result<()> {
    use anyhow::bail;
    if !path.is_absolute() {
        bail!("путь к программе должен быть абсолютным: {}", path.display());
    }
    if !path.is_file() {
        bail!("программа не найдена: {}", path.display());
    }
    let mut cur = Some(path);
    let mut level = Level::File;
    while let Some(p) = cur {
        if let Some(who) = untrusted_writer(p, level)? {
            bail!(
                "{} не запускается от имени службы: «{}» может изменять {}. \
                 Установите программу в Program Files или ограничьте права на папку.",
                path.display(),
                who,
                p.display()
            );
        }
        level = if level == Level::File { Level::Dir } else { Level::Ancestor };
        cur = p.parent();
    }
    Ok(())
}

/// Что проверяется на очередном уровне пути.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    /// Сам исполняемый файл: любая запись.
    File,
    /// Его каталог: ещё и добавление файлов (подложить DLL рядом с программой).
    Dir,
    /// Каталоги выше: только удаление/переименование вложенных объектов и смена прав.
    /// Право создавать новые папки (есть у всех на корне диска) подменить программу не позволяет.
    Ancestor,
}

#[cfg(windows)]
fn write_mask(level: Level) -> u32 {
    use windows_sys::Win32::Foundation::{GENERIC_ALL, GENERIC_WRITE};
    use windows_sys::Win32::Storage::FileSystem::*;
    let replace = FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | GENERIC_ALL;
    match level {
        // Для файла FILE_WRITE_DATA/FILE_APPEND_DATA — изменение содержимого.
        Level::File => replace | FILE_WRITE_DATA | FILE_APPEND_DATA | GENERIC_WRITE,
        // Для каталога FILE_WRITE_DATA = FILE_ADD_FILE.
        Level::Dir => replace | FILE_ADD_FILE | GENERIC_WRITE,
        Level::Ancestor => replace,
    }
}

/// Первая учётная запись (кроме доверенных), которой DACL объекта разрешает запись.
/// Владелец объекта неявно может менять права, поэтому недоверенный владелец тоже считается писателем.
#[cfg(windows)]
fn untrusted_writer(path: &Path, level: Level) -> Result<Option<String>> {
    use anyhow::bail;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::*;
    use windows_sys::Win32::Security::*;
    use windows_sys::Win32::System::SystemServices::ACCESS_ALLOWED_ACE_TYPE;

    let p = wide(path);
    unsafe {
        let mut owner: PSID = std::ptr::null_mut();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        let rc = GetNamedSecurityInfoW(
            p.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            std::ptr::null_mut(),
            &mut dacl,
            std::ptr::null_mut(),
            &mut sd,
        );
        if rc != 0 {
            bail!("права {}: {}", path.display(), std::io::Error::from_raw_os_error(rc as i32));
        }
        let result = (|| {
            if !owner.is_null() && !is_trusted_sid(owner) {
                return Some(sid_name(owner));
            }
            // DACL отсутствует — полный доступ всем.
            if dacl.is_null() {
                return Some("Все".into());
            }
            for i in 0..(*dacl).AceCount as u32 {
                let mut ace: *mut core::ffi::c_void = std::ptr::null_mut();
                if GetAce(dacl, i, &mut ace) == 0 {
                    continue;
                }
                let hdr = &*(ace as *const ACE_HEADER);
                if hdr.AceType as u32 != ACCESS_ALLOWED_ACE_TYPE {
                    continue;
                }
                // Только наследуемые записи не дают прав на сам объект.
                if hdr.AceFlags as u32 & INHERIT_ONLY_ACE != 0 {
                    continue;
                }
                let a = &*(ace as *const ACCESS_ALLOWED_ACE);
                if a.Mask & write_mask(level) == 0 {
                    continue;
                }
                let sid = &a.SidStart as *const u32 as PSID;
                if !is_trusted_sid(sid) {
                    return Some(sid_name(sid));
                }
            }
            None
        })();
        LocalFree(sd as _);
        Ok(result)
    }
}

/// SYSTEM, «Администраторы», TrustedInstaller.
#[cfg(windows)]
unsafe fn is_trusted_sid(sid: windows_sys::Win32::Security::PSID) -> bool {
    use windows_sys::Win32::Security::*;
    if IsWellKnownSid(sid, WinLocalSystemSid) != 0 || IsWellKnownSid(sid, WinBuiltinAdministratorsSid) != 0 {
        return true;
    }
    // NT SERVICE\TrustedInstaller — служебный SID без well-known константы.
    sid_string(sid).as_deref() == Some("S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464")
}

#[cfg(windows)]
unsafe fn sid_string(sid: windows_sys::Win32::Security::PSID) -> Option<String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    let mut s: windows_sys::core::PWSTR = std::ptr::null_mut();
    if ConvertSidToStringSidW(sid, &mut s) == 0 {
        return None;
    }
    let mut n = 0;
    while *s.add(n) != 0 {
        n += 1;
    }
    let r = String::from_utf16_lossy(std::slice::from_raw_parts(s, n));
    LocalFree(s as _);
    Some(r)
}

/// «ДОМЕН\Имя» для сообщения об ошибке; при неудаче — строковый SID.
#[cfg(windows)]
unsafe fn sid_name(sid: windows_sys::Win32::Security::PSID) -> String {
    use windows_sys::Win32::Security::*;
    let mut name = [0u16; 256];
    let mut domain = [0u16; 256];
    let (mut nl, mut dl) = (name.len() as u32, domain.len() as u32);
    let mut use_ = 0;
    if LookupAccountSidW(std::ptr::null(), sid, name.as_mut_ptr(), &mut nl, domain.as_mut_ptr(), &mut dl, &mut use_) != 0 {
        let n = String::from_utf16_lossy(&name[..nl as usize]);
        let d = String::from_utf16_lossy(&domain[..dl as usize]);
        return if d.is_empty() { n } else { format!("{d}\\{n}") };
    }
    sid_string(sid).unwrap_or_else(|| "?".into())
}

#[cfg(not(windows))]
fn untrusted_writer(_: &Path, _: Level) -> Result<Option<String>> {
    Ok(None)
}

#[cfg(windows)]
fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("steady-acl-{name}-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Файл, созданный обычным процессом (тесты идут без повышения прав), принадлежит пользователю.
    #[test]
    fn user_file_is_not_admin_owned() {
        let d = tmp("owner");
        let f = d.join("job.json");
        std::fs::write(&f, b"{}").unwrap();
        assert!(!owned_by_admins(&f).unwrap());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_file_is_admin_owned() {
        // Владелец %ProgramData% — SYSTEM.
        let pd = std::env::var_os("ProgramData").map(std::path::PathBuf::from).unwrap();
        assert!(owned_by_admins(&pd).unwrap());
    }

    /// Без повышения прав сменить владельца на Администраторов нельзя — ошибка с подсказкой,
    /// а не молчаливый успех.
    #[test]
    fn secure_without_elevation_fails_clearly() {
        if is_elevated() {
            return;
        }
        let d = tmp("secure");
        let err = secure_data_dir(&d).unwrap_err().to_string();
        assert!(err.contains("нужны права администратора"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn system_exe_is_trusted() {
        let windir = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap();
        check_trusted_exe(&windir.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"))
            .unwrap();
        check_trusted_exe(&windir.join("System32").join("cmd.exe")).unwrap();
    }

    /// Программа в каталоге пользователя (%TEMP%) — пользователь может её подменить.
    #[test]
    fn user_writable_exe_is_rejected() {
        let d = tmp("exe");
        let f = d.join("1cv8.exe");
        std::fs::copy(std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap().join("System32\\cmd.exe"), &f)
            .unwrap();
        let err = check_trusted_exe(&f).unwrap_err().to_string();
        assert!(err.contains("не запускается от имени службы"), "{err}");
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn relative_and_missing_are_rejected() {
        assert!(check_trusted_exe(Path::new("sqlcmd.exe")).unwrap_err().to_string().contains("абсолютным"));
        assert!(check_trusted_exe(Path::new("C:\\нет\\такой\\программы.exe")).unwrap_err().to_string().contains("не найдена"));
    }

    fn is_elevated() -> bool {
        std::process::Command::new("net").args(["session"]).output().map(|o| o.status.success()).unwrap_or(false)
    }
}
