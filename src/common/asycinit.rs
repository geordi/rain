
use errors::Error;
use futures::{IntoFuture, unsync, Future};

enum State<T> {
    // Object is still in initialization, vector contains callbacks when
    // object is ready
    Initing(Vec<unsync::oneshot::Sender<()>>),

    // Value is ready
    Ready(T)
}

pub struct AsyncInitWrapper<T> {
    state: State<T>
}


impl<T> AsyncInitWrapper<T> {

    pub fn new() -> Self {
        Self { state: State::Initing(Vec::new()) }
    }

    pub fn is_ready(&self) -> bool {
        match self.state {
            State::Initing(_) => false,
            State::Ready(_) => true
        }
    }

    pub fn get(&self) -> &T {
        match self.state {
            State::Ready(ref value) => &value,
            State::Initing(_) => panic!("Element is not ready")
        }
    }

    pub fn set_value(&mut self, value: T) {
        match ::std::mem::replace(&mut self.state, State::Ready(value)) {
            State::Initing(senders) => {
                for sender in senders {
                    sender.send(()).unwrap();
                }
            }
            State::Ready(_) => panic!("Element is already finished"),
        }
    }

    pub fn wait(&mut self) -> Box<Future<Item=(), Error=Error>> {
        match self.state {
            State::Ready(ref value) => Box::new(Ok(()).into_future()),
            State::Initing(ref mut senders) => {
                let (sender, receiver) = unsync::oneshot::channel();
                senders.push(sender);
                // TODO: Convert to testable error
                Box::new(receiver.map_err(|e| "Cancelled".into()))
            }
        }
    }

}