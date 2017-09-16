// Copyright 2017 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Spans are encoded using 2-bit tag and 4 different encoding formats for each tag.
// Three formats are used for keeping span data inline,
// the fourth one contains index into out-of-line span interner.
// The encoding formats for inline spans were obtained by optimizing over crates in rustc/libstd.
// See https://internals.rust-lang.org/t/rfc-compiler-refactoring-spans/1357/28

use super::*;

/// A compressed span.
/// Contains either fields of `SpanData` inline if they are small, or index into span interner.
/// The primary goal of `Span` is to be as small as possible and fit into other structures
/// (that's why it uses `packed` as well). Decoding speed is the second priority.
/// See `SpanData` for the info on span fields in decoded representation.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(packed)]
pub struct Span(u32);

/// Dummy span, both position and length are zero, syntax context is zero as well.
/// This span is kept inline and encoded with format 0.
pub const DUMMY_SP: Span = Span(0);

impl Span {
    #[inline]
    pub fn new(lo: BytePos, hi: BytePos, ctxt: SyntaxContext) -> Self {
        encode(&match lo <= hi {
            true => SpanData { lo, hi, ctxt },
            false => SpanData { lo: hi, hi: lo, ctxt },
        })
    }

    #[inline]
    pub fn data(self) -> SpanData {
        decode(self)
    }
}

// Tags
const TAG_INLINE0: u32 = 0b00;
const TAG_INLINE1: u32 = 0b01;
const TAG_INLINE2: u32 = 0b10;
const TAG_INTERNED: u32 = 0b11;
const TAG_MASK: u32 = 0b11;

// Fields indexes
const BASE_INDEX: usize = 0;
const LEN_INDEX: usize = 1;
const CTXT_INDEX: usize = 2;

// Tag = 0b00, inline format 0.
// -----------------------------------
// | base 31:8  | len 7:2  | tag 1:0 |
// -----------------------------------
const INLINE0_SIZES: [u32; 3] = [24, 6, 0];
const INLINE0_OFFSETS: [u32; 3] = [8, 2, 2];

// Tag = 0b01, inline format 1.
// -----------------------------------
// | base 31:10 | len 9:2 | tag 1:0 |
// -----------------------------------
const INLINE1_SIZES: [u32; 3] = [22, 8, 0];
const INLINE1_OFFSETS: [u32; 3] = [10, 2, 2];

// Tag = 0b10, inline format 2.
// ------------------------------------------------
// | base 31:14 | len 13:13 | ctxt 12:2 | tag 1:0 |
// ------------------------------------------------
const INLINE2_SIZES: [u32; 3] = [18, 1, 11];
const INLINE2_OFFSETS: [u32; 3] = [14, 13, 2];

// Tag = 0b11, interned format.
// ------------------------
// | index 31:3 | tag 1:0 |
// ------------------------
const INTERNED_INDEX_SIZE: u32 = 30;
const INTERNED_INDEX_OFFSET: u32 = 2;

fn encode(sd: &SpanData) -> Span {
    let (base, len, ctxt) = (sd.lo.0, sd.hi.0 - sd.lo.0, sd.ctxt.0);

    // Can we fit the span data into this encoding?
    let fits = |sizes: [u32; 3]| {
        (base >> sizes[BASE_INDEX]) == 0 && (len >> sizes[LEN_INDEX]) == 0 &&
        (ctxt >> sizes[CTXT_INDEX]) == 0
    };
    // Turn fields into a single `u32` value.
    let compose = |offsets: [u32; 3], tag| {
        (base << offsets[BASE_INDEX]) | (len << offsets[LEN_INDEX]) |
        (ctxt << offsets[CTXT_INDEX]) | tag
    };

    let val = if fits(INLINE0_SIZES) {
        compose(INLINE0_OFFSETS, TAG_INLINE0)
    } else if fits(INLINE1_SIZES) {
        compose(INLINE1_OFFSETS, TAG_INLINE1)
    } else if fits(INLINE2_SIZES) {
        compose(INLINE2_OFFSETS, TAG_INLINE2)
    } else {
        let index = with_span_interner(|interner| interner.intern(sd));
        if (index >> INTERNED_INDEX_SIZE) == 0 {
            (index << INTERNED_INDEX_OFFSET) | TAG_INTERNED
        } else {
            panic!("too many spans in a crate");
        }
    };
    Span(val)
}

fn decode(span: Span) -> SpanData {
    let val = span.0;

    // Extract a field at position `pos` having size `size`.
    let extract = |pos, size| {
        let mask = ((!0u32) as u64 >> (32 - size)) as u32; // Can't shift u32 by 32
        (val >> pos) & mask
    };

    let (base, len, ctxt) = match val & TAG_MASK {
        TAG_INLINE0 => (
            extract(INLINE0_OFFSETS[BASE_INDEX], INLINE0_SIZES[BASE_INDEX]),
            extract(INLINE0_OFFSETS[LEN_INDEX], INLINE0_SIZES[LEN_INDEX]),
            extract(INLINE0_OFFSETS[CTXT_INDEX], INLINE0_SIZES[CTXT_INDEX]),
        ),
        TAG_INLINE1 => (
            extract(INLINE1_OFFSETS[BASE_INDEX], INLINE1_SIZES[BASE_INDEX]),
            extract(INLINE1_OFFSETS[LEN_INDEX], INLINE1_SIZES[LEN_INDEX]),
            extract(INLINE1_OFFSETS[CTXT_INDEX], INLINE1_SIZES[CTXT_INDEX]),
        ),
        TAG_INLINE2 => (
            extract(INLINE2_OFFSETS[BASE_INDEX], INLINE2_SIZES[BASE_INDEX]),
            extract(INLINE2_OFFSETS[LEN_INDEX], INLINE2_SIZES[LEN_INDEX]),
            extract(INLINE2_OFFSETS[CTXT_INDEX], INLINE2_SIZES[CTXT_INDEX]),
        ),
        TAG_INTERNED => {
            let index = extract(INTERNED_INDEX_OFFSET, INTERNED_INDEX_SIZE);
            return with_span_interner(|interner| *interner.get(index));
        }
        _ => unreachable!()
    };
    SpanData { lo: BytePos(base), hi: BytePos(base + len), ctxt: SyntaxContext(ctxt) }
}

#[derive(Default)]
struct SpanInterner {
    spans: HashMap<SpanData, u32>,
    span_data: Vec<SpanData>,
}

impl SpanInterner {
    fn intern(&mut self, span_data: &SpanData) -> u32 {
        if let Some(index) = self.spans.get(span_data) {
            return *index;
        }

        let index = self.spans.len() as u32;
        self.span_data.push(*span_data);
        self.spans.insert(*span_data, index);
        index
    }

    fn get(&self, index: u32) -> &SpanData {
        &self.span_data[index as usize]
    }
}

// If an interner exists in TLS, return it. Otherwise, prepare a fresh one.
fn with_span_interner<T, F: FnOnce(&mut SpanInterner) -> T>(f: F) -> T {
    thread_local!(static INTERNER: RefCell<SpanInterner> = {
        RefCell::new(SpanInterner::default())
    });
    INTERNER.with(|interner| f(&mut *interner.borrow_mut()))
}
