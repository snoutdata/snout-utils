//! The utility hook: every statement that is not a query passes through here.
//!
//! In two halves, and the split is the design. **Deciding** reads the statement, the settings and
//! the catalogs and says what to do (a [`Plan`]); it runs under pgrx's guard, so a bug or a failed
//! lookup is an ordinary ERROR. **Carrying it out** runs the statement, as its caller or as the
//! superuser, through [`raw`] only, so any ERROR the statement raises reaches the client exactly as
//! Postgres raised it, and the caller's role is always put back first.
use crate::raw::{self, Identity, Refusal};
use crate::{extension, list, names, settings};
use pgrx::PgSqlErrorCode;
use pgrx::pg_sys;
use std::ffi::{CStr, c_char, c_int};

static mut PREVIOUS: pg_sys::ProcessUtility_hook_type = None;

/// Set while a custom script runs, so a `CREATE EXTENSION` inside one does not run scripts itself.
static mut RUNNING_SCRIPT: bool = false;

pub unsafe fn install() {
	unsafe {
		PREVIOUS = pg_sys::ProcessUtility_hook;
		pg_sys::ProcessUtility_hook = Some(process_utility);
	}
}

/// The arguments the hook was called with, to pass on unchanged.
#[derive(Clone, Copy)]
struct Call {
	pstmt: *mut pg_sys::PlannedStmt,
	query_string: *const c_char,
	read_only_tree: bool,
	context: pg_sys::ProcessUtilityContext::Type,
	params: pg_sys::ParamListInfo,
	query_env: *mut pg_sys::QueryEnvironment,
	dest: *mut pg_sys::DestReceiver,
	qc: *mut pg_sys::QueryCompletion,
}

impl Call {
	/// Runs the statement: the next hook in the chain, or Postgres itself.
	unsafe fn run(self) {
		unsafe {
			match PREVIOUS {
				Some(next) => next(
					self.pstmt,
					self.query_string,
					self.read_only_tree,
					self.context,
					self.params,
					self.query_env,
					self.dest,
					self.qc,
				),
				None => raw::standard_ProcessUtility(
					self.pstmt,
					self.query_string,
					self.read_only_tree,
					self.context,
					self.params,
					self.query_env,
					self.dest,
					self.qc,
				),
			}
		}
	}
}

/// What to do with a statement.
#[derive(Clone, Copy)]
enum Plan {
	/// Run it as its caller, as if this library were not loaded.
	Pass,
	/// Refuse it.
	Refuse(Refusal),
	/// Run it as the superuser, then do `then` while still the superuser.
	AsSuperuser { superuser: pg_sys::Oid, then: Then },
	/// `CREATE EXTENSION`: the custom scripts as the superuser, and the statement as the superuser
	/// when it is delegated or as its caller when it is not.
	CreateExtension {
		superuser: pg_sys::Oid,
		delegated: bool,
		scripts: extension::Scripts,
	},
}

/// What is done after a delegated statement, before the caller's role comes back: an object the
/// superuser created is given to the role that asked for it.
#[derive(Clone, Copy)]
enum Then {
	Nothing,
	PublicationOwner {
		name: *const c_char,
		owner: pg_sys::Oid,
	},
	FdwOwner {
		name: *const c_char,
		owner: pg_sys::Oid,
	},
	EventTriggerOwner {
		name: *const c_char,
		owner: pg_sys::Oid,
	},
}

unsafe extern "C-unwind" fn process_utility(
	pstmt: *mut pg_sys::PlannedStmt,
	query_string: *const c_char,
	read_only_tree: bool,
	context: pg_sys::ProcessUtilityContext::Type,
	params: pg_sys::ParamListInfo,
	query_env: *mut pg_sys::QueryEnvironment,
	dest: *mut pg_sys::DestReceiver,
	qc: *mut pg_sys::QueryCompletion,
) {
	let call = Call {
		pstmt,
		query_string,
		read_only_tree,
		context,
		params,
		query_env,
		dest,
		qc,
	};
	// SAFETY: `decide` returns plain data; a panic or a caught ERROR inside it is raised by the guard.
	let plan = unsafe { pg_sys::submodules::panic::pgrx_extern_c_guard(|| decide(pstmt)) };
	unsafe {
		match plan {
			Plan::Pass => call.run(),
			Plan::Refuse(refusal) => raw::raise(refusal),
			Plan::AsSuperuser { superuser, then } => as_superuser(call, superuser, then),
			Plan::CreateExtension {
				superuser,
				delegated,
				scripts,
			} => create_extension(call, superuser, delegated, scripts),
		}
	}
}

// ---------------------------------------------------------------------------------------------
// Carrying it out: `raw` calls only, and nothing on the stack that needs dropping.
// ---------------------------------------------------------------------------------------------

unsafe fn as_superuser(call: Call, superuser: pg_sys::Oid, then: Then) {
	unsafe {
		let caller = Identity::current();
		raw::try_finally(
			&mut || {
				caller.become_superuser(superuser);
				call.run();
				give(then);
			},
			&mut || caller.restore(),
		);
		caller.restore();
	}
}

unsafe fn create_extension(
	call: Call,
	superuser: pg_sys::Oid,
	delegated: bool,
	scripts: extension::Scripts,
) {
	unsafe {
		let caller = Identity::current();
		let was_running = RUNNING_SCRIPT;
		raw::try_finally(
			&mut || {
				caller.become_superuser(superuser);
				run_script(scripts.before_all);
				run_script(scripts.before);
				if delegated {
					call.run();
				} else {
					caller.restore();
					call.run();
					caller.become_superuser(superuser);
				}
				match scripts.after {
					Ok(sql) => run_script(sql),
					Err(refusal) => {
						if !RUNNING_SCRIPT {
							raw::raise(refusal)
						}
					}
				}
			},
			&mut || {
				caller.restore();
				RUNNING_SCRIPT = was_running;
			},
		);
		caller.restore();
	}
}

unsafe fn run_script(sql: *const c_char) {
	unsafe {
		if sql.is_null() || RUNNING_SCRIPT {
			return;
		}
		RUNNING_SCRIPT = true;
		raw::PushActiveSnapshot(raw::GetTransactionSnapshot());
		if raw::SPI_connect() != pg_sys::SPI_OK_CONNECT as c_int {
			raw::raise(internal(c"could not connect to SPI to run a custom script"));
		}
		if raw::SPI_execute(sql, false, 0) < 0 {
			raw::raise(internal(c"a custom script failed"));
		}
		raw::SPI_finish();
		raw::PopActiveSnapshot();
		RUNNING_SCRIPT = false;
	}
}

/// Gives a newly created object to the role that asked for it. A foreign-data wrapper and an event
/// trigger may only be owned by a superuser, so the role is one for the length of the change, inside
/// this transaction, and never after it.
unsafe fn give(then: Then) {
	unsafe {
		match then {
			Then::Nothing => {}
			Then::PublicationOwner { name, owner } => {
				raw::AlterPublicationOwner(name, owner);
				raw::CommandCounterIncrement();
			}
			Then::FdwOwner { name, owner } => {
				set_superuser_attribute(owner, true);
				raw::AlterForeignDataWrapperOwner(name, owner);
				raw::CommandCounterIncrement();
				set_superuser_attribute(owner, false);
			}
			Then::EventTriggerOwner { name, owner } => {
				set_superuser_attribute(owner, true);
				raw::AlterEventTriggerOwner(name, owner);
				raw::CommandCounterIncrement();
				set_superuser_attribute(owner, false);
			}
		}
	}
}

/// `ALTER ROLE <role> [NO]SUPERUSER`, built as a statement and run directly.
unsafe fn set_superuser_attribute(role: pg_sys::Oid, on: bool) {
	unsafe {
		let spec = raw::palloc0(size_of::<pg_sys::RoleSpec>()).cast::<pg_sys::RoleSpec>();
		(*spec).type_ = pg_sys::NodeTag::T_RoleSpec;
		(*spec).roletype = pg_sys::RoleSpecType::ROLESPEC_CSTRING;
		(*spec).rolename = role_name(role);
		(*spec).location = -1;
		let stmt = raw::palloc0(size_of::<pg_sys::AlterRoleStmt>()).cast::<pg_sys::AlterRoleStmt>();
		(*stmt).type_ = pg_sys::NodeTag::T_AlterRoleStmt;
		(*stmt).role = spec;
		let option = raw::makeDefElem(
			c"superuser".as_ptr().cast_mut(),
			raw::makeBoolean(on).cast(),
			-1,
		);
		(*stmt).options = raw::lappend(std::ptr::null_mut(), option.cast());
		raw::AlterRole(std::ptr::null_mut(), stmt);
		raw::CommandCounterIncrement();
	}
}

unsafe extern "C-unwind" {
	#[link_name = "GetUserNameFromId"]
	fn get_user_name_from_id(role: pg_sys::Oid, noerr: bool) -> *mut c_char;
}

unsafe fn role_name(role: pg_sys::Oid) -> *mut c_char {
	unsafe { get_user_name_from_id(role, false) }
}

fn internal(message: &'static CStr) -> Refusal {
	Refusal {
		code: PgSqlErrorCode::ERRCODE_INTERNAL_ERROR as c_int,
		message: message.as_ptr(),
		detail: std::ptr::null(),
		hint: std::ptr::null(),
	}
}

// ---------------------------------------------------------------------------------------------
// Deciding: under pgrx's guard, so ordinary pgrx calls and errors are fine here.
// ---------------------------------------------------------------------------------------------

unsafe fn decide(pstmt: *mut pg_sys::PlannedStmt) -> Plan {
	unsafe {
		let node = (*pstmt).utilityStmt;
		if node.is_null() {
			return Plan::Pass;
		}
		match (*node).type_ {
			pg_sys::NodeTag::T_CreateExtensionStmt => create_extension_plan(node.cast()),
			pg_sys::NodeTag::T_AlterExtensionStmt => {
				let stmt = node.cast::<pg_sys::AlterExtensionStmt>();
				if pg_sys::superuser() {
					return Plan::Pass;
				}
				// Run once, as the superuser. Every other hook in the chain sees one statement: pgaudit,
				// for one, refuses a statement that begins twice.
				delegate_if(caller_is_privileged() && extension_is_delegated(cstr((*stmt).extname)))
			}
			pg_sys::NodeTag::T_AlterObjectSchemaStmt => {
				let stmt = node.cast::<pg_sys::AlterObjectSchemaStmt>();
				if pg_sys::superuser() || (*stmt).objectType != pg_sys::ObjectType::OBJECT_EXTENSION
				{
					return Plan::Pass;
				}
				let name = cstr(list::string_value((*stmt).object));
				delegate_if(caller_is_privileged() && extension_is_delegated(name))
			}
			pg_sys::NodeTag::T_DropStmt => {
				let stmt = node.cast::<pg_sys::DropStmt>();
				if pg_sys::superuser() || (*stmt).removeType != pg_sys::ObjectType::OBJECT_EXTENSION
				{
					return Plan::Pass;
				}
				let all = !(*stmt).objects.is_null()
					&& list::pointers::<pg_sys::Node>((*stmt).objects)
						.all(|object| extension_is_delegated(cstr(list::string_value(object))));
				delegate_if(caller_is_privileged() && all)
			}
			pg_sys::NodeTag::T_CommentStmt => {
				let stmt = node.cast::<pg_sys::CommentStmt>();
				let wanted = pg_sys::IsTransactionState()
					&& !pg_sys::superuser()
					&& (*stmt).objtype == pg_sys::ObjectType::OBJECT_EXTENSION
					&& caller_is_privileged();
				delegate_if(wanted)
			}
			pg_sys::NodeTag::T_CreatePublicationStmt => {
				if pg_sys::superuser() || !caller_is_privileged() {
					return Plan::Pass;
				}
				let stmt = node.cast::<pg_sys::CreatePublicationStmt>();
				as_superuser_then(Then::PublicationOwner {
					name: (*stmt).pubname,
					owner: pg_sys::GetUserId(),
				})
			}
			pg_sys::NodeTag::T_AlterPublicationStmt => {
				delegate_if(!pg_sys::superuser() && caller_is_privileged())
			}
			pg_sys::NodeTag::T_CreateFdwStmt => {
				if pg_sys::superuser() || !caller_is_privileged() {
					return Plan::Pass;
				}
				let stmt = node.cast::<pg_sys::CreateFdwStmt>();
				if let Err(refusal) = crate::fdw::check_functions((*stmt).func_options) {
					return Plan::Refuse(refusal);
				}
				as_superuser_then(Then::FdwOwner {
					name: (*stmt).fdwname,
					owner: pg_sys::GetUserId(),
				})
			}
			pg_sys::NodeTag::T_CreateEventTrigStmt => {
				if !pg_sys::IsTransactionState() || !caller_is_privileged() {
					return Plan::Pass;
				}
				crate::evtrig::create_plan(node.cast()).map_or_else(
					Plan::Refuse,
					|give_to_caller| {
						as_superuser_then(if give_to_caller {
							Then::EventTriggerOwner {
								name: (*node.cast::<pg_sys::CreateEventTrigStmt>()).trigname,
								owner: pg_sys::GetUserId(),
							}
						} else {
							Then::Nothing
						})
					},
				)
			}
			pg_sys::NodeTag::T_VariableSetStmt => {
				let stmt = node.cast::<pg_sys::VariableSetStmt>();
				let wanted = pg_sys::IsTransactionState()
					&& !pg_sys::superuser()
					&& config_is_allowed((*stmt).name)
					&& caller_is_privileged();
				delegate_if(wanted)
			}
			pg_sys::NodeTag::T_AlterRoleSetStmt => alter_role_set_plan(node.cast()),
			pg_sys::NodeTag::T_AlterRoleStmt => alter_role_plan(node.cast()),
			pg_sys::NodeTag::T_CreateRoleStmt => create_role_plan(node.cast()),
			_ => Plan::Pass,
		}
	}
}

unsafe fn create_extension_plan(stmt: *mut pg_sys::CreateExtensionStmt) -> Plan {
	unsafe {
		let name = cstr((*stmt).extname);
		// Only a role with the privileged role's privileges (or a superuser) is ever helped: any
		// other role gets exactly what Postgres gives it, so a role made for a reporting tool cannot
		// drop an extension (and every column of its types) as the superuser.
		let delegated =
			(pg_sys::superuser() || caller_is_privileged()) && extension_is_delegated(name);
		let scripts = if RUNNING_SCRIPT {
			extension::Scripts::NONE
		} else {
			extension::prepare(name, (*stmt).options)
		};
		Plan::CreateExtension {
			superuser: superuser_oid(),
			delegated,
			scripts,
		}
	}
}

unsafe fn alter_role_set_plan(stmt: *mut pg_sys::AlterRoleSetStmt) -> Plan {
	unsafe {
		if !pg_sys::IsTransactionState() || pg_sys::superuser() || !caller_is_privileged() {
			return Plan::Pass;
		}
		// ALTER ROLE ALL SET: no role, and nothing to delegate.
		if (*stmt).role.is_null()
			|| (*stmt).setstmt.is_null()
			|| !config_is_allowed((*(*stmt).setstmt).name)
		{
			return Plan::Pass;
		}
		// Never on a superuser: a delegated setting there changes how OUR sessions behave
		// (session_replication_role = replica turns off every trigger they fire). Nor on any other
		// role the privileged role does not administer: those are the platform's.
		let target = pg_sys::get_rolespec_oid((*stmt).role, false);
		delegate_if(!pg_sys::superuser_arg(target) && administers(target))
	}
}

unsafe fn alter_role_plan(stmt: *mut pg_sys::AlterRoleStmt) -> Plan {
	unsafe {
		if !pg_sys::IsTransactionState() || pg_sys::superuser() || !caller_is_privileged() {
			return Plan::Pass;
		}
		let target = pg_sys::get_rolespec_oid((*stmt).role, false);
		if pg_sys::has_privs_of_role(target, privileged_role_oid()) {
			return Plan::Refuse(refusal(
				PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
				"permission denied to alter role",
				Some("Only superusers can alter privileged roles."),
			));
		}
		let options: Vec<*mut pg_sys::DefElem> =
			list::pointers::<pg_sys::DefElem>((*stmt).options).collect();
		for &option in &options {
			if CStr::from_ptr((*option).defname).to_bytes() == b"superuser" {
				return Plan::Refuse(refusal(
					PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
					"permission denied to alter role",
					Some(
						"Only roles with the SUPERUSER attribute may alter roles with the SUPERUSER attribute.",
					),
				));
			}
		}
		// A role the privileged role administers (one it created, or was given ADMIN on), or one of
		// the API stack's service roles with only the attributes its bootstrap sets: that runs as
		// the owner and sets LOGIN, CREATEROLE, BYPASSRLS and REPLICATION on them this way, on pods
		// where some of them were made by our superuser. Anything else is the platform's, and
		// Postgres decides it as it would without us: it refuses a role the caller does not
		// administer.
		let name = CStr::from_ptr(pg_sys::GetUserNameFromId(target, false));
		let defnames: Vec<&CStr> = options
			.iter()
			.map(|&option| CStr::from_ptr((*option).defname))
			.collect();
		delegate_if(administers(target) || is_service_role_change(name, &defnames))
	}
}

/// Whether the privileged role holds ADMIN on `role`, directly or through a role it is a member of.
fn administers(role: pg_sys::Oid) -> bool {
	let privileged = privileged_role_oid();
	privileged != pg_sys::Oid::INVALID && unsafe { pg_sys::is_admin_of_role(privileged, role) }
}

/// The API stack's service roles, and the attributes (as `ALTER ROLE` names them in its parse tree)
/// the stack's bootstrap sets on each over the owner's connection.
const SERVICE_ROLES: &[(&str, &[&str])] = &[
	("anon", &["canlogin", "inherit"]),
	("authenticated", &["canlogin", "inherit"]),
	("service_role", &["canlogin", "inherit", "bypassrls"]),
	("authenticator", &["canlogin", "inherit"]),
	("snout_auth_admin", &["canlogin", "inherit", "createrole"]),
	(
		"snout_realtime_admin",
		&["canlogin", "inherit", "createrole", "isreplication"],
	),
	(
		"snout_storage_admin",
		&["canlogin", "inherit", "createrole"],
	),
];

/// Whether an `ALTER ROLE` of `role` setting `options` is one the bootstrap makes: a service role,
/// and nothing but that role's attributes (no password, no connection limit, no expiry).
fn is_service_role_change(role: &CStr, options: &[&CStr]) -> bool {
	SERVICE_ROLES
		.iter()
		.find(|(name, _)| name.as_bytes() == role.to_bytes())
		.is_some_and(|(_, allowed)| {
			!options.is_empty()
				&& options
					.iter()
					.all(|option| allowed.iter().any(|a| a.as_bytes() == option.to_bytes()))
		})
}

unsafe fn create_role_plan(stmt: *mut pg_sys::CreateRoleStmt) -> Plan {
	unsafe {
		if !pg_sys::IsTransactionState() || pg_sys::superuser() {
			return Plan::Pass;
		}
		// A role that exists fails with Postgres's own "already exists".
		if pg_sys::get_role_oid((*stmt).role, true) != pg_sys::Oid::INVALID {
			return Plan::Pass;
		}
		for option in list::pointers::<pg_sys::DefElem>((*stmt).options) {
			if CStr::from_ptr((*option).defname).to_bytes() == b"superuser"
				&& pg_sys::defGetBoolean(option)
			{
				return Plan::Refuse(refusal(
					PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
					"permission denied to create role",
					Some(
						"Only roles with the SUPERUSER attribute may create roles with the SUPERUSER attribute.",
					),
				));
			}
		}
		Plan::Pass
	}
}

fn delegate_if(wanted: bool) -> Plan {
	if wanted {
		as_superuser_then(Then::Nothing)
	} else {
		Plan::Pass
	}
}

fn as_superuser_then(then: Then) -> Plan {
	Plan::AsSuperuser {
		superuser: unsafe { superuser_oid() },
		then,
	}
}

/// The superuser to run as: the configured one, or the role that created the cluster.
unsafe fn superuser_oid() -> pg_sys::Oid {
	match settings::superuser().filter(|name| !name.is_empty()) {
		Some(name) => unsafe { pg_sys::get_role_oid(name.as_ptr(), false) },
		None => pg_sys::Oid::from(pg_sys::BOOTSTRAP_SUPERUSERID),
	}
}

fn privileged_role_oid() -> pg_sys::Oid {
	match settings::privileged_role() {
		Some(name) => unsafe { pg_sys::get_role_oid(name.as_ptr(), true) },
		None => pg_sys::Oid::INVALID,
	}
}

/// Whether the current role has the privileged role's privileges (a superuser has every role's).
fn caller_is_privileged() -> bool {
	let privileged = privileged_role_oid();
	privileged != pg_sys::Oid::INVALID
		&& unsafe { pg_sys::has_privs_of_role(pg_sys::GetUserId(), privileged) }
}

fn extension_is_delegated(name: &str) -> bool {
	settings::privileged_extensions().is_some_and(|list| names::contains(list, name))
		&& extension::is_available(name)
}

unsafe fn config_is_allowed(name: *const c_char) -> bool {
	if name.is_null() {
		return false;
	}
	let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
	settings::privileged_role_allowed_configs().is_some_and(|list| names::contains(list, &name))
}

/// A C string as `&str` (a name that is not UTF-8 matches nothing on a list).
unsafe fn cstr<'a>(text: *const c_char) -> &'a str {
	if text.is_null() {
		""
	} else {
		unsafe { CStr::from_ptr(text) }.to_str().unwrap_or("")
	}
}

pub fn refusal(code: PgSqlErrorCode, message: &str, detail: Option<&str>) -> Refusal {
	Refusal {
		code: code as c_int,
		message: extension::in_postgres(message),
		detail: detail.map_or(std::ptr::null(), extension::in_postgres),
		hint: std::ptr::null(),
	}
}

#[cfg(test)]
mod tests {
	use super::is_service_role_change;

	#[test]
	fn the_bootstrap_s_service_role_changes_are_delegated() {
		assert!(is_service_role_change(c"anon", &[c"canlogin", c"inherit"]));
		assert!(is_service_role_change(c"service_role", &[c"bypassrls"]));
		assert!(is_service_role_change(
			c"snout_realtime_admin",
			&[c"isreplication"]
		));
		assert!(is_service_role_change(
			c"snout_auth_admin",
			&[c"canlogin", c"inherit", c"createrole"]
		));
	}

	#[test]
	fn nothing_else_on_a_service_role_and_no_other_role() {
		assert!(!is_service_role_change(c"authenticator", &[c"password"]));
		assert!(!is_service_role_change(c"anon", &[c"bypassrls"]));
		assert!(!is_service_role_change(
			c"authenticator",
			&[c"isreplication"]
		));
		assert!(!is_service_role_change(
			c"snout_realtime_admin",
			&[c"connectionlimit"]
		));
		assert!(!is_service_role_change(
			c"snout_storage_admin",
			&[c"canlogin", c"validUntil"]
		));
		assert!(!is_service_role_change(c"snout_storage_admin", &[]));
		assert!(!is_service_role_change(c"reporting", &[c"canlogin"]));
		assert!(!is_service_role_change(c"snoutpod_oauth", &[c"canlogin"]));
	}
}
