use std::{
    ffi::{CStr, CString},
    mem::MaybeUninit,
    ptr,
};

use dek_target::backend::BackendError;

#[derive(Clone, Copy)]
pub(super) enum Key<'a> {
    Name(&'a str),
    Id(u32),
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct Identity {
    pub(super) name: String,
    pub(super) id: u32,
}

pub(super) trait NameService {
    fn user(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError>;
    fn group(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError>;
}

pub(super) struct SystemNss;

impl NameService for SystemNss {
    fn user(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError> {
        let name = lookup_name(key)?;
        lookup(
            |entry: *mut libc::passwd, buf, size, found| {
                // NSS initializes the provided entry and buffer on a successful lookup.
                unsafe {
                    match key {
                        Key::Name(_) => libc::getpwnam_r(name.as_ptr(), entry, buf, size, found),
                        Key::Id(id) => libc::getpwuid_r(id, entry, buf, size, found),
                    }
                }
            },
            |entry| identity(entry.pw_name, entry.pw_uid),
        )
    }

    fn group(&self, key: Key<'_>) -> Result<Option<Identity>, BackendError> {
        let name = lookup_name(key)?;
        lookup(
            |entry: *mut libc::group, buf, size, found| {
                // NSS initializes the provided entry and buffer on a successful lookup.
                unsafe {
                    match key {
                        Key::Name(_) => libc::getgrnam_r(name.as_ptr(), entry, buf, size, found),
                        Key::Id(id) => libc::getgrgid_r(id, entry, buf, size, found),
                    }
                }
            },
            |entry| identity(entry.gr_name, entry.gr_gid),
        )
    }
}

pub fn user_id(name: &str) -> Result<Option<u32>, BackendError> {
    Ok(SystemNss.user(Key::Name(name))?.map(|entry| entry.id))
}

pub fn group_id(name: &str) -> Result<Option<u32>, BackendError> {
    Ok(SystemNss.group(Key::Name(name))?.map(|entry| entry.id))
}

fn lookup_name(key: Key<'_>) -> Result<CString, BackendError> {
    let name = match key {
        Key::Name(name) => name,
        Key::Id(_) => "",
    };
    CString::new(name).map_err(|_| BackendError("账户名称无效".into()))
}

fn identity(name: *const libc::c_char, id: u32) -> Result<Identity, BackendError> {
    if name.is_null() {
        return Err(BackendError("NSS 结果缺少名称".into()));
    }
    // The successful NSS call supplied a NUL-terminated name in its retained buffer.
    let name = unsafe { CStr::from_ptr(name) }
        .to_str()
        .map_err(|_| BackendError("NSS 账户名称不是有效的 UTF-8".into()))?;
    Ok(Identity {
        name: name.into(),
        id,
    })
}

fn lookup<T>(
    call: impl Fn(*mut T, *mut libc::c_char, usize, *mut *mut T) -> i32,
    extract: impl Fn(&T) -> Result<Identity, BackendError>,
) -> Result<Option<Identity>, BackendError> {
    let mut bytes = vec![0_u8; 4096];
    loop {
        let mut entry = MaybeUninit::uninit();
        let mut found = ptr::null_mut();
        match call(
            entry.as_mut_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut found,
        ) {
            0 if found.is_null() => return Ok(None),
            // A successful result initializes the supplied entry until the buffer is dropped.
            0 => return extract(unsafe { entry.assume_init_ref() }).map(Some),
            libc::ERANGE if bytes.len() < 1024 * 1024 => bytes.resize(bytes.len() * 2, 0),
            error => {
                return Err(BackendError(format!(
                    "NSS 查询失败：{}",
                    std::io::Error::from_raw_os_error(error)
                )));
            }
        }
    }
}
