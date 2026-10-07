//! Against the real chain: headers captured by `examples/capture_headers.rs`

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_persistence::fs::SimFs;
use zaino_primitives::testing::{encode_header, Chain};
use zaino_primitives::types::{BlockHash, BlockRef, Height, ReorgDepth};
use zcash_protocol::consensus::NetworkType;

use crate::rules::{equihash_valid, expected_bits, in_context, Ancestor, MEDIAN_SPAN};
use crate::{check, decode_header, link_run, Header, HeaderChain, HeaderStore, Inserted, Params};
use crate::{DecodeError, Rejected};
use zaino_primitives::types::HeaderError;

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

fn height(h: u32) -> Height {
    Height::try_from(h).expect("in range")
}

/// Ancestors of `headers[at]`, newest first (all the range holds, up to `CONTEXT`)
fn context(headers: &[Header], at: usize) -> Vec<Ancestor> {
    let from = at.saturating_sub(crate::rules::CONTEXT);
    headers[from..at].iter().rev().map(|h| Ancestor { bits: h.bits(), time: h.time() }).collect()
}

fn store(fs: Arc<SimFs>, network: NetworkType) -> HeaderStore {
    HeaderStore::open(fs, std::path::Path::new("/headers"), network).expect("store opens")
}

/// Every captured header passes stage A and stage B, its nBits reproduced exactly:
/// - mainnet: genesis's first 300 (the limit while the window runs off genesis, height 1
///   included), Blossom's spacing switch at 653,600, the tip in 2026
/// - testnet: minimum difficulty from 299,188 (gap > 6 × 150 s) and Blossom at 584,000 (> 6 × 75 s)
/// - mid-chain ranges from their 29th header (17-block window + 11 medians held); each header
///   links to the one before by hash
#[test]
fn every_captured_header_passes_and_its_nbits_is_reproduced() {
    let window = 17 + MEDIAN_SPAN;
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
            let checked = check(&params, header.clone()).expect("stage A");
            let own = in_context(&params, checked.header(), at_height, &context, now);
            assert!(own.is_ok(), "{at_height:?}: {own:?}");
            at_limit += usize::from(header.bits() == params.limit_bits() && start > 0);
        }
        assert_eq!(at_limit, min_difficulty, "{start}: minimum-difficulty blocks");
    }
}

/// From genesis through the chain itself: 201 headers, each the new best, cumulative work = the
/// sum of each header's; finalized behind a 10-block bound into a store, reopened, and the rest
/// verified from the reopened tail (no context lost across a restart); the published chain
/// answers every height, final or not, and its locator ends at the final tip
#[test]
fn mainnet_from_genesis_through_a_store_and_a_reopen() {
    let headers = headers(GENESIS_RANGE);
    let mainnet = Params::mainnet();
    let now = i64::from(headers.last().expect("non-empty").time());
    let depth = ReorgDepth::new(NonZeroU32::new(10).expect("nz"));
    let fs = SimFs::new();
    let mut chain = HeaderChain::open(mainnet, depth, store(Arc::clone(&fs), NetworkType::Main));
    let checked = |header: &Header| check(&mainnet, header.clone()).expect("mainnet header");

    for header in &headers[..200] {
        let inserted = chain.insert(&checked(header), now).expect("mainnet header");
        assert_eq!(inserted, Inserted::Best { reorg: false });
        if let Some(boundary) = chain.finalizable() {
            chain.finalize(boundary).expect("store commits");
        }
        chain.check();
    }
    assert_eq!(chain.insert(&checked(&headers[180]), now), Ok(Inserted::Known), "final tail");
    let deep = chain.insert(&checked(&headers[50]), now);
    assert_eq!(deep, Err(Rejected::Orphan), "below the tail: no hash index over every final");
    assert_eq!(chain.final_tip().map(|tip| tip.height), Some(height(189)));
    assert_eq!(chain.tree_len(), 10, "only the non-final window in memory");
    let best = chain.best().expect("best");
    let work: u128 = headers[..200]
        .iter()
        .map(|h| {
            crate::target::work(crate::target::expand(h.bits()).expect("valid")).expect("fits")
        })
        .sum();
    assert_eq!(best.cumulative_work, work);
    assert_eq!(best.block, BlockRef { hash: headers[199].hash(), height: height(199) });
    let verified = chain.verified().expect("verified");
    for at in [0, 27, 150, 189, 190, 199] {
        let header = verified.header_at(height(at)).expect("held");
        let real = &headers[at as usize];
        assert_eq!((header.hash, header.merkle_root), (real.hash(), real.merkle_root()), "{at}");
    }
    assert_eq!(verified.hash_at(height(200)), None, "above the best");
    let locator: Vec<BlockHash> =
        [199, 198, 197, 196, 195, 194, 193, 192, 191, 190, 189].map(|h| headers[h].hash()).into();
    assert_eq!(verified.locator(), locator, "tip, then consecutive down to the final tip");

    let reopened = store(fs.restarted(), NetworkType::Main);
    let mut chain = HeaderChain::open(mainnet, depth, reopened);
    assert_eq!(chain.best().map(|tip| tip.block.height), Some(height(189)), "tree is memory");
    let stored = chain.verified().and_then(|verified| verified.hash_at(height(42)));
    assert_eq!(stored, Some(headers[42].hash()), "stored below the tail");
    for header in &headers[190..] {
        chain.insert(&checked(header), now).expect("verified from the reopened tail");
    }
    assert_eq!(chain.best().map(|tip| tip.block.hash), Some(headers[300].hash()));
    chain.check();
}

/// Real headers, mutated one field at a time: each refused by the rule it breaks
/// - mainnet 3,508,600: Equihash, hash ≤ target, nBits, time, version (int32), nBits encoding
/// - a regtest header: version > 4 valid, a non-minimal solution length refused by decode
/// - a run: a header not linked to the one before is cut there
/// - testnet NU7: the 102-block window at 25 s decides nBits (the 17-block rule's is refused);
///   the minimum-difficulty gap stays 450 s across Blossom → NU7 (451 qualifies, 450 does not)
/// - linkage in the chain: a foreign genesis, an unknown parent, a parent off the final chain
#[test]
fn each_mutation_is_refused_by_its_own_rule() {
    let mainnet = Params::mainnet();
    let headers = headers(RECENT_RANGE);
    let at = 100;
    let (real, at_height) = (&headers[at], height(3_508_500 + at as u32));
    let context = context(&headers, at);
    let now = i64::from(real.time());
    let median_time_past = {
        let mut times: Vec<u32> = context.iter().take(11).map(|a| a.time).collect();
        times.sort_unstable();
        times[5]
    };
    let mutated = |edit: &dyn Fn(&mut Vec<u8>)| {
        let mut raw = real.bytes().to_vec();
        edit(&mut raw);
        decode_header(&raw).expect("same shape")
    };
    let set_u32 = |at: usize, value: u32| {
        move |raw: &mut Vec<u8>| raw[at..at + 4].copy_from_slice(&value.to_le_bytes())
    };
    let alone = |header: &Header| check(&mainnet, header.clone()).map(|_| ());
    let in_place = |header: &Header, now: i64| {
        in_context(&mainnet, header, at_height, &context, now).map(|_| ())
    };

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
    assert!(future.is_deferred() && !too_late.is_deferred(), "only the clock rule waits");
    assert_eq!(alone(&mutated(&set_u32(0, 3))), Err(Rejected::Version { version: 3 }));
    let high_bit = mutated(&set_u32(0, 0x8000_0004));
    let negative = Rejected::Version { version: i32::from_le_bytes(0x8000_0004u32.to_le_bytes()) };
    assert_eq!(alone(&high_bit), Err(negative), "nVersion is int32: high bit set = negative");
    let invalid_bits = mutated(&set_u32(104, 0x0180_0000));
    assert_eq!(alone(&invalid_bits), Err(Rejected::Bits { bits: 0x0180_0000 }));

    let regtest = Params::regtest(height(1), None);
    let builder = Chain::new();
    let raw = encode_header(builder.block(builder.genesis().hash).header());
    let versioned = |version: u32| {
        let mut raw = raw.clone();
        raw[..4].copy_from_slice(&version.to_le_bytes());
        check(&regtest, decode_header(&raw).expect("shape")).map(|_| ())
    };
    assert_eq!([4, 5, 0x2000_0000].map(versioned), [Ok(()), Ok(()), Ok(())], "> 4 stays valid");
    let mut padded = raw[..140].to_vec();
    padded.extend([0xfd, 36, 0]);
    padded.extend(&raw[141..]);
    let non_minimal = HeaderError::NonMinimalLength { len: 36 };
    assert_eq!(decode_header(&padded), Err(DecodeError::Header(non_minimal)));

    let run = |order: [usize; 3]| {
        let run = order.map(|at| check(&mainnet, headers[at].clone()));
        let (kept, cut) = link_run(run);
        (kept.iter().map(|c| c.header().hash()).collect::<Vec<_>>(), cut)
    };
    let linked = [10, 11, 12].map(|at| headers[at].hash()).to_vec();
    assert_eq!(run([10, 11, 12]), (linked, None));
    let gap = [10, 11].map(|at| headers[at].hash()).to_vec();
    assert_eq!(run([10, 11, 13]), (gap, Some(Rejected::Unlinked)), "cut at the broken link");

    let testnet = Params::testnet();
    let pre_nu7 = Params { nu7: None, ..testnet };
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
    let gap = |params: &Params, at: Height, gap: u32| {
        let time = ancestors[0].time + gap;
        expected_bits(params, at, time, ancestors[0].time, &ancestors) == Some(0x2007_ffff)
    };
    let blossom = height(4_000_000);
    let gaps = [(blossom, 450), (blossom, 451), (nu7, 450), (nu7, 451)];
    assert_eq!(gaps.map(|(at, g)| gap(&testnet, at, g)), [false, true, false, true]);

    // linkage through the chain (regtest, so each header passes stage A): a foreign genesis, an
    // unknown parent, a parent off the final chain
    let mut builder = Chain::new();
    let trunk = builder.extend(builder.genesis().hash, 5);
    let trunk: Vec<BlockHash> = builder.path(trunk.hash).iter().map(|b| b.header().hash).collect();
    let skipped = builder.mine(trunk[5]).hash;
    let orphan = builder.mine(skipped).hash;
    let below = builder.mine(trunk[1]).hash;
    let coinbase = builder.block(skipped).transactions().to_vec();
    let foreign = Chain::with_genesis(coinbase);
    let regtest = regtest.with_genesis(trunk[0]);
    let checked = |builder: &Chain, hash: BlockHash| {
        let raw = encode_header(builder.block(hash).header());
        check(&regtest, decode_header(&raw).expect("shape")).expect("stage A")
    };
    let depth = ReorgDepth::new(NonZeroU32::new(2).expect("nz"));
    let fs = SimFs::new();
    let mut chain = HeaderChain::open(regtest, depth, store(fs, NetworkType::Regtest));
    let foreign_genesis = checked(&foreign, foreign.genesis().hash);
    assert_eq!(chain.insert(&foreign_genesis, now), Err(Rejected::WrongGenesis));
    for hash in &trunk {
        chain.insert(&checked(&builder, *hash), now).expect("trunk");
    }
    let boundary = chain.finalizable().expect("6 headers, depth 2");
    chain.finalize(boundary).expect("finalizes 0..=3");
    assert_eq!(chain.insert(&checked(&builder, orphan), now), Err(Rejected::Orphan));
    assert_eq!(chain.insert(&checked(&builder, below), now), Err(Rejected::BelowFinal));
    chain.check();
}

/// Decode: one exact header; a short one, a trailing byte, a wrong solution length all refused
#[test]
fn decode_takes_exactly_one_header() {
    let raw = &GENESIS_RANGE[..HEADER_LEN];
    let header = decode_header(raw).expect("genesis");
    assert_eq!(header.hash(), Params::mainnet().genesis(), "hash recomputed from the bytes");
    assert_eq!(header.prev_hash(), BlockHash::ZERO);
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
