//! Searches for a pattern whose whole bytecode is one literal run.
//!
//! `regexp-quote` output, `orderless-literal` components and most
//! `completion-regexp-list` entries compile to `Exactn ... Succeed` and
//! nothing else. GNU `re_search_2` still enters the matcher at every
//! fastmap hit for them; here the same candidates are decided by one
//! substring scan (case-sensitive, one character representation) or by the
//! matcher's own `Exactn` character test run without the matcher's setup
//! (folded, or mixed representations). Either way the first match is the
//! one `re_search` would report, with the same group-0 registers.

use super::{
    CompiledPattern, MatchRegisters, PREFILTER_MIN_BUILD_SPAN, RegexOp, match_exactn_char_at, re_tr,
};

/// The literal run of a pattern that is nothing else, as the bytecode
/// stores it (case-folded patterns keep their translated characters).
///
/// Immutable after compilation and free of Lisp values: the descriptor
/// adds no cache and no shared state to the pattern that owns it.
#[derive(Clone)]
pub(super) struct WholeLiteral {
    bytes: Box<[u8]>,
    /// Every literal byte is ASCII, so the bytes are the characters in
    /// every pattern/text representation mix.
    ascii: bool,
    finder: memchr::memmem::Finder<'static>,
}

impl WholeLiteral {
    /// The literal when PATTERN's sealed bytecode is `Exactn` runs (and
    /// no-ops) ending in its final `Succeed`; `None` for anything else,
    /// including the empty pattern and any pattern with groups.
    pub(super) fn derive(pattern: &CompiledPattern) -> Option<Self> {
        if !pattern.buffer_sealed || pattern.re_nsub != 0 {
            return None;
        }
        let mut literal = Vec::new();
        let mut pc = 0;
        loop {
            let op = *pattern.buffer.get(pc)?;
            pc += 1;
            if op == RegexOp::Exactn as u8 {
                let count = *pattern.buffer.get(pc)? as usize;
                pc += 1;
                literal.extend_from_slice(pattern.buffer.get(pc..pc + count)?);
                pc += count;
            } else if op == RegexOp::NoOp as u8 {
                continue;
            } else if op == RegexOp::Succeed as u8 || op == RegexOp::PosixEnd as u8 {
                if pc != pattern.buffer.len() || literal.is_empty() {
                    return None;
                }
                break;
            } else {
                return None;
            }
        }
        let finder = memchr::memmem::Finder::new(&literal).into_owned();
        Some(Self {
            ascii: literal.is_ascii(),
            bytes: literal.into_boxed_slice(),
            finder,
        })
    }

    /// `re_search (PATTERN, TEXT, START, RANGE)` for this literal. The outer
    /// `None` hands the search back to the general matcher: backward
    /// searches, a pending quit (the matcher polls it), and folded or
    /// mixed-representation searches over long spans, where the pattern's
    /// prefilter and folded scans are the better scanners.
    pub(super) fn search(
        &self,
        pattern: &CompiledPattern,
        text: &[u8],
        start: usize,
        range: isize,
    ) -> Option<Option<(usize, MatchRegisters)>> {
        if range < 0 || start > text.len() || neovm_value::quit::quit_pending() {
            return None;
        }
        // Every candidate of a forward search starts and ends at or before
        // this bound (see `re_search`'s forward scan).
        let stop = start.saturating_add(range as usize).min(text.len());
        let bytes_are_characters = pattern.translate.is_none()
            && (self.ascii || pattern.multibyte == pattern.target_multibyte);
        let found = if bytes_are_characters {
            // Byte equality is character equality here (the matcher's own
            // `Exactn` run test), and no occurrence starts inside a
            // multibyte character: the literal's first byte is ASCII or a
            // leading code, never a continuation byte.
            self.finder
                .find(&text[start..stop])
                .map(|at| (start + at, start + at + self.bytes.len()))
        } else if stop - start < PREFILTER_MIN_BUILD_SPAN {
            self.find_by_characters(pattern, text, start, stop)
        } else {
            return None;
        };
        Some(found.map(|(match_start, match_end)| {
            let mut registers = MatchRegisters::new(1);
            registers.start[0] = match_start as i64;
            registers.end[0] = match_end as i64;
            (match_start, registers)
        }))
    }

    /// Try every candidate start in order with the matcher's `Exactn`
    /// character test, which carries the case translation and the
    /// unibyte/multibyte conversions.
    fn find_by_characters(
        &self,
        pattern: &CompiledPattern,
        text: &[u8],
        start: usize,
        stop: usize,
    ) -> Option<(usize, usize)> {
        let window = &text[start..stop];
        if self.ascii && window.is_ascii() {
            // An ASCII character is one byte in every representation, and the
            // matcher's `Exactn` test of an ASCII literal byte against an ASCII
            // text byte is `translate (byte) == literal byte` (a translation
            // leaving ASCII never equals one). Scan for the first match and
            // stop there, as the matcher's candidate loop does.
            let folds_to = |byte: u8, literal: u8| {
                re_tr(&pattern.translate, u32::from(byte)) == u32::from(literal)
            };
            return window
                .windows(self.bytes.len())
                .position(|candidate| {
                    candidate
                        .iter()
                        .zip(self.bytes.iter())
                        .all(|(&byte, &literal)| folds_to(byte, literal))
                })
                .map(|at| (start + at, start + at + self.bytes.len()));
        }
        (start..stop)
            // A match cannot start inside a multibyte character.
            .filter(|&candidate| !pattern.target_multibyte || text[candidate] & 0xC0 != 0x80)
            .find_map(|candidate| {
                let mut literal_at = 0;
                let mut text_at = candidate;
                while literal_at < self.bytes.len() {
                    let (literal_advance, text_advance) = match_exactn_char_at(
                        &self.bytes,
                        literal_at,
                        pattern.multibyte,
                        pattern.target_multibyte,
                        &pattern.translate,
                        text,
                        text_at,
                        stop,
                    )?;
                    literal_at += literal_advance;
                    text_at += text_advance;
                    // The character read may end past a bound that falls
                    // inside it; the match must not.
                    if text_at > stop {
                        return None;
                    }
                }
                Some((candidate, text_at))
            })
    }
}
