//! The text of an extension's custom script, with its four variables filled in. Pure: no Postgres,
//! no `unsafe`, tested with plain `#[test]`s and fuzzed.
//!
//! A custom script runs as the superuser, and three of the four values come from the statement a
//! non-superuser typed, so this is the one place in the crate where a string from a customer is put
//! into SQL. Two rules make that safe, and both are tested:
//!
//! 1. **One pass.** The script is read left to right and a value, once put in, is never read again,
//!    so a value that itself looks like a variable (an extension called `@extschema@`) cannot be
//!    expanded a second time into code.
//! 2. **No quoting characters at all.** A value goes in as a single-quoted literal, but a script
//!    author may use a variable inside a dollar-quoted body or an identifier, where single quotes
//!    mean nothing. No one way of quoting is safe in all three places, so a value holding a quote,
//!    a dollar sign or a backslash is refused rather than escaped.
#![forbid(unsafe_code)]

/// The values a script may name, as they were given to `CREATE EXTENSION`.
pub struct Values<'a> {
	pub name: Option<&'a str>,
	pub schema: Option<&'a str>,
	pub version: Option<&'a str>,
	pub cascade: bool,
}

/// Why a value may not go into a script.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
	Name,
	Schema,
	Version,
}

impl Refused {
	pub fn what(&self) -> &'static str {
		match self {
			Refused::Name => "extension name",
			Refused::Schema => "extension schema",
			Refused::Version => "extension version",
		}
	}
}

/// The characters a value may not contain.
pub const QUOTING: &str = "\"$'\\";

/// `sql` with `@extname@`, `@extschema@`, `@extversion@` and `@extcascade@` replaced by their values
/// as SQL literals (`null` for one that was not given, `true`/`false` for cascade).
pub fn substitute(sql: &str, values: &Values<'_>) -> Result<String, Refused> {
	let refuses = |value: Option<&str>| value.is_some_and(|v| v.contains(|c| QUOTING.contains(c)));
	if refuses(values.name) {
		return Err(Refused::Name);
	}
	if refuses(values.schema) {
		return Err(Refused::Schema);
	}
	if refuses(values.version) {
		return Err(Refused::Version);
	}
	let literal = |value: Option<&str>| match value {
		Some(v) => format!("'{v}'"),
		None => "null".to_string(),
	};
	let name = literal(values.name);
	let schema = literal(values.schema);
	let version = literal(values.version);
	let cascade = if values.cascade { "true" } else { "false" };

	let mut out = String::with_capacity(sql.len());
	let mut rest = sql;
	while let Some(at) = rest.find('@') {
		out.push_str(&rest[..at]);
		rest = &rest[at..];
		let replaced = [
			("@extname@", name.as_str()),
			("@extschema@", schema.as_str()),
			("@extversion@", version.as_str()),
			("@extcascade@", cascade),
		]
		.into_iter()
		.find(|(variable, _)| rest.starts_with(variable));
		match replaced {
			Some((variable, value)) => {
				out.push_str(value);
				rest = &rest[variable.len()..];
			}
			None => {
				out.push('@');
				rest = &rest[1..];
			}
		}
	}
	out.push_str(rest);
	Ok(out)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn values<'a>(name: &'a str, schema: Option<&'a str>) -> Values<'a> {
		Values {
			name: Some(name),
			schema,
			version: None,
			cascade: false,
		}
	}

	#[test]
	fn fills_every_variable() {
		let sql = "select @extname@, @extschema@, @extversion@, @extcascade@";
		let got = substitute(
			sql,
			&Values {
				name: Some("hstore"),
				schema: Some("extensions"),
				version: Some("1.8"),
				cascade: true,
			},
		);
		assert_eq!(got.unwrap(), "select 'hstore', 'extensions', '1.8', true");
		let got = substitute(sql, &values("hstore", None));
		assert_eq!(got.unwrap(), "select 'hstore', null, null, false");
	}

	#[test]
	fn a_value_is_never_expanded_twice() {
		// The known attack: a name that is itself a variable, and a schema that is code.
		let sql = "do $$ declare n text := @extname@; s text := @extschema@; begin end $$";
		let got = substitute(
			sql,
			&values("@extschema@", Some(" || public.make_superuser() || ")),
		)
		.unwrap();
		assert_eq!(
			got,
			"do $$ declare n text := '@extschema@'; s text := ' || public.make_superuser() || '; begin end $$"
		);
	}

	#[test]
	fn a_quoting_character_is_refused() {
		for bad in ["a'b", "a\"b", "a$b", "a\\b"] {
			assert_eq!(
				substitute("@extname@", &values(bad, None)),
				Err(Refused::Name)
			);
			assert_eq!(
				substitute("@extname@", &values("x", Some(bad))),
				Err(Refused::Schema)
			);
		}
		let v = Values {
			name: Some("x"),
			schema: None,
			version: Some("1'"),
			cascade: false,
		};
		assert_eq!(substitute("", &v), Err(Refused::Version));
	}

	#[test]
	fn text_that_only_looks_like_a_variable_is_left_alone() {
		let sql = "select '@' || 'x@extname' || '@@extname@@'";
		assert_eq!(
			substitute(sql, &values("h", None)).unwrap(),
			"select '@' || 'x@extname' || '@'h'@'"
		);
	}
}
