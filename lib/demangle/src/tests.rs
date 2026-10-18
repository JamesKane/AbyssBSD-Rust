// SPDX-License-Identifier: BSD-2-Clause
//! Unit tests: real compiler output, hand-encoded grammar corners, fallback
//! behaviour, and robustness against truncated and corrupted input.

extern crate std;

use super::{Demangled, demangle};
use core::fmt::{self, Write};
use std::format;
use std::string::String;
use std::vec::Vec;

fn dm(sym: &[u8]) -> String {
    let mut s = String::new();
    demangle(sym, &mut s).expect("String never fails");
    s
}

fn raw(sym: &[u8]) -> String {
    String::from_utf8_lossy(sym)
        .chars()
        .map(|c| {
            if c == '\u{fffd}' || c.is_control() {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// v0 symbols emitted by rustc 1.99.0 (a test crate, std, and the rustc driver), with
/// the expected output cross-checked against binutils `c++filt` after removing its
/// `[hash]` crate disambiguators and `: type` const suffixes.
const V0_COMPILER: &[(&str, &str)] = &[
    ("_RNvCsk9g3uQ2qDQs_2dv2f1", "dv::f1"),
    ("_RNvCsk9g3uQ2qDQs_2dv4main", "dv::main"),
    ("_RNvCsk9g3uQ2qDQs_2dvu12unicode__k3a", "dv::unicode_ñ"),
    ("_RNvMs_Csk9g3uQ2qDQs_2dvNtB4_2Sq3new", "<dv::Sq>::new"),
    ("_RINvCsk9g3uQ2qDQs_2dv3rawyEB2_", "dv::raw::<u64>"),
    (
        "_RNCNvCsk9g3uQ2qDQs_2dv4mains0_0B3_",
        "dv::main::{closure#2}",
    ),
    (
        "_RNCNCNvCsk9g3uQ2qDQs_2dv4main00B5_",
        "dv::main::{closure#0}::{closure#0}",
    ),
    (
        "_RNvXs0_Csk9g3uQ2qDQs_2dvhNtB5_5Assoc3get",
        "<u8 as dv::Assoc>::get",
    ),
    (
        "_RNvXs1g_NtCsevLNFiNqfJP_4core3fmtRRReNtB6_5Debug3fmtCsk9g3uQ2qDQs_2dv",
        "<&&&str as core::fmt::Debug>::fmt",
    ),
    (
        "_RNvXsr_NtCsevLNFiNqfJP_4core3fmtSxNtB5_5Debug3fmtCsk9g3uQ2qDQs_2dv",
        "<[i64] as core::fmt::Debug>::fmt",
    ),
    (
        "_RNvMNtNtCsevLNFiNqfJP_4core3ptr9const_ptrPu13is_aligned_toCsk9g3uQ2qDQs_2dv",
        "<*const ()>::is_aligned_to",
    ),
    (
        "_RNvMNtNtCsk9g3uQ2qDQs_2dv5outer5innerINtB2_5ThingKj4_E3lenB6_",
        "<dv::outer::inner::Thing<4>>::len",
    ),
    (
        "_RINvNtNtCsk9g3uQ2qDQs_2dv5outer5inner4deepAxj3_EB6_",
        "dv::outer::inner::deep::<[i64; 3]>",
    ),
    (
        "_RINvNtNtCsk9g3uQ2qDQs_2dv5outer5inner4deepThcRefEEB6_",
        "dv::outer::inner::deep::<(u8, char, &str, f32)>",
    ),
    (
        "_RNvXCsk9g3uQ2qDQs_2dvNtB2_2SqNtB2_5Shape4area",
        "<dv::Sq as dv::Shape>::area",
    ),
    (
        "_RNvYNtCsk9g3uQ2qDQs_2dv2SqNtB4_5Shape4nameB4_",
        "<dv::Sq as dv::Shape>::name",
    ),
    (
        "_RINvMs2_NtCsevLNFiNqfJP_4core3fmtNtB6_9Arguments3newKj2_Kj1_ECsk9g3uQ2qDQs_2dv",
        "<core::fmt::Arguments>::new::<2, 1>",
    ),
    (
        "_RNvNvMs_NtCs1C1lLj4PWSY_5alloc3vecINtB6_3VecppE7set_len18precondition_checkCsk9g3uQ2qDQs_2dv",
        "<alloc::vec::Vec<_, _>>::set_len::precondition_check",
    ),
    (
        "_RNSNvYNCINvNtCsgHYyB3hU8Ec_3std2rt10lang_startuE0INtNtNtCsevLNFiNqfJP_4core3ops8function6FnOnceuE9call_once6vtableCsk9g3uQ2qDQs_2dv",
        "<std::rt::lang_start<()>::{closure#0} as core::ops::function::FnOnce<()>>::call_once::{shim:vtable#0}",
    ),
    (
        "_RINvNtNtCsgHYyB3hU8Ec_3std3sys9backtrace28___rust_begin_short_backtraceFEuuECsk9g3uQ2qDQs_2dv",
        "std::sys::backtrace::__rust_begin_short_backtrace::<fn(), ()>",
    ),
    (
        "_RNvMs_NtNtCsk9g3uQ2qDQs_2dv5outer5innerINtB4_4FlagKb1_Kc78_Kln5_E4showB8_",
        "<dv::outer::inner::Flag<true, 'x', -5>>::show",
    ),
    (
        "_RINvXs_NvMNtCs1C1lLj4PWSY_5alloc5sliceSp9to_vec_inhNtB5_10ConvertVec6to_vecNtNtBa_5alloc6GlobalECsgHYyB3hU8Ec_3std",
        "<u8 as <[_]>::to_vec_in::ConvertVec>::to_vec::<alloc::alloc::Global>",
    ),
    (
        "_RNSNvYNCNvCsk9g3uQ2qDQs_2dv4mains3_0INtNtNtCsevLNFiNqfJP_4core3ops8function6FnOnceThEE9call_once6vtableB8_",
        "<dv::main::{closure#5} as core::ops::function::FnOnce<(u8,)>>::call_once::{shim:vtable#0}",
    ),
    (
        "_RNvYNvYeNtNtCs1C1lLj4PWSY_5alloc6borrow7ToOwned8to_ownedINtNtNtCsevLNFiNqfJP_4core3ops8function6FnOnceTReEE9call_onceCsk9g3uQ2qDQs_2dv",
        "<<str as alloc::borrow::ToOwned>::to_owned as core::ops::function::FnOnce<(&str,)>>::call_once",
    ),
    (
        "_RINvMNtCsevLNFiNqfJP_4core6optionINtB3_6OptionReE11map_or_elseNtNtCs1C1lLj4PWSY_5alloc6string6StringNCNvNtB12_3fmt6format0NvYeNtNtB12_6borrow7ToOwned8to_ownedECsk9g3uQ2qDQs_2dv",
        "<core::option::Option<&str>>::map_or_else::<alloc::string::String, alloc::fmt::format::{closure#0}, <str as alloc::borrow::ToOwned>::to_owned>",
    ),
    (
        "_RINvNtCsevLNFiNqfJP_4core3ptr9drop_glueINtNtCs1C1lLj4PWSY_5alloc5boxed3BoxDINtNtNtB4_3ops8function2FnThEEp6OutputhNtNtB4_6marker4SendNtB1Q_4SyncEL_EECsk9g3uQ2qDQs_2dv",
        "core::ptr::drop_glue::<alloc::boxed::Box<dyn core::ops::function::Fn<(u8,), Output = u8> + core::marker::Send + core::marker::Sync>>",
    ),
    (
        "_RNvXsv_NtCs1C1lLj4PWSY_5alloc5boxedINtB5_3BoxDINtNtNtCsevLNFiNqfJP_4core3ops8function2FnThEEp6OutputhNtNtBP_6marker4SendNtB1D_4SyncEL_EIBJ_B1o_E4callCsk9g3uQ2qDQs_2dv",
        "<alloc::boxed::Box<dyn core::ops::function::Fn<(u8,), Output = u8> + core::marker::Send + core::marker::Sync> as core::ops::function::Fn<(u8,)>>::call",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrFG_RL0_hERL0_hEB2_",
        "dv::hr::<for<'a> fn(&'a u8) -> &'a u8>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrFK6systemEuEB2_",
        "dv::hr::<extern \"system\" fn()>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrFKClElEB2_",
        "dv::hr::<extern \"C\" fn(i32) -> i32>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrFUKCOhvElEB2_",
        "dv::hr::<unsafe extern \"C\" fn(*mut u8, ...) -> i32>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrINtNtCsevLNFiNqfJP_4core6option6OptionQAPhj2_EEB2_",
        "dv::hr::<core::option::Option<&mut [*const u8; 2]>>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrRDG_INtNtNtCsevLNFiNqfJP_4core3ops8function2FnTRL0_hEEp6OutputRL0_hEL_EB2_",
        "dv::hr::<&dyn for<'a> core::ops::function::Fn<(&'a u8,), Output = &'a u8>>",
    ),
    (
        "_RINvCsk9g3uQ2qDQs_2dv2hrTOnoisfuAcj0_EEB2_",
        "dv::hr::<(*mut i128, u128, isize, i16, f32, (), [char; 0])>",
    ),
    (
        "_RINvMs3_Csk9g3uQ2qDQs_2dvINtB6_3GenaE6methodNtNtCs1C1lLj4PWSY_5alloc6string6StringEB6_",
        "<dv::Gen<i8>>::method::<alloc::string::String>",
    ),
    (
        "_RNvXs4_Csk9g3uQ2qDQs_2dvINtB5_3GenReEINtB5_5VisitQSaE5visit",
        "<dv::Gen<&str> as dv::Visit<&mut [i8]>>::visit",
    ),
    (
        "_RNvMs1_Csk9g3uQ2qDQs_2dvINtB5_3BigKoffffffffffffffffffffffffffffffff_E1vB5_",
        "<dv::Big<340282366920938463463374607431768211455>>::v",
    ),
    (
        "_RNvMs2_Csk9g3uQ2qDQs_2dvINtB5_3NegKxn8000000000000000_E1vB5_",
        "<dv::Neg<-9223372036854775808>>::v",
    ),
    (
        "_RNvNtNtCsgHYyB3hU8Ec_3std6thread11main_thread4MAIN.0",
        "std::thread::main_thread::MAIN",
    ),
    (
        "_RNCINvNtNtCsgHYyB3hU8Ec_3std6thread7current17with_current_nameNCNCNvNtB8_9panicking12default_hook00uE0B8_",
        "std::thread::current::with_current_name::<std::panicking::default_hook::{closure#0}::{closure#0}, ()>::{closure#0}",
    ),
    (
        "_RNnNvNtCseca8QvGOu3U_10rustc_lint8internal14BAD_OPT_ACCESS0",
        "rustc_lint::internal::BAD_OPT_ACCESS",
    ),
    (
        "_RNcNtNtNtNtCs8HMaf5nuCYM_12rustc_middle3mir6syntax7Operand4Copy0",
        "rustc_middle::mir::syntax::Operand::Copy",
    ),
    (
        "_RNvXs_NtCs1bRX4niITE5_4test5statsSdNtB4_5Stats3max",
        "<[f64] as test::stats::Stats>::max",
    ),
    (
        "_RINvMs2_NtCsbf9XTTZXLAs_4jiff4spanNtB6_4Span15try_days_rangedINtNtNtB8_4util8rangeint5ri128Knn80000000000000000000000000000000_Kn7fffffffffffffffffffffffffffffff_EEB8_",
        "<jiff::span::Span>::try_days_ranged::<jiff::util::rangeint::ri128<-170141183460469231731687303715884105728, 170141183460469231731687303715884105727>>",
    ),
    (
        "_RNvNtCsk9g3uQ2qDQs_2dvu10wgv71a119eu7kdvt68h",
        "dv::日本語::関数",
    ),
    ("_RNvCsk9g3uQ2qDQs_2dvu12unicode__k3a", "dv::unicode_ñ"),
    ("_RNvCsk9g3uQ2qDQs_2dvu11gre__wna7od", "dv::grüße_ü"),
    (
        "_RNvCsk9g3uQ2qDQs_2dvu30____7hkackfecea1cbdathfdh9hlq6y",
        "dv::საჭმელად_გემრიელი_სადილი",
    ),
];
/// Hand-encoded v0 symbols for grammar rustc rarely emits (structural and
/// string constants, lifetimes, binders, ABIs), cross-checked against `c++filt`
/// where it supports them.
const V0_HAND: &[(&str, &str)] = &[
    ("_RINvCs1a_1c3fooKTj1_b0_EE", "c::foo::<{(1, false)}>"),
    ("_RINvCs1a_1c3fooKTj1_EE", "c::foo::<{(1,)}>"),
    ("_RINvCs1a_1c3fooKVNtB2_3BarUE", "c::foo::<{c::Bar}>"),
    ("_RINvCs1a_1c3fooKVNtB2_3BarTj7_EE", "c::foo::<{c::Bar(7)}>"),
    (
        "_RINvCs1a_1c3fooKVNtB2_3BarS1xj7_1yc61_EE",
        "c::foo::<{c::Bar { x: 7, y: 'a' }}>",
    ),
    ("_RINvCs1a_1c3fooKRAj1_EE", "c::foo::<{&[1]}>"),
    ("_RINvCs1a_1c3fooL_RL_hE", "c::foo::<'_, &u8>"),
    (
        "_RINvCs1a_1c3fooFK9rust_callEuE",
        "c::foo::<extern \"rust-call\" fn()>",
    ),
    (
        "_RINvCs1a_1c3fooFK13system_unwindEuE",
        "c::foo::<extern \"system-unwind\" fn()>",
    ),
    ("_RINvCs1a_1c3fooDNtB2_3BarEL_E", "c::foo::<dyn c::Bar>"),
    (
        "_RNvYINtCs1a_1c3BazhENtB5_3Bar3foo",
        "<c::Baz<u8> as c::Bar>::foo",
    ),
    ("_RNvMs_Cs1a_1cINtB4_3BazhE3new", "<c::Baz<u8>>::new"),
    ("_RINvCs1a_1c3fooKe68c3a9_E", "c::foo::<\"hé\">"),
    ("_RINvCs1a_1c3fooKe22275c_E", "c::foo::<\"\\\"'\\\\\">"),
    (
        "_RINvCs1a_1c3fooFG_RL0_DNtB2_3BarEL0_EuE",
        "c::foo::<for<'a> fn(&'a dyn c::Bar + 'a)>",
    ),
];
/// Legacy symbols from the same test crate built with
/// `-C symbol-mangling-version=legacy`.
const LEGACY: &[(&str, &str)] = &[
    (
        "_ZN2dv12Gen$LT$T$GT$6method17h41ff3fe4c16611c0E",
        "dv::Gen<T>::method",
    ),
    ("_ZN2dv13unicode_$uf1$17haa6a90dea5a29507E", "dv::unicode_ñ"),
    (
        "_ZN2dv4main28_$u7b$$u7b$closure$u7d$$u7d$17h8f07e8a2bced45d6E",
        "dv::main::{{closure}}",
    ),
    (
        "_ZN2dv4main28_$u7b$$u7b$closure$u7d$$u7d$28_$u7b$$u7b$closure$u7d$$u7d$17hafc1c0f284541bcbE",
        "dv::main::{{closure}}::{{closure}}",
    ),
    (
        "_ZN2dv5outer5inner21Flag$LT$_$C$_$C$_$GT$4show17hdf736e0fe21212c3E",
        "dv::outer::inner::Flag<_,_,_>::show",
    ),
    (
        "_ZN32_$LT$u8$u20$as$u20$dv..Assoc$GT$3get17h78d74944af60363fE",
        "<u8 as dv::Assoc>::get",
    ),
    (
        "_ZN36_$LT$dv..Sq$u20$as$u20$dv..Shape$GT$4area17h95781c8129cf3d05E",
        "<dv::Sq as dv::Shape>::area",
    ),
    (
        "_ZN3std2rt10lang_start28_$u7b$$u7b$closure$u7d$$u7d$17h87499c7dbccb8b09E",
        "std::rt::lang_start::{{closure}}",
    ),
    (
        "_ZN5alloc3fmt6format28_$u7b$$u7b$closure$u7d$$u7d$17h82b8f8f4cbbba785E",
        "alloc::fmt::format::{{closure}}",
    ),
    (
        "_ZN84_$LT$dv..Gen$LT$$RF$str$GT$$u20$as$u20$dv..Visit$LT$$RF$mut$u20$$u5b$i8$u5d$$GT$$GT$5visit17h964f5597f6cf5ed5E",
        "<dv::Gen<&str> as dv::Visit<&mut [i8]>>::visit",
    ),
    (
        "_ZN41_$LT$bool$u20$as$u20$core..fmt..Debug$GT$3fmt17he4eb1724361e6e9eE",
        "<bool as core::fmt::Debug>::fmt",
    ),
    (
        "_ZN42_$LT$$RF$T$u20$as$u20$core..fmt..Debug$GT$3fmt17h058c851ed4f96204E",
        "<&T as core::fmt::Debug>::fmt",
    ),
];

/// The examples from the crate's specification.
const V0_SPEC: &[(&str, &str)] = &[
    ("_RNvCs34uFkGXw8xg_1k5TABLE", "k::TABLE"),
    (
        "_RNvXCs34uFkGXw8xg_1kNtB2_1WNtNtCs1R9DcZHJAgT_4core3fmt5Write9write_str",
        "<k::W as core::fmt::Write>::write_str",
    ),
];

fn all_vectors() -> impl Iterator<Item = &'static (&'static str, &'static str)> {
    V0_COMPILER
        .iter()
        .chain(V0_HAND)
        .chain(V0_SPEC)
        .chain(LEGACY)
}

#[test]
fn v0_compiler_vectors() {
    for (m, d) in V0_COMPILER {
        assert_eq!(dm(m.as_bytes()), *d, "{m}");
    }
}

#[test]
fn v0_hand_vectors() {
    for (m, d) in V0_HAND.iter().chain(V0_SPEC) {
        assert_eq!(dm(m.as_bytes()), *d, "{m}");
    }
}

#[test]
fn legacy_vectors() {
    for (m, d) in LEGACY {
        assert_eq!(dm(m.as_bytes()), *d, "{m}");
    }
}

#[test]
fn display_adapter_matches() {
    for (m, d) in all_vectors() {
        assert_eq!(format!("{}", Demangled(m.as_bytes())), *d);
    }
    assert_eq!(format!("[{:>6}]", Demangled(b"x")), "[x]");
}

#[test]
fn prefix_variants() {
    assert_eq!(dm(b"__RNvCs34uFkGXw8xg_1k5TABLE"), "k::TABLE");
    assert_eq!(dm(b"__ZN1a1b17h0123456789abcdefE"), "a::b");
}

#[test]
fn vendor_suffixes_dropped() {
    assert_eq!(dm(b"_RNvCs34uFkGXw8xg_1k5TABLE.llvm.8812"), "k::TABLE");
    assert_eq!(dm(b"_RNvCs34uFkGXw8xg_1k5TABLE$foo"), "k::TABLE");
    assert_eq!(
        dm(b"_ZN4core3fmt5write17h0123456789abcdefE.llvm.42"),
        "core::fmt::write"
    );
}

#[test]
fn legacy_details() {
    // No hash: every element is shown.
    assert_eq!(dm(b"_ZN3foo3barE"), "foo::bar");
    // A lone hash-looking element is the name itself.
    assert_eq!(dm(b"_ZN17h0123456789abcdefE"), "h0123456789abcdef");
    // Not a hash (15 digits): kept.
    assert_eq!(dm(b"_ZN1a16h0123456789abcdeE"), "a::h0123456789abcde");
    let elem = "_$LT$$u5b$T$u5d$$u20$as$u20$core..fmt..Debug$GT$$u7b$$u27$$u7d$$SP$$BP$$C$$LP$$RP$";
    let sym = format!("_ZN{}{elem}3fmt17h0123456789abcdefE", elem.len());
    assert_eq!(dm(sym.as_bytes()), "<[T] as core::fmt::Debug>{'}@*,()::fmt");
    assert_eq!(dm(b"_ZN5a.b.c1dE"), "a.b.c::d");
}

#[test]
fn non_rust_symbols_are_raw() {
    for sym in [
        &b"memcpy"[..],
        b"",
        b"_R",
        b"_ZN",
        b"_ZN3foo3barEv",
        b"_Z3foov",
        b"_R0NvC1a1b",
        b"_RNvCs34uFkGXw8xg_1k5TABLEgarbage",
        b"_ZN3fooEjunk",
    ] {
        assert_eq!(dm(sym), raw(sym), "{sym:?}");
    }
    assert_eq!(dm(b"a\xffb\xc3"), "a?b?");
}

#[test]
fn malformed_v0_is_never_half_printed() {
    // Each is a valid prefix followed by junk: output must be the raw bytes,
    // not "c::foo::<" followed by the raw bytes.
    for sym in [
        &b"_RINvCs1a_1c3fooQ"[..],
        b"_RINvCs1a_1c3foohhh",
        b"_RINvCs1a_1c3foohKj1",
        b"_RINvCs1a_1c3fooKb2_E",
        b"_RINvCs1a_1c3fooKcd800_E",
        b"_RINvCs1a_1c3fooKjn1_E",
        b"_RINvCs1a_1c3fooKeff_E",
        b"_RINvCs1a_1c3fooKe6_E",
        b"_RINvCs1a_1c3fooL0_E",
        b"_RNvCs1a_1c4foo",
        b"_RNvCs1a_1c3foo\xff",
    ] {
        assert_eq!(dm(sym), raw(sym), "{sym:?}");
    }
    assert_eq!(dm(b"_ZN3foo3$LTE"), raw(b"_ZN3foo3$LTE"));
    assert_eq!(dm(b"_ZN3foo4$XX$E"), raw(b"_ZN3foo4$XX$E"));
    assert_eq!(dm(b"_ZN03fooE"), raw(b"_ZN03fooE"));
}

#[test]
fn control_characters_are_replaced() {
    assert_eq!(dm(b"bad\x1b[2Jname\x7f"), "bad?[2Jname?");
    assert_eq!(dm(b"_RNvCs1a_1c3a\x1bb"), "c::a?b");
    assert_eq!(dm(b"_ZN3a\tbE"), "a?b");
    // Punycode decoding to C1 controls.
    assert_eq!(dm(b"_RNvCs1a_1cu3a1b"), "c::??");
}

#[test]
fn undecodable_punycode_is_shown_encoded() {
    // `9` decodes past the last code point.
    assert_eq!(dm(b"_RNvCs1a_1cu5ab_99"), "c::punycode{ab-99}");
}

#[test]
fn backrefs_must_point_backwards() {
    // B_ (offset 0) from offset 0, and forward references.
    assert_eq!(dm(b"_RB_"), "_RB_");
    assert_eq!(dm(b"_RNvB2_3foo"), "_RNvB2_3foo");
    assert_eq!(dm(b"_RNvB_3foo"), "_RNvB_3foo");
    assert_eq!(
        dm(b"_RNvBzzzzzzzzzzzzzzzzzzz_3foo"),
        raw(b"_RNvBzzzzzzzzzzzzzzzzzzz_3foo")
    );
}

/// Base-62 encoding of `n` as a backref operand.
fn b62(n: usize) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    if n == 0 {
        return String::from("_");
    }
    let mut n = n - 1;
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[n % 62]);
        n /= 62;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    out.push(b'_');
    String::from_utf8(out).unwrap()
}

#[test]
fn exponential_backrefs_fall_back_quickly() {
    // Each tuple holds two backrefs to the previous one: 2^levels leaves.
    let mut body = String::from("INvCs1a_1c3foo");
    let mut prev = body.len();
    body.push_str("ThhE");
    for _ in 0..30 {
        let here = body.len();
        body.push_str(&format!("TB{}B{}E", b62(prev), b62(prev)));
        prev = here;
    }
    body.push('E');
    let sym = format!("_R{body}");
    assert_eq!(dm(sym.as_bytes()), sym);

    // A few levels are fine.
    let mut body = String::from("INvCs1a_1c3foo");
    let mut prev = body.len();
    body.push_str("ThhE");
    for _ in 0..2 {
        let here = body.len();
        body.push_str(&format!("TB{}B{}E", b62(prev), b62(prev)));
        prev = here;
    }
    body.push('E');
    assert_eq!(
        dm(format!("_R{body}").as_bytes()),
        "c::foo::<(u8, u8), ((u8, u8), (u8, u8)), (((u8, u8), (u8, u8)), ((u8, u8), (u8, u8)))>"
    );
}

#[test]
fn deep_nesting_falls_back() {
    let mut sym = String::from("_RINvCs1a_1c3foo");
    sym.push_str(&"R".repeat(10_000));
    sym.push_str("hE");
    assert_eq!(dm(sym.as_bytes()), sym);

    let mut ok = String::from("_RINvCs1a_1c3foo");
    ok.push_str(&"R".repeat(20));
    ok.push_str("hE");
    assert_eq!(dm(ok.as_bytes()), format!("c::foo::<{}u8>", "&".repeat(20)));
}

#[test]
fn many_binder_lifetimes() {
    // 30 bound lifetimes run past 'z into '_26 and beyond.
    let sym = format!(
        "_RINvCs1a_1c3fooF{}{}EuE",
        format_args!("G{}", b62(29)),
        "RL0_h"
    );
    let out = dm(sym.as_bytes());
    assert!(out.starts_with("c::foo::<for<'a, 'b, "), "{out}");
    assert!(
        out.contains("'z, '_26, '_27, '_28, '_29> fn(&'_29 u8)"),
        "{out}"
    );
    // A huge binder count is cut off by the work cap, not looped over.
    let sym = b"_RINvCs1a_1c3fooFGzzzzzzzzzz_EuE";
    assert_eq!(dm(sym), raw(sym));
}

#[test]
fn writer_errors_propagate() {
    struct Fail;
    impl Write for Fail {
        fn write_str(&mut self, _: &str) -> fmt::Result {
            Err(fmt::Error)
        }
    }
    assert!(demangle(b"_RNvCs34uFkGXw8xg_1k5TABLE", &mut Fail).is_err());
    assert!(demangle(b"_ZN3foo3barE", &mut Fail).is_err());
    assert!(demangle(b"memcpy", &mut Fail).is_err());
    assert!(demangle(b"", &mut Fail).is_ok());
}

#[test]
fn every_truncation_is_handled() {
    for (m, _) in all_vectors() {
        let m = m.as_bytes();
        for end in 0..m.len() {
            let out = dm(&m[..end]);
            assert!(out.len() <= super::sink::MAX_OUTPUT + m.len());
        }
    }
}

/// xorshift64*: deterministic, no dependencies.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

#[test]
fn random_mutations_are_handled() {
    // Bytes biased towards grammar characters so mutations go deep.
    const ALPHABET: &[u8] = b"_0123456789BCDEFGIKLMNOPQRSTUVXYZabcehjlmnpsuvxyz$.";
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for (m, _) in all_vectors() {
        for _ in 0..300 {
            let mut s = m.as_bytes().to_vec();
            for _ in 0..=rng.below(4) {
                let at = rng.below(s.len() + 1);
                let byte = if rng.below(8) == 0 {
                    u8::try_from(rng.below(256)).unwrap()
                } else {
                    ALPHABET[rng.below(ALPHABET.len())]
                };
                match rng.below(3) {
                    0 if at < s.len() => s[at] = byte,
                    1 if at < s.len() => {
                        s.remove(at);
                    }
                    _ => s.insert(at, byte),
                }
            }
            let out = dm(&s);
            assert!(out.len() <= super::sink::MAX_OUTPUT + s.len());
        }
    }
}

#[test]
fn random_bytes_are_handled() {
    let mut rng = Rng(42);
    for _ in 0..20_000 {
        let len = rng.below(48);
        let mut s: Vec<u8> = (0..len)
            .map(|_| u8::try_from(rng.below(128)).unwrap())
            .collect();
        let prefix: &[u8] = if rng.below(2) == 0 { b"_R" } else { b"_ZN" };
        s.splice(0..0, prefix.iter().copied());
        let _ = dm(&s);
    }
}
