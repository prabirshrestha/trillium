use futures_lite::io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use http_types::headers::{CONTENT_TYPE, HOST, UPGRADE};
use http_types::{
    content::ContentLength,
    headers::{DATE, EXPECT, TRANSFER_ENCODING},
    other::Date,
    transfer::{Encoding, TransferEncoding},
    Body, Extensions, Headers, Method, StatusCode, Url, Version,
};
use memmem::{Searcher, TwoWaySearcher};
use std::future::Future;

use std::{
    convert::TryInto,
    fmt::{self, Debug, Formatter},
};

use crate::{
    body_encoder::BodyEncoder, request_body::RequestBodyState, Error, RequestBody, Result, Upgrade,
};

const MAX_HEADERS: usize = 128;
const MAX_HEAD_LENGTH: usize = 8 * 1024;

#[derive(Debug)]
pub enum ConnectionStatus<RW> {
    Close,
    Conn(Conn<RW>),
    Upgrade(Upgrade<RW>),
}

pub struct Conn<RW> {
    pub(crate) request_headers: Headers,
    response_headers: Headers,
    pub(crate) path: String,
    pub(crate) method: Method,
    pub(crate) status: Option<StatusCode>,
    version: Version,
    pub(crate) state: Extensions,
    response_body: Option<Body>,
    pub(crate) rw: RW,
    pub(crate) buffer: Option<Vec<u8>>,
    pub(crate) request_body_state: RequestBodyState,
    secure: bool,
}

impl<RW> Debug for Conn<RW> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("Conn")
            .field("request_headers", &self.request_headers)
            .field("response_headers", &self.response_headers)
            .field("path", &self.path)
            .field("method", &self.method)
            .field("status", &self.status)
            .field("version", &self.version)
            .field("request_body_state", &self.request_body_state)
            .finish()
    }
}

impl<RW> Conn<RW>
where
    RW: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
{
    pub fn state(&self) -> &Extensions {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut Extensions {
        &mut self.state
    }

    pub fn request_headers(&self) -> &Headers {
        &self.request_headers
    }

    pub fn response_headers(&mut self) -> &mut Headers {
        &mut self.response_headers
    }

    pub fn set_status(&mut self, status: impl TryInto<http_types::StatusCode>) {
        self.status = status.try_into().ok();
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn host(&self) -> Option<String> {
        self.request_headers.get(HOST).map(|v| v.to_string())
    }

    pub fn url(&self) -> Result<Url> {
        let path = self.path();
        let host = self.host().unwrap_or_else(|| String::from("_"));
        let method = self.method();
        if path.starts_with("http://") || path.starts_with("https://") {
            Ok(Url::parse(path)?)
        } else if path.starts_with('/') {
            Ok(Url::parse(&format!("http://{}{}", host, path))?)
        } else if method == &Method::Connect {
            Ok(Url::parse(&format!("http://{}/", path))?)
        } else {
            Err(Error::UnexpectedURIFormat)
        }
    }

    pub fn set_body(&mut self, body: impl Into<Body>) {
        let body = body.into();

        if self.response_headers.get(CONTENT_TYPE).is_none() {
            self.response_headers
                .insert(CONTENT_TYPE, body.mime().clone());
        }

        self.response_body = Some(body);
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    pub fn status(&self) -> Option<&StatusCode> {
        self.status.as_ref()
    }

    pub fn response_body(&self) -> Option<&Body> {
        self.response_body.as_ref()
    }

    fn needs_100_continue(&self) -> bool {
        self.request_headers
            .contains_ignore_ascii_case(EXPECT, "100-continue")
    }

    pub async fn request_body<'a>(&'a mut self) -> RequestBody<'a, RW> {
        self.initialize_request_body_state().await.ok();
        RequestBody::new(self)
    }

    pub async fn map<F, Fut>(rw: RW, f: &F) -> crate::Result<Option<Upgrade<RW>>>
    where
        F: Fn(Conn<RW>) -> Fut,
        Fut: Future<Output = Conn<RW>> + Send,
    {
        let mut conn = Conn::new(rw, None).await?;

        loop {
            conn = match f(conn).await.encode().await? {
                ConnectionStatus::Upgrade(upgrade) => return Ok(Some(upgrade)),
                ConnectionStatus::Close => return Ok(None),
                ConnectionStatus::Conn(next) => next,
            }
        }
    }

    pub async fn new(rw: RW, bytes: Option<Vec<u8>>) -> Result<Self> {
        let (rw, buf, extra_bytes) = Self::head(rw, bytes).await?;
        let buffer = if extra_bytes.is_empty() {
            None
        } else {
            Some(extra_bytes)
        };
        let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
        let mut httparse_req = httparse::Request::new(&mut headers);
        let status = httparse_req.parse(&buf[..])?;
        if status.is_partial() {
            return Err(Error::PartialHead);
        }

        let method = httparse_req
            .method
            .ok_or(Error::MissingMethod)?
            .parse()
            .map_err(|_| Error::UnrecognizedMethod(httparse_req.method.unwrap().to_string()))?;

        let version = match httparse_req.version {
            Some(1) => Version::Http1_1,
            Some(version) => return Err(Error::UnsupportedVersion(version)),
            None => return Err(Error::MissingVersion),
        };

        let mut request_headers = Headers::new();
        for header in httparse_req.headers.iter() {
            request_headers.insert(header.name, std::str::from_utf8(header.value)?);
        }

        log::trace!("parsed headers: {:#?}", &request_headers);
        let path = httparse_req
            .path
            .ok_or(Error::RequestPathMissing)?
            .to_owned();

        Ok(Self {
            rw,
            request_headers,
            method,
            version,
            path,
            buffer,
            response_headers: Headers::new(),
            status: None,
            state: Extensions::new(),
            response_body: None,
            request_body_state: RequestBodyState::Start,
            secure: false,
        })
    }

    pub fn is_secure(&self) -> bool {
        self.secure
    }

    async fn send_100_continue(&mut self) -> Result<()> {
        log::trace!("sending 100-continue");
        Ok(self.rw.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?)
    }

    async fn head(mut rw: RW, bytes: Option<Vec<u8>>) -> Result<(RW, Vec<u8>, Vec<u8>)> {
        let mut buf = bytes.unwrap_or_default();
        let mut len = 0;

        let searcher = TwoWaySearcher::new(b"\r\n\r\n");
        loop {
            buf.extend(std::iter::repeat(0).take(100));
            let bytes = rw.read(&mut buf[len..]).await?;
            let search = searcher.search_in(&buf[len..]);

            if let Some(index) = search {
                buf.truncate(len + bytes);
                log::trace!("in head, finished headers:\n {}", utf8(&buf[..len + index]));
                let body = buf.split_off(len + index + 4);
                if !body.is_empty() {
                    log::trace!("read the front of the body: {}", utf8(&body));
                }
                return Ok((rw, buf, body));
            }

            len += bytes;

            if bytes == 0 {
                if len == 0 {
                    return Err(Error::ClosedByClient);
                } else {
                    return Err(Error::PartialHead);
                }
            }

            if len >= MAX_HEAD_LENGTH {
                return Err(Error::HeadersTooLong);
            }
        }
    }

    pub fn inner_mut(&mut self) -> &mut RW {
        &mut self.rw
    }

    pub async fn next(self) -> Result<Self> {
        Conn::new(self.rw, self.buffer).await
    }

    fn should_close(&self) -> bool {
        self.request_headers
            .contains_ignore_ascii_case("connection", "close")
            || self
                .response_headers
                .contains_ignore_ascii_case("connection", "close")
    }

    fn should_upgrade(&self) -> bool {
        let has_upgrade_header = self.request_headers.get(UPGRADE).is_some();
        let connection_upgrade = match self.request_headers.get("connection") {
            Some(h) => h
                .as_str()
                .split(",")
                .any(|h| h.eq_ignore_ascii_case("upgrade")),
            None => false,
        };
        let response_is_switching_protocols = self.status == Some(StatusCode::SwitchingProtocols);

        has_upgrade_header && connection_upgrade && response_is_switching_protocols
    }

    pub async fn finish(self) -> Result<ConnectionStatus<RW>> {
        if self.should_close() {
            Ok(ConnectionStatus::Close)
        } else if self.should_upgrade() {
            Ok(ConnectionStatus::Upgrade(self.into()))
        } else {
            match self.next().await {
                Err(Error::ClosedByClient) => {
                    log::trace!("connection closed by client");
                    Ok(ConnectionStatus::Close)
                }
                Err(e) => Err(e),
                Ok(conn) => Ok(ConnectionStatus::Conn(conn)),
            }
        }
    }

    pub fn request_content_length(&self) -> crate::Result<Option<usize>> {
        Ok(ContentLength::from_headers(&self.request_headers)
            .map_err(|_| crate::Error::MalformedHeader("content-length"))?
            .map(|cl| cl.len() as usize))
    }

    pub(crate) async fn initialize_request_body_state(&mut self) -> Result<()> {
        if let &RequestBodyState::Start = &self.request_body_state {
            if self.needs_100_continue() {
                self.send_100_continue().await?;
            }

            let content_length = self.request_content_length()?;

            let transfer_encoding_chunked = self
                .request_headers
                .contains_ignore_ascii_case(TRANSFER_ENCODING, "chunked");

            if content_length.is_some() && transfer_encoding_chunked {
                return Err(Error::UnexpectedHeader("content-length"));
            }

            self.request_body_state = if transfer_encoding_chunked {
                RequestBodyState::Chunked { remaining: 0 }
            } else if let Some(total_length) = content_length {
                RequestBodyState::FixedLength {
                    current_index: 0,
                    total_length,
                }
            } else {
                RequestBodyState::End
            }
        }

        Ok(())
    }

    pub async fn encode(mut self) -> Result<ConnectionStatus<RW>> {
        self.send_headers().await?;

        if self.method() != &Method::Head {
            if let Some(body) = self.response_body.take() {
                io::copy(BodyEncoder::new(body), &mut self.rw).await?;
            }
        }

        self.finish().await
    }

    fn body_len(&self) -> Option<usize> {
        match self.response_body {
            Some(ref body) => body.len(),
            None => Some(0),
        }
    }

    fn finalize_headers(&mut self) {
        if self.response_headers.get(TRANSFER_ENCODING).is_none() {
            // If the body isn't streaming, we can set the content-length ahead of time. Else we need to
            // send all items in chunks.
            if let Some(len) = self.body_len() {
                ContentLength::new(len as u64).apply(&mut self.response_headers);
            } else {
                TransferEncoding::new(Encoding::Chunked).apply(&mut self.response_headers);
            }
        }
        if self.response_headers.get(DATE).is_none() {
            Date::now().apply(&mut self.response_headers);
        }
    }

    /// Encode the headers to a buffer, the first time we poll.
    async fn send_headers(&mut self) -> Result<()> {
        let status = self.status().unwrap_or(&StatusCode::NotFound);
        let first_line = format!("HTTP/1.1 {} {}\r\n", status, status.canonical_reason());
        log::trace!("sending: {}", &first_line);
        self.rw.write_all(first_line.as_bytes()).await?;

        self.finalize_headers();
        let mut headers = self.response_headers.iter().collect::<Vec<_>>();
        headers.sort_unstable_by_key(|(h, _)| h.as_str());

        for (header, values) in headers {
            for value in values.iter() {
                log::trace!("sending: {}: {}", &header, &value);

                self.rw
                    .write_all(format!("{}: {}\r\n", header, value).as_bytes())
                    .await?;
            }
        }

        self.rw.write_all(b"\r\n").await?;

        Ok(())
    }
}

pub fn utf8(d: &[u8]) -> &str {
    std::str::from_utf8(d).unwrap_or("not utf8")
}
