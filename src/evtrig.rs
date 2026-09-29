//! Event triggers a non-superuser may create, and the rule that keeps them from ever running with a
//! superuser's rights.
//!
//! The privileged role may create an event trigger, which Postgres reserves for superusers because
//! an event trigger fires for whoever runs the DDL, a superuser included: a function the owner wrote
//! would then run as the superuser. So such a function is never run for a superuser. The rule, for
//! every event trigger function that is called:
//!
//! - a superuser runs only functions owned by that same superuser;
//! - everyone else runs what they would have run anyway.
//!
//! A function that is not run is swapped for `version()`, whose result an event trigger ignores.
use crate::hook::refusal;
use crate::raw::{self, Refusal};
use pgrx::pg_sys;
use std::ffi::CStr;

static mut NEXT_NEEDS: pg_sys::needs_fmgr_hook_type = None;
static mut NEXT: pg_sys::fmgr_hook_type = None;

pub unsafe fn install() {
	unsafe {
		NEXT_NEEDS = pg_sys::needs_fmgr_hook;
		pg_sys::needs_fmgr_hook = Some(needs);
		NEXT = pg_sys::fmgr_hook;
		pg_sys::fmgr_hook = Some(hook);
	}
}

/// `CREATE EVENT TRIGGER` by a privileged role: the function must be owned the same way the trigger
/// will be (by a superuser for a superuser, by anyone else otherwise). `Ok(true)` when the trigger,
/// created as the superuser, is then given to the caller.
pub unsafe fn create_plan(stmt: *mut pg_sys::CreateEventTrigStmt) -> Result<bool, Refusal> {
	unsafe {
		let me = pg_sys::GetUserId();
		let function = pg_sys::LookupFuncName((*stmt).funcname, 0, std::ptr::null(), false);
		let owner = attributes(function).0;
		let caller_is_super = pg_sys::superuser_arg(me);
		let function_is_super = pg_sys::superuser_arg(owner);
		let name = || {
			CStr::from_ptr(pg_sys::NameListToString((*stmt).funcname))
				.to_string_lossy()
				.into_owned()
		};
		let user = || {
			CStr::from_ptr(pg_sys::GetUserNameFromId(me, false))
				.to_string_lossy()
				.into_owned()
		};
		if !caller_is_super && function_is_super {
			return Err(refusal(
				pgrx::PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
				"Non-superuser owned event trigger must execute a non-superuser owned function",
				Some(&format!(
					"The current user \"{}\" is not a superuser and the function \"{}\" is owned by a superuser",
					user(),
					name()
				)),
			));
		}
		if caller_is_super && !function_is_super {
			return Err(refusal(
				pgrx::PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
				"Superuser owned event trigger must execute a superuser owned function",
				Some(&format!(
					"The current user \"{}\" is a superuser and the function \"{}\" is owned by a non-superuser",
					user(),
					name()
				)),
			));
		}
		Ok(!caller_is_super)
	}
}

/// A function's owner, and whether it is SECURITY DEFINER.
unsafe fn attributes(function: pg_sys::Oid) -> (pg_sys::Oid, bool) {
	unsafe {
		let tuple = pg_sys::SearchSysCache1(
			pg_sys::SysCacheIdentifier::PROCOID as i32,
			pg_sys::Datum::from(function),
		);
		if tuple.is_null() {
			pgrx::error!("cache lookup failed for function {}", function.to_u32());
		}
		let proc = pg_sys::heap_tuple_get_struct::<pg_sys::FormData_pg_proc>(tuple);
		let found = ((*proc).proowner, (*proc).prosecdef);
		pg_sys::ReleaseSysCache(tuple);
		found
	}
}

fn is_event_trigger_function(function: pg_sys::Oid) -> bool {
	unsafe { pg_sys::get_func_rettype(function) == pg_sys::EVENT_TRIGGEROID }
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn needs(function: pg_sys::Oid) -> bool {
	unsafe {
		if let Some(next) = NEXT_NEEDS
			&& pg_sys::ffi::pg_guard_ffi_boundary(|| next(function))
		{
			return true;
		}
	}
	is_event_trigger_function(function)
}

#[pgrx::pg_guard]
unsafe extern "C-unwind" fn hook(
	event: pg_sys::FmgrHookEventType::Type,
	info: *mut pg_sys::FmgrInfo,
	arg: *mut pg_sys::Datum,
) {
	unsafe {
		if event == pg_sys::FmgrHookEventType::FHET_START
			&& is_event_trigger_function((*info).fn_oid)
		{
			let (owner, security_definer) = attributes((*info).fn_oid);
			// A SECURITY DEFINER function has already switched to its owner by now, so the role that
			// ran the statement is the outer one.
			let role = if security_definer {
				pg_sys::GetOuterUserId()
			} else {
				pg_sys::GetUserId()
			};
			if pg_sys::superuser_arg(role) && (!pg_sys::superuser_arg(owner) || role != owner) {
				skip(info);
			}
		}
		if let Some(next) = NEXT {
			pg_sys::ffi::pg_guard_ffi_boundary(|| next(event, info, arg));
		}
	}
}

/// Postgres cannot be told not to call a function from here, so the function is replaced by one
/// that does nothing an event trigger would notice.
unsafe fn skip(info: *mut pg_sys::FmgrInfo) {
	unsafe {
		(*info).fn_addr = Some(raw::pgsql_version);
		(*info).fn_oid = pg_sys::Oid::from(89); // version(), stable across releases
		(*info).fn_nargs = 0;
		(*info).fn_strict = false;
		(*info).fn_retset = false;
		(*info).fn_stats = 0;
		(*info).fn_extra = std::ptr::null_mut();
		(*info).fn_mcxt = pg_sys::CurrentMemoryContext;
		(*info).fn_expr = std::ptr::null_mut();
	}
}
