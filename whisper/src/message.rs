// Copyright 2015-2017 Parity Technologies (UK) Ltd.
// This file is part of Parity.

// Parity is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity.  If not, see <http://www.gnu.org/licenses/>.

//! Whisper message parsing, handlers, and construction.

use std::fmt;
use std::time::{self, SystemTime, Duration};

use bigint::hash::{H256, H512};
use rlp::{self, DecoderError, RlpStream, UntrustedRlp};
use smallvec::SmallVec;
use tiny_keccak::Keccak;

/// Work-factor proved. Takes 3 parameters: size of message, time to live,
/// and hash.
///
/// Panics if size or TTL is zero.
pub fn work_factor_proved(size: u64, ttl: u64, hash: H256) -> f64 {
	assert!(size != 0 && ttl != 0);

	let leading_zeros = hash.0.iter().take_while(|&&x| x == 0).count();
	let spacetime = size as f64 * ttl as f64;

	(1u64 << leading_zeros) as f64 / spacetime
}

/// A topic of a message.
#[derive(Debug, Default, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Topic(pub [u8; 4]);

impl From<[u8; 4]> for Topic {
	fn from(x: [u8; 4]) -> Self {
		Topic(x)
	}
}

impl Topic {
	// set up to three bits in the 64-byte bloom passed.
	//
	// this takes 3 sets of 9 bits, treating each as an index in the range
	// 0..512 into the bloom and setting the corresponding bit in the bloom to 1.
	fn bloom_into(&self, bloom: &mut H512) {
		let mut set_bit = |idx: usize| {
			let idx = idx & 511;
			bloom[idx / 8] |= 1 << idx % 8;
		};

		let data = &self.0;
		let mut combined = ((data[0] as usize) << 24) |
			((data[1] as usize) << 16) |
			((data[2] as usize) << 8) |
			data[3] as usize;

		// take off the last 5 bits as we only use 27.
		combined >>= 5;

		set_bit(combined);
		set_bit(combined >> 9);
		set_bit(combined >> 18);
	}
}

impl rlp::Encodable for Topic {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.encoder().encode_value(&self.0);
	}
}

impl rlp::Decodable for Topic {
	fn decode(rlp: &UntrustedRlp) -> Result<Self, DecoderError> {
		use std::cmp;

		rlp.decoder().decode_value(|bytes| match bytes.len().cmp(&4) {
			cmp::Ordering::Less => Err(DecoderError::RlpIsTooShort),
			cmp::Ordering::Greater => Err(DecoderError::RlpIsTooBig),
			cmp::Ordering::Equal => {
				let mut t = [0u8; 4];
				t.copy_from_slice(bytes);
				Ok(Topic(t))
			}
		})
	}
}

/// Message errors.
#[derive(Debug)]
pub enum Error {
	Decoder(DecoderError),
	LivesTooLong,
	IssuedInFuture,
	ZeroTTL,
}

impl From<DecoderError> for Error {
	fn from(err: DecoderError) -> Self {
		Error::Decoder(err)
	}
}

impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match *self {
			Error::Decoder(ref err) => write!(f, "Failed to decode message: {}", err),
			Error::LivesTooLong => write!(f, "Message claims to be issued before the unix epoch."),
			Error::IssuedInFuture => write!(f, "Message issued in future."),
			Error::ZeroTTL => write!(f, "Message live for zero time."),
		}
	}
}

// Raw envelope struct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Envelope {
	expiry: u64,
	ttl: u64,
	topics: SmallVec<[Topic; 4]>,
	data: Vec<u8>,
	nonce: u64,
}

impl Envelope {
	fn proving_hash(&self) -> H256 {
		use byteorder::{BigEndian, ByteOrder};

		let mut buf = [0; 32];

		let mut stream = RlpStream::new_list(4);
		stream.append(&self.expiry)
			.append(&self.ttl)
			.append_list(&self.topics)
			.append(&self.data);

		let mut digest = Keccak::new_keccak256();
		digest.update(&*stream.drain());
		digest.update(&{
			let mut nonce_bytes = [0u8; 8];
			BigEndian::write_u64(&mut nonce_bytes, self.nonce);

			nonce_bytes
		});

		digest.finalize(&mut buf);
		H256(buf)
	}
}

impl rlp::Encodable for Envelope {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(5)
			.append(&self.expiry)
			.append(&self.ttl)
			.append_list(&self.topics)
			.append(&self.data)
			.append(&self.nonce);
	}
}

impl rlp::Decodable for Envelope {
	fn decode(rlp: &UntrustedRlp) -> Result<Self, DecoderError> {
		if rlp.item_count()? != 5 { return Err(DecoderError::RlpIncorrectListLen) }

		Ok(Envelope {
			expiry: rlp.val_at(0)?,
			ttl: rlp.val_at(1)?,
			topics: rlp.at(2)?.iter().map(|x| x.as_val()).collect::<Result<_, _>>()?,
			data: rlp.val_at(3)?,
			nonce: rlp.val_at(4)?,
		})
	}
}

/// Message creation parameters.
/// Pass this to `Message::create` to make a message.
pub struct CreateParams {
	/// time-to-live in seconds.
	pub ttl: u64,
	/// payload data.
	pub payload: Vec<u8>,
	/// Topics.
	pub topics: Vec<Topic>,
	/// How many milliseconds to spend proving work.
	pub work: u64,
}

/// A whisper message.
#[derive(Debug, PartialEq, Eq)]
pub struct Message {
	envelope: Envelope,
	bloom: H512,
	hash: H256,
	encoded_size: usize,
}

impl Message {
	/// Create a message from creation parameters.
	/// Panics if TTL is 0.
	pub fn create(params: CreateParams) -> Self {
		use byteorder::{BigEndian, ByteOrder};
		use rand::{Rng, SeedableRng, XorShiftRng};

		let mut rng = {
			let mut thread_rng = ::rand::thread_rng();

			XorShiftRng::from_seed(thread_rng.gen::<[u32; 4]>())
		};

		assert!(params.ttl > 0);

		let expiry = {
			let after_mining = SystemTime::now() + Duration::from_millis(params.work);
			let since_epoch = after_mining.duration_since(time::UNIX_EPOCH)
				.expect("time after now is after unix epoch; qed");

			// round up the sub-second to next whole second.
			since_epoch.as_secs() + if since_epoch.subsec_nanos() == 0 { 0 } else { 1 }
		};

		let start_digest = {
			let mut stream = RlpStream::new_list(4);
			stream.append(&expiry)
				.append(&params.ttl)
				.append_list(&params.topics)
				.append(&params.payload);

			let mut digest = Keccak::new_keccak256();
			digest.update(&*stream.drain());
			digest
		};

		let mut buf = [0; 32];
		let mut try_nonce = move |nonce: &[u8; 8]| {
			let mut digest = start_digest.clone();
			digest.update(&nonce[..]);
			digest.finalize(&mut buf[..]);

			buf.clone()
		};

		let mut nonce: [u8; 8] = rng.gen();
		let mut best_found = try_nonce(&nonce);

		let start = ::time::precise_time_ns();

		while ::time::precise_time_ns() <= start + params.work * 1_000_000 {
			let temp_nonce = rng.gen();
			let hash = try_nonce(&temp_nonce);

			if hash < best_found {
				nonce = temp_nonce;
				best_found = hash;
			}
		}

		let envelope = Envelope {
			expiry: expiry,
			ttl: params.ttl,
			topics: params.topics.into_iter().collect(),
			data: params.payload,
			nonce: BigEndian::read_u64(&nonce[..]),
		};

		debug_assert_eq!(H256(best_found.clone()), envelope.proving_hash());

		let encoded = ::rlp::encode(&envelope);

		Message::from_components(
			envelope,
			encoded.len(),
			SystemTime::now(),
		).expect("Message generated here known to be valid; qed")
	}

	/// Decode message from RLP and check for validity against system time.
	pub fn decode(rlp: UntrustedRlp, now: SystemTime) -> Result<Self, Error> {
		let envelope: Envelope = rlp.as_val()?;
		let encoded_size = rlp.as_raw().len();

		Message::from_components(envelope, encoded_size, now)
	}

	// create message from envelope, hash, and encoded size.
	// does checks for validity.
	fn from_components(envelope: Envelope, size: usize, now: SystemTime)
		-> Result<Self, Error>
	{
		const LEEWAY_SECONDS: u64 = 2;

		if envelope.expiry <= envelope.ttl { return Err(Error::LivesTooLong) }
		if envelope.ttl == 0 { return Err(Error::ZeroTTL) }

		let issue_time_adjusted = Duration::from_secs(envelope.expiry - envelope.ttl - LEEWAY_SECONDS);
		if time::UNIX_EPOCH + issue_time_adjusted > now {
			return Err(Error::IssuedInFuture);
		}

		// other validity checks?
		let mut bloom = H512::default();
		for topic in &envelope.topics {
			topic.bloom_into(&mut bloom);
		}

		let proving_hash = envelope.proving_hash();

		Ok(Message {
			envelope: envelope,
			bloom: bloom,
			hash: proving_hash,
			encoded_size: size,
		})
	}

	/// Get a reference to the envelope.
	pub fn envelope(&self) -> &Envelope {
		&self.envelope
	}

	/// Get the encoded size of the envelope.
	pub fn encoded_size(&self) -> usize {
		self.encoded_size
	}

	/// Get a uniquely identifying hash for the message.
	/// This is not equal to `sha3(rlp(message))
	pub fn hash(&self) -> &H256 {
		&self.hash
	}

	/// Get the bloom filter of the topics
	pub fn bloom(&self) -> &H512 {
		&self.bloom
	}

	/// Get the work proved by the hash.
	pub fn work_proved(&self) -> f64 {
		work_factor_proved(self.encoded_size as _, self.envelope.ttl, self.hash)
	}

	/// Get the expiry time.
	pub fn expiry(&self) -> SystemTime {
		time::UNIX_EPOCH + Duration::from_secs(self.envelope.expiry)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{self, Duration, SystemTime};
	use rlp::UntrustedRlp;

	fn unix_time(x: u64) -> SystemTime {
		time::UNIX_EPOCH + Duration::from_secs(x)
	}

	#[test]
	fn create_message() {
		let _ = Message::create(CreateParams {
			ttl: 100,
			payload: vec![1, 2, 3, 4],
			topics: Vec::new(),
			work: 50,
		});
	}

	#[test]
	fn round_trip() {
		let envelope = Envelope {
			expiry: 100_000,
			ttl: 30,
			data: vec![9; 256],
			topics: Default::default(),
			nonce: 1010101,
		};

		let encoded = ::rlp::encode(&envelope);
		let decoded = ::rlp::decode(&encoded);

		assert_eq!(envelope, decoded)
	}

	#[test]
	fn passes_checks() {
		let envelope = Envelope {
			expiry: 100_000,
			ttl: 30,
			data: vec![9; 256],
			topics: Default::default(),
			nonce: 1010101,
		};

		let encoded = ::rlp::encode(&envelope);

		for i in 0..30 {
			let now = unix_time(100_000 - i);
			Message::decode(UntrustedRlp::new(&*encoded), now).unwrap();
		}
	}

	#[test]
	#[should_panic]
	fn future_message() {
		let envelope = Envelope {
			expiry: 100_000,
			ttl: 30,
			data: vec![9; 256],
			topics: Default::default(),
			nonce: 1010101,
		};

		let encoded = ::rlp::encode(&envelope);

		let now = unix_time(100_000 - 1_000);
		Message::decode(UntrustedRlp::new(&*encoded), now).unwrap();
	}

	#[test]
	#[should_panic]
	fn pre_epoch() {
		let envelope = Envelope {
			expiry: 100_000,
			ttl: 200_000,
			data: vec![9; 256],
			topics: Default::default(),
			nonce: 1010101,
		};

		let encoded = ::rlp::encode(&envelope);

		let now = unix_time(95_000);
		Message::decode(UntrustedRlp::new(&*encoded), now).unwrap();
	}
}
