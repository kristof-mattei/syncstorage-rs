//! Parameter types for database methods.

#![allow(proc_macro_derive_resolution_fallback)]

use web::extractors::{BatchBsoBody, BsoQueryParams, HawkIdentifier};

macro_rules! data {
    ($name:ident {$($property:ident: $type:ty,)*}) => {
        #[derive(Debug)]
        pub struct $name {
            $(pub $property: $type,)*
        }
    }
}

macro_rules! uid_data {
    ($($name:ident,)+) => ($(
        pub type $name = HawkIdentifier;
    )+)
}

macro_rules! collection_data {
    ($($name:ident {$($property:ident: $type:ty,)*},)+) => ($(
        data! {
            $name {
                user_id: HawkIdentifier,
                collection: String,
                $($property: $type,)*
            }
        }
    )+)
}

macro_rules! bso_data {
    ($($name:ident {$($property:ident: $type:ty,)*},)+) => ($(
        data! {
            $name {
                user_id: HawkIdentifier,
                collection: String,
                id: String,
                $($property: $type,)*
            }
        }
    )+)
}

uid_data! {
    GetCollectionTimestamps,
    GetCollectionCounts,
    GetCollectionUsage,
    GetStorageTimestamp,
    GetStorageUsage,
    DeleteStorage,
}

collection_data! {
    LockCollection {},
    DeleteCollection {},
    GetCollectionTimestamp {},
    DeleteBsos {
        ids: Vec<String>,
    },
    GetBsos {
        params: BsoQueryParams,
    },
    PostBsos {
        bsos: Vec<PostCollectionBso>,
    },

    CreateBatch {
        bsos: Vec<PostCollectionBso>,
    },
    ValidateBatch {
        id: i64,
    },
    AppendToBatch {
        id: i64,
        bsos: Vec<PostCollectionBso>,
    },
    CommitBatch {
        batch: Batch,
    },
    GetBatch {
        id: i64,
    },
    DeleteBatch {
        id: i64,
    },
}

pub type GetBsoIds = GetBsos;

bso_data! {
    DeleteBso {},
    GetBso {},
    GetBsoTimestamp {},
}

#[derive(Debug, Default, Queryable)]
pub struct Batch {
    pub id: i64,
    pub bsos: String,
    pub expiry: i64,
}

pub struct PutBso {
    pub user_id: HawkIdentifier,
    pub collection: String,
    pub id: String,
    pub sortindex: Option<i32>,
    pub payload: Option<String>,
    // ttl in seconds
    pub ttl: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PostCollectionBso {
    pub id: String,
    pub sortindex: Option<i32>,
    pub payload: Option<String>,
    // ttl in seconds
    pub ttl: Option<u32>,
}

impl From<BatchBsoBody> for PostCollectionBso {
    fn from(b: BatchBsoBody) -> PostCollectionBso {
        PostCollectionBso {
            id: b.id,
            sortindex: b.sortindex,
            payload: b.payload,
            ttl: b.ttl,
        }
    }
}
