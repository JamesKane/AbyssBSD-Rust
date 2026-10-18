// SPDX-License-Identifier: BSD-2-Clause
//! Data objects, operators, conversions, and store semantics.

extern crate std;

use alloc::vec::Vec;

use super::util::*;
use crate::{Error, Value};

const L0: &[u8] = &[LOCAL0];
const L1: &[u8] = &[LOCAL0 + 1];
const NULL: &[u8] = &[0];

fn op2(opc: u8, a: &[u8], b: &[u8], t: &[u8]) -> Vec<u8> {
    cat(&[&[opc], a, b, t])
}

fn op1(opc: &[u8], a: &[u8], t: &[u8]) -> Vec<u8> {
    cat(&[opc, a, t])
}

fn int_of(v: Value) -> u64 {
    v.as_integer()
        .unwrap_or_else(|| panic!("not an integer: {v:?}"))
}

#[test]
fn integer_constants() {
    assert_eq!(run(&ret(&[0x00])), Value::Integer(0));
    assert_eq!(run(&ret(&[0x01])), Value::Integer(1));
    assert_eq!(run(&ret(&[0xff])), Value::Integer(u64::MAX));
    assert_eq!(run(&ret(&[0x0a, 0x12])), Value::Integer(0x12));
    assert_eq!(run(&ret(&[0x0b, 0x34, 0x12])), Value::Integer(0x1234));
    assert_eq!(run(&ret(&int(0x1234_5678))), Value::Integer(0x1234_5678));
    assert_eq!(
        run(&ret(&int(0x1122_3344_5566_7788))),
        Value::Integer(0x1122_3344_5566_7788)
    );
    // Revision 1: Ones and QWord constants are 32 bits wide.
    let (mut i, mut h) = load_rev(
        &cat(&[
            &method("ONES", 0, &ret(&[0xff])),
            &method("QWRD", 0, &ret(&int(0x1122_3344_5566_7788))),
            &method("WRAP", 0, &ret(&op2(0x72, &[0xff], &int(2), NULL))),
        ]),
        1,
    );
    assert_eq!(eval_int(&mut i, &mut h, "\\ONES"), 0xffff_ffff);
    assert_eq!(eval_int(&mut i, &mut h, "\\QWRD"), 0x5566_7788);
    assert_eq!(eval_int(&mut i, &mut h, "\\WRAP"), 1);
}

#[test]
fn strings_buffers_packages() {
    assert_eq!(run(&ret(&string("ACPI"))), Value::String("ACPI".into()));
    // A buffer larger than its initializer is zero-padded; a smaller size
    // grows to the initializer.
    let b = pkg(&[0x11], &cat(&[&int(6), &[1, 2, 3]]));
    assert_eq!(run(&ret(&b)), Value::Buffer(std::vec![1, 2, 3, 0, 0, 0]));
    let b = pkg(&[0x11], &cat(&[&int(1), &[1, 2, 3]]));
    assert_eq!(run(&ret(&b)), Value::Buffer(std::vec![1, 2, 3]));
    // Package (4) { 1, "x" } pads with uninitialized elements.
    let p = pkg(&[0x12], &cat(&[&[4], &int(1), &string("x")]));
    assert_eq!(
        run(&ret(&p)),
        Value::Package(std::vec![
            Value::Integer(1),
            Value::String("x".into()),
            Value::Uninitialized,
            Value::Uninitialized
        ])
    );
    // VarPackage with a computed count, and a nested package.
    let vp = pkg(
        &[0x13],
        &cat(&[&op2(0x72, &int(1), &int(1), NULL), &package(&[int(9)])]),
    );
    let Value::Package(p) = run(&ret(&vp)) else {
        panic!()
    };
    assert_eq!(p.len(), 2);
    assert_eq!(p[0], Value::Package(std::vec![Value::Integer(9)]));
    // Names in packages stay unresolved until used.
    let (i, _) = load(&cat(&[
        &name_op("PKG", &package(&[name("LATE"), name("\\_SB")])),
        &name_op("LATE", &int(5)),
    ]));
    let pkg_node = i.lookup(&crate::AmlPath::parse("\\PKG").unwrap()).unwrap();
    let _ = pkg_node;
    let mut h = FakeHost::default();
    let mut i = i;
    let Value::Package(p) = eval(&mut i, &mut h, "\\PKG", Vec::new()) else {
        panic!()
    };
    assert!(matches!(p[0], Value::Name(..)));
    let late = i.resolve_value_name(&p[0]).unwrap();
    assert_eq!(i.path(late).to_text(), "\\LATE");
}

#[test]
fn arithmetic() {
    let cases: &[(Vec<u8>, u64)] = &[
        (op2(0x72, &int(5), &int(7), NULL), 12),
        (op2(0x74, &int(5), &int(7), NULL), 5u64.wrapping_sub(7)),
        (op2(0x77, &int(6), &int(7), NULL), 42),
        (op2(0x85, &int(17), &int(5), NULL), 2),
        (op2(0x79, &int(1), &int(4), NULL), 16),
        (op2(0x79, &int(1), &int(64), NULL), 0),
        (op2(0x7a, &int(0x80), &int(3), NULL), 0x10),
        (op2(0x7b, &int(0xf0), &int(0x3c), NULL), 0x30),
        (op2(0x7d, &int(0xf0), &int(0x0f), NULL), 0xff),
        (op2(0x7f, &int(0xff), &int(0x0f), NULL), 0xf0),
        (op2(0x7c, &int(0xff), &int(0x0f), NULL), !0x0f),
        (op2(0x7e, &int(0xf0), &int(0x0f), NULL), !0xff),
        (op1(&[0x80], &int(0), NULL), u64::MAX),
        (op1(&[0x81], &int(0), NULL), 0),
        (op1(&[0x81], &int(0x90), NULL), 8),
        (op1(&[0x82], &int(0x90), NULL), 5),
        (op1(&[0x5b, 0x28], &int(0x1234), NULL), 1234),
        (op1(&[0x5b, 0x29], &int(1234), NULL), 0x1234),
    ];
    for (code, want) in cases {
        assert_eq!(int_of(run(&ret(code))), *want, "{code:x?}");
    }
    // Divide stores the remainder and quotient.
    let body = cat(&[
        &cat(&[&[0x78], &int(17), &int(5), L0, L1]),
        &ret(&op2(0x77, L0, &int(100), NULL)),
    ]);
    // Local0 = remainder 2, Local1 = quotient 3; return 2 * 100.
    assert_eq!(int_of(run(&body)), 200);
    assert_eq!(
        run_err(&ret(&op2(0x85, &int(1), &int(0), NULL))),
        Error::DivideByZero
    );
    assert_eq!(
        run_err(&ret(&cat(&[&[0x78], &int(1), &int(0), NULL, NULL]))),
        Error::DivideByZero
    );
    // Increment and Decrement on a local.
    let body = cat(&[
        &store(&int(41), L0),
        &[0x75, LOCAL0],
        &[0x75, LOCAL0],
        &[0x76, LOCAL0],
        &ret(L0),
    ]);
    assert_eq!(int_of(run(&body)), 42);
}

#[test]
fn logic_and_comparison() {
    let t = u64::MAX;
    let cases: &[(Vec<u8>, u64)] = &[
        (op2(0x90, &int(1), &int(2), &[]), t),
        (op2(0x90, &int(1), &int(0), &[]), 0),
        (op2(0x91, &int(0), &int(2), &[]), t),
        (cat(&[&[0x92], &int(0)]), t),
        (op2(0x93, &int(3), &int(3), &[]), t),
        (op2(0x94, &int(4), &int(3), &[]), t),
        (op2(0x95, &int(4), &int(3), &[]), 0),
        // LNotEqual, LLessEqual, LGreaterEqual are LNot of the others.
        (cat(&[&[0x92], &op2(0x93, &int(3), &int(4), &[])]), t),
        (cat(&[&[0x92], &op2(0x94, &int(3), &int(3), &[])]), t),
        (cat(&[&[0x92], &op2(0x95, &int(2), &int(3), &[])]), 0),
        // Strings compare as strings, with the second converted.
        (op2(0x93, &string("abc"), &string("abc"), &[]), t),
        (op2(0x95, &string("abc"), &string("abd"), &[]), t),
        (op2(0x94, &string("abcd"), &string("abc"), &[]), t),
        (op2(0x93, &buffer(&[1, 2]), &buffer(&[1, 2]), &[]), t),
        (op2(0x95, &buffer(&[1]), &buffer(&[1, 0]), &[]), t),
        // An integer first operand converts a string as hex.
        (op2(0x93, &int(0x1f), &string("1F"), &[]), t),
    ];
    for (code, want) in cases {
        assert_eq!(int_of(run(&ret(code))), *want, "{code:x?}");
    }
}

#[test]
fn explicit_conversions() {
    let s = |v: &str| Value::String(v.into());
    assert_eq!(
        run(&ret(&op1(&[0x99], &string("0x1F"), NULL))),
        Value::Integer(31)
    );
    assert_eq!(
        run(&ret(&op1(&[0x99], &string("123"), NULL))),
        Value::Integer(123)
    );
    assert_eq!(
        run(&ret(&op1(&[0x99], &buffer(&[0x34, 0x12]), NULL))),
        Value::Integer(0x1234)
    );
    assert_eq!(
        run(&ret(&op1(&[0x96], &string("ab"), NULL))),
        Value::Buffer(std::vec![b'a', b'b', 0])
    );
    assert_eq!(
        run(&ret(&op1(&[0x96], &int(0x0102), NULL))),
        Value::Buffer(std::vec![2, 1, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(run(&ret(&op1(&[0x98], &int(0x1f), NULL))), s("0x1F"));
    assert_eq!(run(&ret(&op1(&[0x98], &int(0), NULL))), s("0x0"));
    assert_eq!(
        run(&ret(&op1(&[0x98], &buffer(&[1, 0xab]), NULL))),
        s("0x01,0xAB")
    );
    assert_eq!(run(&ret(&op1(&[0x97], &int(1234), NULL))), s("1234"));
    assert_eq!(
        run(&ret(&op1(&[0x97], &buffer(&[1, 200]), NULL))),
        s("1,200")
    );
    // ToString stops at a NUL or the length.
    let ts = |b: &[u8], len: &[u8]| ret(&cat(&[&[0x9c], &buffer(b), len, NULL]));
    assert_eq!(run(&ts(b"abc\0de", &[0xff])), s("abc"));
    assert_eq!(run(&ts(b"abcde", &int(2))), s("ab"));
}

#[test]
fn concatenation_and_mid() {
    let s = |v: &str| Value::String(v.into());
    assert_eq!(
        run(&ret(&op2(0x73, &string("ab"), &string("cd"), NULL))),
        s("abcd")
    );
    // An integer appended to a string is fixed-width hex.
    assert_eq!(
        run(&ret(&op2(0x73, &string("v"), &int(0x1f), NULL))),
        s("v000000000000001F")
    );
    let (mut i, mut h) = load_rev(
        &method("CAT", 0, &ret(&op2(0x73, &string("v"), &int(0x1f), NULL))),
        1,
    );
    assert_eq!(eval(&mut i, &mut h, "\\CAT", Vec::new()), s("v0000001F"));
    // A buffer converts to space-separated hex bytes.
    assert_eq!(
        run(&ret(&op2(0x73, &string("b:"), &buffer(&[1, 2]), NULL))),
        s("b:0x01 0x02")
    );
    // Integers concatenate to a buffer of both.
    assert_eq!(
        run(&ret(&op2(0x73, &int(1), &int(2), NULL))),
        Value::Buffer(std::vec![1, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0])
    );
    assert_eq!(
        run(&ret(&op2(0x73, &buffer(&[9]), &string("a"), NULL))),
        Value::Buffer(std::vec![9, b'a', 0])
    );
    // Mid clips to the source.
    let mid = |src: &[u8], i: u64, n: u64| ret(&cat(&[&[0x9e], src, &int(i), &int(n), NULL]));
    assert_eq!(run(&mid(&string("abcdef"), 2, 3)), s("cde"));
    assert_eq!(run(&mid(&string("abc"), 2, 10)), s("c"));
    assert_eq!(run(&mid(&string("abc"), 5, 1)), s(""));
    assert_eq!(
        run(&mid(&buffer(&[1, 2, 3, 4]), 1, 2)),
        Value::Buffer(std::vec![2, 3])
    );
    // ConcatenateResTemplate drops the first end tag.
    let a = buffer(&[0x22, 0x01, 0x00, 0x79, 0x00]);
    let b = buffer(&[0x47, 1, 0xf8, 3, 0xf8, 3, 1, 8, 0x79, 0x00]);
    assert_eq!(
        run(&ret(&op2(0x84, &a, &b, NULL))),
        Value::Buffer(std::vec![
            0x22, 1, 0, 0x47, 1, 0xf8, 3, 0xf8, 3, 1, 8, 0x79, 0
        ])
    );
    assert_eq!(
        run_err(&ret(&op2(0x84, &buffer(&[0x22, 1, 0]), &b, NULL))),
        Error::TypeMismatch
    );
}

#[test]
fn index_sizeof_derefof_match() {
    let aml = cat(&[
        &name_op("PKG", &package(&[int(10), string("s"), int(30)])),
        &name_op("BUF", &buffer(&[1, 2, 3])),
        &name_op("STR", &string("hey")),
        &method("SZ", 1, &ret(&cat(&[&[0x87], &[ARG0]]))),
        &method(
            "IDX",
            0,
            &ret(&op2(
                0x72,
                &cat(&[&[0x83], &op2(0x88, &name("PKG"), &int(2), NULL)]),
                &cat(&[&[0x83], &op2(0x88, &name("BUF"), &int(1), NULL)]),
                NULL,
            )),
        ),
        // Index (PKG, 0) = 99; Index (BUF, 2) = 0x1FF (low byte kept);
        // Index (STR, 0) = 0x48 ('H').
        &method(
            "WRT",
            0,
            &cat(&[
                &store(&int(99), &op2(0x88, &name("PKG"), &int(0), NULL)),
                &store(&int(0x1ff), &op2(0x88, &name("BUF"), &int(2), NULL)),
                &store(&int(0x48), &op2(0x88, &name("STR"), &int(0), NULL)),
            ]),
        ),
        // Index into a local copy, stored through a reference in Local1.
        &method(
            "LOC",
            0,
            &cat(&[
                &store(&name("PKG"), L0),
                &op2(0x88, L0, &int(1), L1),
                &store(&int(5), &cat(&[&[0x83], L1])),
                &ret(&cat(&[&[0x83], &op2(0x88, L0, &int(1), NULL)])),
            ]),
        ),
        &method("OOB", 0, &ret(&op2(0x88, &name("PKG"), &int(3), NULL))),
        // Match (PKG, MEQ, 30, MTR, 0, 0) and a miss.
        &method(
            "MAT",
            1,
            &ret(&cat(&[
                &[0x89],
                &name("PKG"),
                &[1],
                &[ARG0],
                &[0],
                &int(0),
                &int(0),
            ])),
        ),
    ]);
    let (mut i, mut h) = load(&aml);
    let sz = |i: &mut crate::Interpreter, h: &mut FakeHost, v: Value| {
        int_of(eval(i, h, "\\SZ", std::vec![v]))
    };
    assert_eq!(sz(&mut i, &mut h, Value::String("four".into())), 4);
    assert_eq!(sz(&mut i, &mut h, Value::Buffer(std::vec![0; 7])), 7);
    assert_eq!(
        sz(&mut i, &mut h, Value::Package(std::vec![Value::Integer(1)])),
        1
    );
    assert_eq!(eval_int(&mut i, &mut h, "\\IDX"), 32);
    eval(&mut i, &mut h, "\\WRT", Vec::new());
    let Value::Package(p) = eval(&mut i, &mut h, "\\PKG", Vec::new()) else {
        panic!()
    };
    assert_eq!(p[0], Value::Integer(99));
    assert_eq!(
        eval(&mut i, &mut h, "\\BUF", Vec::new()),
        Value::Buffer(std::vec![1, 2, 0xff])
    );
    assert_eq!(
        eval(&mut i, &mut h, "\\STR", Vec::new()),
        Value::String("Hey".into())
    );
    assert_eq!(eval_int(&mut i, &mut h, "\\LOC"), 5);
    // The named package was not changed through the local copy.
    let Value::Package(p) = eval(&mut i, &mut h, "\\PKG", Vec::new()) else {
        panic!()
    };
    assert_eq!(p[1], Value::String("s".into()));
    assert_eq!(
        i.evaluate_path("\\OOB", Vec::new(), &mut h),
        Err(Error::IndexOutOfBounds)
    );
    assert_eq!(
        int_of(eval(&mut i, &mut h, "\\MAT", std::vec![Value::Integer(30)])),
        2
    );
    assert_eq!(
        int_of(eval(&mut i, &mut h, "\\MAT", std::vec![Value::Integer(31)])),
        u64::MAX
    );
}

#[test]
fn references_and_object_type() {
    let aml = cat(&[
        &name_op("INT", &int(5)),
        &device("DEV", &[]),
        &method("SET", 1, &store(&int(9), &[ARG0])),
        &method(
            "TST",
            0,
            &cat(&[
                // SET (RefOf (INT)) stores through the reference.
                &cat(&[&name("SET"), &[0x71], &name("INT")]),
                &ret(&name("INT")),
            ]),
        ),
        &method(
            "TYP",
            0,
            // ObjectType (INT) + ObjectType (DEV) * 0x100.
            &ret(&cat(&[
                &[0x72],
                &cat(&[&[0x8e], &name("INT")]),
                &op2(0x77, &cat(&[&[0x8e], &name("DEV")]), &int(0x100), NULL),
                NULL,
            ])),
        ),
        &method("MTY", 0, &ret(&cat(&[&[0x8e], &name("SET")]))),
        &method(
            "CRF",
            0,
            &cat(&[
                &if_(&cat(&[&[0x5b, 0x12], &name("NONE"), L0]), &ret(&int(1))),
                &if_(
                    &cat(&[&[0x5b, 0x12], &name("INT"), L0]),
                    &ret(&cat(&[&[0x83], L0])),
                ),
                &ret(&int(2)),
            ]),
        ),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\TST"), 9);
    assert_eq!(eval_int(&mut i, &mut h, "\\TYP"), 1 + 6 * 0x100);
    assert_eq!(eval_int(&mut i, &mut h, "\\MTY"), 8);
    assert_eq!(eval_int(&mut i, &mut h, "\\CRF"), 9);
}

#[test]
fn store_converts_to_the_target_type() {
    let aml = cat(&[
        &name_op("INT", &int(5)),
        &name_op("STR", &string("abc")),
        &name_op("BUF", &buffer(&[0xaa; 4])),
        &name_op("EMP", &pkg(&[0x11], &int(0))),
        &name_op("OBJ", &int(1)),
        &method("S", 2, &[]),
    ]);
    let (mut i, mut h) = load(&aml);
    let check = |src: &[u8], target: &str, want: Value| {
        let mut i2 = crate::Interpreter::new(test_config());
        let mut h2 = FakeHost::default();
        let aml2 = cat(&[&aml, &method("DO", 0, &store(src, &name(target)))]);
        i2.load_table(&table(b"DSDT", 2, &aml2), &mut h2).unwrap();
        eval(&mut i2, &mut h2, "\\DO", Vec::new());
        assert_eq!(
            eval(&mut i2, &mut h2, &alloc::format!("\\{target}"), Vec::new()),
            want
        );
    };
    check(&string("1F"), "INT", Value::Integer(0x1f));
    check(&buffer(&[1, 2]), "INT", Value::Integer(0x0201));
    check(&int(0x1f), "STR", Value::String("000000000000001F".into()));
    check(&buffer(&[1, 2]), "STR", Value::String("0x01 0x02".into()));
    check(&int(0x0102), "BUF", Value::Buffer(std::vec![2, 1, 0, 0]));
    check(
        &buffer(&[1, 2, 3, 4, 5, 6]),
        "BUF",
        Value::Buffer(std::vec![1, 2, 3, 4]),
    );
    check(
        &string("ab"),
        "BUF",
        Value::Buffer(std::vec![b'a', b'b', 0, 0]),
    );
    // An empty buffer takes the source's length.
    check(
        &buffer(&[7, 8, 9]),
        "EMP",
        Value::Buffer(std::vec![7, 8, 9]),
    );
    // A package cannot become an integer.
    let e = {
        let aml2 = cat(&[&aml, &method("DO", 0, &store(&package(&[]), &name("INT")))]);
        let (mut i2, mut h2) = load(&aml2);
        i2.evaluate_path("\\DO", Vec::new(), &mut h2).unwrap_err()
    };
    assert_eq!(e, Error::TypeMismatch);
    // CopyObject replaces the type; Store to a local replaces it too.
    let aml2 = cat(&[
        &aml,
        &method("CPY", 0, &cat(&[&[0x9d], &string("now"), &name("OBJ")])),
        &method(
            "LCL",
            0,
            &cat(&[&store(&int(1), L0), &store(&string("x"), L0), &ret(L0)]),
        ),
    ]);
    let (mut i2, mut h2) = load(&aml2);
    eval(&mut i2, &mut h2, "\\CPY", Vec::new());
    assert_eq!(
        eval(&mut i2, &mut h2, "\\OBJ", Vec::new()),
        Value::String("now".into())
    );
    assert_eq!(
        eval(&mut i2, &mut h2, "\\LCL", Vec::new()),
        Value::String("x".into())
    );
    let _ = (&mut i, &mut h);
}

#[test]
fn debug_object_goes_to_the_host() {
    let (mut i, mut h) = load(&method(
        "DBG",
        0,
        &cat(&[
            &store(&string("hello"), &[0x5b, 0x31]),
            &store(&int(3), &[0x5b, 0x31]),
        ]),
    ));
    eval(&mut i, &mut h, "\\DBG", Vec::new());
    assert_eq!(h.debug, [Value::String("hello".into()), Value::Integer(3)]);
}
