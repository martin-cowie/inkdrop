//! Finds printers that handle a supported format, by browsing mDNS or by
//! probing configured URIs over IPP, and keeps a [`Registry`] of them.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http::Uri;
use mdns_sd::{ResolvedService, ServiceDaemon, ServiceEvent};
use tokio::sync::watch;
use tracing::{debug, info, warn};

/// A printer, discovered via mDNS or configured by URI, that handles at least
/// one of [`crate::raster::SUPPORTED_FORMATS`].
#[derive(Clone, Debug)]
pub struct Printer {
    /// Stable key for this printer in the [`Registry`], shared by its `_ipp`
    /// and `_ipps` advertisements.
    pub id: String,
    /// Display name: the mDNS instance name, or for a configured printer its
    /// `printer-info` or `printer-name`.
    pub name: String,
    /// Where to send IPP requests, e.g. `ipp://192.168.1.20:631/ipp/print`.
    pub uri: Uri,
    /// Model, from the TXT "ty" key or `printer-make-and-model`, e.g.
    /// "DeskJet 3630 series".
    pub model: Option<String>,
    /// Display labels of the formats this printer accepts, e.g.
    /// `["PDF", "URF", "PWG-Raster"]`. Always non-empty for a printer in the
    /// registry.
    pub formats: Vec<&'static str>,
}

/// The printers currently available, keyed by [`Printer::id`]. Subscribers
/// are notified whenever a printer appears, changes or disappears.
pub type Registry = watch::Sender<HashMap<String, Printer>>;

const IPP_SERVICE: &str = "_ipp._tcp.local.";
const IPPS_SERVICE: &str = "_ipps._tcp.local.";

/// A printer can be advertised under both `_ipp._tcp` (plain) and
/// `_ipps._tcp` (TLS) as two separate mDNS service instances. These slots
/// track both for one physical printer so it shows up once, preferring the
/// encrypted transport when both are present.
#[derive(Default)]
struct TransportSlots {
    secure: Option<Printer>,
    insecure: Option<Printer>,
}

impl TransportSlots {
    fn best(&self) -> Option<&Printer> {
        self.secure.as_ref().or(self.insecure.as_ref())
    }
}

type MergeState = Arc<Mutex<HashMap<String, TransportSlots>>>;

/// Spawn background tasks that browse mDNS for IPP printers that handle a
/// supported format, keeping `registry` up to date as printers appear and
/// disappear. Printers that advertise no "pdl" TXT key are asked over IPP.
///
/// # Panics
///
/// If called outside a Tokio runtime.
pub fn spawn(registry: Registry) {
    let merge_state: MergeState = Arc::new(Mutex::new(HashMap::new()));

    for service_type in [IPP_SERVICE, IPPS_SERVICE] {
        let registry = registry.clone();
        let merge_state = merge_state.clone();
        tokio::spawn(async move {
            if let Err(err) = browse(service_type, registry, merge_state).await {
                warn!(service_type, %err, "mDNS browse task ended");
            }
        });
    }
}

/// How often a configured printer is re-probed, so that it appears and
/// disappears as it starts and stops, like an mDNS-advertised one.
const CONFIGURED_PROBE_INTERVAL: Duration = Duration::from_secs(10);

/// Add the printers at `uris` (e.g. `ipp://localhost:1631/ipp/print`) to
/// `registry`, bypassing mDNS. Each is probed over IPP periodically, and is
/// listed only while it answers and handles a supported format. A URI that
/// isn't `ipp://host...` is logged and ignored.
///
/// # Panics
///
/// If called outside a Tokio runtime.
pub fn spawn_configured(uris: Vec<String>, registry: Registry) {
    for uri in uris {
        let registry = registry.clone();
        tokio::spawn(async move { watch_configured(uri, registry, CONFIGURED_PROBE_INTERVAL).await });
    }
}

async fn watch_configured(uri: String, registry: Registry, probe_interval: Duration) {
    let parsed = match uri.parse::<Uri>() {
        Ok(parsed) if parsed.scheme_str() == Some("ipp") && parsed.host().is_some() => parsed,
        _ => {
            warn!(%uri, "ignoring configured printer: expected a URI like ipp://host:631/ipp/print");
            return;
        }
    };

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    uri.hash(&mut hasher);
    let id = format!("configured-{:016x}", hasher.finish());

    let mut interval = tokio::time::interval(probe_interval);
    let mut last_problem: Option<String> = None;
    loop {
        interval.tick().await;
        match configured_printer(&parsed, id.clone()).await {
            Ok(printer) => {
                if last_problem.is_some() || !registry.borrow().contains_key(&id) {
                    info!(name = %printer.name, uri = %printer.uri, formats = ?printer.formats, "added configured printer");
                }
                last_problem = None;
                apply_best(id.clone(), Some(printer), &registry);
            }
            Err(problem) => {
                if last_problem.as_ref() != Some(&problem) {
                    warn!(%uri, %problem, "configured printer unavailable; will keep retrying");
                }
                last_problem = Some(problem);
                apply_best(id.clone(), None, &registry);
            }
        }
    }
}

async fn configured_printer(uri: &Uri, id: String) -> Result<Printer, String> {
    let info = crate::printing::probe_printer(uri).await.map_err(|err| err.to_string())?;
    if info.formats.is_empty() {
        return Err("printer does not handle a supported format".to_owned());
    }

    Ok(Printer {
        id,
        name: info.name.unwrap_or_else(|| uri.host().unwrap_or_default().to_owned()),
        uri: uri.clone(),
        model: info.model,
        formats: info.formats,
    })
}

async fn browse(service_type: &'static str, registry: Registry, merge_state: MergeState) -> Result<(), mdns_sd::Error> {
    let secure = service_type == IPPS_SERVICE;
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(service_type)?;

    // A removal event gives only the fullname, not the merged id.
    let mut fullname_to_id: HashMap<String, String> = HashMap::new();

    while let Ok(event) = receiver.recv_async().await {
        handle_event(event, secure, &mut fullname_to_id, &registry, &merge_state);
    }

    Ok(())
}

fn handle_event(
    event: ServiceEvent,
    secure: bool,
    fullname_to_id: &mut HashMap<String, String>,
    registry: &Registry,
    merge_state: &MergeState,
) {
    match event {
        ServiceEvent::ServiceResolved(info) => {
            if let Some((fullname, id)) = handle_resolved(*info, secure, registry, merge_state) {
                fullname_to_id.insert(fullname, id);
            }
        }
        ServiceEvent::ServiceRemoved(_service_type, fullname) => {
            if let Some(id) = fullname_to_id.remove(&fullname) {
                remove_transport(&id, secure, registry, merge_state);
            }
        }
        _ => {}
    }
}

/// Handles one resolved mDNS service, updating the registry as needed.
/// Returns the (fullname, merged id) pair for the caller to remember, so a
/// later removal of this exact service can be matched back to it.
fn handle_resolved(
    info: ResolvedService,
    secure: bool,
    registry: &Registry,
    merge_state: &MergeState,
) -> Option<(String, String)> {
    debug!(
        fullname = %info.fullname,
        host = %info.host,
        port = info.port,
        pdl = ?info.txt_properties.get_property_val_str("pdl"),
        "resolved mDNS service"
    );

    let id = merged_id(&info);
    let fullname = info.fullname.clone();

    match info.txt_properties.get_property_val_str("pdl") {
        Some(pdl) if !pdl.is_empty() => {
            let formats = crate::raster::matching_labels(pdl.split(',').map(str::trim));
            if formats.is_empty() {
                remove_transport(&id, secure, registry, merge_state);
                return Some((fullname, id));
            }

            let printer = build_printer(&info, id.clone(), formats)?;
            info!(name = %printer.name, uri = %printer.uri, formats = ?printer.formats, "discovered printer (via mDNS pdl)");
            upsert_transport(id.clone(), secure, printer, registry, merge_state);
        }
        // Some printers omit "pdl" even though they support one of our formats.
        _ => {
            let printer = build_printer(&info, id.clone(), Vec::new())?;
            let registry = registry.clone();
            let merge_state = merge_state.clone();
            tokio::spawn(async move {
                let formats = crate::printing::probe_formats(&printer.uri).await;
                if formats.is_empty() {
                    debug!(name = %printer.name, "printer does not handle a supported format");
                    return;
                }

                let mut printer = printer;
                printer.formats = formats;
                info!(name = %printer.name, uri = %printer.uri, formats = ?printer.formats, "discovered printer (via IPP probe)");
                upsert_transport(printer.id.clone(), secure, printer, &registry, &merge_state);
            });
        }
    }

    Some((fullname, id))
}

fn upsert_transport(id: String, secure: bool, printer: Printer, registry: &Registry, merge_state: &MergeState) {
    let best = {
        let mut state = merge_state.lock().unwrap();
        let slots = state.entry(id.clone()).or_default();
        if secure {
            slots.secure = Some(printer);
        } else {
            slots.insecure = Some(printer);
        }
        slots.best().cloned()
    };
    apply_best(id, best, registry);
}

fn remove_transport(id: &str, secure: bool, registry: &Registry, merge_state: &MergeState) {
    let best = {
        let mut state = merge_state.lock().unwrap();
        let Some(slots) = state.get_mut(id) else {
            return;
        };
        if secure {
            slots.secure = None;
        } else {
            slots.insecure = None;
        }
        let best = slots.best().cloned();
        if slots.secure.is_none() && slots.insecure.is_none() {
            state.remove(id);
        }
        best
    };
    apply_best(id.to_owned(), best, registry);
}

fn apply_best(id: String, best: Option<Printer>, registry: &Registry) {
    registry.send_modify(|map| match best {
        Some(printer) => {
            map.insert(id, printer);
        }
        None => {
            map.remove(&id);
        }
    });
}

fn build_printer(info: &ResolvedService, id: String, formats: Vec<&'static str>) -> Option<Printer> {
    let ip = info
        .addresses
        .iter()
        .find(|addr| addr.is_ipv4())
        .or_else(|| info.addresses.iter().next())?
        .to_ip_addr();

    // The TXT "rp" key is the resource path without its leading slash.
    let resource_path = info.txt_properties.get_property_val_str("rp").unwrap_or("ipp/print");

    // SocketAddr brackets IPv6 addresses, as the URI authority requires.
    let uri = Uri::builder()
        .scheme("ipp")
        .authority(SocketAddr::new(ip, info.port).to_string())
        .path_and_query(format!("/{}", resource_path.trim_start_matches('/')))
        .build()
        .ok()?;

    let name = display_name(&info.fullname);
    let model = info.txt_properties.get_property_val_str("ty").map(str::to_owned);

    Some(Printer {
        id,
        name,
        uri,
        model,
        formats,
    })
}

/// A stable id for the physical printer behind `info`, independent of which
/// transport (`_ipp._tcp` vs `_ipps._tcp`) it was seen on — derived from its
/// instance name plus host, so the two transports of one printer merge, but
/// two different printers that happen to share a friendly name (on
/// different hosts) don't.
fn merged_id(info: &ResolvedService) -> String {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    display_name(&info.fullname).hash(&mut hasher);
    info.host.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// The instance name portion of the fullname (before the first dot), e.g.
/// "Cheap HP Printer" from "Cheap HP Printer._ipp._tcp.local.".
fn display_name(fullname: &str) -> String {
    fullname.split('.').next().unwrap_or(fullname).to_owned()
}

#[cfg(test)]
mod tests {
    use mdns_sd::ServiceInfo;

    use super::*;
    use crate::test_support::{self, Config, FakePrinter};

    const WAIT: Duration = Duration::from_secs(5);

    fn resolved(ty: &str, name: &str, host: &str, ips: &str, port: u16, txt: &[(&str, &str)]) -> ResolvedService {
        ServiceInfo::new(ty, name, host, ips, port, txt).unwrap().as_resolved_service()
    }

    fn office(ty: &str, port: u16, txt: &[(&str, &str)]) -> ResolvedService {
        resolved(ty, "Office Printer", "office.local.", "192.168.1.20", port, txt)
    }

    fn new_registry() -> Registry {
        watch::channel(HashMap::new()).0
    }

    /// Wait until the registry satisfies `condition`, failing after [`WAIT`].
    async fn wait_until(registry: &Registry, condition: impl FnMut(&HashMap<String, Printer>) -> bool) {
        let mut receiver = registry.subscribe();
        tokio::time::timeout(WAIT, receiver.wait_for(condition))
            .await
            .expect("timed out waiting for the registry")
            .unwrap();
    }

    /// Process `events` as one browse task would, returning its fullname map.
    fn feed(events: Vec<ServiceEvent>, secure: bool, registry: &Registry, merge: &MergeState) -> HashMap<String, String> {
        let mut fullname_to_id = HashMap::new();
        for event in events {
            handle_event(event, secure, &mut fullname_to_id, registry, merge);
        }
        fullname_to_id
    }

    fn only_printer(registry: &Registry) -> Printer {
        let printers = registry.borrow();
        assert_eq!(printers.len(), 1, "{printers:?}");
        printers.values().next().unwrap().clone()
    }

    #[test]
    fn display_name_is_the_instance_name() {
        assert_eq!(display_name("Cheap HP Printer._ipp._tcp.local."), "Cheap HP Printer");
        assert_eq!(display_name("bare"), "bare");
    }

    #[test]
    fn merged_ids_match_across_transports_but_not_hosts() {
        let ipp = office(IPP_SERVICE, 631, &[]);
        let ipps = office(IPPS_SERVICE, 443, &[]);
        let elsewhere = resolved(IPP_SERVICE, "Office Printer", "attic.local.", "192.168.1.21", 631, &[]);
        assert_eq!(merged_id(&ipp), merged_id(&ipps));
        assert_ne!(merged_id(&ipp), merged_id(&elsewhere));
    }

    #[test]
    fn builds_printers_from_resolved_services() {
        let info = resolved(IPP_SERVICE, "Office Printer", "office.local.", "fe80::1,192.168.1.20", 631, &[
            ("rp", "ipp/print"),
            ("ty", "LaserJet 9000"),
        ]);
        let printer = build_printer(&info, "id".to_owned(), vec!["PDF"]).unwrap();
        assert_eq!(printer.uri, "ipp://192.168.1.20:631/ipp/print", "IPv4 preferred");
        assert_eq!(printer.name, "Office Printer");
        assert_eq!(printer.model.as_deref(), Some("LaserJet 9000"));
        assert_eq!(printer.formats, ["PDF"]);

        let ipv6 = resolved(IPP_SERVICE, "P", "p.local.", "fe80::1", 8631, &[("rp", "/printers/q")]);
        let printer = build_printer(&ipv6, "id".to_owned(), Vec::new()).unwrap();
        assert_eq!(printer.uri, "ipp://[fe80::1]:8631/printers/q");
        assert_eq!(printer.model, None);

        let default_path = build_printer(&office(IPP_SERVICE, 631, &[]), "id".to_owned(), Vec::new()).unwrap();
        assert_eq!(default_path.uri.path(), "/ipp/print");

        let no_address = resolved(IPP_SERVICE, "P", "p.local.", "", 631, &[]);
        assert!(build_printer(&no_address, "id".to_owned(), Vec::new()).is_none());
    }

    #[test]
    fn pdl_listing_a_supported_format_adds_the_printer() {
        test_support::init_tracing();
        let (registry, merge) = (new_registry(), MergeState::default());
        let info = office(IPP_SERVICE, 631, &[("pdl", "application/octet-stream, image/urf,application/pdf")]);

        let tracked = feed(vec![ServiceEvent::ServiceResolved(Box::new(info))], false, &registry, &merge);

        let printer = only_printer(&registry);
        assert_eq!(printer.formats, ["PDF", "URF"]);
        assert_eq!(printer.uri, "ipp://192.168.1.20:631/ipp/print");
        assert_eq!(tracked.get("Office Printer._ipp._tcp.local."), Some(&printer.id));
    }

    #[test]
    fn pdl_without_a_supported_format_removes_the_printer() {
        let (registry, merge) = (new_registry(), MergeState::default());
        let supported = office(IPP_SERVICE, 631, &[("pdl", "application/pdf")]);
        let unsupported = office(IPP_SERVICE, 631, &[("pdl", "application/postscript")]);

        let tracked = feed(
            vec![ServiceEvent::ServiceResolved(Box::new(supported)), ServiceEvent::ServiceResolved(Box::new(unsupported))],
            false,
            &registry,
            &merge,
        );

        assert!(registry.borrow().is_empty());
        assert!(merge.lock().unwrap().is_empty());
        assert_eq!(tracked.len(), 1, "still tracked, for a later removal event");
    }

    #[test]
    fn services_without_an_address_are_ignored() {
        let (registry, merge) = (new_registry(), MergeState::default());
        let info = resolved(IPP_SERVICE, "P", "p.local.", "", 631, &[("pdl", "application/pdf")]);
        let tracked = feed(vec![ServiceEvent::ServiceResolved(Box::new(info))], false, &registry, &merge);
        assert!(registry.borrow().is_empty());
        assert!(tracked.is_empty());
    }

    #[tokio::test]
    async fn printers_without_pdl_are_probed_over_ipp() {
        let fake = FakePrinter::start(Config::raster_only(300, &["srgb_8"])).await;
        let (registry, merge) = (new_registry(), MergeState::default());
        let info = resolved(IPP_SERVICE, "Quiet", "quiet.local.", "127.0.0.1", fake.port(), &[("rp", "ipp/print")]);

        feed(vec![ServiceEvent::ServiceResolved(Box::new(info))], false, &registry, &merge);

        wait_until(&registry, |printers| !printers.is_empty()).await;
        assert_eq!(only_printer(&registry).formats, ["PWG-Raster"]);
    }

    #[tokio::test]
    async fn probed_printers_without_a_supported_format_are_left_out() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config { formats: vec!["application/postscript"], ..Config::default() }).await;
        let (registry, merge) = (new_registry(), MergeState::default());
        let info = resolved(IPP_SERVICE, "PS", "ps.local.", "127.0.0.1", fake.port(), &[("pdl", "")]);

        feed(vec![ServiceEvent::ServiceResolved(Box::new(info))], false, &registry, &merge);

        tokio::time::timeout(WAIT, async {
            while fake.received().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("printer should be probed");
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(registry.borrow().is_empty());
    }

    #[test]
    fn secure_transport_is_preferred_and_removals_fall_back() {
        let (registry, merge) = (new_registry(), MergeState::default());
        let pdl = [("pdl", "application/pdf")];

        let mut insecure = feed(
            vec![ServiceEvent::ServiceResolved(Box::new(office(IPP_SERVICE, 631, &pdl)))],
            false,
            &registry,
            &merge,
        );
        let mut secure = feed(
            vec![ServiceEvent::ServiceResolved(Box::new(office(IPPS_SERVICE, 443, &pdl)))],
            true,
            &registry,
            &merge,
        );
        assert_eq!(only_printer(&registry).uri.port_u16(), Some(443));

        let removed = ServiceEvent::ServiceRemoved(IPPS_SERVICE.to_owned(), "Office Printer._ipps._tcp.local.".to_owned());
        handle_event(removed, true, &mut secure, &registry, &merge);
        assert_eq!(only_printer(&registry).uri.port_u16(), Some(631));

        let unknown = ServiceEvent::ServiceRemoved(IPP_SERVICE.to_owned(), "Other._ipp._tcp.local.".to_owned());
        handle_event(unknown, false, &mut insecure, &registry, &merge);
        handle_event(ServiceEvent::SearchStarted(IPP_SERVICE.to_owned()), false, &mut insecure, &registry, &merge);
        assert_eq!(registry.borrow().len(), 1);

        let removed = ServiceEvent::ServiceRemoved(IPP_SERVICE.to_owned(), "Office Printer._ipp._tcp.local.".to_owned());
        handle_event(removed, false, &mut insecure, &registry, &merge);
        assert!(registry.borrow().is_empty());
        assert!(merge.lock().unwrap().is_empty());
    }

    #[test]
    fn removing_an_untracked_printer_does_nothing() {
        let registry = new_registry();
        remove_transport("missing", true, &registry, &MergeState::default());
        assert!(registry.borrow().is_empty());
    }

    #[tokio::test]
    async fn configured_uris_must_be_ipp_with_a_host() {
        test_support::init_tracing();
        for uri in ["http://printer/ipp/print", "ipp:/ipp/print", "not a uri"] {
            let registry = new_registry();
            tokio::time::timeout(WAIT, watch_configured(uri.to_owned(), registry.clone(), Duration::from_millis(10)))
                .await
                .unwrap_or_else(|_| panic!("{uri} should be rejected immediately"));
            assert!(registry.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn configured_printers_come_and_go_with_the_printer() {
        test_support::init_tracing();
        let fake = FakePrinter::start(Config::default()).await;
        let registry = new_registry();
        let task = tokio::spawn(watch_configured(fake.uri(), registry.clone(), Duration::from_millis(20)));

        wait_until(&registry, |printers| !printers.is_empty()).await;
        let printer = only_printer(&registry);
        assert!(printer.id.starts_with("configured-"), "{}", printer.id);
        assert_eq!(printer.name, "Fake Printer");
        assert_eq!(printer.model.as_deref(), Some("Fake Model 1"));
        assert_eq!(printer.formats, ["PDF"]);
        assert_eq!(printer.uri.to_string(), fake.uri());

        fake.configure(|c| c.online = false);
        wait_until(&registry, HashMap::is_empty).await;

        fake.configure(|c| c.formats = vec!["application/postscript"]);
        fake.configure(|c| c.online = true);
        let probes = fake.received().len();
        tokio::time::timeout(WAIT, async {
            while fake.received().len() < probes + 2 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("printer should be probed again");
        assert!(registry.borrow().is_empty(), "no supported format");

        fake.configure(|c| {
            c.formats = vec!["image/pwg-raster"];
            c.printer_info = None;
            c.printer_name = None;
        });
        wait_until(&registry, |printers| !printers.is_empty()).await;
        assert_eq!(only_printer(&registry).name, "127.0.0.1", "falls back to the host");

        task.abort();
    }

    #[tokio::test]
    async fn spawn_configured_watches_each_uri() {
        let fake = FakePrinter::start(Config::default()).await;
        let registry = new_registry();

        spawn_configured(vec![fake.uri()], registry.clone());

        wait_until(&registry, |printers| !printers.is_empty()).await;
    }

    #[tokio::test]
    async fn spawn_starts_browsing_without_error() {
        let registry = new_registry();
        spawn(registry.clone());
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
