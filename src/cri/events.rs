//! Container events for `GetContainerEvents`: each call gets a queue of its own, and every
//! event goes to every queue.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::pb;

/// The most events a slow listener may have waiting; older ones are dropped.
const MAX_WAITING: usize = 1024;

#[derive(Default)]
struct Queue {
    events: Mutex<VecDeque<(String, pb::ContainerEventType, i64)>>,
    ready: Condvar,
}

/// The listeners of container events.
#[derive(Default)]
pub struct Events {
    listeners: Mutex<Vec<Arc<Queue>>>,
}

/// One listener's end: it is removed when dropped.
pub struct Listener<'a> {
    events: &'a Events,
    queue: Arc<Queue>,
}

impl Events {
    /// Tell every listener that container `id` was created, started, stopped or deleted.
    pub fn send(&self, id: &str, kind: pb::ContainerEventType) {
        let at = super::records::now_ns();
        let listeners = self.listeners.lock().unwrap_or_else(|e| e.into_inner());
        for q in listeners.iter() {
            let mut events = q.events.lock().unwrap_or_else(|e| e.into_inner());
            if events.len() >= MAX_WAITING {
                events.pop_front();
            }
            events.push_back((id.to_string(), kind, at));
            q.ready.notify_one();
        }
    }

    pub fn listen(&self) -> Listener<'_> {
        let queue = Arc::new(Queue::default());
        self.listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&queue));
        Listener {
            events: self,
            queue,
        }
    }
}

impl Listener<'_> {
    /// The next event, or None after `wait` without one.
    pub fn next(&self, wait: Duration) -> Option<(String, pb::ContainerEventType, i64)> {
        let mut events = self.queue.events.lock().unwrap_or_else(|e| e.into_inner());
        if events.is_empty() {
            events = self
                .queue
                .ready
                .wait_timeout(events, wait)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        events.pop_front()
    }
}

impl Drop for Listener<'_> {
    fn drop(&mut self) {
        let mut listeners = self
            .events
            .listeners
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        listeners.retain(|q| !Arc::ptr_eq(q, &self.queue));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_listener_hears_every_event_in_order() {
        let events = Events::default();
        events.send("before", pb::ContainerEventType::ContainerCreatedEvent);
        let a = events.listen();
        let b = events.listen();
        events.send("c1", pb::ContainerEventType::ContainerCreatedEvent);
        events.send("c1", pb::ContainerEventType::ContainerStartedEvent);
        let wait = Duration::from_millis(10);
        for l in [&a, &b] {
            assert_eq!(
                l.next(wait).unwrap().1,
                pb::ContainerEventType::ContainerCreatedEvent
            );
            let (id, kind, _) = l.next(wait).unwrap();
            assert_eq!(
                (id.as_str(), kind),
                ("c1", pb::ContainerEventType::ContainerStartedEvent)
            );
            assert!(l.next(wait).is_none());
        }
        drop(a);
        assert_eq!(events.listeners.lock().unwrap().len(), 1);
    }
}
