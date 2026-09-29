//! Calls into Postgres that an ERROR must leave exactly as Postgres raised it.
//!
//! Every function pgrx binds catches a Postgres ERROR, copies the message, detail and hint, and
//! raises a new one from Rust. That new ERROR has lost the rest: the cursor position psql draws a
//! caret from, the schema, table, column and constraint names a driver reads, the context lines.
//! Every utility statement a database runs passes through this library's hook, so a statement that
//! fails must fail with Postgres's own ERROR, untouched, or every `CREATE TABLE` that names a missing
//! type would report less than it does without us.
//!
//! So the statement itself, and everything done while it runs as the superuser, is called through
//! these declarations instead, which pgrx does not wrap: an ERROR goes straight back to Postgres as
//! a `siglongjmp`, the way it would through a hook written in C. The Rust frames it passes over hold
//! nothing to drop (they are "plain old frames", which is when that is defined behaviour), and
//! [`try_finally`] is the one place an ERROR is caught, to put the caller's role back before it
//! carries on unchanged.
use pgrx::pg_sys;
use std::ffi::{c_char, c_int, c_long, c_void};

unsafe extern "C-unwind" {
	pub fn standard_ProcessUtility(
		pstmt: *mut pg_sys::PlannedStmt,
		query_string: *const c_char,
		read_only_tree: bool,
		context: pg_sys::ProcessUtilityContext::Type,
		params: pg_sys::ParamListInfo,
		query_env: *mut pg_sys::QueryEnvironment,
		dest: *mut pg_sys::DestReceiver,
		qc: *mut pg_sys::QueryCompletion,
	);
	pub fn pg_re_throw() -> !;
	pub fn SetUserIdAndSecContext(userid: pg_sys::Oid, sec_context: c_int);
	pub fn GetUserIdAndSecContext(userid: *mut pg_sys::Oid, sec_context: *mut c_int);
	pub fn SPI_connect() -> c_int;
	pub fn SPI_execute(src: *const c_char, read_only: bool, tcount: c_long) -> c_int;
	pub fn SPI_finish() -> c_int;
	pub fn GetTransactionSnapshot() -> pg_sys::Snapshot;
	pub fn PushActiveSnapshot(snapshot: pg_sys::Snapshot);
	pub fn PopActiveSnapshot();
	pub fn CommandCounterIncrement();
	pub fn AlterRole(
		pstate: *mut pg_sys::ParseState,
		stmt: *mut pg_sys::AlterRoleStmt,
	) -> pg_sys::Oid;
	pub fn AlterPublicationOwner(
		name: *const c_char,
		new_owner: pg_sys::Oid,
	) -> pg_sys::ObjectAddress;
	pub fn AlterForeignDataWrapperOwner(
		name: *const c_char,
		new_owner: pg_sys::Oid,
	) -> pg_sys::ObjectAddress;
	pub fn AlterEventTriggerOwner(
		name: *const c_char,
		new_owner: pg_sys::Oid,
	) -> pg_sys::ObjectAddress;
	pub fn palloc0(size: usize) -> *mut c_void;
	pub fn lappend(list: *mut pg_sys::List, datum: *mut c_void) -> *mut pg_sys::List;
	pub fn makeDefElem(
		name: *mut c_char,
		arg: *mut pg_sys::Node,
		location: c_int,
	) -> *mut pg_sys::DefElem;
	pub fn makeBoolean(val: bool) -> *mut pg_sys::Boolean;
	pub fn pgsql_version(fcinfo: pg_sys::FunctionCallInfo) -> pg_sys::Datum;

	fn errstart(elevel: c_int, domain: *const c_char) -> bool;
	fn errcode(sqlerrcode: c_int) -> c_int;
	fn errmsg(fmt: *const c_char, ...) -> c_int;
	fn errdetail(fmt: *const c_char, ...) -> c_int;
	fn errhint(fmt: *const c_char, ...) -> c_int;
	fn errfinish(filename: *const c_char, lineno: c_int, funcname: *const c_char);
}

/// A role and its security context, as `GetUserIdAndSecContext` reports them.
#[derive(Clone, Copy)]
pub struct Identity {
	pub user: pg_sys::Oid,
	pub context: c_int,
}

impl Identity {
	pub unsafe fn current() -> Identity {
		let mut user = pg_sys::Oid::INVALID;
		let mut context = 0;
		unsafe { GetUserIdAndSecContext(&mut user, &mut context) };
		Identity { user, context }
	}

	pub unsafe fn restore(self) {
		unsafe { SetUserIdAndSecContext(self.user, self.context) };
	}

	/// Becomes `superuser` inside the caller's context, marked the way a SECURITY DEFINER call is:
	/// the role was changed locally, and nothing that needs a clean session (SET ROLE, a temporary
	/// table) may be done under it.
	pub unsafe fn become_superuser(self, superuser: pg_sys::Oid) {
		let context = self.context
			| pg_sys::SECURITY_LOCAL_USERID_CHANGE as c_int
			| pg_sys::SECURITY_RESTRICTED_OPERATION as c_int;
		unsafe { SetUserIdAndSecContext(superuser, context) };
	}
}

/// Runs `body`. If it raises an ERROR, runs `cleanup` and lets the ERROR carry on exactly as it was
/// raised. On success `cleanup` is not run: the caller does whatever comes next itself.
///
/// This is `PG_TRY` / `PG_CATCH` / `PG_RE_THROW`. `body` and everything it calls must hold nothing
/// that needs dropping when an ERROR is raised, since the `siglongjmp` skips it; in this crate it
/// only ever calls the declarations above.
pub unsafe fn try_finally(body: &mut dyn FnMut(), cleanup: &mut dyn FnMut()) {
	unsafe {
		let exception_stack = pg_sys::PG_exception_stack;
		let context_stack = pg_sys::error_context_stack;
		let caught = cee_scape::call_with_sigsetjmp(false, |jump| {
			pg_sys::PG_exception_stack = std::ptr::from_ref(jump).cast_mut().cast();
			body();
			0
		});
		pg_sys::PG_exception_stack = exception_stack;
		pg_sys::error_context_stack = context_stack;
		if caught != 0 {
			cleanup();
			pg_re_throw();
		}
	}
}

/// An ERROR this library raises itself: a code and up to three sentences, all in memory Postgres
/// owns (or static), so it is `Copy` and can be raised from a frame that holds nothing to drop.
#[derive(Clone, Copy)]
pub struct Refusal {
	pub code: c_int,
	pub message: *const c_char,
	pub detail: *const c_char,
	pub hint: *const c_char,
}

/// Raises `refusal` as an ERROR, the way `ereport(ERROR, ...)` does in C.
pub unsafe fn raise(refusal: Refusal) -> ! {
	const PERCENT_S: &std::ffi::CStr = c"%s";
	unsafe {
		if errstart(pgrx::PgLogLevel::ERROR as c_int, std::ptr::null()) {
			errcode(refusal.code);
			errmsg(PERCENT_S.as_ptr(), refusal.message);
			if !refusal.detail.is_null() {
				errdetail(PERCENT_S.as_ptr(), refusal.detail);
			}
			if !refusal.hint.is_null() {
				errhint(PERCENT_S.as_ptr(), refusal.hint);
			}
			errfinish(c"snout_utils".as_ptr(), 0, c"snout_utils".as_ptr());
		}
	}
	// errfinish never returns from an ERROR, and errstart only says no below the logging threshold,
	// which an ERROR never is.
	std::process::abort()
}
