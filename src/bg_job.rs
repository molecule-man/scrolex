// Background worker pool that runs jobs against a poppler Document. Each worker
// keeps its own Document open (Document is not Send), reopening only when the
// requested uri changes.

use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use once_cell::sync::Lazy;
use poppler::Document;

// Pool used for bbox computation (cheap, latency-sensitive layout work).
const BBOX_POOL_SIZE: usize = 1;

thread_local!(
    pub(crate) static JOB_MANAGER: Lazy<JobManager> = Lazy::new(|| JobManager::new(BBOX_POOL_SIZE));
);

type Job = Box<dyn FnOnce(&Document) + Send + 'static>;

struct Request {
    uri: String,
    job: Job,
}

pub(crate) struct JobManager {
    send: std::sync::mpsc::Sender<Request>,
}

impl JobManager {
    pub(crate) fn new(pool_size: usize) -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let recv = Arc::new(Mutex::new(recv));
        let manager = Self { send };
        for _ in 0..pool_size {
            Self::spawn_bg_thread(recv.clone());
        }
        manager
    }

    pub(crate) fn execute(&self, uri: &str, job: Job) {
        self.send
            .send(Request {
                uri: uri.to_string(),
                job,
            })
            .expect("Failed to send job request");
    }

    fn spawn_bg_thread(recv: Arc<Mutex<Receiver<Request>>>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            loop {
                let req = recv.lock().unwrap().recv().unwrap();
                if doc.is_none() || doc_uri != req.uri {
                    let f = gtk::gio::File::for_uri(&req.uri);
                    doc = Some(
                        Document::from_gfile(&f, None, gtk::gio::Cancellable::NONE)
                            .expect("Couldn't open the file!"),
                    );
                    doc_uri.clone_from(&req.uri);
                }
                let doc = doc.as_ref().unwrap();

                (req.job)(doc);
            }
        });
    }
}

pub(crate) fn execute(uri: &str, job: Job) {
    JOB_MANAGER.with(|manager| manager.execute(uri, job));
}

// Priority of a render request. Visible pages always render before prefetched
// ones.
#[derive(Clone, Copy)]
pub(crate) enum RenderPriority {
    Visible,
    Prefetch,
}

struct RenderRequest {
    uri: String,
    job: Job,
}

struct RenderQueue {
    // Both are LIFO stacks (newest at the end): when scrolling fast, the page
    // just landed on renders before ones scrolled past. Oldest entries are
    // dropped once a stack is over its cap; a dropped request's job is simply
    // never run.
    visible: Vec<RenderRequest>,
    prefetch: Vec<RenderRequest>,
    max_visible: usize,
    max_prefetch: usize,
}

impl RenderQueue {
    fn new(max_visible: usize, max_prefetch: usize) -> Self {
        Self {
            visible: Vec::new(),
            prefetch: Vec::new(),
            max_visible,
            max_prefetch,
        }
    }

    fn push(&mut self, priority: RenderPriority, req: RenderRequest) {
        let (stack, max) = match priority {
            RenderPriority::Visible => (&mut self.visible, self.max_visible),
            RenderPriority::Prefetch => (&mut self.prefetch, self.max_prefetch),
        };
        stack.push(req);
        while stack.len() > max {
            stack.remove(0);
        }
    }

    // Visible pages take priority; within each stack the newest wins.
    fn pop(&mut self) -> Option<RenderRequest> {
        if let Some(req) = self.visible.pop() {
            Some(req)
        } else {
            self.prefetch.pop()
        }
    }
}

// Thread pool for page rendering. Prioritises the visible page over prefetch
// and bounds how many requests wait, so a fast scroll can't build an unbounded
// backlog ahead of the page being viewed.
pub(crate) struct RenderPool {
    inner: Arc<(Mutex<RenderQueue>, Condvar)>,
}

impl RenderPool {
    pub(crate) fn new(pool_size: usize, max_visible: usize, max_prefetch: usize) -> Self {
        let inner = Arc::new((
            Mutex::new(RenderQueue::new(max_visible, max_prefetch)),
            Condvar::new(),
        ));
        for _ in 0..pool_size {
            Self::spawn_bg_thread(inner.clone());
        }
        Self { inner }
    }

    pub(crate) fn submit(&self, uri: &str, priority: RenderPriority, job: Job) {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        queue.push(
            priority,
            RenderRequest {
                uri: uri.to_string(),
                job,
            },
        );
        cvar.notify_one();
    }

    fn spawn_bg_thread(inner: Arc<(Mutex<RenderQueue>, Condvar)>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            loop {
                let req = {
                    let (lock, cvar) = &*inner;
                    let mut queue = lock.lock().unwrap();
                    loop {
                        if let Some(req) = queue.pop() {
                            break req;
                        }
                        queue = cvar.wait(queue).unwrap();
                    }
                };

                if doc.is_none() || doc_uri != req.uri {
                    let f = gtk::gio::File::for_uri(&req.uri);
                    doc = Some(
                        Document::from_gfile(&f, None, gtk::gio::Cancellable::NONE)
                            .expect("Couldn't open the file!"),
                    );
                    doc_uri.clone_from(&req.uri);
                }

                (req.job)(doc.as_ref().unwrap());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The uri field doubles as an identity tag here; the job is never run.
    fn req(tag: &str) -> RenderRequest {
        RenderRequest {
            uri: tag.to_string(),
            job: Box::new(|_| {}),
        }
    }

    fn drain(queue: &mut RenderQueue) -> Vec<String> {
        let mut order = Vec::new();
        while let Some(req) = queue.pop() {
            order.push(req.uri);
        }
        order
    }

    #[test]
    fn visible_beats_prefetch_and_newest_wins() {
        let mut q = RenderQueue::new(4, 4);
        q.push(RenderPriority::Prefetch, req("p1"));
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::Prefetch, req("p2"));
        q.push(RenderPriority::Visible, req("v2"));

        // both visible pages first (newest first), then prefetch (newest first)
        assert_eq!(drain(&mut q), vec!["v2", "v1", "p2", "p1"]);
    }

    #[test]
    fn over_cap_drops_oldest() {
        let mut q = RenderQueue::new(2, 2);
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::Visible, req("v3"));

        // v1 (oldest) evicted; newest served first
        assert_eq!(drain(&mut q), vec!["v3", "v2"]);
    }
}
