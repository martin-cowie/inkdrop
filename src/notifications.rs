//! IPP event notifications (RFC 3995) with the `ippget` pull method
//! (RFC 3996): subscribe to a printer's events, then long-poll for them.
//!
//! The `ipp` crate can neither encode nor parse the subscription (0x06) and
//! event-notification (0x07) attribute groups these operations use, so this
//! module has its own minimal IPP message codec.

use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use http::Uri;

/// Create-Printer-Subscriptions.
pub const CREATE_PRINTER_SUBSCRIPTIONS: u16 = 0x0016;
/// Cancel-Subscription.
pub const CANCEL_SUBSCRIPTION: u16 = 0x001B;
/// Get-Notifications.
pub const GET_NOTIFICATIONS: u16 = 0x001C;

/// The operation-attributes group.
pub const OPERATION_ATTRIBUTES: u8 = 0x01;
/// The subscription-attributes group.
pub const SUBSCRIPTION_ATTRIBUTES: u8 = 0x06;
/// The event-notification-attributes group.
pub const EVENT_NOTIFICATION_ATTRIBUTES: u8 = 0x07;
const END_OF_ATTRIBUTES: u8 = 0x03;

/// The status code `client-error-not-found`.
pub const CLIENT_ERROR_NOT_FOUND: u16 = 0x0406;
/// The status code `server-error-operation-not-supported`.
pub const SERVER_ERROR_OPERATION_NOT_SUPPORTED: u16 = 0x0501;

const INTEGER: u8 = 0x21;
const BOOLEAN: u8 = 0x22;
const ENUM: u8 = 0x23;
const TEXT: u8 = 0x41;
const NAME: u8 = 0x42;
const KEYWORD: u8 = 0x44;
const URI: u8 = 0x45;
const CHARSET: u8 = 0x47;
const NATURAL_LANGUAGE: u8 = 0x48;

/// One attribute value: its IPP value tag and encoded bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    /// The IPP value tag, e.g. 0x21 for `integer`.
    pub tag: u8,
    /// The value as encoded on the wire.
    pub bytes: Vec<u8>,
}

impl Value {
    /// An `integer` value.
    pub fn integer(value: i32) -> Self {
        Value { tag: INTEGER, bytes: value.to_be_bytes().to_vec() }
    }

    /// An `enum` value.
    pub fn enumeration(value: i32) -> Self {
        Value { tag: ENUM, bytes: value.to_be_bytes().to_vec() }
    }

    /// A `boolean` value.
    pub fn boolean(value: bool) -> Self {
        Value { tag: BOOLEAN, bytes: vec![u8::from(value)] }
    }

    /// A `keyword` value.
    pub fn keyword(value: &str) -> Self {
        Self::string(KEYWORD, value)
    }

    /// A `textWithoutLanguage` value.
    pub fn text(value: &str) -> Self {
        Self::string(TEXT, value)
    }

    fn string(tag: u8, value: &str) -> Self {
        Value { tag, bytes: value.as_bytes().to_vec() }
    }

    /// The value of an `integer` or `enum`, or `None` for any other type.
    pub fn as_integer(&self) -> Option<i32> {
        match (self.tag, <[u8; 4]>::try_from(self.bytes.as_slice())) {
            (INTEGER | ENUM, Ok(bytes)) => Some(i32::from_be_bytes(bytes)),
            _ => None,
        }
    }

    /// The value of a `boolean`, or `None` for any other type.
    pub fn as_bool(&self) -> Option<bool> {
        match (self.tag, self.bytes.as_slice()) {
            (BOOLEAN, [byte]) => Some(*byte != 0),
            _ => None,
        }
    }

    /// The value of a string type such as `keyword` or `textWithoutLanguage`,
    /// or `None` for any other type or for invalid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        matches!(self.tag, 0x41..=0x49).then(|| std::str::from_utf8(&self.bytes).ok()).flatten()
    }
}

/// A named attribute with one or more values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attribute {
    /// The attribute name, e.g. `notify-subscription-id`.
    pub name: String,
    /// The values, in order; more than one for a `1setOf` attribute.
    pub values: Vec<Value>,
}

/// An attribute group, such as the operation attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    /// The group's delimiter tag, e.g. [`OPERATION_ATTRIBUTES`].
    pub tag: u8,
    /// The group's attributes, in order.
    pub attributes: Vec<Attribute>,
}

impl Group {
    /// The first value of attribute `name`, if present.
    pub fn first(&self, name: &str) -> Option<&Value> {
        self.attributes.iter().find(|attr| attr.name == name).and_then(|attr| attr.values.first())
    }
}

/// An IPP request or response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    /// The operation id of a request, or the status code of a response.
    pub code: u16,
    /// The request id, echoed by the response.
    pub request_id: i32,
    /// The attribute groups, in order.
    pub groups: Vec<Group>,
}

/// Why bytes couldn't be decoded as an IPP message.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    /// The message ended part-way through.
    #[error("IPP message is truncated")]
    Truncated,
    /// An attribute appeared before any group delimiter.
    #[error("IPP attribute outside any group")]
    NoGroup,
}

impl Message {
    /// A request for operation `code` on the printer at `printer_uri`, with
    /// the operation attributes every request carries.
    pub fn request(code: u16, request_id: i32, printer_uri: &str) -> Self {
        let mut result = Message { code, request_id, groups: Vec::new() };
        result.add(OPERATION_ATTRIBUTES, "attributes-charset", vec![Value::string(CHARSET, "utf-8")]);
        result.add(OPERATION_ATTRIBUTES, "attributes-natural-language", vec![Value::string(NATURAL_LANGUAGE, "en")]);
        result.add(OPERATION_ATTRIBUTES, "printer-uri", vec![Value::string(URI, printer_uri)]);
        result.add(OPERATION_ATTRIBUTES, "requesting-user-name", vec![Value::string(NAME, "inkdrop")]);
        result
    }

    /// A response with status `code` to the request with `request_id`.
    pub fn response(code: u16, request_id: i32) -> Self {
        let mut result = Message { code, request_id, groups: Vec::new() };
        result.add(OPERATION_ATTRIBUTES, "attributes-charset", vec![Value::string(CHARSET, "utf-8")]);
        result.add(OPERATION_ATTRIBUTES, "attributes-natural-language", vec![Value::string(NATURAL_LANGUAGE, "en")]);
        result
    }

    /// Append attribute `name` with `values` to the last group tagged `tag`,
    /// starting a new group if the last group has a different tag.
    pub fn add(&mut self, tag: u8, name: &str, values: Vec<Value>) {
        if self.groups.last().is_none_or(|group| group.tag != tag) {
            self.groups.push(Group { tag, attributes: Vec::new() });
        }
        let group = self.groups.last_mut().expect("a group was just ensured");
        group.attributes.push(Attribute { name: name.to_owned(), values });
    }

    /// Start a new group tagged `tag`, even if the last group has that tag,
    /// as each event notification needs its own group.
    pub fn start_group(&mut self, tag: u8) {
        self.groups.push(Group { tag, attributes: Vec::new() });
    }

    /// The groups tagged `tag`, in order.
    pub fn groups_of(&self, tag: u8) -> impl Iterator<Item = &Group> {
        self.groups.iter().filter(move |group| group.tag == tag)
    }

    /// Whether this response's status code is a success (0x0000-0x00FF).
    pub fn is_success(&self) -> bool {
        self.code <= 0x00FF
    }

    /// Encode as IPP 2.0, with no document data.
    pub fn encode(&self) -> Vec<u8> {
        let mut result = vec![2, 0];
        result.extend_from_slice(&self.code.to_be_bytes());
        result.extend_from_slice(&self.request_id.to_be_bytes());
        for group in &self.groups {
            result.push(group.tag);
            for attribute in &group.attributes {
                for (index, value) in attribute.values.iter().enumerate() {
                    let name = if index == 0 { attribute.name.as_bytes() } else { &[] };
                    result.push(value.tag);
                    result.extend_from_slice(&(name.len() as u16).to_be_bytes());
                    result.extend_from_slice(name);
                    result.extend_from_slice(&(value.bytes.len() as u16).to_be_bytes());
                    result.extend_from_slice(&value.bytes);
                }
            }
        }
        result.push(END_OF_ATTRIBUTES);
        result
    }

    /// Decode a message, ignoring any document data after the attributes.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Truncated`] if `bytes` ends early, and
    /// [`DecodeError::NoGroup`] if an attribute precedes every group tag.
    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut reader = Reader(bytes);
        reader.take(2)?;
        let code = u16::from_be_bytes(reader.array()?);
        let request_id = i32::from_be_bytes(reader.array()?);
        let mut groups: Vec<Group> = Vec::new();
        loop {
            let tag = reader.take(1)?[0];
            if tag == END_OF_ATTRIBUTES {
                break;
            }
            if tag <= 0x0F {
                groups.push(Group { tag, attributes: Vec::new() });
                continue;
            }
            let name_len = u16::from_be_bytes(reader.array()?) as usize;
            let name = String::from_utf8_lossy(reader.take(name_len)?).into_owned();
            let value_len = u16::from_be_bytes(reader.array()?) as usize;
            let value = Value { tag, bytes: reader.take(value_len)?.to_vec() };
            let attributes = &mut groups.last_mut().ok_or(DecodeError::NoGroup)?.attributes;
            match attributes.last_mut() {
                Some(previous) if name.is_empty() => previous.values.push(value),
                _ => attributes.push(Attribute { name, values: vec![value] }),
            }
        }
        Ok(Message { code, request_id, groups })
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.0.len() < n {
            return Err(DecodeError::Truncated);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("take returns exactly N bytes"))
    }
}

/// Why a subscription request failed.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// The printer couldn't be reached, or answered with an HTTP error.
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The printer's response wasn't a valid IPP message.
    #[error("invalid IPP response: {0}")]
    Decode(#[from] DecodeError),
    /// The printer answered with an unsuccessful IPP status code.
    #[error("printer answered with IPP status {0:#06x}")]
    Status(u16),
    /// The printer accepted Create-Printer-Subscriptions but created no
    /// subscription.
    #[error("printer created no subscription")]
    NoSubscription,
}

impl EventError {
    /// Whether the printer answered but refused, so retrying won't help.
    pub fn is_refusal(&self) -> bool {
        matches!(self, EventError::Status(_) | EventError::NoSubscription | EventError::Decode(_))
    }
}

/// One event from Get-Notifications.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// `notify-sequence-number`, which increases with each event.
    pub sequence: i32,
    /// `notify-subscribed-event`, e.g. `printer-state-changed`.
    pub name: String,
}

/// Talks to one printer's subscription operations over HTTP.
#[derive(Clone)]
pub struct EventClient {
    http: reqwest::Client,
    url: String,
    printer_uri: String,
    next_request_id: std::sync::Arc<AtomicI32>,
}

impl EventClient {
    /// A client for the printer at `uri` (`ipp://` or `ipps://`). Each request
    /// gives up after `timeout`, which must exceed how long the printer may
    /// hold a waiting Get-Notifications request.
    pub fn new(uri: &Uri, timeout: Duration) -> Self {
        let printer_uri = uri.to_string();
        let url = match uri.scheme_str() {
            Some("ipps") => printer_uri.replacen("ipps://", "https://", 1),
            _ => printer_uri.replacen("ipp://", "http://", 1),
        };
        let http = reqwest::Client::builder().timeout(timeout).build().expect("a client with only a timeout builds");
        EventClient { http, url, printer_uri, next_request_id: std::sync::Arc::new(AtomicI32::new(1)) }
    }

    /// Subscribe to the printer's `events` for `lease`, returning the
    /// subscription id.
    ///
    /// # Errors
    ///
    /// [`EventError::Status`] if the printer refuses, e.g. with
    /// [`SERVER_ERROR_OPERATION_NOT_SUPPORTED`];
    /// [`EventError::NoSubscription`] if it answers without a subscription id;
    /// and [`EventError::Http`] or [`EventError::Decode`] if it can't be
    /// reached or understood.
    pub async fn subscribe(&self, events: &[&str], lease: Duration) -> Result<i32, EventError> {
        let mut request = self.request(CREATE_PRINTER_SUBSCRIPTIONS);
        request.add(SUBSCRIPTION_ATTRIBUTES, "notify-pull-method", vec![Value::keyword("ippget")]);
        request.add(SUBSCRIPTION_ATTRIBUTES, "notify-events", events.iter().map(|event| Value::keyword(event)).collect());
        let lease_secs = i32::try_from(lease.as_secs()).unwrap_or(i32::MAX);
        request.add(SUBSCRIPTION_ATTRIBUTES, "notify-lease-duration", vec![Value::integer(lease_secs)]);

        let response = self.send(&request).await?;
        response
            .groups_of(SUBSCRIPTION_ATTRIBUTES)
            .find_map(|group| group.first("notify-subscription-id"))
            .and_then(Value::as_integer)
            .ok_or(EventError::NoSubscription)
    }

    /// Fetch the events of `subscription` numbered `sequence` or later, oldest
    /// first. With `wait`, the printer may hold the request until an event
    /// arrives; the result is empty if none did.
    ///
    /// # Errors
    ///
    /// [`EventError::Status`] with [`CLIENT_ERROR_NOT_FOUND`] if the
    /// subscription has gone, e.g. because the printer restarted or the lease
    /// expired; other errors as for [`EventClient::subscribe`].
    pub async fn notifications(&self, subscription: i32, sequence: i32, wait: bool) -> Result<Vec<Event>, EventError> {
        let mut request = self.request(GET_NOTIFICATIONS);
        request.add(OPERATION_ATTRIBUTES, "notify-subscription-ids", vec![Value::integer(subscription)]);
        request.add(OPERATION_ATTRIBUTES, "notify-sequence-numbers", vec![Value::integer(sequence)]);
        request.add(OPERATION_ATTRIBUTES, "notify-wait", vec![Value::boolean(wait)]);

        let response = self.send(&request).await?;
        let result = response
            .groups_of(EVENT_NOTIFICATION_ATTRIBUTES)
            .filter_map(|group| {
                let sequence = group.first("notify-sequence-number")?.as_integer()?;
                let name = group.first("notify-subscribed-event")?.as_str()?.to_owned();
                Some(Event { sequence, name })
            })
            .collect();
        Ok(result)
    }

    /// Cancel `subscription`.
    ///
    /// # Errors
    ///
    /// As for [`EventClient::subscribe`].
    pub async fn cancel(&self, subscription: i32) -> Result<(), EventError> {
        let mut request = self.request(CANCEL_SUBSCRIPTION);
        request.add(OPERATION_ATTRIBUTES, "notify-subscription-id", vec![Value::integer(subscription)]);
        self.send(&request).await.map(|_| ())
    }

    fn request(&self, code: u16) -> Message {
        Message::request(code, self.next_request_id.fetch_add(1, Ordering::Relaxed), &self.printer_uri)
    }

    async fn send(&self, request: &Message) -> Result<Message, EventError> {
        let body = self
            .http
            .post(&self.url)
            .header(http::header::CONTENT_TYPE, "application/ipp")
            .body(request.encode())
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        let response = Message::decode(&body)?;
        if response.is_success() { Ok(response) } else { Err(EventError::Status(response.code)) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{Config, FakePrinter};

    fn client(fake: &FakePrinter) -> EventClient {
        EventClient::new(&fake.uri().parse().unwrap(), Duration::from_secs(5))
    }

    #[test]
    fn messages_round_trip() {
        let mut message = Message::request(GET_NOTIFICATIONS, 7, "ipp://printer/ipp/print");
        message.add(OPERATION_ATTRIBUTES, "notify-wait", vec![Value::boolean(true)]);
        message.add(SUBSCRIPTION_ATTRIBUTES, "notify-events", vec![Value::keyword("job-created"), Value::keyword("job-completed")]);
        message.start_group(EVENT_NOTIFICATION_ATTRIBUTES);
        message.add(EVENT_NOTIFICATION_ATTRIBUTES, "job-state", vec![Value::enumeration(9)]);
        message.start_group(EVENT_NOTIFICATION_ATTRIBUTES);
        message.add(EVENT_NOTIFICATION_ATTRIBUTES, "notify-text", vec![Value::text("Done")]);

        let decoded = Message::decode(&message.encode()).unwrap();

        assert_eq!(decoded, message);
        assert_eq!(decoded.groups_of(EVENT_NOTIFICATION_ATTRIBUTES).count(), 2);
        let operation = decoded.groups_of(OPERATION_ATTRIBUTES).next().unwrap();
        assert_eq!(operation.first("printer-uri").and_then(Value::as_str), Some("ipp://printer/ipp/print"));
        assert_eq!(operation.first("notify-wait").and_then(Value::as_bool), Some(true));
        assert_eq!(operation.first("missing"), None);
        let events = &decoded.groups_of(SUBSCRIPTION_ATTRIBUTES).next().unwrap().attributes[0];
        assert_eq!(events.values.len(), 2, "additional values join the attribute");
    }

    #[test]
    fn values_only_convert_to_their_own_types() {
        assert_eq!(Value::integer(-3).as_integer(), Some(-3));
        assert_eq!(Value::enumeration(5).as_integer(), Some(5));
        assert_eq!(Value::keyword("x").as_integer(), None);
        assert_eq!(Value { tag: INTEGER, bytes: vec![1] }.as_integer(), None);
        assert_eq!(Value::boolean(false).as_bool(), Some(false));
        assert_eq!(Value::integer(1).as_bool(), None);
        assert_eq!(Value::integer(1).as_str(), None);
        assert_eq!(Value { tag: KEYWORD, bytes: vec![0xff] }.as_str(), None);
    }

    #[test]
    fn decoding_rejects_malformed_messages() {
        let valid = Message::response(0, 1).encode();
        assert_eq!(Message::decode(&valid[..valid.len() - 1]), Err(DecodeError::Truncated));
        assert_eq!(Message::decode(&[2, 0, 0, 0, 0, 0, 0, 1, INTEGER, 0, 0, 0, 0]), Err(DecodeError::NoGroup));
        assert!(!Message::response(0x0400, 1).is_success());
    }

    #[test]
    fn ipps_uris_are_sent_over_https() {
        let secure = EventClient::new(&"ipps://printer:631/ipp/print".parse().unwrap(), Duration::from_secs(1));
        assert_eq!(secure.url, "https://printer:631/ipp/print");
        let plain = EventClient::new(&"ipp://printer:631/ipp/print".parse().unwrap(), Duration::from_secs(1));
        assert_eq!(plain.url, "http://printer:631/ipp/print");
    }

    #[tokio::test]
    async fn subscribes_waits_for_events_and_cancels() {
        let fake = FakePrinter::start(Config { notifications: true, notify_wait: Duration::from_secs(5), ..Config::default() }).await;
        let client = client(&fake);

        let subscription = client.subscribe(&["printer-state-changed"], Duration::from_secs(60)).await.unwrap();
        assert_eq!(fake.subscriptions(), [subscription]);

        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.notifications(subscription, 1, true).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        fake.configure(|c| c.printer_state_message = Some("Warming up"));
        let events = waiting.await.unwrap().unwrap();
        assert_eq!(events, [Event { sequence: 1, name: "printer-state-changed".to_owned() }]);

        assert!(client.notifications(subscription, 2, false).await.unwrap().is_empty());

        client.cancel(subscription).await.unwrap();
        assert!(fake.subscriptions().is_empty());
        let gone = client.notifications(subscription, 2, false).await.unwrap_err();
        assert!(matches!(gone, EventError::Status(CLIENT_ERROR_NOT_FOUND)), "{gone}");
    }

    #[tokio::test]
    async fn printers_without_notifications_refuse() {
        let fake = FakePrinter::start(Config::default()).await;
        let err = client(&fake).subscribe(&["job-created"], Duration::from_secs(60)).await.unwrap_err();
        assert!(matches!(err, EventError::Status(SERVER_ERROR_OPERATION_NOT_SUPPORTED)), "{err}");
        assert!(err.is_refusal());
    }

    #[tokio::test]
    async fn unreachable_printers_are_not_refusals() {
        let fake = FakePrinter::start(Config { online: false, ..Config::default() }).await;
        let err = client(&fake).subscribe(&["job-created"], Duration::from_secs(60)).await.unwrap_err();
        assert!(matches!(err, EventError::Http(_)), "{err}");
        assert!(!err.is_refusal());
    }
}
