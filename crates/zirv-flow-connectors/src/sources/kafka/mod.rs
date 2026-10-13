//! The `kafka` source: JSON topics read with per-partition offsets (architecture 7.1, 7.6).
//!
//! A stream is a topic; a message is a record keyed by the message key; a message with a null
//! value is a tombstone. The module yields batches whose checkpoint is the typed [`Positions`];
//! the consumer job stores them in the apply-batch transaction. The only network path is the
//! egress relay in [`relay`], and nothing here names a record value, a key, a broker or a
//! credential in an error, a log or a metric label.

#[cfg(test)]
mod broker;
mod client;
mod error;
mod fetch;
mod message;
mod positions;
mod relay;

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use serde_json::Value;

use self::client::{Connect, Mechanism, Session, TlsMaterial};
use self::fetch::{IDLE_SLEEP, read_partitions};
use self::message::{KEY_FIELD, TIMESTAMP_FIELD};
pub use self::positions::{PartitionPosition, Positions, ReadPlan};
use super::context::SourceContext;
use super::credential::Credential;
use super::error::SourceError;
use super::source::Source;
use super::spec::{Capabilities, FieldKind, FieldSpec, SourceSpec};
use super::types::{
    Batch, Checkpoint, FieldDescriptor, FieldType, StreamConfig, StreamDescriptor, SyncMode,
};
use super::validate::{FieldError, validate_config};

/// The messages discovery samples per topic.
const SAMPLE_MESSAGES: usize = 50;

/// How long discovery reads one topic.
const SAMPLE_TOPIC_TIME: Duration = Duration::from_secs(10);

/// How long discovery reads in all, under the API's call timeout.
const SAMPLE_TOTAL_TIME: Duration = Duration::from_secs(25);

/// The most topics one source reads.
const MAX_TOPICS: usize = 20;

pub(crate) struct Kafka;

impl Source for Kafka {
    const SPEC: SourceSpec = SourceSpec {
        kind: "kafka",
        label: "Kafka",
        description: "JSON messages from Apache Kafka topics, one stream per topic. A message with an empty value deletes its record.",
        config_fields: &[
            FieldSpec {
                key: "brokers",
                label: "Brokers",
                help: "Comma-separated host:port pairs, such as kafka-1.example.com:9093. One to ten, without a scheme.",
                kind: FieldKind::HostList,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "topics",
                label: "Topics",
                help: "Comma-separated topic names, one to twenty. Each topic is a stream; the message key is the record key.",
                kind: FieldKind::Text,
                required: true,
                secret: false,
            },
            FieldSpec {
                key: "security",
                label: "Connection security",
                help: "tls encrypts the connection and checks the broker's certificate (default). plaintext sends everything unencrypted.",
                kind: FieldKind::Select(&["plaintext", "tls"]),
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "sasl",
                label: "Authentication",
                help: "none, or the SASL mechanism the brokers require. The user name and password go in the credential.",
                kind: FieldKind::Select(&["none", "plain", "scram_sha_256", "scram_sha_512"]),
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "tls_ca",
                label: "Certificate authority",
                help: "PEM of the authority that signed the brokers' certificates, for a private authority. Empty trusts the system roots.",
                kind: FieldKind::LongText,
                required: false,
                secret: false,
            },
            FieldSpec {
                key: "max_lag",
                label: "Highest healthy lag",
                help: "How many messages, over all partitions, the stream may be behind and still count as fresh. Default 10000.",
                kind: FieldKind::Number,
                required: false,
                secret: false,
            },
        ],
        credential_fields: &[
            FieldSpec {
                key: "username",
                label: "User name",
                help: "For SASL; needs the password too.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "password",
                label: "Password",
                help: "For SASL; needs the user name too.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "tls_client_cert",
                label: "Client certificate",
                help: "PEM, for brokers that require a client certificate; needs the key too.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
            FieldSpec {
                key: "tls_client_key",
                label: "Client key",
                help: "PEM private key of the client certificate.",
                kind: FieldKind::Text,
                required: false,
                secret: true,
            },
        ],
        rate_limit: None,
        config_check: Some(config_check),
        capabilities: Capabilities {
            incremental: false,
            full_refresh: false,
            emits_deletes: true,
        },
    };

    async fn check(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<(), SourceError> {
        let settings = Settings::new(config, credential)?;
        let session = Session::connect(ctx, &settings.connect(credential)).await?;
        // Opening the partitions connects to each leader, so a broker the metadata names but
        // the policy refuses, or one that cannot be reached, fails here and not on the first run.
        for topic in &settings.topics {
            session.open_topic(topic, &Positions::default()).await?;
        }
        Ok(())
    }

    async fn discover(
        &self,
        ctx: &SourceContext,
        config: &Value,
        credential: &Credential,
    ) -> Result<Vec<StreamDescriptor>, SourceError> {
        let settings = Settings::new(config, credential)?;
        let session = Session::connect(ctx, &settings.connect(credential)).await?;
        let started = Instant::now();
        let mut streams = Vec::with_capacity(settings.topics.len());
        for topic in &settings.topics {
            let partitions = session.open_topic(topic, &Positions::default()).await?;
            let remaining = SAMPLE_TOTAL_TIME.saturating_sub(started.elapsed());
            let plan = ReadPlan {
                budget: Some(SAMPLE_TOPIC_TIME.min(remaining)),
                stop_when_caught_up: true,
            };
            let mut batches = read_partitions(partitions, plan, IDLE_SLEEP, Box::new(()));
            let mut sampled = Vec::new();
            while sampled.len() < SAMPLE_MESSAGES
                && let Some(batch) = batches.next().await
            {
                for record in batch?.records {
                    if let Value::Object(object) = record.data
                        && sampled.len() < SAMPLE_MESSAGES
                    {
                        sampled.push(object);
                    }
                }
            }
            streams.push(descriptor(topic, &sampled));
        }
        Ok(streams)
    }

    fn read<'a>(
        &'a self,
        ctx: &'a SourceContext,
        config: &'a Value,
        credential: &'a Credential,
        stream: &'a StreamConfig,
        checkpoint: Option<Checkpoint>,
    ) -> BoxStream<'a, Result<Batch, SourceError>> {
        let (plan, positions) = ReadPlan::from_checkpoint(checkpoint.as_ref());
        stream::once(async move {
            let settings = Settings::new(config, credential)?;
            if !settings.topics.contains(&stream.name) {
                return Err(SourceError::Config {
                    field: "stream".to_owned(),
                    reason: "is not a configured topic".to_owned(),
                });
            }
            let session = Session::connect(ctx, &settings.connect(credential)).await?;
            let partitions = session.open_topic(&stream.name, &positions).await?;
            Ok(read_partitions(
                partitions,
                plan,
                IDLE_SLEEP,
                Box::new(session),
            ))
        })
        .flat_map(|opened| match opened {
            Ok(batches) => batches,
            Err(error) => stream::once(async move { Err(error) }).boxed(),
        })
        .boxed()
    }
}

/// The settings of a source, checked.
struct Settings {
    brokers: Vec<String>,
    topics: Vec<String>,
    tls: bool,
    sasl: Option<Mechanism>,
    tls_ca: Option<String>,
}

fn config_error(field: &str, reason: &str) -> SourceError {
    SourceError::Config {
        field: field.to_owned(),
        reason: reason.to_owned(),
    }
}

/// The rules that refine a field: the topics are one to twenty legal topic names.
fn config_check(config: &Value) -> Vec<FieldError> {
    let Some(text) = config.get("topics").and_then(Value::as_str) else {
        return Vec::new();
    };
    let topics: Vec<&str> = text.split(',').map(str::trim).collect();
    if topics.len() > MAX_TOPICS || !topics.iter().all(|topic| topic_is_valid(topic)) {
        return vec![FieldError {
            field: "topics".to_owned(),
            reason:
                "must be one to twenty topic names of letters, digits, dots, dashes and underscores"
                    .to_owned(),
        }];
    }
    Vec::new()
}

/// Whether `name` is a legal Kafka topic name.
fn topic_is_valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 249
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

impl Settings {
    fn new(config: &Value, credential: &Credential) -> Result<Self, SourceError> {
        if let Err(errors) = validate_config(&Kafka::SPEC, config)
            && let Some(first) = errors.into_iter().next()
        {
            return Err(SourceError::Config {
                field: first.field,
                reason: first.reason,
            });
        }
        let text = |key: &str| config.get(key).and_then(Value::as_str);
        let list = |key: &str| -> Vec<String> {
            text(key)
                .unwrap_or_default()
                .split(',')
                .map(|item| item.trim().to_owned())
                .collect()
        };
        let topics = list("topics");
        let sasl = match text("sasl").unwrap_or("none") {
            "plain" => Some(Mechanism::Plain),
            "scram_sha_256" => Some(Mechanism::ScramSha256),
            "scram_sha_512" => Some(Mechanism::ScramSha512),
            _ => None,
        };
        if sasl.is_some() {
            for field in ["username", "password"] {
                if credential.get(field).is_none() {
                    return Err(config_error(field, "is required with SASL"));
                }
            }
        }
        if credential.get("tls_client_cert").is_some() != credential.get("tls_client_key").is_some()
        {
            return Err(config_error(
                "tls_client_key",
                "and the client certificate go together",
            ));
        }
        Ok(Self {
            brokers: list("brokers"),
            topics,
            tls: text("security").unwrap_or("tls") == "tls",
            sasl,
            tls_ca: text("tls_ca")
                .filter(|pem| !pem.trim().is_empty())
                .map(str::to_owned),
        })
    }

    fn connect<'a>(&'a self, credential: &'a Credential) -> Connect<'a> {
        Connect {
            brokers: self.brokers.clone(),
            tls: self.tls.then(|| TlsMaterial {
                ca: self.tls_ca.as_deref(),
                client_cert: credential.get("tls_client_cert"),
                client_key: credential.get("tls_client_key"),
            }),
            sasl: self.sasl.and_then(|mechanism| {
                Some((
                    mechanism,
                    credential.get("username")?,
                    credential.get("password")?,
                ))
            }),
        }
    }
}

/// The stream a topic is: the virtual fields and the union of the sampled top-level keys.
fn descriptor(topic: &str, sampled: &[serde_json::Map<String, Value>]) -> StreamDescriptor {
    let mut fields = vec![
        FieldDescriptor {
            name: KEY_FIELD.to_owned(),
            field_type: FieldType::String,
        },
        FieldDescriptor {
            name: TIMESTAMP_FIELD.to_owned(),
            field_type: FieldType::Timestamp,
        },
    ];
    for object in sampled {
        for (name, value) in object {
            if name.trim().is_empty() || name == KEY_FIELD || name == TIMESTAMP_FIELD {
                continue;
            }
            let seen = match value {
                Value::Null => FieldType::Unknown,
                Value::String(_) => FieldType::String,
                Value::Number(_) => FieldType::Number,
                Value::Bool(_) => FieldType::Boolean,
                Value::Array(_) | Value::Object(_) => FieldType::Json,
            };
            match fields.iter_mut().find(|field| &field.name == name) {
                Some(field) if field.field_type == FieldType::Unknown => field.field_type = seen,
                Some(_) => {}
                None => fields.push(FieldDescriptor {
                    name: name.clone(),
                    field_type: seen,
                }),
            }
        }
    }
    StreamDescriptor {
        name: topic.to_owned(),
        fields,
        sync_modes: vec![SyncMode::Stream],
        default_cursor_field: None,
        default_primary_key: vec![KEY_FIELD.to_owned()],
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use zirv_flow_core::SecretString;

    use super::*;

    fn credential(pairs: &[(&str, &str)]) -> Credential {
        let mut credential = Credential::new();
        for (key, value) in pairs {
            credential.insert(*key, SecretString::new((*value).to_owned()));
        }
        credential
    }

    fn config_of(extra: Value) -> Value {
        let mut config = json!({"brokers": "a.test:9092", "topics": "customers"});
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        config
    }

    fn error_of(config: &Value, credential: &Credential) -> (String, String) {
        match Settings::new(config, credential).err().unwrap() {
            SourceError::Config { field, reason } => (field, reason),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_spec_lists_the_settings_and_every_credential_field_is_secret() {
        let config: Vec<_> = Kafka::SPEC.config_fields.iter().map(|f| f.key).collect();
        let credentials: Vec<_> = Kafka::SPEC
            .credential_fields
            .iter()
            .map(|f| f.key)
            .collect();

        assert_eq!(
            config,
            ["brokers", "topics", "security", "sasl", "tls_ca", "max_lag"]
        );
        assert_eq!(
            credentials,
            ["username", "password", "tls_client_cert", "tls_client_key"]
        );
        assert!(Kafka::SPEC.credential_fields.iter().all(|f| f.secret));
        assert!(Kafka::SPEC.config_fields.iter().all(|f| !f.secret));
        const { assert!(Kafka::SPEC.capabilities.emits_deletes) };
        const { assert!(!Kafka::SPEC.capabilities.incremental) };
        const { assert!(!Kafka::SPEC.capabilities.full_refresh) };
    }

    #[test]
    fn settings_default_to_tls_without_sasl() {
        let settings = Settings::new(&config_of(json!({})), &Credential::new())
            .ok()
            .unwrap();

        assert!(settings.tls);
        assert_eq!(settings.sasl, None);
        assert_eq!(settings.topics, ["customers"]);
        assert_eq!(settings.brokers, ["a.test:9092"]);
    }

    #[test]
    fn sasl_needs_a_user_name_and_a_password() {
        let config = config_of(json!({"sasl": "scram_sha_256"}));

        assert_eq!(error_of(&config, &Credential::new()).0, "username");
        assert_eq!(
            error_of(&config, &credential(&[("username", "u")])).0,
            "password"
        );
        assert!(
            Settings::new(
                &config,
                &credential(&[("username", "u"), ("password", "p")])
            )
            .is_ok()
        );
    }

    #[test]
    fn a_client_certificate_and_its_key_go_together() {
        let config = config_of(json!({}));

        let (field, _) = error_of(&config, &credential(&[("tls_client_cert", "pem")]));

        assert_eq!(field, "tls_client_key");
    }

    #[test]
    fn topics_are_checked_for_count_and_name() {
        for bad in ["", "a b", "a/b", "..", "a,,b", "é"] {
            let (field, _) = error_of(&config_of(json!({"topics": bad})), &Credential::new());
            assert_eq!(field, "topics", "{bad}");
        }
        let many = (0..21)
            .map(|n| format!("t{n}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(
            error_of(&config_of(json!({"topics": many})), &Credential::new()).0,
            "topics"
        );
        let ok = Settings::new(
            &config_of(json!({"topics": "a.b, c_d,e-f"})),
            &Credential::new(),
        );
        assert_eq!(ok.ok().unwrap().topics, ["a.b", "c_d", "e-f"]);
    }

    #[test]
    fn an_unusable_broker_list_is_a_config_error_naming_the_field() {
        let config = json!({"brokers": "https://a.test:9092", "topics": "t"});

        assert_eq!(error_of(&config, &Credential::new()).0, "brokers");
    }

    #[test]
    fn a_descriptor_has_the_virtual_fields_the_sampled_keys_and_stream_mode_only() {
        let sampled: Vec<_> = [
            json!({"email": "a@b.c", "age": null, "_key": "x"}),
            json!({"age": 3, "tags": ["a"], "ok": true}),
        ]
        .into_iter()
        .map(|value| value.as_object().unwrap().clone())
        .collect();

        let descriptor = descriptor("customers", &sampled);

        let fields: Vec<_> = descriptor
            .fields
            .iter()
            .map(|f| (f.name.as_str(), f.field_type))
            .collect();
        assert_eq!(
            fields,
            [
                ("_key", FieldType::String),
                ("_timestamp", FieldType::Timestamp),
                ("age", FieldType::Number),
                ("email", FieldType::String),
                ("ok", FieldType::Boolean),
                ("tags", FieldType::Json),
            ]
        );
        assert_eq!(descriptor.sync_modes, [SyncMode::Stream]);
        assert_eq!(descriptor.default_primary_key, ["_key"]);
        assert_eq!(descriptor.default_cursor_field, None);
    }

    #[test]
    fn an_empty_topic_reports_only_the_two_virtual_fields() {
        let descriptor = descriptor("empty", &[]);

        assert_eq!(descriptor.fields.len(), 2);
    }

    #[test]
    fn the_only_network_path_is_the_relay() {
        let direct = concat!("TcpStream", "::connect");
        for (name, source) in [
            ("client.rs", include_str!("client.rs")),
            ("fetch.rs", include_str!("fetch.rs")),
            ("message.rs", include_str!("message.rs")),
            ("error.rs", include_str!("error.rs")),
            ("positions.rs", include_str!("positions.rs")),
        ] {
            assert!(!source.contains(direct), "{name} dials directly");
            assert!(!source.contains("reqwest"), "{name} names reqwest");
        }
        let production = include_str!("mod.rs").split("#[cfg(test)]").next().unwrap();
        assert!(!production.contains(direct));
        assert!(!production.contains("reqwest"));
    }
}
