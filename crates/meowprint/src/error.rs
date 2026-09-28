//! Errors from printer input, encoding, and communication.

/// An error from pixel input, encoding, Bluetooth access, or a printer operation.
///
/// Preparation errors leave a [`Printer`](crate::Printer) usable. Communication
/// failures disable the connection. A known printer condition rejected before a
/// write leaves it usable. Any error after a write attempt disables the connection.
/// Previously submitted rows can still print.
///
/// No operation is retried automatically. See [`Printer::is_usable`](crate::Printer::is_usable)
/// for connection state and [`Printer::disconnect`](crate::Printer::disconnect) for explicit cleanup.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An input value is invalid or the requested operation is unsupported.
    #[error("{0}")]
    InvalidInput(String),
    /// Compression failed before submission.
    #[error("Compression failed: {0}")]
    Compression(String),
    /// The host Bluetooth stack rejected an operation.
    #[error("Bluetooth: {0}")]
    Bluetooth(String),
    /// A transport backend rejected an operation.
    #[error("Transport: {0}")]
    Transport(String),
    /// The link closed. Submitted data has an unknown outcome.
    #[error(
        "The printer disconnected. Some submitted rows can still print. Do not retry automatically."
    )]
    Disconnected,
    /// An operation exceeded its deadline. The string names the operation phase.
    #[error("Timed out during {0}.")]
    Timeout(&'static str),
    /// The reported printer state prevents the requested operation.
    ///
    /// Use [`Printer::is_usable`](crate::Printer::is_usable) to determine whether the connection remains usable.
    #[error("The printer is unavailable: {0}.")]
    PrinterUnavailable(crate::PrinterState),
    /// The printer rejected a print job with an unknown error code.
    #[error("The printer rejected the print start with code 0x{code:02x}.")]
    StartRejected {
        /// The code reported by the printer.
        code: u8,
    },
    /// The printer sent data that violates the communication protocol.
    #[error("Protocol: {0}")]
    Protocol(&'static str),
    /// The printer sent an incomplete or unknown reply.
    #[error("The printer sent an incomplete or unknown reply during {0}.")]
    InvalidReply(&'static str),
    /// This connection no longer accepts operations.
    ///
    /// A communication failure, cancellation request, explicit disconnect, or
    /// dropped active operation can cause this state.
    #[error("This connection is no longer usable. Reconnect before starting another job.")]
    UnusableConnection,
    /// The operation observed a cancellation request and stopped submission.
    ///
    /// Cleanup is best effort. Previously submitted rows can still print.
    #[error("Submission stopped. Rows already sent can still print. Do not retry automatically.")]
    Cancelled,
}

/// The result type used by Meowprint, with [`Error`] as its failure type.
pub type Result<T> = std::result::Result<T, Error>;

/// Build an input error for internal validation.
pub fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput(message.into())
}
