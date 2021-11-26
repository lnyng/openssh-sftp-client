mod read_end;
mod threadsafe_waker;
mod write_end;

#[derive(Debug)]
pub struct Client {
    write_end: write_end::WriteEnd,
    read_end: read_end::ReadEnd,
}
