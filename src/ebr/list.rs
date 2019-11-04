use super::concurrent_map::ConcurrentMap;
use crossbeam_ebr::{unprotected, Atomic, Guard, Owned, Shared};

use std::cmp::Ordering::{Equal, Greater, Less};
use std::mem::ManuallyDrop;
use std::ptr;
use std::sync::atomic::Ordering;

#[derive(Debug)]
struct Node<K, V> {
    key: K,
    value: ManuallyDrop<V>,
    /// Mark: tag(), Tag: not needed
    next: Atomic<Node<K, V>>,
}

struct List<K, V> {
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
                let next = curr_ref.next.load(Ordering::Relaxed, unprotected());
                if next.tag() == 0 {
                    ManuallyDrop::drop(&mut curr_ref.value);
                }
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

    #[inline]
    fn harris_find_inner<'g>(
        &'g self,
        key: &K,
        guard: &'g Guard,
    ) -> Result<(bool, Cursor<'g, K, V>), ()> {
        let mut cursor = Cursor {
            prev: &self.head,
            curr: self.head.load(Ordering::Acquire, guard),
        };

        // Finding phase
        // - cursor.curr: first unmarked node w/ key >= search key (4)
        // - cursor.prev: the ref of .next in previous unmarked node (1 -> 2)
        // 1 -> 2 -x-> 3 -x-> 4 -> 5 -> ∅  (search key: 4)
        let mut prev_next = cursor.curr;
        let found = loop {
            let curr_node = match unsafe { cursor.curr.as_ref() } {
                None => return Ok((false, cursor)),
                Some(c) => c,
            };

            let mut next = curr_node.next.load(Ordering::Acquire, guard);

            // - finding stage is done if cursor.curr advancement stops
            // - advance cursor.curr if (.next is marked) || (cursor.curr < key)
            // - stop cursor.curr if (not marked) && (cursor.curr >= key)
            // - advance cursor.prev if not marked
            match (curr_node.key.cmp(key), next.tag()) {
                (Less, tag) => {
                    cursor.curr = next.with_tag(0);
                    if tag == 0 {
                        cursor.prev = &curr_node.next;
                        prev_next = next;
                    }
                }
                (eq, 0) => {
                    next = curr_node.next.load(Ordering::Relaxed, guard);
                    // TODO: why re-check? probably not needed
                    if next.tag() == 0 {
                        break eq == Equal;
                    } else {
                        return Err(());
                    }
                }
                (_, _) => cursor.curr = next.with_tag(0),
            }
        };

        // If prev and curr WERE adjacent, no need to clean up
        if prev_next == cursor.curr {
            return Ok((found, cursor));
        }

        // cleanup marked nodes between prev and curr
        if cursor
            .prev
            .compare_and_set(prev_next, cursor.curr, Ordering::AcqRel, guard)
            .is_err()
        {
            return Err(());
        }

        // defer_destroy from cursor.prev.load() to cursor.curr (exclusive)
        let mut node = prev_next;
        loop {
            let node_ref = unsafe { node.as_ref().unwrap() };
            let next = node_ref.next.load(Ordering::Relaxed, guard);
            if node.with_tag(0) == cursor.curr {
                return Ok((found, cursor));
            }
            unsafe {
                guard.defer_destroy(node);
            }
            node = next;
        }
    }

    fn harris_find<'g>(&'g self, key: &K, guard: &'g Guard) -> (bool, Cursor<'g, K, V>) {
        loop {
            if let Ok(r) = self.harris_find_inner(key, guard) {
                return r;
            }
        }
    }

    pub fn harris_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        let (found, cursor) = self.harris_find(key, guard);

        if found {
            unsafe { cursor.curr.as_ref().map(|n| &*n.value) }
        } else {
            None
        }
    }

    pub fn harris_insert(&self, key: K, value: V, guard: &Guard) -> bool {
        let mut node = Owned::new(Node {
            key,
            value: ManuallyDrop::new(value),
            next: Atomic::null(),
        });

        loop {
            let (found, cursor) = self.harris_find(&node.key, &guard);
            if found {
                unsafe {
                    ManuallyDrop::drop(&mut node.value);
                }
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

    pub fn harris_remove(&self, key: &K, guard: &Guard) -> Option<V> {
        loop {
            let (found, cursor) = self.harris_find(key, &guard);
            if !found {
                return None;
            }

            let curr_node = unsafe { cursor.curr.as_ref() }.unwrap();
            let value = unsafe { ptr::read(&curr_node.value) };

            let next = curr_node.next.fetch_or(1, Ordering::AcqRel, &guard);
            if next.tag() == 1 {
                continue;
            }

            if cursor
                .prev
                .compare_and_set(cursor.curr, next, Ordering::AcqRel, &guard)
                .is_ok()
            {
                unsafe { guard.defer_destroy(cursor.curr) };
            }

            return Some(ManuallyDrop::into_inner(value));
        }
    }

    /// Returns (1) whether it found an entry, and (2) a cursor.
    #[inline]
    fn harris_michael_find_inner<'g>(
        &'g self,
        key: &K,
        guard: &'g Guard,
    ) -> Result<(bool, Cursor<'g, K, V>), ()> {
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

            let mut next = curr_node.next.load(Ordering::Acquire, guard);

            if next.tag() == 0 {
                match curr_node.key.cmp(key) {
                    Less => cursor.prev = &curr_node.next,
                    Equal => return Ok((true, cursor)),
                    Greater => return Ok((false, cursor)),
                }
            } else {
                next = next.with_tag(0);
                match cursor
                    .prev
                    .compare_and_set(cursor.curr, next, Ordering::AcqRel, guard)
                {
                    Err(_) => return Err(()),
                    Ok(_) => unsafe { guard.defer_destroy(cursor.curr) },
                }
            }
            cursor.curr = next;
        }
    }

    fn harris_michael_find<'g>(&'g self, key: &K, guard: &'g Guard) -> (bool, Cursor<'g, K, V>) {
        loop {
            if let Ok(r) = self.harris_michael_find_inner(key, guard) {
                return r;
            }
        }
    }

    pub fn harris_michael_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        let (found, cursor) = self.harris_michael_find(key, guard);

        if found {
            unsafe { cursor.curr.as_ref().map(|n| &*n.value) }
        } else {
            None
        }
    }

    pub fn harris_michael_insert(&self, key: K, value: V, guard: &Guard) -> bool {
        let mut node = Owned::new(Node {
            key,
            value: ManuallyDrop::new(value),
            next: Atomic::null(),
        });

        loop {
            let (found, cursor) = self.harris_michael_find(&node.key, &guard);
            if found {
                unsafe {
                    ManuallyDrop::drop(&mut node.value);
                }
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

    pub fn harris_michael_remove(&self, key: &K, guard: &Guard) -> Option<V> {
        loop {
            let (found, cursor) = self.harris_michael_find(key, &guard);
            if !found {
                return None;
            }

            let curr_node = unsafe { cursor.curr.as_ref() }.unwrap();
            let value = unsafe { ptr::read(&curr_node.value) };

            let next = curr_node.next.fetch_or(1, Ordering::AcqRel, &guard);
            if next.tag() == 1 {
                continue;
            }

            if cursor
                .prev
                .compare_and_set(cursor.curr, next, Ordering::AcqRel, &guard)
                .is_ok()
            {
                unsafe { guard.defer_destroy(cursor.curr) };
            }

            return Some(ManuallyDrop::into_inner(value));
        }
    }

    pub fn harris_herlihy_shavit_get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        let mut curr = self.head.load(Ordering::Acquire, guard);
        loop {
            let curr_node = match unsafe { curr.as_ref() } {
                None => return None,
                Some(c) => c,
            };
            if curr_node.key < *key {
                curr = curr_node.next.load(Ordering::Acquire, guard);
                continue;
            }
            match (
                curr_node.key.cmp(key),
                curr_node.next.load(Ordering::Relaxed, guard).tag(),
            ) {
                (Less, _) => continue,
                (Equal, 0) => return Some(&*curr_node.value),
                _ => return None,
            }
        }
    }
}

pub struct HList<K, V> {
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_insert(key, value, guard)
    }
    #[inline]
    fn remove(&self, key: &K, guard: &Guard) -> Option<V> {
        self.inner.harris_remove(key, guard)
    }
}

pub struct HMList<K, V> {
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HMList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HMList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_michael_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_michael_insert(key, value, guard)
    }
    #[inline]
    fn remove(&self, key: &K, guard: &Guard) -> Option<V> {
        self.inner.harris_michael_remove(key, guard)
    }
}

pub struct HHSList<K, V> {
    inner: List<K, V>,
}

impl<K, V> ConcurrentMap<K, V> for HHSList<K, V>
where
    K: Ord,
{
    fn new() -> Self {
        HHSList { inner: List::new() }
    }

    #[inline]
    fn get<'g>(&'g self, key: &K, guard: &'g Guard) -> Option<&'g V> {
        self.inner.harris_herlihy_shavit_get(key, guard)
    }
    #[inline]
    fn insert(&self, key: K, value: V, guard: &Guard) -> bool {
        self.inner.harris_michael_insert(key, value, guard)
    }
    #[inline]
    fn remove(&self, key: &K, guard: &Guard) -> Option<V> {
        self.inner.harris_michael_remove(key, guard)
    }
}

#[cfg(test)]
mod tests {
    use super::{HHSList, HList, HMList};
    use crate::ebr::concurrent_map;

    #[test]
    fn smoke_h_list() {
        concurrent_map::tests::smoke::<HList<i32, String>>();
    }

    #[test]
    fn smoke_hm_list() {
        concurrent_map::tests::smoke::<HMList<i32, String>>();
    }

    #[test]
    fn smoke_hhs_list() {
        concurrent_map::tests::smoke::<HHSList<i32, String>>();
    }
}
