#[allow(
    dead_code,
    safe_packed_borrows,
    non_upper_case_globals,
    non_camel_case_types,
    non_snake_case,
    clippy::all
)]
extern crate xpc_connection_sys;

mod message;
pub use message::*;

use block::ConcreteBlock;
use futures::{
    sync::mpsc::{unbounded as unbounded_channel, UnboundedReceiver, UnboundedSender},
    Async, Poll, Stream,
};
use std::ffi::CStr;
#[allow(unused_imports)]
use std::{ffi::c_void, ops::Deref};
use xpc_connection_sys::{
    xpc_connection_cancel, xpc_connection_create_mach_service, xpc_connection_resume,
    xpc_connection_send_message, xpc_connection_set_event_handler, xpc_connection_t, xpc_object_t,
    xpc_release, XPC_CONNECTION_MACH_SERVICE_LISTENER, XPC_CONNECTION_MACH_SERVICE_PRIVILEGED,
};

// A connection's event handler could still be waiting or running when we want
// to drop a connection. We must cancel the handler and wait for the final
// call to a handler to occur, which is always a message containing an
// invalidation error.
fn cancel_and_wait_for_event_handler(connection: xpc_connection_t) {
    let (tx, rx) = std::sync::mpsc::channel();

    let block = ConcreteBlock::new(move |_: xpc_object_t| {
        tx.send(())
            .expect("Failed to announce that the xpc connection's event handler has exited");
    });

    // We must move it from the stack to the heap so that when the libxpc
    // reference count is released we don't double free. This limitation is
    // explained in the blocks crate.
    let block = block.copy();

    unsafe {
        xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);

        xpc_connection_cancel(connection);
    }

    rx.recv()
        .expect("Failed to wait for the xpc connection's event handler to exit");
}

#[derive(Debug)]
pub struct XpcListener {
    connection: xpc_connection_t,
    receiver: UnboundedReceiver<XpcClient>,
    sender: UnboundedSender<XpcClient>,
}

impl PartialEq for XpcListener {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.connection, other.connection)
    }
}

impl Drop for XpcListener {
    fn drop(&mut self) {
        unsafe {
            cancel_and_wait_for_event_handler(self.connection);
            xpc_release(self.connection as xpc_object_t);
        }
    }
}

impl Stream for XpcListener {
    type Item = XpcClient;
    type Error = ();

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        self.receiver.poll()
    }
}

impl XpcListener {
    /// The connection must be a listener.
    fn from_raw(connection: xpc_connection_t) -> XpcListener {
        let (sender, receiver) = unbounded_channel();
        let sender_clone = sender.clone();

        let block = ConcreteBlock::new(move |event| match xpc_object_to_message(event) {
            Message::Client(client) => sender_clone.unbounded_send(client).ok(),
            _ => None,
        });

        // We must move it from the stack to the heap so that when the libxpc
        // reference count is released we don't double free. This limitation is
        // explained in the blocks crate.
        let block = block.copy();

        unsafe {
            xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);
            xpc_connection_resume(connection);
        }

        XpcListener {
            connection,
            receiver,
            sender,
        }
    }

    pub fn listen(name: impl AsRef<CStr>) -> Self {
        let name = name.as_ref();
        let flags = XPC_CONNECTION_MACH_SERVICE_LISTENER as u64;
        let connection = unsafe {
            xpc_connection_create_mach_service(name.as_ref().as_ptr(), std::ptr::null_mut(), flags)
        };
        Self::from_raw(connection)
    }
}

#[derive(Debug)]
pub struct XpcClient {
    connection: xpc_connection_t,
    event_handler_is_running: bool,
    receiver: UnboundedReceiver<Message>,
    sender: UnboundedSender<Message>,
}

unsafe impl Send for XpcClient {}

impl PartialEq for XpcClient {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self.connection, other.connection)
    }
}

impl Drop for XpcClient {
    fn drop(&mut self) {
        if self.event_handler_is_running {
            cancel_and_wait_for_event_handler(self.connection);
        }

        unsafe { xpc_release(self.connection as xpc_object_t) };
    }
}

impl Stream for XpcClient {
    type Item = Message;
    type Error = ();

    /// `Poll::Ready(None)` returned in place of `MessageError::ConnectionInvalid`
    /// as it's not recoverable. `MessageError::ConnectionInterrupted` should
    /// be treated as recoverable.
    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        match self.receiver.poll() {
            Ok(Async::Ready(Some(Message::Error(MessageError::ConnectionInvalid)))) => {
                self.event_handler_is_running = false;
                Ok(Async::Ready(None))
            }
            v => v,
        }
    }
}

impl XpcClient {
    /// This sets up a client connection's event handler so that its `Stream`
    /// implementation can be used.
    fn from_raw(connection: xpc_connection_t) -> Self {
        let (sender, receiver) = unbounded_channel();
        let sender_clone = sender.clone();

        // Handle messages received
        let block = ConcreteBlock::new(move |event| {
            let message = xpc_object_to_message(event);
            sender_clone.unbounded_send(message).ok()
        });

        // We must move it from the stack to the heap so that when the libxpc
        // reference count is released we don't double free. This limitation is
        // explained in the blocks crate.
        let block = block.copy();

        unsafe {
            xpc_connection_set_event_handler(connection, block.deref() as *const _ as *mut _);
            xpc_connection_resume(connection);
        }

        XpcClient {
            connection,
            event_handler_is_running: true,
            receiver,
            sender,
        }
    }

    /// The connection isn't established until the first call to `send_message`.
    pub fn connect(name: impl AsRef<CStr>) -> Self {
        let name = name.as_ref();
        let flags = XPC_CONNECTION_MACH_SERVICE_PRIVILEGED as u64;
        let connection = unsafe {
            xpc_connection_create_mach_service(name.as_ptr(), std::ptr::null_mut(), flags)
        };
        Self::from_raw(connection)
    }

    /// The connection is established on the first call to `send_message`. You
    /// may receive an error relating to an invalid mach port name or a variety
    /// of other issues.
    pub fn send_message(&self, message: Message) {
        let xpc_object = message_to_xpc_object(message);
        unsafe {
            xpc_connection_send_message(self.connection, xpc_object);
            xpc_release(xpc_object);
        }
    }

    #[cfg(feature = "audit_token")]
    pub fn audit_token(&self) -> [u8; 32] {
        // This is a private API, but it's also required in order to
        // authenticate XPC clients without requiring a handshake.
        // See https://developer.apple.com/forums/thread/72881 for more info.
        extern "C" {
            fn xpc_connection_get_audit_token(con: xpc_connection_t, token: *mut c_void);
        }

        let mut token_buffer: [u8; 32] = [0; 32];

        unsafe {
            xpc_connection_get_audit_token(
                self.connection as xpc_connection_t,
                token_buffer.as_mut_ptr() as _,
            )
        }

        token_buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::Stream;
    use std::{collections::HashMap, ffi::CString};
    use tokio::runtime::current_thread::Runtime;
    use xpc_connection_sys::xpc_connection_cancel;

    // This also tests that the event handler block is only freed once, as a
    // double free is possible if the block isn't copied on to the heap.
    #[test]
    fn event_handler_receives_error_on_close() {
        let mach_port_name = CString::new("com.apple.blued").unwrap();
        let client = XpcClient::connect(&mach_port_name);

        // Cancelling the connection will cause the event handler to be called
        // with an error message. This will happen under normal circumstances,
        // for example if the service invalidates the connection.
        unsafe { xpc_connection_cancel(client.connection) };

        let mut rt = Runtime::new().unwrap();
        if let Ok((Some(message), _)) = rt.block_on(client.take(1).into_future()) {
            panic!("Expected `None`, but received {:?}", message);
        }
    }

    #[test]
    fn stream_closed_on_drop() -> Result<(), Box<dyn std::error::Error>> {
        let mach_port_name = CString::new("com.apple.blued")?;
        let client = XpcClient::connect(&mach_port_name);

        let message = Message::Dictionary({
            let mut dictionary = HashMap::new();
            dictionary.insert(CString::new("kCBMsgId")?, Message::Int64(1));
            dictionary.insert(
                CString::new("kCBMsgArgs")?,
                Message::Dictionary({
                    let mut temp = HashMap::new();
                    temp.insert(CString::new("kCBMsgArgAlert")?, Message::Int64(1));
                    temp.insert(
                        CString::new("kCBMsgArgName")?,
                        Message::String(CString::new("rust")?),
                    );
                    temp
                }),
            );
            dictionary
        });

        // Can get data while the channel is open
        client.send_message(message);

        let mut count = 0;

        let mut rt = Runtime::new().unwrap();
        let conn = client.connection;
        let _ = rt.block_on(client.for_each(|msg| {
            match msg {
                Message::Error(error) => {
                    panic!("Error: {:?}", error);
                }
                message => {
                    println!("Received message: {:?}", message);
                    count += 1;

                    // Explained in `event_handler_receives_error_on_close`.
                    unsafe { xpc_connection_cancel(conn) };
                }
            }
            Ok(())
        }));

        Ok(())
    }
}
