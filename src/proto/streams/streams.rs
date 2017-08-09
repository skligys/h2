use {client, server};
use proto::*;
use super::*;

use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

// TODO: All the VecDeques should become linked lists using the State
// values.
#[derive(Debug)]
pub struct Streams<B> {
    inner: Arc<Mutex<Inner<B>>>,
}

/// Reference to the stream state
#[derive(Debug)]
pub struct StreamRef<B> {
    inner: Arc<Mutex<Inner<B>>>,
    key: store::Key,
}

#[derive(Debug)]
pub struct Chunk<B>
    where B: Buf,
{
    inner: Arc<Mutex<Inner<B>>>,
    recv: recv::Chunk,
}

/// Fields needed to manage state related to managing the set of streams. This
/// is mostly split out to make ownership happy.
///
/// TODO: better name
#[derive(Debug)]
struct Inner<B> {
    actions: Actions<B>,
    store: Store<B>,
}

#[derive(Debug)]
struct Actions<B> {
    /// Manages state transitions initiated by receiving frames
    recv: Recv<B>,

    /// Manages state transitions initiated by sending frames
    send: Send<B>,
}

impl<B> Streams<B>
    where B: Buf,
{
    pub fn new<P: Peer>(config: Config) -> Self {
        Streams {
            inner: Arc::new(Mutex::new(Inner {
                actions: Actions {
                    recv: Recv::new(&config),
                    send: Send::new::<P>(&config),
                },
                store: Store::new(),
            })),
        }
    }

    /// Process inbound headers
    pub fn recv_headers<P: Peer>(&mut self, frame: frame::Headers)
        -> Result<(), ConnectionError>
    {
        let id = frame.stream_id();
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let key = match me.store.find_entry(id) {
            Entry::Occupied(e) => e.key(),
            Entry::Vacant(e) => {
                // Trailers cannot open a stream. Trailers are header frames
                // that do not contain pseudo headers. Requests MUST contain a
                // method and responses MUST contain a status. If they do not,t
                // hey are considered to be malformed.
                if frame.is_trailers() {
                    return Err(ProtocolError.into());
                }

                match try!(me.actions.recv.open::<P>(id)) {
                    Some(stream) => e.insert(stream),
                    None => return Ok(()),
                }
            }
        };

        let stream = me.store.resolve(key);

        me.actions.transition::<P, _, _>(stream, |actions, stream| {
            if frame.is_trailers() {
                unimplemented!();
                /*
                if !frame.is_end_stream() {
                    // TODO: What error should this return?
                    unimplemented!();
                }

                try!(me.actions.recv.recv_eos(stream));
                */
            } else {
                actions.recv.recv_headers::<P>(frame, stream)
            }
        })
    }

    pub fn recv_data<P: Peer>(&mut self, frame: frame::Data)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let id = frame.stream_id();

        let stream = match me.store.find_mut(&id) {
            Some(stream) => stream,
            None => return Err(ProtocolError.into()),
        };

        me.actions.transition::<P, _, _>(stream, |actions, stream| {
            actions.recv.recv_data(frame, stream)
        })
    }

    pub fn recv_reset<P: Peer>(&mut self, frame: frame::Reset)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let id = frame.stream_id();

        let mut stream = match me.store.find_mut(&id) {
            Some(stream) => stream,
            // TODO: should this be an error?
            None => return Ok(()),
        };

        me.actions.transition::<P, _, _>(stream, |actions, stream| {
            actions.recv.recv_reset(frame, stream)?;
            assert!(stream.state.is_closed());
            Ok(())
        })
    }

    pub fn recv_err(&mut self, err: &ConnectionError) {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let actions = &mut me.actions;
        me.store.for_each(|stream| actions.recv.recv_err(err, stream));
    }

    pub fn recv_window_update(&mut self, frame: frame::WindowUpdate)
        -> Result<(), ConnectionError>
    {
        let id = frame.stream_id();
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        if id.is_zero() {
            try!(me.actions.send.recv_connection_window_update(frame));
        } else {
            // The remote may send window updates for streams that the local now
            // considers closed. It's ok...
            if let Some(mut stream) = me.store.find_mut(&id) {
                try!(me.actions.send.recv_stream_window_update(frame, &mut stream));
            }
        }

        Ok(())
    }

    pub fn recv_push_promise<P: Peer>(&mut self, frame: frame::PushPromise)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let id = frame.stream_id();

        let mut stream = match me.store.find_mut(&id) {
            Some(stream) => stream,
            None => return Err(ProtocolError.into()),
        };

        me.actions.recv.recv_push_promise::<P>(frame, &mut stream)
    }

    pub fn send_headers(&mut self, headers: frame::Headers)
        -> Result<(), ConnectionError>
    {
        unimplemented!();
        /*
        let id = frame.stream_id();
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        // let (id, state) = me.actions.send.open());


        let state = match me.store.entry(id) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(e) => {
                let (id, state) = try!(me.actions.send.open());
                e.insert(state)
            }
        };

        if frame.is_trailers() {
            try!(me.actions.send.send_eos(state));
        } else {
            try!(me.actions.send.send_headers(state, frame.is_end_stream()));
        }

        if state.is_closed() {
            me.actions.dec_num_streams(id);
        }

        Ok(())
        */
    }

    pub fn next_incoming(&mut self) -> Option<StreamRef<B>> {
        let key = {
            let mut me = self.inner.lock().unwrap();
            let me = &mut *me;

            me.actions.recv.next_incoming(&mut me.store)
        };

        key.map(|key| {
            StreamRef {
                inner: self.inner.clone(),
                key,
            }
        })
    }

    pub fn expand_window(&mut self, id: StreamId, sz: WindowSize)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        if id.is_zero() {
            try!(me.actions.recv.expand_connection_window(sz));
        } else {
            if let Some(mut stream) = me.store.find_mut(&id) {
                try!(me.actions.recv.expand_stream_window(id, sz, &mut stream));
            }
        }

        Ok(())
    }

    pub fn send_pending_refusal<T>(&mut self, dst: &mut Codec<T, B>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;
        me.actions.recv.send_pending_refusal(dst)
    }

    pub fn poll_complete<T>(&mut self, dst: &mut Codec<T, B>)
        -> Poll<(), ConnectionError>
        where T: AsyncWrite,
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        me.actions.send.poll_complete(&mut me.store, dst)
    }
}

impl<B> Streams<B>
    where B: Buf,
{
    pub fn send_request(&mut self, request: Request<()>, end_of_stream: bool)
        -> Result<StreamRef<B>, ConnectionError>
    {
        // TODO: There is a hazard with assigning a stream ID before the
        // prioritize layer. If prioritization reorders new streams, this
        // implicitly closes the earlier stream IDs.
        //
        // See: carllerche/h2#11
        let key = {
            let mut me = self.inner.lock().unwrap();
            let me = &mut *me;

            // Initialize a new stream. This fails if the connection is at capacity.
            let mut stream = me.actions.send.open::<client::Peer>()?;

            // Convert the message
            let headers = client::Peer::convert_send_message(
                stream.id, request, end_of_stream);

            let mut stream = me.store.insert(stream.id, stream);

            me.actions.send.send_headers(headers, &mut stream)?;

            // Given that the stream has been initialized, it should not be in the
            // closed state.
            debug_assert!(!stream.state.is_closed());

            stream.key()
        };

        Ok(StreamRef {
            inner: self.inner.clone(),
            key: key,
        })
    }
}

// ===== impl StreamRef =====

impl<B> StreamRef<B>
    where B: Buf,
{
    pub fn send_data<P: Peer>(&mut self, data: B, end_of_stream: bool)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let stream = me.store.resolve(self.key);

        // Create the data frame
        let frame = frame::Data::from_buf(stream.id, data, end_of_stream);

        me.actions.transition::<P, _, _>(stream, |actions, stream| {
            // Send the data frame
            actions.send.send_data(frame, stream)
        })
    }

    /// Called by the server after the stream is accepted. Given that clients
    /// initialize streams by sending HEADERS, the request will always be
    /// available.
    ///
    /// # Panics
    ///
    /// This function panics if the request isn't present.
    pub fn take_request(&self) -> Result<Request<()>, ConnectionError> {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let mut stream = me.store.resolve(self.key);
        me.actions.recv.take_request(&mut stream)
    }

    pub fn send_response(&mut self, response: Response<()>, end_of_stream: bool)
        -> Result<(), ConnectionError>
    {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let stream = me.store.resolve(self.key);

        let frame = server::Peer::convert_send_message(
            stream.id, response, end_of_stream);

        me.actions.transition::<server::Peer, _, _>(stream, |actions, stream| {
            actions.send.send_headers(frame, stream)
        })
    }

    pub fn poll_response(&mut self) -> Poll<Response<()>, ConnectionError> {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        let mut stream = me.store.resolve(self.key);

        me.actions.recv.poll_response(&mut stream)
    }

    pub fn poll_data(&mut self) -> Poll<Option<Chunk<B>>, ConnectionError> {
        let recv = {
            let mut me = self.inner.lock().unwrap();
            let me = &mut *me;

            let mut stream = me.store.resolve(self.key);

            try_ready!(me.actions.recv.poll_chunk(&mut stream))
        };

        // Convert to a chunk
        let chunk = recv.map(|recv| {
            Chunk {
                inner: self.inner.clone(),
                recv: recv,
            }
        });

        Ok(chunk.into())
    }
}

impl<B> Clone for StreamRef<B> {
    fn clone(&self) -> Self {
        StreamRef {
            inner: self.inner.clone(),
            key: self.key.clone(),
        }
    }
}

// ===== impl Chunk =====

impl<B> Chunk<B>
    where B: Buf,
{
    // TODO: Come up w/ a better API
    pub fn pop_bytes(&mut self) -> Option<Bytes> {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        me.actions.recv.pop_bytes(&mut self.recv)
    }
}

impl<B> Drop for Chunk<B>
    where B: Buf,
{
    fn drop(&mut self) {
        let mut me = self.inner.lock().unwrap();
        let me = &mut *me;

        while let Some(_) = me.actions.recv.pop_bytes(&mut self.recv) {
        }
    }
}

// ===== impl Actions =====

impl<B> Actions<B>
    where B: Buf,
{
    fn dec_num_streams<P: Peer>(&mut self, id: StreamId) {
        if self.is_local_init::<P>(id) {
            self.send.dec_num_streams();
        } else {
            self.recv.dec_num_streams();
        }
    }

    fn is_local_init<P: Peer>(&self, id: StreamId) -> bool {
        assert!(!id.is_zero());
        P::is_server() == id.is_server_initiated()
    }

    fn transition<P, F, U>(&mut self, mut stream: store::Ptr<B>, f: F) -> U
        where F: FnOnce(&mut Self, &mut store::Ptr<B>) -> U,
              P: Peer,
    {
        let is_counted = stream.state.is_counted();

        let ret = f(self, &mut stream);

        if is_counted && stream.state.is_closed() {
            self.dec_num_streams::<P>(stream.id);
        }

        ret
    }
}
