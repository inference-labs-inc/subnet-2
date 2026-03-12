use std::borrow::Borrow;
use std::collections::{HashSet, VecDeque};
use std::hash::Hash;

pub struct BoundedFifoSet<T: Eq + Hash + Clone> {
    set: HashSet<T>,
    order: VecDeque<T>,
    capacity: usize,
}

impl<T: Eq + Hash + Clone> BoundedFifoSet<T> {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            set: HashSet::with_capacity(cap),
            order: VecDeque::with_capacity(cap),
            capacity: cap,
        }
    }

    pub fn contains<Q>(&self, item: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.set.contains(item)
    }

    pub fn insert(&mut self, item: T) -> bool {
        if !self.set.insert(item.clone()) {
            return false;
        }
        self.order.push_back(item);
        while self.set.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.set.remove(&oldest);
            } else {
                break;
            }
        }
        true
    }

    pub fn remove<Q>(&mut self, item: &Q) -> bool
    where
        T: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        if self.set.remove(item) {
            if let Some(pos) = self.order.iter().position(|i| i.borrow() == item) {
                self.order.remove(pos);
            }
            true
        } else {
            false
        }
    }

    pub fn clear(&mut self) {
        self.set.clear();
        self.order.clear();
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_contains() {
        let mut s = BoundedFifoSet::new(4);
        assert!(s.insert("a".to_string()));
        assert!(s.contains("a"));
        assert!(!s.insert("a".to_string()));
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut s = BoundedFifoSet::new(2);
        s.insert("a".to_string());
        s.insert("b".to_string());
        s.insert("c".to_string());
        assert!(!s.contains("a"));
        assert!(s.contains("b"));
        assert!(s.contains("c"));
    }

    #[test]
    fn remove_allows_reinsert() {
        let mut s = BoundedFifoSet::new(4);
        s.insert("a".to_string());
        assert!(s.remove("a"));
        assert!(!s.contains("a"));
        assert!(s.insert("a".to_string()));
    }

    #[test]
    fn clear_empties() {
        let mut s = BoundedFifoSet::new(4);
        s.insert("a".to_string());
        s.clear();
        assert!(s.is_empty());
    }
}
