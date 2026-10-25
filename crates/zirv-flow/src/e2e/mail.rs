//! Reading a captured message: its headers, its parts and the links in its HTML.

/// The value of the first header called `name` (unfolded), or none.
pub(super) fn header(data: &[u8], name: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data);
    let head = text.split("\r\n\r\n").next()?;
    let mut found: Option<String> = None;
    for line in head.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(value) = found.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if found.is_some() {
            return found;
        }
        if let Some((candidate, value)) = line.split_once(':')
            && candidate.eq_ignore_ascii_case(name)
        {
            found = Some(value.trim().to_owned());
        }
    }
    found
}

/// Decodes quoted-printable text (RFC 2045 6.7).
fn quoted_printable(text: &str) -> String {
    let mut out = Vec::new();
    for line in text.split("\r\n") {
        let (line, soft) = match line.strip_suffix('=') {
            Some(line) => (line, true),
            None => (line, false),
        };
        let bytes = line.as_bytes();
        let mut index = 0;
        while index < bytes.len() {
            let hex = (bytes[index] == b'=')
                .then(|| bytes.get(index + 1..index + 3))
                .flatten()
                .and_then(|pair| std::str::from_utf8(pair).ok())
                .and_then(|pair| u8::from_str_radix(pair, 16).ok());
            match hex {
                Some(byte) => {
                    out.push(byte);
                    index += 3;
                }
                None => {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
        }
        if !soft {
            out.extend_from_slice(b"\r\n");
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The decoded body of the part of `data` whose `Content-Type` starts with `content_type`.
pub(super) fn part(data: &[u8], content_type: &str) -> Option<String> {
    let text = String::from_utf8_lossy(data).into_owned();
    let boundary = header(data, "Content-Type")?
        .split("boundary=")
        .nth(1)?
        .trim_matches(|c| c == '"' || c == ';' || c == ' ')
        .to_owned();
    let body = text.split_once("\r\n\r\n")?.1;
    for piece in body.split(&format!("--{boundary}")) {
        let Some((head, content)) = piece.trim_start_matches("\r\n").split_once("\r\n\r\n") else {
            continue;
        };
        if head
            .to_ascii_lowercase()
            .contains(&format!("content-type: {content_type}"))
        {
            return Some(quoted_printable(content).trim_end().to_owned());
        }
    }
    None
}

/// Every `http` or `https` URL in `html` that contains `needle`, in order.
pub(super) fn urls_containing(html: &str, needle: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("http") {
        let tail = &rest[start..];
        let end = tail
            .find(|c: char| matches!(c, '"' | '\'' | '<' | '>') || c.is_whitespace())
            .unwrap_or(tail.len());
        if tail.starts_with("http://") || tail.starts_with("https://") {
            let url = &tail[..end];
            if url.contains(needle) {
                found.push(url.to_owned());
            }
        }
        rest = &tail[end.max(1)..];
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    const MESSAGE: &[u8] = b"Subject: Hi\r\nList-Unsubscribe: <https://x.test/u/abc>,\r\n <mailto:u@x.test>\r\nContent-Type: multipart/alternative; boundary=\"b1\"\r\n\r\n--b1\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nplain=20text\r\n--b1\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\n<a href=3D\"https://x.test/t/c/tok\">a=\r\nb</a>\r\n--b1--\r\n";

    #[test]
    fn headers_unfold_and_parts_decode() {
        assert_eq!(header(MESSAGE, "subject").as_deref(), Some("Hi"));
        assert_eq!(
            header(MESSAGE, "List-Unsubscribe").as_deref(),
            Some("<https://x.test/u/abc>, <mailto:u@x.test>")
        );
        assert_eq!(part(MESSAGE, "text/plain").as_deref(), Some("plain text"));
        let html = part(MESSAGE, "text/html").expect("an html part");
        assert!(html.starts_with("<a href=\"https://x.test/t/c/tok\">ab</a>"));
        assert_eq!(
            urls_containing(&html, "/t/c/"),
            ["https://x.test/t/c/tok".to_owned()]
        );
    }
}
