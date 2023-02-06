//! Integrity codec.

use bytes::{Buf, BufMut, Bytes, BytesMut};
use crc::{Crc, CRC_32_BZIP2};
use std::{fmt, io, mem::size_of};
use tokio_util::codec::{Decoder, Encoder};

/// CRC calculator.
const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_BZIP2);

/// A packet decoding error.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum IntegrityError {
    /// A packet exceeds the maximum allowed size.
    PacketTooBig,
    /// A sequence number was skipped or corrupted.
    SeqSkipped,
    /// Data checksum verification failed.
    DataCorrupted,
}

impl fmt::Display for IntegrityError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::PacketTooBig => write!(f, "packet too big"),
            Self::SeqSkipped => write!(f, "sequence number skipped"),
            Self::DataCorrupted => write!(f, "data corrupted"),
        }
    }
}

impl std::error::Error for IntegrityError {}

/// A codec for frames delimited by a header specifying their lengths, sequence number and checksums.
///
/// The data integrity is verified using the CRC32 checksum.
#[derive(Debug, Clone)]
pub struct IntegrityCodec {
    /// Maximum frame length.
    max_frame_len: u32,
    /// Read state
    state: DecodeState,
    /// Next sequence number for decoding
    decode_seq: u16,
    /// Next sequence number for encoding
    encode_seq: u16,
}

#[derive(Debug, Clone, Copy)]
enum DecodeState {
    Header,
    Data(Header),
}

#[derive(Debug, Clone, Copy)]
struct Header {
    length: u32,
    checksum: u32,
}

impl IntegrityCodec {
    const HEAD_LEN: usize = size_of::<u32>() + size_of::<u32>() + size_of::<u16>();

    /// Creates a new `IntegrityCodec` with the default configuration values.
    pub fn new() -> Self {
        Self { max_frame_len: 8 * 1_024 * 1_024, state: DecodeState::Header, decode_seq: 0, encode_seq: 0 }
    }

    /// Returns the maximum packet size.
    ///
    /// This is the largest size this codec will accept from the wire.
    /// Larger packets will be rejected.
    pub fn max_packet_size(&self) -> u32 {
        self.max_frame_len
    }

    /// Sets the maximum packet size.
    ///
    /// This is the largest size this codec will accept from and send to the wire.
    /// Larger packets will be rejected.
    pub fn set_max_packet_size(&mut self, max_packet_size: u32) {
        self.max_frame_len = max_packet_size;
    }

    fn decode_header(&mut self, src: &mut BytesMut) -> io::Result<Option<Header>> {
        if src.len() < Self::HEAD_LEN {
            return Ok(None);
        }

        let length = src.get_u32();
        if length > self.max_frame_len {
            return Err(io::Error::new(io::ErrorKind::InvalidData, IntegrityError::PacketTooBig));
        }
        src.reserve((length as usize).saturating_sub(src.len()));

        let seq = src.get_u16();
        if seq != self.decode_seq {
            return Err(io::Error::new(io::ErrorKind::InvalidData, IntegrityError::SeqSkipped));
        }
        self.decode_seq = self.decode_seq.wrapping_add(1);

        let checksum = src.get_u32();

        Ok(Some(Header { length, checksum }))
    }

    fn decode_data(&self, header: Header, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        if src.len() < header.length as usize {
            return Ok(None);
        }

        let data = src.split_to(header.length as usize);

        if header.checksum != CRC32.checksum(&data) {
            return Err(io::Error::new(io::ErrorKind::InvalidData, IntegrityError::DataCorrupted));
        }

        Ok(Some(data))
    }
}

impl Decoder for IntegrityCodec {
    type Item = BytesMut;
    type Error = io::Error;

    fn decode(&mut self, src: &mut BytesMut) -> io::Result<Option<BytesMut>> {
        let header = match self.state {
            DecodeState::Header => match self.decode_header(src)? {
                Some(header) => {
                    self.state = DecodeState::Data(header);
                    header
                }
                None => return Ok(None),
            },
            DecodeState::Data(header) => header,
        };

        match self.decode_data(header, src)? {
            Some(data) => {
                self.state = DecodeState::Header;
                src.reserve(Self::HEAD_LEN.saturating_sub(src.len()));

                Ok(Some(data))
            }
            None => Ok(None),
        }
    }
}

impl Encoder<Bytes> for IntegrityCodec {
    type Error = io::Error;

    fn encode(&mut self, data: Bytes, dst: &mut BytesMut) -> Result<(), io::Error> {
        if data.len() > self.max_frame_len as usize {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, IntegrityError::PacketTooBig));
        }

        dst.reserve(Self::HEAD_LEN + data.len());

        dst.put_u32(data.len() as u32);

        dst.put_u16(self.encode_seq);
        self.encode_seq = self.encode_seq.wrapping_add(1);

        dst.put_u32(CRC32.checksum(&data));

        dst.extend_from_slice(&data[..]);

        Ok(())
    }
}

impl Default for IntegrityCodec {
    fn default() -> Self {
        Self::new()
    }
}
