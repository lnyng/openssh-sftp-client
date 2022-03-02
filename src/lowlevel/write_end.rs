#![forbid(unsafe_code)]

use super::*;
use crate::Writer;

use awaitable_responses::ArenaArc;
use connection::SharedData;
use writer_buffered::{AtomicWriteIoSlices, WriteBuffer};

use std::borrow::Cow;
use std::convert::TryInto;
use std::fmt::Debug;
use std::io::IoSlice;
use std::ops::{Deref, DerefMut};
use std::path::Path;

use bytes::Bytes;
use openssh_sftp_protocol::file_attrs::FileAttrs;
use openssh_sftp_protocol::request::*;
use openssh_sftp_protocol::serde::Serialize;
use openssh_sftp_protocol::ssh_format::Serializer;
use openssh_sftp_protocol::Handle;

/// It is recommended to create at most one `WriteEnd` per thread
/// using [`WriteEnd::clone`].
#[derive(Debug)]
pub struct WriteEnd<W, Buffer, Auxiliary = ()> {
    serializer: Serializer<WriteBuffer>,
    shared_data: SharedData<W, Buffer, Auxiliary>,
}

impl<W, Buffer, Auxiliary> Clone for WriteEnd<W, Buffer, Auxiliary> {
    fn clone(&self) -> Self {
        Self::new(self.shared_data.clone())
    }
}

impl<W, Buffer, Auxiliary> WriteEnd<W, Buffer, Auxiliary> {
    /// Create a [`WriteEnd`] from [`SharedData`].
    pub fn new(shared_data: SharedData<W, Buffer, Auxiliary>) -> Self {
        Self {
            serializer: Serializer::new(),
            shared_data,
        }
    }

    /// Consume the [`WriteEnd`] and return the stored [`SharedData`].
    pub fn into_shared_data(self) -> SharedData<W, Buffer, Auxiliary> {
        self.shared_data
    }
}

impl<W, Buffer, Auxiliary> Deref for WriteEnd<W, Buffer, Auxiliary> {
    type Target = SharedData<W, Buffer, Auxiliary>;

    fn deref(&self) -> &Self::Target {
        &self.shared_data
    }
}

impl<W, Buffer, Auxiliary> DerefMut for WriteEnd<W, Buffer, Auxiliary> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.shared_data
    }
}

impl<W: Writer, Buffer: Send + Sync, Auxiliary> WriteEnd<W, Buffer, Auxiliary> {
    pub(crate) async fn send_hello(&mut self, version: u32) -> Result<(), Error> {
        self.shared_data
            .get_mut_writer()
            .unwrap()
            .write_all(&*Self::serialize(&mut self.serializer, Hello { version })?)
            .await?;

        Ok(())
    }

    fn serialize<T>(serializer: &mut Serializer<WriteBuffer>, value: T) -> Result<Bytes, Error>
    where
        T: Serialize,
    {
        serializer.reset();
        value.serialize(&mut *serializer)?;

        Ok(serializer.get_output()?.split())
    }

    /// Send requests.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    fn send_request(
        &mut self,
        id: Id<Buffer>,
        request: RequestInner<'_>,
        buffer: Option<Buffer>,
    ) -> Result<ArenaArc<Buffer>, Error> {
        let serialized = Self::serialize(
            &mut self.serializer,
            Request {
                request_id: ArenaArc::slot(&id.0),
                inner: request,
            },
        )?;

        id.0.reset(buffer);
        self.shared_data.writer().push(serialized);
        self.shared_data.notify_new_packet_event();

        Ok(id.into_inner())
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_open_file_request(
        &mut self,
        id: Id<Buffer>,
        params: OpenFileRequest<'_>,
    ) -> Result<AwaitableHandle<Buffer>, Error> {
        self.send_request(id, RequestInner::Open(params), None)
            .map(AwaitableHandle::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_close_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Close(handle), None)
            .map(AwaitableStatus::new)
    }

    /// - `buffer` - If set to `None` or the buffer is not long enough,
    ///   then [`crate::lowlevel::Data::AllocatedBox`] will be returned.
    ///
    /// Return [`crate::lowlevel::Data::Buffer`] or
    /// [`crate::lowlevel::Data::AllocatedBox`] if not EOF, otherwise returns
    /// [`crate::lowlevel::Data::Eof`].
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_read_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        len: u32,
        buffer: Option<Buffer>,
    ) -> Result<AwaitableData<Buffer>, Error> {
        self.send_request(
            id,
            RequestInner::Read {
                handle,
                offset,
                len,
            },
            buffer,
        )
        .map(AwaitableData::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_remove_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Remove(path), None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_rename_request(
        &mut self,
        id: Id<Buffer>,
        oldpath: Cow<'_, Path>,
        newpath: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Rename { oldpath, newpath }, None)
            .map(AwaitableStatus::new)
    }

    /// * `attrs` - [`FileAttrs::get_size`] must be equal to `None`.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_mkdir_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
        attrs: FileAttrs,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Mkdir { path, attrs }, None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_rmdir_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Rmdir(path), None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_opendir_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableHandle<Buffer>, Error> {
        self.send_request(id, RequestInner::Opendir(path), None)
            .map(AwaitableHandle::new)
    }

    /// Return all entries in the directory specified by the `handle`, including
    /// `.` and `..`.
    ///
    /// The `filename` only contains the basename.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_readdir_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
    ) -> Result<AwaitableNameEntries<Buffer>, Error> {
        self.send_request(id, RequestInner::Readdir(handle), None)
            .map(AwaitableNameEntries::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_stat_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableAttrs<Buffer>, Error> {
        self.send_request(id, RequestInner::Stat(path), None)
            .map(AwaitableAttrs::new)
    }

    /// Does not follow symlink
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_lstat_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableAttrs<Buffer>, Error> {
        self.send_request(id, RequestInner::Lstat(path), None)
            .map(AwaitableAttrs::new)
    }

    /// * `handle` - Must be opened with `FileMode::READ`.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_fstat_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
    ) -> Result<AwaitableAttrs<Buffer>, Error> {
        self.send_request(id, RequestInner::Fstat(handle), None)
            .map(AwaitableAttrs::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_setstat_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
        attrs: FileAttrs,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Setstat { path, attrs }, None)
            .map(AwaitableStatus::new)
    }

    /// * `handle` - Must be opened with `OpenOptions::write` set.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_fsetstat_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        attrs: FileAttrs,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Fsetstat { handle, attrs }, None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_readlink_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableName<Buffer>, Error> {
        self.send_request(id, RequestInner::Readlink(path), None)
            .map(AwaitableName::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_realpath_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableName<Buffer>, Error> {
        self.send_request(id, RequestInner::Realpath(path), None)
            .map(AwaitableName::new)
    }

    /// Create symlink
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    pub fn send_symlink_request(
        &mut self,
        id: Id<Buffer>,
        targetpath: Cow<'_, Path>,
        linkpath: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(
            id,
            RequestInner::Symlink {
                linkpath,
                targetpath,
            },
            None,
        )
        .map(AwaitableStatus::new)
    }

    /// Return limits of the server
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// # Precondition
    ///
    /// Requires [`Extensions::limits`] to be true.
    pub fn send_limits_request(
        &mut self,
        id: Id<Buffer>,
    ) -> Result<AwaitableLimits<Buffer>, Error> {
        self.send_request(id, RequestInner::Limits, None)
            .map(AwaitableLimits::new)
    }

    /// This supports canonicalisation of relative paths and those that need
    /// tilde-expansion, i.e. "~", "~/..." and "~user/...".
    ///
    /// These paths are expanded using shell-like rules and the resultant path
    /// is canonicalised similarly to [`WriteEnd::send_realpath_request`].
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// # Precondition
    ///
    /// Requires [`Extensions::expand_path`] to be true.
    pub fn send_expand_path_request(
        &mut self,
        id: Id<Buffer>,
        path: Cow<'_, Path>,
    ) -> Result<AwaitableName<Buffer>, Error> {
        self.send_request(id, RequestInner::ExpandPath(path), None)
            .map(AwaitableName::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// # Precondition
    ///
    /// Requires [`Extensions::fsync`] to be true.
    pub fn send_fsync_request(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::Fsync(handle), None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// # Precondition
    ///
    /// Requires [`Extensions::hardlink`] to be true.
    pub fn send_hardlink_request(
        &mut self,
        id: Id<Buffer>,
        oldpath: Cow<'_, Path>,
        newpath: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::HardLink { oldpath, newpath }, None)
            .map(AwaitableStatus::new)
    }

    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// # Precondition
    ///
    /// Requires [`Extensions::posix_rename`] to be true.
    pub fn send_posix_rename_request(
        &mut self,
        id: Id<Buffer>,
        oldpath: Cow<'_, Path>,
        newpath: Cow<'_, Path>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_request(id, RequestInner::PosixRename { oldpath, newpath }, None)
            .map(AwaitableStatus::new)
    }
}

impl<W: Writer, Buffer: ToBuffer + Send + Sync + 'static, Auxiliary>
    WriteEnd<W, Buffer, Auxiliary>
{
    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// This function is only suitable for writing small data since it needs to copy the
    /// entire `data` into buffer.
    pub fn send_write_request_buffered(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data: Cow<'_, [u8]>,
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let len: u32 = data.len().try_into()?;

        self.serializer.reserve(
            // 9 bytes for the 4-byte len of packet, 1-byte packet type and
            // 4-byte request id
            9 +
            handle.into_inner().len() +
            // 8 bytes for the offset
            8 +
            // 4 bytes for the lenght of the data to be sent
            4 +
            // len of the data
            len as usize,
        );

        self.send_request(
            id,
            RequestInner::Write {
                handle,
                offset,
                data,
            },
            None,
        )
        .map(AwaitableStatus::new)
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// This function is only suitable for writing small data since it needs to copy the
    /// entire `data` into buffer.
    pub fn send_write_request_buffered_vectored(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        io_slices: &[IoSlice<'_>],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_write_request_buffered_vectored2(id, handle, offset, &[io_slices])
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// This function is only suitable for writing small data since it needs to copy the
    /// entire `data` into buffer.
    pub fn send_write_request_buffered_vectored2(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        bufs: &[&[IoSlice<'_>]],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let len: usize = bufs
            .iter()
            .flat_map(Deref::deref)
            .map(|io_slice| io_slice.len())
            .sum();
        let len: u32 = len.try_into()?;

        self.serializer.reserve(
            // 9 bytes for the 4-byte len of packet, 1-byte packet type and
            // 4-byte request id
            9 +
            handle.into_inner().len() +
            // 8 bytes for the offset
            8 +
            // 4 bytes for the lenght of the data to be sent
            4 +
            // len of the data
            len as usize,
        );

        let buffer = Request::serialize_write_request(
            &mut self.serializer,
            ArenaArc::slot(&id.0),
            handle,
            offset,
            len,
        )?;

        for io_slices in bufs {
            buffer.put_io_slices(io_slices);
        }

        id.0.reset(None);
        self.shared_data.writer().push(buffer.split());
        self.shared_data.notify_new_packet_event();

        Ok(AwaitableStatus::new(id.into_inner()))
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// This function is zero-copy.
    pub fn send_write_request_zero_copy(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data: &[Bytes],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_write_request_zero_copy2(id, handle, offset, &[data])
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// NOTE that this merely add the request to the buffer, you need to call
    /// [`SharedData::flush`] to actually send the requests.
    ///
    /// This function is zero-copy.
    pub fn send_write_request_zero_copy2(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data_slice: &[&[Bytes]],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let len: usize = data_slice
            .iter()
            .flat_map(Deref::deref)
            .map(Bytes::len)
            .sum();
        let len: u32 = len.try_into()?;

        let header = Request::serialize_write_request(
            &mut self.serializer,
            ArenaArc::slot(&id.0),
            handle,
            offset,
            len,
        )?
        .split();

        // queue_pusher holds the mutex, so the `push` and `extend` here are atomic.
        let mut queue_pusher = self.shared_data.writer().get_pusher();
        queue_pusher.push(header);
        for data in data_slice {
            queue_pusher.extend_from_exact_size_iter(data.iter().cloned());
        }

        id.0.reset(None);
        self.shared_data.notify_new_packet_event();

        Ok(AwaitableStatus::new(id.into_inner()))
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// Return [`Error::WriteTooLargeToBeAtomic`] if the write is too large.
    ///
    /// # Cancel Safety
    ///
    /// This function is cancel safe.
    pub async fn send_write_request_direct_atomic(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data: &[u8],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let header = Request::serialize_write_request(
            &mut self.serializer,
            ArenaArc::slot(&id.0),
            handle,
            offset,
            data.len().try_into()?,
        )?
        .split();

        let io_slices = [IoSlice::new(&*header), IoSlice::new(data)];
        let bufs = AtomicWriteIoSlices::new(&io_slices)?;

        id.0.reset(None);
        self.shared_data
            .writer()
            .atomic_write_vectored_all(bufs)
            .await?;

        self.shared_data.notify_new_packet_event();

        Ok(AwaitableStatus::new(id.into_inner()))
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// Return [`Error::WriteTooLargeToBeAtomic`] if the write is too large.
    ///
    /// # Cancel Safety
    ///
    /// This function is cancel safe.
    pub async fn send_write_request_direct_atomic_vectored(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data: &[IoSlice<'_>],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        self.send_write_request_direct_atomic_vectored2(id, handle, offset, &[data])
            .await
    }

    /// Write will extend the file if writing beyond the end of the file.
    ///
    /// It is legal to write way beyond the end of the file, the semantics
    /// are to write zeroes from the end of the file to the specified offset
    /// and then the data.
    ///
    /// On most operating systems, such writes do not allocate disk space but
    /// instead leave "holes" in the file.
    ///
    /// Return [`Error::WriteTooLargeToBeAtomic`] if the write is too large.
    ///
    /// # Cancel Safety
    ///
    /// This function is cancel safe.
    pub async fn send_write_request_direct_atomic_vectored2(
        &mut self,
        id: Id<Buffer>,
        handle: Cow<'_, Handle>,
        offset: u64,
        data: &[&[IoSlice<'_>]],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let data_iter = data.iter().flat_map(Deref::deref);

        let len: usize = data_iter.clone().map(|io_slice| io_slice.len()).sum();
        let len: u32 = len.try_into()?;

        let header = Request::serialize_write_request(
            &mut self.serializer,
            ArenaArc::slot(&id.0),
            handle,
            offset,
            len,
        )?
        .split();

        if data.len() <= 29 {
            let mut io_slices = [IoSlice::new(&[]); 30];

            io_slices[0] = IoSlice::new(&*header);
            io_slices[1..]
                .iter_mut()
                .zip(data_iter)
                .for_each(|(dst, src)| {
                    *dst = *src;
                });

            self.send_write_request_direct_atomic_vectored_impl(id, &io_slices)
                .await
        } else {
            let mut vec = Vec::with_capacity(1 + data.len());
            vec.push(IoSlice::new(&*header));
            for io_slices in data {
                vec.extend_from_slice(io_slices);
            }

            self.send_write_request_direct_atomic_vectored_impl(id, &vec)
                .await
        }
    }

    async fn send_write_request_direct_atomic_vectored_impl(
        &mut self,
        id: Id<Buffer>,
        io_slices: &[IoSlice<'_>],
    ) -> Result<AwaitableStatus<Buffer>, Error> {
        let bufs = AtomicWriteIoSlices::new(io_slices)?;

        id.0.reset(None);
        self.shared_data
            .writer()
            .atomic_write_vectored_all(bufs)
            .await?;

        self.shared_data.notify_new_packet_event();

        Ok(AwaitableStatus::new(id.into_inner()))
    }
}