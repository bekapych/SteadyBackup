//! Теневые копии локальных томов для файлового источника.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[cfg(windows)]
use windows_impl::WindowsVss;

pub struct VssSet {
    #[cfg(windows)]
    inner: Option<WindowsVss>,
}

impl VssSet {
    /// Создаёт один набор снимков для всех локальных fixed-томов.
    /// UNC, WebDAV и сетевые диски остаются на прямом чтении.
    pub fn create(paths: &[String], timeout: Duration) -> Result<Self> {
        #[cfg(windows)]
        {
            let inner = WindowsVss::create(paths, timeout)?;
            Ok(Self { inner })
        }
        #[cfg(not(windows))]
        {
            let _ = timeout;
            if let Some(path) = paths.iter().find(|path| !is_remote_path(path)) {
                bail!("VSS для источника {path} поддерживается только в Windows");
            }
            Ok(Self {})
        }
    }

    /// Режим без VSS: все пути читаются напрямую.
    pub fn disabled() -> Self {
        Self {
            #[cfg(windows)]
            inner: None,
        }
    }

    /// Возвращает путь в снимке для исходного корня, переданного в `create`.
    pub fn scan_path(&self, original: &str) -> Result<PathBuf> {
        if is_remote_path(original) {
            return Ok(PathBuf::from(original));
        }
        #[cfg(windows)]
        if let Some(inner) = &self.inner {
            return inner.scan_path(original);
        }
        Ok(PathBuf::from(original))
    }

    /// Удаляет набор снимков; при любой ошибке COM-объекты всё равно освобождаются.
    pub fn close(mut self) -> Result<()> {
        #[cfg(windows)]
        if let Some(inner) = self.inner.take() {
            return inner.close();
        }
        Ok(())
    }
}

impl Drop for VssSet {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(inner) = self.inner.take() {
            if let Err(e) = inner.close() {
                tracing::error!("VSS: аварийная очистка снимков: {e:#}");
            }
        }
    }
}

fn is_remote_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    if lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.starts_with(r"\\?\unc\")
    {
        return true;
    }
    // Обычный UNC начинается с двух обратных косых, а \\?\ и \\.\ — локальные пространства имён.
    path.starts_with(r"\\") && !lower.starts_with(r"\\?\") && !lower.starts_with(r"\\.\")
}

#[cfg(windows)]
fn snapshot_path(device: &Path, volume_root: &Path, original: &Path) -> Result<PathBuf> {
    let relative = original.strip_prefix(volume_root).with_context(|| {
        format!(
            "VSS: путь {} не принадлежит тому {}",
            original.display(),
            volume_root.display()
        )
    })?;
    Ok(device.join(relative))
}

#[cfg(windows)]
mod windows_impl {
    use super::*;
    use anyhow::anyhow;
    use std::collections::HashMap;
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::ptr::{null, null_mut};
    use std::sync::OnceLock;
    use std::time::Instant;
    use winapi::shared::guiddef::{GUID, IID_NULL};
    use winapi::um::vsbackup::{
        CreateVssBackupComponents, IVssBackupComponents, VssFreeSnapshotProperties,
    };
    use winapi::um::vss::{
        IVssAsync, VSS_CTX_FILE_SHARE_BACKUP, VSS_ID, VSS_OBJECT_SNAPSHOT_SET, VSS_SNAPSHOT_PROP,
    };
    use winapi::um::vsserror::{VSS_S_ASYNC_CANCELLED, VSS_S_ASYNC_FINISHED, VSS_S_ASYNC_PENDING};
    use winapi::um::winnt::HRESULT;
    use windows_sys::Win32::Foundation::RPC_E_TOO_LATE;
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetVolumeNameForVolumeMountPointW, GetVolumePathNameW,
    };
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoInitializeSecurity, CoUninitialize, COINIT_MULTITHREADED,
        EOAC_STATIC_CLOAKING, RPC_C_AUTHN_LEVEL_PKT_PRIVACY, RPC_C_IMP_LEVEL_IMPERSONATE,
    };
    use windows_sys::Win32::System::WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE};

    pub struct WindowsVss {
        sources: Vec<SourceMap>,
        volumes: Vec<Volume>,
        snapshot: Option<SnapshotSet>,
        // Снимок освобождается до отключения COM на этом потоке.
        _apartment: ComApartment,
    }

    struct SourceMap {
        original: String,
        target: SourceTarget,
    }

    enum SourceTarget {
        Snapshot {
            canonical: PathBuf,
            volume_root: PathBuf,
            volume: usize,
        },
        Direct,
    }

    impl SourceMap {
        fn scan_path(&self, volumes: &[Volume]) -> Result<PathBuf> {
            match &self.target {
                SourceTarget::Direct => Ok(PathBuf::from(&self.original)),
                SourceTarget::Snapshot {
                    canonical,
                    volume_root,
                    volume,
                } => {
                    let device = volumes[*volume]
                        .device
                        .as_deref()
                        .context("VSS: устройство снимка недоступно")?;
                    snapshot_path(device, volume_root, canonical)
                }
            }
        }
    }

    struct Volume {
        name: PathBuf,
        id: VSS_ID,
        device: Option<PathBuf>,
    }

    impl WindowsVss {
        pub fn create(paths: &[String], timeout: Duration) -> Result<Option<Self>> {
            let (sources, mut volumes) = resolve_sources(paths)?;
            if volumes.is_empty() {
                return Ok(None);
            }
            tracing::info!("VSS: создание снимка для томов: {}", volumes.len());
            let apartment = ComApartment::init()?;
            ensure_com_security()?;
            let components = BackupComponents::create()?;
            components.initialize()?;
            let mut snapshot = SnapshotSet::start(components, volumes.len())?;
            for volume in &mut volumes {
                snapshot.add_volume(volume)?;
            }
            snapshot.create(timeout)?;
            for volume in &mut volumes {
                volume.device = Some(snapshot.device(volume.id)?);
            }
            Ok(Some(Self {
                sources,
                volumes,
                snapshot: Some(snapshot),
                _apartment: apartment,
            }))
        }

        pub fn scan_path(&self, original: &str) -> Result<PathBuf> {
            let source = self
                .sources
                .iter()
                .find(|source| source.original == original)
                .with_context(|| format!("VSS: источник не входил в набор снимков: {original}"))?;
            source.scan_path(&self.volumes)
        }

        pub fn close(mut self) -> Result<()> {
            match self.snapshot.take() {
                Some(snapshot) => snapshot.close(),
                None => Ok(()),
            }
        }
    }

    fn resolve_sources(paths: &[String]) -> Result<(Vec<SourceMap>, Vec<Volume>)> {
        let mut sources = Vec::new();
        let mut volumes = Vec::new();
        let mut by_name = HashMap::new();
        for original in paths {
            if is_remote_path(original) {
                continue;
            }
            let canonical = std::fs::canonicalize(original)
                .with_context(|| format!("VSS: канонизация источника {original}"))?;
            let volume_root = volume_mount_point(&canonical)?;
            let root_wide = wide_null(volume_root.as_os_str());
            // SAFETY: root_wide содержит NUL; вызов синхронный и не сохраняет указатель.
            let drive_type = unsafe { GetDriveTypeW(root_wide.as_ptr()) };
            if drive_type == DRIVE_REMOTE {
                sources.push(SourceMap {
                    original: original.clone(),
                    target: SourceTarget::Direct,
                });
                continue;
            }
            if drive_type != DRIVE_FIXED {
                bail!("VSS: источник {original} находится не на локальном fixed-томе (тип {drive_type})");
            }
            let volume_name = volume_guid_name(&volume_root)?;
            let key = volume_name.to_string_lossy().to_ascii_lowercase();
            let volume = *by_name.entry(key).or_insert_with(|| {
                volumes.push(Volume {
                    name: volume_name,
                    id: IID_NULL,
                    device: None,
                });
                volumes.len() - 1
            });
            sources.push(SourceMap {
                original: original.clone(),
                target: SourceTarget::Snapshot {
                    canonical,
                    volume_root,
                    volume,
                },
            });
        }
        Ok((sources, volumes))
    }

    fn volume_mount_point(path: &Path) -> Result<PathBuf> {
        let input = wide_null(path.as_os_str());
        let mut output = vec![0u16; input.len().max(260)];
        // SAFETY: оба буфера живут до возврата, output имеет достаточную ёмкость по документации API.
        let ok =
            unsafe { GetVolumePathNameW(input.as_ptr(), output.as_mut_ptr(), output.len() as u32) };
        if ok == 0 {
            bail!(
                "GetVolumePathNameW({}): {}",
                path.display(),
                std::io::Error::last_os_error()
            );
        }
        let mut mount = wide_to_path(&output);
        let text = mount.as_os_str().as_encoded_bytes();
        if !text.ends_with(b"\\") {
            mount.as_mut_os_string().push("\\");
        }
        Ok(mount)
    }

    fn volume_guid_name(mount: &Path) -> Result<PathBuf> {
        let mount = wide_null(mount.as_os_str());
        let mut output = vec![0u16; 50];
        // SAFETY: входная строка завершена NUL, выходной буфер соответствует размеру из документации.
        let ok = unsafe {
            GetVolumeNameForVolumeMountPointW(
                mount.as_ptr(),
                output.as_mut_ptr(),
                output.len() as u32,
            )
        };
        if ok == 0 {
            bail!(
                "GetVolumeNameForVolumeMountPointW: {}",
                std::io::Error::last_os_error()
            );
        }
        Ok(wide_to_path(&output))
    }

    fn wide_null(value: &std::ffi::OsStr) -> Vec<u16> {
        value.encode_wide().chain(std::iter::once(0)).collect()
    }

    fn wide_to_path(value: &[u16]) -> PathBuf {
        let len = value.iter().position(|ch| *ch == 0).unwrap_or(value.len());
        OsString::from_wide(&value[..len]).into()
    }

    unsafe fn wide_ptr_to_path(value: *const u16) -> PathBuf {
        let mut len = 0;
        // SAFETY: VSS возвращает NUL-терминированную строку, которая жива до VssFreeSnapshotProperties.
        unsafe {
            while *value.add(len) != 0 {
                len += 1;
            }
            PathBuf::from(OsString::from_wide(std::slice::from_raw_parts(value, len)))
        }
    }

    struct ComApartment;

    impl ComApartment {
        fn init() -> Result<Self> {
            // SAFETY: COM инициализируется и затем отключается на одном и том же потоке.
            let hr = unsafe { CoInitializeEx(null(), COINIT_MULTITHREADED as u32) };
            check_hresult(hr, "CoInitializeEx")?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            // SAFETY: соответствует успешному CoInitializeEx на этом потоке.
            unsafe { CoUninitialize() };
        }
    }

    static COM_SECURITY: OnceLock<Result<(), String>> = OnceLock::new();

    fn ensure_com_security() -> Result<()> {
        COM_SECURITY
            .get_or_init(|| {
                // SAFETY: вызывается один раз за процесс; все необязательные аргументы — NULL.
                let hr = unsafe {
                    CoInitializeSecurity(
                        null_mut(),
                        -1,
                        null(),
                        null(),
                        RPC_C_AUTHN_LEVEL_PKT_PRIVACY,
                        RPC_C_IMP_LEVEL_IMPERSONATE,
                        null(),
                        EOAC_STATIC_CLOAKING as u32,
                        null(),
                    )
                };
                if hr == RPC_E_TOO_LATE {
                    Err(
                        "CoInitializeSecurity: безопасность COM уже настроена другим компонентом"
                            .into(),
                    )
                } else if hr < 0 {
                    Err(format!("CoInitializeSecurity: HRESULT 0x{:08X}", hr as u32))
                } else {
                    Ok(())
                }
            })
            .as_ref()
            .map_err(|e| anyhow!(e.clone()))?;
        Ok(())
    }

    struct BackupComponents(*mut IVssBackupComponents);

    impl BackupComponents {
        fn create() -> Result<Self> {
            let mut components = null_mut();
            // SAFETY: указатель на выходной параметр валиден; интерфейс затем принадлежит RAII-обёртке.
            let hr = unsafe { CreateVssBackupComponents(&mut components) };
            check_hresult(hr, "CreateVssBackupComponents")?;
            if components.is_null() {
                bail!("CreateVssBackupComponents вернул пустой интерфейс");
            }
            Ok(Self(components))
        }

        fn initialize(&self) -> Result<()> {
            // SAFETY: интерфейс жив; для новой backup-операции XML не требуется.
            let hr = unsafe { (*self.0).InitializeForBackup(null_mut()) };
            check_hresult(hr, "IVssBackupComponents::InitializeForBackup")
        }
    }

    impl Drop for BackupComponents {
        fn drop(&mut self) {
            // SAFETY: это последнее обращение к интерфейсу; auto-release удалит снимки, если они остались.
            unsafe { (*self.0).Release() };
        }
    }

    struct SnapshotSet {
        components: BackupComponents,
        id: VSS_ID,
        expected: usize,
        committed: bool,
        active: bool,
    }

    impl SnapshotSet {
        fn start(components: BackupComponents, expected: usize) -> Result<Self> {
            // SAFETY: интерфейс жив; контекст без writers задаёт auto-release, nonpersistent снимок.
            let hr = unsafe {
                (*components.0).SetContext(VSS_CTX_FILE_SHARE_BACKUP as winapi::um::winnt::LONG)
            };
            check_hresult(hr, "IVssBackupComponents::SetContext")?;
            let mut id = IID_NULL;
            // SAFETY: id указывает на доступную для записи GUID-структуру.
            let hr = unsafe { (*components.0).StartSnapshotSet(&mut id) };
            check_hresult(hr, "IVssBackupComponents::StartSnapshotSet")?;
            Ok(Self {
                components,
                id,
                expected,
                committed: false,
                active: true,
            })
        }

        fn add_volume(&self, volume: &mut Volume) -> Result<()> {
            let name = wide_null(volume.name.as_os_str());
            // SAFETY: имя завершено NUL и живёт во время вызова; id — валидный выходной буфер.
            let hr = unsafe {
                (*self.components.0).AddToSnapshotSet(
                    name.as_ptr() as *mut _,
                    IID_NULL,
                    &mut volume.id,
                )
            };
            check_hresult(
                hr,
                format!(
                    "IVssBackupComponents::AddToSnapshotSet({})",
                    volume.name.display()
                ),
            )
        }

        fn create(&mut self, timeout: Duration) -> Result<()> {
            let mut async_ptr = null_mut();
            // SAFETY: выходной указатель валиден; возвращённый IVssAsync сразу переходит под RAII.
            let hr = unsafe { (*self.components.0).DoSnapshotSet(&mut async_ptr) };
            check_hresult(hr, "IVssBackupComponents::DoSnapshotSet")?;
            if async_ptr.is_null() {
                bail!("IVssBackupComponents::DoSnapshotSet вернул пустой IVssAsync");
            }
            let async_ptr = VssAsync(async_ptr);
            if let Err(e) = wait_async(&async_ptr, timeout) {
                // Операция могла завершиться одновременно с отменой: тогда набор уже создан,
                // и очистка должна идти через DeleteSnapshots, а не AbortBackup.
                match wait_terminal(&async_ptr, Duration::from_secs(5)) {
                    Ok(VSS_S_ASYNC_FINISHED) => self.committed = true,
                    Ok(_) => {}
                    Err(cleanup) => {
                        tracing::warn!("VSS: проверка отменённой операции: {cleanup:#}")
                    }
                }
                return Err(e);
            }
            self.committed = true;
            Ok(())
        }

        fn device(&self, snapshot_id: VSS_ID) -> Result<PathBuf> {
            // SAFETY: нулевая структура — допустимое начальное состояние для GetSnapshotProperties.
            let mut properties: VSS_SNAPSHOT_PROP = unsafe { std::mem::zeroed() };
            // SAFETY: properties валидна для записи, snapshot_id получен из AddToSnapshotSet.
            let hr =
                unsafe { (*self.components.0).GetSnapshotProperties(snapshot_id, &mut properties) };
            let result = if hr < 0 {
                Err(anyhow!(
                    "IVssBackupComponents::GetSnapshotProperties: HRESULT 0x{:08X}",
                    hr as u32
                ))
            } else if properties.m_pwszSnapshotDeviceObject.is_null() {
                Err(anyhow!(
                    "VSS: provider вернул пустой путь к устройству снимка"
                ))
            } else {
                // SAFETY: указатель принадлежит properties и используется только до её освобождения.
                Ok(unsafe { wide_ptr_to_path(properties.m_pwszSnapshotDeviceObject) })
            };
            // SAFETY: строки properties освобождаются ровно один раз на всех путях после вызова GetSnapshotProperties.
            unsafe { VssFreeSnapshotProperties(&mut properties) };
            result
        }

        fn finish(&mut self) -> Result<()> {
            if self.committed {
                let mut deleted = 0;
                let mut nondeleted = IID_NULL;
                // SAFETY: set id и оба выходных буфера валидны; удаляется только собственный набор.
                let hr = unsafe {
                    (*self.components.0).DeleteSnapshots(
                        self.id,
                        VSS_OBJECT_SNAPSHOT_SET,
                        1,
                        &mut deleted,
                        &mut nondeleted,
                    )
                };
                check_hresult(hr, "IVssBackupComponents::DeleteSnapshots")?;
                if deleted as usize != self.expected {
                    bail!(
                        "VSS: удалено снимков {deleted} из {}, первый неудалённый: {}",
                        self.expected,
                        format_guid(&nondeleted)
                    );
                }
                Ok(())
            } else {
                // SAFETY: набор был начат, но DoSnapshotSet не завершился успешно; Release следует сразу после этого.
                let hr = unsafe { (*self.components.0).AbortBackup() };
                check_hresult(hr, "IVssBackupComponents::AbortBackup")
            }
        }

        fn close(mut self) -> Result<()> {
            let result = self.finish();
            self.active = false;
            result
        }
    }

    impl Drop for SnapshotSet {
        fn drop(&mut self) {
            if self.active {
                if let Err(e) = self.finish() {
                    tracing::error!("VSS: очистка набора снимков: {e:#}");
                }
                self.active = false;
            }
        }
    }

    struct VssAsync(*mut IVssAsync);

    impl VssAsync {
        fn cancel(&self) -> Result<()> {
            // SAFETY: интерфейс ещё жив; Cancel допустим и для уже завершённой операции.
            let hr = unsafe { (*self.0).Cancel() };
            check_hresult(hr, "IVssAsync::Cancel")
        }

        fn status(&self) -> Result<HRESULT> {
            let mut result = 0;
            // SAFETY: result — валидный выходной буфер; зарезервированный параметр — NULL.
            let hr = unsafe { (*self.0).QueryStatus(&mut result, null_mut()) };
            check_hresult(hr, "IVssAsync::QueryStatus")?;
            Ok(result)
        }
    }

    impl Drop for VssAsync {
        fn drop(&mut self) {
            // SAFETY: интерфейс освобождается один раз после ожидания или отмены.
            unsafe { (*self.0).Release() };
        }
    }

    fn wait_async(async_ptr: &VssAsync, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        loop {
            if let Err(e) = crate::process::check_cancel() {
                if let Err(cancel) = async_ptr.cancel() {
                    tracing::warn!("VSS: отмена операции: {cancel:#}");
                }
                return Err(e.context("создание VSS-снимка отменено"));
            }
            if started.elapsed() >= timeout {
                if let Err(cancel) = async_ptr.cancel() {
                    tracing::warn!("VSS: отмена операции по тайм-ауту: {cancel:#}");
                }
                bail!(
                    "создание VSS-снимка: превышено время ожидания ({})",
                    crate::process::fmt_duration(timeout)
                );
            }
            match async_ptr.status()? {
                VSS_S_ASYNC_FINISHED => return Ok(()),
                VSS_S_ASYNC_PENDING => {
                    let remaining = timeout.saturating_sub(started.elapsed());
                    std::thread::sleep(Duration::from_millis(100).min(remaining));
                }
                VSS_S_ASYNC_CANCELLED => bail!("создание VSS-снимка отменено службой VSS"),
                hr if hr < 0 => check_hresult(hr, "создание VSS-снимка")?,
                other => bail!(
                    "создание VSS-снимка: неожиданный статус 0x{:08X}",
                    other as u32
                ),
            }
        }
    }

    /// После Cancel ограниченно ждёт терминальный статус, не реагируя на отмену задания повторно.
    fn wait_terminal(async_ptr: &VssAsync, timeout: Duration) -> Result<HRESULT> {
        let started = Instant::now();
        loop {
            let status = async_ptr.status()?;
            if status != VSS_S_ASYNC_PENDING || started.elapsed() >= timeout {
                return Ok(status);
            }
            let remaining = timeout.saturating_sub(started.elapsed());
            std::thread::sleep(Duration::from_millis(100).min(remaining));
        }
    }

    fn check_hresult(hr: HRESULT, what: impl std::fmt::Display) -> Result<()> {
        if hr < 0 {
            bail!("{what}: HRESULT 0x{:08X}", hr as u32);
        }
        Ok(())
    }

    fn format_guid(guid: &GUID) -> String {
        let d = &guid.Data4;
        format!(
            "{{{:08X}-{:04X}-{:04X}-{:02X}{:02X}-{:02X}{:02X}{:02X}{:02X}{:02X}{:02X}}}",
            guid.Data1, guid.Data2, guid.Data3, d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn mapped_remote_source_stays_direct_next_to_a_snapshot() {
            let volumes = vec![Volume {
                name: PathBuf::from(r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\"),
                id: IID_NULL,
                device: Some(PathBuf::from(
                    r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42",
                )),
            }];
            for original in [r"Z:\mapped\Docs", r"//server/share/Docs"] {
                let source = SourceMap {
                    original: original.into(),
                    target: SourceTarget::Direct,
                };
                assert_eq!(source.scan_path(&volumes).unwrap(), PathBuf::from(original));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    #[test]
    fn remote_sources_never_use_local_vss() {
        assert!(is_remote_path("https://cloud.example.ru/dav"));
        assert!(is_remote_path("http://cloud.example.ru/dav"));
        assert!(is_remote_path(r"\\server\share\Docs"));
        assert!(is_remote_path(r"\\?\UNC\server\share\Docs"));
        assert!(!is_remote_path(r"C:\Docs"));
        assert!(!is_remote_path(r"\\?\C:\Docs"));
        assert!(!is_remote_path(
            r"\\?\Volume{12345678-1234-1234-1234-123456789abc}\Docs"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_path_keeps_source_below_volume_root() {
        let path = snapshot_path(
            Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42"),
            Path::new(r"\\?\E:\"),
            Path::new(r"\\?\E:\Docs\Project"),
        )
        .unwrap();
        assert_eq!(
            path,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42\Docs\Project")
        );
    }

    #[cfg(windows)]
    #[test]
    fn snapshot_path_accepts_volume_root_and_rejects_another_volume() {
        let root = snapshot_path(
            Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42\"),
            Path::new(r"E:\"),
            Path::new(r"E:\"),
        )
        .unwrap();
        assert_eq!(
            root,
            PathBuf::from(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42\")
        );
        assert!(snapshot_path(
            Path::new(r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy42"),
            Path::new(r"E:\"),
            Path::new(r"D:\Docs")
        )
        .is_err());
    }

    #[test]
    fn disabled_vss_leaves_path_unchanged() {
        let set = VssSet::disabled();
        assert_eq!(
            set.scan_path(r"C:\Docs").unwrap(),
            PathBuf::from(r"C:\Docs")
        );
    }

    #[cfg(windows)]
    fn temp_source(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "steady-vss-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("busy.txt"), b"snapshot").unwrap();
        dir
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "требует права администратора и доступную службу VSS"]
    fn snapshot_device_disappears_after_close() {
        let source = temp_source("close");
        let paths = vec![source.to_string_lossy().into_owned()];
        let set = VssSet::create(&paths, Duration::from_secs(120)).unwrap();
        let snapshot_file = set.scan_path(&paths[0]).unwrap().join("busy.txt");
        assert!(
            snapshot_file.exists(),
            "файл не найден в снимке: {}",
            snapshot_file.display()
        );
        set.close().unwrap();
        assert!(
            !snapshot_file.exists(),
            "устройство снимка осталось после DeleteSnapshots"
        );
        let _ = std::fs::remove_dir_all(source);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "требует права администратора и доступную службу VSS"]
    fn create_honors_cancellation() {
        let source = temp_source("cancel");
        let paths = vec![source.to_string_lossy().into_owned()];
        let flag = crate::process::CancelFlag::new();
        let _guard = crate::process::enter_cancel(flag.clone());
        flag.request("тестовая отмена VSS");
        match VssSet::create(&paths, Duration::from_secs(120)) {
            Ok(set) => {
                let _ = set.close();
                panic!("создание снимка не заметило отмену");
            }
            Err(e) => assert!(format!("{e:#}").contains("отмен"), "{e:#}"),
        }
        let _ = std::fs::remove_dir_all(source);
    }
}
