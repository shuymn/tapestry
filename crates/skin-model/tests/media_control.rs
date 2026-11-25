//! The media worker's control pipe: every message survives its encoding,
//! and the decoder refuses whatever is not exactly one message, since the
//! game reads the worker's replies as untrusted (design.md "Threat Model"
//! T02).

use skin_model::media::control::{
    Command, Failure, MAX_FRAME, MAX_TEXT, Malformed, Matrix, PROTOCOL, Range, Refusal, Reply,
    frame, frame_len,
};

/// Commands, each with an id of its own.
fn commands() -> Vec<(u32, Command)> {
    let all = vec![
        Command::Open {
            player: 3,
            file: 0x0123_4567_89ab_cdef,
        },
        Command::Play {
            player: 250,
            section: 0x1f4,
            event: u64::MAX - 7,
            looping: false,
        },
        Command::Play {
            player: 251,
            section: 0x2f4,
            event: 0x88,
            looping: true,
        },
        Command::Seek {
            player: 1,
            run: 9,
            at: -1_500_000,
        },
        Command::Close { player: 77 },
        Command::Quit,
        #[cfg(feature = "dev-tools")]
        Command::Fire,
    ];
    (1..)
        .zip(all)
        .map(|(i, c)| (0x8000_0001_u32 ^ i, c))
        .collect()
}

fn replies() -> Vec<Reply> {
    let mut all = vec![
        Reply::Hello {
            protocol: PROTOCOL,
            version: "n9.0.2-39-g7adfbe40dc-2".to_owned(),
            configuration: "--enable-shared --disable-static ÿ".to_owned(),
        },
        Reply::Probed {
            id: 0xdead_beef,
            player: 2,
            width: 2_560,
            height: 1_080,
            duration: 15_000_000,
            matrix: Matrix::Bt709,
            range: Range::Limited,
            codec: "h264".to_owned(),
            container: "mov,mp4,m4a,3gp,3g2,mj2".to_owned(),
        },
        Reply::Probed {
            id: 0xbeef,
            player: 3,
            width: 640,
            height: 480,
            duration: 0,
            matrix: Matrix::Bt601,
            range: Range::Full,
            codec: "mjpeg".to_owned(),
            container: "avi".to_owned(),
        },
        Reply::Beat,
        Reply::Playing { id: 5, player: 5 },
        Reply::Sought {
            id: u32::MAX,
            player: 6,
            run: 4,
            landed: Some(1_500_000),
            attempts: 2,
        },
        // An empty seek's answer: no landing.
        Reply::Sought {
            id: 10,
            player: 6,
            run: 5,
            landed: None,
            attempts: 0,
        },
        Reply::Ended { player: 7, run: 12 },
        Reply::Closed { id: 8, player: 8 },
        Reply::Rejected {
            id: 9,
            player: 9,
            code: -5,
        },
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
            id: 100 + u32::try_from(i).unwrap(),
            player: u8::try_from(i).unwrap() + 10,
            why,
            code: -1_094_995_529 - i32::try_from(i).unwrap(),
        });
    }
    for (i, why) in [
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
    #[cfg(feature = "dev-tools")]
    all.push(Reply::Fired {
        qpc: -0x0123_4567_89ab,
    });
    all
}

// Covers: T02
#[test]
fn control_messages_round_trip() {
    for (id, c) in commands() {
        let bytes = c.encode(id);
        assert_eq!(Command::decode(&bytes), Ok((id, c.clone())), "{c:?}");
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
        match r {
            Reply::Refused { .. } => codes.insert((bytes[0], bytes[6])),
            Reply::Failed { .. } => codes.insert((bytes[0], bytes[2])),
            // The matrix and the range, after the tag, id, player, sizes
            // and duration.
            Reply::Probed { .. } => codes.insert((bytes[22], bytes[23])),
            _ => false,
        };
    }
    // Each reason, and each colour pair, has a code of its own.
    assert_eq!(codes.len(), 13);
    // A string longer than the cap is cut at the character boundary before
    // the cap, which falls inside the two bytes of an "é".
    let long = format!("a{}", "é".repeat(MAX_TEXT));
    let bytes = Reply::Hello {
        protocol: PROTOCOL,
        version: long.clone(),
        configuration: String::new(),
    }
    .encode();
    let Ok(Reply::Hello { version, .. }) = Reply::decode(&bytes) else {
        panic!("a cut string decodes");
    };
    assert_eq!(version.len(), MAX_TEXT - 1);
    assert!(long.starts_with(&version));
    // Two strings at the cap would overrun a frame: the second is cut to
    // the room the first leaves, and the reply still frames.
    let (first, second) = ("v".repeat(MAX_TEXT), "c".repeat(MAX_TEXT));
    for reply in [
        Reply::Hello {
            protocol: PROTOCOL,
            version: first.clone(),
            configuration: second.clone(),
        },
        Reply::Probed {
            id: 1,
            player: 1,
            width: 2,
            height: 3,
            duration: 4,
            matrix: Matrix::Bt709,
            range: Range::Full,
            codec: first.clone(),
            container: second.clone(),
        },
    ] {
        let bytes = reply.encode();
        assert_eq!(bytes.len(), MAX_FRAME, "{reply:?}");
        assert!(frame(&bytes).is_some(), "{reply:?}");
        let (a, b) = match Reply::decode(&bytes) {
            Ok(Reply::Hello {
                version,
                configuration,
                ..
            }) => (version, configuration),
            Ok(Reply::Probed {
                codec, container, ..
            }) => (codec, container),
            other => panic!("{other:?}"),
        };
        assert_eq!(a, first);
        assert!(b.len() < MAX_TEXT && second.starts_with(&b), "{}", b.len());
    }
}

// Covers: T02
#[test]
fn control_refuses_malformed_messages() {
    // Every message cut short, and with a byte after it.
    for (id, c) in commands() {
        let bytes = c.encode(id);
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
    let mut big = Command::Quit.encode(1);
    big.resize(MAX_FRAME + 1, 0);
    assert_eq!(Command::decode(&big), Err(Malformed::Length(MAX_FRAME + 1)));
    // A command's tag is no reply, and a reply's no command.
    assert_eq!(
        Reply::decode(&Command::Quit.encode(1)),
        Err(Malformed::Tag(5))
    );
    assert_eq!(
        Command::decode(&Reply::Playing { id: 1, player: 1 }.encode()),
        Err(Malformed::Tag(67))
    );
    assert_eq!(Command::decode(&[0]), Err(Malformed::Tag(0)));
    // A reason the protocol does not have.
    let mut refused = Reply::Refused {
        id: 1,
        player: 1,
        why: Refusal::Size,
        code: 0,
    }
    .encode();
    refused[6] = 6;
    assert_eq!(Reply::decode(&refused), Err(Malformed::Reason(6)));
    let mut failed = Reply::Failed {
        player: 1,
        why: Failure::Seek,
        code: 0,
    }
    .encode();
    failed[2] = 5;
    assert_eq!(Reply::decode(&failed), Err(Malformed::Reason(5)));
    failed[2] = 200;
    assert_eq!(Reply::decode(&failed), Err(Malformed::Reason(200)));
    // A colour and a loop flag the protocol does not have.
    let probed = Reply::Probed {
        id: 1,
        player: 1,
        width: 2,
        height: 2,
        duration: 0,
        matrix: Matrix::Bt601,
        range: Range::Full,
        codec: String::new(),
        container: String::new(),
    }
    .encode();
    for (at, v) in [(22, 2), (23, 2)] {
        let mut bad = probed.clone();
        bad[at] = v;
        assert_eq!(Reply::decode(&bad), Err(Malformed::Reason(v)), "byte {at}");
    }
    let mut play = Command::Play {
        player: 1,
        section: 2,
        event: 3,
        looping: true,
    }
    .encode(1);
    let last = play.len() - 1;
    play[last] = 2;
    assert_eq!(Command::decode(&play), Err(Malformed::Reason(2)));
    // A seek's landing flag the protocol does not have: after the tag, id,
    // player and run.
    let mut sought = Reply::Sought {
        id: 1,
        player: 1,
        run: 2,
        landed: None,
        attempts: 0,
    }
    .encode();
    assert_eq!(sought[14], 0);
    sought[14] = 2;
    assert_eq!(Reply::decode(&sought), Err(Malformed::Reason(2)));
    // A string that is not UTF-8, and one longer than the cap though it
    // fits in a frame.
    let mut hello = Reply::Hello {
        protocol: PROTOCOL,
        version: "ab".to_owned(),
        configuration: String::new(),
    }
    .encode();
    hello[7] = 0xff;
    assert_eq!(Reply::decode(&hello), Err(Malformed::Text));
    let mut over = vec![64];
    over.extend_from_slice(&PROTOCOL.to_le_bytes());
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
