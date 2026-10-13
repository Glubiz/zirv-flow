//! Tests against real brokers. They are `#[ignore]`d: `scripts/kafka-fixture.sh` starts the
//! brokers and prints the variables they read (`ZIRV_KAFKA_BROKER`, `ZIRV_KAFKA_SASL_BROKER`,
//! `ZIRV_KAFKA_TLS_FRONT_PORT`, `ZIRV_KAFKA_TLS_BACKEND_PORT`). The brokers advertise the name
//! `broker.test`, which these tests resolve to a loopback address through an injected lookup.

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::io;
    use std::net::IpAddr;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use chrono::Utc;
    use futures_util::StreamExt;
    use rcgen::{
        BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa,
        KeyPair,
    };
    use rskafka::client::partition::{Compression, UnknownTopicHandling};
    use rskafka::record::Record as KafkaRecord;
    use serde_json::{Value, json};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::task::JoinHandle;
    use tokio_rustls::TlsAcceptor;
    use zirv_flow_core::SecretString;
    use zirv_flow_core::egress::EgressPolicy;

    use super::super::super::context::SourceContext;
    use super::super::super::credential::Credential;
    use super::super::super::error::SourceError;
    use super::super::super::source::Source;
    use super::super::super::types::{Batch, Checkpoint, Record, Refused, StreamConfig, SyncMode};
    use super::super::Kafka;
    use super::super::client::{Connect, Mechanism, Session};
    use super::super::positions::{PartitionPosition, Positions};
    use crate::http::{HttpLimits, Lookup};

    struct Table(BTreeMap<String, Vec<IpAddr>>);

    impl Lookup for Table {
        fn lookup<'a>(
            &'a self,
            host: &'a str,
        ) -> Pin<Box<dyn Future<Output = io::Result<Vec<IpAddr>>> + Send + 'a>> {
            Box::pin(async move {
                self.0
                    .get(host)
                    .cloned()
                    .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            })
        }
    }

    fn lookup(entries: &[(&str, &str)]) -> Arc<dyn Lookup> {
        Arc::new(Table(
            entries
                .iter()
                .map(|(host, ip)| ((*host).to_owned(), vec![ip.parse().unwrap()]))
                .collect(),
        ))
    }

    fn env(name: &str) -> String {
        std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set; run kafka-fixture.sh"))
    }

    fn context(allow: &[&str], names: &[(&str, &str)]) -> SourceContext {
        let entries: Vec<String> = allow.iter().map(|entry| (*entry).to_owned()).collect();
        SourceContext::new(
            Arc::new(EgressPolicy::parse(&entries).unwrap()),
            HttpLimits::default(),
            "kafka",
            "source-1",
            Arc::default(),
        )
        .unwrap()
        .with_lookup(lookup(names))
    }

    /// A context that reaches `broker.test` at the loopback address.
    fn local() -> SourceContext {
        context(&["127.0.0.0/8"], &[("broker.test", "127.0.0.1")])
    }

    fn unique(prefix: &str) -> String {
        format!(
            "{prefix}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn credential(pairs: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in pairs {
            credential.insert(*key, SecretString::new((*value).to_owned()));
        }
        credential
    }

    fn config(broker: &str, topics: &str, extra: Value) -> Value {
        let mut config = json!({"brokers": broker, "topics": topics, "security": "plaintext"});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        config
    }

    fn record(key: Option<&str>, value: Option<&str>) -> KafkaRecord {
        KafkaRecord {
            key: key.map(|key| key.as_bytes().to_vec()),
            value: value.map(|value| value.as_bytes().to_vec()),
            headers: BTreeMap::new(),
            timestamp: Utc::now(),
        }
    }

    async fn admin(
        ctx: &SourceContext,
        broker: &str,
        extra: Option<(Mechanism, &str, &str)>,
    ) -> Arc<Session> {
        let username = extra.map(|(_, user, _)| SecretString::new(user.to_owned()));
        let password = extra.map(|(_, _, pass)| SecretString::new(pass.to_owned()));
        let connect = Connect {
            brokers: vec![broker.to_owned()],
            tls: None,
            sasl: extra.map(|(mechanism, _, _)| {
                (
                    mechanism,
                    username.as_ref().unwrap(),
                    password.as_ref().unwrap(),
                )
            }),
        };
        Session::connect(ctx, &connect).await.unwrap()
    }

    async fn create_topic(session: &Session, topic: &str, partitions: i32) {
        session
            .raw()
            .controller_client()
            .unwrap()
            .create_topic(topic, partitions, 1, 10_000)
            .await
            .unwrap();
        // The topic is visible to metadata a moment after it is created.
        for _ in 0..50 {
            if session
                .partitions(topic)
                .await
                .is_ok_and(|p| p.len() == partitions as usize)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the topic did not appear");
    }

    async fn produce(
        session: &Session,
        topic: &str,
        partition: i32,
        records: Vec<KafkaRecord>,
        compression: Compression,
    ) {
        let client = session
            .raw()
            .partition_client(topic, partition, UnknownTopicHandling::Retry)
            .await
            .unwrap();
        client.produce(records, compression).await.unwrap();
    }

    fn stream_config(topic: &str) -> StreamConfig {
        StreamConfig {
            name: topic.to_owned(),
            sync_mode: SyncMode::Stream,
            cursor_field: None,
            primary_key: vec!["_key".to_owned()],
        }
    }

    /// Reads a topic to its end; the records, the refusals and the final positions.
    #[derive(Debug)]
    struct Read {
        records: Vec<Record>,
        refused: Vec<Refused>,
        positions: Positions,
    }

    async fn read_topic(
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
        topic: &str,
        start: Option<Checkpoint>,
    ) -> Result<Read, SourceError> {
        let stream = stream_config(topic);
        let mut batches = Kafka.read(ctx, config, credential, &stream, start);
        let mut read = Read {
            records: Vec::new(),
            refused: Vec::new(),
            positions: Positions::default(),
        };
        while let Some(batch) = batches.next().await {
            let Batch {
                records,
                checkpoint,
                refused,
            } = batch?;
            read.records.extend(records);
            read.refused.extend(refused);
            read.positions = Positions::from_checkpoint(&checkpoint).unwrap();
        }
        Ok(read)
    }

    fn other(error: SourceError) -> String {
        match error {
            SourceError::Other(code) => code,
            other => format!("{other:?}"),
        }
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn gzip_snappy_lz4_and_zstd_batches_all_decode() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("codecs");
        create_topic(&session, &topic, 1).await;
        let codecs = [
            Compression::NoCompression,
            Compression::Gzip,
            Compression::Snappy,
            Compression::Lz4,
            Compression::Zstd,
        ];
        for (index, codec) in codecs.into_iter().enumerate() {
            let records = (0..3)
                .map(|n| {
                    record(
                        Some(&format!("k{index}-{n}")),
                        Some(&format!(
                            r#"{{"codec":{index},"n":{n},"pad":"{}"}}"#,
                            "x".repeat(200)
                        )),
                    )
                })
                .collect();
            produce(&session, &topic, 0, records, codec).await;
        }

        let read = read_topic(
            &ctx,
            &config(&broker, &topic, json!({})),
            &Credential::new(),
            &topic,
            None,
        )
        .await
        .unwrap();

        assert_eq!(read.records.len(), 15);
        for (index, chunk) in read.records.chunks(3).enumerate() {
            for (n, found) in chunk.iter().enumerate() {
                assert_eq!(found.key, format!("k{index}-{n}"));
                assert_eq!(found.data["codec"], index);
            }
        }
        assert_eq!(read.positions.get(0).unwrap().next, 15);
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn discovery_samples_fifty_messages_and_reports_the_virtual_fields() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("discover");
        let empty = unique("empty");
        create_topic(&session, &topic, 1).await;
        create_topic(&session, &empty, 1).await;
        let mut records: Vec<KafkaRecord> = (0..50)
            .map(|n| {
                record(
                    Some(&format!("k{n}")),
                    Some(&format!(r#"{{"email":"a{n}@b.c","age":{n}}}"#)),
                )
            })
            .collect();
        records.extend((50..60).map(|n| record(Some(&format!("k{n}")), Some(r#"{"late":true}"#))));
        records.push(record(Some("gone"), None));
        produce(&session, &topic, 0, records, Compression::NoCompression).await;
        let both = format!("{topic},{empty}");

        let streams = Kafka
            .discover(&ctx, &config(&broker, &both, json!({})), &Credential::new())
            .await
            .unwrap();

        let described = streams.iter().find(|s| s.name == topic).unwrap();
        let names: Vec<_> = described.fields.iter().map(|f| f.name.as_str()).collect();
        assert!(names.contains(&"_key") && names.contains(&"_timestamp"));
        assert!(names.contains(&"email") && names.contains(&"age"));
        assert!(
            !names.contains(&"late"),
            "only the first 50 messages are sampled"
        );
        assert_eq!(described.sync_modes, [SyncMode::Stream]);
        assert_eq!(described.default_primary_key, ["_key"]);
        let blank = streams.iter().find(|s| s.name == empty).unwrap();
        assert_eq!(blank.fields.len(), 2);
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn check_succeeds_and_a_missing_topic_is_named_by_code() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("check");
        create_topic(&session, &topic, 1).await;

        let ok = Kafka
            .check(
                &ctx,
                &config(&broker, &topic, json!({})),
                &Credential::new(),
            )
            .await;
        let missing = Kafka
            .check(
                &ctx,
                &config(&broker, "no-such-topic-zf", json!({})),
                &Credential::new(),
            )
            .await;

        assert_eq!(ok, Ok(()));
        assert_eq!(other(missing.unwrap_err()), "kafka_topic_missing");
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn partitions_are_discovered_at_the_start_and_a_new_one_starts_at_the_earliest() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("parts");
        create_topic(&session, &topic, 3).await;
        for partition in 0..3 {
            let records = (0..3)
                .map(|n| record(Some(&format!("p{partition}-{n}")), Some(r#"{"v":1}"#)))
                .collect();
            produce(
                &session,
                &topic,
                partition,
                records,
                Compression::NoCompression,
            )
            .await;
        }
        // Offsets are stored for two of the three partitions.
        let mut stored = Positions::default();
        stored.set(0, PartitionPosition { next: 2, end: None });
        stored.set(1, PartitionPosition { next: 3, end: None });

        let read = read_topic(
            &ctx,
            &config(&broker, &topic, json!({})),
            &Credential::new(),
            &topic,
            Some(stored.to_checkpoint()),
        )
        .await
        .unwrap();

        let mut occurrences: Vec<_> = read
            .records
            .iter()
            .filter_map(|r| r.occurrence.clone())
            .collect();
        occurrences.sort();
        assert_eq!(occurrences, ["0/2", "2/0", "2/1", "2/2"]);
        assert_eq!(read.positions.get(0).unwrap().next, 3);
        assert_eq!(read.positions.get(1).unwrap().next, 3);
        assert_eq!(read.positions.get(2).unwrap().next, 3);
        assert_eq!(read.positions.get(2).unwrap().end, Some(3));
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn tombstones_and_malformed_messages_are_told_apart_and_the_partition_goes_on() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("mixed");
        create_topic(&session, &topic, 1).await;
        let records = vec![
            record(Some("u1"), Some(r#"{"email":"a@b.c"}"#)),
            record(Some("u1"), None),
            record(None, Some(r#"{"email":"x"}"#)),
            record(Some("u2"), Some("not json")),
            record(Some("u3"), Some("[1]")),
            record(Some("u4"), Some(r#"{"email":"z@b.c"}"#)),
        ];
        produce(&session, &topic, 0, records, Compression::NoCompression).await;

        let read = read_topic(
            &ctx,
            &config(&broker, &topic, json!({})),
            &Credential::new(),
            &topic,
            None,
        )
        .await
        .unwrap();

        let shape: Vec<_> = read
            .records
            .iter()
            .map(|r| (r.key.as_str(), r.deleted))
            .collect();
        assert_eq!(shape, [("u1", false), ("u1", true), ("u4", false)]);
        let refused: Vec<_> = read
            .refused
            .iter()
            .map(|r| (r.location.as_str(), r.code))
            .collect();
        assert_eq!(
            refused,
            [
                ("partition 0 offset 2", "kafka_key_missing"),
                ("partition 0 offset 3", "kafka_value_not_json"),
                ("partition 0 offset 4", "kafka_value_not_object"),
            ]
        );
        assert_eq!(read.positions.get(0).unwrap().next, 6);
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn resuming_from_the_final_positions_reads_nothing_more() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("resume");
        create_topic(&session, &topic, 1).await;
        let records = (0..4)
            .map(|n| record(Some(&format!("k{n}")), Some(r#"{"v":1}"#)))
            .collect();
        produce(&session, &topic, 0, records, Compression::NoCompression).await;
        let cfg = config(&broker, &topic, json!({}));

        let first = read_topic(&ctx, &cfg, &Credential::new(), &topic, None)
            .await
            .unwrap();
        let second = read_topic(
            &ctx,
            &cfg,
            &Credential::new(),
            &topic,
            Some(first.positions.to_checkpoint()),
        )
        .await
        .unwrap();

        assert_eq!(first.records.len(), 4);
        assert!(second.records.is_empty());
        assert_eq!(second.positions, first.positions);
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn a_stored_offset_past_the_end_fails_with_its_code() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &broker, None).await;
        let topic = unique("recreated");
        create_topic(&session, &topic, 1).await;
        let mut stored = Positions::default();
        stored.set(
            0,
            PartitionPosition {
                next: 99,
                end: None,
            },
        );

        let error = read_topic(
            &ctx,
            &config(&broker, &topic, json!({})),
            &Credential::new(),
            &topic,
            Some(stored.to_checkpoint()),
        )
        .await
        .err()
        .unwrap();

        assert_eq!(other(error), "kafka_offset_out_of_range");
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn a_broker_advertising_a_blocked_address_is_refused_and_never_dialled() {
        let broker = env("ZIRV_KAFKA_BROKER");
        let port: u16 = broker.rsplit(':').next().unwrap().parse().unwrap();
        // The bootstrap name is allowed; the name the broker advertises resolves to a loopback
        // address the policy does not allow, where a listener counts connections.
        let ctx = context(
            &["127.0.0.1/32"],
            &[
                ("bootstrap.test", "127.0.0.1"),
                ("broker.test", "127.0.0.2"),
            ],
        );
        let setup = local();
        let session = admin(&setup, &broker, None).await;
        let topic = unique("egress");
        create_topic(&session, &topic, 1).await;
        let counter = TcpListener::bind(("127.0.0.2", port)).await;
        let connections = Arc::new(AtomicUsize::new(0));
        if let Ok(listener) = counter {
            let seen = Arc::clone(&connections);
            tokio::spawn(async move {
                while listener.accept().await.is_ok() {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
        let cfg = config(&format!("bootstrap.test:{port}"), &topic, json!({}));

        let error = Kafka
            .check(&ctx, &cfg, &Credential::new())
            .await
            .unwrap_err();
        let read = read_topic(&ctx, &cfg, &Credential::new(), &topic, None)
            .await
            .unwrap_err();

        for error in [error, read] {
            assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
            let text = error.to_string();
            assert!(text.contains("not allowed by the egress policy"), "{text}");
            assert!(
                !text.contains("127.0.0.2") && !text.contains("broker.test"),
                "{text}"
            );
        }
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "no connection reached the blocked address"
        );
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn a_bootstrap_address_the_policy_denies_is_refused_before_any_connection() {
        let ctx = context(&[], &[("broker.test", "10.0.0.1")]);
        let cfg = config("broker.test:9092", "t", json!({}));

        let error = Kafka
            .check(&ctx, &cfg, &Credential::new())
            .await
            .unwrap_err();

        assert!(matches!(error, SourceError::Egress(_)), "{error:?}");
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn sasl_plain_and_both_scram_mechanisms_authenticate_and_a_wrong_password_does_not() {
        let broker = env("ZIRV_KAFKA_SASL_BROKER");
        let ctx = local();
        let session = admin(
            &ctx,
            &broker,
            Some((Mechanism::Plain, "alice", "alice-secret")),
        )
        .await;
        let topic = unique("sasl");
        create_topic(&session, &topic, 1).await;
        produce(
            &session,
            &topic,
            0,
            vec![record(Some("k"), Some(r#"{"v":1}"#))],
            Compression::NoCompression,
        )
        .await;

        for (mechanism, user, pass) in [
            ("plain", "alice", "alice-secret"),
            ("scram_sha_256", "bob", "bob-secret"),
            ("scram_sha_512", "carol", "carol-secret"),
        ] {
            let cfg = config(&broker, &topic, json!({"sasl": mechanism}));
            let creds = credential(&[("username", user), ("password", pass)]);
            assert_eq!(Kafka.check(&ctx, &cfg, &creds).await, Ok(()), "{mechanism}");
            let read = read_topic(&ctx, &cfg, &creds, &topic, None).await.unwrap();
            assert_eq!(read.records.len(), 1, "{mechanism}");

            if mechanism == "plain" {
                // A refused login is retried until the client gives up, so one is enough.
                let wrong = credential(&[("username", user), ("password", "wrong")]);
                let error = Kafka.check(&ctx, &cfg, &wrong).await.unwrap_err();
                assert_eq!(other(error), "kafka_auth_failed");
            }
        }
    }

    // --- TLS ---------------------------------------------------------------------------------

    struct Pki {
        authority_pem: String,
        authority: CertifiedIssuer<'static, KeyPair>,
    }

    fn authority() -> Pki {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "zirv test authority");
        let authority = CertifiedIssuer::self_signed(params, KeyPair::generate().unwrap()).unwrap();
        Pki {
            authority_pem: authority.pem(),
            authority,
        }
    }

    /// A certificate for `name` signed by `pki`: (certificate PEM, key PEM, DER pair).
    fn issue(
        pki: &Pki,
        name: &str,
        usage: ExtendedKeyUsagePurpose,
    ) -> (
        String,
        String,
        rustls::pki_types::CertificateDer<'static>,
        rustls::pki_types::PrivateKeyDer<'static>,
    ) {
        let mut params = CertificateParams::new(vec![name.to_owned()]).unwrap();
        params.extended_key_usages = vec![usage];
        let key = KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, &pki.authority).unwrap();
        let der_key = rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap();
        (
            certificate.pem(),
            key.serialize_pem(),
            certificate.der().clone(),
            der_key,
        )
    }

    /// A TLS terminator on `front` that forwards to the plaintext listener on `backend`.
    async fn terminator(
        front: u16,
        backend: u16,
        server: (
            rustls::pki_types::CertificateDer<'static>,
            rustls::pki_types::PrivateKeyDer<'static>,
        ),
        client_roots: Option<rustls::RootCertStore>,
    ) -> JoinHandle<()> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .unwrap();
        let builder = match client_roots {
            Some(roots) => builder.with_client_cert_verifier(
                rustls::server::WebPkiClientVerifier::builder_with_provider(
                    Arc::new(roots),
                    provider,
                )
                .build()
                .unwrap(),
            ),
            None => builder.with_no_client_auth(),
        };
        let config = builder.with_single_cert(vec![server.0], server.1).unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind(("127.0.0.1", front)).await.unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(socket).await else {
                        return;
                    };
                    let Ok(mut plain) = TcpStream::connect(("127.0.0.1", backend)).await else {
                        return;
                    };
                    let _ = tokio::io::copy_bidirectional(&mut tls, &mut plain).await;
                });
            }
        })
    }

    #[tokio::test]
    #[ignore = "needs a Kafka broker: scripts/kafka-fixture.sh"]
    async fn tls_verifies_the_hostname_over_the_relay_with_a_private_ca_and_a_client_certificate() {
        let front: u16 = env("ZIRV_KAFKA_TLS_FRONT_PORT").parse().unwrap();
        let backend: u16 = env("ZIRV_KAFKA_TLS_BACKEND_PORT").parse().unwrap();
        let plain_broker = env("ZIRV_KAFKA_BROKER");
        let ctx = local();
        let session = admin(&ctx, &plain_broker, None).await;
        let topic = unique("tls");
        create_topic(&session, &topic, 1).await;
        produce(
            &session,
            &topic,
            0,
            vec![record(Some("k"), Some(r#"{"v":1}"#))],
            Compression::NoCompression,
        )
        .await;
        let broker = format!("broker.test:{front}");
        let cfg = |ca: &str| config(&broker, &topic, json!({"security": "tls", "tls_ca": ca}));
        let pki = authority();
        let (_, _, server_cert, server_key) =
            issue(&pki, "broker.test", ExtendedKeyUsagePurpose::ServerAuth);

        // 1. A certificate for the right name, signed by the private authority, is accepted.
        let task = terminator(front, backend, (server_cert, server_key), None).await;
        let read = read_topic(
            &ctx,
            &cfg(&pki.authority_pem),
            &Credential::new(),
            &topic,
            None,
        )
        .await
        .unwrap();
        assert_eq!(read.records.len(), 1);
        // 2. Without that authority the system roots do not trust it.
        let no_ca = config(&broker, &topic, json!({"security": "tls"}));
        let error = Kafka
            .check(&ctx, &no_ca, &Credential::new())
            .await
            .unwrap_err();
        assert_eq!(other(error), "kafka_tls_failed");
        task.abort();
        let _ = task.await;

        // 3. A certificate for another name is refused although the authority is trusted.
        let (_, _, wrong_cert, wrong_key) =
            issue(&pki, "other.test", ExtendedKeyUsagePurpose::ServerAuth);
        let task = terminator(front, backend, (wrong_cert, wrong_key), None).await;
        let error = Kafka
            .check(&ctx, &cfg(&pki.authority_pem), &Credential::new())
            .await
            .unwrap_err();
        assert_eq!(other(error), "kafka_tls_failed");
        task.abort();
        let _ = task.await;

        // 4. A terminator that requires a client certificate accepts one from the authority
        //    and turns away a client without.
        let (_, _, server_cert, server_key) =
            issue(&pki, "broker.test", ExtendedKeyUsagePurpose::ServerAuth);
        let mut roots = rustls::RootCertStore::empty();
        roots.add(pki.authority.der().clone()).unwrap();
        let task = terminator(front, backend, (server_cert, server_key), Some(roots)).await;
        let (client_pem, client_key, _, _) =
            issue(&pki, "zirv-flow", ExtendedKeyUsagePurpose::ClientAuth);
        let with_cert = credential(&[
            ("tls_client_cert", &client_pem),
            ("tls_client_key", &client_key),
        ]);
        assert_eq!(
            Kafka
                .check(&ctx, &cfg(&pki.authority_pem), &with_cert)
                .await,
            Ok(())
        );
        let without = Kafka
            .check(&ctx, &cfg(&pki.authority_pem), &Credential::new())
            .await
            .unwrap_err();
        assert!(matches!(
            other(without).as_str(),
            "kafka_tls_failed" | "kafka_connect_failed"
        ));
        task.abort();
    }
}
