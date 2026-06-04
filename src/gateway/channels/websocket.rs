use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::ErrorKind;
use std::net::TcpStream;
use std::time::Duration;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Error as WsError, Message as WsMessage, WebSocket};

pub(in crate::gateway::channels) type ChannelWebSocket = WebSocket<MaybeTlsStream<TcpStream>>;

pub(in crate::gateway::channels) fn set_read_timeout(
    socket: &mut ChannelWebSocket,
    timeout: Duration,
) {
    match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => {
            let _ = stream.set_read_timeout(Some(timeout));
        }
        MaybeTlsStream::Rustls(stream) => {
            let _ = stream.get_ref().set_read_timeout(Some(timeout));
        }
        _ => {}
    }
}

pub(in crate::gateway::channels) fn is_transient_read_error(error: &anyhow::Error) -> bool {
    for cause in error.chain() {
        if let Some(io_error) = cause.downcast_ref::<std::io::Error>() {
            if transient_read_error(io_error.kind()) {
                return true;
            }
        }
        if let Some(WsError::Io(io_error)) = cause.downcast_ref::<WsError>() {
            if transient_read_error(io_error.kind()) {
                return true;
            }
        }
    }

    let text = error.to_string();
    text.contains("WouldBlock")
        || text.contains("timed out")
        || text.contains("Resource temporarily unavailable")
}

pub(in crate::gateway::channels) fn read_json_message(
    socket: &mut ChannelWebSocket,
    read_context: &'static str,
    invalid_json_context: &'static str,
) -> Result<Option<Value>> {
    loop {
        let message = socket.read().context(read_context)?;
        match message {
            WsMessage::Text(text) => {
                return serde_json::from_str(text.as_ref())
                    .map(Some)
                    .context(invalid_json_context);
            }
            WsMessage::Binary(bytes) => {
                return serde_json::from_slice(&bytes)
                    .map(Some)
                    .context(invalid_json_context);
            }
            WsMessage::Ping(payload) => {
                socket
                    .send(WsMessage::Pong(payload))
                    .context("websocket pong failed")?;
            }
            WsMessage::Pong(_) => {}
            WsMessage::Close(frame) => bail!("websocket closed: {frame:?}"),
            _ => {}
        }
    }
}

pub(in crate::gateway::channels) fn send_json_message(
    socket: &mut ChannelWebSocket,
    value: &Value,
    send_context: &'static str,
) -> Result<()> {
    socket
        .send(WsMessage::Text(value.to_string().into()))
        .context(send_context)
}

fn transient_read_error(kind: ErrorKind) -> bool {
    matches!(
        kind,
        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn transient_read_error_detects_tungstenite_io_error_inside_anyhow_context() {
        let error = anyhow::Error::new(WsError::Io(io::Error::from(ErrorKind::WouldBlock)))
            .context("Feishu/Lark websocket read failed");

        assert!(is_transient_read_error(&error));
    }

    #[test]
    fn transient_read_error_detects_plain_io_timeout_inside_anyhow_context() {
        let error = anyhow::Error::new(io::Error::from(ErrorKind::TimedOut))
            .context("websocket read failed");

        assert!(is_transient_read_error(&error));
    }

    #[test]
    fn transient_read_error_detects_macos_resource_temporarily_unavailable_text() {
        let error = anyhow::anyhow!(
            "Feishu/Lark websocket read failed: IO error: Resource temporarily unavailable (os error 35)"
        );

        assert!(is_transient_read_error(&error));
    }

    #[test]
    fn transient_read_error_rejects_connection_reset() {
        let error = anyhow::Error::new(WsError::Io(io::Error::from(ErrorKind::ConnectionReset)))
            .context("websocket read failed");

        assert!(!is_transient_read_error(&error));
    }
}
