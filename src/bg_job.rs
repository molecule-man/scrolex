// Background worker pool that runs jobs against a poppler Document. Each worker
// keeps its own Document open (Document is not Send), reopening only when the
// requested uri changes. One pool serves every kind of job - bbox/layout,
// visible-page renders and low-res previews - so the number of resident
// Documents equals the pool size, independent of how many job kinds exist.

use std::sync::{Arc, Condvar, Mutex};
use std::thread;

// Priority of a queued job. Visible (on-screen full renders) outrank the low-res previews: the
// current page's own blur (VisiblePreview) still comes first so a stand-in appears in ~40ms, but
// the look-ahead previews yield to the sharp renders of pages on screen.
#[derive(Clone, Copy)]
pub(crate) enum RenderPriority {
    Bbox,
    VisiblePreview,
    Visible,
    Preview,
}

impl RenderPriority {
    // Short tag for logging what kind of work a render was.
    pub(crate) fn label(self) -> &'static str {
        match self {
            RenderPriority::Bbox => "bbox",
            RenderPriority::VisiblePreview => "low-res (visible)",
            RenderPriority::Visible => "on-demand (visible)",
            RenderPriority::Preview => "low-res (prefetch)",
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
    max_bbox: usize,
    max_visible_preview: usize,
    max_visible: usize,
    max_preview: usize,
}

impl RenderQueue {
    fn new(
        max_bbox: usize,
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
    ) -> Self {
        Self {
            bbox: Vec::new(),
            visible_preview: Vec::new(),
            visible: Vec::new(),
            preview: Vec::new(),
            max_bbox,
            max_visible_preview,
            max_visible,
            max_preview,
        }
    }

    fn push(&mut self, priority: RenderPriority, req: RenderRequest) {
        let (stack, max) = match priority {
            RenderPriority::Bbox => (&mut self.bbox, self.max_bbox),
            RenderPriority::VisiblePreview => (&mut self.visible_preview, self.max_visible_preview),
            RenderPriority::Visible => (&mut self.visible, self.max_visible),
            RenderPriority::Preview => (&mut self.preview, self.max_preview),
        };
        stack.push(req);
        while stack.len() > max {
            stack.remove(0);
        }
    }

    // Next runnable request, highest priority first and newest within a tier.
    fn pop(&mut self) -> Option<RenderRequest> {
        self.bbox
            .pop()
            .or_else(|| self.visible_preview.pop())
            .or_else(|| self.visible.pop())
            .or_else(|| self.preview.pop())
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
        max_bbox: usize,
        max_visible_preview: usize,
        max_visible: usize,
        max_preview: usize,
    ) -> Self {
        let inner = Arc::new((
            Mutex::new(RenderQueue::new(
                max_bbox,
                max_visible_preview,
                max_visible,
                max_preview,
            )),
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
        q.push(RenderPriority::Preview, req("pv1"));
        q.push(RenderPriority::Visible, req("v1"));
        q.push(RenderPriority::VisiblePreview, req("vp1"));
        q.push(RenderPriority::Bbox, req("b1"));
        q.push(RenderPriority::Preview, req("pv2"));
        q.push(RenderPriority::Visible, req("v2"));
        q.push(RenderPriority::VisiblePreview, req("vp2"));
        q.push(RenderPriority::Bbox, req("b2"));

        // bbox, then visible preview, visible full, look-ahead preview; newest first in each
        assert_eq!(
            drain(&mut q),
            vec!["b2", "b1", "vp2", "vp1", "v2", "v1", "pv2", "pv1"]
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
}
