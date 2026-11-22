use super::*;

const START: &str = r#"{"t":"start","run":"0123456789abcdef0123456789abcdef","row":"sink","role":"run","build":"0123456789ab","process":{"pid":7,"created":11},"commit":"fedcba987654","tag":7,"cut":1073741831,"args":["--build","0123456789ab"],"set":null}"#;
const SEAL: &str = r#"{"t":"seal","run":"0123456789abcdef0123456789abcdef","role":"run","path":"link.jsonl","sha256":"00"}"#;
const END: &str = r#"{"t":"end","exit":0,"errors":[]}"#;

/// The lines joined, each ending in a line feed.
fn file(lines: &[&str]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| format!("{l}\n").into_bytes())
        .collect()
}

fn read_run(bytes: &[u8]) -> Result<Stream<Run>, Error> {
    read::<Run>(bytes)
}

fn record_error(bytes: &[u8]) -> String {
    match read_run(bytes) {
        Err(Error::Record { error, .. }) => error,
        other => panic!("not a record error: {other:?}"),
    }
}

#[test]
fn a_complete_stream_reads() {
    let s = read_run(&file(&[START, SEAL, END])).unwrap();
    assert_eq!(s.records.len(), 3);
    assert!(s.ended && s.dropped.is_none());
    assert_eq!(s.finished(false), Ok(()));
    let Run::Start(start) = &s.records[0] else {
        panic!("{s:?}")
    };
    assert_eq!(
        start.process,
        Process {
            pid: 7,
            created: 11
        }
    );
    assert_eq!(start.set, None);
}

#[test]
fn invalid_utf8_fails() {
    let mut bytes = file(&[START]);
    bytes.extend_from_slice(b"{\"t\":\"seal\",\"run\":\"\xff\"}\n");
    assert_eq!(read_run(&bytes), Err(Error::Utf8 { line: 2 }));
}

#[test]
fn nan_fails() {
    let line = r#"{"t":"end","exit":NaN,"errors":[]}"#;
    assert!(record_error(&file(&[START, line])).contains("expected value"));
    let inf = r#"{"t":"end","exit":Infinity,"errors":[]}"#;
    assert!(record_error(&file(&[START, inf])).contains("expected value"));
}

#[test]
fn an_out_of_range_number_fails() {
    let huge = r#"{"t":"end","exit":1e999,"errors":[]}"#;
    assert!(record_error(&file(&[START, huge])).contains("out of range"));
    let wide = r#"{"t":"end","exit":4294967296,"errors":[]}"#;
    assert!(record_error(&file(&[START, wide])).contains("invalid value"));
    let fraction = r#"{"t":"end","exit":0.5,"errors":[]}"#;
    assert!(record_error(&file(&[START, fraction])).contains("invalid type"));
}

#[test]
fn a_duplicate_key_fails() {
    let twice = r#"{"t":"end","exit":0,"exit":1,"errors":[]}"#;
    assert!(record_error(&file(&[START, twice])).contains("duplicate field"));
    let tag = r#"{"t":"end","t":"end","exit":0,"errors":[]}"#;
    assert!(record_error(&file(&[START, tag])).contains("duplicate field"));
    let nested = START.replace(r#""pid":7"#, r#""pid":7,"pid":8"#);
    assert!(record_error(&file(&[&nested])).contains("duplicate field"));
}

#[test]
fn a_malformed_middle_line_fails() {
    let bytes = file(&[START, r#"{"t":"seal","#, END]);
    assert!(matches!(
        read_run(&bytes),
        Err(Error::Record { line: 2, .. })
    ));
    assert_eq!(
        read_run(&file(&[START, "", END])),
        Err(Error::Framing { line: 2 })
    );
    let crlf = format!("{START}\n{SEAL}\r\n{END}\n");
    assert_eq!(read_run(crlf.as_bytes()), Err(Error::Framing { line: 2 }));
    let two = format!("{SEAL} {SEAL}");
    assert!(matches!(
        read_run(&file(&[START, &two])),
        Err(Error::Record { line: 2, .. })
    ));
}

#[test]
fn an_unterminated_last_line_fails_unless_its_writer_was_killed() {
    let mut bytes = file(&[START, SEAL]);
    bytes.extend_from_slice(br#"{"t":"seal","run":"01"#);
    let s = read_run(&bytes).unwrap();
    assert_eq!(s.records.len(), 2);
    assert_eq!(s.dropped.as_deref(), Some(r#"{"t":"seal","run":"01"#));
    assert_eq!(
        s.finished(false),
        Err(Error::Unfinished {
            ended: false,
            dropped: true
        })
    );
    assert_eq!(s.finished(true), Ok(()));
    // Even a whole record is dropped without its line feed, after an end
    // as anywhere else.
    let mut ended = file(&[START, END]);
    ended.extend_from_slice(SEAL.as_bytes());
    let s = read_run(&ended).unwrap();
    assert!(s.ended && s.dropped.is_some());
    assert!(s.finished(false).is_err());
    // Nothing follows an end, even from a writer cut short.
    assert_eq!(
        s.finished(true),
        Err(Error::Unfinished {
            ended: true,
            dropped: true
        })
    );
}

#[test]
fn an_unknown_field_fails() {
    let extra = r#"{"t":"end","exit":0,"errors":[],"note":"x"}"#;
    assert!(record_error(&file(&[START, extra])).contains("unknown field"));
    let nested = START.replace(r#""created":11"#, r#""created":11,"user":"x""#);
    assert!(record_error(&file(&[&nested])).contains("unknown field"));
    let kind = r#"{"t":"note","q":1}"#;
    assert!(record_error(&file(&[START, kind])).contains("unknown variant"));
    let missing = r#"{"t":"end","exit":0}"#;
    assert!(record_error(&file(&[START, missing])).contains("missing field"));
}

#[test]
fn a_second_first_record_fails() {
    assert_eq!(
        read_run(&file(&[START, SEAL, START, END])),
        Err(Error::SecondFirst { line: 3 })
    );
    assert_eq!(
        read_run(&file(&[SEAL, START, END])),
        Err(Error::First { want: "start" })
    );
    assert_eq!(read_run(b""), Err(Error::First { want: "start" }));
}

#[test]
fn a_second_end_record_fails() {
    assert_eq!(
        read_run(&file(&[START, END, END])),
        Err(Error::SecondEnd { line: 3 })
    );
}

#[test]
fn a_record_after_end_fails() {
    assert_eq!(
        read_run(&file(&[START, END, SEAL])),
        Err(Error::AfterEnd { line: 3 })
    );
}

#[test]
fn a_killed_stream_needs_its_observed_exit() {
    let s = read_run(&file(&[START, SEAL])).unwrap();
    assert!(!s.ended && s.dropped.is_none());
    assert_eq!(
        s.finished(false),
        Err(Error::Unfinished {
            ended: false,
            dropped: false
        })
    );
    assert_eq!(s.finished(true), Ok(()));
}

#[test]
fn the_link_stream_reads_what_the_link_writes() {
    let lines = [
        r#"{"com":"COM4","freq":10000000,"location":"PCIROOT(0)#USBROOT(0)#USB(12)","path":"\\\\?\\USB#VID_1209&PID_0001&MI_02#x","process":{"created":11,"pid":7},"q":5,"run":"0123456789abcdef0123456789abcdef","t":"open"}"#,
        r#"{"line":"hello 2 build 0123456789ab mode 24","q":6,"t":"ans"}"#,
        r#"{"line":"ping 1","q":7,"t":"cmd"}"#,
        r#"{"cmd":"ping 1","line":"pong 1 99","q":9,"sent":7,"t":"ans"}"#,
        r#"{"device":99,"n":1,"recv":9,"sent":7,"t":"ping"}"#,
        r#"{"line":"F 1 0","q":10,"t":"rec"}"#,
        r#"{"case":"tab","q":11,"t":"malformed"}"#,
        r#"{"error":"the port failed","q":12,"t":"lost"}"#,
        r#"{"problems":["a ping went unanswered"],"q":13,"t":"end"}"#,
    ];
    let s = read::<Link>(&file(&lines)).unwrap();
    assert_eq!(s.records.len(), 9);
    assert!(s.ended);
    let refused = [
        lines[0],
        r#"{"error":"another build","q":6,"t":"refused"}"#,
        r#"{"problems":[],"q":7,"t":"end"}"#,
    ];
    assert!(read::<Link>(&file(&refused)).is_ok());
}

#[test]
fn a_reservation_holds_one_record() {
    let reserved = r#"{"t":"reserved","set":"s","row":"p1-compare","build":"0123456789ab","roles":[{"role":"A1","row":"p1","run":"r","dir":"A1"}]}"#;
    let s = read::<Reservation>(&file(&[reserved])).unwrap();
    assert!(s.ended);
    assert_eq!(
        read::<Reservation>(&file(&[reserved, reserved])),
        Err(Error::SecondEnd { line: 2 })
    );
}

#[test]
fn verdict_lines_read_strictly() {
    let line = r#"{"row":"sink","run":"r","check":"provenance","outcome":"pass","detail":"","inputs":[{"role":"run","path":"link.jsonl","sha256":"00"}],"producer":{"name":"judge","version":"0123456789ab"}}"#;
    let s = read::<Verdict>(&file(&[line, line])).unwrap();
    assert_eq!(s.records.len(), 2);
    assert_eq!(s.records[0].outcome, Outcome::Pass);
    assert_eq!(s.finished(false), Ok(()));
    let other = line.replace(r#""pass""#, r#""passed""#);
    assert!(matches!(
        read::<Verdict>(&file(&[&other])),
        Err(Error::Record { line: 1, .. })
    ));
    let mut cut = file(&[line]);
    cut.extend_from_slice(line.as_bytes());
    assert!(read::<Verdict>(&cut).unwrap().finished(false).is_err());
}

/// The runner's children: started, terminated, observed ended.
#[test]
fn the_runner_s_children_read() {
    let spawn = r#"{"t":"spawn","q":1,"name":"recorder","process":{"pid":8,"created":12}}"#;
    let kill =
        r#"{"t":"kill","q":2,"name":"recorder","process":{"pid":8,"created":12},"requested":1}"#;
    let exit = r#"{"t":"exit","q":3,"name":"recorder","process":{"pid":8,"created":12},"code":1073741831}"#;
    let s = read_run(&file(&[START, spawn, kill, exit, END])).unwrap();
    assert_eq!(s.records.len(), 5);
    let Run::Start(start) = &s.records[0] else {
        panic!("{s:?}")
    };
    assert_eq!(start.cut, 0x4000_0007);
    assert_eq!(s.records[3].q(), Some(3));
    // An exit says nothing of who ended it: its code does.
    let killed = exit.replace(r#""code":1073741831"#, r#""code":0,"killed":true"#);
    assert!(record_error(&file(&[START, &killed])).contains("unknown field"));
}

/// Every stream's first record names its run and its writer.
#[test]
fn a_stream_s_first_record_names_its_run_and_writer() {
    let ready = r#"{"t":"ready","q":1,"run":"0123456789abcdef0123456789abcdef","process":{"pid":8,"created":12},"window":5,"location":"L","keyboard":null,"pad":null}"#;
    let end = r#"{"t":"end","q":2}"#;
    assert!(read::<Recorder>(&file(&[ready, end])).is_ok());
    let anonymous = ready.replace(r#""run":"0123456789abcdef0123456789abcdef","#, "");
    assert!(matches!(
        read::<Recorder>(&file(&[&anonymous, end])),
        Err(Error::Record { line: 1, .. })
    ));
}

/// JSON whitespace may lead a record; anything else than an object may not.
#[test]
fn whitespace_may_lead_a_record() {
    let spaced = format!(" \t{END}");
    assert!(read_run(&file(&[START, &spaced])).unwrap().ended);
}

#[test]
fn a_positional_array_fails() {
    let array = r#"["end",0,[]]"#;
    assert_eq!(
        read_run(&file(&[START, array])),
        Err(Error::Record {
            line: 2,
            error: "the line is not a JSON object".into()
        })
    );
    let verdict = r#"["sink","r","provenance","pass","",[],["judge","0"]]"#;
    assert!(matches!(
        read::<Verdict>(&file(&[verdict])),
        Err(Error::Record { line: 1, .. })
    ));
}
