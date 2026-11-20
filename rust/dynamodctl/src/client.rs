/// The control socket client: one request, one response.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use dynamod_common::protocol::{self, DecodeError, Message, MessageBody, MessageKind};

pub type Error = Box<dyn std::error::Error>;

/// Send a request to the control socket and return the response body.
pub fn send_request(socket_path: &str, body: MessageBody) -> Result<MessageBody, Error> {
    let mut stream = UnixStream::connect(socket_path)
        .map_err(|e| format!("cannot connect to {socket_path}: {e}"))?;

    stream.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;

    let msg = Message {
        id: 1,
        kind: MessageKind::Request,
        body,
    };

    let data = protocol::encode(&msg)?;
    stream.write_all(&data)?;

    let mut buf = vec![0u8; protocol::HEADER_SIZE + protocol::MAX_MESSAGE_SIZE as usize];
    let mut filled: usize = 0;
    loop {
        let n = stream.read(&mut buf[filled..])?;
        if n == 0 {
            return Err("connection closed before complete message".into());
        }
        filled += n;
        match protocol::decode(&buf[..filled]) {
            Ok((resp, _consumed)) => return Ok(resp.body),
            Err(DecodeError::Incomplete) => continue,
            Err(e) => return Err(e.into()),
        }
    }
}
