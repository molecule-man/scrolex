// Background worker pool that runs jobs against a poppler Document. Each worker
// keeps its own Document open (Document is not Send), reopening only when the
// requested uri changes. One pool serves every kind of job - visible-page
// renders and low-res previews - so the number of resident Documents equals the
// pool size, independent of how many job kinds exist.

use std::sync::{Arc, Condvar, Mutex};
use std::thread;

// Priority of a queued job. Visible (on-screen full renders) outrank the low-res previews: the
// current page's own blur (VisiblePreview) still comes first so a stand-in appears in ~40ms, but
// the look-ahead previews yield to the sharp renders of pages on screen. Prefetch (full render of
// the next pages in the scroll direction) is nice-to-have and runs last, only once everything on
// screen is done.
#[derive(Clone, Copy)]
pub(crate) enum RenderPriority {
    VisiblePreview,
    Visible,
    Preview,
    Prefetch,
}

impl RenderPriority {
    // Short tag for logging what kind of work a render was.
    pub(crate) fn label(self) -> &'static str {
        match self {
            RenderPriority::VisiblePreview => "low-res (visible)",
            RenderPriority::Visible => "on-demand (visible)",
            RenderPriority::Preview => "low-res (prefetch)",
            RenderPriority::Prefetch => "on-demand (prefetch)",
        }
    }
}

type Job = Box<dyn FnOnce(&poppler::Document) + Send + 'static>;

struct RenderRequest {
    uri: String,
    job: Job,
}

struct RenderQueue {
    // Each is a LIFO stack (newest at the end): when scrolling fast, the page
    // just landed on renders before ones scrolled past. Oldest entries are
    // dropped once a stack is over its cap; a dropped request's job is simply
    // never run (callers treat a dropped job as "reschedule on next draw").
    visible_preview: Vec<RenderRequest>,
    visible: Vec<RenderRequest>,
    preview: Vec<RenderRequest>,
    prefetch: Vec<RenderRequest>,
    max_visible_preview: usize,
    max_visible: usize,
    max_preview: usize,
    max_prefetch: usize,
    // worker count bookkeeping for set_size: live threads and how many should exit next
    live_threads: usize,
    stop_requested: usize,
}

impl RenderQueue {
    fn new(
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
        max_prefetch: usize,
    ) -> Self {
        Self {
            visible_preview: Vec::new(),
            visible: Vec::new(),
            preview: Vec::new(),
            prefetch: Vec::new(),
            max_visible_preview,
            max_visible,
            max_preview,
            max_prefetch,
            live_threads: 0,
            stop_requested: 0,
        }
    }

    fn push(&mut self, priority: RenderPriority, req: RenderRequest) {
        let (stack, max) = match priority {
            RenderPriority::VisiblePreview => (&mut self.visible_preview, self.max_visible_preview),
            RenderPriority::Visible => (&mut self.visible, self.max_visible),
            RenderPriority::Preview => (&mut self.preview, self.max_preview),
            RenderPriority::Prefetch => (&mut self.prefetch, self.max_prefetch),
        };
        stack.push(req);
        while stack.len() > max {
            stack.remove(0);
        }
    }

    // Next runnable request, highest priority first and newest within a tier. All threads pull from
    // here, so whenever visible work exists every thread takes it; prefetch only runs once the
    // higher tiers are drained.
    fn pop(&mut self) -> Option<RenderRequest> {
        self.visible_preview
            .pop()
            .or_else(|| self.visible.pop())
            .or_else(|| self.preview.pop())
            .or_else(|| self.prefetch.pop())
    }
}

// Thread pool serving all background poppler work. Prioritises layout and the visible page over
// previews, and bounds how many requests wait so a fast scroll can't build an unbounded backlog
// ahead of the page being viewed.
pub(crate) struct RenderPool {
    inner: Arc<(Mutex<RenderQueue>, Condvar)>,
}

impl RenderPool {
    pub(crate) fn new(
        pool_size: usize,
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
        max_prefetch: usize,
    ) -> Self {
        let inner = Arc::new((
            Mutex::new(RenderQueue::new(
                max_visible_preview,
                max_visible,
                max_preview,
                max_prefetch,
            )),
            Condvar::new(),
        ));
        let pool = Self { inner };
        pool.set_size(pool_size);
        pool
    }

    // Grow or shrink the worker pool. Growing spawns threads; shrinking asks surplus workers to exit
    // after their current job, dropping their resident poppler Document and freeing its memory.
    pub(crate) fn set_size(&self, n: usize) {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        let plan = plan_resize(queue.live_threads, queue.stop_requested, n);
        queue.live_threads = n;
        queue.stop_requested = plan.stop_requested;
        drop(queue);
        for _ in 0..plan.to_spawn {
            Self::spawn_bg_thread(self.inner.clone());
        }
        if plan.notify {
            cvar.notify_all();
        }
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
                        if queue.stop_requested > 0 {
                            queue.stop_requested -= 1;
                            return; // pool shrank: exit and drop this thread's Document
                        }
                        if let Some(req) = queue.pop() {
                            break req;
                        }
                        queue = cvar.wait(queue).unwrap();
                    }
                };

                if doc.is_none() || doc_uri != req.uri {
                    let f = gtk::gio::File::for_uri(&req.uri);
                    doc = Some(
                        poppler::Document::from_gfile(&f, None, gtk::gio::Cancellable::NONE)
                            .expect("Couldn't open the file!"),
                    );
                    doc_uri.clone_from(&req.uri);
                }

                (req.job)(doc.as_ref().unwrap());
            }
        });
    }
}

struct ResizePlan {
    stop_requested: usize,
    to_spawn: usize,
    notify: bool,
}

// Resize from `live` workers (with `pending_stops` exits not yet honored) to `target`. Growing
// first cancels pending stops, then spawns the rest; shrinking queues more stops.
fn plan_resize(live: usize, pending_stops: usize, target: usize) -> ResizePlan {
    if target > live {
        let revived = pending_stops.min(target - live);
        ResizePlan {
            stop_requested: pending_stops - revived,
            to_spawn: (target - live) - revived,
            notify: false,
        }
    } else {
        ResizePlan {
            stop_requested: pending_stops + (live - target),
            to_spawn: 0,
            notify: target < live,
        }
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
    fn priority_order_and_newest_wins() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.push(RenderPriority::Prefetch, req("pf1"));
        q.push(RenderPriority::Preview, req("pv1"));
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::VisiblePreview, req("vp1"));
        q.push(RenderPriority::Prefetch, req("pf2"));
        q.push(RenderPriority::Preview, req("pv2"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::VisiblePreview, req("vp2"));

        // visible preview, visible full, look-ahead preview, prefetch; newest first
        assert_eq!(
            drain(&mut q),
            vec!["vp2", "vp1", "v2", "v1", "pv2", "pv1", "pf2", "pf1"]
        );
    }

    #[test]
    fn over_cap_drops_oldest() {
        let mut q = RenderQueue::new(2, 2, 2, 2);
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::Visible, req("v3"));

        // v1 (oldest) evicted; newest served first
        assert_eq!(drain(&mut q), vec!["v3", "v2"]);
    }

    #[test]
    fn grow_spawns_missing_threads() {
        let p = plan_resize(2, 0, 5);
        assert_eq!((p.to_spawn, p.stop_requested, p.notify), (3, 0, false));
    }

    #[test]
    fn shrink_queues_stops() {
        let p = plan_resize(5, 0, 2);
        assert_eq!((p.to_spawn, p.stop_requested, p.notify), (0, 3, true));
    }

    // Growing again before pending stops are honored revives them instead of over-spawning.
    #[test]
    fn grow_revives_pending_stops() {
        // was 5, shrunk to 2 (3 stops pending, 2 live), now back to 4
        let p = plan_resize(2, 3, 4);
        assert_eq!((p.to_spawn, p.stop_requested, p.notify), (0, 1, false));
    }

    #[test]
    fn no_change_is_noop() {
        let p = plan_resize(3, 0, 3);
        assert_eq!((p.to_spawn, p.stop_requested, p.notify), (0, 0, false));
    }
}
