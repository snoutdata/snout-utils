//! `CREATE FOREIGN DATA WRAPPER` by the privileged role, which runs as the superuser. A wrapper's
//! handler and validator are functions the superuser then calls, so they are checked first: both
//! must be named, both must belong to the same extension, that extension must be one installed on
//! this server (not one defined inside the database), and both names are rewritten schema-qualified
//! so the superuser's `search_path` cannot resolve them to a different function.
use crate::extension;
use crate::hook::refusal;
use crate::list;
use crate::raw::Refusal;
use pgrx::PgSqlErrorCode;
use pgrx::pg_sys;
use std::ffi::CStr;

pub unsafe fn check_functions(options: *mut pg_sys::List) -> Result<(), Refusal> {
	unsafe {
		let mut handler: Option<(*mut pg_sys::DefElem, pg_sys::Oid)> = None;
		let mut validator: Option<(*mut pg_sys::DefElem, pg_sys::Oid)> = None;
		for option in list::pointers::<pg_sys::DefElem>(options) {
			let arg = (*option).arg.cast::<pg_sys::List>();
			match CStr::from_ptr((*option).defname).to_bytes() {
				b"handler" => {
					let found = if arg.is_null() {
						pg_sys::Oid::INVALID
					} else {
						let function = pg_sys::LookupFuncName(arg, 0, std::ptr::null(), false);
						if pg_sys::get_func_rettype(function) != pg_sys::FDW_HANDLEROID {
							pgrx::ereport!(
								ERROR,
								PgSqlErrorCode::ERRCODE_WRONG_OBJECT_TYPE,
								format!("function {} must return type fdw_handler", names(arg))
							);
						}
						function
					};
					handler = Some((option, found));
				}
				b"validator" => {
					let found = if arg.is_null() {
						pg_sys::Oid::INVALID
					} else {
						let types = [pg_sys::TEXTARRAYOID, pg_sys::OIDOID];
						pg_sys::LookupFuncName(arg, 2, types.as_ptr(), false)
					};
					validator = Some((option, found));
				}
				_ => {}
			}
		}
		let (Some((handler, handler_fn)), true) = (
			handler,
			handler.is_some_and(|(_, f)| f != pg_sys::Oid::INVALID),
		) else {
			return Err(missing(
				"A handler must be specified when creating a foreign data wrapper",
			));
		};
		let (Some((validator, validator_fn)), true) = (
			validator,
			validator.is_some_and(|(_, f)| f != pg_sys::Oid::INVALID),
		) else {
			return Err(missing(
				"A validator must be specified when creating a foreign data wrapper",
			));
		};
		let handler_ext = pg_sys::getExtensionOfObject(pg_sys::ProcedureRelationId, handler_fn);
		let validator_ext = pg_sys::getExtensionOfObject(pg_sys::ProcedureRelationId, validator_fn);
		if handler_ext == pg_sys::Oid::INVALID
			|| validator_ext == pg_sys::Oid::INVALID
			|| handler_ext != validator_ext
		{
			return Err(missing(
				"Handler and validator functions must both be owned by the same extension",
			));
		}
		for (which, option, ext) in [
			("Handler", handler, handler_ext),
			("Validator", validator, validator_ext),
		] {
			if !installed(ext) {
				return Err(refusal(
					PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
					&format!(
						"{which} function \"{}\" must be owned by a non-TLE extension",
						names((*option).arg.cast())
					),
					None,
				));
			}
		}
		(*handler).arg = qualified(handler_fn);
		(*validator).arg = qualified(validator_fn);
		Ok(())
	}
}

fn missing(message: &str) -> Refusal {
	refusal(
		PgSqlErrorCode::ERRCODE_FDW_OPTION_NAME_NOT_FOUND,
		message,
		None,
	)
}

/// Whether an extension is one with a control file on this server.
unsafe fn installed(ext: pg_sys::Oid) -> bool {
	let name = unsafe { pg_sys::get_extension_name(ext) };
	!name.is_null() && extension::is_available(&unsafe { CStr::from_ptr(name) }.to_string_lossy())
}

unsafe fn names(list: *mut pg_sys::List) -> String {
	unsafe { CStr::from_ptr(pg_sys::NameListToString(list)) }
		.to_string_lossy()
		.into_owned()
}

/// The function as a two-part name, schema first.
unsafe fn qualified(function: pg_sys::Oid) -> *mut pg_sys::Node {
	unsafe {
		let schema = pg_sys::get_namespace_name(pg_sys::get_func_namespace(function));
		let name = pg_sys::get_func_name(function);
		if schema.is_null() || name.is_null() {
			pgrx::ereport!(
				ERROR,
				PgSqlErrorCode::ERRCODE_UNDEFINED_FUNCTION,
				format!("function with oid {} no longer exists", function.to_u32())
			);
		}
		let list = pg_sys::lappend(std::ptr::null_mut(), pg_sys::makeString(schema).cast());
		pg_sys::lappend(list, pg_sys::makeString(name).cast()).cast()
	}
}
