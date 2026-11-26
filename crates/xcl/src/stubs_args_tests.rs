use super::*;

#[test]
fn ordinal_first_caller_arriving_late_still_records_as_first_and_last() {
    let (addr, info) = make_stub_info("args-race.dll", "ArgsRace", 9);
    let key = info as *const StubInfo as usize;
    let (resume_tx, arrived_rx) = arm_before_table_update(key);
    // Linux twin: the thunk speaks Win64 even on Linux, so exercise it with
    // `extern "win64"` (on Windows `extern "system"` is the same ABI).
    let f: extern "win64" fn(u64, u64, u64, u64) -> u64 =
        unsafe { std::mem::transmute(addr) };
    let handle = std::thread::spawn(move || f(0, 0, 0x0A01, 0x0A02));
    arrived_rx.recv().unwrap(); // A holds ordinal 0, paused before the table update
    let b = std::thread::spawn(move || f(0, 0, 0x0B01, 0x0B02));
    // Let B (ordinal 1) enter and update the table before A resumes.
    let mut tries = 0;
    while stub_args(info).is_none() {
        std::thread::sleep(std::time::Duration::from_millis(1));
        tries += 1;
        assert!(tries < 1000, "B never entered the side table");
    }
    resume_tx.send(()).unwrap();
    handle.join().unwrap();
    b.join().unwrap();
    let (fr8, fr9, lr8, lr9) = stub_args(info).expect("args recorded");
    // A (ordinal 0) is the first call even though it entered the table second
    assert_eq!((fr8, fr9), (0x0A01, 0x0A02));
    assert_eq!((lr8, lr9), (0x0B01, 0x0B02));
    assert_eq!(info.calls.load(Ordering::Relaxed), 2);
}

#[test]
fn thunk_records_first_and_last_r8_r9_args() {
    let (addr, info) = make_stub_info("args.dll", "ArgsRecord", 7);
    let f: extern "win64" fn(u64, u64, u64, u64) -> u64 =
        unsafe { std::mem::transmute(addr) };
    assert_eq!(f(0, 0, 0x1111, 0x2222), 7);
    assert_eq!(f(0, 0, 0x3333, 0x4444), 7);
    let (fr8, fr9, lr8, lr9) = stub_args(info).expect("args recorded");
    assert_eq!(fr8, 0x1111);
    assert_eq!(fr9, 0x2222);
    assert_eq!(lr8, 0x3333);
    assert_eq!(lr9, 0x4444);
    assert_eq!(info.calls.load(Ordering::Relaxed), 2);
}

#[test]
fn record_call_without_args_leaves_side_table_empty() {
    let info = new_info("args-noargs.dll", "NoArgs", 0);
    record_call(info, 0x1234);
    assert_eq!(info.calls.load(Ordering::Relaxed), 1);
    assert!(stub_args(info).is_none());
}

#[test]
fn stub_call_serializes_args_only_when_present() {
    let (addr, info) = make_stub_info("args-ser.dll", "ArgsSer", 1);
    let f: extern "win64" fn(u64, u64, u64, u64) -> u64 =
        unsafe { std::mem::transmute(addr) };
    f(0, 0, 0xabc, 0xdef);
    let (fr8, fr9, lr8, lr9) = stub_args(info).unwrap();
    let call = crate::report::StubCall {
        dll: info.dll.clone(),
        function: info.name.clone(),
        calls: info.calls.load(Ordering::Relaxed),
        first_caller: String::new(),
        first_args: Some(format!("{fr8:#x} {fr9:#x}")),
        last_args: Some(format!("{lr8:#x} {lr9:#x}")),
    };
    let s = serde_json::to_string(&call).unwrap();
    assert!(s.contains("first_args"));
    assert!(s.contains("last_args"));
    let call2 = crate::report::StubCall {
        dll: "d.dll".into(),
        function: "no-args".into(),
        calls: 1,
        first_caller: String::new(),
        first_args: None,
        last_args: None,
    };
    let s2 = serde_json::to_string(&call2).unwrap();
    assert!(!s2.contains("first_args"));
    assert!(!s2.contains("last_args"));
}
