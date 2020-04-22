use imp::AesmClient;
pub use error::{AesmError, Error, Result};
use protobuf::Message;
use std::io::{Read, Write};
use std::mem::size_of;
use byteorder::{LittleEndian, NativeEndian, ReadBytesExt, WriteBytesExt};
use {
    AesmRequest, FromResponse, QuoteInfo, QuoteResult, QuoteType,
    Request_GetQuoteRequest, Request_InitQuoteRequest,
};
// FIXME: remove conditional compilation after resolving https://github.com/fortanix/rust-sgx/issues/31
#[cfg(not(target_env = "sgx"))]
use std::time::Duration;


/// This timeout is an argument in AESM request protobufs.
///
/// This value should be used for requests that can be completed locally, i.e.
/// without network interaction.
pub(super) const LOCAL_AESM_TIMEOUT_US: u32 = 1_000_000;
/// This timeout is an argument in AESM request protobufs.
///
/// This value should be used for requests that might need interaction with
/// remote servers, such as provisioning EPID.
pub(super) const REMOTE_AESM_TIMEOUT_US: u32 = 30_000_000;

impl AesmClient {
    pub fn try_connect(&self) -> Result<()> {
        self.open_socket().map(|_| ())
    }

    pub(super) fn transact<T: AesmRequest>(&self, req: T) -> Result<T::Response> {
        let mut sock = self.open_socket()?;

        // FIXME: remove conditional compilation after resolving https://github.com/fortanix/rust-sgx/issues/31
        #[cfg(not(target_env = "sgx"))]
        let _ = sock.set_read_timeout(req.get_timeout().map(|t| Duration::from_micros(t as _)))?;

        // impl Write appends to the vector. Reserve space to fill in the
        // length after serializing.
        let mut req_bytes = vec![0u8; size_of::<u32>()];
        req.into()
            .write_to_writer(&mut req_bytes)
            .expect("Failed to serialize protobuf");
        let req_len = (req_bytes.len() - size_of::<u32>()) as u32;
        (&mut req_bytes[0..size_of::<u32>()]).write_u32::<NativeEndian>(req_len)?;
        sock.write_all(&req_bytes)?;

        let res_len = sock.read_u32::<NativeEndian>()?;
        let mut res_bytes = vec![0; res_len as usize];
        sock.read_exact(&mut res_bytes)?;

        let res = T::Response::from_response(protobuf::parse_from_bytes(&res_bytes))?;
        Ok(res)
    }

    /// Obtain target info from QE.
    pub fn init_quote(&self) -> Result<QuoteInfo> {
        let mut req = Request_InitQuoteRequest::new();
        req.set_timeout(LOCAL_AESM_TIMEOUT_US);
        let mut res = self.transact(req)?;

        let (target_info, mut gid) = (res.take_targetInfo(), res.take_gid());

        // AESM gives it to us little-endian, we want big-endian for writing into IAS URL with to_hex()
        gid.reverse();

        Ok(QuoteInfo { target_info, gid })
    }

    /// Obtain remote attestation quote from QE.
    pub fn get_quote(
        &self,
        session: &QuoteInfo,
        report: Vec<u8>,
        spid: Vec<u8>,
        sig_rl: Vec<u8>,
        quote_type: QuoteType,
        nonce: Vec<u8>,
    ) -> Result<QuoteResult> {
        let mut req = Request_GetQuoteRequest::new();
        req.set_report(report);
        req.set_quote_type(quote_type.into());
        req.set_spid(spid);
        req.set_nonce(nonce);
        req.set_buf_size(session.quote_buffer_size(&sig_rl));
        if sig_rl.len() != 0 {
            req.set_sig_rl(sig_rl);
        }
        req.set_qe_report(true);

        req.set_timeout(REMOTE_AESM_TIMEOUT_US);

        let mut res = self.transact(req)?;

        let (mut quote, qe_report) = (res.take_quote(), res.take_qe_report());

        // AESM allocates a buffer of the size we supplied and returns the whole
        // thing to us, regardless of how much space QE needed. Trim the excess.
        // The signature length is a little endian word at offset 432 in the quote
        // structure. See "QUOTE Structure" in the IAS API Spec.
        let sig_len = (&quote[432..436]).read_u32::<LittleEndian>().unwrap();
        let new_len = 436 + sig_len as usize;
        if quote.len() < new_len {
            // Quote is already too short, should not happen.
            // Probably we are interpreting the quote structure incorrectly.
            return Err(Error::InvalidQuoteSize);
        }
        quote.truncate(new_len);

        Ok(QuoteResult::new(quote, qe_report))
    }

}
