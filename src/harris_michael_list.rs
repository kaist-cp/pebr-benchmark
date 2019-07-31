use crate::concurrent_map::ConcurrentMap;
use crossbeam_epoch::{unprotected, Atomic, Guard, Owned, Shared};

use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::Ordering;

#[derive(Debug)]
struct Node<K, V> {
    key: K,

    value: ManuallyDrop<V>,

    // Mark: tag()
    // Tag: not needed
    next: Atomic<Node<K, V>>,
}

pub struct List<K, V> {
    head: Atomic<Node<K, V>>,
}

impl<K, V> Default for List<K, V>
where
    K: Ord,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V> Drop for List<K, V> {
    fn drop(&mut self) {
        unsafe {
            let mut curr = self.head.load(Ordering::Relaxed, unprotected());

            while !curr.is_null() {
                let curr_ref = curr.deref_mut();
                ManuallyDrop::drop(&mut curr_ref.value);
                let next = curr_ref.next.load(Ordering::Relaxed, unprotected());
                drop(curr.into_owned());
                curr = next;
            }
        }
    }
}

struct Cursor<'g, K, V> {
    prev: &'g Atomic<Node<K, V>>,
    curr: Shared<'g, Node<K, V>>,
}

impl<K, V> List<K, V>
where
    K: Ord,
{
    pub fn new() -> Self {
        List {
            head: Atomic::null(),
        }
    }

    /// Returns (1) whether it found an entry, and (2) a cursor.
    #[inline]
    fn find_inner<'g>(&'g self, key: &K, guard: &'g Guard) -> Result<(bool, Cursor<'g, K, V>), ()> {
        let mut cursor = Cursor {
            prev: &self.head,
            curr: self.head.load(Ordering::Acquire, guard),
        };

        loop {
            debug_assert_eq!(cursor.curr.tag(), 0);

            let curr_node = match unsafe { cursor.curr.as_ref() } {
                None => return Ok((false, cursor)),
                Some(c) => c,
            };

            if cursor.prev.load(Ordering::Acquire, guard) != cursor.curr {
                return Err(());
            }

            let mut next = curr_node.next.load(Ordering::Acquire, guard);

            let curr_key = &curr_node.key;
            if next.tag() == 0 {
                if curr_key >= key {
                    return Ok((curr_key == key, cursor));
                }
                cursor.prev = &curr_node.next;
            } else {
                next = next.with_tag(0);
                match cursor.prev.compare_and_set(
                    cursor.curr,
                    next,
                    Ordering::AcqRel,
                    guard,
                ) {
                    Err(_) => return Err(()),
                    Ok(_) => unsafe { guard.defer_destroy(cursor.curr) },
                }
            }
            cursor.curr = next;
        }
    }

    fn find<'g>(&'g self, key: &K, guard: &'g Guard) -> (bool, Cursor<'g, K, V>) {
        loop {
            if let Ok(r) = self.find_inner(key, guard) {
                return r;
            }
        }
    }

    pub fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        let (found, cursor) = self.find(key, guard);

        if found {
            unsafe { cursor.curr.as_ref().map(|n| &*n.value) }
        } else {
            None
        }
    }

    pub fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        let mut node = Owned::new(Node {
            key,
            value: ManuallyDrop::new(value),
            next: Atomic::null(),
        });

        loop {
            let (found, cursor) = self.find(&node.key, &guard);
            if found {
                unsafe { ManuallyDrop::drop(&mut node.value); }
                return false;
            }

            node.next.store(cursor.curr, Ordering::Relaxed);
            match cursor
                .prev
                .compare_and_set(cursor.curr, node, Ordering::AcqRel, &guard)
            {
                Ok(_) => return true,
                Err(e) => node = e.new,
            }
        }
    }

    pub fn remove(&self, key: &K, guard: &Guard) -> Option<V> {
        loop {
            let (found, cursor) = self.find(key, &guard);
            if !found {
                return None;
            }

            let curr_node = unsafe { cursor.curr.as_ref() }.unwrap();
            let value = unsafe { ptr::read(&curr_node.value) };

            let next = curr_node.next.fetch_or(1, Ordering::AcqRel, &guard);
            if next.tag() == 1 {
                continue;
            }

            match cursor
                .prev
                .compare_and_set(cursor.curr, next, Ordering::AcqRel, &guard)
            {
                Ok(_) => unsafe { guard.defer_destroy(cursor.curr) },
                Err(_) => {
                    self.find(key, &guard);
                }
            }

            return Some(ManuallyDrop::into_inner(value));
        }
    }
}

impl<K, V> ConcurrentMap<K, V> for List<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        Self::new()
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.insert(key, value, guard)
    }
    #[inline]
    fn remove(&self, key: &K, guard: &Guard) -> Option<V> {
        self.remove(key, guard)
    }
}

#[cfg(test)]
mod tests {
    extern crate rand;
    use super::List;
    use crossbeam_utils::thread;
    use rand::prelude::*;

    #[test]
    fn smoke_list() {
        let list = &List::new();

        // insert
        thread::scope(|s| {
            for t in 0..10 {
                s.spawn(move |_| {
                    let mut rng = rand::thread_rng();
                    let mut keys: Vec<i32> = (0..1000).map(|k| k * 10 + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert!(list.insert(i, i.to_string(), &crossbeam_epoch::pin()));
                    }
                });
            }
        })
        .unwrap();

        // remove
        thread::scope(|s| {
            for t in 0..5 {
                s.spawn(move |_| {
                    let mut rng = rand::thread_rng();
                    let mut keys: Vec<i32> = (0..1000).map(|k| k * 10 + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert_eq!(
                            i.to_string(),
                            list.remove(&i, &crossbeam_epoch::pin()).unwrap()
                        );
                    }
                });
            }
        })
        .unwrap();

        // get
        thread::scope(|s| {
            for t in 5..10 {
                s.spawn(move |_| {
                    let mut rng = rand::thread_rng();
                    let mut keys: Vec<i32> = (0..1000).map(|k| k * 10 + t).collect();
                    keys.shuffle(&mut rng);
                    for i in keys {
                        assert_eq!(
                            i.to_string(),
                            *list.get(&i, &crossbeam_epoch::pin()).unwrap()
                        );
                    }
                });
            }
        })
        .unwrap();
    }
}
