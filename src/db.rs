//! Event persistence and querying
use crate::config;
use crate::error::Result;
use crate::event::Event;
use crate::subscription::Subscription;
use governor::clock::Clock;
use governor::{Quota, RateLimiter};
use hex;
use log::*;
use rusqlite::params;
use rusqlite::Connection;
use rusqlite::OpenFlags;
//use std::num::NonZeroU32;
use crate::config::SETTINGS;
use r2d2;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::limits::Limit;
use rusqlite::types::ToSql;
use std::path::Path;
use std::thread;
use std::time::Instant;
use tokio::task;

pub type SqlitePool = r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>;

/// Database file
const DB_FILE: &str = "nostr.db";

/// Startup DB Pragmas
const STARTUP_SQL: &str = r##"
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
pragma mmap_size = 536870912; -- 512MB of mmap
"##;

/// Schema definition
const INIT_SQL: &str = r##"
-- Database settings
PRAGMA encoding = "UTF-8";
PRAGMA journal_mode=WAL;
PRAGMA main.synchronous=NORMAL;
PRAGMA foreign_keys = ON;
PRAGMA application_id = 1654008667;
PRAGMA user_version = 3;

-- Event Table
CREATE TABLE IF NOT EXISTS event (
id INTEGER PRIMARY KEY,
event_hash BLOB NOT NULL, -- 4-byte hash
first_seen INTEGER NOT NULL, -- when the event was first seen (not authored!) (seconds since 1970)
created_at INTEGER NOT NULL, -- when the event was authored
author BLOB NOT NULL, -- author pubkey
kind INTEGER NOT NULL, -- event kind
hidden INTEGER, -- relevant for queries
content TEXT NOT NULL -- serialized json of event object
);

-- Event Indexes
CREATE UNIQUE INDEX IF NOT EXISTS event_hash_index ON event(event_hash);
CREATE INDEX IF NOT EXISTS created_at_index ON event(created_at);
CREATE INDEX IF NOT EXISTS author_index ON event(author);
CREATE INDEX IF NOT EXISTS kind_index ON event(kind);

-- Tag Table
-- Tag values are stored as either a BLOB (if they come in as a
-- hex-string), or TEXT otherwise.
-- This means that searches need to select the appropriate column.
CREATE TABLE IF NOT EXISTS tag (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains a tag.
name TEXT, -- the tag name ("p", "e", whatever)
value TEXT, -- the tag value, if not hex.
value_hex BLOB, -- the tag value, if it can be interpreted as a hex string.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS tag_val_index ON tag(value);
CREATE INDEX IF NOT EXISTS tag_val_hex_index ON tag(value_hex);

-- Event References Table
CREATE TABLE IF NOT EXISTS event_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #e tag.
referenced_event BLOB NOT NULL, -- the event that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);

-- Event References Index
CREATE INDEX IF NOT EXISTS event_ref_index ON event_ref(referenced_event);

-- Pubkey References Table
CREATE TABLE IF NOT EXISTS pubkey_ref (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains an #p tag.
referenced_pubkey BLOB NOT NULL, -- the pubkey that is referenced.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE RESTRICT ON DELETE CASCADE
);

-- Pubkey References Index
CREATE INDEX IF NOT EXISTS pubkey_ref_index ON pubkey_ref(referenced_pubkey);
"##;

pub fn build_read_pool() -> SqlitePool {
    let config = config::SETTINGS.read().unwrap();
    let db_dir = &config.database.data_directory;
    let full_path = Path::new(db_dir).join(DB_FILE);
    let manager = SqliteConnectionManager::file(&full_path)
        .with_flags(OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_init(|c| c.execute_batch(STARTUP_SQL));
    let pool: SqlitePool = r2d2::Pool::builder()
        .test_on_check_out(true) // no noticeable performance hit
        .min_idle(Some(config.database.min_conn))
        .max_size(config.database.max_conn)
        .build(manager)
        .unwrap();
    info!(
        "Built a connection pool (min={}, max={})",
        config.database.min_conn, config.database.max_conn
    );
    return pool;
}

/// Upgrade DB to latest version, and execute pragma settings
pub fn upgrade_db(conn: &mut Connection) -> Result<()> {
    // check the version.
    let mut curr_version = db_version(conn)?;
    info!("DB version = {:?}", curr_version);

    debug!(
        "SQLite max query parameters: {}",
        conn.limit(Limit::SQLITE_LIMIT_VARIABLE_NUMBER)
    );
    debug!(
        "SQLite max table/blob/text length: {} MB",
        (conn.limit(Limit::SQLITE_LIMIT_LENGTH) as f64 / (1024 * 1024) as f64).floor()
    );
    debug!(
        "SQLite max SQL length: {} MB",
        (conn.limit(Limit::SQLITE_LIMIT_SQL_LENGTH) as f64 / (1024 * 1024) as f64).floor()
    );

    // initialize from scratch
    if curr_version == 0 {
        match conn.execute_batch(INIT_SQL) {
            Ok(()) => {
                info!("database pragma/schema initialized to v3, and ready");
                //curr_version = 3;
            }
            Err(err) => {
                error!("update failed: {}", err);
                panic!("database could not be initialized");
            }
        }
    }
    if curr_version == 1 {
        // only change is adding a hidden column to events.
        let upgrade_sql = r##"
ALTER TABLE event ADD hidden INTEGER;
UPDATE event SET hidden=FALSE;
PRAGMA user_version = 2;
"##;
        match conn.execute_batch(upgrade_sql) {
            Ok(()) => {
                info!("database schema upgraded v1 -> v2");
                curr_version = 2;
            }
            Err(err) => {
                error!("update failed: {}", err);
                panic!("database could not be upgraded");
            }
        }
    }
    if curr_version == 2 {
        // this version lacks the tag column
        debug!("database schema needs update from 2->3");
        let upgrade_sql = r##"
CREATE TABLE IF NOT EXISTS tag (
id INTEGER PRIMARY KEY,
event_id INTEGER NOT NULL, -- an event ID that contains a tag.
name TEXT, -- the tag name ("p", "e", whatever)
value TEXT, -- the tag value, if not hex.
value_hex BLOB, -- the tag value, if it can be interpreted as a hex string.
FOREIGN KEY(event_id) REFERENCES event(id) ON UPDATE CASCADE ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS tag_val_index ON tag(value);
CREATE INDEX IF NOT EXISTS tag_val_hex_index ON tag(value_hex);
PRAGMA user_version = 3;
"##;
        // TODO: load existing refs into tag table
        match conn.execute_batch(upgrade_sql) {
            Ok(()) => {
                info!("database schema upgraded v2 -> v3");
                //curr_version = 3;
            }
            Err(err) => {
                error!("update failed: {}", err);
                panic!("database could not be upgraded");
            }
        }
        info!("Starting transaction");
        // iterate over every event/pubkey tag
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare("select event_id, \"e\", lower(hex(referenced_event)) from event_ref union select event_id, \"p\", lower(hex(referenced_pubkey)) from pubkey_ref;")?;
            let mut tag_rows = stmt.query([])?;
            while let Some(row) = tag_rows.next()? {
                // we want to capture the event_id that had the tag, the tag name, and the tag hex value.
                let event_id: u64 = row.get(0)?;
                let tag_name: String = row.get(1)?;
                let tag_value: String = row.get(2)?;
                // this will leave behind p/e tags that were non-hex, but they are invalid anyways.
                if is_hex(&tag_value) {
                    tx.execute(
                        "INSERT INTO tag (event_id, name, value_hex) VALUES (?1, ?2, ?3);",
                        params![event_id, tag_name, hex::decode(&tag_value).ok()],
                    )?;
                }
            }
        }
        tx.commit()?;
        info!("Upgrade complete");
    } else if curr_version == 3 {
        debug!("Database version was already current");
    } else if curr_version > 3 {
        panic!("Database version is newer than supported by this executable");
    }
    // Setup PRAGMA
    conn.execute_batch(STARTUP_SQL)?;
    info!("Finished pragma");
    Ok(())
}

/// Spawn a database writer that persists events to the SQLite store.
pub async fn db_writer(
    mut event_rx: tokio::sync::mpsc::Receiver<Event>,
    bcast_tx: tokio::sync::broadcast::Sender<Event>,
    mut shutdown: tokio::sync::broadcast::Receiver<()>,
) -> tokio::task::JoinHandle<Result<()>> {
    task::spawn_blocking(move || {
        // get database configuration settings
        let config = SETTINGS.read().unwrap();
        let db_dir = &config.database.data_directory;
        let full_path = Path::new(db_dir).join(DB_FILE);
        // create a connection
        let mut conn = Connection::open_with_flags(
            &full_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        info!("opened database {:?} for writing", full_path);
        upgrade_db(&mut conn)?;

        // get rate limit settings
        let rps_setting = config.limits.messages_per_sec;
        let mut most_recent_rate_limit = Instant::now();
        let mut lim_opt = None;
        let clock = governor::clock::QuantaClock::default();
        if let Some(rps) = rps_setting {
            if rps > 0 {
                info!("Enabling rate limits for event creation ({}/sec)", rps);
                let quota = core::num::NonZeroU32::new(rps * 60).unwrap();
                lim_opt = Some(RateLimiter::direct(Quota::per_minute(quota)));
            }
        }
        loop {
            if shutdown.try_recv().is_ok() {
                info!("shutting down database writer");
                break;
            }
            // call blocking read on channel
            let next_event = event_rx.blocking_recv();
            // if the channel has closed, we will never get work
            if next_event.is_none() {
                break;
            }
            let mut event_write = false;
            let event = next_event.unwrap();
            let start = Instant::now();
            match write_event(&mut conn, &event) {
                Ok(updated) => {
                    if updated == 0 {
                        debug!("ignoring duplicate event");
                    } else {
                        info!(
                            "persisted event: {} in {:?}",
                            event.get_event_id_prefix(),
                            start.elapsed()
                        );
                        event_write = true;
                        // send this out to all clients
                        bcast_tx.send(event.clone()).ok();
                    }
                }
                Err(err) => {
                    warn!("event insert failed: {}", err);
                }
            }
            // use rate limit, if defined, and if an event was actually written.
            if event_write {
                if let Some(ref lim) = lim_opt {
                    if let Err(n) = lim.check() {
                        let wait_for = n.wait_time_from(clock.now());
                        // check if we have recently logged rate
                        // limits, but print out a message only once
                        // per second.
                        if most_recent_rate_limit.elapsed().as_secs() > 1 {
                            warn!(
                                "rate limit reached for event creation (sleep for {:?})",
                                wait_for
                            );
                            // reset last rate limit message
                            most_recent_rate_limit = Instant::now();
                        }
                        // block event writes, allowing them to queue up
                        thread::sleep(wait_for);
                        continue;
                    }
                }
            }
        }
        conn.close().ok();
        info!("database connection closed");
        Ok(())
    })
}

pub fn db_version(conn: &mut Connection) -> Result<usize> {
    let query = "PRAGMA user_version;";
    let curr_version = conn.query_row(query, [], |row| row.get(0))?;
    Ok(curr_version)
}

/// Persist an event to the database.
pub fn write_event(conn: &mut Connection, e: &Event) -> Result<usize> {
    // start transaction
    let tx = conn.transaction()?;
    // get relevant fields from event and convert to blobs.
    let id_blob = hex::decode(&e.id).ok();
    let pubkey_blob = hex::decode(&e.pubkey).ok();
    let event_str = serde_json::to_string(&e).ok();
    // ignore if the event hash is a duplicate.
    let ins_count = tx.execute(
        "INSERT OR IGNORE INTO event (event_hash, created_at, kind, author, content, first_seen, hidden) VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'), FALSE);",
        params![id_blob, e.created_at, e.kind, pubkey_blob, event_str]
    )?;
    if ins_count == 0 {
        // if the event was a duplicate, no need to insert event or
        // pubkey references.
        return Ok(ins_count);
    }
    // remember primary key of the event most recently inserted.
    let ev_id = tx.last_insert_rowid();
    // add all tags to the tag table
    for tag in e.tags.iter() {
        // ensure we have 2 values.
        if tag.len() >= 2 {
            let tagname = &tag[0];
            let tagval = &tag[1];
            // if tagvalue is hex;
            if is_hex(tagval) {
                tx.execute(
                    "INSERT OR IGNORE INTO tag (event_id, name, value_hex) VALUES (?1, ?2, ?3)",
                    params![ev_id, &tagname, hex::decode(&tagval).ok()],
                )?;
            } else {
                tx.execute(
                    "INSERT OR IGNORE INTO tag (event_id, name, value) VALUES (?1, ?2, ?3)",
                    params![ev_id, &tagname, &tagval],
                )?;
            }
        }
    }
    // if this event is for a metadata update, hide every other kind=0
    // event from the same author that was issued earlier than this.
    if e.kind == 0 {
        let update_count = tx.execute(
            "UPDATE event SET hidden=TRUE WHERE id!=? AND kind=0 AND author=? AND created_at <= ? and hidden!=TRUE",
            params![ev_id, hex::decode(&e.pubkey).ok(), e.created_at],
        )?;
        if update_count > 0 {
            info!("hid {} older metadata events", update_count);
        }
    }
    // if this event is for a contact update, hide every other kind=3
    // event from the same author that was issued earlier than this.
    if e.kind == 3 {
        let update_count = tx.execute(
            "UPDATE event SET hidden=TRUE WHERE id!=? AND kind=3 AND author=? AND created_at <= ? and hidden!=TRUE",
            params![ev_id, hex::decode(&e.pubkey).ok(), e.created_at],
        )?;
        if update_count > 0 {
            info!("hid {} older contact events", update_count);
        }
    }
    tx.commit()?;
    Ok(ins_count)
}

/// Event resulting from a specific subscription request
#[derive(PartialEq, Debug, Clone)]
pub struct QueryResult {
    /// Subscription identifier
    pub sub_id: String,
    /// Serialized event
    pub event: String,
}

/// Check if a string contains only hex characters.
fn is_hex(s: &str) -> bool {
    s.chars().all(|x| char::is_ascii_hexdigit(&x))
}

/// Check if a string contains only f chars
fn is_all_fs(s: &str) -> bool {
    s.chars().all(|x| x == 'f' || x == 'F')
}

#[derive(PartialEq, Debug, Clone)]
enum HexSearch {
    // when no range is needed, exact 32-byte
    Exact(Vec<u8>),
    // lower (inclusive) and upper range (exclusive)
    Range(Vec<u8>, Vec<u8>),
    // lower bound only, upper bound is MAX inclusive
    LowerOnly(Vec<u8>),
}

/// Find the next hex sequence greater than the argument.
fn hex_range(s: &str) -> Option<HexSearch> {
    // handle special cases
    if !is_hex(s) || s.len() > 64 {
        return None;
    }
    if s.len() == 64 {
        return Some(HexSearch::Exact(hex::decode(s).ok()?));
    }
    // if s is odd, add a zero
    let mut hash_base = s.to_owned();
    let mut odd = hash_base.len() % 2 != 0;
    if odd {
        // extend the string to make it even
        hash_base.push('0');
    }
    let base = hex::decode(hash_base).ok()?;
    // check for all ff's
    if is_all_fs(s) {
        // there is no higher bound, we only want to search for blobs greater than this.
        return Some(HexSearch::LowerOnly(base));
    }

    // return a range
    let mut upper = base.clone();
    let mut byte_len = upper.len();

    // for odd strings, we made them longer, but we want to increment the upper char (+16).
    // we know we can do this without overflowing because we explicitly set the bottom half to 0's.
    while byte_len > 0 {
        byte_len -= 1;
        // check if byte can be incremented, or if we need to carry.
        let b = upper[byte_len];
        if b == u8::MAX {
            // reset and carry
            upper[byte_len] = 0;
        } else if odd {
            // check if first char in this byte is NOT 'f'
            if b < 240 {
                upper[byte_len] = b + 16; // bump up the first character in this byte
                                          // increment done, stop iterating through the vec
                break;
            } else {
                // if it is 'f', reset the byte to 0 and do a carry
                // reset and carry
                upper[byte_len] = 0;
            }
            // done with odd logic, so don't repeat this
            odd = false;
        } else {
            // bump up the first character in this byte
            upper[byte_len] = b + 1;
            // increment done, stop iterating
            break;
        }
    }
    Some(HexSearch::Range(base, upper))
}

fn repeat_vars(count: usize) -> String {
    if count == 0 {
        return "".to_owned();
    }
    let mut s = "?,".repeat(count);
    // Remove trailing comma
    s.pop();
    s
}

/// Create a dynamic SQL query string and params from a subscription.
fn query_from_sub(sub: &Subscription) -> (String, Vec<Box<dyn ToSql>>) {
    // build a dynamic SQL query.  all user-input is either an integer
    // (sqli-safe), or a string that is filtered to only contain
    // hexadecimal characters.  Strings that require escaping (tag
    // names/values) use parameters.
    let mut query =
        "SELECT DISTINCT(e.content) FROM event e LEFT JOIN tag t ON e.id=t.event_id ".to_owned();
    // parameters
    let mut params: Vec<Box<dyn ToSql>> = vec![];

    // for every filter in the subscription, generate a where clause
    let mut filter_clauses: Vec<String> = Vec::new();
    for f in sub.filters.iter() {
        // individual filter components
        let mut filter_components: Vec<String> = Vec::new();
        // Query for "authors", allowing prefix matches
        if let Some(authvec) = &f.authors {
            // take each author and convert to a hexsearch
            let mut auth_searches: Vec<String> = vec![];
            for auth in authvec {
                match hex_range(auth) {
                    Some(HexSearch::Exact(ex)) => {
                        auth_searches.push("author=?".to_owned());
                        params.push(Box::new(ex));
                    }
                    Some(HexSearch::Range(lower, upper)) => {
                        auth_searches.push("(author>? AND author<?)".to_owned());
                        params.push(Box::new(lower));
                        params.push(Box::new(upper));
                    }
                    Some(HexSearch::LowerOnly(lower)) => {
                        //                        info!("{:?} => lower; {:?} ", auth, hex::encode(lower));
                        auth_searches.push("author>?".to_owned());
                        params.push(Box::new(lower));
                    }
                    None => {
                        info!("Could not parse hex range from author {:?}", auth);
                    }
                }
            }
            let authors_clause = format!("({})", auth_searches.join(" OR "));
            filter_components.push(authors_clause);
        }
        // Query for Kind
        if let Some(ks) = &f.kinds {
            // kind is number, no escaping needed
            let str_kinds: Vec<String> = ks.iter().map(|x| x.to_string()).collect();
            let kind_clause = format!("kind IN ({})", str_kinds.join(", "));
            filter_components.push(kind_clause);
        }
        // Query for event, allowing prefix matches
        if let Some(idvec) = &f.ids {
            // take each author and convert to a hexsearch
            let mut id_searches: Vec<String> = vec![];
            for id in idvec {
                match hex_range(id) {
                    Some(HexSearch::Exact(ex)) => {
                        id_searches.push("event_hash=?".to_owned());
                        params.push(Box::new(ex));
                    }
                    Some(HexSearch::Range(lower, upper)) => {
                        id_searches.push("(event_hash>? AND event_hash<?)".to_owned());
                        params.push(Box::new(lower));
                        params.push(Box::new(upper));
                    }
                    Some(HexSearch::LowerOnly(lower)) => {
                        id_searches.push("event_hash>?".to_owned());
                        params.push(Box::new(lower));
                    }
                    None => {
                        info!("Could not parse hex range from id {:?}", id);
                    }
                }
            }
            let id_clause = format!("({})", id_searches.join(" OR "));
            filter_components.push(id_clause);
        }
        // Query for tags
        if let Some(map) = &f.tags {
            for (key, val) in map.iter() {
                let mut str_vals: Vec<Box<dyn ToSql>> = vec![];
                let mut blob_vals: Vec<Box<dyn ToSql>> = vec![];
                for v in val {
                    if is_hex(v) {
                        if let Ok(h) = hex::decode(&v) {
                            blob_vals.push(Box::new(h));
                        }
                    } else {
                        str_vals.push(Box::new(v.to_owned()));
                    }
                }
                // create clauses with "?" params for each tag value being searched
                let str_clause = format!("value IN ({})", repeat_vars(str_vals.len()));
                let blob_clause = format!("value_hex IN ({})", repeat_vars(blob_vals.len()));
                let tag_clause = format!("(name=? AND ({} OR {}))", str_clause, blob_clause);
                // add the tag name as the first parameter
                params.push(Box::new(key.to_owned()));
                // add all tag values that are plain strings as params
                params.append(&mut str_vals);
                // add all tag values that are blobs as params
                params.append(&mut blob_vals);
                filter_components.push(tag_clause);
            }
        }
        // Query for timestamp
        if f.since.is_some() {
            let created_clause = format!("created_at > {}", f.since.unwrap());
            filter_components.push(created_clause);
        }
        // Query for timestamp
        if f.until.is_some() {
            let until_clause = format!("created_at < {}", f.until.unwrap());
            filter_components.push(until_clause);
        }

        // combine all clauses, and add to filter_clauses
        if !filter_components.is_empty() {
            let mut fc = "( ".to_owned();
            fc.push_str(&filter_components.join(" AND "));
            fc.push_str(" )");
            filter_clauses.push(fc);
        }
    }

    // never display hidden events
    query.push_str(" WHERE hidden!=TRUE ");

    // combine all filters with OR clauses, if any exist
    if !filter_clauses.is_empty() {
        query.push_str(" AND (");
        query.push_str(&filter_clauses.join(" OR "));
        query.push_str(") ");
    }
    // add order clause
    query.push_str(" ORDER BY created_at ASC");
    debug!("query string: {}", query);
    (query, params)
}

/// Perform a database query using a subscription.
///
/// The [`Subscription`] is converted into a SQL query.  Each result
/// is published on the `query_tx` channel as it is returned.  If a
/// message becomes available on the `abandon_query_rx` channel, the
/// query is immediately aborted.
pub async fn db_query(
    sub: Subscription,
    conn: r2d2::PooledConnection<r2d2_sqlite::SqliteConnectionManager>,
    query_tx: tokio::sync::mpsc::Sender<QueryResult>,
    mut abandon_query_rx: tokio::sync::oneshot::Receiver<()>,
) {
    task::spawn_blocking(move || {
        debug!("going to query for: {:?}", sub);
        let mut row_count: usize = 0;
        let start = Instant::now();
        // generate SQL query
        let (q, p) = query_from_sub(&sub);
        // execute the query. Don't cache, since queries vary so much.
        let mut stmt = conn.prepare(&q)?;
        let mut event_rows = stmt.query(rusqlite::params_from_iter(p))?;
        while let Some(row) = event_rows.next()? {
            // check if this is still active (we could do this every N rows)
            if abandon_query_rx.try_recv().is_ok() {
                debug!("query aborted");
                return Ok(());
            }
            row_count += 1;
            let event_json = row.get(0)?;
            query_tx
                .blocking_send(QueryResult {
                    sub_id: sub.get_id(),
                    event: event_json,
                })
                .ok();
        }
        debug!(
            "query completed ({} rows) in {:?}",
            row_count,
            start.elapsed()
        );
        let ok: Result<()> = Ok(());
        ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_range_exact() -> Result<()> {
        let hex = "abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00";
        let r = hex_range(hex);
        assert_eq!(
            r,
            Some(HexSearch::Exact(hex::decode(hex).expect("invalid hex")))
        );
        Ok(())
    }
    #[test]
    fn hex_full_range() -> Result<()> {
        //let hex = "abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00abcdef00";
        let hex = "aaaa";
        let hex_upper = "aaab";
        let r = hex_range(hex);
        assert_eq!(
            r,
            Some(HexSearch::Range(
                hex::decode(hex).expect("invalid hex"),
                hex::decode(hex_upper).expect("invalid hex")
            ))
        );
        Ok(())
    }

    #[test]
    fn hex_full_range_odd() -> Result<()> {
        let r = hex_range("abc");
        assert_eq!(
            r,
            Some(HexSearch::Range(
                hex::decode("abc0").expect("invalid hex"),
                hex::decode("abd0").expect("invalid hex")
            ))
        );
        Ok(())
    }

    #[test]
    fn hex_full_range_odd_end_f() -> Result<()> {
        let r = hex_range("abf");
        assert_eq!(
            r,
            Some(HexSearch::Range(
                hex::decode("abf0").expect("invalid hex"),
                hex::decode("ac00").expect("invalid hex")
            ))
        );
        Ok(())
    }

    #[test]
    fn hex_no_upper() -> Result<()> {
        let r = hex_range("ffff");
        assert_eq!(
            r,
            Some(HexSearch::LowerOnly(
                hex::decode("ffff").expect("invalid hex")
            ))
        );
        Ok(())
    }

    #[test]
    fn hex_no_upper_odd() -> Result<()> {
        let r = hex_range("fff");
        assert_eq!(
            r,
            Some(HexSearch::LowerOnly(
                hex::decode("fff0").expect("invalid hex")
            ))
        );
        Ok(())
    }
}
