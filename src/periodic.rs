use std::{cmp::Reverse, collections::BinaryHeap, ops::{Deref, DerefMut}, time::Instant};


pub(crate) type MinInstantHeap<T> = BinaryHeap<MinInstantEntry<T>>;

#[derive(Debug)]
pub(crate) struct MinInstantEntry<T> {
    pub(crate) timestamp: Instant,
    pub(crate) task: T,
}

impl<T> PartialEq for MinInstantEntry<T> {
    fn eq(&self, other: &Self) -> bool {
        self.timestamp == other.timestamp
    }
}

impl<T> Eq for MinInstantEntry<T> {}

impl<T> PartialOrd for MinInstantEntry<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Reverse(self.timestamp).partial_cmp(&Reverse(other.timestamp))
    }
}

impl<T> Ord for MinInstantEntry<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

impl<T> Deref for MinInstantEntry<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.task
    }
}

impl<T> DerefMut for MinInstantEntry<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.task
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    
    #[test]
    fn test_order() {
        let mut heap = MinInstantHeap::new();
        let now = Instant::now();
        heap.push(MinInstantEntry {
            timestamp: now + Duration::from_secs(10),
            task: "task1",
        });
        heap.push(MinInstantEntry {
            timestamp: now + Duration::from_secs(20),
            task: "task2",
        });
        let rst = heap.peek().unwrap().deref();
        assert_eq!(*rst, "task1");
    }
}