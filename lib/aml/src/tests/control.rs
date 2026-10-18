// SPDX-License-Identifier: BSD-2-Clause
//! Control flow, methods, bounds on hostile AML, synchronization, and the
//! host hooks.

extern crate std;

use alloc::vec::Vec;

use super::util::*;
use crate::{AmlPath, Config, Error, Interpreter, Limits, Value};

const L0: &[u8] = &[LOCAL0];
const L1: &[u8] = &[LOCAL0 + 1];
const NULL: &[u8] = &[0];

fn lequal(a: &[u8], b: &[u8]) -> Vec<u8> {
    cat(&[&[0x93], a, b])
}

fn lless(a: &[u8], b: &[u8]) -> Vec<u8> {
    cat(&[&[0x95], a, b])
}

fn add(a: &[u8], b: &[u8], t: &[u8]) -> Vec<u8> {
    cat(&[&[0x72], a, b, t])
}

#[test]
fn if_else_chains() {
    let body = cat(&[
        &if_(&lequal(&[ARG0], &int(1)), &ret(&int(10))),
        &else_(&cat(&[
            &if_(&lequal(&[ARG0], &int(2)), &ret(&int(20))),
            &else_(&ret(&int(30))),
        ])),
    ]);
    let (mut i, mut h) = load(&method("SEL", 1, &body));
    for (arg, want) in [(1, 10), (2, 20), (3, 30)] {
        let v = eval(&mut i, &mut h, "\\SEL", std::vec![Value::Integer(arg)]);
        assert_eq!(v, Value::Integer(want));
    }
}

#[test]
fn while_break_continue_return() {
    // i = 0; sum = 0; while (i < 10) { i++; if (i == 3) continue;
    // if (i == 8) break; sum += i }  =>  1+2+4+5+6+7 = 25
    let body = cat(&[
        &store(&int(0), L0),
        &store(&int(0), L1),
        &while_(
            &lless(L0, &int(10)),
            &cat(&[
                &[0x75, LOCAL0],
                &if_(&lequal(L0, &int(3)), &[0x9f]),
                &if_(&lequal(L0, &int(8)), &[0xa5]),
                &add(L1, L0, L1),
            ]),
        ),
        &ret(L1),
    ]);
    assert_eq!(run(&body), Value::Integer(25));
    // Return from inside a loop.
    let body = cat(&[
        &store(&int(0), L0),
        &while_(
            &[0x01],
            &cat(&[&[0x75, LOCAL0], &if_(&lequal(L0, &int(5)), &ret(L0))]),
        ),
    ]);
    assert_eq!(run(&body), Value::Integer(5));
    // Break outside a loop is an error.
    assert!(matches!(run_err(&[0xa5]), Error::InvalidOpcode(_)));
}

#[test]
fn loops_are_bounded() {
    let config = Config {
        limits: Limits {
            max_loop_iterations: 1000,
            ..test_config().limits
        },
        ..test_config()
    };
    let (mut i, mut h) = load_cfg(&method("SPIN", 0, &while_(&[0x01], &[])), 2, config);
    assert_eq!(
        i.evaluate_path("\\SPIN", Vec::new(), &mut h),
        Err(Error::LoopLimit)
    );
    // The default limit stops an endless loop too.
    let (mut i, mut h) = load(&method("SPIN", 0, &while_(&[0x01], &[0xa3])));
    assert_eq!(
        i.evaluate_path("\\SPIN", Vec::new(), &mut h),
        Err(Error::LoopLimit)
    );
    // With a running clock, the time limit (30 s) applies first: each
    // timer read advances 1 s.
    let (mut i, mut h) = load(&method("SPIN", 0, &while_(&[0x01], &[0xa3])));
    h.clock = 1;
    h.clock_step = 10_000_000;
    assert_eq!(
        i.evaluate_path("\\SPIN", Vec::new(), &mut h),
        Err(Error::LoopLimit)
    );
    assert!(h.clock < 40 * 10_000_000, "{}", h.clock);
}

#[test]
fn recursion_and_nesting_are_bounded() {
    // Unbounded recursion.
    let (mut i, mut h) = load(&method("RECU", 1, &ret(&cat(&[&name("RECU"), &[ARG0]]))));
    let e = i
        .evaluate_path("\\RECU", std::vec![Value::Integer(0)], &mut h)
        .unwrap_err();
    assert!(matches!(e, Error::CallDepth | Error::DepthLimit), "{e:?}");
    // Bounded recursion works: count down from 10.
    let body = cat(&[
        &if_(&lequal(&[ARG0], &int(0)), &ret(&int(0))),
        &ret(&add(
            &int(1),
            &cat(&[&name("DOWN"), &cat(&[&[0x74], &[ARG0], &int(1), NULL])]),
            NULL,
        )),
    ]);
    let (mut i, mut h) = load(&method("DOWN", 1, &body));
    assert_eq!(
        eval(&mut i, &mut h, "\\DOWN", std::vec![Value::Integer(10)]),
        Value::Integer(10)
    );
    // Expressions nested thousands deep fail cleanly.
    let mut e = int(1);
    for _ in 0..5000 {
        e = add(&e, &int(1), NULL);
    }
    assert_eq!(run_err(&ret(&e)), Error::DepthLimit);
    // So do scopes and packages nested in the table itself.
    let mut p = int(1);
    for _ in 0..3000 {
        p = package(&[p]);
    }
    let mut d = Vec::new();
    for _ in 0..3000 {
        d = device("D", &d);
    }
    for aml in [name_op("DEEP", &p), d] {
        let mut i = Interpreter::new(test_config());
        let mut h = FakeHost::default();
        let r = i.load_table(&table(b"DSDT", 2, &aml), &mut h).unwrap();
        assert!(
            r.warnings.iter().any(|w| w.error == Error::DepthLimit),
            "{:?}",
            r.warnings
        );
    }
}

/// Stack bytes per nesting level for each hostile shape, measured from the
/// probe `enter` keeps in test builds (on the normal test thread, so the
/// measurement itself cannot overflow).
fn stack_per_level() -> Vec<(&'static str, usize, u32)> {
    let mut out = Vec::new();
    for (what, body, args) in hostile() {
        let (mut i, mut h) = load(&method("TST", args, &body));
        let a = (0..args).map(|_| Value::Integer(0)).collect();
        let e = i.evaluate_path("\\TST", a, &mut h).unwrap_err();
        assert!(
            matches!(e, Error::DepthLimit | Error::CallDepth),
            "{what}: {e:?}"
        );
        let p = &i.stack_probe;
        let n = p.len();
        let per = (p[0] - p[n - 1]) / (n - 1);
        out.push((what, per, i.peak_depth()));
    }
    out
}

/// The hostile shapes, as method bodies with their argument counts.
fn hostile() -> Vec<(&'static str, Vec<u8>, u8)> {
    let mut deep_add = int(1);
    for _ in 0..2000 {
        deep_add = add(&deep_add, &int(1), NULL);
    }
    let mut deep_if = int(1);
    for _ in 0..2000 {
        deep_if = if_(&int(1), &deep_if);
    }
    let mut deep_deref = Vec::new();
    for _ in 0..2000 {
        deep_deref = cat(&[&[0x83], &deep_deref]);
    }
    deep_deref.push(LOCAL0);
    let mut recurse = ret(&cat(&[&name("TST"), &[0x01]]));
    for _ in 0..3 {
        recurse = if_(&[0x01], &recurse);
    }
    std::vec![
        ("nested Add", ret(&deep_add), 0),
        ("nested If", deep_if, 0),
        ("nested DerefOf", ret(&deep_deref), 0),
        ("recursive calls", recurse, 1),
    ]
}

/// With the default limits, every hostile shape stops with
/// `DepthLimit` once the measured stack use reaches the budget (10 KiB,
/// sized for a 16 KiB kernel thread stack, `kernel/src/threads.rs`).
#[test]
fn default_limits_stop_at_the_stack_budget() {
    let limits = Limits::default();
    for (what, body, args) in hostile() {
        let (mut i, mut h) = load_cfg(&method("TST", args, &body), 2, Config::default());
        let a = (0..args).map(|_| Value::Integer(0)).collect();
        let e = i.evaluate_path("\\TST", a, &mut h).unwrap_err();
        assert!(
            matches!(e, Error::DepthLimit | Error::CallDepth),
            "{what}: {e:?}"
        );
        std::println!(
            "{what}: stopped at depth {} with {} bytes of stack",
            i.peak_depth(),
            i.peak_stack()
        );
        assert!(i.peak_stack() <= limits.stack_budget + 1024, "{what}");
    }
    assert_eq!(
        Limits::for_stack(64 * 1024).stack_budget,
        64 * 1024 - Limits::STACK_HEADROOM
    );
}

/// The hostile shapes on a thread with a kernel-sized 16 KiB stack get the
/// error back rather than overflowing.
#[test]
fn hostile_nesting_on_a_kernel_sized_stack() {
    let cases = hostile();
    let t = std::thread::Builder::new()
        .stack_size(16 * 1024)
        .spawn(move || {
            for (what, body, args) in cases {
                let mut i = Interpreter::new(Config::default());
                let mut h = FakeHost::default();
                let t = table(b"DSDT", 2, &method("TST", args, &body));
                i.load_table(&t, &mut h).unwrap();
                let a = (0..args).map(|_| Value::Integer(0)).collect();
                let e = i.evaluate_path("\\TST", a, &mut h);
                assert!(
                    matches!(e, Err(Error::DepthLimit | Error::CallDepth)),
                    "{what}: {e:?}"
                );
            }
        })
        .unwrap();
    t.join().unwrap();
}

/// Prints the stack a nesting level costs in this build.
#[test]
#[ignore = "prints frame sizes"]
fn stack_per_level_report() {
    for (what, per, peak) in stack_per_level() {
        std::println!("{what}: {per} bytes/level (depth {peak})");
    }
}

/// Loops that wrap a value in ever deeper packages or references stop at
/// the nesting bound: such values are dropped and cloned recursively.
#[test]
fn value_nesting_is_bounded() {
    let wrap_by_index = cat(&[
        &store(&package(&[int(0)]), L0),
        &while_(
            &[0x01],
            &cat(&[
                &store(&package(&[int(0)]), L1),
                &store(L0, &cat(&[&[0x88], L1, &int(0), NULL])),
                &store(L1, L0),
            ]),
        ),
    ]);
    let wrap_literal = while_(&[0x01], &store(&pkg(&[0x12], &cat(&[&[1], L0])), L0));
    let wrap_reference = cat(&[
        &store(&package(&[int(0)]), L0),
        &while_(
            &[0x01],
            &store(
                &cat(&[&[0x88], &package(&[L0.to_vec()]), &int(0), NULL]),
                L0,
            ),
        ),
    ]);
    for body in [wrap_by_index, wrap_literal, wrap_reference] {
        assert_eq!(run_err(&body), Error::DepthLimit);
    }
}

#[test]
#[ignore = "prints type sizes"]
fn sizes() {
    use core::mem::size_of;
    std::println!(
        "Value {} Error {} Result {} Location {} NameString {} Cursor {}",
        size_of::<Value>(),
        size_of::<Error>(),
        size_of::<Result<Value, Error>>(),
        size_of::<crate::value::Location>(),
        size_of::<crate::NameString>(),
        size_of::<crate::stream::Cursor>()
    );
}

#[test]
fn implicit_return_and_slack() {
    // Slack mode (FreeBSD's default): the last value is returned and an
    // uninitialized local reads as 0.
    let aml = cat(&[
        &method("IMPL", 0, &add(&int(1), &int(2), L0)),
        &method("UNIN", 0, &ret(&add(L0, &int(1), NULL))),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\IMPL"), 3);
    assert_eq!(eval_int(&mut i, &mut h, "\\UNIN"), 1);
    let strict = Config {
        slack: false,
        ..test_config()
    };
    let (mut i, mut h) = load_cfg(&aml, 2, strict);
    assert_eq!(
        eval(&mut i, &mut h, "\\IMPL", Vec::new()),
        Value::Uninitialized
    );
    assert_eq!(
        i.evaluate_path("\\UNIN", Vec::new(), &mut h),
        Err(Error::Uninitialized)
    );
}

#[test]
fn arguments_and_locals() {
    // Seven arguments, summed through all eight locals.
    let mut body = Vec::new();
    for k in 0..7u8 {
        body.extend(store(&[ARG0 + k], &[LOCAL0 + k]));
    }
    body.extend(store(&int(100), &[LOCAL0 + 7]));
    let mut sum = alloc::vec![LOCAL0];
    for k in 1..8u8 {
        sum = add(&sum, &[LOCAL0 + k], NULL);
    }
    body.extend(ret(&sum));
    let (mut i, mut h) = load(&method("SUM", 7, &body));
    let args = (1..=7).map(Value::Integer).collect();
    assert_eq!(eval(&mut i, &mut h, "\\SUM", args), Value::Integer(128));
    // Missing arguments read as uninitialized (0 in slack mode); too many
    // through the API are refused.
    let args = std::vec![Value::Integer(1)];
    assert_eq!(eval(&mut i, &mut h, "\\SUM", args), Value::Integer(101));
    let args = (0..8).map(Value::Integer).collect();
    assert_eq!(
        i.evaluate_path("\\SUM", args, &mut h),
        Err(Error::TooManyArgs)
    );
}

#[test]
fn host_hooks() {
    let aml = cat(&[
        &device("\\_SB.PWRB", &[]),
        &method(
            "HOOK",
            0,
            &cat(&[
                &cat(&[&[0x86], &name("\\_SB.PWRB"), &int(0x80)]),
                &cat(&[&[0x5b, 0x22], &int(15)]),
                &cat(&[&[0x5b, 0x21], &int(50)]),
                &cat(&[&[0x5b, 0x32], &[7], &0x1234u32.to_le_bytes(), &int(9)]),
                &ret(&[0x5b, 0x33]),
            ]),
        ),
        &method("REV", 0, &ret(&[0x5b, 0x30])),
    ]);
    let (mut i, mut h) = load(&aml);
    h.clock = 777;
    assert_eq!(eval_int(&mut i, &mut h, "\\HOOK"), 777);
    let pwrb = i.lookup(&AmlPath::parse("\\_SB.PWRB").unwrap()).unwrap();
    assert_eq!(h.notifies, [(pwrb, 0x80)]);
    assert_eq!((h.sleeps, h.stalls), (15, 50));
    assert_eq!(h.fatals, [(7, 0x1234, 9)]);
    assert_eq!(eval_int(&mut i, &mut h, "\\REV"), test_config().revision);
}

#[test]
fn mutexes() {
    let acquire = |m: &str| cat(&[&[0x5b, 0x23], &name(m), &0xffffu16.to_le_bytes()]);
    let release = |m: &str| cat(&[&[0x5b, 0x27], &name(m)]);
    let aml = cat(&[
        &cat(&[&[0x5b, 0x01], &name("MHI"), &[5]]),
        &cat(&[&[0x5b, 0x01], &name("MLO"), &[3]]),
        // Recursive acquisition, then release twice: returns 0 (acquired).
        &method(
            "REC",
            0,
            &cat(&[
                &store(&acquire("MHI"), L0),
                &acquire("MHI"),
                &release("MHI"),
                &release("MHI"),
                &ret(L0),
            ]),
        ),
        // Acquiring a lower sync level while holding a higher one.
        &method("ORD", 0, &cat(&[&acquire("MHI"), &acquire("MLO")])),
        &method("UNO", 0, &release("MLO")),
        // Left held: released when the method returns.
        &method("LEAK", 0, &acquire("MHI")),
        &method("GL", 0, &cat(&[&acquire("\\_GL"), &release("\\_GL")])),
        // A serialized method at sync level 4 cannot take MLO (level 3).
        &method("SER", 8 | (4 << 4), &acquire("MLO")),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\REC"), 0);
    assert_eq!(
        i.evaluate_path("\\ORD", Vec::new(), &mut h),
        Err(Error::MutexOrder)
    );
    assert_eq!(
        i.evaluate_path("\\UNO", Vec::new(), &mut h),
        Err(Error::MutexNotOwned)
    );
    // ORD's error released MHI on the way out, so LEAK can take it twice.
    eval(&mut i, &mut h, "\\LEAK", Vec::new());
    eval(&mut i, &mut h, "\\LEAK", Vec::new());
    eval(&mut i, &mut h, "\\GL", Vec::new());
    assert_eq!((h.gl_acquires, h.gl_releases), (1, 1));
    assert_eq!(
        i.evaluate_path("\\SER", Vec::new(), &mut h),
        Err(Error::MutexOrder)
    );
}

#[test]
fn events() {
    let wait = |t: u64| cat(&[&[0x5b, 0x25], &name("EVT"), &int(t)]);
    let aml = cat(&[
        &cat(&[&[0x5b, 0x02], &name("EVT")]),
        &method("SIG", 0, &cat(&[&[0x5b, 0x24], &name("EVT")])),
        &method("RST", 0, &cat(&[&[0x5b, 0x26], &name("EVT")])),
        &method("WT", 0, &ret(&wait(10))),
    ]);
    let (mut i, mut h) = load(&aml);
    eval(&mut i, &mut h, "\\SIG", Vec::new());
    eval(&mut i, &mut h, "\\SIG", Vec::new());
    assert_eq!(eval_int(&mut i, &mut h, "\\WT"), 0);
    assert_eq!(eval_int(&mut i, &mut h, "\\WT"), 0);
    // No signal pending: the wait times out (after sleeping its timeout).
    assert_eq!(eval_int(&mut i, &mut h, "\\WT"), u64::MAX);
    assert_eq!(h.sleeps, 10);
    eval(&mut i, &mut h, "\\SIG", Vec::new());
    eval(&mut i, &mut h, "\\RST", Vec::new());
    assert_eq!(eval_int(&mut i, &mut h, "\\WT"), u64::MAX);
}

#[test]
fn dynamic_table_loading_is_unsupported() {
    assert_eq!(
        run_err(&cat(&[&[0x5b, 0x20], &name("XXXX"), L0])),
        Error::Unsupported("Load and Unload")
    );
    let lt = cat(&[
        &[0x5b, 0x1f],
        &string("SSDT"),
        &string(""),
        &string(""),
        &string(""),
        &string(""),
        &int(0),
    ]);
    assert_eq!(run_err(&ret(&lt)), Error::Unsupported("LoadTable"));
}

#[test]
fn module_level_code_runs_at_load() {
    // If at table level, with _OSI, decides what gets defined.
    let aml = cat(&[
        &name_op("OSYS", &int(0)),
        &if_(
            &cat(&[&name("\\_OSI"), &string("Windows 2015")]),
            &store(&int(2015), &name("OSYS")),
        ),
        &if_(
            &cat(&[&name("\\_OSI"), &string("Linux")]),
            &name_op("LINX", &int(1)),
        ),
        &else_(&name_op("NOLX", &int(1))),
    ]);
    let (mut i, mut h) = load(&aml);
    assert_eq!(eval_int(&mut i, &mut h, "\\OSYS"), 2015);
    assert!(i.lookup(&AmlPath::parse("\\LINX").unwrap()).is_none());
    assert!(i.lookup(&AmlPath::parse("\\NOLX").unwrap()).is_some());
    // The host can change the answers.
    let mut config = test_config();
    config.osi.push("Linux".into());
    let (i, _) = load_cfg(&aml, 2, config);
    assert!(i.lookup(&AmlPath::parse("\\LINX").unwrap()).is_some());
}
