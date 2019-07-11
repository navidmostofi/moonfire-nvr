// This file is part of Moonfire NVR, a security camera digital video recorder.
// Copyright (C) 2016 Scott Lamb <slamb@slamb.org>
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// In addition, as a special exception, the copyright holders give
// permission to link the code of portions of this program with the
// OpenSSL library under certain conditions as described in each
// individual source file, and distribute linked combinations including
// the two.
//
// You must obey the GNU General Public License in all respects for all
// of the code used other than OpenSSL. If you modify file(s) with this
// exception, you may extend this exception to your version of the
// file(s), but you are not obligated to do so. If you do not wish to do
// so, delete this exception statement from your version. If you delete
// this exception statement from all source files in the program, then
// also delete it here.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

/// Upgrades the database schema.
///
/// See `guide/schema.md` for more information.

use crate::db;
use failure::{Error, bail};
use log::info;
use rusqlite::params;

mod v0_to_v1;
mod v1_to_v2;
mod v2_to_v3;
mod v3_to_v4;
mod v4_to_v5;

const UPGRADE_NOTES: &'static str =
    concat!("upgraded using moonfire-db ", env!("CARGO_PKG_VERSION"));

#[derive(Debug)]
pub struct Args<'a> {
    pub flag_sample_file_dir: Option<&'a str>,
    pub flag_preset_journal: &'a str,
    pub flag_no_vacuum: bool,
}

fn set_journal_mode(conn: &rusqlite::Connection, requested: &str) -> Result<(), Error> {
    assert!(!requested.contains(';'));  // quick check for accidental sql injection.
    let actual = conn.query_row(&format!("pragma journal_mode = {}", requested), params![],
                                |row| row.get::<_, String>(0))?;
    info!("...database now in journal_mode {} (requested {}).", actual, requested);
    Ok(())
}

fn upgrade(args: &Args, target_ver: i32, conn: &mut rusqlite::Connection) -> Result<(), Error> {
    let upgraders = [
        v0_to_v1::run,
        v1_to_v2::run,
        v2_to_v3::run,
        v3_to_v4::run,
        v4_to_v5::run,
    ];

    {
        assert_eq!(upgraders.len(), db::EXPECTED_VERSION as usize);
        let old_ver =
            conn.query_row("select max(id) from version", params![],
                           |row| row.get(0))?;
        if old_ver > db::EXPECTED_VERSION {
            bail!("Database is at version {}, later than expected {}",
                  old_ver, db::EXPECTED_VERSION);
        } else if old_ver < 0 {
            bail!("Database is at negative version {}!", old_ver);
        }
        info!("Upgrading database from version {} to version {}...", old_ver, target_ver);
        set_journal_mode(&conn, args.flag_preset_journal).unwrap();
        for ver in old_ver .. target_ver {
            info!("...from version {} to version {}", ver, ver + 1);
            let tx = conn.transaction()?;
            upgraders[ver as usize](&args, &tx)?;
            tx.execute(r#"
                insert into version (id, unix_time, notes)
                             values (?, cast(strftime('%s', 'now') as int32), ?)
            "#, params![&(ver + 1), &UPGRADE_NOTES])?;
            tx.commit()?;
        }
    }

    Ok(())
}

pub fn run(args: &Args, conn: &mut rusqlite::Connection) -> Result<(), Error> {
    // Enforce foreign keys. This is on by default with --features=bundled (as rusqlite
    // compiles the SQLite3 amalgamation with -DSQLITE_DEFAULT_FOREIGN_KEYS=1). Ensure it's
    // always on. Note that our foreign keys are immediate rather than deferred, so we have to
    // be careful about the order of operations during the upgrade.
    conn.execute("pragma foreign_keys = on", params![])?;

    // Make the database actually durable.
    conn.execute("pragma fullfsync = on", params![])?;
    conn.execute("pragma synchronous = 2", params![])?;

    upgrade(args, db::EXPECTED_VERSION, conn)?;

    // WAL is the preferred journal mode for normal operation; it reduces the number of syncs
    // without compromising safety.
    set_journal_mode(&conn, "wal").unwrap();
    if !args.flag_no_vacuum {
        info!("...vacuuming database after upgrade.");
        conn.execute_batch(r#"
            pragma page_size = 16384;
            vacuum;
        "#).unwrap();
    }
    info!("...done.");

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::compare;
    use super::*;

    fn new_conn() -> Result<rusqlite::Connection, Error> {
        let conn = rusqlite::Connection::open_in_memory()?;
        conn.execute("pragma foreign_keys = on", params![])?;
        conn.execute("pragma fullfsync = on", params![])?;
        conn.execute("pragma synchronous = 2", params![])?;
        Ok(conn)
    }

    fn compare(c: &rusqlite::Connection, ver: i32, fresh_sql: &str) -> Result<(), Error> {
        let fresh = new_conn()?;
        fresh.execute_batch(fresh_sql)?;
        if let Some(diffs) = compare::get_diffs("upgraded", &c, "fresh", &fresh)? {
            panic!("Version {}: differences found:\n{}", ver, diffs);
        }
        Ok(())
    }

    /// Upgrades and compares schemas.
    /// Doesn't (yet) compare any actual data.
    #[test]
    fn upgrade_and_compare() -> Result<(), Error> {
        let tmpdir = tempdir::TempDir::new("moonfire-nvr-test").unwrap();
        let path = tmpdir.path().to_str().unwrap().to_owned();
        let mut upgraded = new_conn()?;
        upgraded.execute_batch(include_str!("v0.sql"))?;

        for (ver, fresh_sql) in &[(1, Some(include_str!("v1.sql"))),
                                  (2, None),  // transitional; don't compare schemas.
                                  (3, Some(include_str!("v3.sql"))),
                                  (4, None),  // transitional; don't compare schemas.
                                  (4, Some(include_str!("../schema.sql")))] {
            upgrade(&Args {
                flag_sample_file_dir: Some(&path),
                flag_preset_journal: "delete",
                flag_no_vacuum: false,
            }, *ver, &mut upgraded)?;
            if let Some(f) = fresh_sql {
                compare(&upgraded, *ver, f)?;
            }
        }
        Ok(())
    }
}
