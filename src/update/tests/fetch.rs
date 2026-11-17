//! The fetcher against a tiny local server thread: plain http on loopback
//! (debug builds only) for the protocol cases, a self-signed TLS server for
//! the certificate cases.

use super::*;
use crate::update::fetch::{url_allowed, HttpsFetcher};
use crate::update::url::Url;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

type Seen = Arc<Mutex<Vec<String>>>;

/// Serves `answers[n]` to the n-th connection (the last one repeats); returns
/// the port and every request head received.
fn serve(answers: Vec<Vec<u8>>, hold: Duration) -> (u16, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for n in 0.. {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut head = Vec::new();
            let mut buf = [0u8; 1024];
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(k) => head.extend_from_slice(&buf[..k]),
                }
            }
            log.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&head).into_owned());
            let answer = &answers[n.min(answers.len() - 1)];
            let _ = stream.write_all(answer);
            std::thread::sleep(hold);
        }
    });
    (port, seen)
}

fn ok(body: &str) -> Vec<u8> {
    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\nETag: \"abc\"\r\nContent-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
}

fn fetcher(port: u16, path: &str) -> HttpsFetcher {
    let mut f = HttpsFetcher::new(&format!("http://127.0.0.1:{port}{path}")).unwrap();
    f.set_total(Duration::from_secs(2));
    f
}

#[test]
fn a_200_returns_the_body_and_etag() {
    let (port, _) = serve(vec![ok("{\"a\":1}")], Duration::ZERO);
    assert_eq!(
        fetcher(port, "/latest.json").get(None),
        Ok(Fetched::Body {
            bytes: b"{\"a\":1}".to_vec(),
            etag: Some("\"abc\"".into())
        })
    );
}

#[test]
fn a_304_answers_a_stored_etag() {
    let (port, seen) = serve(
        vec![b"HTTP/1.0 304 Not Modified\r\n\r\n".to_vec()],
        Duration::ZERO,
    );
    assert_eq!(
        fetcher(port, "/latest.json").get(Some("\"abc\"")),
        Ok(Fetched::NotModified)
    );
    assert!(seen.lock().unwrap()[0].contains("If-None-Match: \"abc\"\r\n"));
}

#[test]
fn the_request_carries_only_the_allowed_headers() {
    let (port, seen) = serve(vec![ok("{}")], Duration::ZERO);
    fetcher(port, "/latest.json").get(Some("\"e\"")).unwrap();
    let head = seen.lock().unwrap()[0].clone();
    let names: Vec<&str> = head
        .lines()
        .skip(1)
        .filter(|l| !l.is_empty())
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(names, ["Host", "User-Agent", "Accept", "If-None-Match"]);
    assert!(head.starts_with("GET /latest.json HTTP/1.0\r\n"));
    assert!(head.contains(&format!(
        "User-Agent: DeskLink-update-check/{}\r\n",
        crate::config::VERSION
    )));
    let lower = head.to_ascii_lowercase();
    for forbidden in [
        "cookie",
        "authorization",
        "referer",
        "accept-language",
        "x-",
    ] {
        assert!(!lower.contains(forbidden), "{forbidden}");
    }
    // Nothing about this computer: not its name, not its user.
    let me = std::env::var("USER").unwrap_or_default();
    assert!(me.is_empty() || !head.contains(&me));
}

#[test]
fn a_hostile_etag_is_never_sent_or_kept() {
    let (port, seen) = serve(vec![ok("{}")], Duration::ZERO);
    fetcher(port, "/").get(Some("\"x\"\r\nX-Evil: 1")).unwrap();
    assert!(!seen.lock().unwrap()[0].contains("Evil"));
    let (port, _) = serve(
        vec![b"HTTP/1.0 200 OK\r\nETag: a\x01b\r\nContent-Length: 2\r\n\r\n{}".to_vec()],
        Duration::ZERO,
    );
    assert!(matches!(
        fetcher(port, "/").get(None),
        Ok(Fetched::Body { etag: None, .. })
    ));
}

#[test]
fn a_body_over_64_kib_is_refused_and_not_read_on() {
    let big = "x".repeat(70_000);
    // Declared too large: refused from the head alone.
    let (port, _) = serve(vec![ok(&big)], Duration::ZERO);
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Oversized));
    // No length given: the read stops at the cap.
    let mut body = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
    body.extend_from_slice(big.as_bytes());
    let (port, _) = serve(vec![body], Duration::ZERO);
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Oversized));
    // Exactly the cap is fine.
    let exact = "y".repeat(64 * 1024);
    let (port, _) = serve(vec![ok(&exact)], Duration::ZERO);
    assert!(matches!(
        fetcher(port, "/").get(None),
        Ok(Fetched::Body { .. })
    ));
}

#[test]
fn redirects_stay_on_the_host_and_are_limited() {
    let (port, _) = serve(
        vec![
            b"HTTP/1.0 301 Moved\r\nLocation: /second\r\n\r\n".to_vec(),
            ok("{}"),
        ],
        Duration::ZERO,
    );
    assert!(matches!(
        fetcher(port, "/first").get(None),
        Ok(Fetched::Body { .. })
    ));
    // Another host: refused.
    let (port, _) = serve(
        vec![b"HTTP/1.0 302 Found\r\nLocation: http://127.0.0.2:1/x\r\n\r\n".to_vec()],
        Duration::ZERO,
    );
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Parse));
    // Another port on the same host: refused.
    let (port, _) = serve(
        vec![b"HTTP/1.0 302 Found\r\nLocation: http://127.0.0.1:1/x\r\n\r\n".to_vec()],
        Duration::ZERO,
    );
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Parse));
    // A scheme change, a protocol-relative link, no Location: refused.
    for location in [
        "https://127.0.0.1/x",
        "//evil.example/x",
        "ftp://127.0.0.1/x",
    ] {
        let (port, _) = serve(
            vec![format!("HTTP/1.0 302 Found\r\nLocation: {location}\r\n\r\n").into_bytes()],
            Duration::ZERO,
        );
        assert_eq!(
            fetcher(port, "/").get(None),
            Err(FetchError::Parse),
            "{location}"
        );
    }
    let (port, _) = serve(vec![b"HTTP/1.0 302 Found\r\n\r\n".to_vec()], Duration::ZERO);
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Parse));
    // A loop ends after 3 hops.
    let (port, seen) = serve(
        vec![b"HTTP/1.0 302 Found\r\nLocation: /again\r\n\r\n".to_vec()],
        Duration::ZERO,
    );
    assert_eq!(fetcher(port, "/").get(None), Err(FetchError::Parse));
    assert_eq!(seen.lock().unwrap().len(), 4);
}

#[test]
fn errors_and_resets_are_codes() {
    for (status, want) in [
        ("500 Server Error", FetchError::Http),
        ("404 Not Found", FetchError::Http),
        ("204 No Content", FetchError::Http),
    ] {
        let (port, _) = serve(
            vec![format!("HTTP/1.0 {status}\r\n\r\n").into_bytes()],
            Duration::ZERO,
        );
        assert_eq!(fetcher(port, "/").get(None), Err(want), "{status}");
    }
    for junk in [
        &b"garbage\r\n\r\n"[..],
        b"HTTP/2 200\r\n\r\n",
        b"HTTP/1.0 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n",
        b"HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nshort",
    ] {
        let (port, _) = serve(vec![junk.to_vec()], Duration::ZERO);
        assert_eq!(
            fetcher(port, "/").get(None),
            Err(FetchError::Parse),
            "{junk:?}"
        );
    }
    // Closed before any answer.
    let (port, _) = serve(vec![Vec::new()], Duration::ZERO);
    assert!(fetcher(port, "/").get(None).is_err());
    // Nothing listening.
    let closed = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_eq!(fetcher(closed, "/").get(None), Err(FetchError::Offline));
}

#[test]
fn a_slow_server_times_out_within_the_budget() {
    let (port, _) = serve(
        vec![b"HTTP/1.0 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_vec()],
        Duration::from_secs(3),
    );
    let mut f = fetcher(port, "/");
    f.set_total(Duration::from_millis(400));
    let started = Instant::now();
    assert_eq!(f.get(None), Err(FetchError::Timeout));
    assert!(started.elapsed() < Duration::from_secs(2));
    // One that never answers at all.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = silent.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let held = silent.accept();
        std::thread::sleep(Duration::from_secs(3));
        drop(held);
    });
    let mut f = fetcher(port, "/");
    f.set_total(Duration::from_millis(400));
    assert_eq!(f.get(None), Err(FetchError::Timeout));
}

#[test]
fn which_addresses_a_build_may_fetch() {
    let p = |t: &str| Url::parse(t).unwrap();
    assert!(url_allowed(
        &p("https://desklink-updates.web.app/latest.json"),
        false
    ));
    assert!(
        !url_allowed(&p("http://desklink-updates.web.app/latest.json"), true),
        "http only for loopback"
    );
    assert!(
        !url_allowed(&p("http://127.0.0.1:8099/latest.json"), false),
        "never in a release build"
    );
    assert!(url_allowed(&p("http://127.0.0.1:8099/latest.json"), true));
    assert!(url_allowed(&p("http://localhost:8099/x"), true));
    assert!(!url_allowed(&p("https://example.com:8443/x"), false));
    assert!(HttpsFetcher::new("").is_none());
    assert!(HttpsFetcher::new("ftp://x/").is_none());
    assert!(HttpsFetcher::new("https://user@example.com/").is_none());
}

mod tls {
    use super::*;
    use openssl::asn1::Asn1Time;
    use openssl::bn::BigNum;
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::ssl::{SslAcceptor, SslMethod};
    use openssl::x509::extension::SubjectAlternativeName;
    use openssl::x509::{X509NameBuilder, X509};

    fn cert(san_ip: &str) -> (X509, PKey<openssl::pkey::Private>) {
        let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "test").unwrap();
        let name = name.build();
        let mut b = X509::builder().unwrap();
        b.set_version(2).unwrap();
        b.set_serial_number(&BigNum::from_u32(1).unwrap().to_asn1_integer().unwrap())
            .unwrap();
        b.set_subject_name(&name).unwrap();
        b.set_issuer_name(&name).unwrap();
        b.set_pubkey(&key).unwrap();
        b.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        b.set_not_after(&Asn1Time::days_from_now(2).unwrap())
            .unwrap();
        let san = SubjectAlternativeName::new()
            .ip(san_ip)
            .build(&b.x509v3_context(None, None))
            .unwrap();
        b.append_extension(san).unwrap();
        b.sign(&key, MessageDigest::sha256()).unwrap();
        (b.build(), key)
    }

    fn serve_tls(cert: &X509, key: &PKey<openssl::pkey::Private>, answer: Vec<u8>) -> u16 {
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls()).unwrap();
        acceptor.set_private_key(key).unwrap();
        acceptor.set_certificate(cert).unwrap();
        let acceptor = acceptor.build();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                if let Ok(mut tls) = acceptor.accept(stream) {
                    let mut buf = [0u8; 1024];
                    let _ = tls.read(&mut buf);
                    let _ = tls.write_all(&answer);
                    let _ = tls.shutdown();
                }
            }
        });
        port
    }

    fn client(port: u16, root: Option<X509>) -> HttpsFetcher {
        let mut f = HttpsFetcher::new(&format!("https://127.0.0.1:{port}/latest.json")).unwrap();
        f.extra_root = root;
        f.set_total(Duration::from_secs(5));
        f
    }

    #[test]
    fn a_trusted_certificate_for_the_right_address_is_accepted() {
        let (cert, key) = cert("127.0.0.1");
        let port = serve_tls(&cert, &key, ok("{\"tls\":true}"));
        assert!(matches!(
            client(port, Some(cert)).get(None),
            Ok(Fetched::Body { .. })
        ));
    }

    #[test]
    fn an_untrusted_or_mismatched_certificate_is_refused() {
        let (cert_ok, key) = cert("127.0.0.1");
        let port = serve_tls(&cert_ok, &key, ok("{}"));
        assert!(
            client(port, None).get(None).is_err(),
            "not trusted by the system store"
        );
        let (wrong, key) = cert("10.9.8.7");
        let port = serve_tls(&wrong, &key, ok("{}"));
        assert!(
            client(port, Some(wrong)).get(None).is_err(),
            "right chain, wrong host"
        );
    }
}
