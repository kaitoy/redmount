//! WinFsp adapter: maps `FileSystemContext` calls onto the `fs` core.
use crate::fs::{E, RedmineFs, Stat};
use anyhow::Context;
use std::ffi::c_void;
use std::sync::Mutex;
use windows::Win32::Foundation::{
    STATUS_ACCESS_DENIED, STATUS_DIRECTORY_NOT_EMPTY, STATUS_END_OF_FILE, STATUS_IO_DEVICE_ERROR,
    STATUS_NOT_A_DIRECTORY, STATUS_OBJECT_NAME_COLLISION, STATUS_OBJECT_NAME_NOT_FOUND,
    STATUS_OBJECT_PATH_NOT_FOUND,
};
use winfsp::constants::FspCleanupFlags;
use winfsp::filesystem::{
    DirInfo, DirMarker, FileInfo, FileSecurity, FileSystemContext, OpenFileInfo, VolumeInfo,
    WideNameInfo,
};
use winfsp::host::{FileSystemHost, VolumeParams};
use winfsp::{FspError, U16CStr};

type Res<T> = winfsp::Result<T>;

const DIR: u32 = 0x10; // FILE_ATTRIBUTE_DIRECTORY
const FILE: u32 = 0x20; // FILE_ATTRIBUTE_ARCHIVE
const FILE_DIRECTORY_FILE: u32 = 0x1;

impl From<E> for FspError {
    fn from(e: E) -> Self {
        FspError::NTSTATUS(
            match e {
                E::NotFound => STATUS_OBJECT_NAME_NOT_FOUND,
                E::PathNotFound => STATUS_OBJECT_PATH_NOT_FOUND,
                E::Denied => STATUS_ACCESS_DENIED,
                E::Exists => STATUS_OBJECT_NAME_COLLISION,
                E::NotDir => STATUS_NOT_A_DIRECTORY,
                E::NotEmpty => STATUS_DIRECTORY_NOT_EMPTY,
                E::Io => STATUS_IO_DEVICE_ERROR,
            }
            .0,
        )
    }
}

fn fi(s: Stat) -> FileInfo {
    FileInfo {
        file_attributes: if s.dir { DIR } else { FILE },
        file_size: s.size,
        allocation_size: s.size.div_ceil(4096) * 4096,
        creation_time: s.created,
        last_access_time: s.modified,
        last_write_time: s.modified,
        change_time: s.modified,
        index_number: s.ino,
        ..Default::default()
    }
}

pub struct Handle {
    path: Mutex<String>,
}

impl Handle {
    fn new(path: String) -> Self {
        Handle {
            path: Mutex::new(path),
        }
    }
    fn path(&self) -> String {
        self.path.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    fn set_path(&self, p: String) {
        *self.path.lock().unwrap_or_else(|e| e.into_inner()) = p;
    }
}

pub struct WinFs {
    fs: RedmineFs,
    sd: Vec<u8>,
}

impl WinFs {
    fn copy_sd(&self, buf: Option<&mut [c_void]>) {
        if let Some(buf) = buf
            && buf.len() >= self.sd.len()
        {
            // SAFETY: buf holds at least sd.len() bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.sd.as_ptr(),
                    buf.as_mut_ptr().cast::<u8>(),
                    self.sd.len(),
                )
            };
        }
    }
}

impl FileSystemContext for WinFs {
    type FileContext = Handle;

    fn get_security_by_name(
        &self,
        file_name: &U16CStr,
        security_descriptor: Option<&mut [c_void]>,
        _reparse_point_resolver: impl FnOnce(&U16CStr) -> Option<FileSecurity>,
    ) -> Res<FileSecurity> {
        let dir = self.fs.is_dir(&file_name.to_string_lossy())?;
        self.copy_sd(security_descriptor);
        Ok(FileSecurity {
            reparse: false,
            sz_security_descriptor: self.sd.len() as u64,
            attributes: if dir { DIR } else { FILE },
        })
    }

    fn open(
        &self,
        file_name: &U16CStr,
        _create_options: u32,
        _granted_access: u32,
        file_info: &mut OpenFileInfo,
    ) -> Res<Handle> {
        let path = file_name.to_string_lossy();
        *file_info.as_mut() = fi(self.fs.open(&path)?);
        Ok(Handle::new(path))
    }

    fn close(&self, h: Handle) {
        self.fs.close(&h.path());
    }

    fn create(
        &self,
        file_name: &U16CStr,
        create_options: u32,
        _granted_access: u32,
        _file_attributes: u32,
        _security_descriptor: Option<&[c_void]>,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        _extra_buffer_is_reparse_point: bool,
        file_info: &mut OpenFileInfo,
    ) -> Res<Handle> {
        let path = file_name.to_string_lossy();
        *file_info.as_mut() = fi(self
            .fs
            .create(&path, create_options & FILE_DIRECTORY_FILE != 0)?);
        Ok(Handle::new(path))
    }

    fn cleanup(&self, h: &Handle, _file_name: Option<&U16CStr>, flags: u32) {
        self.fs.cleanup(
            &h.path(),
            FspCleanupFlags::FspCleanupDelete.is_flagged(flags),
        );
    }

    fn flush(&self, h: Option<&Handle>, file_info: &mut FileInfo) -> Res<()> {
        match h {
            Some(h) => self.get_file_info(h, file_info),
            None => Ok(()),
        }
    }

    fn get_file_info(&self, h: &Handle, file_info: &mut FileInfo) -> Res<()> {
        *file_info = fi(self.fs.stat(&h.path())?);
        Ok(())
    }

    fn get_security(&self, _h: &Handle, security_descriptor: Option<&mut [c_void]>) -> Res<u64> {
        self.copy_sd(security_descriptor);
        Ok(self.sd.len() as u64)
    }

    fn overwrite(
        &self,
        h: &Handle,
        _file_attributes: u32,
        _replace_file_attributes: bool,
        _allocation_size: u64,
        _extra_buffer: Option<&[u8]>,
        file_info: &mut FileInfo,
    ) -> Res<()> {
        *file_info = fi(self.fs.set_size(&h.path(), 0, false)?);
        Ok(())
    }

    fn read_directory(
        &self,
        h: &Handle,
        _pattern: Option<&U16CStr>,
        marker: DirMarker,
        buffer: &mut [u8],
    ) -> Res<u32> {
        let path = h.path();
        let mut entries = self.fs.read_dir(&path)?;
        if !path.trim_matches('\\').is_empty() {
            let me = self.fs.stat(&path)?;
            let parent_path = path.rsplit_once('\\').map_or("", |(p, _)| p);
            let parent = self.fs.stat(parent_path)?;
            entries.insert(0, ("..".to_string(), parent));
            entries.insert(0, (".".to_string(), me));
        }
        let start = match marker.inner_as_cstr() {
            None => 0,
            Some(m) => {
                let m = m.to_string_lossy();
                match entries.iter().position(|(n, _)| *n == m) {
                    Some(i) => i + 1,
                    None => entries
                        .iter()
                        .position(|(n, _)| n.to_lowercase() > m.to_lowercase())
                        .unwrap_or(entries.len()),
                }
            }
        };
        let mut cursor = 0u32;
        for (name, s) in &entries[start..] {
            let mut di = DirInfo::<255>::new();
            *di.file_info_mut() = fi(*s);
            di.set_name(name)?;
            if !di.append_to_buffer(buffer, &mut cursor) {
                return Ok(cursor);
            }
        }
        DirInfo::<255>::finalize_buffer(buffer, &mut cursor);
        Ok(cursor)
    }

    fn rename(
        &self,
        h: &Handle,
        file_name: &U16CStr,
        new_file_name: &U16CStr,
        replace_if_exists: bool,
    ) -> Res<()> {
        let to = new_file_name.to_string_lossy();
        self.fs
            .rename(&file_name.to_string_lossy(), &to, replace_if_exists)?;
        h.set_path(to);
        Ok(())
    }

    fn set_basic_info(
        &self,
        h: &Handle,
        _file_attributes: u32,
        _creation_time: u64,
        _last_access_time: u64,
        _last_write_time: u64,
        _last_change_time: u64,
        file_info: &mut FileInfo,
    ) -> Res<()> {
        self.get_file_info(h, file_info)
    }

    fn set_delete(&self, h: &Handle, _file_name: &U16CStr, delete_file: bool) -> Res<()> {
        if delete_file {
            self.fs.check_delete(&h.path())?;
        }
        Ok(())
    }

    fn set_file_size(
        &self,
        h: &Handle,
        new_size: u64,
        set_allocation_size: bool,
        file_info: &mut FileInfo,
    ) -> Res<()> {
        *file_info = fi(self.fs.set_size(&h.path(), new_size, set_allocation_size)?);
        Ok(())
    }

    fn read(&self, h: &Handle, buffer: &mut [u8], offset: u64) -> Res<u32> {
        let n = self.fs.read(&h.path(), buffer, offset)?;
        if n == 0 && !buffer.is_empty() {
            return Err(FspError::NTSTATUS(STATUS_END_OF_FILE.0));
        }
        Ok(n as u32)
    }

    fn write(
        &self,
        h: &Handle,
        buffer: &[u8],
        offset: u64,
        write_to_eof: bool,
        constrained_io: bool,
        file_info: &mut FileInfo,
    ) -> Res<u32> {
        let (n, s) = self
            .fs
            .write(&h.path(), buffer, offset, write_to_eof, constrained_io)?;
        *file_info = fi(s);
        Ok(n as u32)
    }

    fn get_volume_info(&self, out: &mut VolumeInfo) -> Res<()> {
        out.total_size = 1 << 40;
        out.free_size = 1 << 39;
        out.set_volume_label("Redmine");
        Ok(())
    }
}

pub fn run(fs: RedmineFs, mount: &str) -> anyhow::Result<()> {
    // winfsp_init loads winfsp-x64.dll from the DLL search path.
    // ponytail: assumes the default WinFsp install dir; read HKLM\SOFTWARE\WOW6432Node\WinFsp\InstallDir if it moves
    let bin = format!(
        r"{}\WinFsp\bin",
        std::env::var("ProgramFiles(x86)").unwrap_or_default()
    );
    // SAFETY: still single-threaded here.
    unsafe {
        std::env::set_var(
            "PATH",
            format!("{bin};{}", std::env::var("PATH").unwrap_or_default()),
        )
    };
    let _init = winfsp::winfsp_init().context("Failed to load WinFsp. Please install WinFsp")?;
    let mut vp = VolumeParams::new();
    vp.sector_size(4096)
        .sectors_per_allocation_unit(1)
        .case_sensitive_search(false)
        .case_preserved_names(true)
        .unicode_on_disk(true)
        .persistent_acls(true)
        .post_cleanup_when_modified_only(true)
        // file contents change on the server; don't let the kernel serve what we wrote last time
        .flush_and_purge_on_cleanup(true)
        .file_info_timeout(1000)
        .filesystem_name("redmount");
    let mut host: FileSystemHost<WinFs> = FileSystemHost::new(
        vp,
        WinFs {
            fs,
            sd: security_descriptor()?,
        },
    )?;
    host.mount(mount)?;
    host.start()?;
    println!("Mounted on {mount}. Press Enter to unmount.");
    crate::wait()?;
    host.stop();
    host.unmount();
    Ok(())
}

fn security_descriptor() -> anyhow::Result<Vec<u8>> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    use windows::Win32::Security::{GetSecurityDescriptorLength, PSECURITY_DESCRIPTOR};
    let mut sd = PSECURITY_DESCRIPTOR::default();
    // SAFETY: plain Win32 calls; the descriptor is read right after creation.
    // ponytail: the LocalAlloc'd descriptor is leaked once at startup
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            windows::core::w!("O:BAG:BAD:P(A;;FA;;;SY)(A;;FA;;;BA)(A;;FA;;;WD)"),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )?;
        Ok(
            std::slice::from_raw_parts(sd.0 as *const u8, GetSecurityDescriptorLength(sd) as usize)
                .to_vec(),
        )
    }
}
