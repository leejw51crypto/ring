// Copyright 2015-2016 Brian Smith.
//
// Permission to use, copy, modify, and/or distribute this software for any
// purpose with or without fee is hereby granted, provided that the above
// copyright notice and this permission notice appear in all copies.
//
// THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHORS DISCLAIM ALL WARRANTIES
// WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
// MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHORS BE LIABLE FOR ANY
// SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
// WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN ACTION
// OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF OR IN
// CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

//! Authenticated Encryption with Associated Data (AEAD).
//!
//! See [Authenticated encryption: relations among notions and analysis of the
//! generic composition paradigm][AEAD] for an introduction to the concept of
//! AEADs.
//!
//! C analog: `GFp/aead.h`
//!
//! Go analog: [`crypto.cipher.AEAD`]
//!
//! [AEAD]: http://www-cse.ucsd.edu/~mihir/papers/oem.html
//! [`crypto.cipher.AEAD`]: https://golang.org/pkg/crypto/cipher/#AEAD

use self::{
    block::{Block, BLOCK_LEN},
    nonce::NonceRef,
};
use crate::{
    constant_time, cpu, error,
    polyfill::{self, convert::*},
};

pub use self::{
    aes_gcm::{AES_128_GCM, AES_256_GCM},
    chacha20_poly1305::CHACHA20_POLY1305,
};

/// A key for authenticating and decrypting (“opening”) AEAD-protected data.
///
/// C analog: `EVP_AEAD_CTX` with direction `evp_aead_open`
///
/// Go analog: [`crypto.cipher.AEAD`]
pub struct OpeningKey {
    key: Key,
}

impl OpeningKey {
    /// Create a new opening key.
    ///
    /// `key_bytes` must be exactly `algorithm.key_len` bytes long.
    ///
    /// C analogs: `EVP_AEAD_CTX_init_with_direction` with direction
    ///            `evp_aead_open`, `EVP_AEAD_CTX_init`.
    ///
    /// Go analog:
    ///   [`crypto.aes.NewCipher`](https://golang.org/pkg/crypto/aes/#NewCipher)
    /// + [`crypto.cipher.NewGCM`](https://golang.org/pkg/crypto/cipher/#NewGCM)
    #[inline]
    pub fn new(
        algorithm: &'static Algorithm, key_bytes: &[u8],
    ) -> Result<OpeningKey, error::Unspecified> {
        Ok(OpeningKey {
            key: Key::new(algorithm, key_bytes)?,
        })
    }

    /// The key's AEAD algorithm.
    ///
    /// C analog: `EVP_AEAD_CTX.aead`
    #[inline(always)]
    pub fn algorithm(&self) -> &'static Algorithm { self.key.algorithm() }
}

/// Authenticates and decrypts (“opens”) data in place.
///
/// The input may have a prefix that is `in_prefix_len` bytes long; any such
/// prefix is ignored on input and overwritten on output. The last
/// `key.algorithm().tag_len()` bytes of `ciphertext_and_tag_modified_in_place`
/// must be the tag. The part of `ciphertext_and_tag_modified_in_place` between
/// the prefix and the tag is the input ciphertext.
///
/// When `open_in_place()` returns `Ok(plaintext)`, the decrypted output is
/// `plaintext`, which is
/// `&mut ciphertext_and_tag_modified_in_place[..plaintext.len()]`. That is,
/// the output plaintext overwrites some or all of the prefix and ciphertext.
/// To put it another way, the ciphertext is shifted forward `in_prefix_len`
/// bytes and then decrypted in place. To have the output overwrite the input
/// without shifting, pass 0 as `in_prefix_len`.
///
/// When `open_in_place()` returns `Err(..)`,
/// `ciphertext_and_tag_modified_in_place` may have been overwritten in an
/// unspecified way.
///
/// The shifting feature is useful in the case where multiple packets are
/// being reassembled in place. Consider this example where the peer has sent
/// the message “Split stream reassembled in place” split into three sealed
/// packets:
///
/// ```ascii-art
///                 Packet 1                  Packet 2                 Packet 3
/// Input:  [Header][Ciphertext][Tag][Header][Ciphertext][Tag][Header][Ciphertext][Tag]
///                      |         +--------------+                        |
///               +------+   +-----+    +----------------------------------+
///               v          v          v
/// Output: [Plaintext][Plaintext][Plaintext]
///        “Split stream reassembled in place”
/// ```
///
/// Let's say the header is always 5 bytes (like TLS 1.2) and the tag is always
/// 16 bytes (as for AES-GCM and ChaCha20-Poly1305). Then for this example,
/// `in_prefix_len` would be `5` for the first packet, `(5 + 16) + 5` for the
/// second packet, and `(2 * (5 + 16)) + 5` for the third packet.
///
/// (The input/output buffer is expressed as combination of `in_prefix_len`
/// and `ciphertext_and_tag_modified_in_place` because Rust's type system
/// does not allow us to have two slices, one mutable and one immutable, that
/// reference overlapping memory.)
///
/// C analog: `EVP_AEAD_CTX_open`
///
/// Go analog: [`AEAD.Open`](https://golang.org/pkg/crypto/cipher/#AEAD)
pub fn open_in_place<'a>(
    key: &OpeningKey, nonce: &[u8], ad: &[u8], in_prefix_len: usize,
    ciphertext_and_tag_modified_in_place: &'a mut [u8],
) -> Result<&'a mut [u8], error::Unspecified> {
    let nonce = NonceRef::try_from(nonce)?;
    let ciphertext_and_tag_len = ciphertext_and_tag_modified_in_place
        .len()
        .checked_sub(in_prefix_len)
        .ok_or(error::Unspecified)?;
    let ciphertext_len = ciphertext_and_tag_len
        .checked_sub(TAG_LEN)
        .ok_or(error::Unspecified)?;
    check_per_nonce_max_bytes(key.key.algorithm, ciphertext_len)?;
    let (in_out, received_tag) =
        ciphertext_and_tag_modified_in_place.split_at_mut(in_prefix_len + ciphertext_len);
    let Tag(calculated_tag) =
        (key.key.algorithm.open)(&key.key.inner, nonce, &ad, in_prefix_len, in_out);
    if constant_time::verify_slices_are_equal(calculated_tag.as_ref(), received_tag).is_err() {
        // Zero out the plaintext so that it isn't accidentally leaked or used
        // after verification fails. It would be safest if we could check the
        // tag before decrypting, but some `open` implementations interleave
        // authentication with decryption for performance.
        for b in &mut in_out[..ciphertext_len] {
            *b = 0;
        }
        return Err(error::Unspecified);
    }
    // `ciphertext_len` is also the plaintext length.
    Ok(&mut in_out[..ciphertext_len])
}

/// A key for encrypting and signing (“sealing”) data.
///
/// C analog: `EVP_AEAD_CTX` with direction `evp_aead_seal`.
///
/// Go analog: [`AEAD`](https://golang.org/pkg/crypto/cipher/#AEAD)
pub struct SealingKey {
    key: Key,
}

impl SealingKey {
    /// C analogs: `EVP_AEAD_CTX_init_with_direction` with direction
    ///            `evp_aead_seal`, `EVP_AEAD_CTX_init`.
    ///
    /// Go analog:
    ///   [`crypto.aes.NewCipher`](https://golang.org/pkg/crypto/aes/#NewCipher)
    /// + [`crypto.cipher.NewGCM`](https://golang.org/pkg/crypto/cipher/#NewGCM)
    #[inline]
    pub fn new(
        algorithm: &'static Algorithm, key_bytes: &[u8],
    ) -> Result<SealingKey, error::Unspecified> {
        Ok(SealingKey {
            key: Key::new(algorithm, key_bytes)?,
        })
    }

    /// The key's AEAD algorithm.
    ///
    /// C analog: `EVP_AEAD_CTX.aead`
    #[inline(always)]
    pub fn algorithm(&self) -> &'static Algorithm { self.key.algorithm() }
}

/// Encrypts and signs (“seals”) data in place.
///
/// `nonce` must be unique for every use of the key to seal data.
///
/// The input is `in_out[..(in_out.len() - out_suffix_capacity)]`; i.e. the
/// input is the part of `in_out` that precedes the suffix. When
/// `seal_in_place()` returns `Ok(out_len)`, the encrypted and signed output is
/// `in_out[..out_len]`; i.e.  the output has been written over input and at
/// least part of the data reserved for the suffix. (The input/output buffer
/// is expressed this way because Rust's type system does not allow us to have
/// two slices, one mutable and one immutable, that reference overlapping
/// memory at the same time.)
///
/// `out_suffix_capacity` must be at least `key.algorithm().tag_len()`. See
/// also `MAX_TAG_LEN`.
///
/// `ad` is the additional authenticated data, if any.
///
/// C analog: `EVP_AEAD_CTX_seal`.
///
/// Go analog: [`AEAD.Seal`](https://golang.org/pkg/crypto/cipher/#AEAD)
pub fn seal_in_place(
    key: &SealingKey, nonce: &[u8], ad: &[u8], in_out: &mut [u8], out_suffix_capacity: usize,
) -> Result<usize, error::Unspecified> {
    if out_suffix_capacity < key.key.algorithm.tag_len() {
        return Err(error::Unspecified);
    }
    let nonce = NonceRef::try_from(nonce)?;
    let in_out_len = in_out
        .len()
        .checked_sub(out_suffix_capacity)
        .ok_or(error::Unspecified)?;
    check_per_nonce_max_bytes(key.key.algorithm, in_out_len)?;
    let (in_out, tag_out) = in_out.split_at_mut(in_out_len);

    let tag_out: &mut [u8; TAG_LEN] = tag_out.try_into_()?;
    let Tag(tag) = (key.key.algorithm.seal)(&key.key.inner, nonce, ad, in_out);
    tag_out.copy_from_slice(tag.as_ref());

    Ok(in_out_len + TAG_LEN)
}

/// `OpeningKey` and `SealingKey` are type-safety wrappers around `Key`, which
/// does all the actual work via the C AEAD interface.
///
/// C analog: `EVP_AEAD_CTX`
struct Key {
    inner: KeyInner,
    algorithm: &'static Algorithm,
}

#[allow(variant_size_differences)]
enum KeyInner {
    AesGcm(aes_gcm::Key),
    ChaCha20Poly1305(chacha20_poly1305::Key),
}

impl Key {
    fn new(algorithm: &'static Algorithm, key_bytes: &[u8]) -> Result<Self, error::Unspecified> {
        cpu::cache_detected_features();
        Ok(Key {
            inner: (algorithm.init)(key_bytes)?,
            algorithm,
        })
    }

    /// The key's AEAD algorithm.
    #[inline(always)]
    fn algorithm(&self) -> &'static Algorithm { self.algorithm }
}

/// An AEAD Algorithm.
///
/// C analog: `EVP_AEAD`
///
/// Go analog:
///     [`crypto.cipher.AEAD`](https://golang.org/pkg/crypto/cipher/#AEAD)
pub struct Algorithm {
    init: fn(key: &[u8]) -> Result<KeyInner, error::Unspecified>,

    seal: fn(key: &KeyInner, nonce: NonceRef, ad: &[u8], in_out: &mut [u8]) -> Tag,
    open: fn(
        ctx: &KeyInner,
        nonce: NonceRef,
        ad: &[u8],
        in_prefix_len: usize,
        in_out: &mut [u8],
    ) -> Tag,

    key_len: usize,
    id: AlgorithmID,

    /// Use `max_input_len!()` to initialize this.
    // TODO: Make this `usize`.
    max_input_len: u64,
}

const fn max_input_len(block_len: usize, overhead_blocks_per_nonce: usize) -> u64 {
    // Each of our AEADs use a 32-bit block counter so the maximum is the
    // largest input that will not overflow the counter.
    ((1u64 << 32) - polyfill::u64_from_usize(overhead_blocks_per_nonce))
        * polyfill::u64_from_usize(block_len)
}

impl Algorithm {
    /// The length of the key.
    ///
    /// C analog: `EVP_AEAD_key_length`
    #[inline(always)]
    pub fn key_len(&self) -> usize { self.key_len }

    /// The length of a tag.
    ///
    /// See also `MAX_TAG_LEN`.
    ///
    /// C analog: `EVP_AEAD_max_overhead`
    ///
    /// Go analog:
    ///   [`crypto.cipher.AEAD.Overhead`](https://golang.org/pkg/crypto/cipher/#AEAD)
    #[inline(always)]
    pub fn tag_len(&self) -> usize { TAG_LEN }

    /// The length of the nonces.
    ///
    /// C analog: `EVP_AEAD_nonce_length`
    ///
    /// Go analog:
    ///   [`crypto.cipher.AEAD.NonceSize`](https://golang.org/pkg/crypto/cipher/#AEAD)
    #[inline(always)]
    pub fn nonce_len(&self) -> usize { self::nonce::NONCE_LEN }
}

derive_debug_via_self!(Algorithm, self.id);

#[derive(Debug, Eq, PartialEq)]
enum AlgorithmID {
    AES_128_GCM,
    AES_256_GCM,
    CHACHA20_POLY1305,
}

impl PartialEq for Algorithm {
    fn eq(&self, other: &Self) -> bool { self.id == other.id }
}

impl Eq for Algorithm {}

/// An authentication tag.
#[must_use]
#[repr(C)]
struct Tag(Block);

// All the AEADs we support use 128-bit tags.
const TAG_LEN: usize = BLOCK_LEN;

/// The maximum length of a tag for the algorithms in this module.
pub const MAX_TAG_LEN: usize = TAG_LEN;

fn check_per_nonce_max_bytes(alg: &Algorithm, in_out_len: usize) -> Result<(), error::Unspecified> {
    if polyfill::u64_from_usize(in_out_len) > alg.max_input_len {
        return Err(error::Unspecified);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum Direction {
    Opening { in_prefix_len: usize },
    Sealing,
}

mod aes;
mod aes_gcm;
mod block;
mod chacha;
mod chacha20_poly1305;
pub mod chacha20_poly1305_openssh;
mod gcm;
mod nonce;
mod poly1305;
mod shift;
