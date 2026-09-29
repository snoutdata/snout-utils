//! The settings. All of them are `sighup`: a server's configuration (or its command line) sets
//! them, a reload changes them, and no session can, which is what makes them safe to trust.
use crate::names;
use pgrx::pg_sys;
use std::ffi::{CStr, c_char, c_void};

static mut SUPERUSER: *mut c_char = std::ptr::null_mut();
static mut PRIVILEGED_ROLE: *mut c_char = std::ptr::null_mut();
static mut PRIVILEGED_EXTENSIONS: *mut c_char = std::ptr::null_mut();
static mut EXTENSION_CUSTOM_SCRIPTS_PATH: *mut c_char = std::ptr::null_mut();
static mut PRIVILEGED_ROLE_ALLOWED_CONFIGS: *mut c_char = std::ptr::null_mut();

/// The role delegated statements run as. Unset, the role that created the cluster.
pub fn superuser() -> Option<&'static CStr> {
	read(&raw const SUPERUSER)
}

/// The role (and every role with its privileges) that may do what a superuser may, within limits.
pub fn privileged_role() -> Option<&'static CStr> {
	read(&raw const PRIVILEGED_ROLE)
}

/// The extensions a privileged role may create, update, move and drop as the superuser.
pub fn privileged_extensions() -> Option<&'static str> {
	read(&raw const PRIVILEGED_EXTENSIONS).and_then(|s| s.to_str().ok())
}

/// Where the scripts that run around `CREATE EXTENSION` live, one folder per extension.
pub fn extension_custom_scripts_path() -> Option<&'static str> {
	read(&raw const EXTENSION_CUSTOM_SCRIPTS_PATH)
		.and_then(|s| s.to_str().ok())
		.filter(|s| !s.is_empty())
}

/// Superuser-only settings a privileged role may set anyway.
pub fn privileged_role_allowed_configs() -> Option<&'static str> {
	read(&raw const PRIVILEGED_ROLE_ALLOWED_CONFIGS).and_then(|s| s.to_str().ok())
}

fn read(setting: *const *mut c_char) -> Option<&'static CStr> {
	// SAFETY: Postgres owns the string and replaces the pointer only between statements.
	let value = unsafe { *setting };
	if value.is_null() {
		None
	} else {
		Some(unsafe { CStr::from_ptr(value) })
	}
}

pub fn init() {
	unsafe {
		define(
			c"snout_utils.superuser",
			c"The superuser a delegated statement runs as",
			&raw mut SUPERUSER,
			None,
		);
		define(
			c"snout_utils.privileged_role",
			c"The role that may run some superuser-only statements, as snout_utils.superuser",
			&raw mut PRIVILEGED_ROLE,
			None,
		);
		define(
			c"snout_utils.privileged_extensions",
			c"Extensions the privileged role may create, update, move and drop, comma-separated",
			&raw mut PRIVILEGED_EXTENSIONS,
			Some(check_list),
		);
		define(
			c"snout_utils.extension_custom_scripts_path",
			c"Folder of scripts run as the superuser around CREATE EXTENSION",
			&raw mut EXTENSION_CUSTOM_SCRIPTS_PATH,
			None,
		);
		define(
			c"snout_utils.privileged_role_allowed_configs",
			c"Superuser-only settings the privileged role may set, comma-separated; a trailing * is a prefix",
			&raw mut PRIVILEGED_ROLE_ALLOWED_CONFIGS,
			Some(check_list),
		);
		// Any other snout_utils.* name is a mistake in the configuration, and Postgres says so.
		pg_sys::MarkGUCPrefixReserved(c"snout_utils".as_ptr());
	}
}

unsafe fn define(
	name: &'static CStr,
	description: &'static CStr,
	variable: *mut *mut c_char,
	check: pg_sys::GucStringCheckHook,
) {
	unsafe {
		pg_sys::DefineCustomStringVariable(
			name.as_ptr(),
			description.as_ptr(),
			std::ptr::null(),
			variable,
			std::ptr::null(),
			pg_sys::GucContext::PGC_SIGHUP,
			0,
			check,
			None,
			None,
		);
	}
}

/// A list setting that does not parse is refused, and the old value stays, rather than being half
/// read. (The check runs in the postmaster too, where an ERROR would stop the server.)
#[pgrx::pg_guard]
unsafe extern "C-unwind" fn check_list(
	value: *mut *mut c_char,
	_extra: *mut *mut c_void,
	_source: pg_sys::GucSource::Type,
) -> bool {
	let text = unsafe { *value };
	if text.is_null() {
		return true;
	}
	let ok = unsafe { CStr::from_ptr(text) }
		.to_str()
		.ok()
		.and_then(names::split)
		.is_some();
	if !ok {
		unsafe {
			pg_sys::GUC_check_errdetail_string =
				pg_sys::pstrdup(c"The value must be a comma-separated list of names.".as_ptr());
		}
	}
	ok
}
