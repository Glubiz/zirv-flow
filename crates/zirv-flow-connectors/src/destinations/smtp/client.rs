//! The wire half of the SMTP client (RFC 5321): command lines out, replies in. Every read is
//! bounded in line length, reply lines and total reply size, and every step has a timeout, so a
//! server that talks forever, never ends a line or never answers cannot hold a worker.

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// How long a command may take to be answered.
pub(super) const STEP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the message body may take to be accepted: the server may scan it before it answers.
pub(super) const DATA_TIMEOUT: Duration = Duration::from_secs(120);
/// How long the goodbye may take; its answer does not matter.
pub(super) const QUIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The longest reply line, with its line break. RFC 5321 allows 512; servers add a little.
const MAX_LINE: usize = 1024;
/// The most lines in one reply.
const MAX_REPLY_LINES: usize = 64;
/// The most bytes in one reply.
const MAX_REPLY_BYTES: usize = 16 * 1024;

/// Why the wire failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WireError {
    /// A step took longer than its limit.
    Timeout,
    /// The connection broke or closed.
    Io,
    /// The server sent something that is not a reply, or more than the bounds allow.
    Protocol,
    /// The end of the message was sent, and the connection was lost, the answer never came or it
    /// could not be read: the server may have accepted the message.
    Unconfirmed,
}

/// An enhanced status code (RFC 3463), such as `5.1.1`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Enhanced {
    pub(super) class: u8,
    pub(super) subject: u16,
    pub(super) detail: u16,
}

/// One complete reply, multi-line replies joined.
pub(super) struct Reply {
    pub(super) code: u16,
    pub(super) enhanced: Option<Enhanced>,
    /// The text of each line, without the code. It can name addresses: never log it.
    pub(super) lines: Vec<String>,
}

impl std::fmt::Debug for Reply {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Reply({}, {} lines, [text redacted])",
            self.code,
            self.lines.len()
        )
    }
}

/// A connection and what has been read from it and not yet used.
pub(super) struct Session<S> {
    io: S,
    buffer: Vec<u8>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Session<S> {
    pub(super) fn new(io: S) -> Self {
        Self {
            io,
            buffer: Vec::new(),
        }
    }

    /// Whether bytes arrived that no reply has consumed. After the answer to `STARTTLS` there must
    /// be none: they would have been sent before the handshake and could be injected replies.
    pub(super) fn has_buffered(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub(super) fn into_inner(self) -> S {
        self.io
    }

    /// Writes `line` and a line break, then reads the reply, all within `limit`.
    pub(super) async fn command(
        &mut self,
        line: &str,
        limit: Duration,
    ) -> Result<Reply, WireError> {
        let mut bytes = Vec::with_capacity(line.len() + 2);
        bytes.extend_from_slice(line.as_bytes());
        bytes.extend_from_slice(b"\r\n");
        self.exchange(&bytes, limit).await
    }

    /// Writes `bytes` as they are, then reads the reply, all within `limit`.
    pub(super) async fn exchange(
        &mut self,
        bytes: &[u8],
        limit: Duration,
    ) -> Result<Reply, WireError> {
        tokio::time::timeout(limit, async {
            self.io.write_all(bytes).await.map_err(|_| WireError::Io)?;
            self.io.flush().await.map_err(|_| WireError::Io)?;
            self.read_reply().await
        })
        .await
        .map_err(|_| WireError::Timeout)?
    }

    /// Writes the message and its end marker as they are, then reads the reply, all within
    /// `limit`. A failure once everything was written is [`WireError::Unconfirmed`]: the server
    /// has seen the end of the message and may have accepted it. A failure while writing is a
    /// plain one, since a server that did not see the end marker cannot have accepted the message.
    pub(super) async fn exchange_final(
        &mut self,
        bytes: &[u8],
        limit: Duration,
    ) -> Result<Reply, WireError> {
        let mut written = false;
        let result = tokio::time::timeout(limit, async {
            self.io.write_all(bytes).await.map_err(|_| WireError::Io)?;
            self.io.flush().await.map_err(|_| WireError::Io)?;
            written = true;
            self.read_reply().await
        })
        .await;
        match result {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) | Err(_) if written => Err(WireError::Unconfirmed),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(WireError::Timeout),
        }
    }

    /// Reads one reply within `limit`, such as the greeting.
    pub(super) async fn reply(&mut self, limit: Duration) -> Result<Reply, WireError> {
        tokio::time::timeout(limit, self.read_reply())
            .await
            .map_err(|_| WireError::Timeout)?
    }

    async fn read_reply(&mut self) -> Result<Reply, WireError> {
        let mut code: Option<u16> = None;
        let mut lines = Vec::new();
        let mut total = 0;
        loop {
            let line = self.read_line().await?;
            total += line.len() + 2;
            if lines.len() >= MAX_REPLY_LINES || total > MAX_REPLY_BYTES {
                return Err(WireError::Protocol);
            }
            let (this, last, text) = parse_line(&line)?;
            if code.is_some_and(|first| first != this) {
                return Err(WireError::Protocol);
            }
            code = Some(this);
            lines.push(text);
            if last {
                break;
            }
        }
        let code = code.ok_or(WireError::Protocol)?;
        let enhanced = lines.first().and_then(|text| parse_enhanced(text, code));
        Ok(Reply {
            code,
            enhanced,
            lines,
        })
    }

    /// The next line, without its line break, from the buffer or the connection.
    async fn read_line(&mut self) -> Result<Vec<u8>, WireError> {
        loop {
            if let Some(end) = self.buffer.iter().position(|byte| *byte == b'\n') {
                if end + 1 > MAX_LINE {
                    return Err(WireError::Protocol);
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if self.buffer.len() >= MAX_LINE {
                return Err(WireError::Protocol);
            }
            let mut chunk = [0u8; 1024];
            let read = self.io.read(&mut chunk).await.map_err(|_| WireError::Io)?;
            if read == 0 {
                return Err(WireError::Io);
            }
            self.buffer.extend_from_slice(&chunk[..read]);
        }
    }
}

/// A reply line as the code, whether it ends the reply, and the text after the code.
fn parse_line(line: &[u8]) -> Result<(u16, bool, String), WireError> {
    if line.len() < 3 || !line[..3].iter().all(u8::is_ascii_digit) {
        return Err(WireError::Protocol);
    }
    let code = line[..3]
        .iter()
        .fold(0u16, |value, digit| value * 10 + u16::from(digit - b'0'));
    if !(200..=599).contains(&code) {
        return Err(WireError::Protocol);
    }
    let (last, text) = match line.get(3) {
        None => (true, &[][..]),
        Some(b' ') => (true, &line[4..]),
        Some(b'-') => (false, &line[4..]),
        Some(_) => return Err(WireError::Protocol),
    };
    Ok((code, last, String::from_utf8_lossy(text).into_owned()))
}

/// The enhanced status code at the start of a reply's text, when it has one whose class agrees
/// with the reply code (a server that sends `5.x.x` on a 4xx reply is not believed).
fn parse_enhanced(text: &str, code: u16) -> Option<Enhanced> {
    let token = text.split_whitespace().next()?;
    let mut parts = token.split('.');
    let class: u8 = parts.next()?.parse().ok()?;
    let subject: u16 = parts.next()?.parse().ok()?;
    let detail: u16 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !matches!(class, 2 | 4 | 5) || u16::from(class) != code / 100 {
        return None;
    }
    if subject > 999 || detail > 999 {
        return None;
    }
    Some(Enhanced {
        class,
        subject,
        detail,
    })
}

/// The message as `DATA` sends it: lines that start with a dot get a second dot, and the end
/// marker follows. `message` ends in a line break.
pub(super) fn dot_stuffed(message: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(message.len() + 16);
    let mut line_start = true;
    for byte in message {
        if line_start && *byte == b'.' {
            out.push(b'.');
        }
        out.push(*byte);
        line_start = *byte == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b".\r\n");
    out
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::*;

    async fn session_with(script: &'static [u8]) -> Session<tokio::io::DuplexStream> {
        let (client, mut server) = duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = server.write_all(script).await;
            // Keep the server end open so a reader sees a stall, not a close.
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        Session::new(client)
    }

    #[tokio::test]
    async fn a_multi_line_reply_is_joined_and_its_enhanced_code_read() {
        let mut session = session_with(
            b"250-mail.test\r\n250-SIZE 1000\r\n250 2.0.0 ok\r\n550 5.1.1 no such user\r\n",
        )
        .await;

        let first = session.reply(Duration::from_secs(1)).await.unwrap();
        let second = session.reply(Duration::from_secs(1)).await.unwrap();

        assert_eq!(first.code, 250);
        assert_eq!(first.lines, ["mail.test", "SIZE 1000", "2.0.0 ok"]);
        assert_eq!(first.enhanced, None, "only the first line is read for it");
        assert_eq!(second.code, 550);
        assert_eq!(
            second.enhanced,
            Some(Enhanced {
                class: 5,
                subject: 1,
                detail: 1
            })
        );
        assert!(!format!("{second:?}").contains("such user"));
    }

    #[tokio::test]
    async fn a_reply_that_is_not_one_or_exceeds_a_bound_is_a_protocol_error() {
        let too_long = Box::leak(
            format!("250 {}\r\n", "a".repeat(2000))
                .into_bytes()
                .into_boxed_slice(),
        );
        let many = Box::leak("250-x\r\n".repeat(70).into_bytes().into_boxed_slice());
        let big_total = Box::leak(
            format!(
                "{}250 end\r\n",
                format!("250-{}\r\n", "b".repeat(500)).repeat(40)
            )
            .into_bytes()
            .into_boxed_slice(),
        );
        let cases: [&'static [u8]; 7] = [
            b"hello\r\n",
            b"25\r\n",
            b"250x text\r\n",
            b"199 low\r\n",
            b"250-a\r\n251 b\r\n",
            too_long,
            many,
        ];
        for script in cases {
            let mut session = session_with(script).await;

            let result = session.reply(Duration::from_secs(1)).await;

            assert_eq!(result.err(), Some(WireError::Protocol), "{script:?}");
        }
        let mut session = session_with(big_total).await;
        assert_eq!(
            session.reply(Duration::from_secs(1)).await.err(),
            Some(WireError::Protocol)
        );
    }

    #[tokio::test]
    async fn a_line_that_never_ends_a_silent_server_and_a_closed_connection_end_the_step() {
        let endless = Box::leak("2".repeat(5000).into_bytes().into_boxed_slice());
        let mut session = session_with(endless).await;
        assert_eq!(
            session.reply(Duration::from_secs(1)).await.err(),
            Some(WireError::Protocol)
        );

        let mut silent = session_with(b"250-never finished\r\n").await;
        assert_eq!(
            silent.reply(Duration::from_millis(50)).await.err(),
            Some(WireError::Timeout)
        );

        let (client, server) = duplex(1024);
        drop(server);
        let mut closed = Session::new(client);
        assert_eq!(
            closed.reply(Duration::from_secs(1)).await.err(),
            Some(WireError::Io)
        );
    }

    #[tokio::test]
    async fn a_failure_after_the_whole_message_was_written_is_unconfirmed_and_before_it_is_not() {
        // The server reads everything and then closes without answering.
        let (client, mut server) = duplex(64 * 1024);
        let mut session = Session::new(client);
        let reader = tokio::spawn(async move {
            let mut sink = vec![0u8; 16];
            let _ = tokio::io::AsyncReadExt::read(&mut server, &mut sink).await;
        });
        let closed = session
            .exchange_final(b"x\r\n.\r\n", Duration::from_secs(1))
            .await;
        reader.await.unwrap();
        assert_eq!(closed.err(), Some(WireError::Unconfirmed));

        // The server takes the message and never answers.
        let (client, mut server) = duplex(64 * 1024);
        let mut session = Session::new(client);
        let silent = session
            .exchange_final(b"x\r\n.\r\n", Duration::from_millis(50))
            .await;
        assert_eq!(silent.err(), Some(WireError::Unconfirmed));
        let _ = server.shutdown().await;

        // The server's buffer is full and never read: the end marker was not written.
        let (client, _server) = duplex(8);
        let mut session = Session::new(client);
        let stuck = session
            .exchange_final(&[b'x'; 1024], Duration::from_millis(50))
            .await;
        assert_eq!(stuck.err(), Some(WireError::Timeout));
    }

    #[test]
    fn an_enhanced_code_must_agree_with_the_reply_class() {
        assert!(parse_enhanced("5.1.1 gone", 550).is_some());
        assert!(parse_enhanced("2.0.0 ok", 250).is_some());
        assert_eq!(parse_enhanced("5.1.1 gone", 450), None);
        assert_eq!(parse_enhanced("hello", 550), None);
        assert_eq!(parse_enhanced("5.1 gone", 550), None);
        assert_eq!(parse_enhanced("5.1.1.1 gone", 550), None);
    }

    #[test]
    fn dot_stuffing_doubles_a_leading_dot_and_ends_with_the_marker() {
        assert_eq!(
            dot_stuffed(b"a\r\n.b\r\n..c\r\n"),
            b"a\r\n..b\r\n...c\r\n.\r\n".to_vec()
        );
        assert_eq!(dot_stuffed(b".x\r\n"), b"..x\r\n.\r\n".to_vec());
        assert_eq!(dot_stuffed(b"no break"), b"no break\r\n.\r\n".to_vec());
    }
}
