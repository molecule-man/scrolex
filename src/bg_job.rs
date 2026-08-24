// Background worker pool for page rendering. Jobs are self-contained closures (each opens/reuses its
// own MuPDF Document via the renderer's thread-local), so the pool holds no document itself. One
// pool serves every kind of job - visible-page renders and low-res previews.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

// Priority of a queued job. Visible (on-screen full renders) outrank the low-res previews: the
// current page's own blur (VisiblePreview) still comes first. Full pages ahead run before distant
// previews, so nearby pages stay sharp during fast input.
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

#[derive(Clone, Default, Debug)]
pub(crate) struct RenderDemand {
    clients: Arc<Mutex<HashSet<u64>>>,
}

impl RenderDemand {
    pub(crate) fn from_client(client: u64) -> Self {
        Self::from_clients([client])
    }

    pub(crate) fn from_clients(clients: impl IntoIterator<Item = u64>) -> Self {
        Self {
            clients: Arc::new(Mutex::new(clients.into_iter().collect())),
        }
    }

    pub(crate) fn add(&self, client: u64) {
        self.clients.lock().unwrap().insert(client);
    }

    pub(crate) fn remove(&self, client: u64) {
        self.clients.lock().unwrap().remove(&client);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.clients.lock().unwrap().is_empty()
    }

    pub(crate) fn clear(&self) {
        self.clients.lock().unwrap().clear();
    }

    pub(crate) fn same_request(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.clients, &other.clients)
    }

    fn any(&self, predicate: impl Fn(u64) -> bool) -> bool {
        self.clients.lock().unwrap().iter().copied().any(predicate)
    }

    #[cfg(test)]
    fn clients(&self) -> Vec<u64> {
        let mut clients: Vec<_> = self.clients.lock().unwrap().iter().copied().collect();
        clients.sort_unstable();
        clients
    }
}

struct RenderRequest {
    uri: String,
    document: u64,
    demand: RenderDemand,
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
    // Each viewport has one page range. Full renders outside every interested range drop on pop.
    wanted: HashMap<u64, (i32, i32)>,
    wanted_documents: HashMap<u64, u64>,
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
            wanted_documents: HashMap::new(),
            live_threads: 0,
            stop_requested: 0,
        }
    }

    fn in_wanted(&self, request: &RenderRequest) -> bool {
        request.demand.any(|client| {
            self.wanted
                .get(&client)
                .is_none_or(|&(lo, hi)| request.page >= lo && request.page <= hi)
        })
    }

    // Remove one viewport from queued full renders. Keep work that another viewport needs.
    fn clear_full(&mut self, client: u64) {
        for request in self.visible.iter().chain(&self.prefetch) {
            request.demand.remove(client);
        }
        self.visible.retain(|request| !request.demand.is_empty());
        self.prefetch.retain(|request| !request.demand.is_empty());
    }

    fn clear_all_document(&mut self, document: u64) {
        self.visible_preview
            .retain(|request| request.document != document);
        self.visible.retain(|request| request.document != document);
        self.preview.retain(|request| request.document != document);
        self.prefetch.retain(|request| request.document != document);
        let clients: Vec<_> = self
            .wanted_documents
            .iter()
            .filter_map(|(&client, &owner)| (owner == document).then_some(client))
            .collect();
        for client in clients {
            self.wanted.remove(&client);
            self.wanted_documents.remove(&client);
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

    fn pop(&mut self) -> Option<RenderRequest> {
        if let Some(req) = self.visible_preview.pop() {
            return Some(req);
        }
        while let Some(req) = self.visible.pop() {
            if self.in_wanted(&req) {
                return Some(req);
            }
        }
        while let Some(req) = self.prefetch.pop() {
            if self.in_wanted(&req) {
                return Some(req);
            }
        }
        if let Some(req) = self.preview.pop() {
            return Some(req);
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

    pub(crate) fn submit(
        &self,
        uri: &str,
        document: u64,
        demand: RenderDemand,
        page: i32,
        priority: RenderPriority,
        job: Job,
    ) {
        let (lock, cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        queue.push(
            priority,
            RenderRequest {
                uri: uri.to_string(),
                document,
                demand,
                page,
                job,
            },
        );
        cvar.notify_one();
    }

    pub(crate) fn set_wanted(&self, document: u64, client: u64, range: Option<(i32, i32)>) {
        let (lock, _cvar) = &*self.inner;
        let mut queue = lock.lock().unwrap();
        match range {
            Some(range) => {
                queue.wanted_documents.insert(client, document);
                queue.wanted.insert(client, range)
            }
            None => {
                queue.wanted_documents.remove(&client);
                queue.wanted.remove(&client)
            }
        };
    }

    // Remove one viewport from queued full renders. Previews survive zoom.
    pub(crate) fn clear_full(&self, client: u64) {
        let (lock, _cvar) = &*self.inner;
        lock.lock().unwrap().clear_full(client);
    }

    // Drop queued renders and wanted ranges for one document.
    pub(crate) fn clear_all_document(&self, document: u64) {
        let (lock, _cvar) = &*self.inner;
        lock.lock().unwrap().clear_all_document(document);
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
            document: 0,
            demand: RenderDemand::from_client(0),
            page: 0,
            job: Box::new(|| {}),
        }
    }

    fn req_cp(client: u64, page: i32) -> RenderRequest {
        RenderRequest {
            uri: String::new(),
            document: 0,
            demand: RenderDemand::from_client(client),
            page,
            job: Box::new(|| {}),
        }
    }

    fn req_with_demand(document: u64, demand: RenderDemand, page: i32) -> RenderRequest {
        RenderRequest {
            uri: String::new(),
            document,
            demand,
            page,
            job: Box::new(|| {}),
        }
    }

    #[test]
    fn a_shared_request_survives_one_viewport_clear() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        let demand = RenderDemand::from_clients([1, 2]);
        q.wanted.insert(1, (1, 2));
        q.wanted.insert(2, (10, 20));
        q.push(
            RenderPriority::Visible,
            req_with_demand(7, demand.clone(), 15),
        );

        q.clear_full(1);

        assert_eq!(drain_pages(&mut q), vec![15]);
        assert_eq!(demand.clients(), vec![2]);
    }

    #[test]
    fn a_document_clear_removes_each_viewport_request() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.push(
            RenderPriority::Visible,
            req_with_demand(7, RenderDemand::from_clients([1]), 3),
        );
        q.push(
            RenderPriority::Visible,
            req_with_demand(8, RenderDemand::from_clients([2]), 4),
        );

        q.clear_all_document(7);

        assert_eq!(drain_pages(&mut q), vec![4]);
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

        // Visible work comes first. Full nearby pages precede distant previews.
        assert_eq!(
            drain(&mut q),
            vec!["vp2", "vp1", "v2", "v1", "pf2", "pf1", "pv2", "pv1"]
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
    fn range_is_per_viewport() {
        let mut q = RenderQueue::new(4, 4, 4, 4);
        q.wanted.insert(1, (10, 20));
        q.push(RenderPriority::Visible, req_cp(1, 100)); // viewport 1: dropped
        q.push(RenderPriority::Visible, req_cp(2, 50)); // viewport 2: kept
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
