use async_lock::RwLock;
use std::{
    any::{Any, TypeId},
    future::Future,
    hash::{BuildHasher, Hash},
    sync::Arc,
};

type ErrorObject = Arc<dyn Any + Send + Sync + 'static>;
type Waiter<V> = Arc<RwLock<Option<Result<V, ErrorObject>>>>;

pub(crate) enum InitResult<V, E> {
    Initialized(V),
    ReadExisting(V),
    InitErr(Arc<E>),
}

pub(crate) struct ValueInitializer<K, V, S> {
    // TypeId is the type ID of the concrete error type of generic type E in
    // try_init_or_read(). We use the type ID as a part of the key to ensure that
    // we can always downcast the trait object ErrorObject (in Waiter<V>) into
    // its concrete type.
    waiters: moka_cht::SegmentedHashMap<(Arc<K>, TypeId), Waiter<V>, S>,
}

impl<K, V, S> ValueInitializer<K, V, S>
where
    Arc<K>: Eq + Hash,
    V: Clone,
    S: BuildHasher,
{
    pub(crate) fn with_hasher(hasher: S) -> Self {
        Self {
            waiters: moka_cht::SegmentedHashMap::with_num_segments_and_hasher(16, hasher),
        }
    }

    pub(crate) async fn init_or_read<F>(&self, key: Arc<K>, init: F) -> InitResult<V, ()>
    where
        F: Future<Output = V>,
    {
        use InitResult::*;

        let type_id = TypeId::of::<()>();
        let waiter = Arc::new(RwLock::new(None));
        let mut lock = waiter.write().await;

        match self.try_insert_waiter(&key, type_id, &waiter) {
            None => {
                // Our waiter was inserted. Let's resolve the init future.
                let value = init.await;
                *lock = Some(Ok(value.clone()));
                Initialized(value)
            }
            Some(res) => {
                // Somebody else's waiter already exists. Drop our write lock and wait
                // for a read lock to become available.
                std::mem::drop(lock);
                match &*res.read().await {
                    Some(Ok(value)) => ReadExisting(value.clone()),
                    Some(Err(_)) | None => unreachable!(),
                }
            }
        }
    }

    pub(crate) async fn try_init_or_read<F, E>(&self, key: Arc<K>, init: F) -> InitResult<V, E>
    where
        F: Future<Output = Result<V, E>>,
        E: Send + Sync + 'static,
    {
        use InitResult::*;

        let type_id = TypeId::of::<E>();
        let waiter = Arc::new(RwLock::new(None));
        let mut lock = waiter.write().await;

        match self.try_insert_waiter(&key, type_id, &waiter) {
            None => {
                // Our waiter was inserted. Let's resolve the init future.
                match init.await {
                    Ok(value) => {
                        *lock = Some(Ok(value.clone()));
                        Initialized(value)
                    }
                    Err(e) => {
                        let err: ErrorObject = Arc::new(e);
                        *lock = Some(Err(Arc::clone(&err)));
                        self.remove_waiter(&key, type_id);
                        InitErr(err.downcast().unwrap())
                    }
                }
            }
            Some(res) => {
                // Somebody else's waiter already exists. Drop our write lock and wait
                // for a read lock to become available.
                std::mem::drop(lock);
                match &*res.read().await {
                    Some(Ok(value)) => ReadExisting(value.clone()),
                    Some(Err(e)) => InitErr(Arc::clone(e).downcast().unwrap()),
                    None => unreachable!(),
                }
            }
        }
    }

    #[inline]
    pub(crate) fn remove_waiter(&self, key: &Arc<K>, type_id: TypeId) {
        let key = Arc::clone(key);
        self.waiters.remove(&(key, type_id));
    }

    fn try_insert_waiter(
        &self,
        key: &Arc<K>,
        type_id: TypeId,
        waiter: &Waiter<V>,
    ) -> Option<Waiter<V>> {
        let key = Arc::clone(key);
        let waiter = Arc::clone(waiter);

        self.waiters
            .insert_with_or_modify((key, type_id), || waiter, |_, w| Arc::clone(w))
    }
}
