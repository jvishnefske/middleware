//! Tests of the wire contract: keys and bodies round-trip, malformed input
//! is refused, and [`super::serve`] applies the phase table and ranges.

use super::*;

const NODE: [u8; 8] = [0x24, 0x6f, 0x28, 0xaa, 0x01, 0xfe, 0, 0];

#[test]
fn keys_name_the_node_in_hex_and_parse_back() {
    let mut buf = [0u8; MAX_KEY];
    let k = command_key(NODE, "LED", "Blink", &mut buf).unwrap();
    assert_eq!(k, "unit/246f28aa01fe0000/phase/LED.Blink");
    assert_eq!(
        parse_key(k),
        Ok(PhaseKey {
            node: NODE,
            em: "LED",
            phase: "Blink",
            state: false
        })
    );
    let mut buf = [0u8; MAX_KEY];
    let s = state_key(NODE, "LED", "Blink", &mut buf).unwrap();
    assert_eq!(s, "unit/246f28aa01fe0000/phase/LED.Blink/state");
    assert!(parse_key(s).unwrap().state);
    let long = "A23456789012345678901234";
    let mut buf = [0u8; MAX_KEY];
    assert_eq!(
        state_key(NODE, long, long, &mut buf).unwrap().len(),
        MAX_KEY
    );
}

#[test]
fn malformed_keys_and_identifiers_are_refused() {
    let mut buf = [0u8; MAX_KEY];
    assert_eq!(
        command_key(NODE, "has.dot", "Blink", &mut buf),
        Err(WireError::BadKey)
    );
    let mut small = [0u8; 10];
    assert_eq!(
        command_key(NODE, "LED", "Blink", &mut small),
        Err(WireError::BufferTooSmall)
    );
    for k in [
        "unit/246f28aa01fe0000/phase/LEDBlink",
        "unit/246f28aa01fe000/phase/LED.Blink",
        "unit/246F28AA01FE0000/phase/LED.Blink",
        "unit/246f28aa01fe0000/phase/LED.Blink/other",
        "unit/246f28aa01fe0000/phase/LED.Blink/state/x",
        "node/246f28aa01fe0000/phase/LED.Blink",
    ] {
        assert_eq!(parse_key(k), Err(WireError::BadKey), "{k}");
    }
}

#[test]
fn command_ack_and_report_bodies_round_trip() {
    let mut buf = [0u8; MAX_BODY];
    let b = encode_command(Command::Start, 7, &[250, 3], &mut buf).unwrap();
    assert_eq!(b, [1, 1, 7, 0, 0, 0, 2, 250, 0, 0, 0, 3, 0, 0, 0]);
    let c = decode_command(b).unwrap();
    assert_eq!(
        (c.command, c.id, c.n, &c.args[..2]),
        (Command::Start, 7, 2, &[250, 3][..])
    );

    let mut buf = [0u8; ACK_LEN];
    let a = encode_ack(true, PhaseState::Running, 7, &mut buf).unwrap();
    assert_eq!(decode_ack(a), Ok((7, Ack::Accepted(PhaseState::Running))));
    let mut buf = [0u8; ACK_LEN];
    let a = encode_ack(false, PhaseState::Held, 9, &mut buf).unwrap();
    assert_eq!(decode_ack(a), Ok((9, Ack::Rejected(PhaseState::Held))));

    let mut buf = [0u8; MAX_BODY];
    let r = encode_report(PhaseState::Complete, 8, &[-42], &mut buf).unwrap();
    let d = decode_report(r).unwrap();
    assert_eq!(
        (d.state, d.last_id, d.n, d.values[0]),
        (PhaseState::Complete, 8, 1, -42)
    );
}

#[test]
fn malformed_bodies_are_refused() {
    let mut buf = [0u8; MAX_BODY];
    assert_eq!(
        encode_command(Command::Hold, 1, &[1], &mut buf),
        Err(WireError::TooManyValues)
    );
    assert_eq!(
        encode_command(Command::Start, 1, &[1; 5], &mut buf),
        Err(WireError::TooManyValues)
    );
    for body in [
        &[][..],
        &[2, 1, 0, 0, 0, 0, 0],
        &[1, 9, 0, 0, 0, 0, 0],
        &[1, 1, 0, 0, 0],
        &[1, 1, 0, 0, 0, 0, 1, 0, 0],
        &[1, 2, 0, 0, 0, 0, 1, 0, 0, 0, 0],
    ] {
        assert_eq!(decode_command(body), Err(WireError::BadBody), "{body:?}");
    }
    assert_eq!(decode_ack(&[1, 2, 0, 0, 0, 0, 0]), Err(WireError::BadBody));
    assert_eq!(decode_ack(&[1, 0, 11, 0, 0, 0, 0]), Err(WireError::BadBody));
    assert_eq!(decode_ack(&[1, 0, 0, 0, 0, 0]), Err(WireError::BadBody));
    assert_eq!(
        decode_report(&[1, 12, 0, 0, 0, 0, 0]),
        Err(WireError::BadBody)
    );
}

#[test]
fn serve_applies_the_table_and_checks_start_arguments_against_ranges() {
    let ranges = [(50, 1_000), (1, 10)];
    let mut buf = [0u8; MAX_BODY];
    let start = encode_command(Command::Start, 3, &[250, 3], &mut buf).unwrap();
    let s = serve(PhaseState::Idle, &ranges, start).unwrap();
    assert_eq!(
        (s.state, s.accepted, s.command.id),
        (PhaseState::Running, true, 3)
    );

    let s = serve(PhaseState::Running, &ranges, start).unwrap();
    assert_eq!((s.state, s.accepted), (PhaseState::Running, false));

    let mut buf = [0u8; MAX_BODY];
    let bad = encode_command(Command::Start, 4, &[20, 3], &mut buf).unwrap();
    assert_eq!(
        serve(PhaseState::Idle, &ranges, bad),
        Err(ServeError::InvalidRequest)
    );
    let mut buf = [0u8; MAX_BODY];
    let short = encode_command(Command::Start, 4, &[250], &mut buf).unwrap();
    assert_eq!(
        serve(PhaseState::Idle, &ranges, short),
        Err(ServeError::InvalidRequest)
    );
    assert_eq!(
        serve(PhaseState::Idle, &ranges, &[0]),
        Err(ServeError::InvalidRequest)
    );

    let mut buf = [0u8; MAX_BODY];
    let hold = encode_command(Command::Hold, 5, &[], &mut buf).unwrap();
    let s = serve(PhaseState::Running, &ranges, hold).unwrap();
    assert_eq!((s.state, s.accepted), (PhaseState::Holding, true));
}
