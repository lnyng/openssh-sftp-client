use super::lowlevel::Extensions;

use once_cell::sync::OnceCell;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Copy, Clone)]
pub(super) struct Limits {
    pub(super) read_len: u32,
    pub(super) write_len: u32,
}

#[derive(Debug)]
pub(super) struct ConnInfo {
    pub(super) limits: Limits,
    pub(super) extensions: Extensions,
}

#[derive(Debug)]
pub(super) struct Auxiliary {
    pub(super) conn_info: OnceCell<ConnInfo>,

    pub(super) max_buffered_write: u32,

    /// cancel_token is used to cancel `Awaitable*Future`
    /// when the read_task/flush_task has failed.
    pub(super) cancel_token: CancellationToken,

    /// flush_end_notify is used to avoid unnecessary wakeup
    /// in flush_task.
    pub(super) flush_end_notify: Notify,

    /// There can be at most `u32::MAX` pending requests, since each request
    /// requires a request id that is 32 bits.
    pub(super) pending_requests: AtomicU32,

    pub(super) max_pending_requests: u16,

    pub(super) shutdown_requested: AtomicBool,

    /// `Notify::notify_one` is called if
    /// pending_requests == max_pending_requests.
    pub(super) flush_immediately: Notify,
}

impl Auxiliary {
    pub(super) fn new(max_pending_requests: u16, max_buffered_write: u32) -> Self {
        Self {
            conn_info: OnceCell::new(),
            max_buffered_write,

            cancel_token: CancellationToken::new(),
            flush_end_notify: Notify::new(),

            pending_requests: AtomicU32::new(0),
            max_pending_requests,

            shutdown_requested: AtomicBool::new(false),
            flush_immediately: Notify::new(),
        }
    }

    pub(super) fn wakeup_flush_task(&self) {
        self.flush_end_notify.notify_one();

        // Use `==` here to avoid unnecessary wakeup of flush_task.
        if self.pending_requests.fetch_add(1, Ordering::Relaxed) == self.max_pending_requests() {
            self.flush_immediately.notify_one();
        }
    }

    pub(super) fn consume_pending_requests(&self, requests_consumed: u32) {
        self.pending_requests
            .fetch_sub(requests_consumed, Ordering::Relaxed);
    }

    fn conn_info(&self) -> &ConnInfo {
        self.conn_info
            .get()
            .expect("auxiliary.conn_info shall be initialized by sftp::Sftp::new")
    }

    pub(super) fn extensions(&self) -> Extensions {
        // since writing to conn_info is only done in `Sftp::new`,
        // reading these variable should never block.
        self.conn_info().extensions
    }

    pub(super) fn limits(&self) -> Limits {
        // since writing to conn_info is only done in `Sftp::new`,
        // reading these variable should never block.
        self.conn_info().limits
    }

    pub(super) fn max_pending_requests(&self) -> u32 {
        self.max_pending_requests as u32
    }

    pub(super) fn requests_shutdown(&self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);

        self.flush_immediately.notify_one();
        self.flush_end_notify.notify_one();
    }
}