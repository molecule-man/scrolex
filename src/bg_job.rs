use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::thread;

use once_cell::sync::Lazy;
use poppler::Document;

const POOL_SIZE: usize = 1;

thread_local!(
    pub(crate) static JOB_MANAGER: Lazy<JobManager> = Lazy::new(JobManager::new);
);

type Job = Box<dyn FnOnce(&Document) + Send + 'static>;
type DebouncableJob = Box<dyn FnOnce(Result<&Document, ()>) + Send + 'static>;

struct Request {
    uri: String,
    job: Job,
}

pub(crate) struct JobManager {
    send: std::sync::mpsc::Sender<Request>,
}

impl JobManager {
    fn new() -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let recv = Arc::new(Mutex::new(recv));
        let manager = Self { send };
        for _ in 0..POOL_SIZE {
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
                    doc =
                        Some(Document::from_file(&req.uri, None).expect("Couldn't open the file!"));
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

struct DebouncableRequest {
    uri: String,
    job: DebouncableJob,
}

pub(crate) struct DebouncingJobQueue {
    send: std::sync::mpsc::Sender<DebouncableRequest>,
    //debounce_timeout: std::time::Duration,
}

impl DebouncingJobQueue {
    pub(crate) fn new(
        pool_size: u8,
        //debounce_timeout: std::time::Duration,
    ) -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let recv = Arc::new(Mutex::new(recv));
        let manager = Self {
            send,
            //debounce_timeout,
        };
        for _ in 0..pool_size {
            Self::spawn_bg_thread(recv.clone());
        }
        manager
    }

    pub(crate) fn execute(&self, uri: &str, job: DebouncableJob) {
        self.send
            .send(DebouncableRequest {
                uri: uri.to_string(),
                job,
            })
            .expect("Failed to send job request");
    }

    fn spawn_bg_thread(recv: Arc<Mutex<Receiver<DebouncableRequest>>>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            loop {
                let mut req = recv.lock().unwrap().recv().unwrap();

                'inner: loop {
                    match recv
                        .lock()
                        .unwrap()
                        .try_recv()
                        //.recv_timeout(std::time::Duration::from_secs(1))
                    {
                        Ok(next_req) => {
                            (req.job)(Err(()));
                            req = next_req;
                        }
                        Err(_) => {
                            break 'inner;
                        }
                    }
                }

                if doc.is_none() || doc_uri != req.uri {
                    doc =
                        Some(Document::from_file(&req.uri, None).expect("Couldn't open the file!"));
                    doc_uri.clone_from(&req.uri);
                }
                let doc = doc.as_ref().unwrap();

                (req.job)(Ok(doc));
            }
        });
    }
}
