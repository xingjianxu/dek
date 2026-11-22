use std::{ffi::CString, mem::MaybeUninit, ptr};

use dek_target::backend::BackendError;

pub(super) fn user_id(name: &str) -> Result<Option<u32>, BackendError> {
    account_id(
        name,
        |name, entry: *mut libc::passwd, buf, size, found| {
            // NSS writes into the provided entry, buffer and result pointer.
            unsafe { libc::getpwnam_r(name, entry, buf, size, found) }
        },
        |entry| entry.pw_uid,
    )
}

pub(super) fn group_id(name: &str) -> Result<Option<u32>, BackendError> {
    account_id(
        name,
        |name, entry: *mut libc::group, buf, size, found| {
            // NSS writes into the provided entry, buffer and result pointer.
            unsafe { libc::getgrnam_r(name, entry, buf, size, found) }
        },
        |entry| entry.gr_gid,
    )
}

fn account_id<T>(
    name: &str,
    lookup: impl Fn(*const libc::c_char, *mut T, *mut libc::c_char, usize, *mut *mut T) -> i32,
    id: impl Fn(&T) -> u32,
) -> Result<Option<u32>, BackendError> {
    let name = CString::new(name).map_err(|_| BackendError("账户名称无效".into()))?;
    let mut bytes = vec![0_u8; 4096];
    loop {
        let mut entry = MaybeUninit::uninit();
        let mut found = ptr::null_mut();
        match lookup(
            name.as_ptr(),
            entry.as_mut_ptr(),
            bytes.as_mut_ptr().cast(),
            bytes.len(),
            &mut found,
        ) {
            0 if found.is_null() => return Ok(None),
            // A successful NSS lookup with a non-null result initializes entry.
            0 => return Ok(Some(id(unsafe { entry.assume_init_ref() }))),
            libc::ERANGE if bytes.len() < 1024 * 1024 => bytes.resize(bytes.len() * 2, 0),
            error => {
                return Err(BackendError(format!(
                    "无法解析账户 {:?}：{}",
                    name,
                    std::io::Error::from_raw_os_error(error)
                )));
            }
        }
    }
}
