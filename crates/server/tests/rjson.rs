//! The JSON layer must match Ruby's json 2.21.2 byte for byte: these vectors
//! were produced by `JSON.generate` / `Float#to_s` inside the rw-baseline
//! container (Ruby 4.0.0), see tests/fixtures/ruby_float_vectors.json.

use railwatch_server::rjson::{self, Value, ruby};

#[test]
fn floats_generate_exactly_like_ruby_json_and_to_s() {
    let raw = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/ruby_float_vectors.json")).unwrap();
    let vectors: Vec<(String, String, String)> = serde_json::from_str(&raw).unwrap();
    assert!(vectors.len() > 200);
    for (hex, json, to_s) in vectors {
        let f = f64::from_bits(u64::from_str_radix(&hex, 16).unwrap());
        assert_eq!(rjson::generate(&Value::Float(f)).unwrap(), json, "JSON.generate({hex})");
        assert_eq!(ruby::float_to_s(f), to_s, "Float#to_s({hex})");
    }
}

#[test]
fn generate_matches_ruby_for_strings_and_structure() {
    let v = rjson::parse(br#"{"a":"</x>&\u2028\u2029/\t\u0001\u007f\u00e9","n":null,"l":[1,2.5,true]}"#).unwrap();
    assert_eq!(rjson::generate(&v).unwrap(), "{\"a\":\"</x>&\u{2028}\u{2029}/\\t\\u0001\u{7f}é\",\"n\":null,\"l\":[1,2.5,true]}");
    // Rails' render json: escapes html entities and the JS separators.
    assert_eq!(rjson::generate_escaped(&v).unwrap(), "{\"a\":\"\\u003c/x\\u003e\\u0026\\u2028\\u2029/\\t\\u0001\u{7f}é\",\"n\":null,\"l\":[1,2.5,true]}");
    assert_eq!(rjson::generate(&Value::Str(vec![0xff])).unwrap_err().0, "source sequence is illegal/malformed utf-8");
    assert_eq!(rjson::generate(&Value::Float(f64::INFINITY)).unwrap_err().0, "Infinity not allowed in JSON");
}

#[test]
fn parse_keeps_ruby_semantics() {
    // Last duplicate wins, insertion order kept; bignums and -0 survive.
    let v = rjson::parse(br#"{"a":1,"b":2,"a":3}"#).unwrap();
    assert_eq!(rjson::generate(&v).unwrap(), r#"{"a":3,"b":2}"#);
    let v = rjson::parse(b"[123456789012345678901234567890, 1e400, -0, 1E2, 1.50, 01.5e-400]").err();
    assert!(v.is_some()); // leading zero
    let v = rjson::parse(b"[123456789012345678901234567890, -0.0, 1E2, 1.50]").unwrap();
    assert_eq!(rjson::generate(&v).unwrap(), "[123456789012345678901234567890,-0.0,100.0,1.5]");
    // Comments are accepted; a lone low surrogate is encoded as is.
    assert_eq!(rjson::parse(b"/*a*/[1]// x\n").unwrap(), Value::Array(vec![Value::Int(1)]));
    assert_eq!(rjson::parse(br#"["\udc00"]"#).unwrap(), Value::Array(vec![Value::Str(vec![0xED, 0xB0, 0x80])]));
    // Raw invalid UTF-8 is kept, not rejected (Ruby does not validate).
    assert_eq!(rjson::parse(b"[\"\xff\"]").unwrap(), Value::Array(vec![Value::Str(vec![0xff])]));
    // 100 levels parse, 101 do not.
    let ok = format!("{}{}", "[".repeat(100), "]".repeat(100));
    assert!(rjson::parse(ok.as_bytes()).is_ok());
    let deep = format!("{}{}", "[".repeat(102), "]".repeat(102));
    assert_eq!(rjson::parse(deep.as_bytes()).unwrap_err().0, "nesting of 101 is too deep");
}

#[test]
fn parse_errors_carry_ruby_messages() {
    let cases: &[(&[u8], &str)] = &[
        (br#"{"t":"x""#, "expected ',' or '}' after object value, got: EOF at line 1 column 9"),
        (b"hello", "unexpected character: 'hello' at line 1 column 1"),
        (br#"{"a":1}x"#, "unexpected token at end of stream 'x' at line 1 column 8"),
        (b"[1,2", "expected ',' or ']' after array value at line 1 column 5"),
        (br#"{"a""#, "expected ':' after object key at line 1 column 5"),
        (b"", "unexpected end of input at line 1 column 1"),
        (b"{bad", "expected object key, got 'bad' at line 1 column 2"),
        (b"[1,]", "unexpected character: ']' at line 1 column 4"),
        (b"[NaN]", "unexpected token 'NaN]' at line 1 column 2"),
        (b"[01]", "invalid number: '01]' at line 1 column 2"),
        (b"{\"a\":}", "unexpected character: '}' at line 1 column 6"),
        (b"{1:2}", "expected object key, got '1:2}' at line 1 column 2"),
        (b"[\"a\tb\x01c\"]", "invalid ASCII control character in string: \tb\u{1}c\"] at line 1 column 4"),
        (br#"["\q"]"#, "invalid escape character in string: '\\q\"]' at line 1 column 3"),
        (br#"["\ud800x"]"#, "incomplete surrogate pair at '\\ud800x\"]' at line 1 column 3"),
    ];
    for (input, message) in cases {
        assert_eq!(rjson::parse(input).unwrap_err().0, *message, "{}", String::from_utf8_lossy(input));
    }
}

#[test]
fn ruby_conversions() {
    assert_eq!(
        ruby::inspect(&rjson::parse(br#"{"a":1.0e-05,"b":[18446744073709551616,-0.0,null]}"#).unwrap()),
        br#"{"a" => 1.0e-05, "b" => [18446744073709551616, -0.0, nil]}"#.to_vec()
    );
    assert_eq!(
        ruby::inspect(&Value::Str(b"a\"b\\\n\x1b\x00\x7f\xc3\xa9\xe2\x80\x8b#{x}".to_vec())),
        b"\"a\\\"b\\\\\\n\\e\\u0000\\u007F\xc3\xa9\xe2\x80\x8b\\#{x}\"".to_vec()
    );
    assert_eq!(ruby::inspect(&Value::Str(b"a\xffb".to_vec())), b"\"a\\xFFb\"".to_vec());
    for (s, ok) in [
        ("1.5", Some(1.5)),
        (" 2.5 ", Some(2.5)),
        ("1_000.5", Some(1000.5)),
        ("0x10", Some(16.0)),
        (".5", Some(0.5)),
        ("1.", Some(1.0)),
        ("1e5", Some(1e5)),
        ("+3", Some(3.0)),
        ("", None),
        ("abc", None),
        ("1__0", None),
        ("_1", None),
        ("1e", None),
        ("Infinity", None),
        ("1 2", None),
    ] {
        assert_eq!(ruby::float(&Value::Str(s.as_bytes().to_vec())).ok(), ok, "Float({s:?})");
    }
    assert_eq!(ruby::float(&Value::Str(b"abc".to_vec())).unwrap_err().1, "invalid value for Float(): \"abc\"");
    assert_eq!(ruby::float(&Value::Bool(true)).unwrap_err(), ("TypeError", "can't convert true into Float".into()));
    // Time.at(1767225600.123).usec == 122999 in Ruby: it keeps the exact binary value.
    assert_eq!(ruby::time_at_float(1767225600.123), (1767225600, 122999));
    assert_eq!(ruby::time_at_float(1767225600.1234567), (1767225600, 123456));
    assert_eq!(ruby::time_at_float(-1.5), (-2, 500000));
    assert_eq!(ruby::scrub(b"\xF0\x9F\x98x"), "\u{FFFD}x".as_bytes());
    assert_eq!(ruby::scrub(b"\xED\xB0\x80"), "\u{FFFD}\u{FFFD}\u{FFFD}".as_bytes());
    assert!(ruby::blank(" \u{3000}\u{85}".as_bytes()));
    assert!(!ruby::blank("\u{200b}".as_bytes()));
}
