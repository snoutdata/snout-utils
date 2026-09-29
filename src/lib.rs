//! snout_utils: lets a database's owner do the handful of things a hosted Postgres reserves for a
//! superuser (manage the extensions the platform offers, publications, foreign-data wrappers, event
//! triggers and a few superuser-only settings) without ever being one.
//!
//! It is a preloaded library with no SQL objects: `shared_preload_libraries = 'snout_utils'` and the
//! settings in settings.rs are the whole of its interface, so nothing is written into a database and
//! a dump and restore carries nothing of it. README.md is the reference.
mod evtrig;
mod extension;
mod fdw;
mod hook;
mod list;
pub mod names;
mod raw;
pub mod script;
mod settings;

pgrx::pg_module_magic!();

#[pgrx::pg_guard]
pub extern "C-unwind" fn _PG_init() {
	settings::init();
	unsafe {
		hook::install();
		evtrig::install();
	}
}

/// Required by `cargo pgrx test`; must sit at the crate root.
#[cfg(test)]
pub mod pg_test {
	pub fn setup(_options: Vec<&str>) {}

	pub fn postgresql_conf_options() -> Vec<&'static str> {
		vec!["shared_preload_libraries = 'snout_utils'"]
	}
}
