//! Stream encoders.

use std::{
    error::Error as StdError,
    future::Future,
    io::{self, Write as _},
    pin::Pin,
    task::{Context, Poll},
};

use actix_rt::task::{spawn_blocking, JoinHandle};
use bytes::Bytes;
use derive_more::Display;
use futures_core::ready;
use pin_project_lite::pin_project;

#[cfg(feature = "compress-brotli")]
use brotli2::write::BrotliEncoder;

#[cfg(feature = "compress-gzip")]
use flate2::write::{GzEncoder, ZlibEncoder};

#[cfg(feature = "compress-zstd")]
use zstd::stream::write::Encoder as ZstdEncoder;

use super::Writer;
use crate::{
    body::{BodySize, MessageBody},
    error::BlockingError,
    header::{self, ContentEncoding, HeaderValue, CONTENT_ENCODING},
    ResponseHead, StatusCode,
};

const MAX_CHUNK_SIZE_ENCODE_IN_PLACE: usize = 1024;

pin_project! {
    pub struct Encoder<B> {
        #[pin]
        body: EncoderBody<B>,
        encoder: Option<ContentEncoder>,
        fut: Option<JoinHandle<Result<ContentEncoder, io::Error>>>,
        eof: bool,
    }
}

impl<B: MessageBody> Encoder<B> {
    fn none() -> Self {
        Encoder {
            body: EncoderBody::None,
            encoder: None,
            fut: None,
            eof: true,
        }
    }

    pub fn response(encoding: ContentEncoding, head: &mut ResponseHead, mut body: B) -> Self {
        let can_encode = !(head.headers().contains_key(&CONTENT_ENCODING)
            || head.status == StatusCode::SWITCHING_PROTOCOLS
            || head.status == StatusCode::NO_CONTENT
            || encoding == ContentEncoding::Identity
            || encoding == ContentEncoding::Auto);

        // no need to compress an empty body
        if matches!(body.size(), BodySize::None) {
            return Self::none();
        }

        let body = if body.is_complete_body() {
            let body = body.take_complete_body();
            EncoderBody::Full { body }
        } else {
            EncoderBody::Stream { body }
        };

        if can_encode {
            // Modify response body only if encoder is set
            if let Some(enc) = ContentEncoder::encoder(encoding) {
                update_head(encoding, head);

                return Encoder {
                    body,
                    encoder: Some(enc),
                    fut: None,
                    eof: false,
                };
            }
        }

        Encoder {
            body,
            encoder: None,
            fut: None,
            eof: false,
        }
    }
}

pin_project! {
    #[project = EncoderBodyProj]
    enum EncoderBody<B> {
        None,
        Full { body: Bytes },
        Stream { #[pin] body: B },
    }
}

impl<B> MessageBody for EncoderBody<B>
where
    B: MessageBody,
{
    type Error = EncoderError;

    fn size(&self) -> BodySize {
        match self {
            EncoderBody::None => BodySize::None,
            EncoderBody::Full { body } => body.size(),
            EncoderBody::Stream { body } => body.size(),
        }
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        match self.project() {
            EncoderBodyProj::None => Poll::Ready(None),
            EncoderBodyProj::Full { body } => {
                Pin::new(body).poll_next(cx).map_err(|err| match err {})
            }
            EncoderBodyProj::Stream { body } => body
                .poll_next(cx)
                .map_err(|err| EncoderError::Body(err.into())),
        }
    }

    fn is_complete_body(&self) -> bool {
        match self {
            EncoderBody::None => true,
            EncoderBody::Full { .. } => true,
            EncoderBody::Stream { .. } => false,
        }
    }

    fn take_complete_body(&mut self) -> Bytes {
        match self {
            EncoderBody::None => Bytes::new(),
            EncoderBody::Full { body } => body.take_complete_body(),
            EncoderBody::Stream { .. } => {
                panic!("EncoderBody::Stream variant cannot be taken")
            }
        }
    }
}

impl<B> MessageBody for Encoder<B>
where
    B: MessageBody,
{
    type Error = EncoderError;

    fn size(&self) -> BodySize {
        if self.encoder.is_some() {
            BodySize::Stream
        } else {
            self.body.size()
        }
    }

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, Self::Error>>> {
        let mut this = self.project();
        loop {
            if *this.eof {
                return Poll::Ready(None);
            }

            if let Some(ref mut fut) = this.fut {
                let mut encoder = ready!(Pin::new(fut).poll(cx))
                    .map_err(|_| EncoderError::Blocking(BlockingError))?
                    .map_err(EncoderError::Io)?;

                let chunk = encoder.take();
                *this.encoder = Some(encoder);
                this.fut.take();

                if !chunk.is_empty() {
                    return Poll::Ready(Some(Ok(chunk)));
                }
            }

            let result = ready!(this.body.as_mut().poll_next(cx));

            match result {
                Some(Err(err)) => return Poll::Ready(Some(Err(err))),

                Some(Ok(chunk)) => {
                    if let Some(mut encoder) = this.encoder.take() {
                        if chunk.len() < MAX_CHUNK_SIZE_ENCODE_IN_PLACE {
                            encoder.write(&chunk).map_err(EncoderError::Io)?;
                            let chunk = encoder.take();
                            *this.encoder = Some(encoder);

                            if !chunk.is_empty() {
                                return Poll::Ready(Some(Ok(chunk)));
                            }
                        } else {
                            *this.fut = Some(spawn_blocking(move || {
                                encoder.write(&chunk)?;
                                Ok(encoder)
                            }));
                        }
                    } else {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                }

                None => {
                    if let Some(encoder) = this.encoder.take() {
                        let chunk = encoder.finish().map_err(EncoderError::Io)?;

                        if chunk.is_empty() {
                            return Poll::Ready(None);
                        } else {
                            *this.eof = true;
                            return Poll::Ready(Some(Ok(chunk)));
                        }
                    } else {
                        return Poll::Ready(None);
                    }
                }
            }
        }
    }

    fn is_complete_body(&self) -> bool {
        if self.encoder.is_some() {
            false
        } else {
            self.body.is_complete_body()
        }
    }

    fn take_complete_body(&mut self) -> Bytes {
        if self.encoder.is_some() {
            panic!("compressed body stream cannot be taken")
        } else {
            self.body.take_complete_body()
        }
    }
}

fn update_head(encoding: ContentEncoding, head: &mut ResponseHead) {
    head.headers_mut().insert(
        header::CONTENT_ENCODING,
        HeaderValue::from_static(encoding.as_str()),
    );

    head.no_chunking(false);
}

enum ContentEncoder {
    #[cfg(feature = "compress-gzip")]
    Deflate(ZlibEncoder<Writer>),

    #[cfg(feature = "compress-gzip")]
    Gzip(GzEncoder<Writer>),

    #[cfg(feature = "compress-brotli")]
    Br(BrotliEncoder<Writer>),

    // Wwe need explicit 'static lifetime here because ZstdEncoder needs a lifetime argument and we
    // use `spawn_blocking` in `Encoder::poll_next` that requires `FnOnce() -> R + Send + 'static`.
    #[cfg(feature = "compress-zstd")]
    Zstd(ZstdEncoder<'static, Writer>),
}

impl ContentEncoder {
    fn encoder(encoding: ContentEncoding) -> Option<Self> {
        match encoding {
            #[cfg(feature = "compress-gzip")]
            ContentEncoding::Deflate => Some(ContentEncoder::Deflate(ZlibEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),

            #[cfg(feature = "compress-gzip")]
            ContentEncoding::Gzip => Some(ContentEncoder::Gzip(GzEncoder::new(
                Writer::new(),
                flate2::Compression::fast(),
            ))),

            #[cfg(feature = "compress-brotli")]
            ContentEncoding::Br => {
                Some(ContentEncoder::Br(BrotliEncoder::new(Writer::new(), 3)))
            }

            #[cfg(feature = "compress-zstd")]
            ContentEncoding::Zstd => {
                let encoder = ZstdEncoder::new(Writer::new(), 3).ok()?;
                Some(ContentEncoder::Zstd(encoder))
            }

            _ => None,
        }
    }

    #[inline]
    pub(crate) fn take(&mut self) -> Bytes {
        match *self {
            #[cfg(feature = "compress-brotli")]
            ContentEncoder::Br(ref mut encoder) => encoder.get_mut().take(),

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Deflate(ref mut encoder) => encoder.get_mut().take(),

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Gzip(ref mut encoder) => encoder.get_mut().take(),

            #[cfg(feature = "compress-zstd")]
            ContentEncoder::Zstd(ref mut encoder) => encoder.get_mut().take(),
        }
    }

    fn finish(self) -> Result<Bytes, io::Error> {
        match self {
            #[cfg(feature = "compress-brotli")]
            ContentEncoder::Br(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Gzip(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Deflate(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },

            #[cfg(feature = "compress-zstd")]
            ContentEncoder::Zstd(encoder) => match encoder.finish() {
                Ok(writer) => Ok(writer.buf.freeze()),
                Err(err) => Err(err),
            },
        }
    }

    fn write(&mut self, data: &[u8]) -> Result<(), io::Error> {
        match *self {
            #[cfg(feature = "compress-brotli")]
            ContentEncoder::Br(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding br encoding: {}", err);
                    Err(err)
                }
            },

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Gzip(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding gzip encoding: {}", err);
                    Err(err)
                }
            },

            #[cfg(feature = "compress-gzip")]
            ContentEncoder::Deflate(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding deflate encoding: {}", err);
                    Err(err)
                }
            },

            #[cfg(feature = "compress-zstd")]
            ContentEncoder::Zstd(ref mut encoder) => match encoder.write_all(data) {
                Ok(_) => Ok(()),
                Err(err) => {
                    trace!("Error decoding ztsd encoding: {}", err);
                    Err(err)
                }
            },
        }
    }
}

#[derive(Debug, Display)]
#[non_exhaustive]
pub enum EncoderError {
    #[display(fmt = "body")]
    Body(Box<dyn StdError>),

    #[display(fmt = "blocking")]
    Blocking(BlockingError),

    #[display(fmt = "io")]
    Io(io::Error),
}

impl StdError for EncoderError {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match self {
            EncoderError::Body(err) => Some(&**err),
            EncoderError::Blocking(err) => Some(err),
            EncoderError::Io(err) => Some(err),
        }
    }
}

impl From<EncoderError> for crate::Error {
    fn from(err: EncoderError) -> Self {
        crate::Error::new_encoder().with_cause(err)
    }
}
