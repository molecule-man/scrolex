// Background worker pool that runs jobs against a poppler Document. Each worker
// keeps its own Document open (Document is not Send), reopening only when the
// requested uri changes. One pool serves every kind of job - bbox/layout,
// visible-page renders, low-res previews and prefetched renders - so the number
// of resident Documents equals the pool size, independent of how many job kinds
// exist.

use std::sync::{Arc, Condvar, Mutex};
use std::thread;

// Priority of a queued job.
#[derive(Clone, Copy)]
pub(crate) enum RenderPriority {
    Bbox,
    VisiblePreview,
    Visible,
    Preview,
    Prefetch,
}

impl RenderPriority {
    // Short tag for logging what kind of work a render was.
    pub(crate) fn label(self) -> &'static str {
        match self {
            RenderPriority::Bbox => "bbox",
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
    bbox: Vec<RenderRequest>,
    visible_preview: Vec<RenderRequest>,
    visible: Vec<RenderRequest>,
    preview: Vec<RenderRequest>,
    prefetch: Vec<RenderRequest>,
    max_bbox: usize,
    max_visible_preview: usize,
    max_visible: usize,
    max_preview: usize,
    max_prefetch: usize,
    // Prefetch full renders currently running, and the cap on that. Poppler renders can't be
    // interrupted once started, so we keep at least one worker off prefetch: a fresh on-demand
    // (visible) render then always finds a worker free to start it, instead of waiting behind a
    // slow prefetch of a page nobody is looking at.
    active_prefetch: usize,
    max_active_prefetch: usize,
}

impl RenderQueue {
    fn new(
        max_bbox: usize,
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
        max_prefetch: usize,
        max_active_prefetch: usize,
    ) -> Self {
        Self {
            bbox: Vec::new(),
            visible_preview: Vec::new(),
            visible: Vec::new(),
            preview: Vec::new(),
            prefetch: Vec::new(),
            max_bbox,
            max_visible_preview,
            max_visible,
            max_preview,
            max_prefetch,
            active_prefetch: 0,
            max_active_prefetch,
        }
    }

    fn push(&mut self, priority: RenderPriority, req: RenderRequest) {
        let (stack, max) = match priority {
            RenderPriority::Bbox => (&mut self.bbox, self.max_bbox),
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

    // Next runnable request, and whether it is a prefetch job (so the worker releases its reserved
    // slot when done). Within each stack newest wins. Prefetch is withheld once max_active_prefetch
    // are already running, even when prefetch jobs are queued, so a worker stays free for on-demand
    // work; cheap tiers (previews) are never withheld.
    fn pop(&mut self) -> Option<(RenderRequest, bool)> {
        if let Some(req) = self
            .bbox
            .pop()
            .or_else(|| self.visible_preview.pop())
            .or_else(|| self.visible.pop())
            .or_else(|| self.preview.pop())
        {
            return Some((req, false));
        }
        if self.active_prefetch < self.max_active_prefetch {
            if let Some(req) = self.prefetch.pop() {
                self.active_prefetch += 1;
                return Some((req, true));
            }
        }
        None
    }

    // Drop queued look-ahead (prefetch full renders and prefetch previews). Called when the
    // viewport moves, i.e. a new on-demand visible render is requested: the queued look-ahead
    // targets a position that's no longer current and would only tie up a worker on a page near
    // nobody's viewport. The new viewport re-queues its own look-ahead on the next draw. In-flight
    // renders can't be stopped; this clears only what's still waiting.
    fn clear_lookahead(&mut self) {
        self.prefetch.clear();
        self.preview.clear();
    }
}

// Thread pool serving all background poppler work. Prioritises layout and the
// visible page over previews and prefetch, and bounds how many requests wait so
// a fast scroll can't build an unbounded backlog ahead of the page being viewed.
pub(crate) struct RenderPool {
    inner: Arc<(Mutex<RenderQueue>, Condvar)>,
}

impl RenderPool {
    pub(crate) fn new(
        pool_size: usize,
        max_bbox: usize,
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
        max_prefetch: usize,
    ) -> Self {
        // Keep one worker off prefetch so on-demand renders never queue behind it, but never drop
        // below one (a single-worker pool must still prefetch).
        let max_active_prefetch = pool_size.saturating_sub(1).max(1);
        let inner = Arc::new((
            Mutex::new(RenderQueue::new(
                max_bbox,
                max_visible_preview,
                max_visible,
                max_preview,
                max_prefetch,
                max_active_prefetch,
            )),
            Condvar::new(),
        ));
        for _ in 0..pool_size {
            Self::spawn_bg_thread(inner.clone());
        }
        Self { inner }
    }

    // Drop queued look-ahead. Called when the focused page changes (the viewport moved to new
    // territory), so workers don't grind through prefetch for the position just left. Not called per
    // visible render: on open several pages come on screen at once, and clearing on each would let
    // the last one drawn win the prefetch window and reorder the queue away from the focused page.
    pub(crate) fn discard_lookahead(&self) {
        let (lock, _cvar) = &*self.inner;
        lock.lock().unwrap().clear_lookahead();
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
                let (req, is_prefetch) = {
                    let (lock, cvar) = &*inner;
                    let mut queue = lock.lock().unwrap();
                    loop {
                        if let Some(popped) = queue.pop() {
                            break popped;
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

                // release the reserved prefetch slot and wake a worker that may have been holding
                // off prefetch while this one ran
                if is_prefetch {
                    let (lock, cvar) = &*inner;
                    lock.lock().unwrap().active_prefetch -= 1;
                    cvar.notify_one();
                }
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

    // Drains in priority order, releasing each prefetch slot as if the job finished immediately so
    // the reservation cap doesn't stall a single-threaded drain.
    fn drain(queue: &mut RenderQueue) -> Vec<String> {
        let mut order = Vec::new();
        while let Some((req, is_prefetch)) = queue.pop() {
            if is_prefetch {
                queue.active_prefetch -= 1;
            }
            order.push(req.uri);
        }
        order
    }

    #[test]
    fn priority_order_and_newest_wins() {
        let mut q = RenderQueue::new(4, 4, 4, 4, 4, 1);
        q.push(RenderPriority::Prefetch, req("p1"));
        q.push(RenderPriority::Preview, req("pv1"));
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::VisiblePreview, req("vp1"));
        q.push(RenderPriority::Bbox, req("b1"));
        q.push(RenderPriority::Prefetch, req("p2"));
        q.push(RenderPriority::Preview, req("pv2"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::VisiblePreview, req("vp2"));
        q.push(RenderPriority::Bbox, req("b2"));

        // bbox, then visible preview, visible full, prefetch preview, prefetch
        // full; newest first in each
        assert_eq!(
            drain(&mut q),
            vec!["b2", "b1", "vp2", "vp1", "v2", "v1", "pv2", "pv1", "p2", "p1"]
        );
    }

    #[test]
    fn over_cap_drops_oldest() {
        let mut q = RenderQueue::new(2, 2, 2, 2, 2, 1);
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::Visible, req("v3"));

        // v1 (oldest) evicted; newest served first
        assert_eq!(drain(&mut q), vec!["v3", "v2"]);
    }

    #[test]
    fn prefetch_is_withheld_while_the_slot_is_taken() {
        let mut q = RenderQueue::new(4, 4, 4, 4, 4, 1);
        q.push(RenderPriority::Prefetch, req("pf1"));
        q.push(RenderPriority::Prefetch, req("pf2"));

        // the newest prefetch takes the only slot...
        let (first, is_prefetch) = q.pop().unwrap();
        assert_eq!(first.uri, "pf2");
        assert!(is_prefetch);

        // ...and the next is withheld while it runs, though the queue isn't empty
        assert!(q.pop().is_none());

        // once the running prefetch finishes, the slot frees and the next one runs
        q.active_prefetch -= 1;
        assert_eq!(q.pop().unwrap().0.uri, "pf1");
    }

    #[test]
    fn on_demand_work_runs_while_the_prefetch_slot_is_full() {
        let mut q = RenderQueue::new(4, 4, 4, 4, 4, 1);
        q.push(RenderPriority::Prefetch, req("pf1"));
        q.pop().unwrap(); // takes the only prefetch slot

        // a visible render arriving while prefetch is saturated is still served immediately
        q.push(RenderPriority::Visible, req("v1"));
        let (req, is_prefetch) = q.pop().unwrap();
        assert_eq!(req.uri, "v1");
        assert!(!is_prefetch);
    }

    #[test]
    fn clearing_lookahead_drops_prefetch_and_preview_only() {
        let mut q = RenderQueue::new(4, 4, 4, 4, 4, 4);
        q.push(RenderPriority::Bbox, req("b"));
        q.push(RenderPriority::VisiblePreview, req("vp"));
        q.push(RenderPriority::Visible, req("v"));
        q.push(RenderPriority::Preview, req("pv"));
        q.push(RenderPriority::Prefetch, req("pf"));

        q.clear_lookahead();

        // look-ahead tiers gone; on-demand tiers untouched
        assert_eq!(drain(&mut q), vec!["b", "vp", "v"]);
    }
}
