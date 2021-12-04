use std::io;
use thiserror::Error;

use openssh_sftp_protocol::ssh_format;

pub use openssh_sftp_protocol::response::ResponseInner;

#[derive(Debug, Error)]
pub enum Error {
    /// Server speaks sftp protocol other than protocol 4.
    #[error("Server speaks sftp protocol other than protocol 4.")]
    UnsupportedSftpProtocol,

    /// IO Error (Excluding `EWOULDBLOCK`): {0}.
    #[error("IO Error (Excluding `EWOULDBLOCK`): {0}.")]
    IOError(#[from] io::Error),

    /// Failed to serialize/deserialize the message: {0}.
    #[error("Failed to serialize/deserialize the message: {0}.")]
    FormatError(#[from] ssh_format::Error),

    /// Sftp protocol can only send and receive at most u32::MAX data in one request.
    #[error("Sftp protocol can only send and receive at most u32::MAX data in one request.")]
    BufferTooLong,

    /// The response id is invalid.
    #[error("The response id is invalid.")]
    InvalidResponseId,
}
