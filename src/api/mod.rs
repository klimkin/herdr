pub mod client;
mod event_hub;
pub mod schema;
mod server;
mod status;
mod subscriptions;
mod wait;

pub use event_hub::EventHub;
pub use server::ServerHandle;
pub(crate) use server::{api_method_name, start_server_with_stop_control};
pub use status::{read_runtime_status_at, RuntimeStatus};

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::api::schema::Request;

pub const SOCKET_PATH_ENV_VAR: &str = "HERDR_SOCKET_PATH";

pub(crate) fn request_changes_ui(request: &Request) -> bool {
    request.method.operation().changes_ui
}

pub struct ApiRequestMessage {
    pub request: Request,
    pub respond_to: std::sync::mpsc::Sender<String>,
    pub response_write_complete: Option<std::sync::mpsc::Receiver<()>>,
    #[cfg(feature = "latency-prof")]
    pub(crate) diagnostic_trace: crate::latency_prof::event::EventTrace,
}

impl ApiRequestMessage {
    pub(crate) fn selected_wait(&mut self, wait: &crate::latency_prof::wait::WaitTrace) {
        #[cfg(feature = "latency-prof")]
        {
            self.diagnostic_trace = self.diagnostic_trace.with_selected_wait(wait);
        }
        #[cfg(not(feature = "latency-prof"))]
        let _ = wait;
    }

    pub(crate) fn deferred(self) -> Self {
        #[cfg(feature = "latency-prof")]
        {
            let mut message = self;
            message.diagnostic_trace = message.diagnostic_trace.deferred();
            message
        }
        #[cfg(not(feature = "latency-prof"))]
        {
            self
        }
    }

    pub(crate) fn received(&self, remaining: usize) {
        #[cfg(feature = "latency-prof")]
        self.diagnostic_trace.received(2, remaining);
        #[cfg(not(feature = "latency-prof"))]
        let _ = remaining;
    }
}

pub type ApiRequestSender = mpsc::UnboundedSender<ApiRequestMessage>;

pub fn socket_path() -> PathBuf {
    crate::session::active_api_socket_path()
}
