//! Protocol conformance, codec, client, monitor, and reconnect tests.

use std::collections::{BTreeMap, HashMap};

use arduino_router::{
    decode_frame, decode_value, encode_frame, encode_frame_checked, encode_value, CodecLimits,
    DecodeError, EncodeError, ErrorCode, FromParams, FromValue, IntoParams, IntoValue, RpcError,
    RpcMessage, Value,
};

#[test]
fn msgpack_value_round_trips_all_types_and_strict_prefixes() {
    let mut btree = BTreeMap::new();
    btree.insert("a".to_string(), 1_i32);
    btree.insert("b".to_string(), 2_i32);

    let values = vec![
        Value::Nil,
        Value::Bool(false),
        Value::Bool(true),
        Value::Int(0),
        Value::Int(127),
        Value::Int(128),
        Value::Int(255),
        Value::Int(256),
        Value::Int(65_535),
        Value::Int(65_536),
        Value::Int(i64::from(i32::MAX)),
        Value::Int(i64::MAX),
        Value::Uint(u64::MAX),
        Value::Int(-1),
        Value::Int(-32),
        Value::Int(-33),
        Value::Int(-128),
        Value::Int(-129),
        Value::Int(-32_768),
        Value::Int(-32_769),
        Value::Int(i64::from(i32::MIN)),
        Value::Int(i64::MIN),
        Value::F32(125.5),
        Value::F64(-0.25),
        Value::Str(String::new()),
        Value::Str("tactus/state".to_string()),
        Value::Str("x".repeat(40)),
        Value::Str("y".repeat(300)),
        Value::Bin(vec![0xF8, 0xFA, 0xFC]),
        Value::Array(vec![
            Value::Int(1200),
            Value::Int(0),
            Value::Int(2),
            Value::Bool(true),
        ]),
        Value::Map(vec![(Value::Str("bpm".to_string()), Value::F64(128.25))]),
        btree.into_value(),
        Value::Ext {
            kind: 7,
            data: vec![1],
        },
        Value::Ext {
            kind: -2,
            data: vec![1, 2],
        },
        Value::Ext {
            kind: 3,
            data: vec![1, 2, 3, 4],
        },
        Value::Ext {
            kind: 4,
            data: vec![1, 2, 3, 4, 5, 6, 7, 8],
        },
        Value::Ext {
            kind: 5,
            data: vec![0xAA; 16],
        },
        Value::Ext {
            kind: 6,
            data: vec![0x55; 3],
        },
    ];

    let limits = CodecLimits::default();
    for val in values {
        let mut buf = Vec::new();
        encode_value(&val, &mut buf).expect("encode value");

        for prefix_len in 0..buf.len() {
            assert_eq!(
                decode_value(&buf[..prefix_len], limits),
                Err(DecodeError::Incomplete),
                "prefix {prefix_len}/{} for {val:?}",
                buf.len()
            );
        }

        let mut with_tail = buf.clone();
        with_tail.extend_from_slice(&[0x01, 0x02]);
        let (decoded, consumed) = decode_value(&with_tail, limits).expect("decode full value");
        assert_eq!(consumed, buf.len());
        assert_eq!(decoded, val);
    }
}

#[test]
fn preserves_f32_and_f64_wire_tags_and_enforces_limits() {
    let mut f32_buf = Vec::new();
    encode_value(&Value::F32(12.5), &mut f32_buf).unwrap();
    assert_eq!(f32_buf[0], 0xca);
    assert_eq!(
        decode_value(&f32_buf, CodecLimits::default()),
        Ok((Value::F32(12.5), 5))
    );

    let mut f64_buf = Vec::new();
    encode_value(&Value::F64(12.5), &mut f64_buf).unwrap();
    assert_eq!(f64_buf[0], 0xcb);
    assert_eq!(
        decode_value(&f64_buf, CodecLimits::default()),
        Ok((Value::F64(12.5), 9))
    );

    let small_limits = CodecLimits {
        max_frame_bytes: 64,
        max_decode_depth: 3,
    };

    // Reserved tag 0xc1 is malformed.
    assert!(matches!(
        decode_value(&[0xc1], small_limits),
        Err(DecodeError::Malformed(_))
    ));

    // Invalid UTF-8 string is malformed.
    assert!(matches!(
        decode_value(&[0xa2, 0xff, 0xfe], small_limits),
        Err(DecodeError::Malformed(_))
    ));

    // Declared oversized str/array triggers Oversized immediately.
    let mut huge_str = vec![0xdb];
    huge_str.extend_from_slice(&128_u32.to_be_bytes());
    assert_eq!(
        decode_value(&huge_str, small_limits),
        Err(DecodeError::Oversized)
    );

    // Depth limit is enforced.
    let nested = Value::Array(vec![Value::Array(vec![Value::Array(vec![Value::Int(1)])])]);
    let mut nested_bytes = Vec::new();
    encode_value(&nested, &mut nested_bytes).unwrap();
    assert!(matches!(
        decode_value(&nested_bytes, small_limits),
        Err(DecodeError::Malformed(_))
    ));

    // encode_frame_checked rolls back buffer on overflow.
    let mut out = vec![0xaa, 0xbb];
    let msg = RpcMessage::Notification {
        method: "big".to_string(),
        params: vec![Value::Str("z".repeat(100))],
    };
    let err = encode_frame_checked(&msg, &mut out, 32).unwrap_err();
    assert!(matches!(err, EncodeError::FrameTooLarge { .. }));
    assert_eq!(out, vec![0xaa, 0xbb]);
}

#[test]
fn golden_wire_vectors_and_error_codes_match_upstream() {
    // Request from arduino-router README: [0, 50, "$/register", ["ping"]]
    let req = RpcMessage::Request {
        msgid: 50,
        method: "$/register".to_string(),
        params: vec![Value::Str("ping".to_string())],
    };
    let mut wire = Vec::new();
    encode_frame(&req, &mut wire).unwrap();
    assert_eq!(
        wire,
        vec![
            0x94, 0x00, 0x32, 0xaa, b'$', b'/', b'r', b'e', b'g', b'i', b's', b't', b'e', b'r',
            0x91, 0xa4, b'p', b'i', b'n', b'g',
        ]
    );
    let (decoded, consumed) = decode_frame(&wire, CodecLimits::default()).unwrap();
    assert_eq!(consumed, wire.len());
    assert_eq!(decoded, req);

    // Error response from arduino-router README: [1, 50, [5, "route already exists: ping"], null]
    let resp_wire = vec![
        0x94, 0x01, 0x32, 0x92, 0x05, 0xba, b'r', b'o', b'u', b't', b'e', b' ', b'a', b'l', b'r',
        b'e', b'a', b'd', b'y', b' ', b'e', b'x', b'i', b's', b't', b's', b':', b' ', b'p', b'i',
        b'n', b'g', 0xc0,
    ];
    let (decoded_resp, _) = decode_frame(&resp_wire, CodecLimits::default()).unwrap();
    match decoded_resp {
        RpcMessage::Response {
            msgid,
            error,
            result,
        } => {
            assert_eq!(msgid, 50);
            assert!(result.is_nil());
            let rpc_err = RpcError::from_wire(error);
            assert_eq!(rpc_err.code, ErrorCode::RouteAlreadyExists);
            assert_eq!(rpc_err.message, "route already exists: ping");
        }
        other => panic!("expected Response, got {other:?}"),
    }

    // Check all upstream ErrorCode mappings.
    let cases = [
        (0x00, ErrorCode::NoError),
        (1, ErrorCode::InvalidParams),
        (2, ErrorCode::MethodNotAvailable),
        (3, ErrorCode::FailedToSendRequest),
        (4, ErrorCode::RouterGenericError),
        (5, ErrorCode::RouteAlreadyExists),
        (6, ErrorCode::BufferLimitExceeded),
        (0xFB, ErrorCode::Timeout),
        (0xFC, ErrorCode::ParsingError),
        (0xFD, ErrorCode::MalformedCall),
        (0xFE, ErrorCode::FunctionNotFound),
        (0xFF, ErrorCode::GenericError),
        (42, ErrorCode::Custom(42)),
    ];
    for (num, expected) in cases {
        assert_eq!(ErrorCode::from_i32(num), expected);
        assert_eq!(expected.as_i32(), num);
    }

    // Legacy bare-string error tolerance preserves raw Value.
    let legacy = RpcError::from_wire(Value::Str("method xxxx not available".to_string()));
    assert_eq!(legacy.code, ErrorCode::GenericError);
    assert_eq!(legacy.message, "method xxxx not available");
    assert_eq!(
        legacy.raw,
        Some(Value::Str("method xxxx not available".to_string()))
    );
}

#[test]
fn typed_value_and_tuple_params_round_trip() {
    let params = (
        1_i8,
        -2_i16,
        3_i32,
        4_i64,
        5_u8,
        6_u16,
        7.5_f32,
        "ok".to_string(),
    )
        .into_params()
        .unwrap();
    let decoded = <(i8, i16, i32, i64, u8, u16, f32, String)>::from_params(params).unwrap();
    assert_eq!(decoded, (1, -2, 3, 4, 5, 6, 7.5_f32, "ok".to_string()));

    // Wrong parameter position error.
    let err =
        <(i32, bool)>::from_params(vec![Value::Int(1), Value::Str("nope".into())]).unwrap_err();
    assert!(err.message.contains("position 1"));

    // Arity mismatch.
    let err_short = <(i32, i32)>::from_params(vec![Value::Int(1)]).unwrap_err();
    assert!(err_short.message.contains("missing"));
    let err_long = <()>::from_params(vec![Value::Int(1)]).unwrap_err();
    assert!(err_long.message.contains("too many"));

    // Bytes accepts both Value::Bin and Value::Array(u8).
    let bin = arduino_router::Bytes::from_value(Value::Bin(vec![1, 2, 3])).unwrap();
    assert_eq!(bin.as_ref(), &[1, 2, 3]);
    let arr = arduino_router::Bytes::from_value(Value::Array(vec![
        Value::Int(4),
        Value::Uint(5),
        Value::Int(6),
    ]))
    .unwrap();
    assert_eq!(arr.as_ref(), &[4, 5, 6]);

    // HashMap round-trip.
    let mut map = HashMap::new();
    map.insert("k".to_string(), 99_u32);
    let back = HashMap::<String, u32>::from_value(map.clone().into_value()).unwrap();
    assert_eq!(back, map);
}

#[cfg(feature = "serde")]
mod serde_tests {
    use arduino_router::{from_value, to_value, Serde, Value};
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) enum Mode {
        Idle,
        Rate(u32),
        Point { x: i32, y: i32 },
    }

    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    pub(crate) struct Telemetry {
        pub(crate) bpm: f64,
        pub(crate) playing: bool,
        pub(crate) tags: Vec<String>,
        pub(crate) mode: Mode,
        pub(crate) note: Option<String>,
    }

    #[test]
    fn serde_round_trip_structs_and_enums() {
        let sample = Telemetry {
            bpm: 124.5,
            playing: true,
            tags: vec!["link".into(), "midi".into()],
            mode: Mode::Point { x: 3, y: 7 },
            note: Some("ready".into()),
        };

        let val = to_value(&sample).unwrap();
        assert!(matches!(val, Value::Map(_)));
        let decoded: Telemetry = from_value(val).unwrap();
        assert_eq!(decoded, sample);

        let wrapped = Serde(Mode::Rate(24));
        let v = arduino_router::IntoValue::try_into_value(wrapped.clone()).unwrap();
        let back: Serde<Mode> = arduino_router::FromValue::from_value(v).unwrap();
        assert_eq!(back, wrapped);
    }
}

#[cfg(feature = "tokio")]
mod async_tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use arduino_router::{
        decode_frame, encode_frame, Client, ClientConfig, CodecLimits, EncodeError, Error,
        ErrorCode, LifecycleEvent, ManagedClient, MonitorStream, ReconnectPolicy, RpcError,
        RpcMessage, Value,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;
    use tokio::time;

    async fn read_one_frame<S: AsyncReadExt + Unpin>(
        stream: &mut S,
        buf: &mut Vec<u8>,
    ) -> RpcMessage {
        let mut tmp = [0u8; 1024];
        loop {
            match decode_frame(buf, CodecLimits::default()) {
                Ok((msg, consumed)) => {
                    buf.drain(..consumed);
                    return msg;
                }
                Err(arduino_router::DecodeError::Incomplete) => {}
                Err(err) => panic!("decode failed: {err}"),
            }
            let n = time::timeout(Duration::from_secs(2), stream.read(&mut tmp))
                .await
                .expect("read timeout")
                .expect("read error");
            assert!(n > 0, "unexpected EOF");
            buf.extend_from_slice(&tmp[..n]);
        }
    }

    async fn send_one_frame<S: AsyncWriteExt + Unpin>(stream: &mut S, msg: &RpcMessage) {
        let mut out = Vec::new();
        encode_frame(msg, &mut out).unwrap();
        stream.write_all(&out).await.unwrap();
        stream.flush().await.unwrap();
    }

    fn spawn_mock_router<S>(stream: S, version: &'static str) -> tokio::task::JoinHandle<()>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(stream);
            let mut buf = Vec::new();
            let mut routes: HashMap<String, bool> = HashMap::new();
            let mut tmp = [0u8; 2048];
            let mut mon_data: Vec<u8> = b"boot ok\n".to_vec();
            let mut mon_connected = true;

            loop {
                let n = match reader.read(&mut tmp).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                buf.extend_from_slice(&tmp[..n]);

                while let Ok((msg, consumed)) = decode_frame(&buf, CodecLimits::default()) {
                    buf.drain(..consumed);
                    match msg {
                        RpcMessage::Request {
                            msgid,
                            method,
                            params,
                        } => {
                            let resp = match method.as_str() {
                                "$/register" => {
                                    let name = params[0].as_str().unwrap().to_string();
                                    if name == "taken/method" {
                                        RpcMessage::Response {
                                            msgid,
                                            error: RpcError::new(
                                                ErrorCode::RouteAlreadyExists,
                                                format!("route already exists: {name}"),
                                            )
                                            .to_wire(),
                                            result: Value::Nil,
                                        }
                                    } else {
                                        routes.insert(name, true);
                                        RpcMessage::Response {
                                            msgid,
                                            error: Value::Nil,
                                            result: Value::Bool(true),
                                        }
                                    }
                                }
                                "$/unregister" => {
                                    let name = params[0].as_str().unwrap();
                                    if routes.remove(name).is_some() {
                                        RpcMessage::Response {
                                            msgid,
                                            error: Value::Nil,
                                            result: Value::Bool(true),
                                        }
                                    } else {
                                        RpcMessage::Response {
                                            msgid,
                                            error: RpcError::new(
                                                ErrorCode::RouterGenericError,
                                                format!(
                                                    "route not registered by this client: {name}"
                                                ),
                                            )
                                            .to_wire(),
                                            result: Value::Nil,
                                        }
                                    }
                                }
                                "$/reset" => {
                                    routes.clear();
                                    RpcMessage::Response {
                                        msgid,
                                        error: Value::Nil,
                                        result: Value::Bool(true),
                                    }
                                }
                                "$/version" => RpcMessage::Response {
                                    msgid,
                                    error: Value::Nil,
                                    result: Value::Str(version.to_string()),
                                },
                                "$/setMaxMsgSize" => {
                                    let sz = params[0].as_i64().unwrap_or(0);
                                    if sz <= 127 {
                                        RpcMessage::Response {
                                            msgid,
                                            error: RpcError::invalid_params(
                                                "max buffer size must be greater than 127",
                                            )
                                            .to_wire(),
                                            result: Value::Nil,
                                        }
                                    } else {
                                        RpcMessage::Response {
                                            msgid,
                                            error: Value::Nil,
                                            result: Value::Bool(true),
                                        }
                                    }
                                }
                                "$/serial/open" | "$/serial/close" => RpcMessage::Response {
                                    msgid,
                                    error: Value::Nil,
                                    result: Value::Bool(true),
                                },
                                "mon/connected" => RpcMessage::Response {
                                    msgid,
                                    error: Value::Nil,
                                    result: Value::Bool(mon_connected),
                                },
                                "mon/reset" => {
                                    mon_connected = false;
                                    RpcMessage::Response {
                                        msgid,
                                        error: Value::Nil,
                                        result: Value::Bool(true),
                                    }
                                }
                                "mon/read" => {
                                    let max_n = params[0].as_u32().unwrap_or(0) as usize;
                                    let take = max_n.min(mon_data.len());
                                    let out: Vec<u8> = mon_data.drain(..take).collect();
                                    RpcMessage::Response {
                                        msgid,
                                        error: Value::Nil,
                                        result: Value::Bin(out),
                                    }
                                }
                                "mon/write" => {
                                    let len = match &params[0] {
                                        Value::Bin(b) => b.len(),
                                        Value::Str(s) => s.len(),
                                        _ => 0,
                                    };
                                    RpcMessage::Response {
                                        msgid,
                                        error: Value::Nil,
                                        result: Value::Uint(len as u64),
                                    }
                                }
                                "echo" => RpcMessage::Response {
                                    msgid,
                                    error: Value::Nil,
                                    result: params.into_iter().next().unwrap_or(Value::Nil),
                                },
                                _ => RpcMessage::Response {
                                    msgid,
                                    error: RpcError::new(
                                        ErrorCode::MethodNotAvailable,
                                        format!("method {method} not available"),
                                    )
                                    .to_wire(),
                                    result: Value::Nil,
                                },
                            };
                            send_one_frame(&mut writer, &resp).await;
                        }
                        RpcMessage::Notification { .. } | RpcMessage::Response { .. } => {}
                    }
                }
            }
        })
    }

    #[tokio::test]
    async fn client_multiplexing_timeouts_router_controls_and_monitor_rpcs() {
        let (client_stream, mut peer) = tokio::io::duplex(8192);
        let client = Client::from_stream(
            client_stream,
            ClientConfig {
                call_timeout: Duration::from_millis(120),
                max_outgoing_msg_size: Some(1024),
                ..ClientConfig::default()
            },
        );

        // Out-of-order response multiplexing: fire call A and call B concurrently.
        let c1 = client.clone();
        let fut_a = tokio::spawn(async move { c1.call::<_, i32>("alpha", (10,)).await });
        let c2 = client.clone();
        let fut_b = tokio::spawn(async move { c2.call::<_, String>("beta", ("hi",)).await });

        let mut peer_buf = Vec::new();
        let req1 = read_one_frame(&mut peer, &mut peer_buf).await;
        let req2 = read_one_frame(&mut peer, &mut peer_buf).await;

        let (id1, m1) = match req1 {
            RpcMessage::Request { msgid, method, .. } => (msgid, method),
            other => panic!("unexpected {other:?}"),
        };
        let (id2, m2) = match req2 {
            RpcMessage::Request { msgid, method, .. } => (msgid, method),
            other => panic!("unexpected {other:?}"),
        };

        // Reply in reverse order.
        let reply_for = |id: u32, m: &str| {
            let result = if m == "alpha" {
                Value::Int(42)
            } else {
                Value::Str("world".to_string())
            };
            RpcMessage::Response {
                msgid: id,
                error: Value::Nil,
                result,
            }
        };
        send_one_frame(&mut peer, &reply_for(id2, &m2)).await;
        send_one_frame(&mut peer, &reply_for(id1, &m1)).await;

        assert_eq!(fut_a.await.unwrap().unwrap(), 42);
        assert_eq!(fut_b.await.unwrap().unwrap(), "world");

        // Timeout cleans up waiter and late response is ignored safely.
        let timeout_task = tokio::spawn({
            let client = client.clone();
            async move { client.call::<_, i32>("never_replies", ()).await }
        });
        let timed_req = read_one_frame(&mut peer, &mut peer_buf).await;
        let timed_id = match timed_req {
            RpcMessage::Request { msgid, .. } => msgid,
            other => panic!("expected Request, got {other:?}"),
        };
        assert!(matches!(timeout_task.await.unwrap(), Err(Error::Timeout)));
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid: timed_id,
                error: Value::Nil,
                result: Value::Int(999),
            },
        )
        .await;

        // max_outgoing_msg_size rejects oversized outbound notify locally.
        let err = client
            .notify("too_big", ("a".repeat(1200),))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Encode(EncodeError::FrameTooLarge { .. })
        ));

        // $/setMaxMsgSize validates > 127 locally before sending.
        assert!(matches!(
            client.set_max_msg_size(127).await,
            Err(Error::InvalidArgument(_))
        ));
        assert!(matches!(
            client.notify_max_msg_size(64).await,
            Err(Error::InvalidArgument(_))
        ));

        client.close().await;
    }

    #[tokio::test]
    async fn router_control_methods_provide_errors_and_reentrancy() {
        let (client_stream, router_stream) = tokio::io::duplex(8192);
        let router_task = spawn_mock_router(router_stream, "0.12.0");
        let client = Client::from_stream(client_stream, ClientConfig::default());

        assert_eq!(client.version().await.unwrap(), "0.12.0");
        assert!(client.set_max_msg_size(256).await.unwrap());
        client.notify_max_msg_size(512).await.unwrap();
        assert!(client.serial_open("/dev/ttyHS1").await.unwrap());
        assert!(client.serial_close("/dev/ttyHS1").await.unwrap());

        assert!(client.monitor_connected().await.unwrap());
        assert_eq!(client.monitor_read(4).await.unwrap(), b"boot");
        assert_eq!(client.monitor_write(b"ping").await.unwrap(), 4);
        client.monitor_write_notify(b"!").await.unwrap();
        assert!(client.monitor_reset().await.unwrap());
        assert!(!client.monitor_connected().await.unwrap());

        // RouteAlreadyExists rolls back local handler cleanly.
        let err = client
            .provide("taken/method", || async { true })
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            Error::Rpc(RpcError {
                code: ErrorCode::RouteAlreadyExists,
                ..
            })
        ));

        // Successful provide + unprovide + reset.
        client
            .provide("host/add", |a: i32, b: i32| async move { a + b })
            .await
            .unwrap();
        assert!(client.unprovide("host/add").await.unwrap());
        assert!(client.reset().await.unwrap());

        client.close().await;
        let _ = router_task.await;
    }

    #[tokio::test]
    async fn incoming_request_notification_validation_and_reentrancy_concurrent_and_fifo() {
        for max_concurrent in [32_usize, 1_usize] {
            let (client_stream, mut peer) = tokio::io::duplex(8192);
            let client = Client::from_stream(
                client_stream,
                ClientConfig {
                    max_concurrent_handlers: max_concurrent,
                    ..ClientConfig::default()
                },
            );

            // Auto-acknowledge $/register in a background peer harness.
            let log = Arc::new(Mutex::new(Vec::<String>::new()));
            let log_c = Arc::clone(&log);

            let client_c = client.clone();
            let reg_task = tokio::spawn(async move {
                client_c
                    .provide("sum", |a: i32, b: i32| async move { a + b })
                    .await
                    .unwrap();
                client_c
                    .provide("checked_div", |a: f64, b: f64| async move {
                        if b == 0.0 {
                            Err(RpcError::invalid_params("zero divisor"))
                        } else {
                            Ok(a / b)
                        }
                    })
                    .await
                    .unwrap();
                let client_inner = client_c.clone();
                client_c
                    .provide("reentrant", move |v: String| {
                        let client_inner = client_inner.clone();
                        let log_c = Arc::clone(&log_c);
                        async move {
                            log_c.lock().unwrap().push(v.clone());
                            let echoed: String =
                                client_inner.call("peer/echo", (v,)).await.unwrap();
                            echoed
                        }
                    })
                    .await
                    .unwrap();
                client_c
                    .provide_raw("raw_echo", |params: Vec<Value>| async move {
                        Ok::<_, RpcError>(Value::Array(params))
                    })
                    .await
                    .unwrap();
            });

            let mut peer_buf = Vec::new();
            for _ in 0..4 {
                let frame = read_one_frame(&mut peer, &mut peer_buf).await;
                let RpcMessage::Request { msgid, method, .. } = frame else {
                    panic!("expected $/register");
                };
                assert_eq!(method, "$/register");
                send_one_frame(
                    &mut peer,
                    &RpcMessage::Response {
                        msgid,
                        error: Value::Nil,
                        result: Value::Bool(true),
                    },
                )
                .await;
            }
            reg_task.await.unwrap();

            // 1. Typed Request succeeds.
            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 10,
                    method: "sum".to_string(),
                    params: vec![Value::Int(20), Value::Int(22)],
                },
            )
            .await;
            assert_eq!(
                read_one_frame(&mut peer, &mut peer_buf).await,
                RpcMessage::Response {
                    msgid: 10,
                    error: Value::Nil,
                    result: Value::Int(42),
                }
            );

            // 2. Wrong type returns MALFORMED_CALL_ERR (253).
            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 11,
                    method: "sum".to_string(),
                    params: vec![Value::Int(1), Value::Str("bad".to_string())],
                },
            )
            .await;
            match read_one_frame(&mut peer, &mut peer_buf).await {
                RpcMessage::Response {
                    msgid: 11,
                    error,
                    result,
                } => {
                    assert!(result.is_nil());
                    assert_eq!(RpcError::from_wire(error).code, ErrorCode::MalformedCall);
                }
                other => panic!("unexpected {other:?}"),
            }

            // 3. Unknown method returns FUNCTION_NOT_FOUND_ERR (254).
            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 12,
                    method: "missing".to_string(),
                    params: vec![],
                },
            )
            .await;
            match read_one_frame(&mut peer, &mut peer_buf).await {
                RpcMessage::Response {
                    msgid: 12,
                    error,
                    result,
                } => {
                    assert!(result.is_nil());
                    assert_eq!(RpcError::from_wire(error).code, ErrorCode::FunctionNotFound);
                }
                other => panic!("unexpected {other:?}"),
            }

            // 4. Fallible handler error.
            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 13,
                    method: "checked_div".to_string(),
                    params: vec![Value::F64(5.0), Value::F64(0.0)],
                },
            )
            .await;
            match read_one_frame(&mut peer, &mut peer_buf).await {
                RpcMessage::Response {
                    msgid: 13,
                    error,
                    result,
                } => {
                    assert!(result.is_nil());
                    let rpc_err = RpcError::from_wire(error);
                    assert_eq!(rpc_err.code, ErrorCode::InvalidParams);
                    assert_eq!(rpc_err.message, "zero divisor");
                }
                other => panic!("unexpected {other:?}"),
            }

            // 5. Re-entrant call inside handler does not deadlock even in FIFO mode (1).
            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 14,
                    method: "reentrant".to_string(),
                    params: vec![Value::Str("first".to_string())],
                },
            )
            .await;
            let reentrant_call = read_one_frame(&mut peer, &mut peer_buf).await;
            let RpcMessage::Request {
                msgid: cb_id,
                method: cb_method,
                params: cb_params,
            } = reentrant_call
            else {
                panic!("expected re-entrant call");
            };
            assert_eq!(cb_method, "peer/echo");
            send_one_frame(
                &mut peer,
                &RpcMessage::Response {
                    msgid: cb_id,
                    error: Value::Nil,
                    result: cb_params[0].clone(),
                },
            )
            .await;
            assert_eq!(
                read_one_frame(&mut peer, &mut peer_buf).await,
                RpcMessage::Response {
                    msgid: 14,
                    error: Value::Nil,
                    result: Value::Str("first".to_string()),
                }
            );

            // 6. Notification invokes handler without emitting a response, and raw_echo works.
            let notify_tx_seen = Arc::new(tokio::sync::Notify::new());
            let notify_tx_seen_c = Arc::clone(&notify_tx_seen);
            let client_n = client.clone();
            let reg_n = tokio::spawn(async move {
                client_n
                    .provide("on_tick", move |beat: u32| {
                        let notify_tx_seen_c = Arc::clone(&notify_tx_seen_c);
                        async move {
                            assert_eq!(beat, 3);
                            notify_tx_seen_c.notify_one();
                        }
                    })
                    .await
                    .unwrap();
            });
            let reg_n_frame = read_one_frame(&mut peer, &mut peer_buf).await;
            let RpcMessage::Request {
                msgid: reg_n_id, ..
            } = reg_n_frame
            else {
                panic!("expected $/register for on_tick");
            };
            send_one_frame(
                &mut peer,
                &RpcMessage::Response {
                    msgid: reg_n_id,
                    error: Value::Nil,
                    result: Value::Bool(true),
                },
            )
            .await;
            reg_n.await.unwrap();

            send_one_frame(
                &mut peer,
                &RpcMessage::Notification {
                    method: "on_tick".to_string(),
                    params: vec![Value::Uint(3)],
                },
            )
            .await;
            time::timeout(Duration::from_secs(2), notify_tx_seen.notified())
                .await
                .expect("notification handler must fire");

            send_one_frame(
                &mut peer,
                &RpcMessage::Request {
                    msgid: 15,
                    method: "raw_echo".to_string(),
                    params: vec![Value::Bool(true)],
                },
            )
            .await;
            assert_eq!(
                read_one_frame(&mut peer, &mut peer_buf).await,
                RpcMessage::Response {
                    msgid: 15,
                    error: Value::Nil,
                    result: Value::Array(vec![Value::Bool(true)]),
                }
            );
            assert_eq!(*log.lock().unwrap(), vec!["first".to_string()]);

            client.close().await;
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_stream_pair_client_and_monitor_stream_round_trip() {
        let (client_sock, router_sock) = tokio::net::UnixStream::pair().unwrap();
        let server_task = spawn_mock_router(router_sock, "0.11.0");

        let client = Client::from_stream(client_sock, ClientConfig::default());
        assert_eq!(client.version().await.unwrap(), "0.11.0");
        assert_eq!(client.call::<_, i32>("echo", (123,)).await.unwrap(), 123);
        client.close().await;
        server_task.await.unwrap();

        let (mon_client_sock, mut mon_server_sock) = tokio::net::UnixStream::pair().unwrap();
        let mon_server = tokio::spawn(async move {
            let mut buf = [0u8; 32];
            let n = mon_server_sock.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"ping\n");
            mon_server_sock.write_all(b"pong\n").await.unwrap();
        });

        let mut mon = MonitorStream::from_stream(mon_client_sock);
        mon.write_str("ping\n").await.unwrap();
        let mut rx = [0u8; 32];
        let n = mon.read_bytes(&mut rx).await.unwrap();
        assert_eq!(&rx[..n], b"pong\n");
        mon_server.await.unwrap();
    }

    #[tokio::test]
    async fn default_socket_path_and_missing_endpoint_errors() {
        let resolved = arduino_router::default_unix_socket_path();
        assert!(!resolved.as_os_str().is_empty());

        #[cfg(unix)]
        {
            let err = Client::connect_unix("/nonexistent/arduino-router-missing.sock")
                .await
                .err()
                .expect("missing unix socket must fail");
            assert!(matches!(err, Error::Io(_)));
        }
    }

    #[tokio::test]
    async fn managed_client_reconnects_replays_registers_and_shuts_down() {
        let policy = ReconnectPolicy {
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            multiplier: 2,
        };
        let mut b = policy.initial_backoff;
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_millis(500));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(1));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(2));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(4));
        b = policy.next_backoff(b);
        assert_eq!(b, Duration::from_secs(5));
        assert_eq!(policy.next_backoff(b), Duration::from_secs(5));

        let attempts = Arc::new(AtomicUsize::new(0));
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let attempts_c = Arc::clone(&attempts);

        let managed = ManagedClient::builder_with_connector(move || {
            let idx = attempts_c.fetch_add(1, Ordering::AcqRel);
            let peer_tx = peer_tx.clone();
            async move {
                if idx == 0 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "router starting",
                    ))
                } else {
                    let (client_side, server_side) = tokio::io::duplex(4096);
                    let _ = peer_tx.send(server_side);
                    Ok(client_side)
                }
            }
        })
        .reconnect_policy(ReconnectPolicy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(80),
            multiplier: 2,
        })
        .provide("device/ping", || async { "pong" })
        .spawn();

        let mut events = managed.subscribe_events();

        // First attempt fails -> ConnectFailed, second succeeds -> Connected after $/register.
        let ev1 = time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, LifecycleEvent::ConnectFailed { .. }));

        let mut peer1 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut buf1 = Vec::new();
        let reg1 = read_one_frame(&mut peer1, &mut buf1).await;
        let RpcMessage::Request {
            msgid,
            method,
            params,
        } = reg1
        else {
            panic!("expected $/register");
        };
        assert_eq!(method, "$/register");
        assert_eq!(params, vec![Value::Str("device/ping".to_string())]);
        send_one_frame(
            &mut peer1,
            &RpcMessage::Response {
                msgid,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;

        let active1 = time::timeout(Duration::from_secs(2), managed.wait_connected())
            .await
            .unwrap()
            .unwrap();
        assert!(managed.is_connected());
        assert!(!active1.is_closed());

        // Drop session 1 -> Disconnected -> reconnects and replays $/register.
        drop(peer1);

        let mut peer2 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut buf2 = Vec::new();
        let reg2 = read_one_frame(&mut peer2, &mut buf2).await;
        let RpcMessage::Request { msgid: msgid2, .. } = reg2 else {
            panic!("expected $/register on reconnect");
        };
        send_one_frame(
            &mut peer2,
            &RpcMessage::Response {
                msgid: msgid2,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;

        // Verify RPC handler works after reconnect.
        send_one_frame(
            &mut peer2,
            &RpcMessage::Request {
                msgid: 77,
                method: "device/ping".to_string(),
                params: vec![],
            },
        )
        .await;
        assert_eq!(
            read_one_frame(&mut peer2, &mut buf2).await,
            RpcMessage::Response {
                msgid: 77,
                error: Value::Nil,
                result: Value::Str("pong".to_string()),
            }
        );

        time::timeout(Duration::from_secs(2), managed.shutdown())
            .await
            .expect("clean shutdown");
        assert!(!managed.is_connected());
        assert!(attempts.load(Ordering::Acquire) >= 3);
    }

    #[tokio::test]
    async fn managed_client_dynamic_provide_unprovide_and_disconnected_call() {
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let managed = ManagedClient::builder_with_connector(move || {
            let peer_tx = peer_tx.clone();
            async move {
                let (client_side, server_side) = tokio::io::duplex(4096);
                let _ = peer_tx.send(server_side);
                Ok(client_side)
            }
        })
        .reconnect_policy(ReconnectPolicy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(50),
            multiplier: 2,
        })
        .spawn();

        // Calling while disconnected fails fast with Error::Closed.
        assert!(matches!(
            managed.call::<_, i32>("ping", ()).await,
            Err(Error::Closed)
        ));

        let mut peer = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let _ = time::timeout(Duration::from_secs(2), managed.wait_connected())
            .await
            .unwrap()
            .unwrap();

        // Provide dynamically while connected -> sends $/register.
        let mut buf = Vec::new();
        let reg_fut = tokio::spawn({
            let managed = managed.clone();
            async move { managed.provide("dyn/hi", || async { "hello" }).await }
        });
        let RpcMessage::Request { msgid, method, .. } = read_one_frame(&mut peer, &mut buf).await
        else {
            panic!("expected $/register");
        };
        assert_eq!(method, "$/register");
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;
        reg_fut.await.unwrap().unwrap();

        // Unprovide dynamically -> sends $/unregister.
        let unreg_fut = tokio::spawn({
            let managed = managed.clone();
            async move { managed.unprovide("dyn/hi").await }
        });
        let RpcMessage::Request {
            msgid: u_id,
            method: u_method,
            ..
        } = read_one_frame(&mut peer, &mut buf).await
        else {
            panic!("expected $/unregister");
        };
        assert_eq!(u_method, "$/unregister");
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid: u_id,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;
        assert!(unreg_fut.await.unwrap().unwrap());

        managed.shutdown().await;
    }

    #[tokio::test]
    async fn malformed_frame_closes_client_and_wakes_pending_waiter_immediately() {
        let (client_stream, mut peer) = tokio::io::duplex(4096);
        let client = Client::from_stream(
            client_stream,
            ClientConfig {
                call_timeout: Duration::from_secs(30),
                ..ClientConfig::default()
            },
        );

        let pending_call = tokio::spawn({
            let client = client.clone();
            async move { client.call::<_, i32>("slow", ()).await }
        });

        let mut peer_buf = Vec::new();
        let _ = read_one_frame(&mut peer, &mut peer_buf).await;

        // Send reserved MessagePack tag 0xc1 -> malformed frame -> client closes immediately.
        peer.write_all(&[0xc1]).await.unwrap();
        peer.flush().await.unwrap();

        let err = time::timeout(Duration::from_secs(2), pending_call)
            .await
            .expect("pending call must wake immediately")
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, Error::Closed));

        time::timeout(Duration::from_secs(2), client.closed())
            .await
            .expect("client.closed() resolves");
        assert!(client.is_closed());
    }

    #[tokio::test]
    async fn fifo_mode_preserves_strict_arrival_order() {
        let (client_stream, mut peer) = tokio::io::duplex(4096);
        let client = Client::from_stream(
            client_stream,
            ClientConfig {
                max_concurrent_handlers: 1,
                ..ClientConfig::default()
            },
        );

        let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
        let done = Arc::new(tokio::sync::Notify::new());
        let seen_c = Arc::clone(&seen);
        let done_c = Arc::clone(&done);

        let reg = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .provide("seq", move |n: u32| {
                        let seen_c = Arc::clone(&seen_c);
                        let done_c = Arc::clone(&done_c);
                        async move {
                            if n == 1 {
                                time::sleep(Duration::from_millis(20)).await;
                            }
                            let mut guard = seen_c.lock().unwrap();
                            guard.push(n);
                            if guard.len() == 5 {
                                done_c.notify_one();
                            }
                        }
                    })
                    .await
                    .unwrap();
            }
        });
        let mut peer_buf = Vec::new();
        let RpcMessage::Request { msgid, .. } = read_one_frame(&mut peer, &mut peer_buf).await
        else {
            panic!("expected $/register");
        };
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;
        reg.await.unwrap();

        for n in 1..=5_u32 {
            send_one_frame(
                &mut peer,
                &RpcMessage::Notification {
                    method: "seq".to_string(),
                    params: vec![Value::Uint(u64::from(n))],
                },
            )
            .await;
        }
        time::timeout(Duration::from_secs(2), done.notified())
            .await
            .unwrap();
        assert_eq!(*seen.lock().unwrap(), vec![1, 2, 3, 4, 5]);
        client.close().await;
    }

    #[cfg(feature = "serde")]
    #[tokio::test]
    async fn serde_rpc_round_trip_over_client() {
        use arduino_router::Serde;

        let (client_stream, mut peer) = tokio::io::duplex(4096);
        let client = Client::from_stream(client_stream, ClientConfig::default());

        let reg = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .provide(
                        "tel/echo",
                        |t: Serde<super::serde_tests::Telemetry>| async move { t },
                    )
                    .await
                    .unwrap();
            }
        });
        let mut peer_buf = Vec::new();
        let RpcMessage::Request { msgid, .. } = read_one_frame(&mut peer, &mut peer_buf).await
        else {
            panic!("expected $/register");
        };
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;
        reg.await.unwrap();

        let sample = super::serde_tests::Telemetry {
            bpm: 120.0,
            playing: false,
            tags: vec!["unoq".into()],
            mode: super::serde_tests::Mode::Idle,
            note: None,
        };
        let encoded = arduino_router::to_value(&sample).unwrap();
        send_one_frame(
            &mut peer,
            &RpcMessage::Request {
                msgid: 99,
                method: "tel/echo".to_string(),
                params: vec![encoded.clone()],
            },
        )
        .await;
        assert_eq!(
            read_one_frame(&mut peer, &mut peer_buf).await,
            RpcMessage::Response {
                msgid: 99,
                error: Value::Nil,
                result: encoded,
            }
        );
        client.close().await;
    }

    #[tokio::test]
    async fn dropping_client_and_managed_client_closes_session_without_explicit_shutdown() {
        let (client_stream, mut peer) = tokio::io::duplex(4096);
        let client = Client::from_stream(client_stream, ClientConfig::default());
        drop(client);

        let mut tmp = [0u8; 16];
        let eof = time::timeout(Duration::from_secs(2), peer.read(&mut tmp))
            .await
            .expect("peer sees EOF on Client drop")
            .expect("read");
        assert_eq!(eof, 0);

        let attempts = Arc::new(AtomicUsize::new(0));
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let attempts_c = Arc::clone(&attempts);
        let managed = ManagedClient::builder_with_connector(move || {
            attempts_c.fetch_add(1, Ordering::AcqRel);
            let peer_tx = peer_tx.clone();
            async move {
                let (client_side, server_side) = tokio::io::duplex(4096);
                let _ = peer_tx.send(server_side);
                Ok(client_side)
            }
        })
        .reconnect_policy(ReconnectPolicy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(40),
            multiplier: 2,
        })
        .spawn();

        let mut managed_peer = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let _ = time::timeout(Duration::from_secs(2), managed.wait_connected())
            .await
            .unwrap()
            .unwrap();
        drop(managed);

        let eof_m = time::timeout(Duration::from_secs(2), managed_peer.read(&mut tmp))
            .await
            .expect("peer sees EOF on ManagedClient drop")
            .expect("read");
        assert_eq!(eof_m, 0);

        time::sleep(Duration::from_millis(80)).await;
        assert_eq!(attempts.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn managed_client_retries_when_register_returns_rpc_error() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let attempts_c = Arc::clone(&attempts);

        let managed = ManagedClient::builder_with_connector(move || {
            attempts_c.fetch_add(1, Ordering::AcqRel);
            let peer_tx = peer_tx.clone();
            async move {
                let (client_side, server_side) = tokio::io::duplex(4096);
                let _ = peer_tx.send(server_side);
                Ok(client_side)
            }
        })
        .reconnect_policy(ReconnectPolicy {
            initial_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(40),
            multiplier: 2,
        })
        .provide("retry/me", || async { "ok" })
        .spawn();

        let mut events = managed.subscribe_events();

        // Session 1: router rejects $/register with RouteAlreadyExists.
        let mut peer1 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut buf1 = Vec::new();
        let RpcMessage::Request { msgid: id1, .. } = read_one_frame(&mut peer1, &mut buf1).await
        else {
            panic!("expected $/register");
        };
        send_one_frame(
            &mut peer1,
            &RpcMessage::Response {
                msgid: id1,
                error: RpcError::new(
                    ErrorCode::RouteAlreadyExists,
                    "route already exists: retry/me",
                )
                .to_wire(),
                result: Value::Nil,
            },
        )
        .await;

        let ev1 = time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev1, LifecycleEvent::RegisterFailed { .. }));
        let ev2 = time::timeout(Duration::from_secs(2), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(ev2, LifecycleEvent::ConnectFailed { .. }));
        assert!(!managed.is_connected());

        let mut tmp = [0u8; 8];
        let eof1 = time::timeout(Duration::from_secs(2), peer1.read(&mut tmp))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(eof1, 0);

        // Session 2: router accepts $/register -> Connected.
        let mut peer2 = time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut buf2 = Vec::new();
        let RpcMessage::Request { msgid: id2, .. } = read_one_frame(&mut peer2, &mut buf2).await
        else {
            panic!("expected $/register on retry");
        };
        send_one_frame(
            &mut peer2,
            &RpcMessage::Response {
                msgid: id2,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;

        let _ = time::timeout(Duration::from_secs(2), managed.wait_connected())
            .await
            .unwrap()
            .unwrap();
        assert!(managed.is_connected());
        managed.shutdown().await;
    }

    #[cfg(feature = "serde")]
    #[tokio::test]
    async fn serde_serialization_error_propagates_on_outbound_and_handler_return() {
        use arduino_router::{IntoValue, Serde};

        assert!(Serde(u128::MAX).try_into_value().is_err());

        let (client_stream, mut peer) = tokio::io::duplex(4096);
        let client = Client::from_stream(client_stream, ClientConfig::default());

        let err_call = client
            .call::<_, Value>("echo", (Serde(u128::MAX),))
            .await
            .unwrap_err();
        assert!(matches!(err_call, Error::Conversion(_)));

        let err_notify = client
            .notify("echo", (Serde(u128::MAX),))
            .await
            .unwrap_err();
        assert!(matches!(err_notify, Error::Conversion(_)));

        let reg = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .provide("bad/ret", || async { Serde(u128::MAX) })
                    .await
                    .unwrap();
            }
        });
        let mut peer_buf = Vec::new();
        let RpcMessage::Request { msgid, .. } = read_one_frame(&mut peer, &mut peer_buf).await
        else {
            panic!("expected $/register");
        };
        send_one_frame(
            &mut peer,
            &RpcMessage::Response {
                msgid,
                error: Value::Nil,
                result: Value::Bool(true),
            },
        )
        .await;
        reg.await.unwrap();

        send_one_frame(
            &mut peer,
            &RpcMessage::Request {
                msgid: 7,
                method: "bad/ret".to_string(),
                params: vec![],
            },
        )
        .await;
        match read_one_frame(&mut peer, &mut peer_buf).await {
            RpcMessage::Response {
                msgid: 7,
                error,
                result,
            } => {
                assert!(result.is_nil());
                assert_eq!(RpcError::from_wire(error).code, ErrorCode::GenericError);
            }
            other => panic!("expected error response, got {other:?}"),
        }

        client.close().await;
    }

    #[tokio::test]
    async fn max_outgoing_msg_size_zero_rejects_outbound_frames() {
        let (client_stream, _peer) = tokio::io::duplex(1024);
        let client = Client::from_stream(
            client_stream,
            ClientConfig {
                max_outgoing_msg_size: Some(0),
                ..ClientConfig::default()
            },
        );
        let err = client.notify("x", ()).await.unwrap_err();
        assert!(matches!(
            err,
            Error::Encode(EncodeError::FrameTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn tcp_connect_managed_and_monitor_round_trip() {
        let listener = match tokio::net::TcpListener::bind("127.0.0.1:0").await {
            Ok(l) => l,
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(err) => panic!("unexpected bind error: {err}"),
        };
        let addr = listener.local_addr().unwrap();

        let accept_task = tokio::spawn(async move {
            let (s1, _) = listener.accept().await.unwrap();
            let r1 = spawn_mock_router(s1, "0.12.0");

            let (s2, _) = listener.accept().await.unwrap();
            let r2 = spawn_mock_router(s2, "0.12.0");

            let (mut s3, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 16];
            let n = s3.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"hi");
            s3.write_all(b"ok").await.unwrap();
            (r1, r2)
        });

        let client = match Client::connect_tcp(addr).await {
            Ok(c) => c,
            Err(Error::Io(ref err)) if err.kind() == std::io::ErrorKind::PermissionDenied => {
                accept_task.abort();
                return;
            }
            Err(err) => panic!("unexpected connect_tcp error: {err}"),
        };
        assert_eq!(client.version().await.unwrap(), "0.12.0");
        client.close().await;

        let managed = ManagedClient::builder_tcp(addr.to_string()).spawn();
        let mc = time::timeout(Duration::from_secs(2), managed.wait_connected())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(mc.version().await.unwrap(), "0.12.0");
        managed.shutdown().await;

        let mut mon = MonitorStream::connect(addr).await.unwrap();
        mon.write_str("hi").await.unwrap();
        let mut rx = [0u8; 8];
        let n = mon.read_bytes(&mut rx).await.unwrap();
        assert_eq!(&rx[..n], b"ok");

        let (r1, r2) = accept_task.await.unwrap();
        let _ = r1.await;
        let _ = r2.await;
    }
}
