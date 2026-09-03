//! Minimal HTTP/1.1 client with Chrome's exact header order (fallback for
//! origins without h2).

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::error::FetchError;

pub struct H1Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Hard cap on an HTTP/1.1 response body (matches the
/// decompression cap : bombs must fail before they allocate).
const MAX_BODY: usize = 64 << 20;

/// Generic over any async stream : works for both TLS
/// (`SslStream<TcpStream>`) and raw plaintext `TcpStream`
/// (the http:// path).
pub async fn get<S>(
    stream: &mut S,
    path: &str,
    headers: &[(String, String)],
) -> Result<H1Response, FetchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // Header values are synthesized partly from response data
    // (cookies). A CR/LF/NUL inside one would split the request
    // on the wire : refuse to send instead.
    for (n, v) in headers {
        if !crate::fetch::guards::valid_header_value(n)
            || !crate::fetch::guards::valid_header_value(v)
        {
            return Err(FetchError::Http(
                "h1: invalid header value (CR/LF/NUL) : refused to send".into(),
            ));
        }
    }
    let mut req = format!("GET {path} HTTP/1.1\r\n");
    for (n, v) in headers {
        req.push_str(&format!("{n}: {v}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read until end of header block.
    let mut buf: Vec<u8> = Vec::with_capacity(16384);
    let mut tmp = [0u8; 16384];
    let header_end;
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            return Err(FetchError::Http("h1: eof before headers".into()));
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            header_end = pos + 4;
            break;
        }
        if buf.len() > 1 << 20 {
            return Err(FetchError::Http("h1: header block too large".into()));
        }
    }

    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut lines = head.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| FetchError::Http(format!("h1: bad status line: {status_line}")))?;
    let mut headers_out = Vec::new();
    for line in lines {
        if let Some((n, v)) = line.split_once(':') {
            headers_out.push((n.trim().to_ascii_lowercase(), v.trim().to_string()));
        }
    }

    let mut body = buf[header_end..].to_vec();
    let is_chunked = headers_out
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"));
    let content_len: Option<usize> = headers_out
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok());

    if is_chunked {
        body = read_chunked(stream, body).await?;
    } else if let Some(cl) = content_len {
        // A lying Content-Length must not turn into a giant alloc.
        if cl > MAX_BODY {
            return Err(FetchError::Http(format!(
                "h1: content-length {cl} exceeds body cap"
            )));
        }
        while body.len() < cl {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(cl);
    } else {
        // Read to close : still capped.
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
            if body.len() > MAX_BODY {
                return Err(FetchError::Http("h1: body exceeds cap".into()));
            }
        }
    }

    Ok(H2Placeholder::into_h1(status, headers_out, body))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

struct H2Placeholder;
impl H2Placeholder {
    fn into_h1(status: u16, headers: Vec<(String, String)>, body: Vec<u8>) -> H1Response {
        H1Response {
            status,
            headers,
            body,
        }
    }
}

/// Decode chunked transfer coding from `prefix` (already-read bytes) + stream.
async fn read_chunked<S>(stream: &mut S, prefix: Vec<u8>) -> Result<Vec<u8>, FetchError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut raw = prefix;
    let mut tmp = [0u8; 16384];
    let mut out = Vec::new();
    loop {
        // Ensure we have a size line.
        let line_end = loop {
            if let Some(pos) = find(&raw, b"\r\n") {
                break pos;
            }
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(FetchError::Http("h1: eof in chunk size".into()));
            }
            raw.extend_from_slice(&tmp[..n]);
        };
        let size_str = String::from_utf8_lossy(&raw[..line_end]);
        let size = usize::from_str_radix(size_str.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| FetchError::Http(format!("h1: bad chunk size: {size_str}")))?;
        if size > MAX_BODY {
            return Err(FetchError::Http("h1: chunk size exceeds cap".into()));
        }
        let mut rest = raw.split_off(line_end + 2);
        if size == 0 {
            // Trailer section ends with empty line.
            while !rest.starts_with(b"\r\n") {
                if let Some(pos) = find(&rest, b"\r\n\r\n") {
                    rest.truncate(pos + 4);
                    break;
                }
                let n = stream.read(&mut tmp).await?;
                if n == 0 {
                    break;
                }
                rest.extend_from_slice(&tmp[..n]);
            }
            break;
        }
        while rest.len() < size + 2 {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                return Err(FetchError::Http("h1: eof in chunk data".into()));
            }
            rest.extend_from_slice(&tmp[..n]);
        }
        if out.len() + size > MAX_BODY {
            return Err(FetchError::Http("h1: chunked body exceeds cap".into()));
        }
        out.extend_from_slice(&rest[..size]);
        raw = rest.split_off(size + 2);
    }
    Ok(out)
}
