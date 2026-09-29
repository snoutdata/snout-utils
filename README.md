# snout_utils

Lets a hosted Postgres database's **owner** do the handful of things Postgres reserves for a
superuser (manage the extensions the platform offers, create publications, foreign-data wrappers
and event triggers, set a few superuser-only settings) **without ever being a superuser**. A
Postgres library written in Rust with [pgrx](https://github.com/pgcentralfoundation/pgrx), built to
run in every SnoutData Cloud project.

> **Status: built, not yet in production.** SnoutData Cloud's pod image builds it in; the fleet
> moves to it when that image is rolled out.

- **Configuration only.** It is loaded with `shared_preload_libraries` and set with five
  settings. It adds no table, function or security label to any database, so a dump and restore,
  or a major-version upgrade, carries nothing of it. It has no control file and is not something a
  database can `CREATE EXTENSION`.
- **Delegation, narrowly.** When the privileged role (or a role granted it) runs one of the
  statements below, that one statement runs as the configured superuser, and whatever it created
  is given back to the role that asked where Postgres allows it. Every other role, and every other
  statement, gets exactly what Postgres gives it.
- **Errors untouched.** A statement that fails, delegated or not, fails with Postgres's own ERROR:
  position, detail, hint and context lines included. The caller's role is put back on every path,
  an ERROR included.

## What the privileged role may do

| Statement | As the superuser when | Afterwards |
|---|---|---|
| `CREATE EXTENSION` | the extension is on `snout_utils.privileged_extensions` and installed on the server (it has a control file) | its custom script runs, as the superuser |
| `ALTER EXTENSION ... UPDATE`, `ALTER EXTENSION ... SET SCHEMA`, `DROP EXTENSION` | every extension named is on the list and installed | |
| `COMMENT ON EXTENSION` | always | |
| `CREATE PUBLICATION` | always, so `FOR ALL TABLES` works | the publication is the caller's |
| `ALTER PUBLICATION` | always | |
| `CREATE FOREIGN DATA WRAPPER` | the handler and validator are both given, both belong to one installed extension, and are then written schema-qualified | the wrapper is the caller's |
| `CREATE EVENT TRIGGER` | the function is not a superuser's | the trigger is the caller's |
| `SET`, `ALTER ROLE ... SET` of a setting on `snout_utils.privileged_role_allowed_configs` | the target role is not a superuser | |
| `ALTER ROLE` | the target has no privileged-role privileges, and the statement does not set SUPERUSER | |

A role that is not the privileged role, and has not been granted it, is never helped: a login role
made for a reporting tool cannot create or drop an extension, whatever the list says.

## Event triggers

An event trigger fires for whoever runs the statement, so one the owner made would run its function
with a superuser's rights the moment a superuser ran DDL. snout_utils does not let that happen: for
every event trigger function Postgres is about to call, a superuser runs it only if it owns the
function itself; everyone else runs what they would have run anyway. A function that is not run is
swapped for one that does nothing an event trigger would notice. A SECURITY DEFINER function is
judged by the role that ran the statement, not by the function's owner.

## Custom scripts

`snout_utils.extension_custom_scripts_path` names a folder. Around every `CREATE EXTENSION`, whoever
runs it and whether or not it is delegated, snout_utils runs, as the superuser:

- `<folder>/before-create.sql`, before any extension;
- `<folder>/<extension>/before-create.sql` and `<folder>/<extension>/after-create.sql`.

A missing file is skipped. The scripts are where a platform grants the owner what an extension
created (its schema, its functions) and revokes what it should not have handed to everyone.

In a script, `@extname@`, `@extschema@`, `@extversion@` and `@extcascade@` are replaced by the
statement's values as SQL literals (`null` when not given, `true`/`false` for cascade), in one pass,
so a value is never expanded twice. A value containing a double quote, a single quote, a dollar
sign or a backslash is refused rather than escaped, because no single way of quoting is safe inside
a literal, a dollar-quoted body and an identifier alike. A name that is not a valid extension name
never becomes a path. A `CREATE EXTENSION` run by a custom script does not run scripts of its own.

## Configuration

Every setting is `sighup`: the server's configuration or command line sets it, a reload changes it,
and no session can. The prefix is reserved, so a misspelt `snout_utils.*` setting is refused.

| Setting | Default | What |
|---|---|---|
| `snout_utils.superuser` | the role that created the cluster | The superuser delegated statements run as |
| `snout_utils.privileged_role` | none (nothing is delegated) | The role that may run the statements above, with every role granted it |
| `snout_utils.privileged_extensions` | none | Extensions it may create, update, move and drop: a comma-separated list, as `search_path` is written |
| `snout_utils.extension_custom_scripts_path` | none | The custom scripts' folder |
| `snout_utils.privileged_role_allowed_configs` | none | Superuser-only settings it may set: a comma-separated list, where a name ending in `*` covers every setting starting with what comes before it (`pgaudit.*`) |

A list that does not parse is refused when it is set, and the previous value stays.

```
shared_preload_libraries = 'snout_utils'   # last, so its hook runs before any other
snout_utils.superuser = 'platform_admin'
snout_utils.privileged_role = 'app_owner'
snout_utils.privileged_extensions = 'postgis,vector,pg_cron,hstore'
snout_utils.extension_custom_scripts_path = '/usr/local/share/extension-custom-scripts'
snout_utils.privileged_role_allowed_configs = 'session_replication_role,pgaudit.*'
```

## Building and testing

Postgres 17. Everything runs in the crate's own container (`container/Containerfile`: Rust,
cargo-pgrx and a Postgres built with assertions), so nothing depends on the machine it runs from.

```sh
bash scripts/dev.sh cargo pgrx test pg17                  # unit tests
bash scripts/dev.sh cargo clippy --lib -- -D warnings
bash scripts/dev.sh cargo deny --locked check              # licences, bans, advisories, sources
bash scripts/build-dist.sh tools 17 && bash scripts/build-dist.sh build 17 /out   # the shipped library
```

`build-dist.sh` is the one recipe for a build that ships: it expects Debian bookworm, installs the
Postgres server headers, and writes `/out/lib/snout_utils.so` (stripped) and its symbols to
`/out/debug`. The parsers of untrusted text (the list settings, the script substitution) are fuzzed
by the stack's `scripts/fuzz.sh` (`utils_names`, `utils_script`).

## Operations

- **Health.** It has no process of its own. A server whose `shared_preload_libraries` names it and
  cannot load it does not start, which is the failure to watch for after an image change.
- **Logs.** It writes nothing in normal operation; a refusal is an ERROR the client sees and
  Postgres logs.
- **Upgrade.** Replace the library and restart the server. There is nothing in any database to
  migrate.

## Licence

[Apache License 2.0](./LICENSE). Security reports: [SECURITY.md](./SECURITY.md).
