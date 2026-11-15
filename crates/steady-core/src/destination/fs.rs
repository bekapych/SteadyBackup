use super::Target;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

pub struct FsTarget {
    root: PathBuf,
}

impl FsTarget {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn put_once(&self, local: &Path, name: &str) -> Result<()> {
        std::fs::create_dir_all(&self.root).with_context(|| format!("создание {}", self.root.display()))?;
        let dst = self.root.join(name);
        let tmp = self.root.join(format!("{name}.partial"));
        if tmp.is_dir() {
            std::fs::remove_dir_all(&tmp).ok();
        } else if tmp.exists() {
            std::fs::remove_file(&tmp).ok();
        }
        if local.is_dir() {
            crate::util::copy_dir(local, &tmp)?;
        } else {
            std::fs::copy(local, &tmp).with_context(|| format!("копирование в {}", tmp.display()))?;
        }
        // Сверяем до переименования: несовпадение — ошибка, повтор сотрёт .partial и отправит заново.
        verify_tree(local, &tmp)?;
        if dst.is_dir() {
            std::fs::remove_dir_all(&dst)?;
        } else if dst.exists() {
            std::fs::remove_file(&dst)?;
        }
        std::fs::rename(&tmp, &dst).with_context(|| format!("переименование в {}", dst.display()))?;
        Ok(())
    }
}

fn verify_tree(local: &Path, remote: &Path) -> Result<()> {
    if local.is_file() {
        return super::expect_size(&remote.display().to_string(), local.metadata()?.len(), remote.metadata()?.len());
    }
    for (file, rel) in super::walk_files(local)? {
        let got = remote.join(rel.replace('/', "\\"));
        let got = if got.exists() { got } else { remote.join(&rel) };
        super::expect_size(&rel, file.metadata()?.len(), got.metadata().with_context(|| format!("нет {}", got.display()))?.len())?;
    }
    Ok(())
}

impl Target for FsTarget {
    fn describe(&self) -> String {
        self.root.display().to_string()
    }

    fn put(&mut self, local: &Path, name: &str) -> Result<()> {
        // Повторы — в конвейере (`Job::upload_attempts`).
        self.put_once(local, name)
    }

    fn list(&mut self) -> Result<Vec<String>> {
        if !self.root.exists() {
            return Ok(vec![]);
        }
        let mut v = Vec::new();
        for e in std::fs::read_dir(&self.root)? {
            v.push(e?.file_name().to_string_lossy().into_owned());
        }
        Ok(v)
    }

    fn delete(&mut self, name: &str) -> Result<()> {
        let p = self.root.join(name);
        if p.is_dir() {
            std::fs::remove_dir_all(&p)?;
        } else if p.exists() {
            std::fs::remove_file(&p)?;
        }
        Ok(())
    }

    #[cfg(windows)]
    fn space(&mut self) -> Option<(u64, u64)> {
        use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        // Ближайший существующий каталог (папки задания может ещё не быть).
        let mut p = self.root.as_path();
        while !p.exists() {
            p = p.parent()?;
        }
        let w: Vec<u16> = p.as_os_str().encode_wide_null();
        let (mut avail, mut total, mut free) = (0u64, 0u64, 0u64);
        let ok = unsafe { GetDiskFreeSpaceExW(w.as_ptr(), &mut avail, &mut total, &mut free) };
        (ok != 0).then_some((total, avail))
    }
}

#[cfg(windows)]
trait EncodeWide {
    fn encode_wide_null(&self) -> Vec<u16>;
}

#[cfg(windows)]
impl EncodeWide for std::ffi::OsStr {
    fn encode_wide_null(&self) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        self.encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// Корень UNC-ресурса: \\server\share\a\b → \\server\share
pub fn share_root(unc: &str) -> Option<String> {
    let rest = unc.strip_prefix(r"\\")?;
    let mut parts = rest.split(['\\', '/']).filter(|s| !s.is_empty());
    Some(format!(r"\\{}\{}", parts.next()?, parts.next()?))
}

/// Подключает UNC-ресурс с учётными данными (без буквы диска, только на время сеанса).
#[cfg(windows)]
pub fn smb_connect(unc: &str, user: &str, password: &str) -> Result<()> {
    use windows_sys::Win32::NetworkManagement::WNet::*;
    let share = share_root(unc).context("SMB: путь должен быть вида \\\\сервер\\ресурс\\...")?;
    let w = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let mut remote = w(&share);
    let pw = w(password);
    let us = w(user);
    let nr = NETRESOURCEW { dwType: RESOURCETYPE_DISK, lpRemoteName: remote.as_mut_ptr(), ..Default::default() };
    let rc = unsafe { WNetAddConnection2W(&nr, pw.as_ptr(), us.as_ptr(), CONNECT_TEMPORARY) };
    match rc {
        0 => Ok(()),
        // 1219: уже есть подключение к серверу с другими учётными данными — используем его.
        1219 => {
            tracing::warn!("SMB: {share}: уже подключено под другой учётной записью, используется существующее подключение");
            Ok(())
        }
        code => bail!("SMB: подключение к {share}: {}", std::io::Error::from_raw_os_error(code as i32)),
    }
}

#[cfg(not(windows))]
pub fn smb_connect(_: &str, _: &str, _: &str) -> Result<()> {
    bail!("SMB с учётными данными поддерживается только в Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_file_and_dir_match_size_and_drop_partial() {
        let root = std::env::temp_dir().join(format!("steady-put-{}", uuid::Uuid::new_v4().simple()));
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("sub")).unwrap();
        std::fs::write(src.join("a.txt"), b"hello").unwrap();
        std::fs::write(src.join("sub").join("b.txt"), b"world!").unwrap();
        let mut t = FsTarget::new(&dst);
        t.put(&src, "set").unwrap();
        assert_eq!(std::fs::read(dst.join("set").join("a.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(dst.join("set").join("sub").join("b.txt")).unwrap(), b"world!");
        assert!(!dst.join("set.partial").exists());
        std::fs::remove_dir_all(&root).ok();
    }
}
