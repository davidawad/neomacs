//! JSON serialization and parsing builtins.
//!
//! Implements the Emacs JSON interface:
//! - `json-serialize` — convert Lisp value to JSON string
//! - `json-parse-string` — parse JSON string to Lisp value
//!
//! Key mapping (Emacs convention):
//! - Lisp nil → JSON object {}
//! - Lisp :null → JSON null
//! - Lisp t → JSON true
//! - Lisp :false → JSON false
//! - Lisp integer/float → JSON number
//! - Lisp string → JSON string
//! - Lisp hash-table → JSON object
//! - Lisp vector → JSON array
//! - Lisp alist/plist → JSON object (when :object-type specifies)
//!
//! No external crate (serde_json etc.) is used — the parser and serializer
//! are implemented from scratch with simple recursive descent.

use crate::emacs_core::text_host::TextHost;
use neovm_edit::buffer::{
    BufferId, BufferManager, EmacsByteLen, EmacsBytePos, EmacsByteRange, TextChange, TextExtent,
};
use neovm_edit::emacs_core::edit_host::EditHost;
use neovm_edit::emacs_core::editfns::{
    signal_after_text_change, signal_before_text_change,
    text_change_for_empty_insertion_at_emacs_byte_pos,
};
use neovm_value::error::LispCondition;
use neovm_value::error::expect_min_args;
use neovm_value::error::{EvalResult, Flow, signal};
use neovm_value::heap_types::LispString;
use neovm_value::intern::{SymId, intern, resolve_sym};
use neovm_value::list_scan::GnuTailCycle;
use neovm_value::tagged::value::SymbolMode;
use neovm_value::value::*;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use strum::{EnumString, IntoStaticStr};

// ---------------------------------------------------------------------------
// Argument helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Keyword argument parsing
// ---------------------------------------------------------------------------

/// Options that control how Lisp values are serialized to JSON.
#[derive(Clone, Debug)]
struct SerializeOpts {
    symbol_mode: SymbolMode,
    /// The Lisp value that maps to JSON null.
    null_object: Value,
    /// The Lisp value that maps to JSON false.
    false_object: Value,
}

impl Default for SerializeOpts {
    fn default() -> Self {
        Self {
            symbol_mode: SymbolMode::Bare,
            null_object: Value::keyword(":null"),
            false_object: Value::keyword(":false"),
        }
    }
}

/// Options that control how a JSON string is parsed into Lisp values.
#[derive(Clone, Debug)]
struct ParseOpts {
    /// How JSON objects are represented.
    object_type: ObjectType,
    /// How JSON arrays are represented.
    array_type: ArrayType,
    /// Lisp value for JSON null.
    null_object: Value,
    /// Lisp value for JSON false.
    false_object: Value,
}

impl Default for ParseOpts {
    fn default() -> Self {
        Self {
            object_type: ObjectType::HashTable,
            array_type: ArrayType::Vector,
            null_object: Value::keyword(":null"),
            false_object: Value::keyword(":false"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "kebab-case")]
enum ObjectType {
    HashTable,
    Alist,
    Plist,
}

impl ObjectType {
    fn from_symbol_value(value: &Value, mode: SymbolMode) -> Option<Self> {
        value.as_symbol_name(mode)?.parse().ok()
    }

    #[cfg(test)]
    fn symbol_name(self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, IntoStaticStr)]
enum ArrayType {
    #[strum(serialize = "array")]
    Vector,
    #[strum(serialize = "list")]
    List,
}

impl ArrayType {
    fn from_symbol_value(value: &Value, mode: SymbolMode) -> Option<Self> {
        value.as_symbol_name(mode)?.parse().ok()
    }

    #[cfg(test)]
    fn symbol_name(self) -> &'static str {
        self.into()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(prefix = ":", serialize_all = "kebab-case")]
enum JsonOptionKey {
    ObjectType,
    ArrayType,
    NullObject,
    FalseObject,
}

impl JsonOptionKey {
    fn from_value(value: &Value, mode: SymbolMode) -> Option<Self> {
        value.as_symbol_name(mode)?.strip_prefix(':')?.parse().ok()
    }

    #[cfg(test)]
    fn keyword(self) -> &'static str {
        self.into()
    }
}

/// Parse keyword arguments from the &rest tail (starting at `start_index`).
/// Returns `ParseOpts`.  Unknown keywords signal `json-error`.
///
/// The `:null` and `:false` defaults are made only for an option the call
/// leaves out: a JSON-RPC client passes both on every message, and building
/// a default keyword interns its name.
fn parse_parse_kwargs(
    args: &[Value],
    start_index: usize,
    mode: SymbolMode,
) -> Result<ParseOpts, Flow> {
    let mut object_type = ObjectType::HashTable;
    let mut array_type = ArrayType::Vector;
    let mut null_object = None;
    let mut false_object = None;
    let rest = &args[start_index..];
    if !rest.len().is_multiple_of(2) {
        return Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("plistp"), Value::list(rest.to_vec())],
        ));
    }

    for i in (0..rest.len()).step_by(2).rev() {
        let key = &rest[i];
        let value = &rest[i + 1];
        match JsonOptionKey::from_value(key, mode) {
            Some(JsonOptionKey::ObjectType) => {
                if let Some(requested) = ObjectType::from_symbol_value(value, mode) {
                    object_type = requested;
                } else {
                    return Err(signal(
                        "error",
                        vec![
                            Value::string("One of hash-table, alist or plist should be specified"),
                            *value,
                        ],
                    ));
                }
            }
            Some(JsonOptionKey::ArrayType) => {
                if let Some(requested) = ArrayType::from_symbol_value(value, mode) {
                    array_type = requested;
                } else {
                    return Err(signal(
                        "error",
                        vec![
                            Value::string("One of array or list should be specified"),
                            *value,
                        ],
                    ));
                }
            }
            Some(JsonOptionKey::NullObject) => {
                null_object = Some(*value);
            }
            Some(JsonOptionKey::FalseObject) => {
                false_object = Some(*value);
            }
            _ => {
                return Err(signal(
                    "error",
                    vec![
                        Value::string(
                            "One of :object-type, :array-type, :null-object or :false-object should be specified",
                        ),
                        *value,
                    ],
                ));
            }
        }
    }
    Ok(ParseOpts {
        object_type,
        array_type,
        null_object: null_object.unwrap_or_else(|| Value::keyword(":null")),
        false_object: false_object.unwrap_or_else(|| Value::keyword(":false")),
    })
}

/// Parse keyword arguments relevant to `json-serialize` / `json-insert`,
/// making a default sentinel only for an option the call leaves out (see
/// [`parse_parse_kwargs`]).
fn parse_serialize_kwargs(
    args: &[Value],
    start_index: usize,
    mode: SymbolMode,
) -> Result<SerializeOpts, Flow> {
    let mut null_object = None;
    let mut false_object = None;
    let rest = &args[start_index..];
    if !rest.len().is_multiple_of(2) {
        return Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("plistp"), Value::list(rest.to_vec())],
        ));
    }

    for i in (0..rest.len()).step_by(2).rev() {
        let key = &rest[i];
        let value = &rest[i + 1];
        match JsonOptionKey::from_value(key, mode) {
            Some(JsonOptionKey::NullObject) => {
                null_object = Some(*value);
            }
            Some(JsonOptionKey::FalseObject) => {
                false_object = Some(*value);
            }
            _ => {
                return Err(signal(
                    "error",
                    vec![
                        Value::string("One of :null-object or :false-object should be specified"),
                        *value,
                    ],
                ));
            }
        }
    }
    Ok(SerializeOpts {
        symbol_mode: mode,
        null_object: null_object.unwrap_or_else(|| Value::keyword(":null")),
        false_object: false_object.unwrap_or_else(|| Value::keyword(":false")),
    })
}

// ===========================================================================
// JSON Serializer (Lisp → JSON string)
// ===========================================================================

/// Check if two Values are equivalent for the purpose of matching the
/// null/false sentinel objects.
fn value_matches(a: &Value, b: &Value, mode: SymbolMode) -> bool {
    neovm_value::value::eq_value_swp(a, b, matches!(mode, SymbolMode::Positioned))
}

/// Serialize a Lisp value to a JSON string.
/// Serialize `value` as JSON.
///
/// Thin wrapper over [`serialize_into`], which appends into a single buffer.
/// The serializer used to return a freshly allocated `String` for EVERY node
/// and join them on the way up, so a document cost one allocation per value
/// plus a copy of every nested result into its parent. On an LSP payload --
/// eglot serializes a request per keystroke -- that put ~35% of the time in
/// the allocator and ~7% in `memmove`, and made `json-serialize` 3.3x to 8.3x
/// slower than GNU, widening with size. GNU appends into one growing buffer
/// (`json_out_t`, src/json.c); so does this now.
/// GNU `json_out_nest` (src/json.c): `maxdepth` starts at 50 and every
/// object or array entry decrements it, so entering the 51st container
/// signals `error "Maximum JSON serialization depth exceeded"`. `depth` here
/// is the container's zero-based nesting level (the root container is 0),
/// so the 51st container arrives with depth 50.
fn check_container_depth(depth: usize) -> Result<(), Flow> {
    if depth >= 50 {
        return Err(signal(
            "error",
            vec![Value::string("Maximum JSON serialization depth exceeded")],
        ));
    }
    Ok(())
}

fn serialize_to_json(
    value: &Value,
    opts: &SerializeOpts,
    depth: usize,
) -> Result<JsonOutput, Flow> {
    let mut out = JsonOutput::take();
    serialize_into(&mut out.text, value, opts, depth)?;
    Ok(out)
}

/// The buffer `json-serialize` and `json-insert` write into: this thread's,
/// reused from call to call.
///
/// A JSON-RPC client serializes a message per request, a few hundred bytes
/// each; growing a fresh `String` to that size reallocated and copied it
/// half a dozen times per message, which was most of `json-serialize`'s
/// cost after the one-buffer serializer. The buffer is taken for the call
/// and given back on drop (each thread owns its own; a nested call, from a
/// `json-insert` change hook, just takes an empty one). A buffer that grew
/// past [`Self::MAX_RETAINED_BYTES`] is freed rather than kept.
struct JsonOutput {
    text: String,
}

thread_local! {
    static JSON_OUTPUT: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
}

impl JsonOutput {
    /// Largest buffer kept for the next call.
    const MAX_RETAINED_BYTES: usize = 1 << 20;

    fn take() -> Self {
        let mut text = JSON_OUTPUT
            .try_with(|buffer| std::mem::take(&mut *buffer.borrow_mut()))
            .unwrap_or_default();
        text.clear();
        Self { text }
    }

    fn as_str(&self) -> &str {
        &self.text
    }
}

impl Drop for JsonOutput {
    fn drop(&mut self) {
        if self.text.capacity() <= Self::MAX_RETAINED_BYTES {
            let text = std::mem::take(&mut self.text);
            // During thread teardown the slot is gone; the buffer just goes.
            let _ = JSON_OUTPUT.try_with(|buffer| *buffer.borrow_mut() = text);
        }
    }
}

fn serialize_into(
    out: &mut String,
    value: &Value,
    opts: &SerializeOpts,
    depth: usize,
) -> Result<(), Flow> {
    // Check for null sentinel.
    if value_matches(value, &opts.null_object, opts.symbol_mode) {
        out.push_str("null");
        return Ok(());
    }

    // Check for false sentinel.
    if value_matches(value, &opts.false_object, opts.symbol_mode) {
        out.push_str("false");
        return Ok(());
    }

    match value.kind() {
        // t → true (checked after false sentinel, which is usually :false not t)
        ValueKind::T => {
            out.push_str("true");
            Ok(())
        }

        ValueKind::Fixnum(n) => {
            out.push_str(itoa::Buffer::new().format(n));
            Ok(())
        }

        // Bignums serialize as their full decimal expansion (GNU
        // json_out_bignum), so large integers round-trip without loss.
        ValueKind::Veclike(VecLikeType::Bignum) => {
            let n = value
                .as_bignum()
                .expect("ValueKind::Veclike(Bignum) must carry a bignum payload");
            out.push_str(&n.to_string());
            Ok(())
        }

        ValueKind::Float => {
            let f = value.xfloat();
            if f.is_nan() || f.is_infinite() {
                // GNU `json_out_float` calls `signal_error ("JSON does not
                // allow Inf or NaN", f)`, which signals a plain `error` whose
                // data is `(MESSAGE FLOAT)` (signal_error wraps the non-list
                // FLOAT in a one-element list). Mirror that exactly.
                return Err(signal(
                    "error",
                    vec![Value::string("JSON does not allow Inf or NaN"), *value],
                ));
            }
            // Use neomacs's canonical float printer (the same dtoastr-based
            // routine behind number-to-string / prin1) so json-serialize
            // matches GNU's float_to_string byte-for-byte — e.g. 1e20 →
            // "1e+20", 1e-5 → "1e-05" — and stays consistent with the rest
            // of the runtime. The nan/inf cases are rejected above, so the
            // finite path always yields valid JSON.
            out.push_str(&neovm_value::strings_core::format_float(f));
            Ok(())
        }

        ValueKind::String => json_out_lisp_string(out, value),

        ValueKind::Veclike(VecLikeType::Vector) => {
            check_container_depth(depth)?;
            let items = value.as_vector_data().unwrap().clone();
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                serialize_into(out, item, opts, depth + 1)?;
            }
            out.push(']');
            Ok(())
        }

        ValueKind::Veclike(VecLikeType::HashTable) => {
            check_container_depth(depth)?;
            let table = value.as_hash_table().unwrap().clone();
            // GNU's `json_out_object_hash` walks the table with `DOHASH`
            // (src/json.c), i.e. the slot order `maphash` visits.  The
            // storage's hash index is in hash order, and iterating it made a
            // two-member object serialize "s" before "k" where GNU preserved
            // the parse order (`json_roundtrip` in the oracle suite).
            let entries = table.data.keyed_entries_in_slot_order();
            out.push('{');
            for (index, (_, entry)) in entries.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                // GNU does `CHECK_STRING (k)` whatever the table's test is,
                // then writes the key through `json_out_string`, so the
                // `:null-object` and `:false-object` sentinels never apply.
                if !entry.key.is_string() {
                    return Err(signal(
                        LispCondition::WrongTypeArgument,
                        vec![Value::symbol("stringp"), entry.key],
                    ));
                }
                json_out_lisp_string(out, &entry.key)?;
                out.push(':');
                serialize_into(out, &entry.value, opts, depth + 1)?;
            }
            out.push('}');
            Ok(())
        }

        // Alist (list of (KEY . VALUE) conses) or plist (flat KEY VALUE …)
        // → JSON object.
        ValueKind::Cons => {
            check_container_depth(depth)?;
            serialize_cons_object(out, value, opts, depth)
        }

        ValueKind::Nil => {
            out.push_str("{}");
            Ok(())
        }

        _ if opts.symbol_mode.unwrap(*value).is_t() => {
            out.push_str("true");
            Ok(())
        }

        _ => Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("json-value-p"), *value],
        )),
    }
}

/// Validate and quote a Lisp string, mirroring GNU `json_out_string`.
fn json_out_lisp_string(out: &mut String, value: &Value) -> Result<(), Flow> {
    let string = value
        .as_lisp_string()
        .expect("ValueKind::String must carry LispString payload");
    let bytes = string.as_bytes();
    let rendered = if bytes.is_ascii() {
        // SAFETY: ASCII is valid UTF-8. Most strings are ASCII, and the
        // word-at-a-time `is_ascii` makes the full UTF-8 validation below a
        // second pass they do not need.
        unsafe { std::str::from_utf8_unchecked(bytes) }
    } else if string.is_multibyte() {
        string.as_utf8_str().ok_or_else(|| {
            signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("json-value-p"), *value],
            )
        })?
    } else {
        // A unibyte string may only hold ASCII.
        return Err(signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol("json-value-p"), *value],
        ));
    };
    json_encode_string_into(out, rendered);
    Ok(())
}

/// Serialize an alist or plist as a JSON object, mirroring GNU
/// `json_out_object_cons`.
///
/// The list is treated as an alist when its first element is a cons, and as
/// a plist otherwise. Keys must be symbols (keywords are symbols whose name
/// begins with `:`); for plists a single leading `:` is stripped from the
/// emitted key. When a key repeats, the first value wins and later
/// duplicates are dropped, matching GNU.
fn serialize_cons_object(
    out: &mut String,
    value: &Value,
    opts: &SerializeOpts,
    depth: usize,
) -> Result<(), Flow> {
    let is_alist = matches!(value.cons_car().kind(), ValueKind::Cons);
    let wrong_type = |predicate: &'static str, object: Value| {
        signal(
            LispCondition::WrongTypeArgument,
            vec![Value::symbol(predicate), object],
        )
    };

    let mut seen: SmallVec<[SymId; 16]> = SmallVec::new();
    let mut wrote_any = false;
    out.push('{');

    // Walk like GNU's FOR_EACH_TAIL: element errors come before the end check,
    // and a cycle signals `circular-list` rather than `listp`.
    let mut tail = *value;
    let mut cycle = GnuTailCycle::new(tail);
    while matches!(tail.kind(), ValueKind::Cons) {
        let (key, val) = if is_alist {
            let pair = tail.cons_car();
            if !matches!(pair.kind(), ValueKind::Cons) {
                return Err(wrong_type("consp", pair));
            }
            (pair.cons_car(), pair.cons_cdr())
        } else {
            let key = tail.cons_car();
            // A plist must supply a value for every key.
            tail = tail.cons_cdr();
            if !matches!(tail.kind(), ValueKind::Cons) {
                return Err(wrong_type("consp", tail));
            }
            (key, tail.cons_car())
        };
        let (symbol, name) = symbol_object_key(&key, opts.symbol_mode)?;
        // Dedup on the symbol itself (like GNU's symset). Alist keys are
        // emitted verbatim, plist keys without their leading colon.
        if push_unique(&mut seen, symbol) {
            if wrote_any {
                out.push(',');
            }
            wrote_any = true;
            json_encode_string_into(
                out,
                if is_alist {
                    name
                } else {
                    strip_plist_colon(name)
                },
            );
            out.push(':');
            serialize_into(out, &val, opts, depth + 1)?;
        }
        tail = tail.cons_cdr();
        cycle.check(tail)?;
    }
    if !tail.is_nil() {
        return Err(wrong_type("listp", *value));
    }
    out.push('}');
    Ok(())
}

/// Record `symbol` as seen; return true if it was not already present
/// (first-occurrence-wins semantics).
///
/// Keyed on the symbol id, which is what GNU's `symset` compares and what the
/// call sites always meant: interning makes identity exact, so this replaces a
/// string compare per probe -- and a `String` allocation per key -- with a
/// `u32` compare. Objects carry a handful of keys, so a linear scan over a
/// stack-resident `SmallVec` beats hashing them.
fn push_unique(seen: &mut SmallVec<[SymId; 16]>, symbol: SymId) -> bool {
    if seen.contains(&symbol) {
        false
    } else {
        seen.push(symbol);
        true
    }
}

/// Strip a single leading `:` from a plist key name, but only when more
/// characters follow (mirrors GNU `str[0] == ':' && str[1]`).
fn strip_plist_colon(name: &str) -> &str {
    match name.strip_prefix(':') {
        Some(rest) if !rest.is_empty() => rest,
        _ => name,
    }
}

/// Convert a Lisp value to a string key for a JSON object (used when
/// serializing alists).
///
/// Emacs `json-serialize` expects symbol keys in alists.
/// The object key for a symbol, as its interned name and its id.
///
/// The name is borrowed: `resolve_sym` already yields `&'static str`, so the
/// former `to_owned()` allocated a `String` per key for nothing. The id is
/// returned alongside because duplicate detection compares symbol IDENTITY,
/// which is a `SymId` compare rather than a string compare.
fn symbol_object_key(value: &Value, mode: SymbolMode) -> Result<(SymId, &'static str), Flow> {
    value
        .as_symbol_id(mode)
        .map(|id| (id, resolve_sym(id)))
        .ok_or_else(|| {
            signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("symbolp"), *value],
            )
        })
}

/// Encode a Rust string as a JSON string with proper escaping.
#[cfg(test)]
fn json_encode_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    json_encode_string_into(&mut out, s);
    out
}

/// Append the JSON encoding of `s` to `out`, quotes included.
///
/// Copies in runs rather than character by character. JSON strings are
/// overwhelmingly escape-free -- an LSP diagnostic message, a URI, a symbol
/// name -- so the common path is one `push_str` of the whole string, not one
/// `push` per character with its UTF-8 re-encode. Scanning bytes is safe
/// because every byte that needs escaping is ASCII: UTF-8 continuation bytes
/// are all >= 0x80, so a multi-byte character can never contain one, and its
/// bytes are copied through untouched exactly as the per-char loop emitted
/// them.
fn json_encode_string_into(out: &mut String, s: &str) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('"');
    let bytes = s.as_bytes();
    let mut run_start = 0usize;
    for (index, &byte) in bytes.iter().enumerate() {
        let escape: &str = match byte {
            b'"' => "\\\"",
            b'\\' => "\\\\",
            b'\n' => "\\n",
            b'\r' => "\\r",
            b'\t' => "\\t",
            0x08 => "\\b",
            0x0C => "\\f",
            // Other C0 controls have no short form and take \u00XX.
            b if b < 0x20 => {
                out.push_str(&s[run_start..index]);
                out.push_str("\\u00");
                out.push(HEX[usize::from(b >> 4)] as char);
                out.push(HEX[usize::from(b & 0xf)] as char);
                run_start = index + 1;
                continue;
            }
            _ => continue,
        };
        out.push_str(&s[run_start..index]);
        out.push_str(escape);
        run_start = index + 1;
    }
    out.push_str(&s[run_start..]);
    out.push('"');
}

// ===========================================================================
// JSON error conditions
// ===========================================================================

/// The JSON error conditions, mirroring GNU `src/json.c` `syms_of_json`.
///
/// Keeping every condition symbol in one exhaustive enum (rather than as
/// scattered string literals) makes the error domain explicit, prevents
/// typos, and lets the compiler check that each signalling site names a
/// real condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JsonError {
    /// `json-parse-error` — generic parse failure.
    Parse,
    /// `json-end-of-file` — input ended in the middle of a value.
    EndOfFile,
    /// `json-trailing-content` — extra non-whitespace after the value.
    TrailingContent,
    /// `json-object-too-deep` — nesting exceeded [`MAX_PARSE_DEPTH`].
    ObjectTooDeep,
    /// `json-utf8-decode-error` — invalid UTF-8 in the input.
    Utf8Decode,
    /// `json-invalid-surrogate-error` — malformed UTF-16 surrogate pair.
    InvalidSurrogate,
    /// `json-escape-sequence-error` — malformed `\` escape in a string.
    EscapeSequence,
    /// `json-number-out-of-range-error` — numeric literal outside range.
    NumberOutOfRange,
}

impl JsonError {
    fn symbol(self) -> &'static str {
        match self {
            JsonError::Parse => "json-parse-error",
            JsonError::EndOfFile => "json-end-of-file",
            JsonError::TrailingContent => "json-trailing-content",
            JsonError::ObjectTooDeep => "json-object-too-deep",
            JsonError::Utf8Decode => "json-utf8-decode-error",
            JsonError::InvalidSurrogate => "json-invalid-surrogate-error",
            JsonError::EscapeSequence => "json-escape-sequence-error",
            JsonError::NumberOutOfRange => "json-number-out-of-range-error",
        }
    }
}

// ===========================================================================
// JSON Parser (JSON string → Lisp value)
// ===========================================================================

// Keep guarded mutation out of the inactive insertion clone.
#[cold]
#[inline(never)]
fn insert_json_hash_member_concurrent(
    table_value: Value,
    hash_key: HashKey,
    key_value: Value,
    value: Value,
) {
    let _ = table_value.with_hash_table_mut_concurrent(|table| {
        table.insert(hash_key, key_value, value);
    });
}

/// Parser state: a cursor over the input bytes, plus the scratch space one
/// parse reuses for every container and string in the document.
///
/// GNU's parser (`struct json_parser`, src/json.c) keeps the same two
/// workspaces for the same reason: a document is mostly small strings and
/// small containers, and allocating a fresh buffer for each one -- a `Vec`
/// per array or object, a `String` per string, then a copy of each into the
/// heap object -- costs more than the parse itself.
struct JsonParser<'a> {
    input: &'a [u8],
    input_multibyte: bool,
    pos: usize,
    /// Current object/array nesting depth, bounded by [`MAX_PARSE_DEPTH`].
    depth: usize,
    opts: ParseOpts,
    /// Members of every array and alist/plist object still being parsed,
    /// innermost container last (GNU `object_workspace`). A container
    /// records where its members start, pushes them as it reads them, and
    /// on its closing bracket builds the Lisp vector or list from that tail
    /// and truncates it away.
    object_workspace: Vec<Value>,
    /// Decoded bytes of the string being read (GNU `byte_workspace`). The
    /// Lisp string is made from it with one exact-size copy.
    byte_workspace: Vec<u8>,
    /// Alist and plist key symbols, this thread's [`ObjectKeySymbols`] for
    /// the length of the parse.
    key_symbols: ObjectKeySymbols,
    /// The distinct alist and plist key symbols of this parse, for the
    /// caller to make obarray members: GNU interns every key into the
    /// obarray (`intern_c_multibyte`), and the parser has none.
    key_ids: Vec<SymId>,
}

impl Drop for JsonParser<'_> {
    fn drop(&mut self) {
        std::mem::take(&mut self.key_symbols).give_back();
    }
}

/// Maximum object/array nesting accepted while parsing, mirroring GNU
/// `src/json.c` `json_parser.available_depth` (10000). Without this bound
/// the recursive-descent parser would overflow the stack on adversarial
/// input instead of signalling a catchable error.
const MAX_PARSE_DEPTH: usize = 10000;

/// Integer literals up to this many digits are converted while they are
/// scanned: 10^18 - 1 is below `i64::MAX`.
const MAX_ACCUMULATED_DIGITS: usize = 18;

/// A string [`JsonParser::parse_string_body`] decoded: where its bytes are,
/// and how many characters they hold.
#[derive(Clone, Copy, Debug)]
struct DecodedString {
    bytes: DecodedBytes,
    chars: usize,
}

#[derive(Clone, Copy, Debug)]
enum DecodedBytes {
    /// The string had no escape, so its decoded bytes are this stretch of
    /// the input.
    Input { start: usize, end: usize },
    /// [`JsonParser::byte_workspace`] holds them.
    Workspace,
}

/// The symbols of alist and plist keys this thread has parsed, by decoded
/// name: one map of plain symbols (alists) and one of keywords (plists).
///
/// Each thread owns its own, and a parser takes it for the length of the
/// parse (no lock, nothing shared). An entry stays right as long as the
/// symbol registry maps the name to the same symbol, which only `unintern`
/// or a dump restore changes, and both advance
/// [`symbol_registry_epoch_value`]; the maps are dropped whenever that
/// epoch has moved. Interned symbols are permanent, so an entry never
/// outlives its value. Bounded: names longer than [`Self::MAX_NAME_BYTES`]
/// are not kept, and a map that reaches [`Self::MAX_ENTRIES`] starts over,
/// so a document keyed by unique names (file URIs, ids) cannot grow it
/// without limit.
///
/// Each parse also reports its distinct key symbols once (see
/// [`JsonParser::key_ids`]), so the caller can make them members of its
/// obarray, cached or not.
#[derive(Debug, Default)]
struct ObjectKeySymbols {
    /// The registry epoch the entries were made under.
    epoch: u64,
    /// Counts the parses that took this cache; an entry remembers the last
    /// one that reported it.
    parse: u64,
    symbols: FxHashMap<Box<[u8]>, KeySymbol>,
    keywords: FxHashMap<Box<[u8]>, KeySymbol>,
    /// Whether this is the thread's cache, to be given back on drop.
    taken: bool,
}

/// One [`ObjectKeySymbols`] entry.
#[derive(Clone, Copy, Debug)]
struct KeySymbol {
    id: SymId,
    /// The [`ObjectKeySymbols::parse`] that last reported this symbol.
    reported_in: u64,
}

thread_local! {
    static OBJECT_KEY_SYMBOLS: std::cell::RefCell<ObjectKeySymbols> =
        std::cell::RefCell::default();
}

impl ObjectKeySymbols {
    /// Longest name kept. Real object keys are short; this bounds the
    /// memory a map can hold to about `MAX_ENTRIES * MAX_NAME_BYTES`.
    const MAX_NAME_BYTES: usize = 64;
    /// Most names a map keeps before it starts over.
    const MAX_ENTRIES: usize = 4096;

    /// This thread's cache, emptied first if the symbol registry has moved
    /// since it was filled.
    fn take() -> Self {
        let mut cache = OBJECT_KEY_SYMBOLS
            .try_with(|cache| std::mem::take(&mut *cache.borrow_mut()))
            .unwrap_or_default();
        let epoch = neovm_value::intern::symbol_registry_epoch_value();
        if cache.epoch != epoch {
            cache.symbols.clear();
            cache.keywords.clear();
            cache.epoch = epoch;
        }
        cache.taken = true;
        cache.parse = cache.parse.wrapping_add(1);
        cache
    }

    /// Return a cache [`Self::take`] lent out to its thread.
    fn give_back(self) {
        if self.taken {
            // During thread teardown the slot is gone; the entries just go.
            let _ = OBJECT_KEY_SYMBOLS.try_with(|cache| *cache.borrow_mut() = self);
        }
    }

    /// The symbol (`keyword` false) or keyword for the key named `name`,
    /// which is valid UTF-8, adding it to `reported` the first time this
    /// parse meets it.
    fn symbol(&mut self, name: &[u8], keyword: bool, reported: &mut Vec<SymId>) -> Value {
        let parse = self.parse;
        let map = if keyword {
            &mut self.keywords
        } else {
            &mut self.symbols
        };
        let id = match map.get_mut(name) {
            Some(entry) => {
                if entry.reported_in != parse {
                    entry.reported_in = parse;
                    reported.push(entry.id);
                }
                entry.id
            }
            None => {
                let id = Self::intern_key(name, keyword);
                reported.push(id);
                if name.len() <= Self::MAX_NAME_BYTES {
                    if map.len() >= Self::MAX_ENTRIES {
                        map.clear();
                    }
                    let entry = KeySymbol {
                        id,
                        reported_in: parse,
                    };
                    map.insert(name.into(), entry);
                }
                id
            }
        };
        // A keyword is a symbol whose name starts with a colon; the two
        // constructors make the same value.
        if keyword {
            Value::from_kw_id(id)
        } else {
            Value::from_sym_id(id)
        }
    }

    #[inline(never)]
    fn intern_key(name: &[u8], keyword: bool) -> SymId {
        let name = std::str::from_utf8(name).expect("the string decoder only accepts UTF-8");
        if keyword {
            let mut keyword = String::with_capacity(name.len() + 1);
            keyword.push(':');
            keyword.push_str(name);
            intern(&keyword)
        } else {
            intern(name)
        }
    }
}

impl<'a> JsonParser<'a> {
    fn new(input: &'a [u8], input_multibyte: bool, opts: ParseOpts) -> Self {
        // A hash-table parse makes string keys and needs no symbols.
        let key_symbols = match opts.object_type {
            ObjectType::Alist | ObjectType::Plist => ObjectKeySymbols::take(),
            ObjectType::HashTable => ObjectKeySymbols::default(),
        };
        Self {
            input,
            input_multibyte,
            pos: 0,
            depth: 0,
            opts,
            object_workspace: Vec::new(),
            byte_workspace: Vec::new(),
            key_symbols,
            key_ids: Vec::new(),
        }
    }

    /// Build a `(LINE nil POS)` signal at the current cursor, matching the
    /// shape and values of GNU `json_signal_error`
    /// (src/json.c:json_signal_error).
    ///
    /// GNU computes `byte = input_current - input_begin`, then
    /// `pos = string_byte_to_pos (byte)` (= `count_chars` for multibyte input,
    /// raw byte offset otherwise) and `line = count_newlines (byte) + 1`.
    /// Because every GNU read advances `input_current` *past* the byte it just
    /// consumed before any error fires, POS is the character offset *after* the
    /// offending character.  Our parser is peek-based, so each signalling site
    /// advances the cursor over the offending char first and then calls this
    /// (mirroring GNU's consume-then-signal order).  The LINE number is
    /// deprecated upstream and provided only for compatibility.
    fn signal_at_pos(&self, kind: JsonError) -> Flow {
        let byte = self.pos.min(self.input.len());
        let line = self.input[..byte].iter().filter(|&&b| b == b'\n').count() as i64 + 1;
        signal(
            kind.symbol(),
            vec![
                Value::fixnum(line),
                Value::NIL,
                Value::fixnum(self.source_char_pos() as i64),
            ],
        )
    }

    /// Read the next byte, advancing the cursor, mirroring GNU
    /// `json_input_get`.  At end of input this signals `json-end-of-file`,
    /// exactly like GNU's `json_input_get_slow_path` (src/json.c).
    fn consume(&mut self) -> Result<u8, Flow> {
        match self.input.get(self.pos).copied() {
            Some(b) => {
                self.pos += 1;
                Ok(b)
            }
            None => Err(self.signal_at_pos(JsonError::EndOfFile)),
        }
    }

    /// Read the next byte if one is available, advancing the cursor, mirroring
    /// GNU `json_input_get_if_possible`.  Returns `None` at end of input.
    fn consume_if_possible(&mut self) -> Option<u8> {
        let b = self.input.get(self.pos).copied()?;
        self.pos += 1;
        Some(b)
    }

    /// Put back the last byte read by [`Self::consume`] /
    /// [`Self::consume_if_possible`], mirroring GNU `json_input_put_back`.
    fn put_back(&mut self) {
        self.pos -= 1;
    }

    /// Skip whitespace and return the first non-whitespace byte, consuming it.
    /// Mirrors GNU `json_skip_whitespace`: it reads through `json_input_get`,
    /// so reaching end of input signals `json-end-of-file`.
    ///
    /// It runs before every token, so it scans by index and leaves the
    /// end-of-input error to a cold path, at the same position
    /// [`Self::consume`] reports it.
    #[inline]
    fn skip_ws_consume(&mut self) -> Result<u8, Flow> {
        while let Some(&b) = self.input.get(self.pos) {
            self.pos += 1;
            if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                return Ok(b);
            }
        }
        Err(self.end_of_input())
    }

    #[cold]
    #[inline(never)]
    fn end_of_input(&self) -> Flow {
        self.signal_at_pos(JsonError::EndOfFile)
    }

    /// Skip whitespace and return the first non-whitespace byte, or `None` at
    /// end of input.  Mirrors GNU `json_skip_whitespace_if_possible`.
    fn skip_ws_if_possible(&mut self) -> Option<u8> {
        loop {
            let b = self.consume_if_possible()?;
            if !matches!(b, b' ' | b'\t' | b'\n' | b'\r') {
                return Some(b);
            }
        }
    }

    /// Enter one level of object/array nesting, signalling
    /// `json-object-too-deep` if the recursion limit is exceeded. Paired with
    /// [`Self::leave_nesting`] on the success path; on the error path the
    /// whole parse unwinds, so the counter need not be restored.
    fn enter_nesting(&mut self) -> Result<(), Flow> {
        self.depth += 1;
        if self.depth > MAX_PARSE_DEPTH {
            return Err(self.signal_at_pos(JsonError::ObjectTooDeep));
        }
        Ok(())
    }

    fn leave_nesting(&mut self) {
        self.depth -= 1;
    }

    #[cfg_attr(feature = "test-support", visibility::make(pub))]
    /// Current byte (or None if at end).
    fn peek(&self) -> Option<u8> {
        self.input.get(self.pos).copied()
    }

    /// Advance by one byte.
    fn advance(&mut self) {
        self.pos += 1;
    }

    fn source_char_pos(&self) -> usize {
        if self.input_multibyte {
            neovm_value::emacs_char::byte_to_char_pos(self.input, self.pos)
        } else {
            self.pos
        }
    }

    /// Whether `c` may continue a `true`/`false`/`null` token, mirroring GNU
    /// `json_is_token_char` (src/json.c).  Used so that e.g. `truer` is one
    /// invalid token rather than `true` followed by `r`.
    fn is_token_char(c: u8) -> bool {
        c.is_ascii_alphanumeric() || c == b'-'
    }

    /// Parse a single top-level JSON value, mirroring GNU `json_parse`:
    /// `json_parse_value (parser, json_skip_whitespace (parser))`.  Leading
    /// whitespace is skipped via the consuming reader, so empty or
    /// whitespace-only input signals `json-end-of-file`.
    fn parse(&mut self) -> Result<Value, Flow> {
        let c = self.skip_ws_consume()?;
        self.parse_value(c)
    }

    /// Parse one JSON value whose first non-whitespace byte `c` has already
    /// been consumed, mirroring GNU `json_parse_value`.
    fn parse_value(&mut self, c: u8) -> Result<Value, Flow> {
        match c {
            b'"' => self.parse_string_value(),
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'-' | b'0'..=b'9' => self.parse_number(c),
            b't' => self.parse_literal_token(b"rue", Value::T),
            b'f' => self.parse_literal_token(b"alse", self.opts.false_object),
            b'n' => self.parse_literal_token(b"ull", self.opts.null_object),
            _ => Err(self.signal_at_pos(JsonError::Parse)),
        }
    }

    /// Parse the remaining bytes of a `true`/`false`/`null` token (the leading
    /// byte is already consumed) and ensure the token is not immediately
    /// followed by another token char, mirroring GNU `json_parse_value`.
    fn parse_literal_token(&mut self, rest: &[u8], value: Value) -> Result<Value, Flow> {
        for &expected in rest {
            if self.consume_if_possible() != Some(expected) {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
        }
        match self.consume_if_possible() {
            Some(c2) if Self::is_token_char(c2) => Err(self.signal_at_pos(JsonError::Parse)),
            Some(_) => {
                self.put_back();
                Ok(value)
            }
            None => Ok(value),
        }
    }

    /// Parse a JSON string value; the opening `"` has already been consumed.
    fn parse_string_value(&mut self) -> Result<Value, Flow> {
        let decoded = self.parse_string_body()?;
        Ok(Value::heap_string(self.decoded_lisp_string(decoded)))
    }

    /// The bytes of a string [`Self::parse_string_body`] decoded.
    fn decoded_bytes(&self, decoded: DecodedString) -> &[u8] {
        match decoded.bytes {
            DecodedBytes::Input { start, end } => &self.input[start..end],
            DecodedBytes::Workspace => &self.byte_workspace,
        }
    }

    /// A decoded string as a Lisp string.
    ///
    /// Always multibyte, ASCII and empty strings included: GNU builds every
    /// parsed string with `make_multibyte_string (str, nchars, nbytes)`
    /// (`json_parse_string`, src/json.c), so `multibyte-string-p` is t for
    /// all of them. One copy, sized exactly, with the character count the
    /// decoder already has, so the bytes are not walked again.
    fn decoded_lisp_string(&self, decoded: DecodedString) -> LispString {
        let bytes = self.decoded_bytes(decoded);
        // Room for the NUL every payload carries, so it is not reallocated.
        let mut data = Vec::with_capacity(bytes.len() + 1);
        data.extend_from_slice(bytes);
        LispString::from_emacs_bytes_with_chars(data, decoded.chars)
    }

    /// Decode a JSON string body (the opening `"` has already been consumed)
    /// and say where its bytes are.  Mirrors GNU `json_parse_string`, which
    /// reads bytes through `json_input_get`, so a string left open at end of
    /// input signals `json-end-of-file`.
    ///
    /// Until its first escape a string's decoded bytes are its input bytes,
    /// so a string without escapes -- nearly every key and value in practice
    /// -- is only scanned and validated here, and copied once, by its
    /// consumer. The first backslash moves the decoding to the byte
    /// workspace.
    fn parse_string_body(&mut self) -> Result<DecodedString, Flow> {
        let start = self.pos;
        let mut chars = 0usize;
        loop {
            // Skip the run of plain ASCII up to the next byte that needs
            // deciding on -- quote, backslash, control, or a multi-byte lead.
            let run = plain_ascii_run_len(&self.input[self.pos..]);
            self.pos += run;
            chars += run;
            match self.peek() {
                None => {
                    return Err(self.signal_at_pos(JsonError::EndOfFile));
                }
                Some(b'"') => {
                    let end = self.pos;
                    self.advance();
                    return Ok(DecodedString {
                        bytes: DecodedBytes::Input { start, end },
                        chars,
                    });
                }
                Some(b'\\') => {
                    self.byte_workspace.clear();
                    self.byte_workspace
                        .extend_from_slice(&self.input[start..self.pos]);
                    return self.parse_escaped_string_body(chars);
                }
                Some(b) if b < 0x20 => {
                    // Unescaped control characters are not valid inside a
                    // JSON string (GNU marks 0x00-0x1F as non-plain in
                    // `json_plain_char` and signals a parse error).  GNU has
                    // already consumed the byte (`json_input_get`), so the
                    // position is just after it.
                    self.advance();
                    return Err(self.signal_at_pos(JsonError::Parse));
                }
                Some(lead) => {
                    // A byte >= 0x80: the run above stops at every byte
                    // outside 0x20..0x7F, and the arms above take the rest.
                    self.advance();
                    self.consume_utf8_char(lead)?;
                    chars += 1;
                }
            }
        }
    }

    /// The rest of [`Self::parse_string_body`] from the first backslash, with
    /// the `chars` characters before it already in the byte workspace.
    #[inline(never)]
    fn parse_escaped_string_body(&mut self, mut chars: usize) -> Result<DecodedString, Flow> {
        loop {
            let run_start = self.pos;
            let run = plain_ascii_run_len(&self.input[run_start..]);
            if run > 0 {
                self.byte_workspace
                    .extend_from_slice(&self.input[run_start..run_start + run]);
                chars += run;
                self.pos += run;
            }
            match self.peek() {
                None => {
                    return Err(self.signal_at_pos(JsonError::EndOfFile));
                }
                Some(b'"') => {
                    self.advance();
                    return Ok(DecodedString {
                        bytes: DecodedBytes::Workspace,
                        chars,
                    });
                }
                Some(b'\\') => {
                    self.advance();
                    let decoded = match self.peek() {
                        Some(b'"') => '"',
                        Some(b'\\') => '\\',
                        Some(b'/') => '/',
                        Some(b'n') => '\n',
                        Some(b'r') => '\r',
                        Some(b't') => '\t',
                        Some(b'b') => '\x08',
                        Some(b'f') => '\x0C',
                        Some(b'u') => {
                            self.advance();
                            let escaped = self.parse_unicode_escape_char()?;
                            self.push_workspace_char(escaped);
                            chars += 1;
                            continue;
                        }
                        Some(_) => {
                            // GNU consumes the offending escape char via
                            // `json_input_get` before signalling, so the
                            // reported position is just after it.
                            self.advance();
                            return Err(self.signal_at_pos(JsonError::EscapeSequence));
                        }
                        None => {
                            return Err(self.signal_at_pos(JsonError::EndOfFile));
                        }
                    };
                    self.advance();
                    self.byte_workspace.push(decoded as u8);
                    chars += 1;
                }
                Some(b) if b < 0x20 => {
                    // See the same arm in `parse_string_body`.
                    self.advance();
                    return Err(self.signal_at_pos(JsonError::Parse));
                }
                Some(lead) => {
                    let char_start = self.pos;
                    self.advance();
                    self.consume_utf8_char(lead)?;
                    self.byte_workspace
                        .extend_from_slice(&self.input[char_start..self.pos]);
                    chars += 1;
                }
            }
        }
    }

    /// Consume the rest of one UTF-8 character whose first byte `lead`
    /// (>= 0x80) has just been consumed, validating it strictly, as the
    /// `c & 0x80` branch of GNU `json_parse_string` does.
    ///
    /// Like GNU, this reads each further byte through the consuming reader,
    /// so input that ends inside a character signals `json-end-of-file`,
    /// and a malformed character signals `json-utf8-decode-error` at the
    /// position just after the last byte read: a non-continuation byte where
    /// one is required, an overlong form, a surrogate, or a code point above
    /// U+10FFFF. The input is UTF-8 whether the Lisp string holding it is
    /// unibyte or multibyte, so a raw-byte character's internal bytes are
    /// malformed here too. A valid character's bytes are its decoded bytes.
    fn consume_utf8_char(&mut self, lead: u8) -> Result<(), Flow> {
        let is_continuation = |b: u8| b & 0xC0 == 0x80;
        let c1 = self.consume()?;
        if !is_continuation(c1) || lead <= 0xC1 {
            return Err(self.signal_at_pos(JsonError::Utf8Decode));
        }
        let (lead_bits, c1_bits) = (u32::from(lead), u32::from(c1 & 0x3F));
        if lead <= 0xDF {
            return Ok(());
        }
        if lead <= 0xEF {
            let c2 = self.consume()?;
            let v = ((lead_bits & 0x0F) << 12) + (c1_bits << 6) + u32::from(c2 & 0x3F);
            if !is_continuation(c2) || v < 0x800 || (0xD800..=0xDFFF).contains(&v) {
                return Err(self.signal_at_pos(JsonError::Utf8Decode));
            }
            return Ok(());
        }
        if lead <= 0xF7 {
            let c2 = self.consume()?;
            let c3 = self.consume()?;
            let v = ((lead_bits & 0x07) << 18)
                + (c1_bits << 12)
                + (u32::from(c2 & 0x3F) << 6)
                + u32::from(c3 & 0x3F);
            if !is_continuation(c2) || !is_continuation(c3) || !(0x10000..=0x10FFFF).contains(&v) {
                return Err(self.signal_at_pos(JsonError::Utf8Decode));
            }
            return Ok(());
        }
        Err(self.signal_at_pos(JsonError::Utf8Decode))
    }

    /// Append `c` to [`Self::byte_workspace`] in UTF-8, which is the Emacs
    /// internal encoding of every Unicode scalar value.
    fn push_workspace_char(&mut self, c: char) {
        let mut utf8 = [0u8; 4];
        self.byte_workspace
            .extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
    }

    /// Decode the rest of a `\uXXXX` escape (the `\u` is consumed), joining a
    /// UTF-16 surrogate pair into one character.
    ///
    /// A malformed pair (high surrogate without a following low surrogate, or
    /// a lone low surrogate) is a json-invalid-surrogate-error in GNU rather
    /// than a silent U+FFFD substitution.
    fn parse_unicode_escape_char(&mut self) -> Result<char, Flow> {
        let cp = self.parse_unicode_escape()?;
        if (0xD800..=0xDBFF).contains(&cp) {
            // High surrogate — must be followed by a \uXXXX low surrogate.
            // GNU reads the next two bytes with `json_input_get` (which
            // signals json-end-of-file at end of input) and reports the
            // position just after the consumed byte.
            if self.consume()? != b'\\' {
                return Err(self.signal_at_pos(JsonError::InvalidSurrogate));
            }
            if self.consume()? != b'u' {
                return Err(self.signal_at_pos(JsonError::InvalidSurrogate));
            }
            let low = self.parse_unicode_escape()?;
            if !(0xDC00..=0xDFFF).contains(&low) {
                return Err(self.signal_at_pos(JsonError::InvalidSurrogate));
            }
            let combined = 0x10000 + ((u32::from(cp) - 0xD800) << 10) + (u32::from(low) - 0xDC00);
            return char::from_u32(combined)
                .ok_or_else(|| self.signal_at_pos(JsonError::InvalidSurrogate));
        }
        // A lone low surrogate (0xDC00..=0xDFFF) is the only u16 range
        // char::from_u32 rejects.
        char::from_u32(u32::from(cp)).ok_or_else(|| self.signal_at_pos(JsonError::InvalidSurrogate))
    }

    /// Parse the 4 hex digits of a `\uXXXX` escape, mirroring GNU
    /// `json_parse_unicode`.  Each digit is read with the consuming reader, so
    /// end of input signals `json-end-of-file` and a non-hex byte (consumed
    /// first) signals `json-escape-sequence-error` at the position just after
    /// it.
    fn parse_unicode_escape(&mut self) -> Result<u16, Flow> {
        let mut value: u16 = 0;
        for _ in 0..4 {
            let b = self.consume()?;
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as u16,
                b'a'..=b'f' => (b - b'a' + 10) as u16,
                b'A'..=b'F' => (b - b'A' + 10) as u16,
                _ => return Err(self.signal_at_pos(JsonError::EscapeSequence)),
            };
            value = value * 16 + digit;
        }
        Ok(value)
    }

    /// Parse a JSON number whose first byte `c` (a digit or `-`) has already
    /// been consumed, mirroring GNU `json_parse_number`.
    fn parse_number(&mut self, c: u8) -> Result<Value, Flow> {
        // `c` was already consumed by the caller, so the literal starts one
        // byte back.
        let start = self.pos - 1;
        let mut is_float = false;

        // After an optional leading minus, an integer part is required.
        let first_digit = if c == b'-' {
            match self.peek() {
                Some(b) if b.is_ascii_digit() => {
                    self.advance();
                    b
                }
                _ => return Err(self.signal_at_pos(JsonError::Parse)),
            }
        } else {
            c
        };

        // Integer part: a single 0, or 1-9 followed by more digits. Its value
        // is accumulated on the way: eighteen digits cannot overflow, and a
        // longer literal is converted from its text below.
        let int_start = self.pos - 1;
        let mut magnitude = u64::from(first_digit - b'0');
        if first_digit != b'0' {
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    magnitude = magnitude.wrapping_mul(10).wrapping_add(u64::from(b - b'0'));
                    self.advance();
                } else {
                    break;
                }
            }
        }
        let int_digits = self.pos - int_start;

        // Fractional part.
        if self.peek() == Some(b'.') {
            is_float = true;
            self.advance();
            let frac_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            if self.pos == frac_start {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
        }

        // Exponent part.
        if let Some(b'e') | Some(b'E') = self.peek() {
            is_float = true;
            self.advance();
            if let Some(b'+') | Some(b'-') = self.peek() {
                self.advance();
            }
            let exp_start = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_digit() {
                    self.advance();
                } else {
                    break;
                }
            }
            if self.pos == exp_start {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
        }

        if !is_float && int_digits <= MAX_ACCUMULATED_DIGITS {
            let n = i64::try_from(magnitude).expect("eighteen decimal digits fit in i64");
            return Ok(Value::make_int(if c == b'-' { -n } else { n }));
        }

        let num_str = std::str::from_utf8(&self.input[start..self.pos]).unwrap_or("0");

        if is_float {
            let f: f64 = num_str
                .parse()
                .map_err(|_| self.signal_at_pos(JsonError::Parse))?;
            if f.is_infinite() {
                // JSON has no infinity literal, so an infinite result means
                // the magnitude overflowed the double range. GNU signals
                // json-number-out-of-range-error here rather than returning
                // an infinite float.
                return Err(self.signal_at_pos(JsonError::NumberOutOfRange));
            }
            Ok(Value::make_float(f))
        } else {
            // Integers that fit i64 stay fixnums (or promote to bignum at the
            // fixnum boundary via make_int); larger literals become bignums,
            // matching GNU which never loses integer precision.
            match num_str.parse::<i64>() {
                Ok(n) => Ok(Value::make_int(n)),
                Err(_) => Ok(Value::make_integer_from_str_or_zero(num_str)),
            }
        }
    }

    /// Parse a JSON array: the opening `[` has already been consumed.
    /// Mirrors GNU `json_parse_array`: the elements collect in the object
    /// workspace, and the vector or list is built once from them.
    fn parse_array(&mut self) -> Result<Value, Flow> {
        let mut c = self.skip_ws_consume()?;
        let first = self.object_workspace.len();

        if c != b']' {
            // GNU only counts nesting depth for non-empty containers.
            self.enter_nesting()?;
            loop {
                let val = self.parse_value(c)?;
                self.object_workspace.push(val);

                c = self.skip_ws_consume()?;
                if c == b']' {
                    self.leave_nesting();
                    break;
                }
                if c != b',' {
                    return Err(self.signal_at_pos(JsonError::Parse));
                }
                c = self.skip_ws_consume()?;
            }
        }

        let items = &self.object_workspace[first..];
        let array = match self.opts.array_type {
            ArrayType::Vector => Value::vector(items.to_vec()),
            ArrayType::List => Value::list_from_slice(items),
        };
        self.object_workspace.truncate(first);
        Ok(array)
    }

    /// Parse a JSON object: the opening `{` has already been consumed.
    /// Mirrors GNU `json_parse_object`.
    fn parse_object(&mut self) -> Result<Value, Flow> {
        let c = self.skip_ws_consume()?;

        match self.opts.object_type {
            ObjectType::HashTable => self.parse_object_hash_table(c),
            ObjectType::Alist | ObjectType::Plist => self.parse_object_list(c),
        }
    }

    /// Read an object member's key.  `c` is the already-consumed first char
    /// of the key, which must be `"`.  Mirrors GNU's per-member handling in
    /// `json_parse_object`.
    fn parse_object_key(&mut self, c: u8) -> Result<DecodedString, Flow> {
        if c != b'"' {
            return Err(self.signal_at_pos(JsonError::Parse));
        }
        self.parse_string_body()
    }

    /// Parse the `: value` part of an object member, mirroring GNU
    /// `json_parse_object_member_value`.
    fn parse_object_member_value(&mut self) -> Result<Value, Flow> {
        if self.skip_ws_consume()? != b':' {
            return Err(self.signal_at_pos(JsonError::Parse));
        }
        let vc = self.skip_ws_consume()?;
        self.parse_value(vc)
    }

    /// The hash-table key for the member name just read: a fresh Lisp
    /// string, and the `equal` key a Lisp string with that text gets.
    ///
    /// A JSON member name becomes a Lisp STRING key, so it must be keyed the
    /// way any Lisp string is -- `HashKey::Text` is a runtime tag and a
    /// `gethash` probe would never match it.
    fn member_hash_key(&self, name: DecodedString) -> (Value, HashKey) {
        let key = Value::heap_string(self.decoded_lisp_string(name));
        (
            key,
            HashKey::string_content(self.decoded_bytes(name), name.chars),
        )
    }

    fn parse_object_hash_table(&mut self, mut c: u8) -> Result<Value, Flow> {
        if neovm_value::tagged::gc::concurrent_hash_mutation_active() {
            return self.parse_object_hash_table_concurrent(c);
        }
        // The parser only scans bytes, allocates Values and constructs Flow
        // errors. Allocation/refill never collects or runs Lisp, and signal()
        // returns a pinned payload; signal hooks run after parsing returns.
        // Neither recursion nor an error path can install a heap or change
        // its Tier-H mode. Keep that contract enforced at GC entry whenever
        // the GEN-5 guard family is compiled (debug builds and test builds,
        // including the release workspace test graph).
        #[cfg(any(test, debug_assertions, feature = "test-support"))]
        let _guard = neovm_value::tagged::mutate::HeapMutClosureGuard::enter();
        let ht = Value::hash_table(HashTableTest::Equal);
        if c == b'}' {
            return Ok(ht);
        }
        self.enter_nesting()?;

        loop {
            let name = self.parse_object_key(c)?;
            let (key_val, hash_key) = self.member_hash_key(name);
            let val = self.parse_object_member_value()?;
            // SAFETY: the entry dispatch selected inactive Tier-H, and
            // the entire parser extent above has no callback or GC safe
            // point that could change that selection.
            let _ = unsafe {
                ht.with_hash_table_mut_inactive(|table| {
                    table.insert(hash_key, key_val, val);
                })
            };

            c = self.skip_ws_consume()?;
            if c == b'}' {
                self.leave_nesting();
                break;
            }
            if c != b',' {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
            c = self.skip_ws_consume()?;
        }

        Ok(ht)
    }

    #[cold]
    #[inline(never)]
    fn parse_object_hash_table_concurrent(&mut self, mut c: u8) -> Result<Value, Flow> {
        // The same parser mode-stability contract applies to this active
        // clone. Each insertion keeps the normal snapshot/mutation lease.
        #[cfg(any(test, debug_assertions, feature = "test-support"))]
        let _guard = neovm_value::tagged::mutate::HeapMutClosureGuard::enter();
        let ht = Value::hash_table(HashTableTest::Equal);
        if c == b'}' {
            return Ok(ht);
        }
        self.enter_nesting()?;

        loop {
            let name = self.parse_object_key(c)?;
            let (key_val, hash_key) = self.member_hash_key(name);
            let val = self.parse_object_member_value()?;
            insert_json_hash_member_concurrent(ht, hash_key, key_val, val);

            c = self.skip_ws_consume()?;
            if c == b'}' {
                self.leave_nesting();
                break;
            }
            if c != b',' {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
            c = self.skip_ws_consume()?;
        }

        Ok(ht)
    }

    /// The symbol for the alist or plist key `name`: the name itself for an
    /// alist, the keyword `:NAME` for a plist (GNU `json_parse_string` with
    /// `leading_colon`).
    ///
    /// A JSON document repeats its keys -- an LSP completion response carries
    /// the same dozen (`label`, `kind`, `textEdit`, `range`, ...) once per
    /// item -- and a JSON-RPC client parses thousands of messages made of
    /// the same few dozen. [`ObjectKeySymbols`] finds a key it has met
    /// before by its bytes, without building its name or probing the
    /// obarray; a new key is interned.
    fn object_key_symbol(&mut self, name: DecodedString) -> Value {
        let bytes = match name.bytes {
            DecodedBytes::Input { start, end } => &self.input[start..end],
            DecodedBytes::Workspace => self.byte_workspace.as_slice(),
        };
        let keyword = matches!(self.opts.object_type, ObjectType::Plist);
        self.key_symbols.symbol(bytes, keyword, &mut self.key_ids)
    }
    /// Parse an object as an alist `((KEY . VALUE) ...)` or a plist
    /// `(:KEY VALUE ...)`, mirroring GNU `json_parse_object`.  The list
    /// elements collect in the object workspace and the list is consed once
    /// from them; an empty object is nil.
    fn parse_object_list(&mut self, mut c: u8) -> Result<Value, Flow> {
        if c == b'}' {
            return Ok(Value::NIL);
        }
        let first = self.object_workspace.len();
        self.enter_nesting()?;

        loop {
            let name = self.parse_object_key(c)?;
            let key = self.object_key_symbol(name);
            match self.opts.object_type {
                ObjectType::Plist => {
                    // The key is pushed before the value is parsed: nested
                    // containers use the workspace above it and truncate
                    // back to it.
                    self.object_workspace.push(key);
                    let val = self.parse_object_member_value()?;
                    self.object_workspace.push(val);
                }
                ObjectType::Alist | ObjectType::HashTable => {
                    let val = self.parse_object_member_value()?;
                    self.object_workspace.push(Value::cons(key, val));
                }
            }

            c = self.skip_ws_consume()?;
            if c == b'}' {
                self.leave_nesting();
                break;
            }
            if c != b',' {
                return Err(self.signal_at_pos(JsonError::Parse));
            }
            c = self.skip_ws_consume()?;
        }

        let list = Value::list_from_slice(&self.object_workspace[first..]);
        self.object_workspace.truncate(first);
        Ok(list)
    }
}

/// Length of the run at the start of `bytes` that a JSON string copies
/// through unchanged: printable ASCII other than `"` and `\`.
///
/// Every other byte -- the closing quote, an escape, a control character, a
/// UTF-8 lead -- needs a decision, so the decoder stops there. Eight bytes are
/// tested at a time (the classic "has a byte below / equal to" word tricks),
/// since string bodies are most of a JSON document's bytes.
fn plain_ascii_run_len(bytes: &[u8]) -> usize {
    const ONES: u64 = u64::from_ne_bytes([0x01; 8]);
    const HIGHS: u64 = u64::from_ne_bytes([0x80; 8]);
    /// Nonzero iff some byte of `word` is below `n` (for `n` <= 0x80).
    const fn has_byte_below(word: u64, n: u8) -> u64 {
        word.wrapping_sub(ONES * n as u64) & !word & HIGHS
    }
    const fn has_byte_equal(word: u64, b: u8) -> u64 {
        has_byte_below(word ^ (ONES * b as u64), 1)
    }
    let is_plain = |b: u8| (0x20..0x80).contains(&b) && b != b'"' && b != b'\\';

    let mut len = 0;
    let mut words = bytes.chunks_exact(8);
    for chunk in &mut words {
        let word = u64::from_ne_bytes(chunk.try_into().expect("chunks_exact yields 8 bytes"));
        let stop = (word & HIGHS)
            | has_byte_below(word, 0x20)
            | has_byte_equal(word, b'"')
            | has_byte_equal(word, b'\\');
        if stop != 0 {
            return len + chunk.iter().take_while(|&&b| is_plain(b)).count();
        }
        len += 8;
    }
    len + words
        .remainder()
        .iter()
        .take_while(|&&b| is_plain(b))
        .count()
}

// ===========================================================================
// Public builtin functions
// ===========================================================================

/// `(json-serialize VALUE &rest ARGS)` — serialize a Lisp value to a JSON string.
///
/// ARGS are keyword arguments:
/// - `:null-object VALUE` — Lisp value to serialize as JSON null (default: :null)
/// - `:false-object VALUE` — Lisp value to serialize as JSON false (default: :false)
#[cfg(test)]
pub(crate) fn builtin_json_serialize(args: Vec<Value>) -> EvalResult {
    json_serialize_with_symbols(args, SymbolMode::Bare)
}

#[doc(hidden)]
pub fn builtin_json_serialize_in_state<H: TextHost>(host: &mut H, args: Vec<Value>) -> EvalResult {
    json_serialize_with_symbols(args, host.symbol_mode())
}

fn json_serialize_with_symbols(args: Vec<Value>, mode: SymbolMode) -> EvalResult {
    expect_min_args("json-serialize", &args, 1)?;
    let opts = parse_serialize_kwargs(&args, 1, mode)?;
    let json = serialize_to_json(&args[0], &opts, 0)?;
    // GNU `Fjson_serialize` returns `make_unibyte_string (jo.buf, jo.size)`:
    // a UNIBYTE string of raw UTF-8 bytes (multibyte chars are emitted as
    // their raw UTF-8 byte sequences, see `json_out_string`).  `json` already
    // holds exactly those bytes (a Rust UTF-8 String), so copy them unibyte
    // rather than as a multibyte string.
    Ok(Value::heap_string(LispString::from_unibyte_slice(
        json.as_str().as_bytes(),
    )))
}

/// `(json-parse-string STRING &rest ARGS)` — parse a JSON string into a Lisp value.
///
/// ARGS are keyword arguments:
/// - `:object-type SYMBOL` — `hash-table` (default), `alist`, or `plist`
/// - `:array-type SYMBOL` — `array` (default, yields vector) or `list`
/// - `:null-object VALUE` — Lisp value for JSON null (default: :null)
/// - `:false-object VALUE` — Lisp value for JSON false (default: :false)
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn builtin_json_parse_string(args: Vec<Value>) -> EvalResult {
    json_parse_string_with_symbols(args, SymbolMode::Bare, |_| {})
}

#[doc(hidden)]
pub fn builtin_json_parse_string_in_state<H: TextHost>(
    host: &mut H,
    args: Vec<Value>,
) -> EvalResult {
    let mode = host.symbol_mode();
    json_parse_string_with_symbols(args, mode, |ids| host.ensure_interned_global_ids(ids))
}

/// Parse `args` as `json-parse-string` does, handing `make_members` the key
/// symbols of the parse (see [`JsonParser::key_ids`]) whether
/// or not it succeeded.
fn json_parse_string_with_symbols(
    args: Vec<Value>,
    mode: SymbolMode,
    make_members: impl FnOnce(&[SymId]),
) -> EvalResult {
    expect_min_args("json-parse-string", &args, 1)?;
    let input = match args[0].kind() {
        ValueKind::String => args[0]
            .as_lisp_string()
            .expect("string object must carry LispString payload"),
        _ => {
            return Err(signal(
                LispCondition::WrongTypeArgument,
                vec![Value::symbol("stringp"), args[0]],
            ));
        }
    };
    let opts = parse_parse_kwargs(&args, 1, mode)?;
    let mut parser = JsonParser::new(input.as_bytes(), input.is_multibyte(), opts);
    let result = parser.parse().and_then(|value| {
        // Ensure there is no trailing non-whitespace, mirroring GNU's
        // `json_skip_whitespace_if_possible (&p) >= 0` check.  The trailing
        // char is consumed before signalling, so the reported position is
        // just after it.
        if parser.skip_ws_if_possible().is_some() {
            return Err(parser.signal_at_pos(JsonError::TrailingContent));
        }
        Ok(value)
    });
    make_members(&parser.key_ids);
    result
}

/// `(json-parse-buffer &rest ARGS)` — parse one JSON value from point.
///
/// Unlike `json-parse-string`, this parses a single JSON value starting at the
/// current point (after leading whitespace), leaves trailing buffer content
/// untouched, and advances point to just after the parsed value.
///
/// The parser reads the buffer's own bytes, as GNU's does (`Fjson_parse_buffer`
/// points its input at the text on each side of the gap). It used to read a
/// copy of everything from point to the end of the accessible region, text
/// properties included, so one LSP message cost a copy and a character count
/// of the whole unread process buffer before a byte was parsed.
#[doc(hidden)]
pub fn builtin_json_parse_buffer<H: EditHost>(host: &mut H, args: Vec<Value>) -> EvalResult {
    let opts = parse_parse_kwargs(&args, 0, host.symbol_mode())?;
    let (result, new_point) = {
        let (edit, _vars) = host.edit_and_vars();
        let buf = edit
            .buffers
            .current_buffer()
            .ok_or_else(|| signal("error", vec![Value::string("No current buffer")]))?;
        let point_base = buf.point_emacs_byte_pos();
        let unread = EmacsByteRange::new(point_base, buf.accessible_emacs_byte_region().end());
        let multibyte = buf.get_multibyte();
        let parse = |input: &[u8]| {
            let mut parser = JsonParser::new(input, multibyte, opts.clone());
            let parsed = parser.parse().map(|value| (value, parser.pos));
            (parsed, std::mem::take(&mut parser.key_ids))
        };
        // Moving the gap out of the unread text is what GNU's searches do
        // too; a chunked text backend lends no window, and gets the copy.
        buf.try_make_emacs_byte_range_contiguous(unread);
        let (parsed, key_ids) = match buf.with_contiguous_emacs_byte_range(unread, parse) {
            Some(parsed) => parsed,
            None => parse(
                buf.buffer_substring_lisp_string_no_properties_range(unread)
                    .as_bytes(),
            ),
        };
        host.ensure_interned_global_ids(&key_ids);
        let (value, consumed) = parsed?;
        (value, point_base.add_len(EmacsByteLen::new(consumed)))
    };
    let (edit, _vars) = host.edit_and_vars();
    if let Some(current_id) = edit.buffers.current_buffer_id() {
        let _ = edit
            .buffers
            .goto_buffer_emacs_byte_pos(current_id, new_point);
    }
    Ok(result)
}

fn json_insert_target(buffers: &BufferManager) -> Result<BufferId, Flow> {
    buffers
        .current_buffer_id()
        .ok_or_else(|| signal("error", vec![Value::string("No current buffer")]))
}

/// Measure inserting `json` at the live point of `buffer_id`, and report
/// whether that buffer is multibyte.
fn json_insertion_at_point(
    buffers: &BufferManager,
    buffer_id: BufferId,
    json: &str,
) -> Result<(TextChange, bool), Flow> {
    let (insert_pos, multibyte) = buffers
        .get(buffer_id)
        .map(|b| (b.point_emacs_byte_pos(), b.get_multibyte()))
        .unwrap_or((EmacsBytePos::ZERO, true));
    let change = text_change_for_empty_insertion_at_emacs_byte_pos(
        buffers,
        buffer_id,
        insert_pos,
        TextExtent::from_emacs_bytes(json.as_bytes(), multibyte),
    )?;
    Ok((change, multibyte))
}

/// `(json-insert VALUE &rest ARGS)` — insert JSON text at point.
///
/// Keyword arguments mirror `json-serialize` (`:null-object`, `:false-object`).
#[doc(hidden)]
pub fn builtin_json_insert<H: EditHost>(host: &mut H, args: Vec<Value>) -> EvalResult {
    expect_min_args("json-insert", &args, 1)?;
    let opts = parse_serialize_kwargs(&args, 1, host.symbol_mode())?;
    let json = serialize_to_json(&args[0], &opts, 0)?;
    let before_change = {
        let (edit, _vars) = host.edit_and_vars();
        let current_id = json_insert_target(&edit.buffers)?;
        json_insertion_at_point(&edit.buffers, current_id, json.as_str())?.0
    };
    signal_before_text_change(host, before_change)?;
    // GNU `Fjson_insert` works on the current buffer, point and multibyte flag
    // left by `prepare_to_modify_buffer`, so a hook may have changed any of
    // them. The serialized UTF-8 goes in unchanged: as characters in a
    // multibyte buffer and as raw bytes in a unibyte one.
    let change = {
        let (edit, _vars) = host.edit_and_vars();
        let current_id = json_insert_target(&edit.buffers)?;
        let (change, multibyte) =
            json_insertion_at_point(&edit.buffers, current_id, json.as_str())?;
        if multibyte {
            let _ = edit.buffers.insert_into_buffer(current_id, json.as_str());
        } else {
            let bytes = LispString::from_unibyte_slice(json.as_str().as_bytes());
            let _ = edit
                .buffers
                .insert_lisp_string_into_buffer(current_id, &bytes);
        }
        change
    };
    signal_after_text_change(host, change)?;
    Ok(Value::NIL)
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
#[path = "tests/json_test.rs"]
mod tests;
