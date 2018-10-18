#![allow(proc_macro_derive_resolution_fallback)]

use std::{self, collections::HashMap, ops::Deref, sync::Arc};

use diesel::{
    delete,
    dsl::max,
    expression::sql_literal::sql,
    insert_into,
    mysql::MysqlConnection,
    r2d2::{ConnectionManager, PooledConnection},
    sql_query,
    sql_types::{BigInt, Integer, Text},
    update, Connection, ExpressionMethods, GroupByDsl, OptionalExtension, QueryDsl, RunQueryDsl,
};
#[cfg(test)]
use diesel_logger::LoggingConnection;
use futures::future;

use super::{
    diesel_ext::LockInShareModeDsl,
    pool::CollectionCache,
    schema::{bso, collections, user_collections},
};
use db::{
    error::{DbError, DbErrorKind},
    params, results,
    util::ms_since_epoch,
    Db, DbFuture, Sorting,
};
use settings::Settings;
use web::extractors::HawkIdentifier;

embed_migrations!();

no_arg_sql_function!(last_insert_id, Integer);

pub type Result<T> = std::result::Result<T, DbError>;
type Conn = PooledConnection<ConnectionManager<MysqlConnection>>;

// The ttl to use for rows that are never supposed to expire (in seconds)
pub const DEFAULT_BSO_TTL: u32 = 2100000000;

/// Run the diesel embedded migrations
///
/// Mysql DDL statements implicitly commit which could disrupt MysqlPool's
/// begin_test_transaction during tests. So this runs on its own separate conn.
pub fn run_embedded_migrations(settings: &Settings) -> Result<()> {
    let conn = MysqlConnection::establish(&settings.database_url).unwrap();
    Ok(embedded_migrations::run(&conn)?)
}

enum CollectionLock {
    Read,
    Write,
}

/// Per session Db metadata
pub(super) struct MysqlDbSession {
    /// The "current time" on the server used for this session's operations
    pub(super) timestamp: i64,
    /// Cache of collection modified timestamps per (user_id, collection_id)
    coll_modified_cache: HashMap<(u32, i32), i64>,
    /// Currently locked collections
    coll_locks: HashMap<(u32, i32), CollectionLock>,
}

impl Default for MysqlDbSession {
    fn default() -> Self {
        Self {
            timestamp: ms_since_epoch(),
            coll_modified_cache: Default::default(),
            coll_locks: Default::default(),
        }
    }
}

pub struct MysqlDb {
    #[cfg(not(test))]
    pub(super) conn: Conn,
    #[cfg(test)]
    pub(super) conn: LoggingConnection<Conn>,

    pub(super) session: MysqlDbSession,

    /// Pool level cache of collection_ids and their names
    coll_cache: Arc<CollectionCache>,
}

impl MysqlDb {
    pub fn new(conn: Conn, coll_cache: Arc<CollectionCache>) -> Self {
        MysqlDb {
            #[cfg(not(test))]
            conn,
            #[cfg(test)]
            conn: LoggingConnection::new(conn),
            coll_cache,
            session: Default::default(),
        }
    }

    /// APIs for collection-level locking
    ///
    /// Explicitly lock the matching row in the user_collections table. Read
    /// locks do SELECT ... LOCK IN SHARE MODE and write locks do SELECT
    /// ... FOR UPDATE.
    ///
    /// In theory it would be possible to use serializable transactions rather
    /// than explicit locking, but our ops team have expressed concerns about
    /// the efficiency of that approach at scale.
    pub fn lock_for_read(&mut self, user_id: HawkIdentifier, collection: &str) -> Result<()> {
        let user_id = user_id.legacy_id as u32;
        let collection_id = self
            .get_collection_id(collection)
            .or_else(|e| match e.kind() {
                // If the collection doesn't exist, we still want to start a
                // transaction so it will continue to not exist.
                DbErrorKind::CollectionNotFound => Ok(0),
                _ => Err(e),
            })?;
        // If we already have a read or write lock then it's safe to
        // use it as-is.
        if let Some(_) = self.session.coll_locks.get(&(user_id, collection_id)) {
            return Ok(());
        }

        // XXX: begin transaction here like python?
        // Lock the db
        let modified = user_collections::table
            .select(user_collections::modified)
            .filter(user_collections::user_id.eq(user_id as i32))
            .filter(user_collections::collection_id.eq(collection_id))
            .lock_in_share_mode()
            .first(&self.conn)
            .optional()?;
        if let Some(modified) = modified {
            self.session
                .coll_modified_cache
                .insert((user_id, collection_id), modified);
        }
        // XXX: who's responsible for unlocking
        self.session
            .coll_locks
            .insert((user_id, collection_id), CollectionLock::Read);
        Ok(())
    }

    pub fn lock_for_write(&mut self, user_id: HawkIdentifier, collection: &str) -> Result<()> {
        let user_id = user_id.legacy_id as u32;
        let collection_id = self.get_or_create_collection_id(collection)?;
        if let Some(CollectionLock::Read) = self.session.coll_locks.get(&(user_id, collection_id)) {
            Err(DbError::internal("Can't escalate read-lock to write-lock"))?
        }

        // Lock the db
        let modified = user_collections::table
            .select(user_collections::modified)
            .filter(user_collections::user_id.eq(user_id as i32))
            .filter(user_collections::collection_id.eq(collection_id))
            .for_update()
            .first(&self.conn)
            .optional()?;
        if let Some(modified) = modified {
            // Forbid the write if it would not properly incr the timestamp
            if modified >= self.session.timestamp {
                Err(DbErrorKind::Conflict)?
            }
            self.session
                .coll_modified_cache
                .insert((user_id, collection_id), modified);
        }
        self.session
            .coll_locks
            .insert((user_id, collection_id), CollectionLock::Write);
        Ok(())
    }

    pub fn delete_storage_sync(&self, user_id: u32) -> Result<()> {
        delete(bso::table)
            .filter(bso::user_id.eq(user_id as i32))
            .execute(&self.conn)?;
        delete(user_collections::table)
            .filter(user_collections::user_id.eq(user_id as i32))
            .execute(&self.conn)?;
        Ok(())
    }

    pub fn delete_collection_sync(&self, user_id: u32, collection: &str) -> Result<i64> {
        let collection_id = self.get_collection_id(collection)?;
        let mut count = delete(bso::table)
            .filter(bso::user_id.eq(user_id as i32))
            .filter(bso::collection_id.eq(&collection_id))
            .execute(&self.conn)?;
        count += delete(user_collections::table)
            .filter(user_collections::user_id.eq(user_id as i32))
            .filter(user_collections::collection_id.eq(&collection_id))
            .execute(&self.conn)?;
        if count == 0 {
            Err(DbErrorKind::CollectionNotFound)?
        }
        self.get_storage_modified_sync(user_id)
    }

    pub(super) fn create_collection(&self, name: &str) -> Result<i32> {
        // XXX: handle concurrent attempts at inserts
        let id = self.conn.transaction(|| {
            sql_query("INSERT INTO collections (name) VALUES (?)")
                .bind::<Text, _>(name)
                .execute(&self.conn)?;
            collections::table.select(last_insert_id).first(&self.conn)
        })?;
        self.coll_cache.put(id, name.to_owned())?;
        Ok(id)
    }

    fn get_or_create_collection_id(&self, name: &str) -> Result<i32> {
        self.get_collection_id(name).or_else(|e| match e.kind() {
            DbErrorKind::CollectionNotFound => self.create_collection(name),
            _ => Err(e),
        })
    }

    pub(super) fn get_collection_id(&self, name: &str) -> Result<i32> {
        if let Some(id) = self.coll_cache.get_id(name)? {
            return Ok(id);
        }

        let id = sql_query("SELECT id FROM collections WHERE name = ?")
            .bind::<Text, _>(name)
            .get_result::<IdResult>(&self.conn)
            .optional()?
            .ok_or(DbErrorKind::CollectionNotFound)?
            .id;
        self.coll_cache.put(id, name.to_owned())?;
        Ok(id)
    }

    fn _get_collection_name(&self, id: i32) -> Result<String> {
        let name = if let Some(name) = self.coll_cache.get_name(id)? {
            name
        } else {
            sql_query("SELECT name FROM collections where id = ?")
                .bind::<Integer, _>(&id)
                .get_result::<NameResult>(&self.conn)
                .optional()?
                .ok_or(DbErrorKind::CollectionNotFound)?
                .name
        };
        Ok(name)
    }

    pub fn put_bso_sync(&self, bso: &params::PutBso) -> Result<results::PutBso> {
        /*
        if bso.payload.is_none() && bso.sortindex.is_none() && bso.ttl.is_none() {
            // XXX: go returns an error here (ErrNothingToDo), and is treated
            // as other errors
            return Ok(());
        }
        */

        let collection_id = self.get_or_create_collection_id(&bso.collection)?;
        let user_id: u64 = bso.user_id.legacy_id;

        // XXX: consider mysql ON DUPLICATE KEY UPDATE?
        self.conn.transaction(|| {
            let q = r#"
                SELECT 1 as count FROM bso
                WHERE user_id = ? AND collection_id = ? AND id = ?
            "#;
            let exists = sql_query(q)
                .bind::<Integer, _>(user_id as i32) // XXX:
                .bind::<Integer, _>(&collection_id)
                .bind::<Text, _>(&bso.id)
                .get_result::<Count>(&self.conn)
                .optional()?
                .is_some();

            if exists {
                update(bso::table)
                    .filter(bso::user_id.eq(user_id as i32)) // XXX:
                    .filter(bso::collection_id.eq(&collection_id))
                    .filter(bso::id.eq(&bso.id))
                    .set(put_bso_as_changeset(&bso, self.session.timestamp))
                    .execute(&self.conn)?;
            } else {
                let payload = bso.payload.as_ref().map(Deref::deref).unwrap_or_default();
                let sortindex = bso.sortindex;
                let ttl = bso.ttl.map_or(DEFAULT_BSO_TTL, |ttl| ttl);
                insert_into(bso::table)
                    .values((
                        bso::user_id.eq(user_id as i32), // XXX:
                        bso::collection_id.eq(&collection_id),
                        bso::id.eq(&bso.id),
                        bso::sortindex.eq(sortindex),
                        bso::payload.eq(payload),
                        bso::modified.eq(&self.session.timestamp),
                        bso::expiry.eq(self.session.timestamp + ttl as i64),
                    )).execute(&self.conn)?;
            }
            self.touch_collection(user_id as u32, collection_id)
                .map(|timestamp| timestamp as u64)
        })
    }

    // XXX: limit/offset i64?
    pub fn get_bsos_sync(
        &self,
        user_id: u32,
        collection: &str,
        mut ids: &[&str],
        older: u64,
        newer: u64,
        sort: Sorting,
        limit: i64,
        offset: i64,
    ) -> Result<results::BSOs> {
        let collection_id = self.get_collection_id(collection)?;
        // XXX: ensure offset/limit/newer are valid

        // XXX: should error out (400 Bad Request) when more than 100
        // are provided (move to validation layer)
        if ids.len() > 100 {
            // spec says only 100 ids at a time
            ids = &ids[0..100];
        }

        // XXX: convert to raw SQL for use by other backends
        let mut query = bso::table
            //.select(bso::table::all_columns())
            .select((bso::id, bso::modified, bso::payload, bso::sortindex, bso::expiry))
            .filter(bso::user_id.eq(user_id as i32)) // XXX:
            .filter(bso::collection_id.eq(collection_id as i32)) // XXX:
            .filter(bso::modified.lt(older as i64))
            .filter(bso::modified.gt(newer as i64))
            .filter(bso::expiry.gt(&self.session.timestamp))
            .into_boxed();

        if !ids.is_empty() {
            query = query.filter(bso::id.eq_any(ids));
        }

        query = match sort {
            Sorting::Index => query.order(bso::sortindex.desc()),
            Sorting::Newest => query.order(bso::modified.desc()),
            Sorting::Oldest => query.order(bso::modified.asc()),
            _ => query,
        };

        // fetch an extra row to detect if there are more rows that
        // match the query conditions
        query = query.limit(if limit >= 0 { limit + 1 } else { limit });
        if offset != 0 {
            // XXX: copy over this optimization:
            // https://github.com/mozilla-services/server-syncstorage/blob/a0f8117/syncstorage/storage/sql/__init__.py#L404
            query = query.offset(offset);
        }
        let mut bsos = query.load::<results::GetBso>(&self.conn)?;

        let (more, next_offset) = if limit >= 0 && bsos.len() > limit as usize {
            bsos.pop();
            (true, limit + offset)
        } else {
            (false, 0)
        };

        Ok(results::BSOs {
            bsos,
            more,
            offset: next_offset,
        })
    }

    pub fn get_bso_sync(&self, params: &params::GetBso) -> Result<Option<results::GetBso>> {
        let collection_id = self.get_collection_id(&params.collection)?;
        let user_id = params.user_id.legacy_id;
        Ok(sql_query(r#"
               SELECT id, modified, payload, sortindex, expiry FROM bso
               WHERE user_id = ? AND collection_id = ? AND id = ? AND expiry >= ?
           "#)
           .bind::<Integer, _>(user_id as i32) // XXX:
           .bind::<Integer, _>(&collection_id)
           .bind::<Text, _>(&params.id)
           .bind::<BigInt, _>(&self.session.timestamp)
           .get_result::<results::GetBso>(&self.conn)
           .optional()?)
    }

    pub fn delete_bso_sync(&self, user_id: u32, collection: &str, bso_id: &str) -> Result<i64> {
        self.delete_bsos_sync(user_id, collection, &[bso_id])
    }

    pub fn delete_bsos_sync(&self, user_id: u32, collection: &str, bso_id: &[&str]) -> Result<i64> {
        let collection_id = self.get_collection_id(collection)?;
        delete(bso::table)
            .filter(bso::user_id.eq(user_id as i32))
            .filter(bso::collection_id.eq(&collection_id))
            .filter(bso::id.eq_any(bso_id))
            .execute(&self.conn)?;
        self.touch_collection(user_id, collection_id)
    }

    pub fn post_bsos_sync(
        &self,
        input: &params::PostCollection,
    ) -> Result<results::PostCollection> {
        let collection_id = self.get_or_create_collection_id(&input.collection)?;
        let mut result = results::PostCollection {
            modified: self.session.timestamp as u64,
            success: Default::default(),
            failed: Default::default(),
        };

        for pbso in &input.bsos {
            let put_result = self.put_bso_sync(&params::PutBso {
                user_id: input.user_id.clone(),
                collection: input.collection.clone(),
                id: pbso.id.clone(),
                payload: pbso.payload.as_ref().map(Into::into),
                sortindex: pbso.sortindex,
                ttl: pbso.ttl,
            });
            // XXX: python version doesn't report failures from db layer..
            // XXX: sanitize to.to_string()?
            match put_result {
                Ok(_) => result.success.push(pbso.id.clone()),
                Err(e) => {
                    result.failed.insert(pbso.id.clone(), e.to_string());
                }
            }
        }
        self.touch_collection(input.user_id.legacy_id as u32, collection_id)?;
        Ok(result)
    }

    pub fn get_storage_modified_sync(&self, user_id: u32) -> Result<i64> {
        Ok(user_collections::table
            .select(max(user_collections::modified))
            .filter(user_collections::user_id.eq(user_id as i32))
            .first::<Option<i64>>(&self.conn)?
            .unwrap_or_default())
    }

    pub fn get_collection_modified_sync(&self, user_id: u32, collection: &str) -> Result<i64> {
        let collection_id = self.get_collection_id(collection)?;
        if let Some(modified) = self
            .session
            .coll_modified_cache
            .get(&(user_id, collection_id))
        {
            return Ok(*modified);
        }
        user_collections::table
            .select(user_collections::modified)
            .filter(user_collections::user_id.eq(user_id as i32))
            .filter(user_collections::collection_id.eq(collection_id))
            .first(&self.conn)
            .optional()?
            .ok_or(DbErrorKind::CollectionNotFound.into())
    }

    pub fn get_bso_modified_sync(
        &self,
        user_id: u32,
        collection: &str,
        bso_id: &str,
    ) -> Result<i64> {
        let collection_id = self.get_collection_id(collection)?;
        bso::table
            .select(bso::modified)
            .filter(bso::user_id.eq(user_id as i32))
            .filter(bso::collection_id.eq(&collection_id))
            .filter(bso::id.eq(&bso_id))
            .first(&self.conn)
            .optional()?
            .ok_or(DbErrorKind::ItemNotFound.into())
    }

    pub fn get_collections_modified_sync(
        &self,
        params: &params::GetCollections,
    ) -> Result<results::GetCollections> {
        let modifieds =
            sql_query("SELECT collection_id, modified FROM user_collections WHERE user_id = ?")
                .bind::<Integer, _>(params.user_id.legacy_id as i32)
                .load::<UserCollectionsResult>(&self.conn)?
                .into_iter()
                .map(|cr| (cr.collection_id, cr.modified))
                .collect();
        self.map_collection_names(modifieds)
    }

    fn map_collection_names<T>(&self, by_id: HashMap<i32, T>) -> Result<HashMap<String, T>> {
        let names = self.load_collection_names(&by_id.keys().cloned().collect::<Vec<_>>())?;
        by_id
            .into_iter()
            .map(|(id, value)| {
                names
                    .get(&id)
                    .map(|name| (name.to_owned(), value))
                    .ok_or(DbError::internal("load_collection_names get"))
            }).collect()
    }

    fn load_collection_names(&self, collection_ids: &[i32]) -> Result<HashMap<i32, String>> {
        let mut names = HashMap::new();
        let mut uncached = Vec::new();
        for &id in collection_ids {
            if let Some(name) = self.coll_cache.get_name(id)? {
                names.insert(id, name);
            } else {
                uncached.push(id);
            }
        }

        let result = collections::table
            .select((collections::id, collections::name))
            .filter(collections::id.eq_any(uncached))
            .load::<(i32, String)>(&self.conn)?;

        for (id, name) in result {
            names.insert(id, name.clone());
            self.coll_cache.put(id, name)?;
        }
        Ok(names)
    }

    pub(super) fn touch_collection(&self, user_id: u32, collection_id: i32) -> Result<i64> {
        let upsert = r#"
                INSERT INTO user_collections (user_id, collection_id, modified)
                VALUES (?, ?, ?)
                ON DUPLICATE KEY UPDATE modified = ?
        "#;
        sql_query(upsert)
            .bind::<Integer, _>(user_id as i32)
            .bind::<Integer, _>(&collection_id)
            .bind::<BigInt, _>(&self.session.timestamp)
            .bind::<BigInt, _>(&self.session.timestamp)
            .execute(&self.conn)?;
        Ok(self.session.timestamp)
    }

    pub fn get_storage_size_sync(
        &self,
        user_id: HawkIdentifier,
    ) -> Result<results::GetStorageUsage> {
        let total_size = bso::table
            .select(sql::<BigInt>("SUM(LENGTH(payload))"))
            .filter(bso::user_id.eq(user_id.legacy_id as i32))
            .filter(bso::expiry.gt(&self.session.timestamp))
            .get_result::<i64>(&self.conn)?;
        Ok(total_size as u64)
    }

    pub fn get_collection_sizes_sync(
        &self,
        user_id: HawkIdentifier,
    ) -> Result<results::GetCollectionCounts> {
        let counts = bso::table
            .select((bso::collection_id, sql::<BigInt>("SUM(LENGTH(payload))")))
            .filter(bso::user_id.eq(user_id.legacy_id as i32))
            .filter(bso::expiry.gt(&self.session.timestamp))
            .group_by(bso::collection_id)
            .load(&self.conn)?
            .into_iter()
            .collect();
        self.map_collection_names(counts)
    }

    pub fn get_collection_counts_sync(
        &self,
        user_id: HawkIdentifier,
    ) -> Result<results::GetCollectionCounts> {
        let counts = bso::table
            .select((bso::collection_id, sql::<BigInt>("COUNT(collection_id)")))
            .filter(bso::user_id.eq(user_id.legacy_id as i32))
            .filter(bso::expiry.gt(&self.session.timestamp))
            .group_by(bso::collection_id)
            .load(&self.conn)?
            .into_iter()
            .collect();
        self.map_collection_names(counts)
    }
}

impl Db for MysqlDb {
    mock_db_method!(get_collection_id, GetCollectionId);
    mock_db_method!(get_collections, GetCollections);
    mock_db_method!(get_collection_counts, GetCollectionCounts);
    mock_db_method!(get_collection_usage, GetCollectionUsage);
    mock_db_method!(get_storage_usage, GetStorageUsage);
    mock_db_method!(delete_all, DeleteAll);
    mock_db_method!(delete_collection, DeleteCollection);
    mock_db_method!(get_collection, GetCollection);
    mock_db_method!(post_collection, PostCollection);
    mock_db_method!(delete_bso, DeleteBso);
    mock_db_method!(get_bso, GetBso);
    mock_db_method!(put_bso, PutBso);
}

#[derive(Debug, QueryableByName)]
struct IdResult {
    #[sql_type = "Integer"]
    id: i32,
}

#[allow(dead_code)] // Not really dead, Rust can't see the use above
#[derive(Debug, QueryableByName)]
struct NameResult {
    #[sql_type = "Text"]
    name: String,
}

#[derive(Debug, QueryableByName)]
struct UserCollectionsResult {
    #[sql_type = "Integer"]
    collection_id: i32,
    #[sql_type = "BigInt"]
    modified: i64,
}

#[derive(Debug, QueryableByName)]
struct Count {
    #[sql_type = "BigInt"]
    count: i64,
}

/// Formats a BSO for UPDATEs
#[derive(AsChangeset)]
#[table_name = "bso"]
struct UpdateBSO<'a> {
    pub sortindex: Option<i32>,
    pub payload: Option<&'a str>,
    pub modified: Option<i64>,
    pub expiry: Option<i64>,
}

fn put_bso_as_changeset<'a>(bso: &'a params::PutBso, modified: i64) -> UpdateBSO<'a> {
    UpdateBSO {
        sortindex: bso.sortindex,
        expiry: bso.ttl.map(|ttl| modified + ttl as i64),
        payload: bso.payload.as_ref().map(|payload| &**payload),
        modified: if bso.payload.is_some() || bso.sortindex.is_some() {
            Some(modified)
        } else {
            None
        },
    }
}