// Background worker pool for page rendering. Jobs are self-contained closures (each opens/reuses its
// own MuPDF Document via the renderer's thread-local), so the pool holds no document itself. One
// pool serves every kind of job - visible-page renders and low-res previews.

use std::collections::HashMap;
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
    pub(crate) fn is_preview(self) -> bool {
        matches!(
            self,
            RenderPriority::VisiblePreview | RenderPriority::Preview
        )
    }

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

type Job = Box<dyn FnOnce() + Send + 'static>;

struct RenderRequest {
    uri: String,
    // requesting window (its State id); the wanted-range filter is per-window
    client: u64,
    page: i32,
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
    // Per-window page range worth a full render (keyed by requesting window); full renders outside
    // it drop on pop. Absent window = unrestricted. Previews aren't filtered.
    wanted: HashMap<u64, (i32, i32)>,
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
            wanted: HashMap::new(),
            live_threads: 0,
            stop_requested: 0,
        }
    }

    fn in_wanted(&self, client: u64, page: i32) -> bool {
        self.wanted
            .get(&client)
            .is_none_or(|&(lo, hi)| page >= lo && page <= hi)
    }

    // Drop a window's queued full renders (visible, prefetch), keeping previews (which survive a
    // zoom, rescaled).
    fn clear_full(&mut self, client: u64) {
        self.visible.retain(|req| req.client != client);
        self.prefetch.retain(|req| req.client != client);
    }

    // Drop all of a window's queued renders, previews included (document switch or window close, when
    // nothing of the old view is worth rendering).
    fn clear_all(&mut self, client: u64) {
        self.visible_preview.retain(|req| req.client != client);
        self.visible.retain(|req| req.client != client);
        self.preview.retain(|req| req.client != client);
        self.prefetch.retain(|req| req.client != client);
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

    fn pop(&mut self) -> Option<RenderRequest> {
        if let Some(req) = self.visible_preview.pop() {
            return Some(req);
        }
        while let Some(req) = self.visible.pop() {
            if self.in_wanted(req.client, req.page) {
                return Some(req);
            }
        }
        if let Some(req) = self.preview.pop() {
            return Some(req);
        }
        while let Some(req) = self.prefetch.pop() {
            if self.in_wanted(req.client, req.page) {
                return Some(req);
            }
        }
        None
    }
}

// Thread pool serving all background render work. Prioritises layout and the visible page over
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
    // after their current job, dropping their resident MuPDF Document and freeing its memory.
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

    pub(crate) fn submit(&self, uri: &str, client: u64, page: i32, priority: RenderPriority, job: Job) {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        queue.push(
            priority,
            RenderRequest {
                uri: uri.to_string(),
                client,
                page,
                job,
            },
        );
        cvar.notify_one();
    }

    pub(crate) fn set_wanted(&self, client: u64, range: Option<(i32, i32)>) {
        let (lock, _cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        match range {
            Some(range) => queue.wanted.insert(client, range),
            None => queue.wanted.remove(&client),
        };
    }

    // Drop this window's queued full renders (zoom: previews survive, rescaled).
    pub(crate) fn clear_full(&self, client: u64) {
        let (lock, _cvar) = &*self.inner;
        lock.lock().unwrap().clear_full(client);
    }

    // Drop all of this window's queued renders (document switch / window close).
    pub(crate) fn clear_all(&self, client: u64) {
        let (lock, _cvar) = &*self.inner;
        lock.lock().unwrap().clear_all(client);
    }

    fn spawn_bg_thread(inner: Arc<(Mutex<RenderQueue>, Condvar)>) {
        thread::spawn(move || {
            loop {
                let req = {
                    let (lock, cvar) = &*inner;
                    let mut queue = lock.lock().unwrap();
                    loop {
                        if queue.stop_requested > 0 {
                            queue.stop_requested -= 1;
                            return; // pool shrank: exit and drop this thread's render document
                        }
                        if let Some(req) = queue.pop() {
                            break req;
                        }
                        queue = cvar.wait(queue).unwrap();
                    }
                };

                log::trace!("render job: {}", req.uri);
                (req.job)();
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
            client: 0,
            page: 0,
            job: Box::new(|| {}),
        }
    }

    fn req_cp(client: u64, page: i32) -> RenderRequest {
        RenderRequest {
            uri: String::new(),
            client,
            page,
            job: Box::new(|| {}),
        }
    }

    fn drain(queue: &mut RenderQueue) -> Vec<String> {
        let mut order = Vec::new();
        while let Some(req) = queue.pop() {
            order.push(req.uri);
        }
        order
    }

    fn drain_pages(queue: &mut RenderQueue) -> Vec<i32> {
        let mut order = Vec::new();
        while let Some(req) = queue.pop() {
            order.push(req.page);
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
    fn wanted_range_drops_out_of_range_full_renders() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.wanted.insert(1, (10, 20));
        q.push(RenderPriority::Visible, req_cp(1, 15));
        q.push(RenderPriority::Visible, req_cp(1, 100));
        // the out-of-range page is dropped at pop; only the in-range one is served
        assert_eq!(drain_pages(&mut q), vec![15]);
    }

    #[test]
    fn no_range_means_unrestricted() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.push(RenderPriority::Visible, req_cp(1, 100));
        assert_eq!(drain_pages(&mut q), vec![100]);
    }

    #[test]
    fn previews_ignore_the_range() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.wanted.insert(1, (10, 20));
        q.push(RenderPriority::Preview, req_cp(1, 100));
        q.push(RenderPriority::VisiblePreview, req_cp(1, 200));
        // previews are kept regardless of the range (they mask flung-past pages)
        assert_eq!(drain_pages(&mut q), vec![200, 100]);
    }

    #[test]
    fn range_is_per_window() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.wanted.insert(1, (10, 20));
        q.push(RenderPriority::Visible, req_cp(1, 100)); // window 1, out of range: dropped
        q.push(RenderPriority::Visible, req_cp(2, 50)); // window 2, unrestricted: kept
        assert_eq!(drain_pages(&mut q), vec![50]);
    }

    #[test]
    fn clear_full_drops_full_renders_keeps_previews() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.push(RenderPriority::Visible, req_cp(1, 1));
        q.push(RenderPriority::Prefetch, req_cp(1, 2));
        q.push(RenderPriority::Preview, req_cp(1, 3));
        q.clear_full(1);
        // full renders gone; the preview survives (zoom rescales it)
        assert_eq!(drain_pages(&mut q), vec![3]);
    }

    #[test]
    fn clear_all_drops_everything_for_the_client() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.push(RenderPriority::Visible, req_cp(1, 1));
        q.push(RenderPriority::Preview, req_cp(1, 2));
        q.push(RenderPriority::VisiblePreview, req_cp(1, 3));
        q.push(RenderPriority::Visible, req_cp(2, 4)); // another window: untouched
        q.clear_all(1);
        assert_eq!(drain_pages(&mut q), vec![4]);
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
