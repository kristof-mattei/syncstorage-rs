//! Parameter types for database methods.
use db::Sorting;
use web::extractors::{BatchBsoBody, HawkIdentifier};

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
    GetCollectionModifieds,
    GetCollectionCounts,
    GetCollectionUsage,
    GetStorageModified,
    GetStorageUsage,
    DeleteStorage,
}

collection_data! {
    LockCollection {},
    DeleteCollection {},
    DeleteBsos {
        ids: Vec<String>,
    },
    GetBsos {
        ids: Vec<String>,
        older: u64,
        newer: u64,
        sort: Sorting,
        limit: i64,  // XXX: limit/offset i64?
        offset: i64,
    },
    PostBsos {
        bsos: Vec<PostCollectionBso>,
    },
}

bso_data! {
    DeleteBso {},
    GetBso {},
}

pub struct PutBso {
    pub user_id: HawkIdentifier,
    pub collection: String,
    pub id: String,
    pub sortindex: Option<i32>,
    pub payload: Option<String>,
    pub ttl: Option<u32>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PostCollectionBso {
    pub id: String,
    pub sortindex: Option<i32>,
    pub payload: Option<String>,
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
