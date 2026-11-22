//! The libalpm 16 ABI used for read-only package transaction preparation.

use std::ffi::{CStr, c_char, c_int, c_ushort, c_void};

use dek_target::backend::BackendError;

pub(super) enum Handle {}
pub(super) enum Database {}
pub(super) enum Package {}
pub(super) enum Dependency {}

#[repr(C)]
pub(super) struct List {
    pub data: *mut c_void,
    pub prev: *mut List,
    pub next: *mut List,
}

#[repr(C)]
pub(super) struct Question {
    pub kind: c_int,
    pub answer: c_int,
}

#[repr(C)]
pub(super) struct ReplaceQuestion {
    pub question: Question,
    pub old: *mut Package,
    pub new: *mut Package,
    pub database: *mut Database,
}

#[repr(C)]
pub(super) struct MissingDependency {
    pub target: *const c_char,
    pub depend: *const Dependency,
    pub causes: *const c_char,
}

#[repr(C)]
pub(super) struct DependencyConflict {
    pub first: *mut Package,
    pub second: *mut Package,
    pub reason: *const Dependency,
}

struct Library(*mut c_void);

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: this handle was returned by dlopen and is owned here.
        unsafe { libc::dlclose(self.0) };
    }
}

type QuestionCallback = unsafe extern "C" fn(*mut c_void, *mut Question);

macro_rules! bindings {
    ($(fn $symbol:ident($($argument:ty),*) -> $ret:ty;)*) => {
        pub(super) struct Native {
            $(pub $symbol: unsafe extern "C" fn($($argument),*) -> $ret,)*
            _library: Library,
        }

        impl Native {
            pub fn load() -> Result<Self, BackendError> {
                // SAFETY: use the distribution ABI and let the dynamic loader resolve its dependencies.
                let handle = unsafe { libc::dlopen(c"/usr/lib/libalpm.so.16".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
                if handle.is_null() {
                    // SAFETY: dlerror returns either null or a NUL-terminated diagnostic.
                    let message = unsafe { text(libc::dlerror()) }.unwrap_or_default();
                    return Err(BackendError(format!("无法加载 Arch libalpm 16：{message}")));
                }
                let library = Library(handle);
                $(
                    let symbol = concat!(stringify!($symbol), "\0");
                    // SAFETY: symbol is a NUL-terminated name and library remains open.
                    let address = unsafe { libc::dlsym(handle, symbol.as_ptr().cast()) };
                    if address.is_null() {
                        return Err(BackendError(format!("Arch libalpm 缺少 {}", stringify!($symbol))));
                    }
                    // SAFETY: the installed libalpm 16 defines this symbol with the declared C ABI.
                    let $symbol = unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($argument),*) -> $ret>(address) };
                )*
                Ok(Self { $($symbol,)* _library: library })
            }
        }
    };
}

bindings! {
    fn alpm_initialize(*const c_char, *const c_char, *mut c_int) -> *mut Handle;
    fn alpm_release(*mut Handle) -> c_int;
    fn alpm_errno(*mut Handle) -> c_int;
    fn alpm_strerror(c_int) -> *const c_char;
    fn alpm_option_set_gpgdir(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_set_logfile(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_set_default_siglevel(*mut Handle, c_int) -> c_int;
    fn alpm_option_set_sandboxuser(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_set_disable_dl_timeout(*mut Handle, c_ushort) -> c_int;
    fn alpm_option_set_disable_sandbox_filesystem(*mut Handle, c_ushort) -> c_int;
    fn alpm_option_set_disable_sandbox_syscalls(*mut Handle, c_ushort) -> c_int;
    fn alpm_option_add_architecture(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_add_ignorepkg(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_add_ignoregroup(*mut Handle, *const c_char) -> c_int;
    fn alpm_option_set_questioncb(*mut Handle, Option<QuestionCallback>, *mut c_void) -> c_int;
    fn alpm_get_localdb(*mut Handle) -> *mut Database;
    fn alpm_register_syncdb(*mut Handle, *const c_char, c_int) -> *mut Database;
    fn alpm_db_set_usage(*mut Database, c_int) -> c_int;
    fn alpm_db_add_server(*mut Database, *const c_char) -> c_int;
    fn alpm_db_get_name(*mut Database) -> *const c_char;
    fn alpm_db_get_valid(*mut Database) -> c_int;
    fn alpm_db_get_pkgcache(*mut Database) -> *mut List;
    fn alpm_db_update(*mut Handle, *mut List, c_int) -> c_int;
    fn alpm_db_get_pkg(*mut Database, *const c_char) -> *mut Package;
    fn alpm_list_free(*mut List) -> ();
    fn alpm_pkg_get_name(*mut Package) -> *const c_char;
    fn alpm_pkg_get_version(*mut Package) -> *const c_char;
    fn alpm_pkg_get_sha256sum(*mut Package) -> *const c_char;
    fn alpm_pkg_get_db(*mut Package) -> *mut Database;
    fn alpm_pkg_vercmp(*const c_char, *const c_char) -> c_int;
    fn alpm_pkg_should_ignore(*mut Handle, *mut Package) -> c_int;
    fn alpm_trans_init(*mut Handle, c_int) -> c_int;
    fn alpm_sync_sysupgrade(*mut Handle, c_int) -> c_int;
    fn alpm_add_pkg(*mut Handle, *mut Package) -> c_int;
    fn alpm_trans_prepare(*mut Handle, *mut *mut List) -> c_int;
    fn alpm_trans_get_add(*mut Handle) -> *mut List;
    fn alpm_trans_get_remove(*mut Handle) -> *mut List;
    fn alpm_trans_release(*mut Handle) -> c_int;
    fn alpm_dep_compute_string(*const Dependency) -> *mut c_char;
    fn alpm_depmissing_free(*mut MissingDependency) -> ();
    fn alpm_conflict_free(*mut DependencyConflict) -> ();
}

/// The caller supplies a live NUL-terminated C string or null.
pub(super) unsafe fn text(ptr: *const c_char) -> Option<String> {
    if ptr.is_null() {
        None
    } else {
        // SAFETY: the calling convention guarantees a C string.
        Some(
            unsafe { CStr::from_ptr(ptr) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// The caller keeps the list and its contents alive for the entire iteration.
pub(super) unsafe fn items<T>(list: *mut List) -> impl Iterator<Item = *mut T> {
    std::iter::successors((!list.is_null()).then_some(list), |node| {
        // SAFETY: every node is a live libalpm list node.
        let next = unsafe { (**node).next };
        (!next.is_null()).then_some(next)
    })
    // SAFETY: libalpm uses one data type throughout each of these lists.
    .map(|node| unsafe { (*node).data.cast() })
}
