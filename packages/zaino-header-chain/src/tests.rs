//! Against the real chain: headers captured by `examples/capture_headers.rs`

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::protocol::{median_time_past, MEDIAN_TIME_SPAN};
use zaino_primitives::types::{BlockRef, FinalityDepth, HeaderError};
use zaino_source::testing::{all_at, MockChain};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::BlockHash;

use crate::params::AVERAGING_WINDOW;
use crate::rules::{equihash_valid, expected_bits, header_work, in_context};
use crate::rules::{validate, Ancestor};
use crate::testing::insert;
use crate::{decode_header, Header, HeaderChain, Inserted, Params};
use crate::{DecodeError, Fault, Rejected};

const HEADER_LEN: usize = 1487;
const GENESIS_RANGE: &[u8] = include_bytes!("../tests/fixtures/mainnet_0.headers");
const BLOSSOM_RANGE: &[u8] = include_bytes!("../tests/fixtures/mainnet_653500.headers");
const RECENT_RANGE: &[u8] = include_bytes!("../tests/fixtures/mainnet_3508500.headers");
const TESTNET_MIN_DIFFICULTY_RANGE: &[u8] =
    include_bytes!("../tests/fixtures/testnet_299000.headers");
const TESTNET_BLOSSOM_RANGE: &[u8] = include_bytes!("../tests/fixtures/testnet_583800.headers");

fn headers(range: &[u8]) -> Vec<Header> {
    range.chunks_exact(HEADER_LEN).map(|raw| decode_header(raw).expect("captured header")).collect()
}

fn height(h: u32) -> BlockHeight {
    BlockHeight::from(h)
}

/// Ancestors of `headers[at]`, newest first (all the range holds, up to `CONTEXT`)
fn context(headers: &[Header], at: usize) -> Vec<Ancestor> {
    let from = at.saturating_sub(crate::rules::CONTEXT);
    headers[from..at].iter().rev().map(|h| Ancestor { bits: h.bits(), time: h.time() }).collect()
}

/// Every captured header passes `validate`, its nBits reproduced exactly
///
/// - mainnet: genesis's first 300 (the limit while the window runs off genesis, height 1
///   included), Blossom's spacing switch at 653,600, the tip in 2026
/// - testnet: minimum difficulty from 299,188 (gap > 6 × 150 s) and Blossom at 584,000 (> 6 × 75 s)
/// - mid-chain ranges from their 29th header (17-block window + 11 medians held)
/// - each header links to the one before by hash
#[test]
fn every_captured_header_passes_and_its_nbits_is_reproduced() {
    let window = AVERAGING_WINDOW + MEDIAN_TIME_SPAN;
    #[rustfmt::skip]
    let ranges = [
        (Params::mainnet(), 0,         GENESIS_RANGE,                1,      0),
        (Params::mainnet(), 653_500,   BLOSSOM_RANGE,                window, 0),
        (Params::mainnet(), 3_508_500, RECENT_RANGE,                 window, 0),
        (Params::testnet(), 299_000,   TESTNET_MIN_DIFFICULTY_RANGE, window, 3),
        (Params::testnet(), 583_800,   TESTNET_BLOSSOM_RANGE,        window, 3),
    ];
    for (params, start, range, first, min_difficulty) in ranges {
        let headers = headers(range);
        let now = i64::from(headers.last().expect("non-empty").time());
        let mut at_limit = 0;
        for at in first..headers.len() {
            let (header, at_height) = (&headers[at], height(start + at as u32));
            assert_eq!(header.prev_hash(), headers[at - 1].hash(), "{at_height:?} links");
            let context = context(&headers, at);
            let expected =
                expected_bits(&params, at_height, header.time(), context[0].time, &context);
            assert_eq!(expected, Some(header.bits()), "{at_height:?}: nBits reproduced");
            let valid = validate(&params, header, at_height, &context, now);
            assert_eq!(valid, Ok(()), "{at_height:?}");
            at_limit += usize::from(header.bits() == params.limit_bits() && start > 0);
        }
        assert_eq!(at_limit, min_difficulty, "{start}: minimum-difficulty blocks");
    }
}

/// Anchored mid-chain at mainnet 3,508,500 (no history below), the next 200 real headers inserted
/// as a trusted validator's (no rule run), each the new best, vouched as it lands, finalized
/// behind a 10-block bound
///
/// - work counted from the anchor
/// - only the newest max(2 · depth, CONTEXT) = 113 finalized headers answer by height, older ones
///   = on the best chain by definition (finalized), a header below them = an orphan
/// - locator ends at the finalized tip
/// - re-anchor drops everything held before it
#[test]
fn a_chain_anchored_mid_mainnet_follows_trusted_headers_with_no_history_below() {
    let headers = headers(RECENT_RANGE);
    let at = |index: usize| BlockRef {
        hash: headers[index].hash(),
        height: height(3_508_500 + index as u32),
    };
    let depth = FinalityDepth::new(NonZeroU32::new(10).expect("nz"));
    let mut chain = HeaderChain::new(depth);
    assert_eq!(
        (chain.best(), chain.verified().is_some()),
        (None, false),
        "nothing before an anchor"
    );
    chain.anchor(&headers[0], at(0).height);

    for (index, header) in headers[1..=200].iter().enumerate() {
        assert_eq!(chain.insert(header), Ok(Inserted::Best { reorg: false }));
        chain.vouch(at(index + 1));
        if let Some(boundary) = chain.finalizable() {
            chain.finalize(boundary);
        }
        chain.check();
    }
    assert_eq!(chain.finalized_tip(), Some(at(190)));
    assert_eq!(chain.tree_len(), 10, "only the non-finalized window in the tree");
    assert_eq!(chain.insert(&headers[185]), Ok(Inserted::Known), "a kept finalized");
    assert_eq!(
        chain.insert(&headers[50]),
        Err(Rejected::Orphan),
        "below the finalized headers kept"
    );
    assert_eq!(
        chain.finalized_headers_kept(),
        crate::rules::CONTEXT,
        "depth 10: the rules' context wins"
    );
    let best = chain.best().expect("anchored");
    let work: u128 = headers[1..=200].iter().map(|h| header_work(h.bits()).expect("valid")).sum();
    assert_eq!((best.block, best.cumulative_work), (at(200), work), "work from the anchor");

    let verified = chain.verified().expect("anchored");
    for index in [78, 189, 190, 191, 200] {
        let header = verified.header_at(at(index).height).expect("held");
        let real = &headers[index];
        assert_eq!((header.hash, header.time), (real.hash(), real.time()), "{index}");
    }
    assert_eq!(verified.header_at(at(77).height), None, "past the 113 finalized headers kept");
    assert!(verified.on_best(at(5)) && verified.on_best(at(77)), "finalized by definition");
    let stranger = BlockRef { hash: BlockHash([7; 32]), height: at(195).height };
    assert!(!verified.on_best(stranger) && !verified.holds(stranger), "not ours above finalized");
    assert_eq!(verified.hash_at(at(201).height), None, "above the best");
    let locator: Vec<BlockHash> = (190..=200).rev().map(|index| headers[index].hash()).collect();
    assert_eq!(verified.locator(), locator, "tip, then consecutive down to the finalized tip");

    chain.anchor(&headers[250], at(250).height);
    assert_eq!((chain.finalized_tip(), chain.tree_len()), (Some(at(250)), 0), "re-anchored");
    assert_eq!(chain.insert(&headers[251]), Ok(Inserted::Best { reorg: false }));
    chain.check();
}

/// Anchored at mainnet 3,508,500, trusted 1..=27, then untrusted (every rule over the ancestors the
/// chain holds), finalized behind a 10-block bound so the ancestors read cross into the finalized
/// headers
///
/// - right after the anchor: too few held to judge (`ShortContext`: the caller's, not the header's)
/// - 28..=200: real headers, each the new best
/// - at 201: wrong nBits, broken solution (both: hash above target), local clock 2 h + 1 s behind
///   → each refused, nothing inserted
/// - then its real self accepted, a missing parent = orphan (difficulty and time rules mutation
///   by mutation: the test below)
#[test]
fn untrusted_headers_extend_the_chain_only_when_every_rule_holds() {
    let mainnet = Params::mainnet();
    let headers = headers(RECENT_RANGE);
    let at = |index: usize| BlockRef {
        hash: headers[index].hash(),
        height: height(3_508_500 + index as u32),
    };
    let now = i64::from(headers.last().expect("non-empty").time());
    let mut chain = HeaderChain::new(FinalityDepth::new(NonZeroU32::new(10).expect("nz")));
    chain.anchor(&headers[0], at(0).height);
    let short = chain.insert_untrusted(&headers[1], &mainnet, now);
    let needed = AVERAGING_WINDOW + MEDIAN_TIME_SPAN;
    assert_eq!(short, Err(Rejected::ShortContext { held: 1, needed }));
    for header in &headers[1..28] {
        assert_eq!(chain.insert(header), Ok(Inserted::Best { reorg: false }));
    }
    for (index, header) in headers.iter().enumerate().take(201).skip(28) {
        let inserted = chain.insert_untrusted(header, &mainnet, now);
        assert_eq!(inserted, Ok(Inserted::Best { reorg: false }), "{index}");
        chain.vouch(at(index));
        if let Some(boundary) = chain.finalizable() {
            chain.finalize(boundary);
        }
        chain.check();
    }
    assert_eq!(
        chain.finalized_tip(),
        Some(at(190)),
        "ancestors read across the finalized headers + 10 nodes"
    );

    let real = &headers[201];
    let mutated = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut raw = real.bytes().to_vec();
        edit(&mut raw);
        decode_header(&raw).expect("same shape")
    };
    // any edit moves the hash: proof of work refuses it before the context rules run
    let bits = real.bits() + 1;
    let wrong_bits = mutated(&|raw| raw[104..108].copy_from_slice(&bits.to_le_bytes()));
    let flipped = mutated(&|raw| *raw.last_mut().expect("solution") ^= 1);
    let skewed = i64::from(real.time()) - 2 * 3600 - 1;
    let future = Rejected::FromTheFuture { time: real.time(), max: skewed + 2 * 3600 };
    let refused = [
        chain.insert_untrusted(&wrong_bits, &mainnet, now),
        chain.insert_untrusted(&flipped, &mainnet, now),
        chain.insert_untrusted(real, &mainnet, skewed),
    ];
    let above = |bits| Err(Rejected::AboveTarget { bits });
    assert_eq!(refused, [above(bits), above(real.bits()), Err(future)]);
    assert_eq!(chain.best().map(|best| best.block), Some(at(200)), "nothing refused inserted");
    let accepted = chain.insert_untrusted(real, &mainnet, now);
    assert_eq!(accepted, Ok(Inserted::Best { reorg: false }));
    assert_eq!(chain.insert_untrusted(&headers[203], &mainnet, now), Err(Rejected::Orphan));
    chain.check();
}

/// Real headers, mutated one field at a time: each refused by the rule it breaks
///
/// - mainnet 3,508,600: Equihash, hash ≤ target, nBits, time, version (int32), nBits encoding
/// - a regtest header: version > 4 valid, a non-minimal solution length refused by decode
/// - testnet NU7: 102-block window at 25 s decides nBits (the 17-block rule's is refused)
/// - testnet minimum-difficulty gap stays 450 s across Blossom → NU7 (451 qualifies, 450 not)
/// - linkage in the chain: an unknown parent, a parent off the finalized chain
#[test]
fn each_mutation_is_refused_by_its_own_rule() {
    let mainnet = Params::mainnet();
    let headers = headers(RECENT_RANGE);
    let at = 100;
    let (real, at_height) = (&headers[at], height(3_508_500 + at as u32));
    let context = context(&headers, at);
    let now = i64::from(real.time());
    let median_time_past = median_time_past(context.iter().map(|a| a.time)).expect("held");
    let mutated = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut raw = real.bytes().to_vec();
        edit(&mut raw);
        decode_header(&raw).expect("same shape")
    };
    let set_u32 = |at: usize, value: u32| {
        move |raw: &mut Vec<u8>| raw[at..at + 4].copy_from_slice(&value.to_le_bytes())
    };
    let alone = |header: &Header| crate::rules::alone(&mainnet, header);
    let in_place =
        |header: &Header, now: i64| in_context(&mainnet, header, at_height, &context, now);

    assert_eq!((alone(real), in_place(real, now)), (Ok(()), Ok(())));
    assert!(equihash_valid(real).is_ok());
    let flipped = mutated(&|raw| *raw.last_mut().expect("solution") ^= 1);
    assert_eq!(equihash_valid(&flipped), Err(Rejected::Solution));
    assert_eq!(alone(&flipped), Err(Rejected::AboveTarget { bits: real.bits() }), "hash moved");
    for bits in [real.bits() + 1, real.bits() - 1] {
        let wrong = mutated(&set_u32(104, bits));
        let expected = real.bits();
        assert_eq!(in_place(&wrong, now), Err(Rejected::Difficulty { bits, expected }));
    }
    let early = mutated(&set_u32(100, median_time_past));
    let too_early = Rejected::TimeTooEarly { time: median_time_past, median_time_past };
    assert_eq!(in_place(&early, now), Err(too_early));
    let late = median_time_past + 90 * 60 + 1;
    let after_window = mutated(&set_u32(100, late));
    let max = i64::from(median_time_past) + 90 * 60;
    let too_late = Rejected::TimeTooLate { time: late, max };
    assert_eq!(in_place(&after_window, i64::from(late)), Err(too_late));
    let skewed = now - 2 * 3600 - 1;
    let future = Rejected::FromTheFuture { time: real.time(), max: skewed + 2 * 3600 };
    assert_eq!(in_place(real, skewed), Err(future), "local clock 2 h + 1 s behind the header");
    assert_eq!([future.fault(), too_late.fault()], [Fault::Deferred, Fault::Sender], "clock waits");
    assert_eq!(alone(&mutated(&set_u32(0, 3))), Err(Rejected::Version { version: 3 }));
    let high_bit = mutated(&set_u32(0, 0x8000_0004));
    let negative = Rejected::Version { version: i32::from_le_bytes(0x8000_0004u32.to_le_bytes()) };
    assert_eq!(alone(&high_bit), Err(negative), "nVersion is int32: high bit set = negative");
    let invalid_bits = mutated(&set_u32(104, 0x0180_0000));
    assert_eq!(alone(&invalid_bits), Err(Rejected::Bits { bits: 0x0180_0000 }));

    let regtest = Params::regtest(all_at(height(1)));
    let mut builder = MockChain::regtest();
    let raw = builder.header_bytes(builder.genesis().hash);
    let versioned = |version: u32| {
        let mut raw = raw.clone();
        raw[..4].copy_from_slice(&version.to_le_bytes());
        crate::rules::alone(&regtest, &decode_header(&raw).expect("shape"))
    };
    assert_eq!([4, 5, 0x2000_0000].map(versioned), [Ok(()), Ok(()), Ok(())], "> 4 stays valid");
    let mut padded = raw[..140].to_vec();
    padded.extend([0xfd, 36, 0]);
    padded.extend(&raw[141..]);
    let non_minimal = HeaderError::NonMinimalLength { len: 36 };
    assert_eq!(decode_header(&padded), Err(DecodeError::Header(non_minimal)));

    let testnet = Params::testnet();
    let pre_nu7 = Params { schedule: LocalNetwork { nu7: None, ..testnet.schedule }, ..testnet };
    let nu7 = height(4_465_026 + 50);
    // 113 ancestors 25 s apart, 0x1d00ffff then a harder 0x1c7fffff for the newest 40
    let ancestors: Vec<Ancestor> = (0..crate::rules::CONTEXT as u32)
        .map(|back| Ancestor {
            bits: if back < 40 { 0x1c7f_ffff } else { 0x1d00_ffff },
            time: 1_800_000_000 - 25 * back,
        })
        .collect();
    let next = ancestors[0].time + 25;
    let ruled = |params: &Params| expected_bits(params, nu7, next, ancestors[0].time, &ancestors);
    let (window_102, window_17) = (ruled(&testnet), ruled(&pre_nu7));
    assert_ne!(window_102, window_17, "the window decides: 102 reaches the easier blocks");
    let template = &self::headers(TESTNET_BLOSSOM_RANGE)[200];
    let claiming = |bits: u32, time: u32| {
        let mut raw = template.bytes().to_vec();
        raw[100..104].copy_from_slice(&time.to_le_bytes());
        raw[104..108].copy_from_slice(&bits.to_le_bytes());
        decode_header(&raw).expect("shape")
    };
    let expected = window_102.expect("adjusted");
    let stale_rule = claiming(window_17.expect("adjusted"), next);
    let refused = in_context(&testnet, &stale_rule, nu7, &ancestors, i64::from(next));
    let bits = stale_rule.bits();
    assert_eq!(refused, Err(Rejected::Difficulty { bits, expected }), "17-block nBits after NU7");
    let accepted = in_context(&testnet, &claiming(expected, next), nu7, &ancestors, 2_000_000_000);
    assert!(accepted.is_ok(), "{accepted:?}");
    let gap = |params: &Params, at: BlockHeight, gap: u32| {
        let time = ancestors[0].time + gap;
        expected_bits(params, at, time, ancestors[0].time, &ancestors) == Some(0x2007_ffff)
    };
    let blossom = height(4_000_000);
    let gaps = [(blossom, 450), (blossom, 451), (nu7, 450), (nu7, 451)];
    assert_eq!(gaps.map(|(at, g)| gap(&testnet, at, g)), [false, true, false, true]);

    // linkage in the chain (trusted: no rule run): orphan, off the finalized chain
    let five = builder.mine_empty(5);
    let orphan = builder.mine_empty(2);
    let below = builder.fork(height(1)).mine_empty(1).tip();
    let depth = FinalityDepth::new(NonZeroU32::new(2).expect("nz"));
    let mut chain = HeaderChain::anchored(&builder, depth);
    let one = |mock: &MockChain, at: BlockRef| [Arc::clone(mock.block(at.hash))];
    insert(&mut chain, &builder.blocks(five)).expect("trunk");
    chain.finalize(chain.finalizable().expect("6 headers, depth 2"));
    assert_eq!(insert(&mut chain, &one(&builder, orphan)), Err(Rejected::Orphan), "6 skipped");
    assert_eq!(insert(&mut chain, &one(&builder, below)), Err(Rejected::BelowFinalized));
    chain.check();
}

/// Decode: one exact header (short one, trailing byte, wrong solution length → all refused)
#[test]
fn decode_takes_exactly_one_header() {
    let raw = &GENESIS_RANGE[..HEADER_LEN];
    let header = decode_header(raw).expect("genesis");
    assert_eq!(header.hash(), Params::mainnet().genesis, "hash recomputed from the bytes");
    assert_eq!(header.prev_hash(), BlockHash([0; 32]));
    let truncated = |len| Err(DecodeError::Header(HeaderError::Truncated { len }));
    assert_eq!(decode_header(&raw[..140]), truncated(140));
    assert_eq!(decode_header(&raw[..HEADER_LEN - 1]), truncated(HEADER_LEN - 1));
    let mut long = raw.to_vec();
    long.push(0);
    assert_eq!(decode_header(&long), Err(DecodeError::Trailing { trailing: 1 }));
    let mut odd = raw[..141].to_vec();
    odd[140] = 7;
    odd.extend([0u8; 7]);
    let odd_length = HeaderError::SolutionLength { len: 7 };
    assert_eq!(decode_header(&odd), Err(DecodeError::Header(odd_length)));
    let mut wide = raw[..141].to_vec();
    wide[140] = 0xfe;
    let wide_prefix = HeaderError::SolutionPrefix { prefix: 0xfe };
    assert_eq!(decode_header(&wide), Err(DecodeError::Header(wide_prefix)));
}
