//! A delayed, one-line status, owned only while preparing an eager view.
use indicatif::{ProgressBar, ProgressStyle};
use std::{io::IsTerminal, sync::mpsc, thread::JoinHandle, time::Duration};

pub(crate) struct MaterializationProgress {
    stop: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}
impl MaterializationProgress {
    pub(crate) fn start() -> Self {
        let (stop, receiver) = mpsc::channel();
        let worker = (std::io::stderr().is_terminal()
            && std::env::var_os("TERM").is_none_or(|term| term != "dumb"))
        .then(|| {
            std::thread::Builder::new()
                .name("materialization-progress".into())
                .spawn(move || {
                    if receiver.recv_timeout(Duration::from_millis(500))
                        != Err(mpsc::RecvTimeoutError::Timeout)
                    {
                        return;
                    }
                    let bar = ProgressBar::new_spinner();
                    bar.set_style(
                        ProgressStyle::with_template("{spinner} Preparing snapshot")
                            .expect("static progress template")
                            .tick_strings(&["|", "/", "-", "\\"]),
                    );
                    loop {
                        bar.tick();
                        if receiver.recv_timeout(Duration::from_millis(100))
                            != Err(mpsc::RecvTimeoutError::Timeout)
                        {
                            break;
                        }
                    }
                    bar.finish_and_clear();
                })
        })
        .and_then(Result::ok);
        Self { stop, worker }
    }
}
impl Drop for MaterializationProgress {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(worker) = self.worker.take() {
            // Clearing completes before the child inherits the terminal, also
            // on the error/cancellation return path.
            let _ = worker.join();
        }
    }
}
