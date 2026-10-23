//! Against the real chain: headers captured by `examples/capture_headers.rs`

use std::num::NonZeroU32;
use std::sync::Arc;

use zaino_primitives::protocol::{median_time_past, MEDIAN_TIME_SPAN};
use zaino_primitives::types::{BlockRef, FinalityDepth, HeaderError};
use zaino_source::testing::{h, MockChain};
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::local_consensus::LocalNetwork;
use zcash_protocol::BlockHash;

use crate::params::AVERAGING_WINDOW;
use crate::rules::{alone, equihash_valid, expected_bits, header_work, in_context};
use crate::rules::{validate, Ancestor, CONTEXT};
use crate::testing::insert;
use crate::{decode_header, BestTip, Header, HeaderChain, Inserted, Params, Witness};
use crate::{DecodeError, Fault, Rejected};

const HEADER_LEN: usize = 1487;
/// Byte offsets in a consensus header (`HeaderBytes`)
const VERSION_AT: usize = 0;
const TIME_AT: usize = 100;
const BITS_AT: usize = 104;
const SOLUTION_SIZE_AT: usize = 140;

const GENESIS_RANGE: &[u8] = include_bytes!("../tests/fixtures/mainnet_0.headers");
const BLOSSOM_RANGE: &[u8] = include_bytes!("../tests/fixtures/mainnet_653500.headers");
const RECENT_START: u32 = 3_508_500;
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

/// `RECENT_RANGE[index]` as a block on mainnet
fn recent(headers: &[Header], index: usize) -> BlockRef {
    BlockRef { hash: headers[index].hash(), height: height(RECENT_START + index as u32) }
}

/// Ancestors of `headers[at]`, newest first (all the range holds, up to `CONTEXT`)
fn context(headers: &[Header], at: usize) -> Vec<Ancestor> {
    let from = at.saturating_sub(CONTEXT);
    headers[from..at].iter().rev().map(|h| Ancestor { bits: h.bits(), time: h.time() }).collect()
}

/// `header` with the little-endian u32 at byte `at` set to `value` (hash recomputed)
fn with_u32(header: &Header, at: usize, value: u32) -> Header {
    let mut raw = header.bytes().to_vec();
    raw[at..at + 4].copy_from_slice(&value.to_le_bytes());
    decode_header(&raw).expect("same layout")
}

/// Every captured header passes `validate`, its nBits reproduced exactly
///
/// - mid-chain ranges from their 29th header (17-block window + 11 medians held)
/// - testnet: minimum difficulty from 299,188 (gap > 6 × 150 s), at Blossom 584,000 (> 6 × 75 s)
#[test]
fn every_captured_header_passes_and_its_nbits_is_reproduced() {
    struct Range {
        name: &'static str,
        params: Params,
        start: u32,
        bytes: &'static [u8],
        first_checked: usize,
        min_difficulty_blocks: usize,
    }
    let window = AVERAGING_WINDOW + MEDIAN_TIME_SPAN;
    let ranges = [
        Range {
            name: "mainnet from genesis (window runs off genesis, height 1 included)",
            params: Params::mainnet(),
            start: 0,
            bytes: GENESIS_RANGE,
            first_checked: 1,
            min_difficulty_blocks: 0,
        },
        Range {
            name: "mainnet Blossom spacing switch at 653,600",
            params: Params::mainnet(),
            start: 653_500,
            bytes: BLOSSOM_RANGE,
            first_checked: window,
            min_difficulty_blocks: 0,
        },
        Range {
            name: "mainnet tip in 2026",
            params: Params::mainnet(),
            start: RECENT_START,
            bytes: RECENT_RANGE,
            first_checked: window,
            min_difficulty_blocks: 0,
        },
        Range {
            name: "testnet minimum difficulty from 299,188",
            params: Params::testnet(),
            start: 299_000,
            bytes: TESTNET_MIN_DIFFICULTY_RANGE,
            first_checked: window,
            min_difficulty_blocks: 3,
        },
        Range {
            name: "testnet Blossom at 584,000",
            params: Params::testnet(),
            start: 583_800,
            bytes: TESTNET_BLOSSOM_RANGE,
            first_checked: window,
            min_difficulty_blocks: 3,
        },
    ];
    for Range { name, params, start, bytes, first_checked, min_difficulty_blocks } in ranges {
        let headers = headers(bytes);
        let now = i64::from(headers.last().expect("non-empty").time());
        let mut at_limit = 0;
        for at in first_checked..headers.len() {
            let (header, at_height) = (&headers[at], height(start + at as u32));
            let parent = headers[at - 1].hash();
            assert_eq!(header.prev_hash(), parent, "{name}, {at_height:?}: links to its parent");
            let context = context(&headers, at);
            let expected =
                expected_bits(&params, at_height, header.time(), context[0].time, &context);
            assert_eq!(expected, Some(header.bits()), "{name}, {at_height:?}: nBits reproduced");
            let valid = validate(&params, header, at_height, &context, now);
            assert_eq!(valid, Ok(()), "{name}, {at_height:?}: every rule holds");
            at_limit += usize::from(header.bits() == params.limit_bits() && start > 0);
        }
        assert_eq!(at_limit, min_difficulty_blocks, "{name}: minimum-difficulty blocks");
    }
}

/// Anchored mid-mainnet (no history below), 200 real headers as a trusted validator's (no rule
/// run): each the new best, vouched as it lands, finalized 10 behind the best
///
/// - only the newest `max(2 · depth, CONTEXT)` = `CONTEXT` finalized headers answer by height
/// - older ones = on the best chain by definition (finalized)
#[test]
fn a_chain_anchored_mid_mainnet_follows_trusted_headers_keeping_the_newest_context_finalized() {
    const FOLLOWED: usize = 200;
    const DEPTH: u32 = 10;
    let headers = headers(RECENT_RANGE);
    let mut chain = HeaderChain::new(FinalityDepth::new(NonZeroU32::new(DEPTH).expect("nz")));
    chain.anchor(&headers[0], recent(&headers, 0).height);

    for index in 1..=FOLLOWED {
        let at = recent(&headers, index);
        assert_eq!(chain.insert(&headers[index]), Ok(Inserted::Best { reorg: false }), "{at:?}");
        chain.witness(at.hash, Witness::Trusted);
        if let Some(boundary) = chain.finalizable() {
            chain.finalize(boundary);
        }
        chain.check();
    }

    let finalized = FOLLOWED - DEPTH as usize;
    assert_eq!(chain.finalized_tip(), Some(recent(&headers, finalized)), "best − depth 10");
    assert_eq!(chain.tree_len(), DEPTH as usize, "only the non-finalized headers in the tree");
    let work = headers[1..=FOLLOWED].iter().map(|h| header_work(h.bits()).expect("valid")).sum();
    let best = BestTip { block: recent(&headers, FOLLOWED), cumulative_work: work };
    assert_eq!(chain.best(), Some(best), "the 200th, its work counted from the anchor");
    let verified = chain.snapshot().expect("anchored");
    let locator: Vec<BlockHash> =
        (finalized..=FOLLOWED).rev().map(|index| headers[index].hash()).collect();
    assert_eq!(verified.locator(), locator, "tip, then consecutive down to the finalized tip");
    let above = recent(&headers, FOLLOWED + 1).height;
    assert_eq!(verified.hash_at(above), None, "nothing above the best");

    let oldest_kept = finalized + 1 - CONTEXT;
    assert_eq!(chain.finalized_headers_kept(), CONTEXT, "2 · depth 10 < CONTEXT: CONTEXT wins");
    for index in [oldest_kept, finalized, finalized + 1, FOLLOWED] {
        let at = recent(&headers, index);
        let held = verified.header_at(at.height).map(|record| record.hash);
        assert_eq!(held, Some(at.hash), "{at:?}: answers by height");
    }
    let unkept = recent(&headers, oldest_kept - 1);
    assert_eq!(verified.header_at(unkept.height), None, "{unkept:?}: below the kept");
    for index in [1, oldest_kept - 1] {
        let at = recent(&headers, index);
        assert!(verified.on_best(at), "{at:?}: finalized, unkept = on the best by definition");
    }
    let stranger = BlockRef { hash: BlockHash([7; 32]), height: recent(&headers, 195).height };
    assert!(!verified.on_best(stranger), "{stranger:?}: not ours, above the finalized tip");
    assert!(!verified.holds(stranger), "{stranger:?}: not ours, above the finalized tip");

    assert_eq!(chain.insert(&headers[oldest_kept]), Ok(Inserted::Known), "oldest kept finalized");
    let below_kept = chain.insert(&headers[oldest_kept - 1]);
    assert_eq!(below_kept, Err(Rejected::Orphan), "below the kept: parent not held");
}

/// Nothing before an anchor; re-anchoring drops everything held before it
#[test]
fn re_anchoring_drops_everything_held_before_the_new_anchor() {
    let headers = headers(RECENT_RANGE);
    let mut chain = HeaderChain::new(FinalityDepth::new(NonZeroU32::new(10).expect("nz")));
    assert_eq!(chain.best(), None, "no anchor: no best");
    assert_eq!(chain.snapshot().map(|s| s.best()), None, "no anchor: nothing published");
    chain.anchor(&headers[0], recent(&headers, 0).height);
    insert_all(&mut chain, &headers[1..=20]);

    chain.anchor(&headers[50], recent(&headers, 50).height);

    assert_eq!(chain.finalized_tip(), Some(recent(&headers, 50)), "the new anchor = finalized tip");
    assert_eq!(chain.tree_len(), 0, "the 20 above the old anchor dropped");
    assert_eq!(chain.insert(&headers[21]), Err(Rejected::Orphan), "old chain's next: parent gone");
    let child = chain.insert(&headers[51]);
    assert_eq!(child, Ok(Inserted::Best { reorg: false }), "the new anchor's child");
    chain.check();
}

fn insert_all(chain: &mut HeaderChain, headers: &[Header]) {
    for header in headers {
        chain.insert(header).unwrap_or_else(|refused| panic!("{header:?}: {refused}"));
    }
}

/// Too few ancestors held to judge an untrusted header = the caller's fault, not the header's
#[test]
fn an_untrusted_header_right_after_the_anchor_is_unplaced_for_short_context() {
    let headers = headers(RECENT_RANGE);
    let mut chain = HeaderChain::new(FinalityDepth::new(NonZeroU32::new(10).expect("nz")));
    chain.anchor(&headers[0], recent(&headers, 0).height);
    let now = i64::from(headers[1].time());

    let short = chain.insert_untrusted(&headers[1], &Params::mainnet(), now);

    let needed = AVERAGING_WINDOW + MEDIAN_TIME_SPAN;
    assert_eq!(short, Err(Rejected::ShortContext { held: 1, needed }), "anchor alone held");
    let fault = short.map_err(|refused| refused.fault());
    assert_eq!(fault, Err(Fault::Unplaced), "caller holds too few, the header not blamed");
    assert_eq!(chain.tree_len(), 0, "nothing inserted");
}

/// Trusted 1..=27, then 28..=200 untrusted (every rule, ancestors read across the finalized
/// headers at depth 10), then header 201: refused edits insert nothing, its real self extends
#[test]
fn untrusted_headers_extend_the_chain_only_when_every_rule_holds() {
    let mainnet = Params::mainnet();
    let headers = headers(RECENT_RANGE);
    let now = i64::from(headers.last().expect("non-empty").time());
    let mut chain = HeaderChain::new(FinalityDepth::new(NonZeroU32::new(10).expect("nz")));
    chain.anchor(&headers[0], recent(&headers, 0).height);
    insert_all(&mut chain, &headers[1..28]);
    for index in 28..=200 {
        let inserted = chain.insert_untrusted(&headers[index], &mainnet, now);
        let at = recent(&headers, index);
        assert_eq!(inserted, Ok(Inserted::Best { reorg: false }), "{at:?}");
        chain.witness(at.hash, Witness::Trusted);
        if let Some(boundary) = chain.finalizable() {
            chain.finalize(boundary);
        }
        chain.check();
    }
    assert_eq!(chain.finalized_tip(), Some(recent(&headers, 190)), "200 − depth 10");

    // any edit moves the hash: proof of work refuses it before the context rules run
    let real = &headers[201];
    let bits = real.bits() + 1;
    let mut flipped = real.bytes().to_vec();
    *flipped.last_mut().expect("solution") ^= 1;
    let flipped = decode_header(&flipped).expect("same layout");
    let skewed = i64::from(real.time()) - 2 * 3600 - 1;
    let wrong_bits = chain.insert_untrusted(&with_u32(real, BITS_AT, bits), &mainnet, now);
    assert_eq!(wrong_bits, Err(Rejected::AboveTarget { bits }), "nBits + 1");
    let broken = chain.insert_untrusted(&flipped, &mainnet, now);
    assert_eq!(broken, Err(Rejected::AboveTarget { bits: real.bits() }), "solution byte flipped");
    let future = chain.insert_untrusted(real, &mainnet, skewed);
    let max = skewed + 2 * 3600;
    assert_eq!(future, Err(Rejected::FromTheFuture { time: real.time(), max }), "clock 2 h + 1 s");
    let best = chain.best().map(|best| best.block);
    assert_eq!(best, Some(recent(&headers, 200)), "nothing refused inserted");

    let accepted = chain.insert_untrusted(real, &mainnet, now);
    assert_eq!(accepted, Ok(Inserted::Best { reorg: false }), "real 201");
    let skipped = chain.insert_untrusted(&headers[203], &mainnet, now);
    assert_eq!(skipped, Err(Rejected::Orphan), "203 on an unheld 202");
    chain.check();
}

/// Rule a one-field-edit case runs
#[derive(Debug, Clone, Copy)]
enum Rule {
    Alone,
    InContext,
    Equihash,
}

/// Real mainnet header 3,508,600 with one field edited (or the local clock moved): refused by
/// the rule that field breaks, both sides of each bound
///
/// - `Fault::Deferred` = valid once the clock passes it, every other refusal blames the sender
#[test]
fn a_real_mainnet_header_with_one_field_edited_is_refused_by_that_fields_rule() {
    let mainnet = Params::mainnet();
    let headers = headers(RECENT_RANGE);
    let (real, at) = (&headers[100], height(RECENT_START + 100));
    let context = context(&headers, 100);
    let mtp = median_time_past(context.iter().map(|a| a.time)).expect("held");
    let now = i64::from(real.time());
    let bits = real.bits();
    let mut flipped = real.bytes().to_vec();
    *flipped.last_mut().expect("solution") ^= 1;
    let flipped = decode_header(&flipped).expect("same layout");
    let negative = i32::from_le_bytes(0x8000_0004u32.to_le_bytes());
    let latest = mtp + 90 * 60;
    let (sender, deferred) = (Fault::Sender, Fault::Deferred);
    use Rejected::*;
    use Rule::*;

    #[rustfmt::skip]
    let cases = [
        // (case,                                rule,      header,                               clock,              expected)
        ("unedited",                             Alone,     real.clone(),                         now,                Ok(())),
        ("unedited",                             InContext, real.clone(),                         now,                Ok(())),
        ("unedited",                             Equihash,  real.clone(),                         now,                Ok(())),
        ("solution's last byte flipped",         Equihash,  flipped.clone(),                      now,                Err((Solution, sender))),
        ("solution's last byte flipped: hash moved", Alone, flipped,                              now,                Err((AboveTarget { bits }, sender))),
        ("version 3",                            Alone,     with_u32(real, VERSION_AT, 3),        now,                Err((Version { version: 3 }, sender))),
        ("version high bit set: int32 negative", Alone,     with_u32(real, VERSION_AT, 0x8000_0004), now,             Err((Version { version: negative }, sender))),
        ("version 5: passed, hash moved",        Alone,     with_u32(real, VERSION_AT, 5),        now,                Err((AboveTarget { bits }, sender))),
        ("version 0x20000000 (BIP 9-style): passed, hash moved", Alone, with_u32(real, VERSION_AT, 0x2000_0000), now, Err((AboveTarget { bits }, sender))),
        ("nBits not a compact target",           Alone,     with_u32(real, BITS_AT, 0x0180_0000), now,                Err((Bits { bits: 0x0180_0000 }, sender))),
        ("nBits + 1",                            InContext, with_u32(real, BITS_AT, bits + 1),    now,                Err((Difficulty { bits: bits + 1, expected: bits }, sender))),
        ("nBits − 1",                            InContext, with_u32(real, BITS_AT, bits - 1),    now,                Err((Difficulty { bits: bits - 1, expected: bits }, sender))),
        ("time = median time past",              InContext, with_u32(real, TIME_AT, mtp),         now,                Err((TimeTooEarly { time: mtp, median_time_past: mtp }, sender))),
        ("time = median time past + 1 s",        InContext, with_u32(real, TIME_AT, mtp + 1),     now,                Ok(())),
        ("time = median time past + 90 min",     InContext, with_u32(real, TIME_AT, latest),      now,                Ok(())),
        ("time = median time past + 90 min + 1 s", InContext, with_u32(real, TIME_AT, latest + 1), now,               Err((TimeTooLate { time: latest + 1, max: i64::from(latest) }, sender))),
        ("local clock 2 h behind",               InContext, real.clone(),                         now - 2 * 3600,     Ok(())),
        ("local clock 2 h + 1 s behind",         InContext, real.clone(),                         now - 2 * 3600 - 1, Err((FromTheFuture { time: real.time(), max: now - 1 }, deferred))),
    ];
    for (case, rule, header, clock, expected) in cases {
        let outcome = match rule {
            Alone => alone(&mainnet, &header),
            InContext => in_context(&mainnet, &header, at, &context, clock),
            Equihash => equihash_valid(&header),
        };
        let outcome = outcome.map_err(|refused| (refused, refused.fault()));
        assert_eq!(outcome, expected, "{case} ({rule:?}): {header:?}");
    }
}

/// From NU7 testnet averages nBits over 102 blocks at 25 s: the 17-block rule's answer refused
#[test]
fn testnet_from_nu7_averages_nbits_over_102_blocks() {
    let testnet = Params::testnet();
    let pre_nu7 = Params { schedule: LocalNetwork { nu7: None, ..testnet.schedule }, ..testnet };
    let at = testnet.schedule.nu7.expect("testnet schedules NU7") + 50;
    // CONTEXT ancestors 25 s apart: the newest 40 at 0x1c7fffff, older ones easier at 0x1d00ffff
    let ancestors: Vec<Ancestor> = (0..CONTEXT as u32)
        .map(|back| Ancestor {
            bits: if back < 40 { 0x1c7f_ffff } else { 0x1d00_ffff },
            time: 1_800_000_000 - 25 * back,
        })
        .collect();
    let next = ancestors[0].time + 25;
    let ruled = |params: &Params| {
        expected_bits(params, at, next, ancestors[0].time, &ancestors).expect("adjusted")
    };
    let (window_102, window_17) = (ruled(&testnet), ruled(&pre_nu7));
    assert_ne!(window_102, window_17, "only 102 reaches the easier blocks: {window_102:#010x}");
    let template = &headers(TESTNET_BLOSSOM_RANGE)[200];
    let claiming = |bits| with_u32(&with_u32(template, TIME_AT, next), BITS_AT, bits);

    let stale = in_context(&testnet, &claiming(window_17), at, &ancestors, i64::from(next));
    let fresh = in_context(&testnet, &claiming(window_102), at, &ancestors, i64::from(next));

    let expected = window_102;
    assert_eq!(stale, Err(Rejected::Difficulty { bits: window_17, expected }), "17-block nBits");
    assert_eq!(fresh, Ok(()), "102-block nBits");
}

/// Testnet minimum difficulty: a gap over 450 s = the limit nBits, Blossom through NU7 (6 × 75 s,
/// then 18 × 25 s)
#[test]
fn testnet_minimum_difficulty_needs_a_gap_over_450_s_after_blossom_and_after_nu7() {
    let testnet = Params::testnet();
    let nu7 = testnet.schedule.nu7.expect("testnet schedules NU7");
    let after_blossom = height(4_000_000);
    let ancestors: Vec<Ancestor> = (0..CONTEXT as u32)
        .map(|back| Ancestor { bits: 0x1c7f_ffff, time: 1_800_000_000 - 25 * back })
        .collect();
    let parent = ancestors[0].time;
    #[rustfmt::skip]
    let cases = [
        // (case,          height,        gap s, at the limit)
        ("after Blossom", after_blossom, 450,   false),
        ("after Blossom", after_blossom, 451,   true),
        ("after NU7",     nu7 + 50,      450,   false),
        ("after NU7",     nu7 + 50,      451,   true),
    ];
    for (case, at, gap, limit) in cases {
        let bits = expected_bits(&testnet, at, parent + gap, parent, &ancestors);
        let at_limit = bits == Some(testnet.limit_bits());
        assert_eq!(at_limit, limit, "{case}, gap {gap} s: nBits {bits:#010x?}");
    }
}

/// Trusted headers skip the rules but not linkage: a skipped parent = orphan, a parent below the
/// finalized tip = off the finalized chain
#[test]
fn a_trusted_header_must_link_to_a_held_parent_at_or_above_the_finalized_tip() {
    let mut builder = MockChain::regtest();
    let five = builder.mine_empty(5);
    let seven = builder.mine_empty(2);
    let on_one = builder.fork(h(1)).mine_empty(1).tip();
    let depth = FinalityDepth::new(NonZeroU32::new(2).expect("nz"));
    let mut chain = HeaderChain::anchored(&builder, depth);
    insert(&mut chain, &builder.blocks(five)).expect("genesis ..= 5 link");
    chain.finalize(chain.finalizable().expect("best 5, depth 2: through 3"));
    let one = |at: BlockRef| [Arc::clone(builder.block(at.hash))];

    let orphan = insert(&mut chain, &one(seven));
    let below = insert(&mut chain, &one(on_one));

    assert_eq!(orphan, Err(Rejected::Orphan), "7 on an unheld 6");
    assert_eq!(below, Err(Rejected::BelowFinalized), "2 on 1, finalized through 3");
    chain.check();
}

/// Decode takes exactly one well-formed header: every short, long or malformed one refused
#[test]
fn decode_takes_exactly_one_header() {
    let genesis = &GENESIS_RANGE[..HEADER_LEN];
    let header = decode_header(genesis).expect("genesis");
    assert_eq!(header.hash(), Params::mainnet().genesis, "hash recomputed from the bytes");
    assert_eq!(header.prev_hash(), BlockHash([0; 32]), "genesis: no parent");
    let builder = MockChain::regtest();
    let regtest = builder.header_bytes(builder.genesis().hash);
    assert_eq!(regtest[SOLUTION_SIZE_AT], 36, "regtest solution size, one byte");
    let before_size = |raw: &[u8]| raw[..SOLUTION_SIZE_AT].to_vec();

    use HeaderError::*;
    #[rustfmt::skip]
    let cases: [(&str, Vec<u8>, DecodeError); 6] = [
        ("no solution size",          before_size(genesis),                        Truncated { len: 140 }.into()),
        ("one byte short",            genesis[..HEADER_LEN - 1].to_vec(),          Truncated { len: HEADER_LEN - 1 }.into()),
        ("one trailing byte",         [genesis, &[0]].concat(),                    DecodeError::Trailing { trailing: 1 }),
        ("7-byte solution",           [&before_size(genesis), &[7][..], &[0; 7]].concat(), SolutionLength { len: 7 }.into()),
        ("0xfe size prefix",          [&before_size(genesis), &[0xfe][..]].concat(), SolutionPrefix { prefix: 0xfe }.into()),
        ("size 36 as 0xfd 0x24 0x00", [&before_size(&regtest), &[0xfd, 36, 0][..], &regtest[SOLUTION_SIZE_AT + 1..]].concat(), NonMinimalLength { len: 36 }.into()),
    ];
    for (case, raw, expected) in cases {
        assert_eq!(decode_header(&raw), Err(expected), "{case}: {} bytes", raw.len());
    }
}
