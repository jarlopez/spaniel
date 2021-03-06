use connection::ConnectionError;
use connection::SharedConnectionContext;
use flow_control::Credits;
use flow_control::FC_DENOMINATOR;
use flow_control::FC_NUMERATOR;
use futures;
use futures::sync::mpsc::Receiver;
use futures::task::{self, Task};
use futures::Async;
use futures::Poll;
use protocol::frames;
use protocol::frames::Frame;
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy, Hash, Ord, PartialOrd, Eq, PartialEq)]
pub struct StreamId(pub u32);

impl StreamId {
    pub const ZERO: StreamId = StreamId(0);

    pub fn new(id: u32) -> Self {
        StreamId(id)
    }
}

impl From<StreamId> for u32 {
    fn from(it: StreamId) -> u32 {
        it.0
    }
}

impl From<StreamId> for usize {
    fn from(it: StreamId) -> usize {
        it.0 as usize
    }
}

impl From<u32> for StreamId {
    fn from(it: u32) -> Self {
        StreamId(it)
    }
}

/// Data structure tracking an individual stream
#[derive(Debug)]
pub struct StreamState {
    pub credits: Credits,
    pub data_buffer: VecDeque<frames::Data>,
    pub data: Receiver<frames::Frame>,
    // Task waiting to be able to send data
    pub send_task: Option<Task>,
    // Task waiting to receive data from `data_buffer`
    pub recv_task: Option<Task>,
}

impl StreamState {
    pub fn notify_data_rx(&mut self) {
        if let Some(task) = self.recv_task.take() {
            task.notify();
        }
    }
    pub fn notify_data_tx(&mut self) {
        if let Some(task) = self.send_task.take() {
            task.notify();
        }
    }
}

pub struct IncomingStreams {
    ctx: SharedConnectionContext,
}

impl IncomingStreams {
    pub fn new(ctx: SharedConnectionContext) -> Self {
        IncomingStreams { ctx }
    }
}

pub struct StreamRequester {
    pub stream_id: StreamId,
    pub credit: u32,
    pub ctx: SharedConnectionContext,
}

pub struct StreamRef {
    stream_id: StreamId,
    ctx: SharedConnectionContext,
}

impl StreamRef {
    pub fn clone_ctx(&self) -> SharedConnectionContext {
        self.ctx.clone()
    }

    pub fn send_frame(&mut self, frame: Frame) -> Result<(), ConnectionError> {
        let mut ctx = self.ctx.lock().unwrap();
        let ctx = &mut *ctx;
        ctx.send_frame(frame)
    }

    pub fn stream_id(&self) -> StreamId {
        self.stream_id
    }

    // TODO errors
    // TODO expose configurable credit update strategy
    pub fn return_credit(&mut self, credit: u32) -> Result<(), ()> {
        let mut ctx = self.ctx.lock().unwrap();
        let ctx = &mut *ctx;

        let credit_update: Option<frames::Frame> = {
            let stream = match ctx.get_stream_state_mut(&self.stream_id) {
                None => return Err(()),
                Some(state) => state,
            };

            let initial = stream.credits.available();
            let available = stream.credits.add_credit(credit);
            let capacity = stream.credits.capacity();
            let thr = (capacity * FC_NUMERATOR / FC_DENOMINATOR) as u32;

            let unannounced_credits = available - initial;
            let past_threshold = available >= thr;

            if past_threshold {
                // Only send incremental updates
                let credit_update = frames::Frame::CreditUpdate(frames::CreditUpdate {
                    stream_id: self.stream_id,
                    credit: unannounced_credits,
                });
                Some(credit_update)
            } else {
                None
            }
        };
        credit_update.map(|frame| {
            ctx.send_frame(frame).map_err(|err| {
                println!("Could not send credit frame!! {:?}", err);
                // TODO handle
            })
        });
        Ok(())
    }
}

impl Clone for StreamRef {
    fn clone(&self) -> Self {
        StreamRef {
            stream_id: self.stream_id,
            ctx: self.ctx.clone(),
        }
    }
}

impl futures::Stream for IncomingStreams {
    type Item = StreamRef;
    type Error = (); // TODO

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let stream_id = {
            let mut ctx = self.ctx.lock().unwrap();
            let ctx = &mut *ctx;

            if ctx.has_err() {
                return Ok(Async::Ready(None));
            }

            if let Some(ev) = ctx.next_stream() {
                ev.stream_id
            } else {
                ctx.new_stream_task = Some(task::current());
                return Ok(Async::NotReady);
            }
        };
        let stream = StreamRef {
            stream_id,
            ctx: self.ctx.clone(),
        };
        Ok(Async::Ready(Some(stream)))
    }
}

impl futures::Stream for StreamRef {
    type Item = frames::Frame;
    type Error = (); // TOODO

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut ctx = self.ctx.lock().unwrap();
        let ctx = &mut *ctx;

        let me = {
            match ctx.get_stream_state_mut(&self.stream_id) {
                None => return Err(()),
                Some(stream_state) => stream_state,
            }
        };
        me.data.poll().map_err(|why| {
            println!("Error polling for data; {:?}", why);
        })
    }
}

impl futures::Future for StreamRequester {
    type Item = StreamRef;
    type Error = ();

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        {
            let mut ctx = self.ctx.lock().unwrap();
            let ctx = &mut *ctx;

            match ctx.get_stream_state_mut(&self.stream_id) {
                Some(_) => return Err(()), // TODO StreamAlreadyExists
                None => (),
            };
            let (tx, rx) = futures::sync::mpsc::channel(1);
            let state = StreamState {
                data_buffer: VecDeque::new(),
                credits: Credits::new(self.credit),
                send_task: None,
                recv_task: None,
                data: rx,
            };
            ctx.stream_senders.insert(self.stream_id, tx);
            ctx.stream_states.insert(self.stream_id, state);
            let sr = frames::StreamRequest::new(self.stream_id, self.credit);

            // TODO this should really be driven by the ConnectionDriver's IoHandle to get appropriate
            // TODO feedback on success :-\
            ctx.send_frame(frames::Frame::StreamRequest(sr))
                .map_err(|_| ())?;
        }

        let stream = StreamRef {
            stream_id: self.stream_id,
            ctx: self.ctx.clone(),
        };

        // Hand off ownership of this stream
        Ok(Async::Ready(stream))
    }
}
