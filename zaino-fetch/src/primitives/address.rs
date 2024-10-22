//! Hold primitives relating to zcash addresses.

// use crate::primitives::chain::NetworkKind;
// use std::fmt;

// /// *** THE FOLLOWING CODE IS CURRENTLY UNUSED BY ZINGO-PROXY AND UNTESTED! ***
// /// ***                           TEST BEFORE USE                           ***

// /// Transparent Zcash Addresses
// ///
// /// In Bitcoin a single byte is used for the version field identifying
// /// the address type. In Zcash two bytes are used. For addresses on
// /// the production network, this and the encoded length cause the first
// /// two characters of the Base58Check encoding to be fixed as "t3" for
// /// P2SH addresses, and as "t1" for P2PKH addresses. (This does not
// /// imply that a transparent Zcash address can be parsed identically
// /// to a Bitcoin address just by removing the "t".)
// ///
// /// <https://zips.z.cash/protocol/protocol.pdf#transparentaddrencoding>
// ///
// /// *** UNTESTED - TEST BEFORE USE ***
// #[derive(Clone, Eq, PartialEq, Hash, serde::Deserialize, serde::Serialize)]
// pub enum TransparentAddress {
//     /// P2SH (Pay to Script Hash) addresses
//     PayToScriptHash {
//         /// Production, test, or other network
//         network_kind: NetworkKind,
//         /// 20 bytes specifying a script hash.
//         script_hash: [u8; 20],
//     },

//     /// P2PKH (Pay to Public Key Hash) addresses
//     PayToPublicKeyHash {
//         /// Production, test, or other network
//         network_kind: NetworkKind,
//         /// 20 bytes specifying a public key hash, which is a RIPEMD-160
//         /// hash of a SHA-256 hash of a compressed ECDSA key encoding.
//         pub_key_hash: [u8; 20],
//     },
// }

// impl fmt::Debug for TransparentAddress {
//     fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
//         let mut debug_struct = f.debug_struct("TransparentAddress");

//         match self {
//             TransparentAddress::PayToScriptHash {
//                 network_kind,
//                 script_hash,
//             } => debug_struct
//                 .field("network_kind", network_kind)
//                 .field("script_hash", &hex::encode(script_hash))
//                 .finish(),
//             TransparentAddress::PayToPublicKeyHash {
//                 network_kind,
//                 pub_key_hash,
//             } => debug_struct
//                 .field("network_kind", network_kind)
//                 .field("pub_key_hash", &hex::encode(pub_key_hash))
//                 .finish(),
//         }
//     }
// }
