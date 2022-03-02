#![forbid(unsafe_code)]

use crate::Writer;

use super::awaitable_responses::ArenaArc;
use super::awaitable_responses::Response;
use super::connection::SharedData;
use super::reader_buffered::ReaderBuffered;
use super::Error;
use super::Extensions;
use super::ToBuffer;

use std::fmt::Debug;
use std::io;

use openssh_sftp_protocol::response::{self, ServerVersion};
use openssh_sftp_protocol::serde::de::DeserializeOwned;
use openssh_sftp_protocol::ssh_format::from_bytes;

use tokio::io::{copy_buf, sink, AsyncBufReadExt, AsyncRead, AsyncReadExt};
use tokio_io_utility::{read_exact_to_bytes, read_exact_to_vec};

/// The ReadEnd for the lowlevel API.
#[derive(Debug)]
pub struct ReadEnd<R, W, Buffer, Auxiliary = ()> {
    reader: ReaderBuffered<R>,
    shared_data: SharedData<W, Buffer, Auxiliary>,
}

impl<R: AsyncRead + Unpin, W: Writer, Buffer: ToBuffer + 'static + Send + Sync, Auxiliary>
    ReadEnd<R, W, Buffer, Auxiliary>
{
    pub(crate) fn new(reader: R, shared_data: SharedData<W, Buffer, Auxiliary>) -> Self {
        Self {
            reader: ReaderBuffered::new(reader),
            shared_data,
        }
    }

    pub(crate) async fn receive_server_version(
        &mut self,
        version: u32,
    ) -> Result<Extensions, Error> {
        // Receive server version
        let len: u32 = self.read_and_deserialize(4).await?;
        if (len as usize) > 4096 {
            return Err(Error::SftpServerHelloMsgTooLong { len });
        }

        let drain = self.reader.read_exact_into_buffer(len as usize).await?;
        let server_version = ServerVersion::deserialize(&*drain)?;

        if server_version.version != version {
            Err(Error::UnsupportedSftpProtocol {
                version: server_version.version,
            })
        } else {
            Ok(server_version.extensions)
        }
    }

    async fn read_and_deserialize<T: DeserializeOwned>(&mut self, size: usize) -> Result<T, Error> {
        let drain = self.reader.read_exact_into_buffer(size).await?;
        Ok(from_bytes(&*drain)?.0)
    }

    async fn consume_packet(&mut self, len: u32, err: Error) -> Result<(), Error> {
        let reader = &mut self.reader;
        if let Err(consumption_err) = copy_buf(&mut reader.take(len as u64), &mut sink()).await {
            Err(Error::RecursiveErrors(Box::new((
                err,
                consumption_err.into(),
            ))))
        } else {
            Err(err)
        }
    }

    async fn read_into_box(&mut self, len: usize) -> Result<Box<[u8]>, Error> {
        let mut vec = Vec::new();
        read_exact_to_vec(&mut self.reader, &mut vec, len as usize).await?;

        Ok(vec.into_boxed_slice())
    }

    async fn read_in_data_packet_fallback(
        &mut self,
        len: usize,
    ) -> Result<Response<Buffer>, Error> {
        self.read_into_box(len).await.map(Response::AllocatedBox)
    }

    /// * `len` - excludes packet_type and request_id.
    async fn read_in_data_packet(
        &mut self,
        len: u32,
        buffer: Option<Buffer>,
    ) -> Result<Response<Buffer>, Error> {
        // Since the data is sent as a string, we need to consume the 4-byte length first.
        self.reader.read_exact_into_buffer(4).await?;

        let len = (len - 4) as usize;

        if let Some(mut buffer) = buffer {
            match buffer.get_buffer() {
                super::Buffer::Vector(vec) => {
                    read_exact_to_vec(&mut self.reader, vec, len).await?;
                    Ok(Response::Buffer(buffer))
                }
                super::Buffer::Slice(slice) => {
                    if slice.len() >= len {
                        self.reader.read_exact(slice).await?;
                        Ok(Response::Buffer(buffer))
                    } else {
                        self.read_in_data_packet_fallback(len).await
                    }
                }
                super::Buffer::Bytes(bytes) => {
                    read_exact_to_bytes(&mut self.reader, bytes, len).await?;
                    Ok(Response::Buffer(buffer))
                }
            }
        } else {
            self.read_in_data_packet_fallback(len).await
        }
    }

    /// * `len` - includes packet_type and request_id.
    async fn read_in_packet(&mut self, len: u32) -> Result<Response<Buffer>, Error> {
        let response: response::Response = self.read_and_deserialize(len as usize).await?;

        Ok(Response::Header(response.response_inner))
    }

    /// * `len` - excludes packet_type and request_id.
    async fn read_in_extended_reply(&mut self, len: u32) -> Result<Response<Buffer>, Error> {
        self.read_into_box(len as usize)
            .await
            .map(Response::ExtendedReply)
    }

    /// Precondition: [`ReadEnd::wait_for_new_request`] must not be 0.
    ///
    /// # Restart on Error
    ///
    /// Only when the returned error is [`Error::InvalidResponseId`] or
    /// [`Error::AwaitableError`], can the function be restarted.
    ///
    /// Upon other errors [`Error::IOError`], [`Error::FormatError`] and
    /// [`Error::RecursiveErrors`], the sftp session has to be discarded.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let readend = ...;
    /// loop {
    ///     let new_requests_submit = readend.wait_for_new_request().await;
    ///     if new_requests_submit == 0 {
    ///         break;
    ///     }
    ///
    ///     // If attempt to read in more than new_requests_submit, then
    ///     // `read_in_one_packet` might block forever.
    ///     for _ in 0..new_requests_submit {
    ///         readend.read_in_one_packet().await.unwrap();
    ///     }
    /// }
    /// ```
    /// # Cancel Safety
    ///
    /// This function is not cancel safe.
    ///
    /// Dropping the future might cause the response packet to be partially read,
    /// and the next read would treat the partial response as a new response.
    pub async fn read_in_one_packet(&mut self) -> Result<(), Error> {
        let drain = self.reader.read_exact_into_buffer(9).await?;
        let (len, packet_type, response_id): (u32, u8, u32) = from_bytes(&*drain)?.0;

        let len = len - 5;

        let callback = match self.shared_data.responses().get(response_id) {
            Ok(callback) => callback,

            // Invalid response_id
            Err(err) => {
                drop(drain);

                // Consume the invalid data to return self to a valid state
                // where read_in_one_packet can be called again.
                return self.consume_packet(len, err).await;
            }
        };

        let response = if response::Response::is_data(packet_type) {
            drop(drain);

            let buffer = match callback.take_input() {
                Ok(buffer) => buffer,
                Err(err) => {
                    // Consume the invalid data to return self to a valid state
                    // where read_in_one_packet can be called again.
                    return self.consume_packet(len, err.into()).await;
                }
            };
            self.read_in_data_packet(len, buffer).await?
        } else if response::Response::is_extended_reply(packet_type) {
            drop(drain);

            self.read_in_extended_reply(len).await?
        } else {
            // Consumes 4 bytes and put back the rest, since
            // read_in_packet needs the packet_type and response_id.
            drain.subdrain(4);

            self.read_in_packet(len + 5).await?
        };

        let res = callback.done(response);

        // If counter == 2, then it must be one of the following situation:
        //  - `ReadEnd` is the only holder other than the `Arena` itself;
        //  - `ReadEnd` and the `AwaitableInner` is the holder and `AwaitableInner::drop`
        //    has already `ArenaArc::remove`d it.
        //
        // In case 1, since there is no `AwaitableInner` holding reference to it,
        // it can be removed safely.
        //
        // In case 2, since it is already removed, remove it again is a no-op.
        //
        // NOTE that if the arc is dropped after this call while having the
        // `Awaitable*::drop` executed before `callback.done`, then the callback
        // would not be removed.
        //
        // Though this kind of situation is rare.
        if ArenaArc::strong_count(&callback) == 2 {
            ArenaArc::remove(&callback);
        }

        Ok(res?)
    }

    /// Wait for next packet to be readable.
    ///
    /// Return `Ok(())` if next packet is ready and readable, `Error::IOError(io_error)`
    /// where `io_error.kind() == ErrorKind::UnexpectedEof` if `EOF` is met.
    ///
    /// # Cancel Safety
    ///
    /// This function is cancel safe.
    pub async fn ready_for_read(&mut self) -> Result<(), io::Error> {
        if self.reader.fill_buf().await?.is_empty() {
            // Empty buffer means EOF
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, ""))
        } else {
            Ok(())
        }
    }
}

impl<R, W, Buffer, Auxiliary> ReadEnd<R, W, Buffer, Auxiliary> {
    /// Return number of requests sent (including requests that are still in the write
    /// buffer and not yet flushed) and number of responses to read in.
    /// **Read 0 if the connection is closed.**
    ///
    /// You must call this function in a loop, break if this function returns
    /// 0, otherwise call [`ReadEnd::read_in_one_packet`] for `n` times where `n` in the
    /// return value of this function, then repeat.
    ///
    /// # Cancel Safety
    ///
    /// It is perfectly safe to cancel this future.
    #[inline(always)]
    pub async fn wait_for_new_request(&self) -> u32 {
        self.shared_data.wait_for_new_request().await
    }

    /// Return the [`SharedData`] held by [`ReadEnd`].
    pub fn get_shared_data(&self) -> &SharedData<W, Buffer, Auxiliary> {
        &self.shared_data
    }
}