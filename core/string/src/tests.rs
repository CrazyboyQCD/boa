#![allow(clippy::redundant_clone)]

use std::hash::{BuildHasher, BuildHasherDefault, Hash};

use crate::{JsStr, JsString, JsStringBuilder, StaticJsStrings};

use rustc_hash::FxHasher;

fn hash_value<T: Hash>(value: &T) -> u64 {
    BuildHasherDefault::<FxHasher>::default().hash_one(value)
}

const fn ascii_to_utf16<const LEN: usize>(ascii: &[u8; LEN]) -> [u16; LEN] {
    let mut array = [0; LEN];
    let mut i = 0;
    while i < LEN {
        array[i] = ascii[i] as u16;
        i += 1;
    }
    array
}

#[test]
fn empty() {
    let s = StaticJsStrings::EMPTY_STRING;
    assert_eq!(&s, &[]);
}

#[test]
fn refcount() {
    let x = JsString::from("Hello world");
    assert_eq!(x.refcount(), Some(1));

    {
        let y = x.clone();
        assert_eq!(x.refcount(), Some(2));
        assert_eq!(y.refcount(), Some(2));

        {
            let z = y.clone();
            assert_eq!(x.refcount(), Some(3));
            assert_eq!(y.refcount(), Some(3));
            assert_eq!(z.refcount(), Some(3));
        }

        assert_eq!(x.refcount(), Some(2));
        assert_eq!(y.refcount(), Some(2));
    }

    assert_eq!(x.refcount(), Some(1));
}

#[test]
fn static_refcount() {
    let x = StaticJsStrings::EMPTY_STRING;
    assert_eq!(x.refcount(), None);

    {
        let y = x.clone();
        assert_eq!(x.refcount(), None);
        assert_eq!(y.refcount(), None);
    };

    assert_eq!(x.refcount(), None);
}

#[test]
fn ptr_eq() {
    let x = JsString::from("Hello");
    let y = x.clone();

    assert!(!x.ptr.is_tagged());

    assert_eq!(x.ptr.addr(), y.ptr.addr());

    let z = JsString::from("Hello");
    assert_ne!(x.ptr.addr(), z.ptr.addr());
    assert_ne!(y.ptr.addr(), z.ptr.addr());
}

#[test]
fn static_ptr_eq() {
    let x = StaticJsStrings::EMPTY_STRING;
    let y = x.clone();

    assert!(x.ptr.is_tagged());

    assert_eq!(x.ptr.addr(), y.ptr.addr());

    let z = StaticJsStrings::EMPTY_STRING;
    assert_eq!(x.ptr.addr(), z.ptr.addr());
    assert_eq!(y.ptr.addr(), z.ptr.addr());
}

#[test]
fn as_str() {
    const HELLO: &[u16] = &ascii_to_utf16(b"Hello");
    let x = JsString::from(HELLO);

    assert_eq!(&x, HELLO);
}

#[test]
fn hash() {
    const HELLOWORLD: JsStr<'_> = JsStr::latin1("Hello World!".as_bytes());
    let x = JsString::from(HELLOWORLD);

    assert_eq!(x.as_str(), HELLOWORLD);

    assert!(HELLOWORLD.is_latin1());
    assert!(x.as_str().is_latin1());

    let s_hash = hash_value(&HELLOWORLD);
    let x_hash = hash_value(&x);

    assert_eq!(s_hash, x_hash);
}

#[test]
fn concat() {
    const Y: &[u16] = &ascii_to_utf16(b", ");
    const W: &[u16] = &ascii_to_utf16(b"!");

    let x = JsString::from("hello");
    let z = JsString::from("world");

    let xy = JsString::concat(x.as_str(), JsString::from(Y).as_str());
    assert_eq!(&xy, &ascii_to_utf16(b"hello, "));
    assert_eq!(xy.refcount(), Some(1));

    let xyz = JsString::concat(xy.as_str(), z.as_str());
    assert_eq!(&xyz, &ascii_to_utf16(b"hello, world"));
    assert_eq!(xyz.refcount(), Some(1));

    let xyzw = JsString::concat(xyz.as_str(), JsString::from(W).as_str());
    assert_eq!(&xyzw, &ascii_to_utf16(b"hello, world!"));
    assert_eq!(xyzw.refcount(), Some(1));
}

#[test]
fn trim_start_non_ascii_to_ascii() {
    let s = "\u{2029}abc";
    let x = JsString::from(s);

    let y = JsString::from(x.trim_start());

    assert_eq!(&y, s.trim_start());
}

#[test]
fn conversion_to_known_static_js_string() {
    const JS_STR_U8: &JsStr<'_> = &JsStr::latin1("length".as_bytes());
    const JS_STR_U16: &JsStr<'_> = &JsStr::utf16(&ascii_to_utf16(b"length"));

    assert!(JS_STR_U8.is_latin1());
    assert!(!JS_STR_U16.is_latin1());

    assert_eq!(JS_STR_U8, JS_STR_U8);
    assert_eq!(JS_STR_U16, JS_STR_U16);

    assert_eq!(JS_STR_U8, JS_STR_U16);
    assert_eq!(JS_STR_U16, JS_STR_U8);

    assert_eq!(hash_value(JS_STR_U8), hash_value(JS_STR_U16));

    let string = StaticJsStrings::get_string(JS_STR_U8);

    assert!(string.is_some());
    assert!(string.unwrap().as_str().is_latin1());

    let string = StaticJsStrings::get_string(JS_STR_U16);

    assert!(string.is_some());
    assert!(string.unwrap().as_str().is_latin1());
}

#[test]
fn js_string_builder() {
    let utf16 = "2024年5月21日".encode_utf16().collect::<Vec<_>>();
    let s_utf16 = utf16.as_slice();
    let s_utf8 = &b"Lorem ipsum dolor sit amet"[..];

    // utf8 -- test
    let s_js_string = JsString::from(JsStr::latin1(s_utf8));

    // push
    let mut builder: JsStringBuilder<u8> = JsStringBuilder::new();
    for &code in s_utf8 {
        builder.push(code);
    }
    let s_builder = builder.build();
    assert_eq!(s_js_string, s_builder);

    // from_iter
    let s_builder = s_utf8
        .iter()
        .copied()
        .collect::<JsStringBuilder<_>>()
        .build();
    assert_eq!(s_js_string, s_builder);

    // extend_from_slice
    let mut builder = JsStringBuilder::new();
    builder.extend_from_slice(s_utf8);
    let s_builder = builder.build();
    assert_eq!(s_js_string, s_builder);

    // utf16 -- test
    let s_js_string = JsString::from(s_utf16);

    // push
    let mut builder = JsStringBuilder::new();
    for &code in s_utf16 {
        builder.push(code);
    }
    let s_builder = builder.build();
    assert_eq!(s_js_string, s_builder);

    // from_iter
    let s_builder = s_utf16
        .iter()
        .copied()
        .collect::<JsStringBuilder<_>>()
        .build();
    assert_eq!(s_js_string, s_builder);

    // extend_from_slice
    let mut builder = JsStringBuilder::new();
    builder.extend_from_slice(s_utf16);
    let s_builder = builder.build();
    assert_eq!(s_js_string, s_builder);
}
