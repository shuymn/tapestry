//! The media worker's control pipe: every message survives its encoding,
//! and the decoder refuses whatever is not exactly one message, since the
//! game reads the worker's replies as untrusted (design.md "Threat Model"
//! T02).

use skin_model::media::control::{
    Command, Failure, MAX_FRAME, MAX_TEXT, Malformed, Refusal, Reply, frame, frame_len,
};

fn commands() -> Vec<Command> {
    vec![
        Command::Open {
            player: 3,
            file: 0x0123_4567_89ab_cdef,
        },
        Command::Play {
            player: 250,
            section: 0x1f4,
            event: u64::MAX - 7,
        },
        Command::Seek {
            player: 1,
            run: 9,
            at: -1_500_000,
        },
        Command::Close { player: 77 },
        Command::Quit,
    ]
}

fn replies() -> Vec<Reply> {
    let mut all = vec![
        Reply::Hello {
            version: "n9.0.2-39-g7adfbe40dc-2".to_owned(),
            configuration: "--enable-shared --disable-static ÿ".to_owned(),
        },
        Reply::Probed {
            player: 2,
            width: 2_560,
            height: 1_080,
            duration: 15_000_000,
            codec: "h264".to_owned(),
            container: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
        },
        Reply::Playing { player: 5 },
        Reply::Sought {
            player: 6,
            run: 4,
            landed: 1_500_000,
            attempts: 2,
        },
        Reply::Ended { player: 7, run: 12 },
        Reply::Closed { player: 8 },
        Reply::Log {
            text: "ffmpeg: [h263 @ 0000] Codec (h263) not on whitelist".to_owned(),
        },
    ];
    for (i, why) in [
        Refusal::Read,
        Refusal::Container,
        Refusal::NoVideo,
        Refusal::Codec,
        Refusal::Size,
        Refusal::Decoder,
    ]
    .into_iter()
    .enumerate()
    {
        all.push(Reply::Refused {
            player: u8::try_from(i).unwrap() + 10,
            why,
            code: -1_094_995_529 - i32::try_from(i).unwrap(),
        });
    }
    for (i, why) in [
        Failure::Command,
        Failure::Ring,
        Failure::Read,
        Failure::Memory,
        Failure::Decode,
        Failure::Seek,
    ]
    .into_iter()
    .enumerate()
    {
        all.push(Reply::Failed {
            player: u8::try_from(i).unwrap() + 20,
            why,
            code: -12 - i32::try_from(i).unwrap(),
        });
    }
    all
}

// Covers: T02
#[test]
fn control_messages_round_trip() {
    for c in commands() {
        let bytes = c.encode();
        assert_eq!(Command::decode(&bytes), Ok(c.clone()), "{c:?}");
        let framed = frame(&bytes).unwrap();
        let len = frame_len([framed[0], framed[1]]).unwrap();
        assert_eq!(&framed[2..], &bytes[..]);
        assert_eq!(len, bytes.len(), "{c:?}");
    }
    let mut codes = std::collections::BTreeSet::new();
    for r in replies() {
        let bytes = r.encode();
        assert_eq!(Reply::decode(&bytes), Ok(r.clone()), "{r:?}");
        assert!(frame(&bytes).is_some(), "{r:?}");
        if let Reply::Refused { .. } | Reply::Failed { .. } = r {
            codes.insert((bytes[0], bytes[2]));
        }
    }
    // Each reason has a code of its own.
    assert_eq!(codes.len(), 12);
    // A string longer than the cap is cut at the character boundary before
    // the cap, which falls inside the two bytes of an "é".
    let long = format!("a{}", "é".repeat(MAX_TEXT));
    let bytes = Reply::Hello {
        version: long.clone(),
        configuration: String::new(),
    }
    .encode();
    let Ok(Reply::Hello { version, .. }) = Reply::decode(&bytes) else {
        panic!("a cut string decodes");
    };
    assert_eq!(version.len(), MAX_TEXT - 1);
    assert!(long.starts_with(&version));
}

// Covers: T02
#[test]
fn control_refuses_malformed_messages() {
    // Every message cut short, and with a byte after it.
    for c in commands() {
        let bytes = c.encode();
        for n in 1..bytes.len() {
            assert_eq!(
                Command::decode(&bytes[..n]),
                Err(Malformed::Short),
                "{c:?} cut to {n}"
            );
        }
        let mut more = bytes.clone();
        more.push(0);
        assert_eq!(Command::decode(&more), Err(Malformed::Trailing(1)), "{c:?}");
    }
    for r in replies() {
        let bytes = r.encode();
        for n in 1..bytes.len() {
            assert_eq!(
                Reply::decode(&bytes[..n]),
                Err(Malformed::Short),
                "{r:?} cut to {n}"
            );
        }
        let mut more = bytes.clone();
        more.extend_from_slice(&[0, 0]);
        assert_eq!(Reply::decode(&more), Err(Malformed::Trailing(2)), "{r:?}");
    }
    // No message, and one longer than a frame.
    assert_eq!(Command::decode(&[]), Err(Malformed::Length(0)));
    assert_eq!(Reply::decode(&[]), Err(Malformed::Length(0)));
    let mut big = Command::Quit.encode();
    big.resize(MAX_FRAME + 1, 0);
    assert_eq!(Command::decode(&big), Err(Malformed::Length(MAX_FRAME + 1)));
    // A command's tag is no reply, and a reply's no command.
    assert_eq!(
        Reply::decode(&Command::Quit.encode()),
        Err(Malformed::Tag(5))
    );
    assert_eq!(
        Command::decode(&Reply::Playing { player: 1 }.encode()),
        Err(Malformed::Tag(67))
    );
    assert_eq!(Command::decode(&[0]), Err(Malformed::Tag(0)));
    // A reason the protocol does not have.
    let mut refused = Reply::Refused {
        player: 1,
        why: Refusal::Size,
        code: 0,
    }
    .encode();
    refused[2] = 6;
    assert_eq!(Reply::decode(&refused), Err(Malformed::Reason(6)));
    let mut failed = Reply::Failed {
        player: 1,
        why: Failure::Seek,
        code: 0,
    }
    .encode();
    failed[2] = 200;
    assert_eq!(Reply::decode(&failed), Err(Malformed::Reason(200)));
    // A string that is not UTF-8, and one longer than the cap though it
    // fits in a frame.
    let mut hello = Reply::Hello {
        version: "ab".to_owned(),
        configuration: String::new(),
    }
    .encode();
    hello[3] = 0xff;
    assert_eq!(Reply::decode(&hello), Err(Malformed::Text));
    let mut over = vec![64];
    let len = u16::try_from(MAX_TEXT + 1).unwrap();
    over.extend_from_slice(&len.to_le_bytes());
    over.extend(std::iter::repeat_n(b'a', MAX_TEXT + 1));
    over.extend_from_slice(&0_u16.to_le_bytes());
    assert!(over.len() <= MAX_FRAME);
    assert_eq!(Reply::decode(&over), Err(Malformed::Text));
    // Frames of no bytes or more than the cap.
    assert_eq!(frame_len([0, 0]), Err(Malformed::Length(0)));
    let over = u16::try_from(MAX_FRAME + 1).unwrap().to_le_bytes();
    assert_eq!(frame_len(over), Err(Malformed::Length(MAX_FRAME + 1)));
    let max = u16::try_from(MAX_FRAME).unwrap().to_le_bytes();
    assert_eq!(frame_len(max), Ok(MAX_FRAME));
    assert_eq!(frame(&[]), None);
    assert_eq!(frame(&vec![1; MAX_FRAME + 1]), None);
    assert!(frame(&vec![1; MAX_FRAME]).is_some());
}
