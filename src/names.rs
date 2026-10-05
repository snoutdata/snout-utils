//! Lists of names, as the settings hold them. Pure: no Postgres, no `unsafe`, so it is tested with
//! plain `#[test]`s and fuzzed.
#![forbid(unsafe_code)]

/// Postgres's own limit on an identifier, in bytes, less its terminator (NAMEDATALEN - 1).
const MAX_IDENTIFIER_BYTES: usize = 63;

/// Splits a comma-separated list of identifiers the way Postgres splits one for a setting like
/// `search_path`: whitespace around each name is ignored, an unquoted name is folded to lower case,
/// a double-quoted one is kept as written (`""` inside it is one quote), and a name longer than an
/// identifier may be is cut to 63 bytes. An empty or all-blank string is an empty list.
///
/// `None` when the text is not such a list (an empty name, a missing comma, an unclosed quote), so
/// a setting holding it is refused rather than half read.
pub fn split(text: &str) -> Option<Vec<String>> {
	let mut names = Vec::new();
	let mut chars = text.chars().peekable();
	skip_space(&mut chars);
	if chars.peek().is_none() {
		return Some(names);
	}
	loop {
		let name = if chars.peek() == Some(&'"') {
			chars.next();
			let mut name = String::new();
			loop {
				match chars.next() {
					None => return None,
					Some('"') if chars.peek() == Some(&'"') => {
						chars.next();
						name.push('"');
					}
					Some('"') => break,
					Some(c) => name.push(c),
				}
			}
			if name.is_empty() {
				return None;
			}
			name
		} else {
			let mut name = String::new();
			while let Some(&c) = chars.peek() {
				if c == ',' || is_space(c) {
					break;
				}
				name.push(c);
				chars.next();
			}
			if name.is_empty() {
				return None;
			}
			name.to_ascii_lowercase()
		};
		names.push(truncate(name));
		skip_space(&mut chars);
		match chars.next() {
			None => return Some(names),
			Some(',') => skip_space(&mut chars),
			Some(_) => return None,
		}
	}
}

/// Whether `name` is on `list`. An entry ending in `*` (and longer than the star alone) matches
/// every name that starts with what comes before it, so `pgaudit.*` covers `pgaudit.log`.
///
/// A list that does not parse matches nothing: a setting is checked when it is set, so this is a
/// value that got past that check, and nothing is delegated on the strength of it.
pub fn contains(list: &str, name: &str) -> bool {
	let Some(entries) = split(list) else {
		return false;
	};
	entries.iter().any(|entry| match entry.strip_suffix('*') {
		Some(prefix) if !prefix.is_empty() => name.starts_with(prefix),
		_ => entry == name,
	})
}

/// Whether `name` could be the name of an extension's control file, which is what decides whether
/// it is installed on this server at all. Postgres refuses the same names when it is asked to create
/// one; asking here too means a name like `../x` never reaches a path.
pub fn is_extension_name(name: &str) -> bool {
	!name.is_empty()
		&& !name.contains("--")
		&& !name.starts_with('-')
		&& !name.ends_with('-')
		&& !name.contains('/')
		&& !name.contains('\\')
		&& !name.contains('\0')
		&& name != "."
		&& name != ".."
}

fn truncate(mut name: String) -> String {
	if name.len() > MAX_IDENTIFIER_BYTES {
		let mut end = MAX_IDENTIFIER_BYTES;
		while !name.is_char_boundary(end) {
			end -= 1;
		}
		name.truncate(end);
	}
	name
}

fn is_space(c: char) -> bool {
	// Postgres's scanner_isspace: space, tab, newline, carriage return, form feed.
	matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0c')
}

fn skip_space(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
	while chars.peek().copied().is_some_and(is_space) {
		chars.next();
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn splits_the_way_a_setting_is_read() {
		assert_eq!(split(""), Some(vec![]));
		assert_eq!(split("  "), Some(vec![]));
		assert_eq!(split("a,b"), Some(vec!["a".into(), "b".into()]));
		assert_eq!(split(" a , b "), Some(vec!["a".into(), "b".into()]));
		assert_eq!(
			split("uuid-ossp,pg_net"),
			Some(vec!["uuid-ossp".into(), "pg_net".into()])
		);
		assert_eq!(split("Hstore"), Some(vec!["hstore".into()]));
		assert_eq!(split("\"Hstore\""), Some(vec!["Hstore".into()]));
		assert_eq!(split("\"a\"\"b\""), Some(vec!["a\"b".into()]));
		assert_eq!(split("\"a,b\",c"), Some(vec!["a,b".into(), "c".into()]));
		assert_eq!(split("pgaudit.*"), Some(vec!["pgaudit.*".into()]));
	}

	#[test]
	fn refuses_what_is_not_a_list() {
		assert_eq!(split(","), None);
		assert_eq!(split("a,"), None);
		assert_eq!(split("a,,b"), None);
		assert_eq!(split("a b"), None);
		assert_eq!(split("\"a"), None);
		assert_eq!(split("\"\""), None);
		assert_eq!(split("\"a\"b"), None);
	}

	#[test]
	fn cuts_a_long_name_as_postgres_does() {
		let long = "a".repeat(70);
		assert_eq!(split(&long).unwrap()[0].len(), 63);
		// never inside a character
		let wide = "é".repeat(40);
		let cut = &split(&format!("\"{wide}\"")).unwrap()[0];
		assert!(cut.len() <= 63 && wide.starts_with(cut.as_str()));
	}

	#[test]
	fn a_trailing_star_is_a_prefix() {
		assert!(contains(
			"session_replication_role,pgaudit.*",
			"pgaudit.log"
		));
		assert!(contains(
			"session_replication_role,pgaudit.*",
			"session_replication_role"
		));
		assert!(!contains("session_replication_role,pgaudit.*", "pgaudit"));
		assert!(!contains(
			"session_replication_role,pgaudit.*",
			"log_min_messages"
		));
		// a star alone is a name, not "everything"
		assert!(!contains("*", "anything"));
		assert!(contains("*", "*"));
		assert!(!contains("a,,b", "a"));
	}

	#[test]
	fn extension_names_never_become_paths() {
		assert!(is_extension_name("uuid-ossp"));
		assert!(is_extension_name("postgis_raster"));
		for bad in [
			"", "..", ".", "../x", "a/b", "a\\b", "a--b", "-a", "a-", "a\0b",
		] {
			assert!(!is_extension_name(bad), "{bad:?}");
		}
	}
}
