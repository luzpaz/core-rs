//! Carrier is an in-memory messaging system that exposes both C and Rust
//! interfaces with the goal of connecting two apps such that a) one app is
//! embedded inside of another (such as a rust app inside of a java app) or
//! b) two apps are embedded inside of a third and they need to communicate.
//!
//! Carrier uses global messaging channels, so anyone that can call out to C
//! within an app can send and receive messages to other parts of the app.
//!
//! You could think of Carrier as a poor-(wo)man's nanomsg, however there are
//! some key differences:
//!
//!   1. Carrier is in-memory only (so, inproc://).
//!   2. Carrier only sends a message to one recipient. In other words, if your
//!      app simultaneously sends on and is listening to a channel, there's a
//!      chance that your app will dequeue and consume the message before the
//!      remote gets it. For this reason, you may want to set up an "incoming"
//!      channel that you listen to, and a separate "outgoing" channel the
//!      remote listens to (and, conversely, the remove would listen to your
//!      outgoing and send to your incoming).
//!   3. Channels do not need to be bound/connected before use. By either doing
//!      `send()` or `recv()` on a channel, it is created and can start being
//!      used. Once a channel has no messages on it and also has no listeners,
//!      it is recycled (removed entirely). This allows you to very cheaply make
//!      and use new channels that clean themselves up when finished.

extern crate crossbeam;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate quick_error;

mod error;
pub mod c;

use ::std::sync::{Arc, RwLock};
use ::std::collections::HashMap;

use ::crossbeam::sync::MsQueue;

pub use ::error::CError;
use ::error::CResult;

lazy_static! {
    static ref CONN: Carrier = Carrier::new().unwrap();
}

/// The carrier Queue is a quick and simple wrapper around MsQueue that keeps
/// track of a bit more state than MsQueue does.
struct Queue<T> {
    internal: MsQueue<T>,
    messages: RwLock<i32>,
    users: RwLock<i32>,
}

impl<T> Queue<T> {
    /// Create a new carrier queue.
    fn new() -> Queue<T> {
        Queue {
            internal: MsQueue::new(),
            messages: RwLock::new(0),
            users: RwLock::new(0),
        }
    }

    /// Increment the number of messages this queue has by a certain amount (1).
    fn inc_messages(&self, val: i32) {
        let mut mguard = self.messages.write().unwrap();
        (*mguard) += val;
    }

    /// Increment the number of users this queue has by a certain amount (1).
    fn inc_users(&self, val: i32) {
        let mut uguard = self.users.write().unwrap();
        (*uguard) += val;
    }

    /// Get how many messages this queue currently has listening to it.
    fn num_messages(&self) -> i32 {
        let mguard = self.messages.read().unwrap();
        (*mguard).clone()
    }

    /// Get how many users this queue currently has listening to it.
    fn num_users(&self) -> i32 {
        let uguard = self.users.read().unwrap();
        (*uguard).clone()
    }

    /// MsQueue.push()
    fn push(&self, val: T) {
        self.internal.push(val);
        self.inc_messages(1);
    }

    /// MsQueue.try_pop()
    fn try_pop(&self) -> Option<T> {
        let res = self.internal.try_pop();
        if res.is_some() {
            self.inc_messages(-1);
        } else {
            *(self.messages.write().unwrap()) = 0;
        }
        res
    }

    /// MsQueue.pop()
    fn pop(&self) -> T {
        self.inc_users(1);
        let res = self.internal.pop();
        self.inc_users(-1);
        self.inc_messages(-1);
        res
    }

    /// Determine if this queue has been "abandoned" ...meaning it has no
    /// messages in it and there is nobody listening to it.
    fn is_abandoned(&self) -> bool {
        if self.num_messages() <= 0 && self.num_users() <= 0 {
            true
        } else {
            false
        }
    }
}

pub struct Carrier {
    queues: RwLock<HashMap<String, Arc<Queue<Vec<u8>>>>>,
}

//unsafe impl Send for Carrier {}
//unsafe impl Sync for Carrier {}

impl Carrier {
    /// Create a new carrier
    pub fn new() -> CResult<Carrier> {
        Ok(Carrier {
            queues: RwLock::new(HashMap::new()),
        })
    }

    /// Ensure a channel exists
    fn ensure(&self, channel: &String) -> Arc<Queue<Vec<u8>>> {
        let mut guard = self.queues.write().unwrap();
        if (*guard).contains_key(channel) {
            (*guard).get(channel).unwrap().clone()
        } else {
            let queue = Arc::new(Queue::new());
            (*guard).insert(channel.clone(), queue.clone());
            queue
        }
    }

    fn exists(&self, channel: &String) -> bool {
        let guard = self.queues.read().unwrap();
        (*guard).contains_key(channel)
    }

    /// Remove a channel
    fn remove(&self, channel: &String) {
        let mut guard = self.queues.write().unwrap();
        (*guard).remove(channel);
    }

    fn wipe(&self) {
        let mut guard = self.queues.write().unwrap();
        guard.clear();
    }
}

/// Send a message on a channel
pub fn send(channel: &str, message: Vec<u8>) -> CResult<()> {
    let queue = (*CONN).ensure(&String::from(channel));
    queue.push(message);
    Ok(())
}

/// Send a message on a channel
pub fn send_string(channel: &str, message: String) -> CResult<()> {
    let vec = Vec::from(message.as_bytes());
    send(channel, vec)
}

/// Blocking receive
pub fn recv(channel: &str) -> CResult<Vec<u8>> {
    let queue = (*CONN).ensure(&String::from(channel));
    let res = Ok(queue.pop());
    if queue.is_abandoned() { (*CONN).remove(&String::from(channel)); }
    res
}

/// Non-blocking receive
pub fn recv_nb(channel: &str) -> CResult<Option<Vec<u8>>> {
    let channel = String::from(channel);
    if !(*CONN).exists(&channel) {
        return Ok(None)
    }
    let queue = (*CONN).ensure(&channel);
    let res = Ok(queue.try_pop());
    if queue.is_abandoned() { (*CONN).remove(&channel); }
    res
}

/// Wipe out all queues
pub fn wipe() {
    (*CONN).wipe();
}

#[cfg(test)]
mod tests {
    use ::std::thread;

    use super::*;

    #[test]
    fn send_recv_simple() {
        send("sendrecv", Vec::from(String::from("this is a test").as_bytes())).unwrap();
        send_string("sendrecv", String::from("this is another test")).unwrap();

        let next = String::from_utf8(recv_nb("sendrecv").unwrap().unwrap()).unwrap();
        assert_eq!(next, "this is a test");
        let next = String::from_utf8(recv_nb("sendrecv").unwrap().unwrap()).unwrap();
        assert_eq!(next, "this is another test");
        let next = recv_nb("sendrecv").unwrap();
        assert_eq!(next, None);
        let next = recv_nb("sendrecv").unwrap();
        assert_eq!(next, None);
        let next = recv_nb("nope").unwrap();
        assert_eq!(next, None);
    }

    #[test]
    fn recv_blocking() {
        let handle = thread::spawn(move || {
            send_string("core", String::from("hello, there")).unwrap();
        });
        let msg = String::from_utf8(recv("core").unwrap()).unwrap();
        assert_eq!(msg, "hello, there");
        handle.join().unwrap();
    }

    // Would love to test wiping, but running in multi-thread mode screws up the
    // other tests, so for now it's disabled.
    /*
    #[test]
    fn wiping() {
        send_string("wiper", String::from("this is another test")).unwrap();
        send_string("wiper", String::from("yoohoo")).unwrap();
        wipe();
        assert_eq!(recv_nb("wiper").unwrap(), None);
    }
    */
}
