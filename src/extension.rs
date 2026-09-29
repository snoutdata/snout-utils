//! What `CREATE EXTENSION` needs decided before it runs: whether the extension is installed on this
//! server, and the text of the scripts that run around it.
use crate::hook::refusal;
use crate::raw::Refusal;
use crate::{names, script, settings};
use pgrx::PgSqlErrorCode;
use pgrx::pg_sys;
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Whether `name` has a control file in this server's extension directory, which is what
/// `pg_available_extensions` lists.
///
/// This is what stops a delegated statement running code nobody installed: an extension defined
/// inside the database (by pg_tle or anything like it) has no control file, so it is never created,
/// updated or dropped as the superuser, whatever the list says.
pub fn is_available(name: &str) -> bool {
	names::is_extension_name(name) && extension_dir().join(format!("{name}.control")).is_file()
}

fn extension_dir() -> &'static Path {
	static DIR: OnceLock<PathBuf> = OnceLock::new();
	DIR.get_or_init(|| {
		let mut share = [0 as c_char; pg_sys::MAXPGPATH as usize];
		unsafe {
			pg_sys::get_share_path((&raw const pg_sys::my_exec_path).cast(), share.as_mut_ptr());
			PathBuf::from(
				CStr::from_ptr(share.as_ptr())
					.to_string_lossy()
					.into_owned(),
			)
			.join("extension")
		}
	})
}

/// The three scripts a `CREATE EXTENSION` may run, in Postgres's memory (`null` where there is
/// none), so they can be carried into a frame that holds nothing to drop.
#[derive(Clone, Copy)]
pub struct Scripts {
	pub before_all: *const c_char,
	pub before: *const c_char,
	/// Either the script to run after the statement, or the refusal to raise there instead: a value
	/// that may not go into it is refused once the statement has run, not before, so whatever the
	/// statement itself says (an "already exists" NOTICE) is said first.
	pub after: Result<*const c_char, Refusal>,
}

impl Scripts {
	pub const NONE: Scripts = Scripts {
		before_all: std::ptr::null(),
		before: std::ptr::null(),
		after: Ok(std::ptr::null()),
	};
}

/// Reads and fills in the scripts for `CREATE EXTENSION <name>` with `options`: one run before
/// every extension (`before-create.sql` at the top of the folder), and the extension's own before
/// and after (`<name>/before-create.sql`, `<name>/after-create.sql`).
pub unsafe fn prepare(name: &str, options: *mut pg_sys::List) -> Scripts {
	let Some(folder) = settings::extension_custom_scripts_path() else {
		return Scripts::NONE;
	};
	let folder = Path::new(folder);
	let (schema, version, cascade) = unsafe { read_options(options) };
	let values = script::Values {
		name: Some(name),
		schema: schema.as_deref(),
		version: version.as_deref(),
		cascade,
	};
	// A name that could leave the folder has no script of its own; Postgres refuses the name anyway.
	let own = names::is_extension_name(name);
	// Before the statement a refusal is raised here, as an ordinary ERROR (this is the deciding
	// half, where Rust unwinds); after it, the refusal is carried to where the script would run.
	let now = |path: &Path| {
		load(path, &values).unwrap_or_else(|message| {
			pgrx::ereport!(
				ERROR,
				PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION,
				message
			);
		})
	};
	let after = |path: &Path| {
		load(path, &values).map_err(|message| {
			refusal(
				PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION,
				&message,
				None,
			)
		})
	};
	Scripts {
		before_all: now(&folder.join("before-create.sql")),
		before: if own {
			now(&folder.join(name).join("before-create.sql"))
		} else {
			std::ptr::null()
		},
		after: if own {
			after(&folder.join(name).join("after-create.sql"))
		} else {
			Ok(std::ptr::null())
		},
	}
}

unsafe fn read_options(options: *mut pg_sys::List) -> (Option<String>, Option<String>, bool) {
	let (mut schema, mut version, mut cascade) = (None, None, false);
	for option in unsafe { crate::list::pointers::<pg_sys::DefElem>(options) } {
		let name = unsafe { CStr::from_ptr((*option).defname) }.to_bytes();
		match name {
			b"schema" => schema = Some(unsafe { owned(pg_sys::defGetString(option)) }),
			b"new_version" => version = Some(unsafe { owned(pg_sys::defGetString(option)) }),
			b"cascade" => cascade = unsafe { pg_sys::defGetBoolean(option) },
			_ => {}
		}
	}
	(schema, version, cascade)
}

unsafe fn owned(text: *const c_char) -> String {
	unsafe { CStr::from_ptr(text) }
		.to_string_lossy()
		.into_owned()
}

/// A script's text, checked to be valid in the database's encoding and filled in, copied into
/// Postgres's memory. `null` when there is no such file, and the refusal's message when a value may
/// not go in.
fn load(path: &Path, values: &script::Values<'_>) -> Result<*const c_char, String> {
	let bytes = match std::fs::read(path) {
		Ok(bytes) => bytes,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(std::ptr::null()),
		Err(e) => pgrx::error!("could not read file \"{}\": {e}", path.display()),
	};
	unsafe {
		// Refuses a zero byte and anything not valid in the database's encoding, as Postgres does
		// for any script it reads.
		pg_sys::pg_verify_mbstr(
			pg_sys::GetDatabaseEncoding(),
			bytes.as_ptr().cast(),
			bytes.len() as i32,
			false,
		);
	}
	let Ok(text) = std::str::from_utf8(&bytes) else {
		pgrx::error!("custom script \"{}\" must be UTF-8", path.display());
	};
	script::substitute(text, values)
		.map(|sql| in_postgres(&sql))
		.map_err(|refused| {
			format!(
				"invalid character in {}: must not contain any of \"{}\"",
				refused.what(),
				script::QUOTING
			)
		})
}

/// `text` copied into the current memory context, which outlives the statement.
pub fn in_postgres(text: &str) -> *const c_char {
	let text = CString::new(text).unwrap_or_else(|_| pgrx::error!("text holds a zero byte"));
	unsafe { pg_sys::pstrdup(text.as_ptr()) }
}
