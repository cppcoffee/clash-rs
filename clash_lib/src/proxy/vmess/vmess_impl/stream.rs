use std::{fmt::Debug, mem::MaybeUninit, pin::Pin, task::Poll, time::SystemTime};

use aes_gcm::Aes128Gcm;
use bytes::{BufMut, BytesMut};
use chacha20poly1305::ChaCha20Poly1305;
use futures::ready;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tracing::debug;

use crate::{
    common::{
        crypto::{self, AeadCipherHelper},
        errors::map_io_error,
        utils,
    },
    proxy::vmess::vmess_impl::MAX_CHUNK_SIZE,
    session::SocksAddr,
    vmess_debug,
};

use super::{
    cipher::{AeadCipher, VmessSecurity},
    header,
    kdf::{
        self, KDF_SALT_CONST_AEAD_RESP_HEADER_LEN_IV, KDF_SALT_CONST_AEAD_RESP_HEADER_LEN_KEY,
        KDF_SALT_CONST_AEAD_RESP_HEADER_PAYLOAD_IV, KDF_SALT_CONST_AEAD_RESP_HEADER_PAYLOAD_KEY,
    },
    user::{ID, ID_BYTES_LEN},
    Security, CHUNK_SIZE, COMMAND_TCP, COMMAND_UDP, OPTION_CHUNK_STREAM, SECURITY_AES_128_GCM,
    SECURITY_CHACHA20_POLY1305, SECURITY_NONE, VERSION,
};

pub struct VmessStream<S> {
    stream: S,
    aead_read_cipher: Option<AeadCipher>,
    aead_write_cipher: Option<AeadCipher>,
    dst: SocksAddr,
    id: ID,
    req_body_iv: Vec<u8>,
    req_body_key: Vec<u8>,
    resp_body_iv: Vec<u8>,
    resp_body_key: Vec<u8>,
    resp_v: u8,
    security: u8,
    is_aead: bool,
    is_udp: bool,

    read_state: ReadState,
    read_pos: usize,
    read_buf: BytesMut,

    write_state: WriteState,
    write_buf: BytesMut,
}

impl<S> Debug for VmessStream<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VmessStream")
            .field("dst", &self.dst)
            .field("is_aead", &self.is_aead)
            .field("is_udp", &self.is_udp)
            .finish()
    }
}

enum ReadState {
    AeadWaitingHeaderSize,
    AeadWaitingHeader(usize),
    StreamWaitingLength,
    StreamWaitingData(usize),
    StreamFlushingData(usize),
}

enum WriteState {
    BuildingData,
    FlushingData(usize, (usize, usize)),
}

pub trait ReadExt {
    fn poll_read_exact(
        &mut self,
        cx: &mut std::task::Context,
        size: usize,
    ) -> Poll<std::io::Result<()>>;
    fn get_data(&self) -> &[u8];
}

impl<S: AsyncRead + Unpin> ReadExt for VmessStream<S> {
    // Read exactly `size` bytes into `read_buf`, starting from position 0.
    fn poll_read_exact(
        &mut self,
        cx: &mut std::task::Context,
        size: usize,
    ) -> Poll<std::io::Result<()>> {
        self.read_buf.reserve(size);
        unsafe { self.read_buf.set_len(size) }
        vmess_debug!(
            "poll read exact: {}, read_pos: {}, buf: {}",
            size,
            self.read_pos,
            self.read_buf.len()
        );
        loop {
            if self.read_pos < size {
                let dst = unsafe {
                    &mut *((&mut self.read_buf[self.read_pos..size]) as *mut _
                        as *mut [MaybeUninit<u8>])
                };
                let mut buf = ReadBuf::uninit(dst);
                let ptr = buf.filled().as_ptr();
                ready!(Pin::new(&mut self.stream).poll_read(cx, &mut buf))?;
                assert_eq!(ptr, buf.filled().as_ptr());
                if buf.filled().is_empty() {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "unexpected eof",
                    )));
                }
                self.read_pos += buf.filled().len();
            } else {
                assert!(self.read_pos == size);
                self.read_pos = 0;
                return Poll::Ready(Ok(()));
            }
        }
    }

    fn get_data(&self) -> &[u8] {
        self.read_buf.as_ref()
    }
}

impl<S> VmessStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync,
{
    pub(crate) async fn new(
        stream: S,
        id: &ID,
        dst: &SocksAddr,
        security: &Security,
        is_aead: bool,
        is_udp: bool,
    ) -> std::io::Result<VmessStream<S>> {
        let mut rand_bytes = [0u8; 33];
        utils::rand_fill(&mut rand_bytes[..]);
        let req_body_iv = rand_bytes[0..16].to_vec();
        let req_body_key = rand_bytes[16..32].to_vec();
        let resp_v = rand_bytes[32];

        let (resp_body_key, resp_body_iv) = if is_aead {
            (
                utils::sha256(req_body_key.as_slice())[0..16].to_vec(),
                utils::sha256(req_body_iv.as_slice())[0..16].to_vec(),
            )
        } else {
            (
                utils::md5(req_body_key.as_slice()),
                utils::md5(req_body_iv.as_slice()),
            )
        };

        let (aead_read_cipher, aead_write_cipher) = match security {
            &SECURITY_NONE => (None, None),
            &SECURITY_AES_128_GCM => {
                let write_cipher =
                    VmessSecurity::Aes128Gcm(Aes128Gcm::new_with_slice(&req_body_key));
                let write_cipher = AeadCipher::new(&req_body_iv, write_cipher);
                let reader_cipher =
                    VmessSecurity::Aes128Gcm(Aes128Gcm::new_with_slice(&resp_body_key));
                let read_cipher = AeadCipher::new(&resp_body_iv, reader_cipher);
                (Some(read_cipher), Some(write_cipher))
            }
            &SECURITY_CHACHA20_POLY1305 => {
                let mut key = [0u8; 32];
                let tmp = utils::md5(&req_body_key);
                key.copy_from_slice(&tmp);
                let tmp = utils::md5(&key[..16]);
                key[16..].copy_from_slice(&tmp);
                let write_cipher =
                    VmessSecurity::ChaCha20Poly1305(ChaCha20Poly1305::new_with_slice(&key));
                let write_cipher = AeadCipher::new(&req_body_iv, write_cipher);

                let tmp = utils::md5(&req_body_key);
                key.copy_from_slice(&tmp);
                let tmp = utils::md5(&key[..16]);
                key[16..].copy_from_slice(&tmp);
                let reader_cipher =
                    VmessSecurity::ChaCha20Poly1305(ChaCha20Poly1305::new_with_slice(&key));
                let read_cipher = AeadCipher::new(&resp_body_iv, reader_cipher);

                (Some(read_cipher), Some(write_cipher))
            }
            _ => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "unsupported security",
                ))
            }
        };

        let mut stream = Self {
            stream,
            aead_read_cipher,
            aead_write_cipher,
            dst: dst.to_owned(),
            id: id.to_owned(),
            req_body_iv,
            req_body_key,
            resp_body_iv,
            resp_body_key,
            resp_v,
            security: *security,
            is_aead,
            is_udp,

            read_state: ReadState::AeadWaitingHeaderSize,
            read_pos: 0,
            read_buf: BytesMut::new(),

            write_state: WriteState::BuildingData,
            write_buf: BytesMut::new(),
        };

        stream.send_handshake_request().await?;

        Ok(stream)
    }
}

impl<S> VmessStream<S>
where
    S: AsyncWrite + Unpin + Send + Sync,
{
    async fn send_handshake_request(&mut self) -> std::io::Result<()> {
        let Self {
            ref mut stream,
            ref req_body_key,
            ref req_body_iv,
            ref resp_v,
            ref security,
            ref dst,
            ref is_aead,
            ref is_udp,
            ref id,
            ..
        } = self;

        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("check your system clock")
            .as_secs();

        let mut mbuf = BytesMut::new();

        if !is_aead {
            let mut hash = [0u8; boring_sys::EVP_MAX_MD_SIZE as usize];
            let mut out_len: u32 = 0;

            unsafe {
                boring_sys::HMAC(
                    boring_sys::EVP_md5(),
                    id.uuid.as_bytes().as_ptr() as _,
                    ID_BYTES_LEN,
                    now.to_be_bytes().as_mut_ptr() as _,
                    8,
                    &mut hash as _,
                    &mut out_len as _,
                );
            }
            mbuf.put_slice(&hash[..out_len as _])
        }

        let mut buf = BytesMut::new();
        buf.put_u8(VERSION);
        buf.put_slice(req_body_iv);
        buf.put_slice(req_body_key);
        buf.put_u8(*resp_v);
        buf.put_u8(OPTION_CHUNK_STREAM);

        let p = utils::rand_range(0..16);
        buf.put_u8((p << 4) as u8 | security);

        buf.put_u8(0);

        if *is_udp {
            buf.put_u8(COMMAND_UDP);
        } else {
            buf.put_u8(COMMAND_TCP);
        }

        dst.write_to_buf_vmess(&mut buf);

        if p > 0 {
            let mut padding = vec![0u8; p as usize];
            utils::rand_fill(&mut padding[..]);
            buf.put_slice(&padding);
        }

        unsafe {
            let sum = boring_sys::OPENSSL_hash32(buf.as_mut_ptr() as _, buf.len());
            buf.put_slice(sum.to_be_bytes().as_ref());
        }

        if !is_aead {
            let mut data = buf.to_vec();
            crypto::aes_cfb_encrypt(&id.cmd_key[..], &hash_timestamp(now)[..], &mut data)
                .map_err(map_io_error)?;

            mbuf.put_slice(data.as_slice());
            let out = mbuf.freeze();
            vmess_debug!("send non aead handshake request for user{}", id.uuid);
            stream.write_all(&out).await?;
        } else {
            let out = header::seal_vmess_aead_header(id.cmd_key, buf.freeze().to_vec(), now)
                .map_err(map_io_error)?;
            vmess_debug!("send aead handshake request for user {}", id.uuid);

            stream.write_all(&out).await?;
        }

        stream.flush().await?;

        Ok(())
    }
}

impl<S> AsyncRead for VmessStream<S>
where
    S: AsyncRead + Unpin + Send + Sync,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        vmess_debug!("poll read with aead");

        loop {
            match self.read_state {
                ReadState::AeadWaitingHeaderSize => {
                    vmess_debug!("recv handshake response header");
                    let this = &mut *self;
                    let resp_body_key = this.resp_body_key.clone();
                    let resp_body_iv = this.resp_body_iv.clone();
                    let resp_v = this.resp_v;

                    if !this.is_aead {
                        ready!(this.poll_read_exact(cx, 4))?;
                        let mut buf = this.read_buf.split().freeze().to_vec();
                        crypto::aes_cfb_decrypt(&resp_body_key, &resp_body_iv, &mut buf)
                            .map_err(map_io_error)?;
                        if buf[0] != resp_v {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "invalid response - non aead invalid resp_v",
                            )));
                        }

                        if buf[2] != 0 {
                            return Poll::Ready(Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "invalid response - dynamic port not supported",
                            )));
                        }

                        this.read_state = ReadState::StreamWaitingLength;
                    } else {
                        vmess_debug!("recv handshake response header length");
                        ready!(this.poll_read_exact(cx, 18))?;

                        let aead_response_header_length_encryption_key = &kdf::vmess_kdf_1_one_shot(
                            &resp_body_key,
                            KDF_SALT_CONST_AEAD_RESP_HEADER_LEN_KEY,
                        )[..16];
                        let aead_response_header_length_encryption_iv = &kdf::vmess_kdf_1_one_shot(
                            &resp_body_iv,
                            KDF_SALT_CONST_AEAD_RESP_HEADER_LEN_IV,
                        )[..12];

                        let decrypted_response_header_len = crypto::aes_gcm_open(
                            aead_response_header_length_encryption_key,
                            aead_response_header_length_encryption_iv,
                            this.read_buf.split().as_ref(),
                            None,
                        )
                        .map_err(map_io_error)?;

                        if decrypted_response_header_len.len() < 2 {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "invalid response header length",
                            ))
                            .into();
                        }

                        this.read_state = ReadState::AeadWaitingHeader(u16::from_be_bytes(
                            decrypted_response_header_len[..2].try_into().unwrap(),
                        )
                            as usize);
                    }
                }

                ReadState::AeadWaitingHeader(header_size) => {
                    vmess_debug!("recv handshake header body: {}", header_size);

                    let this = &mut *self;
                    ready!(this.poll_read_exact(cx, header_size + 16))?;

                    let resp_body_key = this.resp_body_key.clone();
                    let resp_body_iv = this.resp_body_iv.clone();

                    let aead_response_header_payload_encryption_key = &kdf::vmess_kdf_1_one_shot(
                        &resp_body_key,
                        KDF_SALT_CONST_AEAD_RESP_HEADER_PAYLOAD_KEY,
                    )[..16];
                    let aead_response_header_payload_encryption_iv = &kdf::vmess_kdf_1_one_shot(
                        &resp_body_iv,
                        KDF_SALT_CONST_AEAD_RESP_HEADER_PAYLOAD_IV,
                    )[..12];

                    let buf = crypto::aes_gcm_open(
                        &aead_response_header_payload_encryption_key,
                        &aead_response_header_payload_encryption_iv,
                        this.read_buf.split().as_ref(),
                        None,
                    )
                    .map_err(map_io_error)?;

                    if buf.len() < 4 {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid response - header too short",
                        )));
                    }

                    if buf[0] != this.resp_v {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid response - version mismatch",
                        )));
                    }

                    if buf[2] != 0 {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid response - dynamic port not supported",
                        )));
                    }

                    this.read_state = ReadState::StreamWaitingLength;
                }

                ReadState::StreamWaitingLength => {
                    vmess_debug!("recv stream length");
                    let this = &mut *self;
                    ready!(this.poll_read_exact(cx, 2))?;
                    let len = u16::from_be_bytes(this.read_buf.split().as_ref().try_into().unwrap())
                        as usize;

                    if len > MAX_CHUNK_SIZE {
                        return Poll::Ready(Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "invalid response - chunk size too large",
                        )));
                    }

                    this.read_state = ReadState::StreamWaitingData(len);
                }

                ReadState::StreamWaitingData(size) => {
                    vmess_debug!("recv stream data: {}", size);
                    let this = &mut *self;
                    ready!(this.poll_read_exact(cx, size))?;

                    if let Some(ref mut cipher) = this.aead_read_cipher {
                        cipher.decrypt_inplace(&mut this.read_buf)?;
                        let data_len = size - cipher.security.overhead_len();
                        this.read_buf.truncate(data_len);
                        this.read_state = ReadState::StreamFlushingData(data_len);
                    } else {
                        this.read_state = ReadState::StreamFlushingData(size);
                    }
                }

                ReadState::StreamFlushingData(size) => {
                    vmess_debug!("flush stream data: {}", size);
                    let to_read = std::cmp::min(buf.remaining(), size);
                    let payload = self.read_buf.split_to(to_read);
                    buf.put_slice(&payload);
                    if to_read < size {
                        // there're unread data, continues in next poll
                        self.read_state = ReadState::StreamFlushingData(size - to_read);
                    } else {
                        // all data consumed, ready to read next chunk
                        self.read_state = ReadState::StreamWaitingLength;
                    }

                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

impl<S> AsyncWrite for VmessStream<S>
where
    S: AsyncWrite + Unpin + Send + Sync,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        loop {
            match self.write_state {
                WriteState::BuildingData => {
                    let this = &mut *self;
                    let mut overhead_len = 0;
                    if let Some(ref mut cipher) = this.aead_write_cipher {
                        overhead_len = cipher.security.overhead_len();
                    }

                    let max_payload_size = CHUNK_SIZE - overhead_len;
                    let consume_len = std::cmp::min(buf.len(), max_payload_size);
                    let payload_len = consume_len + overhead_len;

                    let size_bytes = 2;
                    this.write_buf.reserve(size_bytes + payload_len);
                    this.write_buf.put_u16(payload_len as u16);

                    let mut piece2 = this.write_buf.split_off(size_bytes);

                    piece2.put_slice(&buf[..consume_len]);
                    if let Some(ref mut cipher) = this.aead_write_cipher {
                        piece2
                            .extend_from_slice(vec![0u8; cipher.security.overhead_len()].as_ref());
                        cipher.encrypt_inplace(&mut piece2)?;
                    }

                    this.write_buf.unsplit(piece2);

                    // ready to write data
                    self.write_state =
                        WriteState::FlushingData(consume_len, (this.write_buf.len(), 0));
                }

                // consumed is the consumed plaintext length we're going to return to caller.
                // total is total length of the ciphertext data chunk we're going to write to remote.
                // written is the number of ciphertext bytes were written.
                WriteState::FlushingData(consumed, (total, written)) => {
                    let this = &mut *self;

                    // There would be trouble if the caller change the buf upon pending, but I
                    // believe that's not a usual use case.
                    let nw = ready!(tokio_util::io::poll_write_buf(
                        Pin::new(&mut this.stream),
                        cx,
                        &mut this.write_buf
                    ))?;
                    if nw == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::WriteZero,
                            "failed to write whole data",
                        ))
                        .into();
                    }

                    if written + nw >= total {
                        // data chunk written, go to next chunk
                        this.write_state = WriteState::BuildingData;
                        return Poll::Ready(Ok(consumed));
                    }

                    this.write_state = WriteState::FlushingData(consumed, (total, written + nw));
                }
            }
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { ref mut stream, .. } = self.get_mut();
        Pin::new(stream).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let Self { ref mut stream, .. } = self.get_mut();
        Pin::new(stream).poll_shutdown(cx)
    }
}

fn hash_timestamp(timestamp: u64) -> [u8; 16] {
    unsafe {
        let mut ctx = boring_sys::MD5_CTX::default();
        boring_sys::MD5_Init(&mut ctx);

        boring_sys::MD5_Update(&mut ctx, timestamp.to_be_bytes().as_ptr() as _, 8);
        boring_sys::MD5_Update(&mut ctx, timestamp.to_be_bytes().as_ptr() as _, 8);
        boring_sys::MD5_Update(&mut ctx, timestamp.to_be_bytes().as_ptr() as _, 8);
        boring_sys::MD5_Update(&mut ctx, timestamp.to_be_bytes().as_ptr() as _, 8);

        let mut hash = [0u8; 16];
        boring_sys::MD5_Final(hash.as_mut_ptr() as _, &mut ctx);
        hash
    }
}
