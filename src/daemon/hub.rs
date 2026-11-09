//! Pressed-key fan-out for `keys` subscriptions (key tester). Frames carry layout key ids only
//! and are never logged.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Mutex, MutexGuard};

#[derive(Default)]
struct Inner {
    current: Vec<String>,
    evdev: Vec<String>,
    subscribers: Vec<SyncSender<Vec<String>>>,
}

#[derive(Default)]
pub struct KeyHub {
    inner: Mutex<Inner>,
    /// IF2 (software mode) is the active source; evdev forwarding then stays quiet.
    if2: AtomicBool,
    subscribed: AtomicBool,
}

impl KeyHub {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
    /// New subscription; the current frame is queued immediately.
    pub fn subscribe(&self) -> Receiver<Vec<String>> {
        let (tx, rx) = sync_channel(64);
        let mut inner = self.lock();
        let _ = tx.try_send(inner.current.clone());
        inner.subscribers.push(tx);
        self.subscribed.store(true, Ordering::SeqCst);
        rx
    }
    /// Cheap check so sources skip building frames nobody reads.
    #[cfg(test)]
    pub fn has_subscribers(&self) -> bool {
        self.subscribed.load(Ordering::SeqCst)
    }
    pub fn publish(&self, pressed: Vec<String>) {
        let mut inner = self.lock();
        self.publish_locked(&mut inner, pressed);
    }
    fn publish_locked(&self, inner: &mut Inner, pressed: Vec<String>) {
        if inner.current == pressed {
            return;
        }
        inner
            .subscribers
            .retain(|tx| match tx.try_send(pressed.clone()) {
                Ok(()) => true,
                Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
            });
        self.subscribed
            .store(!inner.subscribers.is_empty(), Ordering::SeqCst);
        inner.current = pressed;
    }
    pub fn publish_evdev(&self, pressed: Vec<String>) {
        let mut inner = self.lock();
        inner.evdev = pressed.clone();
        if !self.if2() {
            self.publish_locked(&mut inner, pressed);
        }
    }
    pub fn clear_evdev(&self) {
        let mut inner = self.lock();
        inner.evdev.clear();
        if !self.if2() {
            self.close_locked(&mut inner);
        }
    }
    pub fn set_if2(&self, active: bool) {
        let mut inner = self.lock();
        if self.if2.swap(active, Ordering::SeqCst) != active {
            let evdev = inner.evdev.clone();
            self.close_locked(&mut inner);
            inner.evdev = evdev.clone();
            if !active {
                self.publish_locked(&mut inner, evdev);
            }
        }
    }
    pub fn if2(&self) -> bool {
        self.if2.load(Ordering::SeqCst)
    }
    /// Drop every subscription (device gone or daemon stopping).
    pub fn close(&self) {
        let mut inner = self.lock();
        self.close_locked(&mut inner);
    }
    fn close_locked(&self, inner: &mut Inner) {
        for tx in &inner.subscribers {
            let _ = tx.try_send(Vec::new());
        }
        inner.subscribers.clear();
        inner.current.clear();
        inner.evdev.clear();
        self.subscribed.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscribers_get_current_then_changes_only() {
        let hub = KeyHub::default();
        assert!(!hub.has_subscribers());
        hub.publish(vec!["a".into()]);
        let rx = hub.subscribe();
        assert!(hub.has_subscribers());
        assert_eq!(rx.try_recv().unwrap(), vec!["a".to_string()]);
        hub.publish(vec!["a".into()]);
        assert!(rx.try_recv().is_err(), "unchanged frames are not repeated");
        hub.publish(vec![]);
        assert_eq!(rx.try_recv().unwrap(), Vec::<String>::new());
        drop(rx);
        hub.publish(vec!["b".into()]);
        assert!(
            !hub.has_subscribers(),
            "disconnected subscribers are dropped"
        );
        let rx = hub.subscribe();
        hub.close();
        assert_eq!(rx.try_recv().unwrap(), vec!["b".to_string()]);
        assert_eq!(rx.recv().unwrap(), Vec::<String>::new());
        assert!(rx.recv().is_err());
    }
}

#[cfg(test)]
mod safety_regressions {
    use super::*;
    #[test]
    fn overflow_disconnects_so_final_snapshot_can_be_reseeded() {
        let hub = KeyHub::default();
        let rx = hub.subscribe();
        for i in 0..64 {
            hub.publish(vec![i.to_string()]);
        }
        hub.publish(vec![]);
        while rx.try_recv().is_ok() {}
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(hub.subscribe().try_recv().unwrap(), Vec::<String>::new());
    }
}

#[cfg(test)]
mod source_transition_regressions {
    use super::*;
    #[test]
    fn switching_away_from_if2_reseeds_already_held_evdev_keys() {
        let hub = KeyHub::default();
        hub.set_if2(true);
        hub.publish(vec!["a".into()]);
        hub.publish_evdev(vec!["b".into()]);
        let old = hub.subscribe();
        assert_eq!(old.try_recv().unwrap(), vec!["a".to_string()]);
        hub.set_if2(false);
        assert_eq!(old.recv().unwrap(), Vec::<String>::new());
        assert!(old.recv().is_err());
        assert_eq!(hub.subscribe().try_recv().unwrap(), vec!["b".to_string()]);
    }
}
