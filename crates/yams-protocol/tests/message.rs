use yams_protocol::{
    Accepted, Completed, MAX_ARGUMENT_BYTES, MAX_ARGUMENTS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    Message, PROTOCOL_VERSION, Rejected, Request, decode_request, decode_response, encode,
};

#[test]
fn request_round_trips_with_exact_fields() {
    let request = Message::Request(
        Request::from_argv(
            vec!["--json".into(), "two words".into()],
            String::from("/tmp/project"),
        )
        .expect("search request"),
    );

    let body = encode(&request).unwrap();

    assert_eq!(decode_request(&body).unwrap(), request);
    assert_eq!(
        String::from_utf8(body).unwrap(),
        format!(
            "{{\"version\":{PROTOCOL_VERSION},\"type\":\"request\",\"operation\":{{\"kind\":\"search\",\"query\":\"two words\",\"k\":\"5\",\"json\":true,\"full\":false,\"no_gate\":false,\"explain\":false}},\"argv\":[\"--json\",\"two words\"],\"cwd\":\"/tmp/project\"}}"
        )
    );
}

#[test]
fn responses_round_trip_with_exact_fields() {
    let cases = [
        (
            Message::Accepted(Accepted {
                request_id: "request-1".into(),
            }),
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"type\":\"accepted\",\"request_id\":\"request-1\"}}"
            ),
        ),
        (
            Message::Completed(Completed {
                request_id: "request-1".into(),
                exit_code: 17,
                stdout: "results\n".into(),
                stderr: "diagnostic\n".into(),
            }),
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"type\":\"completed\",\"request_id\":\"request-1\",\"exit_code\":17,\"stdout\":\"results\\n\",\"stderr\":\"diagnostic\\n\"}}"
            ),
        ),
        (
            Message::Rejected(Rejected {
                code: "busy".into(),
                message: "try again later".into(),
            }),
            format!(
                "{{\"version\":{PROTOCOL_VERSION},\"type\":\"rejected\",\"code\":\"busy\",\"message\":\"try again later\"}}"
            ),
        ),
    ];

    for (message, expected) in cases {
        let body = encode(&message).unwrap();
        assert_eq!(String::from_utf8(body.clone()).unwrap(), expected);
        assert_eq!(decode_response(&body).unwrap(), message);
    }
}

#[test]
fn request_decoder_rejects_invalid_roots_utf8_fields_version_and_type() {
    let invalid: &[&[u8]] = &[
        b"\xff",
        b"[]",
        b"null",
        b"not-json",
        br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp","cwd":"/other"}"#,
        br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp","extra":1}"#,
        br#"{"version":4,"type":"request","argv":[],"cwd":"/tmp"}"#,
        br#"{"version":true,"type":"request","argv":[],"cwd":"/tmp"}"#,
        br#"{"version":2,"type":"accepted","argv":[],"cwd":"/tmp"}"#,
        br#"{"version":2,"type":"request","argv":[],"cwd":"relative"}"#,
        br#"{"version":2,"type":"request","argv":"query","cwd":"/tmp"}"#,
        br#"{"version":2,"type":"request","argv":[1],"cwd":"/tmp"}"#,
        br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp"} trailing"#,
    ];

    for body in invalid {
        assert!(decode_request(body).is_err(), "accepted {body:?}");
    }
}

#[test]
fn duplicate_and_unknown_keys_are_rejected_before_their_values_are_mapped() {
    let duplicate = br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp","cwd":{}}"#;
    let unknown = br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp","secret":{}}"#;

    assert_eq!(
        decode_request(duplicate),
        Err(yams_protocol::ProtocolError::DuplicateField)
    );
    assert_eq!(
        decode_request(unknown),
        Err(yams_protocol::ProtocolError::UnknownField)
    );
}

#[test]
fn hostile_values_cannot_spoof_internal_json_error_classes() {
    let duplicate_spoof =
        br#"{"version":"yams duplicate JSON field","type":"request","argv":[],"cwd":"/tmp"}"#;
    let unknown_spoof =
        br#"{"version":"yams unknown JSON field","type":"request","argv":[],"cwd":"/tmp"}"#;

    assert_eq!(
        decode_request(duplicate_spoof),
        Err(yams_protocol::ProtocolError::InvalidJson)
    );
    assert_eq!(
        decode_request(unknown_spoof),
        Err(yams_protocol::ProtocolError::InvalidJson)
    );
}

#[test]
fn request_decoder_rejects_excessive_nesting_before_mapping() {
    let body = format!("{}0{}", "[".repeat(1_000), "]".repeat(1_000));
    assert!(body.len() < MAX_REQUEST_BYTES);

    let error = decode_request(body.as_bytes()).unwrap_err();

    assert_eq!(error.to_string(), "invalid JSON");
}

#[test]
fn request_argument_and_body_bounds_are_bytes_not_characters() {
    assert_eq!(MAX_REQUEST_BYTES, 64 * 1024);
    assert_eq!(MAX_RESPONSE_BYTES, 8 * 1024 * 1024);
    assert_eq!(MAX_ARGUMENTS, 256);
    assert_eq!(MAX_ARGUMENT_BYTES, 16 * 1024);

    let too_many = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "request",
        "argv": vec!["x"; MAX_ARGUMENTS + 1],
        "cwd": "/tmp",
    });
    let too_large = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "request",
        "argv": ["x".repeat(MAX_ARGUMENT_BYTES + 1)],
        "cwd": "/tmp",
    });
    let multibyte = serde_json::json!({
        "version": PROTOCOL_VERSION,
        "type": "request",
        "argv": ["☃".repeat(MAX_ARGUMENT_BYTES / 3 + 1)],
        "cwd": "/tmp",
    });

    for value in [too_many, too_large, multibyte] {
        assert!(decode_request(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    let oversized_body = vec![b' '; MAX_REQUEST_BYTES + 1];
    assert!(decode_request(&oversized_body).is_err());

    let exact_argument = Message::Request(
        Request::from_argv(vec!["x".repeat(MAX_ARGUMENT_BYTES)], String::from("/tmp"))
            .expect("service request is not --write"),
    );
    let exact_count = Message::Request(
        Request::from_argv(vec!["x".into(); MAX_ARGUMENTS], String::from("/tmp"))
            .expect("service request is not --write"),
    );
    assert!(encode(&exact_argument).is_ok());
    assert!(encode(&exact_count).is_ok());
}

#[test]
fn escaped_lone_surrogates_are_not_accepted_as_utf8_strings() {
    type Decoder = fn(&[u8]) -> Result<Message, yams_protocol::ProtocolError>;
    let invalid: &[(&[u8], Decoder)] = &[
        (
            br#"{"version":2,"type":"request","argv":["\ud800"],"cwd":"/tmp"}"#,
            decode_request,
        ),
        (
            br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp/\ud800"}"#,
            decode_request,
        ),
        (
            br#"{"version":2,"type":"accepted","request_id":"\ud800"}"#,
            decode_response,
        ),
        (
            br#"{"version":2,"type":"completed","request_id":"id","exit_code":0,"stdout":"\ud800","stderr":""}"#,
            decode_response,
        ),
        (
            br#"{"version":2,"type":"rejected","code":"bad","message":"\ud800"}"#,
            decode_response,
        ),
    ];

    for (body, decoder) in invalid {
        assert!(decoder(body).is_err(), "accepted {body:?}");
    }
}

#[test]
fn response_decoder_rejects_wrong_shapes_and_bool_exit_code() {
    let invalid: &[&[u8]] = &[
        br#"{"version":2,"type":"accepted","request_id":""}"#,
        br#"{"version":2,"type":"accepted","request_id":"id","extra":1}"#,
        br#"{"version":2,"type":"accepted","request_id":"a","request_id":"b"}"#,
        br#"{"version":2,"type":"completed","request_id":"id","exit_code":true,"stdout":"","stderr":""}"#,
        br#"{"version":2,"type":"completed","request_id":"id","exit_code":-1,"stdout":"","stderr":""}"#,
        br#"{"version":2,"type":"completed","request_id":"id","exit_code":256,"stdout":"","stderr":""}"#,
        br#"{"version":2,"type":"completed","request_id":"id","exit_code":0,"stdout":""}"#,
        br#"{"version":2,"type":"rejected","code":"","message":"bad"}"#,
        br#"{"version":2,"type":"rejected","code":"bad","message":1}"#,
        br#"{"version":2,"type":"unknown"}"#,
        br#"{"version":2,"type":"request","argv":[],"cwd":"/tmp"}"#,
    ];

    for body in invalid {
        assert!(decode_response(body).is_err(), "accepted {body:?}");
    }

    let oversized_body = vec![b' '; MAX_RESPONSE_BYTES + 1];
    assert!(decode_response(&oversized_body).is_err());
}

#[test]
fn constructors_are_validated_before_encoding() {
    let invalid = [
        Message::Request(
            Request::from_argv(
                vec!["x".repeat(MAX_ARGUMENT_BYTES + 1)],
                String::from("/tmp"),
            )
            .expect("service request is not --write"),
        ),
        Message::Request(
            Request::from_argv(vec![], String::from("relative"))
                .expect("service request is not --write"),
        ),
        Message::Accepted(Accepted {
            request_id: String::new(),
        }),
        Message::Rejected(Rejected {
            code: String::new(),
            message: "bad".into(),
        }),
    ];

    for message in invalid {
        assert!(encode(&message).is_err(), "encoded {message:?}");
    }

    let oversized = Message::Completed(Completed {
        request_id: "id".into(),
        exit_code: 0,
        stdout: "x".repeat(MAX_RESPONSE_BYTES),
        stderr: String::new(),
    });
    assert!(encode(&oversized).is_err());

    let escaping_request = Message::Request(
        Request::from_argv(
            vec!["\0".repeat(MAX_ARGUMENT_BYTES); MAX_ARGUMENTS],
            String::from("/tmp"),
        )
        .expect("service request is not --write"),
    );
    let escaping_response = Message::Completed(Completed {
        request_id: "id".into(),
        exit_code: 0,
        stdout: "\0".repeat(MAX_RESPONSE_BYTES),
        stderr: String::new(),
    });
    assert_eq!(
        encode(&escaping_request),
        Err(yams_protocol::ProtocolError::FrameTooLarge {
            declared: 25_265_055,
            limit: MAX_REQUEST_BYTES,
        })
    );
    assert_eq!(
        encode(&escaping_response),
        Err(yams_protocol::ProtocolError::FrameTooLarge {
            declared: 50_331_736,
            limit: MAX_RESPONSE_BYTES,
        })
    );
}

#[test]
fn debug_output_redacts_all_peer_controlled_values() {
    let secrets = [
        "secret-argv",
        "/secret/cwd",
        "secret-id",
        "secret-output",
        "secret-error",
    ];
    let messages = [
        Message::Request(
            Request::from_argv(vec![secrets[0].into()], secrets[1].to_string())
                .expect("service request is not --write"),
        ),
        Message::Accepted(Accepted {
            request_id: secrets[2].into(),
        }),
        Message::Completed(Completed {
            request_id: secrets[2].into(),
            exit_code: 4,
            stdout: secrets[3].into(),
            stderr: secrets[4].into(),
        }),
        Message::Rejected(Rejected {
            code: secrets[4].into(),
            message: secrets[3].into(),
        }),
    ];

    for message in messages {
        let rendered = format!("{message:?}");
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "debug leaked {secret}: {rendered}"
            );
        }
    }

    let direct = [
        format!(
            "{:?}",
            Request::from_argv(vec![secrets[0].into()], secrets[1].to_string())
                .expect("service request is not --write")
        ),
        format!(
            "{:?}",
            Accepted {
                request_id: secrets[2].into(),
            }
        ),
        format!(
            "{:?}",
            Completed {
                request_id: secrets[2].into(),
                exit_code: 4,
                stdout: secrets[3].into(),
                stderr: secrets[4].into(),
            }
        ),
        format!(
            "{:?}",
            Rejected {
                code: secrets[4].into(),
                message: secrets[3].into(),
            }
        ),
    ];
    for rendered in direct {
        for secret in secrets {
            assert!(
                !rendered.contains(secret),
                "debug leaked {secret}: {rendered}"
            );
        }
    }
}
