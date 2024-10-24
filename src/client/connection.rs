use std::{alloc::{Allocator, Global}, sync::Arc, vec};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{
        mpsc::{
            channel,
            error::TryRecvError,
            Receiver, Sender,
        },
        oneshot, Mutex,
    },
};
use tracing::*;

use crate::{codec, PixelFormat, Rect, VncEncoding, VncError, VncEvent, VncImageEvent, ClientEvent};
const CHANNEL_SIZE: usize = 128;

#[cfg(not(target_arch = "wasm32"))]
use tokio::spawn;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local as spawn;

use super::messages::{ClientMsg, ServerMsg};

struct ImageRect {
    rect: Rect,
    encoding: VncEncoding,
}

impl From<[u8; 12]> for ImageRect {
    fn from(buf: [u8; 12]) -> Self {
        Self {
            rect: Rect {
                x: (buf[0] as u16) << 8 | buf[1] as u16,
                y: (buf[2] as u16) << 8 | buf[3] as u16,
                width: (buf[4] as u16) << 8 | buf[5] as u16,
                height: (buf[6] as u16) << 8 | buf[7] as u16,
            },
            encoding: ((buf[8] as u32) << 24
                | (buf[9] as u32) << 16
                | (buf[10] as u32) << 8
                | (buf[11] as u32))
                .into(),
        }
    }
}

impl ImageRect {
    #[inline]
    async fn read<S>(reader: &mut S) -> Result<Self, VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut rect_buf = [0_u8; 12];
        reader.read_exact(&mut rect_buf).await?;
        Ok(rect_buf.into())
    }
}

pub struct VncClient {
    name: String,
    screen: (u16, u16),
    watch_close: tokio::sync::watch::Sender<bool>,
    client_tx: Sender<ClientEvent>,
    server_rx: Receiver<VncEvent>,
    closed: bool,
}

/// The instance of a connected vnc client

impl VncClient {
    pub(crate) async fn new<S>(
        mut stream: S,
        shared: bool,
        mut pixel_format: Option<PixelFormat>,
        encodings: Vec<VncEncoding>,
    ) -> Result<Self, VncError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (client_tx, mut input_ch_rx) = channel::<ClientEvent>(CHANNEL_SIZE);
        let (server_tx, server_rx) = channel(1);
        let (watch_close_tx, watch_close_rx) = tokio::sync::watch::channel(false);

        trace!("client init msg");
        trace!("Send shared flag: {}", shared);
        stream.write_u8(shared as u8).await?;

        trace!("server init msg");
        let (name, (width, height)) =
            read_server_init(&mut stream, &mut pixel_format, &server_tx)
            .await?;

        trace!("client encodings: {:?}", encodings);
        send_client_encoding(&mut stream, encodings).await?;

        trace!("Require the first frame");
        ClientMsg::FramebufferUpdateRequest(
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
            0,
        ).write(&mut stream).await?;

        // start the decoding thread
        spawn(async move {
            trace!("IO thread starts");
            let pf = pixel_format.as_ref().unwrap();
            let mut reader = Reader::new();
            loop {
                if *watch_close_rx.borrow() {
                    break;
                }
                let result: Result<(), VncError> = try {
                    info!("Waiting for event");
                    reader.read_vnc_event(&mut stream, pf, &server_tx).await?;
                    match input_ch_rx.try_recv() {
                        Ok(event) => event.into_msg(width, height).write(&mut stream).await?,
                        Err(TryRecvError::Empty) => () ,
                        _ => unreachable!(),
                    }
                };
                if let Err(e) = result {
                    server_tx.send(VncEvent::Error(e.to_string())).await.unwrap();
                    break
                }
            }
            trace!("IO thread stops");
        });

        info!("VNC Client {name} starts");
        Ok(Self {
            name,
            screen: (width, height),
            client_tx,
            server_rx,
            watch_close: watch_close_tx,
            closed: false,
        })
    }

    pub fn ensure_alive(&self) -> Result<(), VncError> {
        if self.closed {
            Err(VncError::ClientNotRunning)
        } else {
            Ok(())
        }
    }

    pub async fn send(&mut self, event: ClientEvent) -> Result<(), VncError> {
        self.ensure_alive()?;
        self.client_tx.send(event).await?;
        Ok(())
    }

    pub fn blocking_send(&mut self, event: ClientEvent) -> Result<(), VncError> {
        self.ensure_alive()?;
        self.client_tx.blocking_send(event)?;
        Ok(())
    }

    pub async fn recv(&mut self) -> Result<VncEvent, VncError> {
        self.ensure_alive()?;
        match self.server_rx.recv().await {
            Some(e) => Ok(e),
            None => {
                self.closed = true;
                Err(VncError::ClientNotRunning)
            }
        }
    }

    pub async fn recv_many(&mut self, buffer: &mut Vec<VncEvent>) -> Result<usize, VncError> {
        self.ensure_alive()?;
        Ok(self.server_rx.recv_many(buffer, 0).await)
    }

    pub fn try_recv(&mut self) -> Result<Option<VncEvent>, VncError> {
        self.ensure_alive()?;
        match self.server_rx.try_recv() {
            Err(TryRecvError::Disconnected) => {
                self.closed = true;
                Err(VncError::ClientNotRunning)
            }
            Err(TryRecvError::Empty) => Ok(None),
            Ok(e) => Ok(Some(e)),
        }
        // Ok(self.output_ch.recv().await)
    }

    /// Stop the VNC engine and release resources
    pub fn close(&mut self) -> Result<(), VncError> {
        self.closed = true;
        self.watch_close.send_modify(|i| *i = true);
        Ok(())
    }
}

impl Drop for VncClient {
    fn drop(&mut self) {
        info!("VNC Client {} stops", self.name);
        let _ = self.close();
    }
}

async fn send_client_init<S>(stream: &mut S, shared: bool) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    trace!("Send shared flag: {}", shared);
    stream.write_u8(shared as u8).await?;
    Ok(())
}

async fn read_server_init<S>(
    stream: &mut S,
    pf: &mut Option<PixelFormat>,
    output_channel: &Sender<VncEvent>,
) -> Result<(String, (u16, u16)), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    // +--------------+--------------+------------------------------+
    // | No. of bytes | Type [Value] | Description                  |
    // +--------------+--------------+------------------------------+
    // | 2            | U16          | framebuffer-width in pixels  |
    // | 2            | U16          | framebuffer-height in pixels |
    // | 16           | PIXEL_FORMAT | server-pixel-format          |
    // | 4            | U32          | name-length                  |
    // | name-length  | U8 array     | name-string                  |
    // +--------------+--------------+------------------------------+

    let screen_width = stream.read_u16().await?;
    let screen_height = stream.read_u16().await?;

    output_channel.send(VncEvent::SetResolution(
        (screen_width, screen_height).into(),
    ))
    .await?;

    let pixel_format = PixelFormat::read(stream).await?;
    if let Some(pf) = pf.as_ref() {
        trace!("Send customized pixel format {:#?}", pf);
        ClientMsg::SetPixelFormat(*pf)
            .write(stream)
            .await?;
    } else {
        output_channel.send(VncEvent::SetPixelFormat(pixel_format)).await?;
        let _ = pf.insert(pixel_format);
    }

    let name_len = stream.read_u32().await?;
    let mut name_buf = vec![0_u8; name_len as usize];
    stream.read_exact(&mut name_buf).await?;
    let name = String::from_utf8_lossy(&name_buf).into_owned();

    Ok((name, (screen_width, screen_height)))
}

async fn send_client_encoding<S>(
    stream: &mut S,
    encodings: Vec<VncEncoding>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    ClientMsg::SetEncodings(encodings).write(stream).await?;
    Ok(())
}

struct Reader<A: Allocator = Global> {
    allocator: A,
    frames: Vec<VncImageEvent>,
    raw_decoder: codec::RawDecoder,
    zrle_decoder: codec::ZrleDecoder,
    tight_decoder: codec::TightDecoder,
    trle_decoder: codec::TrleDecoder,
    cursor: codec::CursorDecoder,
}

impl Reader<Global> {
    pub fn new() -> Self {
        Self::new_in(Global)
    }
}

impl<A> Reader<A> where A: Allocator {
    pub fn new_in(allocator: A) -> Self {
        Self {
            allocator,
            frames: Vec::new(),
            raw_decoder: codec::RawDecoder::new(),
            zrle_decoder: codec::ZrleDecoder::new(),
            tight_decoder: codec::TightDecoder::new(),
            trle_decoder: codec::TrleDecoder::new(),
            cursor: codec::CursorDecoder::new(),
        }
    }
    async fn read_vnc_event<S>(
        &mut self,
        stream: &mut S,
        pf: &PixelFormat,
        output_channel: &Sender<VncEvent>,
    ) -> Result<(), VncError>
where
        S: AsyncRead + Unpin {
        let server_msg = ServerMsg::read(stream).await?;
        debug!("Server message got: {:?}", server_msg);
        match server_msg {
            ServerMsg::FramebufferUpdate(rect_num) => {
                self.decode_framebuffer_update(pf, stream, rect_num, &output_channel).await?
            }
            // SetColorMapEntries,
            ServerMsg::Bell => {
                output_channel.send(VncEvent::Bell).await?;
            }
            ServerMsg::ServerCutText(text) => {
                output_channel.send(VncEvent::Text(text)).await?;
            }
        };
        Ok(())
    }
    async fn decode_framebuffer_update<S>(
        &mut self,
        pf: &PixelFormat,
        stream: &mut S,
        rect_num: u16,
        output_channel: &Sender<VncEvent>,
    ) -> Result<(), VncError>
    where
        S: AsyncRead + Unpin,
    {
        let mut frames = &mut self.frames;
        frames.reserve(rect_num as usize);
        debug!("Received {} frames from the server", rect_num);
        for i in 0..rect_num {
            let rect = ImageRect::read(stream).await?;
            trace!("Encoding of {}: {:?}", i, rect.encoding);

            match rect.encoding {
                VncEncoding::Raw => {
                    let image = self.raw_decoder.decode(pf, &rect.rect, stream).await?;
                    frames.push(VncImageEvent::RawImage(image));
                }
                VncEncoding::CopyRect => {
                    let source_x = stream.read_u16().await?;
                    let source_y = stream.read_u16().await?;
                    let mut src_rect = rect.rect;
                    src_rect.x = source_x;
                    src_rect.y = source_y;
                    frames.push(VncImageEvent::Copy(rect.rect, src_rect))
                }
                VncEncoding::Tight => frames.push(self.tight_decoder.decode(pf, &rect.rect, stream).await?),
                VncEncoding::Trle => {
                    self.trle_decoder
                        .decode(pf, &rect.rect, stream, &mut frames)
                        .await?;
                }
                VncEncoding::Zrle => {
                    self.zrle_decoder
                        .decode(pf, &rect.rect, stream, &mut frames)
                        .await?;
                }
                VncEncoding::CursorPseudo => output_channel.send(self.cursor.decode(pf, &rect.rect, stream).await?).await? ,
                VncEncoding::DesktopSizePseudo => output_channel.send(VncEvent::SetResolution((rect.rect.width, rect.rect.height).into())).await?,
                VncEncoding::LastRectPseudo => {
                    break;
                }
            };
        }
        debug!("Sending {} frames", frames.len());
        output_channel.send(VncEvent::ImageEvent(frames.split_off(0))).await?;
        Ok(())
    }
}

/* async fn async_connection_process_loop<S>(
    mut stream: S,
    mut input_ch: Receiver<ClientMsg>,
    conn_ch: Sender<std::io::Result<Vec<u8>>>,
    mut stop_ch: oneshot::Receiver<()>,
) -> Result<(), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let mut buffer = [0; 65535];
    let mut pending = 0;

    // main traffic loop
    while let Err(oneshot::error::TryRecvError::Empty) = stop_ch.try_recv() {
        if pending > 0 {
            match conn_ch.try_send(Ok(buffer[0..pending].to_owned())) {
                Err(TrySendError::Full(_message)) => (),
                Err(TrySendError::Closed(_message)) => break,
                Ok(()) => pending = 0,
            }
        }

        tokio::select! {
            result = stream.read(&mut buffer), if pending == 0 => {
                match result {
                    Ok(nread) => {
                        if nread > 0 {
                            match conn_ch.try_send(Ok(buffer[0..nread].to_owned())) {
                                Err(TrySendError::Full(_message)) => pending = nread,
                                Err(TrySendError::Closed(_message)) => break,
                                Ok(()) => ()
                            }
                        } else {
                            // According to the tokio's Doc
                            // https://docs.rs/tokio/latest/tokio/io/trait.AsyncRead.html
                            // if nread == 0, then EOF is reached
                            trace!("Net Connection EOF detected");
                            break;
                        }
                    }
                    Err(e) => {
                        error!("{}", e.to_string());
                        break;
                    }
                }
            }
            Some(msg) = input_ch.recv() => {
                msg.write(&mut stream).await?;
            }
        }
    }

    // notify the decoding thread
    let _ = conn_ch
        .send(Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof)))
        .await;

    Ok(())
} */
