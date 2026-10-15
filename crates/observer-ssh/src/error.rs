use acc_core::driver::DriverError;
use observer::multiplexer::MuxError;

/// Errors driving a pane over SSH.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Ssh(#[from] acc_ssh::Error),
    /// A multiplexer can't do this, or printed something unexpected.
    #[error("multiplexer: {0}")]
    Mux(MuxError),
    #[error(transparent)]
    Driver(#[from] DriverError),
}

impl Error {
    /// Whether retrying after a reconnect may succeed.
    pub fn is_transient(&self) -> bool {
        matches!(self, Error::Ssh(e) if e.is_transient())
    }

    /// A script on the host exited non-zero.
    pub fn is_remote(&self) -> bool {
        matches!(self, Error::Ssh(acc_ssh::Error::Remote { .. }))
    }
}
