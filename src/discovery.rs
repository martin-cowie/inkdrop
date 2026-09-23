use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
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
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    /// The IPP resource path, e.g. "ipp/print" (from the TXT "rp" key).
    pub resource_path: String,
    /// Model string from the TXT "ty" key, e.g. "DeskJet 3630 series".
    pub model: Option<String>,
    /// Display labels of the formats this printer accepts, e.g.
    /// `["PDF", "URF", "PWG-Raster"]`. Always non-empty for a printer in the
    /// registry.
    pub formats: Vec<&'static str>,
}

impl Printer {
    pub fn ipp_uri(&self) -> String {
        format!(
            "ipp://{}:{}/{}",
            self.ip,
            self.port,
            self.resource_path.trim_start_matches('/')
        )
    }

    pub fn address(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }
}

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

/// Spawn background tasks that browse mDNS for IPP printers that handle URF
/// or PWG-Raster, keeping `registry` up to date as printers appear and
/// disappear.
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

/// Add printers given by URI (e.g. `ipp://localhost:1631/ipp/print`) to
/// `registry`, bypassing mDNS. Each is probed over IPP periodically, and is
/// listed only while it answers and handles a supported format.
pub fn spawn_configured(uris: Vec<String>, registry: Registry) {
    for uri in uris {
        let registry = registry.clone();
        tokio::spawn(async move { watch_configured(uri, registry).await });
    }
}

async fn watch_configured(uri: String, registry: Registry) {
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

    let mut interval = tokio::time::interval(CONFIGURED_PROBE_INTERVAL);
    let mut last_problem: Option<String> = None;
    loop {
        interval.tick().await;
        match configured_printer(&parsed, id.clone()).await {
            Ok(printer) => {
                if last_problem.is_some() || !registry.borrow().contains_key(&id) {
                    info!(name = %printer.name, uri = %printer.ipp_uri(), formats = ?printer.formats, "added configured printer");
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
    let host = uri.host().unwrap_or_default().trim_matches(['[', ']']);
    let port = uri.port_u16().unwrap_or(631);

    let ip = tokio::net::lookup_host((host, port))
        .await
        .map_err(|err| format!("cannot resolve {host}: {err}"))?
        .map(|addr| addr.ip())
        .min_by_key(|ip| !ip.is_ipv4())
        .ok_or_else(|| format!("no addresses for {host}"))?;

    let info = crate::printing::probe_printer(uri).await.map_err(|err| err.to_string())?;
    if info.formats.is_empty() {
        return Err("printer does not handle a supported format".to_owned());
    }

    Ok(Printer {
        id,
        name: info.name.unwrap_or_else(|| host.to_owned()),
        ip,
        port,
        resource_path: uri.path().to_owned(),
        model: info.model,
        formats: info.formats,
    })
}

async fn browse(service_type: &'static str, registry: Registry, merge_state: MergeState) -> Result<(), mdns_sd::Error> {
    let secure = service_type == IPPS_SERVICE;
    let daemon = ServiceDaemon::new()?;
    let receiver = daemon.browse(service_type)?;

    // Maps this task's own mDNS fullnames to the merged printer id they
    // resolved to, so a later removal (which only gives us the fullname)
    // can find the right slot to clear. Local to this task, since each task
    // only ever needs to undo entries it itself inserted.
    let mut fullname_to_id: HashMap<String, String> = HashMap::new();

    while let Ok(event) = receiver.recv_async().await {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let id = handle_resolved(*info, secure, &registry, &merge_state);
                if let Some((fullname, id)) = id {
                    fullname_to_id.insert(fullname, id);
                }
            }
            ServiceEvent::ServiceRemoved(_service_type, fullname) => {
                if let Some(id) = fullname_to_id.remove(&fullname) {
                    remove_transport(&id, secure, &registry, &merge_state);
                }
            }
            _ => {}
        }
    }

    Ok(())
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
            info!(name = %printer.name, uri = %printer.ipp_uri(), formats = ?printer.formats, "discovered printer (via mDNS pdl)");
            upsert_transport(id.clone(), secure, printer, registry, merge_state);
        }
        // No "pdl" TXT key at all: some printers omit it even though they
        // support one of our formats. Ask the printer directly.
        _ => {
            let printer = build_printer(&info, id.clone(), Vec::new())?;
            let registry = registry.clone();
            let merge_state = merge_state.clone();
            tokio::spawn(async move {
                let Ok(uri) = printer.ipp_uri().parse() else {
                    return;
                };
                let formats = crate::printing::probe_formats(&uri).await;
                if formats.is_empty() {
                    debug!(name = %printer.name, "printer does not handle a supported format");
                    return;
                }

                let mut printer = printer;
                printer.formats = formats;
                info!(name = %printer.name, uri = %printer.ipp_uri(), formats = ?printer.formats, "discovered printer (via IPP probe)");
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

    let resource_path = info
        .txt_properties
        .get_property_val_str("rp")
        .unwrap_or("ipp/print")
        .to_owned();

    let name = display_name(&info.fullname);
    let model = info.txt_properties.get_property_val_str("ty").map(str::to_owned);

    Some(Printer {
        id,
        name,
        ip,
        port: info.port,
        resource_path,
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
