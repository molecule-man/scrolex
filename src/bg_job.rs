use std::sync::mpsc::Receiver;
use std::thread;

use once_cell::sync::Lazy;
use poppler::Document;

thread_local!(
    pub(crate) static JOB_MANAGER: Lazy<JobManager> = Lazy::new(JobManager::new);
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
    fn new() -> Self {
        let (send, recv) = std::sync::mpsc::channel();
        let manager = JobManager { send };
        Self::spawn_bg_thread(recv);
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

    fn spawn_bg_thread(recv: Receiver<Request>) {
        thread::spawn(move || {
            let mut doc = None;
            let mut doc_uri = String::new();

            for req in recv {
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
