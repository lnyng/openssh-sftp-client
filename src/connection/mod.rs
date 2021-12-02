mod awaitable;
mod awaitable_responses;

use super::Error;

use awaitable_responses::AwaitableResponses;

use openssh_sftp_protocol::constants::SSH2_FILEXFER_VERSION;
use openssh_sftp_protocol::request::{Hello, Request};
use openssh_sftp_protocol::response::ServerVersion;
use openssh_sftp_protocol::serde::{Deserialize, Serialize};
use ssh_format::Transformer;

use std::io::IoSlice;

use tokio::io::AsyncReadExt;
use tokio_async_write_utility::AsyncWriteUtility;
use tokio_pipe::{PipeRead, PipeWrite};

pub use awaitable_responses::{AwaitableResponse, Response};

pub use openssh_sftp_protocol::file_attrs::FileAttrs;
pub use openssh_sftp_protocol::request::{CreateFlags, FileMode, OpenFile, RequestInner};
pub use openssh_sftp_protocol::response::{NameEntry, ResponseInner};
pub use openssh_sftp_protocol::{Handle, HandleOwned};

#[derive(Debug)]
pub struct Connection {
    writer: PipeWrite,
    reader: PipeRead,
    transformer: Transformer,
    responses: AwaitableResponses,
}
impl Connection {
    async fn write<T>(&mut self, value: T) -> Result<(), Error>
    where
        T: Serialize,
    {
        self.writer
            .write_vectored_all(&mut [IoSlice::new(self.transformer.serialize(value)?)])
            .await?;

        Ok(())
    }

    async fn read_exact(&mut self, size: usize) -> Result<(), Error> {
        self.transformer.get_buffer().resize(size, 0);
        self.reader
            .read_exact(&mut self.transformer.get_buffer())
            .await?;

        Ok(())
    }

    async fn read_and_deserialize<'a, T>(&'a mut self, size: usize) -> Result<T, Error>
    where
        T: Deserialize<'a>,
    {
        self.read_exact(size).await?;

        // Ignore any trailing bytes to be forward compatible
        Ok(self.transformer.deserialize()?.0)
    }

    async fn negotiate(&mut self) -> Result<(), Error> {
        let version = SSH2_FILEXFER_VERSION;

        // Sent hello message
        self.write(Hello {
            version,
            extensions: Default::default(),
        })
        .await?;

        // Receive server version
        let len: u32 = self.read_and_deserialize(4).await?;
        self.read_exact(len as usize).await?;
        let server_version = ServerVersion::deserialize(self.transformer.get_buffer())?;

        if server_version.version != version {
            Err(Error::UnsupportedSftpProtocol)
        } else {
            Ok(())
        }
    }

    pub async fn new(reader: PipeRead, writer: PipeWrite) -> Result<Self, Error> {
        let mut val = Self {
            reader,
            writer,
            transformer: Transformer::default(),
            responses: AwaitableResponses::default(),
        };

        val.negotiate().await?;

        Ok(val)
    }

    /// Send requests.
    ///
    /// **Please use `Self::send_write_request` for sending write requests.**
    pub async fn send_request(
        &mut self,
        request: RequestInner<'_>,
    ) -> Result<AwaitableResponse, Error> {
        let (request_id, awaitable_response) = self.responses.insert();
        match self
            .write(Request {
                request_id,
                inner: request,
            })
            .await
        {
            Ok(_) => Ok(awaitable_response),
            Err(err) => {
                self.responses.remove(request_id).unwrap();

                Err(err)
            }
        }
    }

    async fn send_write_request_impl(
        &mut self,
        request_id: u32,
        handle: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<(), Error> {
        let header = Request::serialize_write_request(
            self.transformer.get_ser(),
            request_id,
            handle,
            offset,
            data.len().try_into().map_err(|_| Error::BufferTooLong)?,
        )?;

        let mut slices = [IoSlice::new(header), IoSlice::new(data)];
        self.writer.write_vectored_all(&mut slices).await?;

        Ok(())
    }

    /// Send write requests
    pub async fn send_write_request(
        &mut self,
        handle: &[u8],
        offset: u64,
        data: &[u8],
    ) -> Result<AwaitableResponse, Error> {
        let (request_id, awaitable_response) = self.responses.insert();

        match self
            .send_write_request_impl(request_id, handle, offset, data)
            .await
        {
            Ok(_) => Ok(awaitable_response),
            Err(err) => {
                self.responses.remove(request_id).unwrap();

                Err(err)
            }
        }
    }
}
