//! A Latin1 or UTF-16 encoded, reference counted, immutable string.

// Required per unsafe code standards to ensure every unsafe usage is properly documented.
// - `unsafe_op_in_unsafe_fn` will be warn-by-default in edition 2024:
//   https://github.com/rust-lang/rust/issues/71668#issuecomment-1189396860
// - `undocumented_unsafe_blocks` and `missing_safety_doc` requires a `Safety:` section in the
//   comment or doc of the unsafe block or function, respectively.
#![deny(
    unsafe_op_in_unsafe_fn,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
// Remove when/if https://github.com/rust-lang/rust/issues/95228 stabilizes.
// Right now this allows us to use the stable polyfill from the `sptr` crate, which uses
// the same names from the unstable functions of the `std::ptr` module.
#![allow(unstable_name_collisions)]
#![allow(clippy::module_name_repetitions)]

mod common;
mod iter;
mod str;
mod tagged;

#[cfg(test)]
mod tests;

use self::{iter::Windows, str::JsSliceIndex};
use crate::tagged::{Tagged, UnwrappedTagged};
#[doc(inline)]
pub use crate::{
    common::StaticJsStrings,
    iter::Iter,
    str::{JsStr, JsStrVariant},
};
use std::{
    alloc::{alloc, dealloc, realloc, Layout},
    cell::Cell,
    convert::Infallible,
    hash::{Hash, Hasher},
    iter::Peekable,
    marker::PhantomData,
    process::abort,
    ptr::{self, addr_of, addr_of_mut, NonNull},
    str::FromStr,
};

fn alloc_overflow() -> ! {
    panic!("detected overflow during string allocation")
}

/// Helper function to check if a `char` is trimmable.
pub(crate) const fn is_trimmable_whitespace(c: char) -> bool {
    // The rust implementation of `trim` does not regard the same characters whitespace as ecma standard does
    //
    // Rust uses \p{White_Space} by default, which also includes:
    // `\u{0085}' (next line)
    // And does not include:
    // '\u{FEFF}' (zero width non-breaking space)
    // Explicit whitespace: https://tc39.es/ecma262/#sec-white-space
    matches!(
        c,
        '\u{0009}' | '\u{000B}' | '\u{000C}' | '\u{0020}' | '\u{00A0}' | '\u{FEFF}' |
    // Unicode Space_Separator category
    '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' |
    // Line terminators: https://tc39.es/ecma262/#sec-line-terminators
    '\u{000A}' | '\u{000D}' | '\u{2028}' | '\u{2029}'
    )
}

/// Helper function to check if a `u8` latin1 character is trimmable.
pub(crate) const fn is_trimmable_whitespace_latin1(c: u8) -> bool {
    // The rust implementation of `trim` does not regard the same characters whitespace as ecma standard does
    //
    // Rust uses \p{White_Space} by default, which also includes:
    // `\u{0085}' (next line)
    // And does not include:
    // '\u{FEFF}' (zero width non-breaking space)
    // Explicit whitespace: https://tc39.es/ecma262/#sec-white-space
    matches!(
        c,
        0x09 | 0x0B | 0x0C | 0x20 | 0xA0 |
        // Line terminators: https://tc39.es/ecma262/#sec-line-terminators
        0x0A | 0x0D
    )
}

/// Represents a Unicode codepoint within a [`JsString`], which could be a valid
/// '[Unicode scalar value]', or an unpaired surrogate.
///
/// [Unicode scalar value]: https://www.unicode.org/glossary/#unicode_scalar_value
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodePoint {
    /// A valid Unicode scalar value.
    Unicode(char),

    /// An unpaired surrogate.
    UnpairedSurrogate(u16),
}

impl CodePoint {
    /// Get the number of UTF-16 code units needed to encode this code point.
    #[inline]
    #[must_use]
    pub const fn code_unit_count(self) -> usize {
        match self {
            Self::Unicode(c) => c.len_utf16(),
            Self::UnpairedSurrogate(_) => 1,
        }
    }

    /// Convert the code point to its [`u32`] representation.
    #[inline]
    #[must_use]
    pub fn as_u32(self) -> u32 {
        match self {
            Self::Unicode(c) => u32::from(c),
            Self::UnpairedSurrogate(surr) => u32::from(surr),
        }
    }

    /// If the code point represents a valid 'Unicode scalar value', returns its [`char`]
    /// representation, otherwise returns [`None`] on unpaired surrogates.
    #[inline]
    #[must_use]
    pub const fn as_char(self) -> Option<char> {
        match self {
            Self::Unicode(c) => Some(c),
            Self::UnpairedSurrogate(_) => None,
        }
    }

    /// Encodes this code point as UTF-16 into the provided u16 buffer, and then returns the subslice
    /// of the buffer that contains the encoded character.
    ///
    /// # Panics
    ///
    /// Panics if the buffer is not large enough. A buffer of length 2 is large enough to encode any
    /// code point.
    #[inline]
    #[must_use]
    pub fn encode_utf16(self, dst: &mut [u16]) -> &mut [u16] {
        match self {
            Self::Unicode(c) => c.encode_utf16(dst),
            Self::UnpairedSurrogate(surr) => {
                dst[0] = surr;
                &mut dst[0..=0]
            }
        }
    }
}

/// The raw representation of a [`JsString`] in the heap.
#[repr(C)]
struct RawJsString {
    /// Contains the flags and Latin1/UTF-16 length.
    ///
    /// The latin1 flag is stored in the bottom bit.
    flags_and_len: usize,

    /// The number of references to the string.
    ///
    /// When this reaches `0` the string is deallocated.
    refcount: Cell<usize>,

    /// An empty array which is used to get the offset of string data.
    data: [u16; 0],
}

impl RawJsString {
    const LATIN1_BITFLAG: usize = 1 << 0;
    const BITFLAG_COUNT: usize = 1;

    const fn is_latin1(&self) -> bool {
        (self.flags_and_len & Self::LATIN1_BITFLAG) != 0
    }

    const fn len(&self) -> usize {
        self.flags_and_len >> Self::BITFLAG_COUNT
    }

    const fn encode_flags_and_len(len: usize, latin1: bool) -> usize {
        (len << Self::BITFLAG_COUNT) | (latin1 as usize)
    }
}

const DATA_OFFSET: usize = size_of::<RawJsString>();

/// A Latin1 or UTF-16–encoded, reference counted, immutable string.
///
/// This is pretty similar to a <code>[Rc][std::rc::Rc]\<[\[u16\]][slice]\></code>, but without the
/// length metadata associated with the `Rc` fat pointer. Instead, the length of every string is
/// stored on the heap, along with its reference counter and its data.
///
/// The string can be latin1 (stored as a byte for space efficiency) or U16 encoding.
///
/// We define some commonly used string constants in an interner. For these strings, we don't allocate
/// memory on the heap to reduce the overhead of memory allocation and reference counting.
#[allow(clippy::module_name_repetitions)]
pub struct JsString {
    ptr: Tagged<RawJsString>,
}

// JsString should always be pointer sized.
static_assertions::assert_eq_size!(JsString, *const ());

impl<'a> From<&'a JsString> for JsStr<'a> {
    #[inline]
    fn from(value: &'a JsString) -> Self {
        value.as_str()
    }
}

impl<'a> IntoIterator for &'a JsString {
    type IntoIter = Iter<'a>;
    type Item = u16;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl JsString {
    /// Create an iterator over the [`JsString`].
    #[inline]
    #[must_use]
    pub fn iter(&self) -> Iter<'_> {
        Iter::new(self.as_str())
    }

    /// Create an iterator over overlapping subslices of length size.
    #[inline]
    #[must_use]
    pub fn windows(&self, size: usize) -> Windows<'_> {
        Windows::new(self.as_str(), size)
    }

    /// Obtains the underlying [`&[u16]`][slice] slice of a [`JsString`]
    #[inline]
    #[must_use]
    pub fn as_str(&self) -> JsStr<'_> {
        match self.ptr.unwrap() {
            UnwrappedTagged::Ptr(h) => {
                // SAFETY:
                // - The `RawJsString` type has all the necessary information to reconstruct a valid
                //   slice (length and starting pointer).
                //
                // - We aligned `h.data` on allocation, and the block is of size `h.len`, so this
                //   should only generate valid reads.
                //
                // - The lifetime of `&Self::Target` is shorter than the lifetime of `self`, as seen
                //   by its signature, so this doesn't outlive `self`.
                unsafe {
                    let h = h.as_ptr();

                    if (*h).is_latin1() {
                        JsStr::latin1(std::slice::from_raw_parts(
                            addr_of!((*h).data).cast(),
                            (*h).len(),
                        ))
                    } else {
                        JsStr::utf16(std::slice::from_raw_parts(
                            addr_of!((*h).data).cast(),
                            (*h).len(),
                        ))
                    }
                }
            }
            UnwrappedTagged::Tag(index) => {
                // SAFETY: all static strings are valid indices on `STATIC_JS_STRINGS`, so `get` should always
                // return `Some`.
                unsafe { StaticJsStrings::get(index).unwrap_unchecked() }
            }
        }
    }

    /// Creates a new [`JsString`] from the concatenation of `x` and `y`.
    #[inline]
    #[must_use]
    pub fn concat(x: JsStr<'_>, y: JsStr<'_>) -> Self {
        Self::concat_array(&[x, y])
    }

    /// Creates a new [`JsString`] from the concatenation of every element of
    /// `strings`.
    #[inline]
    #[must_use]
    pub fn concat_array(strings: &[JsStr<'_>]) -> Self {
        let mut latin1_encoding = true;
        let mut full_count = 0usize;
        for string in strings {
            let Some(sum) = full_count.checked_add(string.len()) else {
                alloc_overflow()
            };
            if !string.is_latin1() {
                latin1_encoding = false;
            }
            full_count = sum;
        }

        let ptr = Self::allocate_inner(full_count, latin1_encoding);

        let string = {
            // SAFETY: `allocate_inner` guarantees that `ptr` is a valid pointer.
            let mut data = unsafe { addr_of_mut!((*ptr.as_ptr()).data).cast::<u8>() };
            for &string in strings {
                // SAFETY:
                // The sum of all `count` for each `string` equals `full_count`, and since we're
                // iteratively writing each of them to `data`, `copy_non_overlapping` always stays
                // in-bounds for `count` reads of each string and `full_count` writes to `data`.
                //
                // Each `string` must be properly aligned to be a valid slice, and `data` must be
                // properly aligned by `allocate_inner`.
                //
                // `allocate_inner` must return a valid pointer to newly allocated memory, meaning
                // `ptr` and all `string`s should never overlap.
                unsafe {
                    // NOTE: The aligment is checked when we allocate the array.
                    #[allow(clippy::cast_ptr_alignment)]
                    match (latin1_encoding, string.variant()) {
                        (true, JsStrVariant::Latin1(s)) => {
                            let count = s.len();
                            ptr::copy_nonoverlapping(s.as_ptr(), data.cast::<u8>(), count);
                            data = data.cast::<u8>().add(count).cast::<u8>();
                        }
                        (false, JsStrVariant::Latin1(s)) => {
                            let count = s.len();
                            for (i, byte) in s.iter().enumerate() {
                                *data.cast::<u16>().add(i) = u16::from(*byte);
                            }
                            data = data.cast::<u16>().add(count).cast::<u8>();
                        }
                        (false, JsStrVariant::Utf16(s)) => {
                            let count = s.len();
                            ptr::copy_nonoverlapping(s.as_ptr(), data.cast::<u16>(), count);
                            data = data.cast::<u16>().add(count).cast::<u8>();
                        }
                        (true, JsStrVariant::Utf16(_)) => {
                            unreachable!("Already checked that it's latin1 encoding")
                        }
                    }
                }
            }
            Self {
                // SAFETY: We already know it's a valid heap pointer.
                ptr: unsafe { Tagged::from_ptr(ptr.as_ptr()) },
            }
        };

        StaticJsStrings::get_string(&string.as_str()).unwrap_or(string)
    }

    /// Decodes a [`JsString`] into a [`String`], replacing invalid data with its escaped representation
    /// in 4 digit hexadecimal.
    #[inline]
    #[must_use]
    pub fn to_std_string_escaped(&self) -> String {
        self.to_string_escaped()
    }

    /// Decodes a [`JsString`] into a [`String`], returning
    ///
    /// # Errors
    ///
    /// [`FromUtf16Error`][std::string::FromUtf16Error] if it contains any invalid data.
    #[inline]
    pub fn to_std_string(&self) -> Result<String, std::string::FromUtf16Error> {
        match self.as_str().variant() {
            JsStrVariant::Latin1(v) => Ok(v.iter().copied().map(char::from).collect()),
            JsStrVariant::Utf16(v) => String::from_utf16(v),
        }
    }

    /// Decodes a [`JsString`] into an iterator of [`Result<String, u16>`], returning surrogates as
    /// errors.
    #[inline]
    pub fn to_std_string_with_surrogates(&self) -> impl Iterator<Item = Result<String, u16>> + '_ {
        struct WideStringDecoderIterator<I: Iterator> {
            codepoints: Peekable<I>,
        }

        impl<I: Iterator> WideStringDecoderIterator<I> {
            fn new(iterator: I) -> Self {
                Self {
                    codepoints: iterator.peekable(),
                }
            }
        }

        impl<I> Iterator for WideStringDecoderIterator<I>
        where
            I: Iterator<Item = CodePoint>,
        {
            type Item = Result<String, u16>;

            fn next(&mut self) -> Option<Self::Item> {
                let cp = self.codepoints.next()?;
                let char = match cp {
                    CodePoint::Unicode(c) => c,
                    CodePoint::UnpairedSurrogate(surr) => return Some(Err(surr)),
                };

                let mut string = String::from(char);

                loop {
                    let Some(cp) = self.codepoints.peek().and_then(|cp| match cp {
                        CodePoint::Unicode(c) => Some(*c),
                        CodePoint::UnpairedSurrogate(_) => None,
                    }) else {
                        break;
                    };

                    string.push(cp);

                    self.codepoints
                        .next()
                        .expect("should exist by the check above");
                }

                Some(Ok(string))
            }
        }

        WideStringDecoderIterator::new(self.code_points())
    }

    /// Maps the valid segments of an UTF16 string and leaves the unpaired surrogates unchanged.
    #[inline]
    #[must_use]
    pub fn map_valid_segments<F>(&self, mut f: F) -> Self
    where
        F: FnMut(String) -> String,
    {
        let mut text = Vec::new();

        for part in self.to_std_string_with_surrogates() {
            match part {
                Ok(string) => text.extend(f(string).encode_utf16()),
                Err(surr) => text.push(surr),
            }
        }

        Self::from(&text[..])
    }

    /// Gets an iterator of all the Unicode codepoints of a [`JsString`].
    #[inline]
    pub fn code_points(&self) -> impl Iterator<Item = CodePoint> + Clone + '_ {
        char::decode_utf16(self.iter()).map(|res| match res {
            Ok(c) => CodePoint::Unicode(c),
            Err(e) => CodePoint::UnpairedSurrogate(e.unpaired_surrogate()),
        })
    }

    /// Abstract operation `StringIndexOf ( string, searchValue, fromIndex )`
    ///
    /// Note: Instead of returning an isize with `-1` as the "not found" value, we make use of the
    /// type system and return <code>[Option]\<usize\></code> with [`None`] as the "not found" value.
    ///
    /// More information:
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-stringindexof
    #[inline]
    #[must_use]
    pub fn index_of(&self, search_value: JsStr<'_>, from_index: usize) -> Option<usize> {
        // 1. Assert: Type(string) is String.
        // 2. Assert: Type(searchValue) is String.
        // 3. Assert: fromIndex is a non-negative integer.

        // 4. Let len be the length of string.
        let len = self.len();

        // 5. If searchValue is the empty String and fromIndex ≤ len, return fromIndex.
        if search_value.is_empty() {
            return if from_index <= len {
                Some(from_index)
            } else {
                None
            };
        }

        // 6. Let searchLen be the length of searchValue.
        // 7. For each integer i starting with fromIndex such that i ≤ len - searchLen, in ascending order, do
        // a. Let candidate be the substring of string from i to i + searchLen.
        // b. If candidate is the same sequence of code units as searchValue, return i.
        // 8. Return -1.
        self.windows(search_value.len())
            .skip(from_index)
            .position(|s| s == search_value)
            .map(|i| i + from_index)
    }

    /// Abstract operation `CodePointAt( string, position )`.
    ///
    /// The abstract operation `CodePointAt` takes arguments `string` (a String) and `position` (a
    /// non-negative integer) and returns a Record with fields `[[CodePoint]]` (a code point),
    /// `[[CodeUnitCount]]` (a positive integer), and `[[IsUnpairedSurrogate]]` (a Boolean). It
    /// interprets string as a sequence of UTF-16 encoded code points, as described in 6.1.4, and reads
    /// from it a single code point starting with the code unit at index `position`.
    ///
    /// More information:
    ///  - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-codepointat
    ///
    /// # Panics
    ///
    /// If `position` is smaller than size of string.
    #[inline]
    #[must_use]
    pub fn code_point_at(&self, position: usize) -> CodePoint {
        // 1. Let size be the length of string.
        let size = self.len();

        // 2. Assert: position ≥ 0 and position < size.
        // position >= 0 ensured by position: usize
        assert!(position < size);

        match self.as_str().variant() {
            JsStrVariant::Latin1(v) => {
                let code_point = v.get(position).expect("Already checked the size");
                CodePoint::Unicode(*code_point as char)
            }
            // 3. Let first be the code unit at index position within string.
            // 4. Let cp be the code point whose numeric value is that of first.
            // 5. If first is not a leading surrogate or trailing surrogate, then
            // a. Return the Record { [[CodePoint]]: cp, [[CodeUnitCount]]: 1, [[IsUnpairedSurrogate]]: false }.
            // 6. If first is a trailing surrogate or position + 1 = size, then
            // a. Return the Record { [[CodePoint]]: cp, [[CodeUnitCount]]: 1, [[IsUnpairedSurrogate]]: true }.
            // 7. Let second be the code unit at index position + 1 within string.
            // 8. If second is not a trailing surrogate, then
            // a. Return the Record { [[CodePoint]]: cp, [[CodeUnitCount]]: 1, [[IsUnpairedSurrogate]]: true }.
            // 9. Set cp to ! UTF16SurrogatePairToCodePoint(first, second).
            JsStrVariant::Utf16(v) => {
                // We can skip the checks and instead use the `char::decode_utf16` function to take care of that for us.
                let code_point = v
                    .get(position..=position + 1)
                    .unwrap_or(&v[position..=position]);

                match char::decode_utf16(code_point.iter().copied())
                    .next()
                    .expect("code_point always has a value")
                {
                    Ok(c) => CodePoint::Unicode(c),
                    Err(e) => CodePoint::UnpairedSurrogate(e.unpaired_surrogate()),
                }
            }
        }
    }

    /// Abstract operation `StringToNumber ( str )`
    ///
    /// More information:
    /// - [ECMAScript reference][spec]
    ///
    /// [spec]: https://tc39.es/ecma262/#sec-stringtonumber
    #[inline]
    #[must_use]
    pub fn to_number(&self) -> f64 {
        // 1. Let text be ! StringToCodePoints(str).
        // 2. Let literal be ParseText(text, StringNumericLiteral).
        let Ok(string) = self.to_std_string() else {
            // 3. If literal is a List of errors, return NaN.
            return f64::NAN;
        };
        // 4. Return StringNumericValue of literal.
        let string = string.trim_matches(is_trimmable_whitespace);
        match string {
            "" => return 0.0,
            "-Infinity" => return f64::NEG_INFINITY,
            "Infinity" | "+Infinity" => return f64::INFINITY,
            _ => {}
        }

        let mut s = string.bytes();
        let base = match (s.next(), s.next()) {
            (Some(b'0'), Some(b'b' | b'B')) => Some(2),
            (Some(b'0'), Some(b'o' | b'O')) => Some(8),
            (Some(b'0'), Some(b'x' | b'X')) => Some(16),
            // Make sure that no further variants of "infinity" are parsed.
            (Some(b'i' | b'I'), _) => {
                return f64::NAN;
            }
            _ => None,
        };

        // Parse numbers that begin with `0b`, `0o` and `0x`.
        if let Some(base) = base {
            let string = &string[2..];
            if string.is_empty() {
                return f64::NAN;
            }

            // Fast path
            if let Ok(value) = u32::from_str_radix(string, base) {
                return f64::from(value);
            }

            // Slow path
            let mut value: f64 = 0.0;
            for c in s {
                if let Some(digit) = char::from(c).to_digit(base) {
                    value = value.mul_add(f64::from(base), f64::from(digit));
                } else {
                    return f64::NAN;
                }
            }
            return value;
        }

        fast_float::parse(string).unwrap_or(f64::NAN)
    }

    /// Allocates a new [`RawJsString`] with an internal capacity of `str_len` chars.
    ///
    /// # Panics
    ///
    /// Panics if `try_allocate_inner` returns `Err`.
    fn allocate_inner(str_len: usize, latin1: bool) -> NonNull<RawJsString> {
        match Self::try_allocate_inner(str_len, latin1) {
            Ok(v) => v,
            Err(None) => alloc_overflow(),
            Err(Some(layout)) => std::alloc::handle_alloc_error(layout),
        }
    }

    // This is marked as safe because it is always valid to call this function to request any number
    // of `u16`, since this function ought to fail on an OOM error.
    /// Allocates a new [`RawJsString`] with an internal capacity of `str_len` chars.
    ///
    /// # Errors
    ///
    /// Returns `Err(None)` on integer overflows `usize::MAX`.
    /// Returns `Err(Some(Layout))` on allocation error.
    fn try_allocate_inner(
        str_len: usize,
        latin1: bool,
    ) -> Result<NonNull<RawJsString>, Option<Layout>> {
        let (layout, offset) = if latin1 {
            Layout::array::<u8>(str_len)
        } else {
            Layout::array::<u16>(str_len)
        }
        .and_then(|arr| Layout::new::<RawJsString>().extend(arr))
        .map(|(layout, offset)| (layout.pad_to_align(), offset))
        .map_err(|_| None)?;

        debug_assert_eq!(offset, DATA_OFFSET);

        #[allow(clippy::cast_ptr_alignment)]
        // SAFETY:
        // The layout size of `RawJsString` is never zero, since it has to store
        // the length of the string and the reference count.
        let inner = unsafe { alloc(layout).cast::<RawJsString>() };

        // We need to verify that the pointer returned by `alloc` is not null, otherwise
        // we should abort, since an allocation error is pretty unrecoverable for us
        // right now.
        let inner = NonNull::new(inner).ok_or(Some(layout))?;

        // SAFETY:
        // `NonNull` verified for us that the pointer returned by `alloc` is valid,
        // meaning we can write to its pointed memory.
        unsafe {
            // Write the first part, the `RawJsString`.
            inner.as_ptr().write(RawJsString {
                flags_and_len: RawJsString::encode_flags_and_len(str_len, latin1),
                refcount: Cell::new(1),
                data: [0; 0],
            });
        }

        debug_assert!({
            let inner = inner.as_ptr();
            // SAFETY:
            // - `inner` must be a valid pointer, since it comes from a `NonNull`,
            // meaning we can safely dereference it to `RawJsString`.
            // - `offset` should point us to the beginning of the array,
            // and since we requested an `RawJsString` layout with a trailing
            // `[u16; str_len]`, the memory of the array must be in the `usize`
            // range for the allocation to succeed.
            unsafe {
                ptr::eq(
                    inner.cast::<u8>().add(offset).cast(),
                    (*inner).data.as_mut_ptr(),
                )
            }
        });

        Ok(inner)
    }

    /// Creates a new [`JsString`] from `data`, without checking if the string is in the interner.
    fn from_slice_skip_interning(string: JsStr<'_>) -> Self {
        let count = string.len();
        let ptr = Self::allocate_inner(count, string.is_latin1());

        // SAFETY: `allocate_inner` guarantees that `ptr` is a valid pointer.
        let data = unsafe { addr_of_mut!((*ptr.as_ptr()).data).cast::<u8>() };

        // SAFETY:
        // - We read `count = data.len()` elements from `data`, which is within the bounds of the slice.
        // - `allocate_inner` must allocate at least `count` elements, which allows us to safely
        //   write at least `count` elements.
        // - `allocate_inner` should already take care of the alignment of `ptr`, and `data` must be
        //   aligned to be a valid slice.
        // - `allocate_inner` must return a valid pointer to newly allocated memory, meaning `ptr`
        //   and `data` should never overlap.
        unsafe {
            // NOTE: The aligment is checked when we allocate the array.
            #[allow(clippy::cast_ptr_alignment)]
            match string.variant() {
                JsStrVariant::Latin1(s) => {
                    ptr::copy_nonoverlapping(s.as_ptr(), data.cast::<u8>(), count);
                }
                JsStrVariant::Utf16(s) => {
                    ptr::copy_nonoverlapping(s.as_ptr(), data.cast::<u16>(), count);
                }
            }
        }
        Self {
            // SAFETY: `allocate_inner` guarantees `ptr` is a valid heap pointer.
            ptr: Tagged::from_non_null(ptr),
        }
    }

    /// Creates a new [`JsString`] from `data`.
    fn from_slice(string: JsStr<'_>) -> Self {
        if let Some(s) = StaticJsStrings::get_string(&string) {
            return s;
        }
        Self::from_slice_skip_interning(string)
    }

    /// Get the length of the [`JsString`].
    #[inline]
    #[must_use]
    pub fn len(&self) -> usize {
        match self.ptr.unwrap() {
            UnwrappedTagged::Ptr(h) => {
                // SAFETY:
                // - The `RawJsString` type has all the necessary information to reconstruct a valid
                //   slice (length and starting pointer).
                //
                // - We aligned `h.data` on allocation, and the block is of size `h.len`, so this
                //   should only generate valid reads.
                //
                // - The lifetime of `&Self::Target` is shorter than the lifetime of `self`, as seen
                //   by its signature, so this doesn't outlive `self`.
                unsafe {
                    let h = h.as_ptr();
                    (*h).len()
                }
            }
            UnwrappedTagged::Tag(index) => {
                // SAFETY: all static strings are valid indices on `STATIC_JS_STRINGS`, so `get` should always
                // return `Some`.
                unsafe { StaticJsStrings::get(index).unwrap_unchecked().len() }
            }
        }
    }

    /// Return true if the [`JsString`] is emtpy.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Convert the [`JsString`] into a [`Vec<U16>`].
    #[inline]
    #[must_use]
    pub fn to_vec(&self) -> Vec<u16> {
        self.as_str().to_vec()
    }

    /// Check if the [`JsString`] contains a byte.
    #[inline]
    #[must_use]
    pub fn contains(&self, element: u8) -> bool {
        match self.as_str().variant() {
            JsStrVariant::Latin1(v) => v.contains(&element),
            JsStrVariant::Utf16(v) => v.contains(&u16::from(element)),
        }
    }

    /// Trim whitespace from the start and end of the [`JsString`].
    #[inline]
    #[must_use]
    pub fn trim(&self) -> JsStr<'_> {
        self.as_str().trim()
    }

    /// Trim whitespace from the start of the [`JsString`].
    #[inline]
    #[must_use]
    pub fn trim_start(&self) -> JsStr<'_> {
        self.as_str().trim_start()
    }

    /// Trim whitespace from the end of the [`JsString`].
    #[inline]
    #[must_use]
    pub fn trim_end(&self) -> JsStr<'_> {
        self.as_str().trim_end()
    }

    /// Check if the [`JsString`] is static.
    #[inline]
    #[must_use]
    pub fn is_static(&self) -> bool {
        self.ptr.is_tagged()
    }

    /// Get the element a the given index, [`None`] otherwise.
    #[inline]
    #[must_use]
    pub fn get<'a, I>(&'a self, index: I) -> Option<I::Value>
    where
        I: JsSliceIndex<'a>,
    {
        I::get(self.as_str(), index)
    }

    /// Returns an element or subslice depending on the type of index, without doing bounds check.
    ///
    /// # Safety
    ///
    /// Caller must ensure the index is not out of bounds
    #[inline]
    #[must_use]
    pub unsafe fn get_unchecked<'a, I>(&'a self, index: I) -> I::Value
    where
        I: JsSliceIndex<'a>,
    {
        // SAFETY: Caller must ensure the index is not out of bounds
        unsafe { I::get_unchecked(self.as_str(), index) }
    }

    /// Get the element a the given index.
    ///
    /// # Panics
    ///
    /// If the index is out of bounds.
    #[inline]
    #[must_use]
    pub fn get_expect<'a, I>(&'a self, index: I) -> I::Value
    where
        I: JsSliceIndex<'a>,
    {
        self.get(index).expect("Index out of bounds")
    }

    /// Gets the number of `JsString`s which point to this allocation.
    #[inline]
    #[must_use]
    pub fn refcount(&self) -> Option<usize> {
        match self.ptr.unwrap() {
            UnwrappedTagged::Ptr(inner) => {
                // SAFETY: The reference count of `JsString` guarantees that `inner` is always valid.
                let inner = unsafe { inner.as_ref() };
                Some(inner.refcount.get())
            }
            UnwrappedTagged::Tag(_inner) => None,
        }
    }
}

impl Clone for JsString {
    #[inline]
    fn clone(&self) -> Self {
        if let UnwrappedTagged::Ptr(inner) = self.ptr.unwrap() {
            // SAFETY: The reference count of `JsString` guarantees that `raw` is always valid.
            let inner = unsafe { inner.as_ref() };
            let strong = inner.refcount.get().wrapping_add(1);
            if strong == 0 {
                abort()
            }
            inner.refcount.set(strong);
        }
        Self { ptr: self.ptr }
    }
}

impl Default for JsString {
    #[inline]
    fn default() -> Self {
        StaticJsStrings::EMPTY_STRING
    }
}

impl Drop for JsString {
    #[inline]
    fn drop(&mut self) {
        if let UnwrappedTagged::Ptr(raw) = self.ptr.unwrap() {
            // See https://doc.rust-lang.org/src/alloc/sync.rs.html#1672 for details.

            // SAFETY: The reference count of `JsString` guarantees that `raw` is always valid.
            let inner = unsafe { raw.as_ref() };
            inner.refcount.set(inner.refcount.get() - 1);
            if inner.refcount.get() != 0 {
                return;
            }

            // SAFETY:
            // All the checks for the validity of the layout have already been made on `alloc_inner`,
            // so we can skip the unwrap.
            let layout = unsafe {
                if inner.is_latin1() {
                    Layout::for_value(inner)
                        .extend(Layout::array::<u8>(inner.len()).unwrap_unchecked())
                        .unwrap_unchecked()
                        .0
                        .pad_to_align()
                } else {
                    Layout::for_value(inner)
                        .extend(Layout::array::<u16>(inner.len()).unwrap_unchecked())
                        .unwrap_unchecked()
                        .0
                        .pad_to_align()
                }
            };

            // SAFETY:
            // If refcount is 0 and we call drop, that means this is the last `JsString` which
            // points to this memory allocation, so deallocating it is safe.
            unsafe {
                dealloc(raw.as_ptr().cast(), layout);
            }
        }
    }
}

impl ToStringEscaped for JsString {
    #[inline]
    fn to_string_escaped(&self) -> String {
        match self.as_str().variant() {
            JsStrVariant::Latin1(v) => v.iter().copied().map(char::from).collect(),
            JsStrVariant::Utf16(v) => v.to_string_escaped(),
        }
    }
}

impl std::fmt::Debug for JsString {
    #[inline]
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.to_std_string_escaped().fmt(f)
    }
}

impl Eq for JsString {}

impl From<&[u16]> for JsString {
    #[inline]
    fn from(s: &[u16]) -> Self {
        JsString::from_slice(JsStr::utf16(s))
    }
}

impl From<&str> for JsString {
    #[inline]
    fn from(s: &str) -> Self {
        // TODO: Check for latin1 encoding
        if s.is_ascii() {
            let js_str = JsStr::latin1(s.as_bytes());
            return StaticJsStrings::get_string(&js_str)
                .unwrap_or_else(|| JsString::from_slice_skip_interning(js_str));
        }
        let s = s.encode_utf16().collect::<Vec<_>>();
        JsString::from_slice_skip_interning(JsStr::utf16(&s[..]))
    }
}

impl From<JsStr<'_>> for JsString {
    #[inline]
    fn from(value: JsStr<'_>) -> Self {
        StaticJsStrings::get_string(&value)
            .unwrap_or_else(|| JsString::from_slice_skip_interning(value))
    }
}

impl From<&[JsString]> for JsString {
    #[inline]
    fn from(value: &[JsString]) -> Self {
        Self::concat_array(
            &value
                .iter()
                .map(Self::as_str)
                .map(Into::into)
                .collect::<Vec<_>>()[..],
        )
    }
}
impl From<String> for JsString {
    #[inline]
    fn from(s: String) -> Self {
        Self::from(s.as_str())
    }
}

impl<const N: usize> From<&[u16; N]> for JsString {
    #[inline]
    fn from(s: &[u16; N]) -> Self {
        Self::from(&s[..])
    }
}

impl Hash for JsString {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl PartialOrd for JsStr<'_> {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for JsString {
    #[inline]
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(&other.as_str())
    }
}

impl PartialEq for JsString {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<JsString> for [u16] {
    #[inline]
    fn eq(&self, other: &JsString) -> bool {
        if self.len() != other.len() {
            return false;
        }
        for (x, y) in self.iter().copied().zip(other.iter()) {
            if x != y {
                return false;
            }
        }
        true
    }
}

impl<const N: usize> PartialEq<JsString> for [u16; N] {
    #[inline]
    fn eq(&self, other: &JsString) -> bool {
        self[..] == *other
    }
}

impl PartialEq<[u16]> for JsString {
    #[inline]
    fn eq(&self, other: &[u16]) -> bool {
        other == self
    }
}

impl<const N: usize> PartialEq<[u16; N]> for JsString {
    #[inline]
    fn eq(&self, other: &[u16; N]) -> bool {
        *self == other[..]
    }
}

impl PartialEq<str> for JsString {
    #[inline]
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for JsString {
    #[inline]
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<JsString> for str {
    #[inline]
    fn eq(&self, other: &JsString) -> bool {
        other == self
    }
}

impl PartialEq<JsStr<'_>> for JsString {
    #[inline]
    fn eq(&self, other: &JsStr<'_>) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<JsString> for JsStr<'_> {
    #[inline]
    fn eq(&self, other: &JsString) -> bool {
        other == self
    }
}

impl PartialOrd for JsString {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl FromStr for JsString {
    type Err = Infallible;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from(s))
    }
}

/// Utility trait that adds a `UTF-16` escaped representation to every [`[u16]`][slice].
pub(crate) trait ToStringEscaped {
    /// Decodes `self` as an `UTF-16` encoded string, escaping any unpaired surrogates by its
    /// codepoint value.
    fn to_string_escaped(&self) -> String;
}

impl ToStringEscaped for [u16] {
    #[inline]
    fn to_string_escaped(&self) -> String {
        char::decode_utf16(self.iter().copied())
            .map(|r| match r {
                Ok(c) => String::from(c),
                Err(e) => format!("\\u{:04X}", e.unpaired_surrogate()),
            })
            .collect()
    }
}

#[doc(hidden)]
pub mod private {
    /// Inner elements represented for `JsStringBuilder`.
    pub trait JsStringData {}

    impl JsStringData for u8 {}
    impl JsStringData for u16 {}
}

/// A mutable builder to create instance of `JsString`.
///
/// # Examples
///
/// ```rust
/// use boa_string::JsStringBuilder;
/// let mut s = JsStringBuilder::new();
/// s.push(b'x');
/// s.extend_from_slice(&[b'1', b'2', b'3']);
/// s.extend([b'1', b'2', b'3']);
/// let js_string = s.build();
/// ```
#[derive(Debug)]
pub struct JsStringBuilder<T: private::JsStringData> {
    cap: usize,
    len: usize,
    inner: NonNull<RawJsString>,
    phantom_data: PhantomData<T>,
}

impl<D: private::JsStringData> Clone for JsStringBuilder<D> {
    #[must_use]
    fn clone(&self) -> Self {
        let mut builder = Self::with_capacity(self.capacity());
        // SAFETY:
        // - `inner` must be a valid pointer, since it comes from a `NonNull`
        // allocated above with the capacity of `s`, and initialize to `s.len()` in
        // ptr::copy_to_non_overlapping below.
        unsafe {
            builder
                .inner
                .as_ptr()
                .cast::<u8>()
                .copy_from_nonoverlapping(self.inner.as_ptr().cast(), self.allocated_byte_len());

            builder.set_len(self.len());
        }
        builder
    }
}

impl<D: private::JsStringData> Default for JsStringBuilder<D> {
    fn default() -> Self {
        Self::new()
    }
}

impl<D: private::JsStringData> JsStringBuilder<D> {
    const DATA_SIZE: usize = size_of::<D>();
    const MIN_NON_ZERO_CAP: usize = 8 / Self::DATA_SIZE;

    /// Create a new `JsStringBuilder` with capacity of zero.
    #[inline]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            cap: 0,
            len: 0,
            inner: NonNull::dangling(),
            phantom_data: PhantomData,
        }
    }

    /// Returns the number of elements that inner holds.
    #[inline]
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Forces the length of the [`JsStringBuilder`] to `new_len`.
    ///
    /// # Safety
    ///
    /// - `new_len` must be less than or equal to `capacity()`.
    /// - The elements at `old_len..new_len` must be initialized.
    ///
    #[inline]
    pub unsafe fn set_len(&mut self, new_len: usize) {
        debug_assert!(new_len <= self.capacity());

        self.len = new_len;
    }

    /// Returns the total number of elements can hold without reallocating
    #[inline]
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.cap
    }

    /// Returns the allocated byte of inner.
    #[must_use]
    const fn allocated_byte_len(&self) -> usize {
        DATA_OFFSET + self.allocated_data_byte_len()
    }

    /// Returns the allocated byte of inner's data.
    #[must_use]
    const fn allocated_data_byte_len(&self) -> usize {
        self.len() * Self::DATA_SIZE
    }

    /// Returns the capacity calculted from given layout.
    #[must_use]
    const fn capacity_from_layout(layout: Layout) -> usize {
        (layout.size() - DATA_OFFSET) / Self::DATA_SIZE
    }

    /// create a new `JsStringBuilder` with specific capacity
    #[must_use]
    pub fn with_capacity(cap: usize) -> Self {
        let layout = Self::new_layout(cap);
        #[allow(clippy::cast_ptr_alignment)]
        // SAFETY:
        // The layout size of `RawJsString` is never zero, since it has to store
        // the length of the string and the reference count.
        let ptr = unsafe { alloc(layout) };

        let Some(ptr) = NonNull::new(ptr.cast()) else {
            std::alloc::handle_alloc_error(layout)
        };
        Self {
            cap: Self::capacity_from_layout(layout),
            len: 0,
            inner: ptr,
            phantom_data: PhantomData,
        }
    }

    /// Checks if the inner is allocated.
    #[must_use]
    fn is_dangling(&self) -> bool {
        self.inner == NonNull::dangling()
    }

    /// Returns the inner's layout.
    #[must_use]
    fn current_layout(&self) -> Layout {
        // SAFETY:
        // All the checks for the validity of the layout have already been made on `new_layout`,
        // so we can skip the unwrap.
        unsafe {
            Layout::for_value(self.inner.as_ref())
                .extend(Layout::array::<D>(self.capacity()).unwrap_unchecked())
                .unwrap_unchecked()
                .0
                .pad_to_align()
        }
    }

    /// Returns the pointer of `data` of inner.
    ///
    /// # Safety
    ///
    /// Caller should ensure that the inner is allocated.
    #[must_use]
    unsafe fn data(&self) -> *mut D {
        // SAFETY:
        // Caller should ensure that the inner is allocated.
        unsafe { addr_of_mut!((*self.inner.as_ptr()).data).cast() }
    }

    /// Inner logic of `allocate`.
    ///
    /// Use `realloc` here because it has a better performance than using `alloc`, `copy` and `dealloc`.
    #[allow(clippy::cast_ptr_alignment)]
    fn allocate_inner(&mut self, new_layout: Layout) {
        let new_ptr = if self.is_dangling() {
            // SAFETY:
            // The layout size of `RawJsString` is never zero, since it has to store
            // the length of the string and the reference count.
            unsafe { alloc(new_layout) }
        } else {
            let old_ptr = self.inner.as_ptr();
            let old_layout = self.current_layout();
            // SAFETY:
            // The layout size of `RawJsString` is never zero, since it has to store
            // the length of the string and the reference count.
            unsafe { realloc(old_ptr.cast(), old_layout, new_layout.size()) }
        };
        let Some(new_ptr) = NonNull::new(new_ptr.cast::<RawJsString>()) else {
            std::alloc::handle_alloc_error(new_layout)
        };
        self.inner = new_ptr;
        self.cap = Self::capacity_from_layout(new_layout);
    }

    /// Appends an element to the inner of `JsStringBuilder`.
    pub fn push(&mut self, v: D) {
        let len = self.len();
        if len == self.capacity() {
            self.allocate(len + 1);
        }
        // SAFETY:
        // Capacity has been expanded to be large enough to hold elements.
        unsafe {
            self.push_unchecked(v);
        }
    }

    /// Push elements from slice to `JsStringBuilder` without doing capacity check.
    ///
    /// Unlike the standard vector, our holded element types are only `u8` and `u16`, which is [`Copy`] derived,
    ///
    /// so we only need to copy them instead of cloning.
    ///
    /// # Safety
    ///
    /// Caller should ensure the capacity is large enough to hold elements.
    pub unsafe fn extend_from_slice_unchecked(&mut self, v: &[D]) {
        // SAFETY: Caller should ensure the capacity is large enough to hold elements.
        unsafe {
            ptr::copy_nonoverlapping(v.as_ptr(), self.data().add(self.len()), v.len());
        }
        self.len += v.len();
    }

    /// push elements from slice to `JsStringBuilder`.
    pub fn extend_from_slice(&mut self, v: &[D]) {
        let required_cap = self.len() + v.len();
        if required_cap > self.capacity() {
            self.allocate(required_cap);
        }
        // SAFETY:
        // Capacity has been expanded to be large enough to hold elements.
        unsafe {
            self.extend_from_slice_unchecked(v);
        }
    }

    fn new_layout(cap: usize) -> Layout {
        let new_layout = Layout::array::<D>(cap)
            .and_then(|arr| Layout::new::<RawJsString>().extend(arr))
            .map(|(layout, offset)| (layout.pad_to_align(), offset))
            .map_err(|_| None);
        match new_layout {
            Ok((new_layout, offset)) => {
                debug_assert_eq!(offset, DATA_OFFSET);
                new_layout
            }
            Err(None) => alloc_overflow(),
            Err(Some(layout)) => std::alloc::handle_alloc_error(layout),
        }
    }

    /// Extends `JsStringBuilder` with the contents of an iterator.
    pub fn extend<I: IntoIterator<Item = D>>(&mut self, iter: I) {
        let iterator = iter.into_iter();
        let (lower_bound, _) = iterator.size_hint();
        let require_cap = self.len() + lower_bound;
        if require_cap > self.capacity() {
            self.allocate(require_cap);
        }
        iterator.for_each(|c| self.push(c));
    }

    /// Reserves capacity for at least `additional` more elements to be inserted
    /// in the given `JsStringBuilder<D>`. The collection may reserve more space to
    /// speculatively avoid frequent reallocations. After calling `reserve`,
    /// capacity will be greater than or equal to `self.len() + additional`.
    /// Does nothing if capacity is already sufficient.
    pub fn reserve(&mut self, additional: usize) {
        if additional > self.capacity().wrapping_sub(self.len) {
            let Some(cap) = self.len().checked_add(additional) else {
                alloc_overflow()
            };
            self.allocate(cap);
        }
    }

    /// Allocates memory to the inner by the given capacity.
    /// Capacity calculation is from [`std::vec::Vec::reserve`].
    fn allocate(&mut self, cap: usize) {
        let cap = std::cmp::max(self.capacity() * 2, cap);
        let cap = std::cmp::max(Self::MIN_NON_ZERO_CAP, cap);
        self.allocate_inner(Self::new_layout(cap));
    }

    /// Appends an element to the inner of `JsStringBuilder` without doing bounds check.
    /// # Safety
    ///
    /// Caller should ensure the capacity is large enough to hold elements.
    pub unsafe fn push_unchecked(&mut self, v: D) {
        // SAFETY: Caller should ensure the capacity is large enough to hold elements.
        unsafe {
            self.data().add(self.len()).write(v);
            self.len += 1;
        }
    }

    /// Returns true if this `JsStringBuilder` has a length of zero, and false otherwise.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Checks if all bytes in this inner is ascii.
    fn is_ascii(&self) -> bool {
        let ptr = self.inner.as_ptr();
        // SAFETY:
        // `NonNull` verified for us that the pointer returned by `alloc` is valid,
        // meaning we can read to its pointed memory.
        let data = unsafe {
            std::slice::from_raw_parts(
                addr_of!((*ptr).data).cast::<u8>(),
                self.allocated_data_byte_len(),
            )
        };
        data.is_ascii()
    }

    /// build `JsString` from `JsStringBuilder`
    #[must_use]
    pub fn build(mut self) -> JsString {
        if self.is_empty() {
            return JsString::default();
        }
        let len = self.len();

        // Shrink to fit the length.
        if len != self.capacity() {
            let layout = Self::new_layout(self.len());
            self.allocate_inner(layout);
        }

        let inner = self.inner;

        // SAFETY:
        // `NonNull` verified for us that the pointer returned by `alloc` is valid,
        // meaning we can write to its pointed memory.
        unsafe {
            inner.as_ptr().write(RawJsString {
                flags_and_len: RawJsString::encode_flags_and_len(len, self.is_ascii()),
                refcount: Cell::new(1),
                data: [0; 0],
            });
        }

        // Tell the compiler not to call the destructor of `JsStringBuilder`,
        // becuase we move inner `RawJsString` to `JsString`.
        std::mem::forget(self);
        JsString {
            ptr: Tagged::from_non_null(inner),
        }
    }
}

impl<D: private::JsStringData> Drop for JsStringBuilder<D> {
    /// Set cold since [`JsStringBuilder`] should be created to build `JsString`
    #[cold]
    fn drop(&mut self) {
        if self.is_dangling() {
            return;
        }
        let layout = self.current_layout();

        // SAFETY:
        // layout: see safety above.
        // `NonNull` verified for us that the pointer returned by `alloc` is valid,
        // meaning we can free its pointed memory.
        unsafe {
            dealloc(self.inner.as_ptr().cast(), layout);
        }
    }
}

impl<D: private::JsStringData> FromIterator<D> for JsStringBuilder<D> {
    fn from_iter<T: IntoIterator<Item = D>>(iter: T) -> Self {
        let mut builder = Self::new();
        builder.extend(iter);
        builder
    }
}
