//! A whole simulated building from one `devices.yaml`: every device the file
//! names, served at once, in one process.
//!
//! ```text
//! # exactly what the file says, which only works where those addresses are local
//! simulated-fleet devices.yaml
//!
//! # somebody else's fleet, on this laptop
//! simulated-fleet devices.yaml --host 127.0.0.1 --port-base 2301
//!
//! # ...with the kernel choosing every port, so nothing can collide
//! simulated-fleet devices.yaml --host 127.0.0.1 --port-base 0
//!
//! # what it would do, without binding anything
//! simulated-fleet devices.yaml --host 127.0.0.1 --port-base 2301 --dry-run --json
//!
//! # a fleet of base units: no Dual Mode, no Confidence stream
//! simulated-fleet devices.yaml --licensed-streams 1
//! ```
//!
//! # The input is not re-parsed
//!
//! The file is read by [`sismatic_core::devices::config::load`] — the same
//! function the server's composition root calls. So `[defaults]` inheritance,
//! the one id namespace for devices and groups, credential presence and the
//! canonicalisation of `disabled_fields` all behave here exactly as they do in
//! production, because they *are* production: a second reader of this schema
//! would be a second thing to keep in step, and the failure would be a fleet
//! that simulates a document nobody wrote.
//!
//! That buys one derivation worth naming. A device's `disabled_fields` is the
//! operator's claim that this recorder cannot answer those fields; here it
//! becomes the simulated unit's refusal set, so the document's claim is the
//! simulated hardware's behaviour rather than a note beside it.
//!
//! # Shape
//!
//! Everything between the file and the sockets is pure. [`plan`] turns a
//! resolved document plus a [`Placement`] into [`Unit`]s, [`collisions`] says
//! whether that plan can be bound at all, [`overrides_for`] maps a unit onto
//! the simulator's own [`Overrides`], and [`report`] renders. None of them
//! touch the network, the clock or the filesystem, which is why the tests at
//! the bottom can check the interesting decisions without binding a port.
//!
//! The effects are [`main`]'s: read the file, bind, print, wait. The one place
//! they meet the pure half is [`bind_unit`].

use std::collections::{BTreeMap, BTreeSet};
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use serde::Serialize;
use sismatic_core::devices::config::{
    self, Barrier, ConfigError, DeviceConfig, GroupConfig, Password, Resolved,
};
use sismatic_core::simulator::{
    DeviceState, Issue, Overrides, ReplyStream, SimulatedDevice, bind, canonical_name,
    catalog_streams,
};
use tracing_subscriber::EnvFilter;

/// The device template every unit starts from — the checked-in fixture, which
/// covers the instruction catalog exactly. Compiled in rather than read from a
/// path so the binary runs from any directory.
const BUILT_IN_DEVICE: &str = include_str!("../../tests/fixtures/device.yaml");

#[derive(Parser, Debug)]
#[command(
    name = "simulated-fleet",
    about = "Serve a simulated Extron SMP for every device in a devices.yaml",
    after_help = "Two schemas share the name `device(s).yaml`, and this takes both:\n\
                  \x20 the POSITIONAL argument is a devices file — the inventory, listing \
                  which recorders exist (`devices:`, each with an `id` and a `host`).\n\
                  \x20 --device-config is a device template — one SMP's field table \
                  (`fields:`, plus its licence block).\n\n\
                  Without --host and --port-base the fleet is bound exactly as the \
                  inventory describes it, which needs those addresses to be local. \
                  Use --dry-run to see the placement before binding it."
)]
struct Args {
    /// The devices file to simulate (`.yaml`, `.yml` or `.toml`).
    config: PathBuf,

    /// Bind every unit on this address instead of the one the file gives it.
    ///
    /// A devices file names the real recorders' addresses, so `--host
    /// 127.0.0.1` is what makes somebody else's fleet servable here.
    #[arg(long, value_name = "ADDR")]
    host: Option<String>,

    /// Assign ports from here upward, in document order, instead of the ones
    /// the file gives.
    ///
    /// `0` is special and useful: every unit gets a kernel-assigned port, so
    /// no two can collide and nothing has to be reserved. The report names the
    /// ports that were actually handed out.
    #[arg(long, value_name = "PORT")]
    port_base: Option<u16>,

    /// Device template (YAML) every unit starts from. Defaults to the built-in
    /// one, which is the fixture the test suite pins.
    #[arg(long, value_name = "FILE")]
    device_config: Option<PathBuf>,

    /// Device template for one device, by id. Repeatable, and beats
    /// `--device-config` for the device it names.
    ///
    /// This is how one fleet simulates several configurations: a base unit
    /// beside a fully-loaded one, a recorder on older firmware, a room whose
    /// alarms are already firing. A template carries its own licence block, so
    /// per-device licences live here rather than in a flag of their own.
    #[arg(long, value_name = "ID=FILE", value_parser = parse_device_template)]
    device_config_for: Vec<(String, PathBuf)>,

    /// Streams every unit is licensed for: `1` for base units, `1,2` with Dual
    /// Mode, `all`, or `none`.
    ///
    /// Fleet-wide, because a devices file has no notion of a stream licence —
    /// it describes what the server may ask, not what the hardware has. Per-
    /// unit refusals still come from each device's own `disabled_fields`.
    #[arg(long, value_name = "LIST", value_parser = parse_streams)]
    licensed_streams: Option<BTreeSet<u8>>,

    /// Print the placement and exit, binding nothing.
    #[arg(long)]
    dry_run: bool,

    /// Report as JSON rather than as a table.
    #[arg(long)]
    json: bool,

    /// Print each unit's password. Masked by default.
    ///
    /// The input is a real devices file, so its credentials are real: a report
    /// that printed them unasked would put them in terminal scrollback, CI
    /// logs and screenshots for everyone who ever runs this. Usernames are not
    /// masked — they are not secrets, and they are needed to read the report.
    #[arg(long)]
    show_credentials: bool,

    /// Answer on stdout instead of the extended-data (stderr) stream real
    /// Extron hardware uses.
    #[arg(long)]
    stdout: bool,

    #[arg(short, long, env = "RUST_LOG", default_value = "info")]
    log_level: String,
}

/// Parse `--device-config-for ID=FILE`.
///
/// Split at the *first* `=`, so a path may contain one; an id may not, which
/// is the right way round — ids come from a document this program only reads,
/// and a device whose id contains `=` would be unaddressable by this flag
/// rather than silently mis-parsed.
fn parse_device_template(raw: &str) -> Result<(String, PathBuf), String> {
    let (id, path) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected ID=FILE, got `{raw}`"))?;
    let id = id.trim();
    if id.is_empty() || path.is_empty() {
        return Err(format!("expected ID=FILE, got `{raw}`"));
    }
    Ok((id.to_owned(), PathBuf::from(path)))
}

/// Parse `--licensed-streams`. See the flag's own help for the accepted forms.
fn parse_streams(raw: &str) -> Result<BTreeSet<u8>, String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return Ok(BTreeSet::new());
    }
    if raw.eq_ignore_ascii_case("all") {
        return Ok(catalog_streams());
    }
    raw.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<u8>()
                .map_err(|_| format!("`{part}` is not a stream index"))
        })
        .collect()
}

// --------------------------------------------------------------------------
// pure: the plan
// --------------------------------------------------------------------------

/// Where a unit is to be bound, against where the document put it.
///
/// Both halves are kept because the pair is the answer an operator needs: the
/// declared address is the one their server config names, and the listening
/// address is the one this process can actually serve. When a placement
/// override is in play the two differ, and that difference is the thing to
/// read off the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Where {
    host: String,
    port: u16,
}

impl std::fmt::Display for Where {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.host, self.port)
    }
}

/// One simulated SMP, fully decided and not yet bound.
///
/// Deliberately **not** `Serialize`, and that is enforced rather than merely
/// intended: [`Password`] has no `Serialize` impl, so a future `#[derive]` here
/// — or a stray `serde_json::to_value(unit)` — does not compile. Rendering goes
/// through [`UnitView`], which is the one place that decides whether a secret
/// is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unit {
    id: String,
    /// The document's configuration identity for this device, carried through
    /// unchanged. Two reports that disagree here were made from two different
    /// documents, whatever the ids say.
    uuid: String,
    declared: Where,
    listening: Where,
    username: String,
    /// The crate's own secret type rather than a `String`: redacted in `Debug`,
    /// zeroized on drop, and read only through `expose_secret` — which is the
    /// audit point, and which this binary reaches for exactly twice (building
    /// the simulator's overrides, and a report the operator asked to unmask).
    password: Password,
    /// The device's `disabled_fields` — the document's claim that this recorder
    /// cannot answer these. Carried into the template's own refusal set rather
    /// than replacing it, because both are claims about the same unit.
    disabled_fields: BTreeSet<String>,
}

/// A planned unit, its template, and the device that pair actually produced.
///
/// The reason this type exists: the licence a unit ends up with is decided by
/// its *template*, not by the plan, so a report built from the plan alone would
/// have to guess at it — and would be wrong for exactly the fleets this program
/// is for, the ones where templates differ per device. Building first and
/// reporting from `state` means the report says what each unit will really do.
struct Built {
    unit: Unit,
    /// Which template this unit came from, for the report.
    template: String,
    state: Arc<DeviceState>,
}

/// Whether a report prints the passwords it knows.
///
/// A two-variant enum rather than a `bool`, so the call sites read as the
/// decision instead of as a flag whose polarity has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Secrets {
    Masked,
    Shown,
}

/// One unit as a report renders it: the plan, minus the secret unless asked.
///
/// `password` is `None` when masked rather than the key being dropped, so the
/// JSON shape does not change with the flag — but it is `null` and not a
/// plausible-looking placeholder, because a consumer that piped `[REDACTED]`
/// into a generated devices file would get a fleet nothing can log in to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct UnitView {
    id: String,
    uuid: String,
    declared: Where,
    listening: Where,
    /// The template this unit was built from, so a fleet of mixed
    /// configurations says which is which.
    template: String,
    username: String,
    password: Option<String>,
    /// Read off the built device rather than off the plan: whatever the layers
    /// resolved to is what this says.
    licensed_streams: BTreeSet<u8>,
    /// Every field this unit will refuse, expanded — the ones named outright
    /// and the ones implied by an unlicensed stream, which is most of them on a
    /// base unit.
    refused_fields: BTreeSet<String>,
    refusal_code: u8,
}

impl UnitView {
    fn of(built: &Built, secrets: Secrets) -> Self {
        let license = built.state.license();
        Self {
            id: built.unit.id.clone(),
            uuid: built.unit.uuid.clone(),
            declared: built.unit.declared.clone(),
            listening: built.unit.listening.clone(),
            template: built.template.clone(),
            username: built.unit.username.clone(),
            password: match secrets {
                Secrets::Masked => None,
                Secrets::Shown => Some(built.unit.password.expose_secret().to_owned()),
            },
            licensed_streams: license.streams().clone(),
            refused_fields: license.refused_fields(),
            refusal_code: license.code(),
        }
    }
}

/// How to place a fleet: the two overrides, or neither.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Placement {
    host: Option<String>,
    port_base: Option<u16>,
}

impl Placement {
    /// Where the `index`-th device of the document goes.
    ///
    /// Ports count from `port_base` in document order, with the one exception
    /// the flag documents: a base of `0` places every unit on `0`, because
    /// port 0 means "kernel, choose" and incrementing from it would ask for
    /// real ports 1, 2, 3 — privileged, and not what anyone meant.
    fn place(&self, declared: &Where, index: usize) -> Where {
        Where {
            host: self.host.clone().unwrap_or_else(|| declared.host.clone()),
            port: match self.port_base {
                None => declared.port,
                Some(0) => 0,
                Some(base) => base.saturating_add(index as u16),
            },
        }
    }
}

/// Turn a resolved document into the fleet it describes.
///
/// Placement, identity and credentials only. What each unit *answers* is its
/// template's business, resolved in [`build`].
fn plan(devices: &[DeviceConfig], placement: &Placement) -> Vec<Unit> {
    devices
        .iter()
        .enumerate()
        .map(|(index, device)| {
            let declared = Where {
                host: device.host.clone(),
                port: device.port,
            };
            Unit {
                id: device.id.clone(),
                uuid: device.uuid.to_string(),
                listening: placement.place(&declared, index),
                declared,
                username: device.username.clone(),
                password: device.password.clone(),
                disabled_fields: device.disabled_fields.clone(),
            }
        })
        .collect()
}

/// The addresses two or more units would both try to bind.
///
/// Worth answering before the first `bind` rather than discovering it halfway
/// through, because the common cause produces a whole fleet of clashes at
/// once: a document where every recorder sits on the standard port, flattened
/// onto one `--host`. Port `0` never collides — the kernel is choosing — so it
/// is excluded rather than reported forty times.
fn collisions(plan: &[Unit]) -> Vec<(Where, Vec<String>)> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for unit in plan {
        if unit.listening.port == 0 {
            continue;
        }
        seen.entry(unit.listening.to_string())
            .or_default()
            .push(unit.id.clone());
    }
    plan.iter()
        .filter(|unit| unit.listening.port != 0)
        .filter_map(|unit| {
            let ids = seen.get(&unit.listening.to_string())?;
            (ids.len() > 1 && ids.first() == Some(&unit.id))
                .then(|| (unit.listening.clone(), ids.clone()))
        })
        .collect()
}

/// Map a unit onto the simulator's own override set — the top layer.
///
/// Each field here is a deliberate decision about what beats what:
///
/// * **Credentials always win.** The server has to be able to log in with what
///   its own devices file says, so a template's credentials are overridden
///   rather than consulted. A template is about what a recorder *answers*.
/// * **A non-empty `disabled_fields` replaces the template's refusal set**,
///   rather than merging with it. Every other override in [`Overrides`]
///   replaces wholesale, for the reason stated there — merging makes a
///   *narrower* value impossible to express — and one rule across the whole
///   layer is worth more than a special case here. The consequence to know:
///   a unit that needs both the document's refusals and its template's must
///   name them in one place, not one each.
/// * **The licence is passed through only when the flag gave one.** `None`
///   leaves whatever the template said — and `DeviceState` falls back to the
///   whole catalog only if the template is silent too. That is what makes a
///   per-device template able to describe a base unit at all.
/// * `unit-name` and `systemname` are seeded from the id so a client querying
///   the fleet can tell the units apart — a building of recorders all called
///   "Main Hall SMP" is a worse simulation than one that answers with the
///   names the document uses. A template that sets them loses, deliberately:
///   the document names the rooms.
fn overrides_for(unit: &Unit, streams: Option<&BTreeSet<u8>>) -> Overrides {
    Overrides {
        username: Some(unit.username.clone()),
        password: Some(unit.password.expose_secret().to_owned()),
        licensed_streams: streams.cloned(),
        unlicensed_fields: (!unit.disabled_fields.is_empty()).then(|| unit.disabled_fields.clone()),
        refusal_code: None,
        fields: [
            ("UNIT_NAME", unit.id.clone()),
            ("SYSTEMNAME", unit.id.clone()),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((canonical_name(name)?, value)))
        .collect(),
    }
}

/// The top-level keys of a YAML document, as written.
///
/// Column zero only, so a key nested under a device (`disabled_fields`) is not
/// mistaken for a document-level one. Crude on purpose: this is consulted only
/// after a real parser has already failed, to work out *which* kind of file it
/// was handed.
fn top_level_keys(text: &str) -> BTreeSet<&str> {
    text.lines()
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with(char::is_whitespace)
                && !line.starts_with('#')
                && !line.starts_with('-')
        })
        .filter_map(|line| line.split_once(':'))
        .map(|(key, _)| key.trim())
        .collect()
}

/// Whether a document that would not parse as a devices file is in fact a
/// *device template* — the other YAML in this crate, and the easy mistake.
///
/// Both files are called `device(s).yaml` and both live here, so the confusion
/// is the repository's fault rather than the reader's: one lists which
/// recorders exist, the other is one recorder's field table. These four keys
/// appear at the top level of a template and nowhere in a devices file at any
/// depth, so finding one is conclusive rather than suggestive.
fn looks_like_a_device_template(text: &str) -> bool {
    const ONLY_IN_A_TEMPLATE: [&str; 4] = [
        "fields",
        "licensed_streams",
        "unlicensed_fields",
        "refusal_code",
    ];
    let keys = top_level_keys(text);
    ONLY_IN_A_TEMPLATE.iter().any(|key| keys.contains(key))
}

/// What is wrong with the device template, summarized.
///
/// `simulated-device` prints its issues one per line and starts anyway, which
/// is right for one device and wrong for a fleet: the template is shared, so a
/// single finding is inherited by every unit and repeating it per unit would
/// bury the report. So this counts them, shows a few, and says what the
/// consequence is.
///
/// A warning and not a failure, for the reason the sibling binary gives: a
/// missing field goes unanswered, and a client seeing a command timeout is
/// more useful to debug against than a device that refused to start.
fn template_warning(label: &str, issues: &[Issue]) -> Option<String> {
    if issues.is_empty() {
        return None;
    }
    const SHOWN: usize = 4;
    let mut out = format!(
        "warning: device template {label} has {} issue(s); \
         every unit in the fleet inherits them\n",
        issues.len()
    );
    for issue in issues.iter().take(SHOWN) {
        out.push_str(&format!("  {issue}\n"));
    }
    if issues.len() > SHOWN {
        out.push_str(&format!("  ... and {} more\n", issues.len() - SHOWN));
    }
    out.push_str(
        "  a missing field is answered with silence, which a client sees as a \
         command timeout\n",
    );
    Some(out)
}

/// The mirror of [`looks_like_a_device_template`]: whether a document handed to
/// `--device-config` is in fact the fleet inventory.
fn looks_like_a_devices_file(text: &str) -> bool {
    const ONLY_IN_AN_INVENTORY: [&str; 4] = ["devices", "device", "groups", "group"];
    let keys = top_level_keys(text);
    ONLY_IN_AN_INVENTORY.iter().any(|key| keys.contains(key))
}

/// Every template text this run will use, already read from disk.
///
/// A loaded value rather than a set of paths, so everything downstream of here
/// is pure: [`build`] resolves the layers and parses, and does no I/O.
struct Templates {
    /// The one every unit uses unless it is named below.
    fleet: (String, String),
    /// Per device, by id.
    per_device: BTreeMap<String, (String, String)>,
}

impl Templates {
    /// The template a unit is built from — the per-device one if it has one.
    fn for_id(&self, id: &str) -> &(String, String) {
        self.per_device.get(id).unwrap_or(&self.fleet)
    }

    /// Each distinct template, by label, so a check runs once per template
    /// rather than once per unit.
    fn distinct(&self) -> BTreeMap<&str, &str> {
        std::iter::once(&self.fleet)
            .chain(self.per_device.values())
            .map(|(label, text)| (label.as_str(), text.as_str()))
            .collect()
    }
}

/// Resolve one unit's layers and parse the device it describes.
///
/// Pure: the template texts arrive loaded, and `from_yaml_str_with` is a
/// function of its two arguments. That is what lets the tests check the
/// precedence rules — which the `--help` text promises and a reader has to
/// trust — without a filesystem or a socket.
fn build(
    unit: &Unit,
    templates: &Templates,
    streams: Option<&BTreeSet<u8>>,
) -> Result<Built, String> {
    let (label, text) = templates.for_id(&unit.id);
    let state = DeviceState::from_yaml_str_with(text, &overrides_for(unit, streams))
        .map_err(|e| format!("{}: cannot build a device from {label}: {e}", unit.id))?;
    Ok(Built {
        unit: unit.clone(),
        template: label.clone(),
        state: Arc::new(state),
    })
}

// --------------------------------------------------------------------------
// pure: the report
// --------------------------------------------------------------------------

/// Everything worth knowing about a running (or planned) fleet.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Report {
    source: String,
    /// The fleet-wide template. A unit built from a per-device one says so in
    /// its own `template` field, which is where a mixed fleet is read.
    device_template: String,
    reply_stream: String,
    bound: bool,
    /// `"masked"` or `"shown"`, so a script can assert it is reading a report
    /// whose `password` fields mean something before it trusts them.
    credentials: String,
    units: Vec<UnitView>,
    groups: Vec<Group>,
}

/// A device group, as the document describes it. Carried because a group is
/// part of what a devices file says and a reader comparing this report against
/// their server config will look for it — nothing here is simulated, since a
/// group is a name the server keeps over devices rather than anything the
/// hardware knows about.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Group {
    id: String,
    members: Vec<String>,
    barrier: String,
    barrier_timeout_secs: u64,
}

/// Spell a barrier policy the way a devices file spells it.
///
/// An explicit match rather than a lower-cased `Debug`, which renders
/// `FailBatch` as `failbatch` — a word that appears in no config file and that
/// a reader comparing this report against their own document would not find.
/// Wildcard-free, so a third policy is a build error here rather than a
/// silently mis-spelled line.
fn barrier_name(barrier: Barrier) -> &'static str {
    match barrier {
        Barrier::FailBatch => "fail_batch",
        Barrier::DispatchReady => "dispatch_ready",
    }
}

fn groups_of(groups: &[GroupConfig]) -> Vec<Group> {
    groups
        .iter()
        .map(|group| Group {
            id: group.id.clone(),
            members: group.device_ids.clone(),
            barrier: barrier_name(group.barrier).to_owned(),
            barrier_timeout_secs: group.barrier_timeout.as_secs(),
        })
        .collect()
}

fn report(
    source: &Path,
    template: &str,
    reply: ReplyStream,
    bound: bool,
    units: &[Built],
    groups: &[GroupConfig],
    secrets: Secrets,
) -> Report {
    Report {
        source: source.display().to_string(),
        device_template: template.to_owned(),
        reply_stream: format!("{reply:?}").to_lowercase(),
        bound,
        credentials: match secrets {
            Secrets::Masked => "masked".to_owned(),
            Secrets::Shown => "shown".to_owned(),
        },
        units: units.iter().map(|u| UnitView::of(u, secrets)).collect(),
        groups: groups_of(groups),
    }
}

/// Render what a unit refuses, capped.
///
/// A base unit refuses fourteen fields it never names — the index is read out
/// of each field's own name — so the full list is both long and derivable.
/// The count is the number worth reading; the first few names say what kind of
/// thing it is.
fn refusals_text(fields: &BTreeSet<String>, code: u8) -> String {
    const SHOWN: usize = 4;
    if fields.is_empty() {
        return "none".to_owned();
    }
    let names = fields
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    let more = fields.len().saturating_sub(SHOWN);
    let tail = if more > 0 {
        format!(", +{more} more")
    } else {
        String::new()
    };
    format!("{} with E{code:02} — {names}{tail}", fields.len())
}

/// Render a list of stream indices, or the two words that say there are none.
fn streams_text(streams: &BTreeSet<u8>) -> String {
    if streams.is_empty() {
        return "none".to_owned();
    }
    streams
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The human form: a summary, then one block per unit, then the groups.
fn render_text(report: &Report) -> String {
    let mut out = String::new();
    let verb = if report.bound {
        "listening"
    } else {
        "would listen"
    };
    out.push_str(&format!(
        "simulated fleet from {}\n  {} unit(s), {} group(s), {verb}; \
         device template {}, replies on {}\n",
        report.source,
        report.units.len(),
        report.groups.len(),
        report.device_template,
        report.reply_stream,
    ));
    // Once, in the header, rather than beside every unit: the hint is useful
    // the first time it is read and noise on the fortieth row.
    if report.units.iter().any(|unit| unit.password.is_none()) {
        out.push_str("  passwords masked — pass --show-credentials to print them\n");
    }

    for unit in &report.units {
        out.push_str(&format!("\n{}\n", unit.id));
        let moved = if unit.declared == unit.listening {
            String::new()
        } else {
            format!("   (declared {})", unit.declared)
        };
        let rows = [
            ("uuid", unit.uuid.clone()),
            ("listening", format!("{}{moved}", unit.listening)),
            // Only when it differs from the fleet-wide one: on a uniform fleet
            // this would be the same line under every unit.
            ("template", unit.template.clone()),
            (
                "credentials",
                format!(
                    "{} / {}",
                    unit.username,
                    // `[REDACTED]` as the crate's own `Password` renders
                    // itself, so one spelling covers logs and reports alike.
                    unit.password.as_deref().unwrap_or("[REDACTED]")
                ),
            ),
            ("licensed streams", streams_text(&unit.licensed_streams)),
            (
                "refused fields",
                refusals_text(&unit.refused_fields, unit.refusal_code),
            ),
        ];
        for (label, value) in rows {
            if label == "template" && value == report.device_template {
                continue;
            }
            out.push_str(&format!("  {label:<18}{value}\n"));
        }
    }

    if !report.groups.is_empty() {
        out.push_str("\ngroups\n");
        for group in &report.groups {
            out.push_str(&format!(
                "  {:<18}{}   barrier={} timeout={}s\n",
                group.id,
                group.members.join(", "),
                group.barrier,
                group.barrier_timeout_secs,
            ));
        }
    }
    out
}

// --------------------------------------------------------------------------
// the edge
// --------------------------------------------------------------------------

/// Bind one already-built unit, or say what went wrong in terms of the flag
/// that fixes it.
async fn bind_unit(built: &Built, reply: ReplyStream) -> Result<SimulatedDevice, String> {
    let unit = &built.unit;
    let addr = (unit.listening.host.as_str(), unit.listening.port);
    // Resolved here rather than left to `bind`, so an unresolvable host is
    // reported as itself instead of as a bind failure on an address we never
    // formed.
    addr.to_socket_addrs()
        .map_err(|e| format!("{}: cannot resolve {}: {e}", unit.id, unit.listening))?;

    bind(addr, Arc::clone(&built.state), reply)
        .await
        .map_err(|e| {
            format!(
                "{}: cannot bind {}: {e}\n  \
             a devices file names the real recorders' addresses — \
             pass --host 127.0.0.1 (and --port-base) to serve this fleet locally",
                unit.id, unit.listening
            )
        })
}

fn read_document(path: &Path) -> Result<Resolved, String> {
    config::load(path).map_err(|e| {
        let detail = match e {
            ConfigError::UnsupportedFormat(ext) => format!(
                "{}: no reader for `.{ext}` — this build reads .yaml, .yml and .toml",
                path.display()
            ),
            other => format!("{}: {other}", path.display()),
        };
        // Reading the file a second time, only to explain a failure. Cheap,
        // and the alternative is an error that lists the five keys a devices
        // file accepts and leaves the reader to work out that they handed over
        // a file of a different kind entirely.
        let misfiled = std::fs::read_to_string(path)
            .map(|text| looks_like_a_device_template(&text))
            .unwrap_or(false);
        if !misfiled {
            return detail;
        }
        format!(
            "{detail}\n\n\
             this looks like a *device template*, not a devices file — the two \
             schemas share a name and this crate ships both.\n  \
             a devices file lists the recorders that exist: `devices:`, each with \
             an `id` and a `host`.\n  \
             a device template is one SMP's field table: `fields:`, plus its \
             licence block.\n\n  \
             to serve a fleet *using* this template:\n    \
             simulated-fleet <devices.yaml> --device-config {}",
            path.display()
        )
    })
}

fn emit(report: &Report, as_json: bool) -> Result<(), String> {
    if as_json {
        let text = serde_json::to_string_pretty(report)
            .map_err(|e| format!("rendering the report: {e}"))?;
        println!("{text}");
    } else {
        print!("{}", render_text(report));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    // Logs to stderr, deliberately: with `--json` this program's stdout is a
    // document, and a tracing line in the middle of it would hand `jq`
    // something that is not JSON. The sibling `simulated-device` has no such
    // obligation, which is why it formats onto stdout.
    let filter = EnvFilter::try_new(&args.log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    match run(&args).await {
        Ok(devices) => {
            if devices.is_empty() {
                return ExitCode::SUCCESS;
            }
            tracing::info!(units = devices.len(), "simulated fleet listening");
            // The devices stay alive because this vector holds them; dropping
            // it aborts every accept loop, which is what shutdown is.
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("stopping");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Everything between the arguments and a listening fleet.
///
/// Returns the bound devices so `main` owns their lifetime — and an empty
/// vector for `--dry-run`, which is the one path that reports without binding.
async fn run(args: &Args) -> Result<Vec<SimulatedDevice>, String> {
    let resolved = read_document(&args.config)?;

    let templates = read_templates(args, &resolved)?;
    let template_label = templates.fleet.0.clone();

    let placement = Placement {
        host: args.host.clone(),
        port_base: args.port_base,
    };
    let units = plan(&resolved.devices, &placement);

    let clashes = collisions(&units);
    if !clashes.is_empty() {
        let detail = clashes
            .iter()
            .map(|(addr, ids)| format!("  {addr}  <-  {}", ids.join(", ")))
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!(
            "{} address(es) would be bound more than once:\n{detail}\n  \
             pass --port-base to lay the fleet out on consecutive ports, \
             or --port-base 0 to let the kernel choose",
            clashes.len()
        ));
    }

    let reply = if args.stdout {
        ReplyStream::Stdout
    } else {
        ReplyStream::Stderr
    };

    // Once per distinct template rather than once per unit: a template is
    // shared, so N units would repeat one finding N times. Before the report,
    // so a warning is not buried under forty lines of healthy output.
    for (label, text) in templates.distinct() {
        let probe = DeviceState::from_yaml_str_with(text, &Overrides::default())
            .map_err(|e| misfiled_template(label, text, &e.to_string()))?;
        if let Some(warning) = template_warning(label, &probe.check()) {
            eprint!("{warning}");
        }
    }

    let built = units
        .iter()
        .map(|unit| build(unit, &templates, args.licensed_streams.as_ref()))
        .collect::<Result<Vec<_>, _>>()?;

    let secrets = if args.show_credentials {
        Secrets::Shown
    } else {
        Secrets::Masked
    };

    if args.dry_run {
        let planned = report(
            &args.config,
            &template_label,
            reply,
            false,
            &built,
            &resolved.groups,
            secrets,
        );
        emit(&planned, args.json)?;
        return Ok(Vec::new());
    }

    // Bound before reporting, because `--port-base 0` means the ports worth
    // reporting are the ones the kernel just handed out.
    let mut devices = Vec::with_capacity(built.len());
    let mut bound = Vec::with_capacity(built.len());
    for one in built {
        let device = bind_unit(&one, reply).await?;
        bound.push(Built {
            unit: Unit {
                listening: Where {
                    host: one.unit.listening.host.clone(),
                    port: device.port(),
                },
                ..one.unit
            },
            ..one
        });
        devices.push(device);
    }

    emit(
        &report(
            &args.config,
            &template_label,
            reply,
            true,
            &bound,
            &resolved.groups,
            secrets,
        ),
        args.json,
    )?;
    Ok(devices)
}

/// Read every template this run needs, failing on the first one that cannot be
/// read — and refusing a `--device-config-for` that names no device in the
/// document, which is otherwise a flag that silently does nothing.
fn read_templates(args: &Args, resolved: &Resolved) -> Result<Templates, String> {
    let read = |path: &PathBuf| {
        std::fs::read_to_string(path)
            .map(|text| (path.display().to_string(), text))
            .map_err(|e| format!("cannot read {}: {e}", path.display()))
    };

    let fleet = match &args.device_config {
        Some(path) => read(path)?,
        None => ("<built-in>".to_owned(), BUILT_IN_DEVICE.to_owned()),
    };

    let mut per_device = BTreeMap::new();
    for (id, path) in &args.device_config_for {
        if !resolved.devices.iter().any(|device| &device.id == id) {
            let known = resolved
                .devices
                .iter()
                .map(|device| device.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "--device-config-for names `{id}`, which is not a device in {}\n                   devices in that file: {known}",
                args.config.display()
            ));
        }
        per_device.insert(id.clone(), read(path)?);
    }

    Ok(Templates { fleet, per_device })
}

/// Dress up a template parse failure when the file is in fact the inventory.
fn misfiled_template(label: &str, text: &str, error: &str) -> String {
    let detail = format!("cannot read device template {label}: {error}");
    if !looks_like_a_devices_file(text) {
        return detail;
    }
    format!(
        "{detail}\n\n\
         this looks like a devices file, not a device template — see the note \
         on the two schemas in --help.\n  \
         a devices file belongs in the positional argument:\n    \
         simulated-fleet {label}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_ON_ONE_PORT: &str = "\
defaults:
  username: admin
  password: extron
  port: 22023
devices:
  - id: hall-a
    host: 10.0.150.1
  - id: hall-b
    host: 10.0.150.2
";

    fn resolve(text: &str) -> Resolved {
        config::from_yaml_str(text).expect("the fixture parses")
    }

    fn units(text: &str, placement: Placement) -> Vec<Unit> {
        plan(&resolve(text).devices, &placement)
    }

    /// A template set with nothing per-device: the built-in one, as a bare run
    /// would use it.
    fn built_in() -> Templates {
        Templates {
            fleet: ("<built-in>".to_owned(), BUILT_IN_DEVICE.to_owned()),
            per_device: BTreeMap::new(),
        }
    }

    fn build_all(text: &str, templates: &Templates, streams: Option<&BTreeSet<u8>>) -> Vec<Built> {
        units(text, Placement::default())
            .iter()
            .map(|unit| build(unit, templates, streams).expect("builds"))
            .collect()
    }

    /// Build a planned fleet against the built-in template, for the report
    /// tests — which are about rendering, not about layering.
    fn as_built(planned: &[Unit]) -> Vec<Built> {
        let templates = built_in();
        planned
            .iter()
            .map(|unit| build(unit, &templates, None).expect("builds"))
            .collect()
    }

    #[test]
    fn with_no_overrides_a_unit_is_bound_where_the_document_put_it() {
        let planned = units(TWO_ON_ONE_PORT, Placement::default());
        assert_eq!(planned[0].listening.to_string(), "10.0.150.1:22023");
        assert_eq!(planned[0].declared, planned[0].listening);
    }

    #[test]
    fn the_host_override_moves_every_unit_and_leaves_the_declared_address_alone() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: None,
            },
        );
        assert!(planned.iter().all(|u| u.listening.host == "127.0.0.1"));
        assert_eq!(planned[1].declared.host, "10.0.150.2");
    }

    #[test]
    fn ports_count_up_from_the_base_in_document_order() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: None,
                port_base: Some(2301),
            },
        );
        assert_eq!(
            planned.iter().map(|u| u.listening.port).collect::<Vec<_>>(),
            vec![2301, 2302]
        );
    }

    /// Port 0 means "kernel, choose", so a base of 0 cannot increment: the
    /// alternative would ask for privileged ports 1 and 2.
    #[test]
    fn a_base_of_zero_places_every_unit_on_zero() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: None,
                port_base: Some(0),
            },
        );
        assert!(planned.iter().all(|u| u.listening.port == 0));
    }

    /// The failure this program is most likely to hit: a real fleet, where
    /// every recorder uses the standard port, flattened onto one host.
    #[test]
    fn flattening_a_fleet_onto_one_host_is_reported_as_a_collision() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: None,
            },
        );
        let clashes = collisions(&planned);
        assert_eq!(clashes.len(), 1, "one address, not one per unit");
        assert_eq!(clashes[0].0.to_string(), "127.0.0.1:22023");
        assert_eq!(clashes[0].1, vec!["hall-a", "hall-b"]);
    }

    #[test]
    fn consecutive_ports_collide_with_nothing() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: Some(2301),
            },
        );
        assert!(collisions(&planned).is_empty());
    }

    /// Port 0 is excluded rather than reported once per unit: the kernel is
    /// handing out distinct ports, so there is no clash to warn about.
    #[test]
    fn kernel_chosen_ports_are_never_collisions() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: Some(0),
            },
        );
        assert!(collisions(&planned).is_empty());
    }

    /// The derivation worth having: the document's claim about a recorder
    /// becomes the simulated recorder's behaviour.
    #[test]
    fn disabled_fields_become_the_simulated_units_refusals() {
        let text = "\
defaults:
  username: admin
  password: extron
devices:
  - id: hall-a
    host: 127.0.0.1
    disabled_fields: [snmp_state]
";
        let planned = units(text, Placement::default());
        assert_eq!(
            planned[0]
                .disabled_fields
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["SNMP_STATE".to_string()],
            "canonicalised by the document loader, not by this binary"
        );
        // ...and the device actually refuses it.
        let built = build(&planned[0], &built_in(), None).expect("builds");
        assert!(built.state.license().refuses("SNMP_STATE"));
    }

    /// A unit answers with its own name, so a client can tell a building of
    /// otherwise identical recorders apart.
    #[test]
    fn a_unit_is_seeded_with_its_own_id() {
        let planned = units(TWO_ON_ONE_PORT, Placement::default());
        let fields = overrides_for(&planned[0], None).fields;
        assert_eq!(fields.get("UNIT_NAME"), Some(&"hall-a".to_string()));
        assert_eq!(fields.get("SYSTEMNAME"), Some(&"hall-a".to_string()));
    }

    /// Every unit built by the plan is a device the simulator accepts — the
    /// check that `overrides_for` cannot drift out of step with the template.
    #[test]
    fn every_planned_unit_builds_a_device_with_no_config_issues() {
        for unit in units(TWO_ON_ONE_PORT, Placement::default()) {
            let state =
                DeviceState::from_yaml_str_with(BUILT_IN_DEVICE, &overrides_for(&unit, None))
                    .expect("the built-in template plus these overrides is a device");
            assert_eq!(state.check(), Vec::new(), "{} has config issues", unit.id);
            assert_eq!(state.credentials().username, unit.username);
        }
    }

    #[test]
    fn the_text_report_names_both_addresses_only_when_they_differ() {
        let moved = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: Some(2301),
            },
        );
        let text = render_text(&report(
            Path::new("devices.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &as_built(&moved),
            &[],
            Secrets::Masked,
        ));
        assert!(text.contains("127.0.0.1:2301   (declared 10.0.150.1:22023)"));

        let still = units(TWO_ON_ONE_PORT, Placement::default());
        let text = render_text(&report(
            Path::new("devices.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &as_built(&still),
            &[],
            Secrets::Masked,
        ));
        assert!(
            !text.contains("(declared"),
            "nothing moved, so nothing to say"
        );
    }

    #[test]
    fn the_json_report_round_trips_the_fields_a_reader_needs() {
        let planned = units(
            TWO_ON_ONE_PORT,
            Placement {
                host: Some("127.0.0.1".into()),
                port_base: Some(2301),
            },
        );
        let value = serde_json::to_value(report(
            Path::new("devices.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            false,
            &as_built(&planned),
            &resolve(TWO_ON_ONE_PORT).groups,
            Secrets::Shown,
        ))
        .expect("the report serializes");
        assert_eq!(value["bound"], false);
        assert_eq!(value["reply_stream"], "stderr");
        assert_eq!(value["units"][0]["id"], "hall-a");
        assert_eq!(value["units"][0]["listening"]["port"], 2301);
        assert_eq!(value["units"][0]["declared"]["port"], 22023);
    }

    /// The password lives in the document, so every default-shaped output has
    /// to be checked against the literal secret rather than against a shape.
    const SECRET: &str = "extron";

    fn rendered(secrets: Secrets) -> String {
        let planned = units(TWO_ON_ONE_PORT, Placement::default());
        render_text(&report(
            Path::new("devices.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &as_built(&planned),
            &[],
            secrets,
        ))
    }

    #[test]
    fn the_text_report_masks_passwords_by_default() {
        let text = rendered(Secrets::Masked);
        assert!(!text.contains(SECRET), "the secret reached the report");
        assert!(text.contains("admin / [REDACTED]"));
        assert!(
            text.contains("--show-credentials"),
            "a masked report says how to unmask itself"
        );
    }

    #[test]
    fn the_text_report_prints_passwords_when_asked() {
        let text = rendered(Secrets::Shown);
        assert!(text.contains(&format!("admin / {SECRET}")));
        assert!(
            !text.contains("--show-credentials"),
            "nothing left to hint at"
        );
    }

    /// `null`, not the key being dropped and not a placeholder: the shape does
    /// not change with the flag, and nothing that looks like a password can be
    /// piped into a generated devices file.
    #[test]
    fn the_json_report_nulls_the_password_unless_asked() {
        let planned = units(TWO_ON_ONE_PORT, Placement::default());
        let masked = serde_json::to_value(report(
            Path::new("d.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &as_built(&planned),
            &[],
            Secrets::Masked,
        ))
        .expect("serializes");
        assert_eq!(masked["credentials"], "masked");
        assert!(masked["units"][0].get("password").is_some(), "key stays");
        assert!(masked["units"][0]["password"].is_null());
        assert_eq!(masked["units"][0]["username"], "admin");
        assert!(!masked.to_string().contains(SECRET));

        let shown = serde_json::to_value(report(
            Path::new("d.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &as_built(&planned),
            &[],
            Secrets::Shown,
        ))
        .expect("serializes");
        assert_eq!(shown["credentials"], "shown");
        assert_eq!(shown["units"][0]["password"], SECRET);
    }

    /// The other way a secret escapes: a `Debug` line in a log or an error.
    /// `Unit` holds the crate's `Password`, so this is inherited rather than
    /// re-implemented — and the test is here to catch a future field that is
    /// not.
    #[test]
    fn a_units_debug_output_carries_no_secret() {
        for unit in units(TWO_ON_ONE_PORT, Placement::default()) {
            let rendered = format!("{unit:?}");
            assert!(!rendered.contains(SECRET), "leaked via Debug: {rendered}");
            assert!(rendered.contains("REDACTED"));
        }
    }

    /// Masking is a rendering decision, not a planning one: the plan still has
    /// to hand the simulator a credential it will accept.
    #[test]
    fn masking_the_report_does_not_change_what_the_unit_accepts() {
        let unit = &units(TWO_ON_ONE_PORT, Placement::default())[0];
        let state = DeviceState::from_yaml_str_with(BUILT_IN_DEVICE, &overrides_for(unit, None))
            .expect("builds");
        assert_eq!(state.credentials().password, SECRET);
    }

    /// The mistake this crate invites: two schemas, one name, both shipped
    /// here. Each file must be recognised as itself and not as the other.
    #[test]
    fn the_two_schemas_are_told_apart() {
        let template = include_str!("../../tests/fixtures/device.yaml");
        assert!(looks_like_a_device_template(template));
        assert!(!looks_like_a_devices_file(template));

        assert!(looks_like_a_devices_file(TWO_ON_ONE_PORT));
        assert!(!looks_like_a_device_template(TWO_ON_ONE_PORT));
    }

    /// Column zero only. `disabled_fields` nested under a device must not be
    /// read as a document-level key, or a devices file would be mistaken for a
    /// template by the one marker they nearly share.
    #[test]
    fn only_column_zero_counts_as_a_top_level_key() {
        let text = "\
# fields: a comment, not a key
devices:
  - id: hall-a
    host: 127.0.0.1
    disabled_fields: [snmp_state]
    fields: not-a-document-key
";
        assert_eq!(top_level_keys(text), BTreeSet::from(["devices"]));
        assert!(!looks_like_a_device_template(text));
    }

    #[test]
    fn a_healthy_template_warns_about_nothing() {
        let state = DeviceState::from_yaml_str(BUILT_IN_DEVICE).expect("the fixture loads");
        assert_eq!(template_warning("<built-in>", &state.check()), None);
    }

    /// Summarized and capped: the template is shared, so a long list repeated
    /// per unit would bury the report it precedes.
    #[test]
    fn a_partial_template_is_summarized_rather_than_listed() {
        let state = DeviceState::from_yaml_str(
            "username: admin\npassword: extron\nfields:\n  FIRMWARE: \"3.06\"\n",
        )
        .expect("a partial template still loads");
        let issues = state.check();
        assert!(issues.len() > 10, "the fixture is deliberately sparse");

        let warning = template_warning("partial.yaml", &issues).expect("warns");
        assert!(warning.contains(&format!("{} issue(s)", issues.len())));
        assert!(warning.contains("... and "), "the list is capped");
        assert_eq!(
            // The backtick matters: the closing line also says "missing
            // field", and counting that too is what made this assertion wrong
            // the first time.
            warning
                .lines()
                .filter(|l| l.trim_start().starts_with("missing field `"))
                .count(),
            4,
            "four shown, however many there are"
        );
        assert!(warning.contains("command timeout"), "says what it costs");
    }

    /// A base unit, described by a template rather than by a flag — the point
    /// of per-device templates.
    const BASE_UNIT: &str = "\
username: ignored
password: ignored
licensed_streams: [1]
fields:
  STREAM_1_NAME: Archive Channel A
";

    fn mixed() -> Templates {
        Templates {
            fleet: ("<built-in>".to_owned(), BUILT_IN_DEVICE.to_owned()),
            per_device: BTreeMap::from([(
                "hall-b".to_owned(),
                ("base-unit.yaml".to_owned(), BASE_UNIT.to_owned()),
            )]),
        }
    }

    const TWO_HALLS: &str = "\
defaults:
  username: admin
  password: extron
devices:
  - id: hall-a
    host: 127.0.0.1
    port: 2301
  - id: hall-b
    host: 127.0.0.1
    port: 2302
";

    #[test]
    fn a_per_device_template_applies_only_to_the_device_it_names() {
        let templates = mixed();
        assert_eq!(templates.for_id("hall-b").0, "base-unit.yaml");
        assert_eq!(templates.for_id("hall-a").0, "<built-in>");
        assert_eq!(templates.for_id("not-in-the-fleet").0, "<built-in>");
    }

    /// The layering the `--help` text promises: with no flag, each unit's
    /// licence is its own template's.
    #[test]
    fn templates_decide_the_licence_when_no_flag_overrides_them() {
        let built = build_all(TWO_HALLS, &mixed(), None);
        let licence = |i: usize| built[i].state.license().streams().clone();
        assert_eq!(
            licence(0),
            catalog_streams(),
            "fully loaded from the fixture"
        );
        assert_eq!(licence(1), BTreeSet::from([1]), "a base unit from its own");
        assert!(built[1].state.license().refuses("STREAM_2_NAME"));
        assert!(!built[0].state.license().refuses("STREAM_2_NAME"));
    }

    /// ...and the flag, when given, is the top layer and flattens the fleet.
    #[test]
    fn the_licence_flag_beats_every_template() {
        let only_two = BTreeSet::from([1, 2]);
        let built = build_all(TWO_HALLS, &mixed(), Some(&only_two));
        for one in &built {
            assert_eq!(
                one.state.license().streams(),
                &only_two,
                "{} kept its template's licence",
                one.unit.id
            );
        }
    }

    /// Credentials are the document's either way: the server has to be able to
    /// log in with what its own devices file says.
    #[test]
    fn the_documents_credentials_beat_a_templates() {
        let built = build_all(TWO_HALLS, &mixed(), None);
        for one in &built {
            let credentials = one.state.credentials();
            assert_eq!(credentials.username, "admin");
            assert_eq!(
                credentials.password, "extron",
                "the template said `ignored`"
            );
        }
    }

    /// The report reads each unit's licence off the device that was built, so a
    /// mixed fleet is legible — and names the template only where it differs
    /// from the fleet-wide one.
    #[test]
    fn the_report_shows_a_mixed_fleet_as_mixed() {
        let built = build_all(TWO_HALLS, &mixed(), None);
        let text = render_text(&report(
            Path::new("devices.yaml"),
            "<built-in>",
            ReplyStream::Stderr,
            true,
            &built,
            &[],
            Secrets::Masked,
        ));
        assert!(text.contains("licensed streams  1, 2, 3"), "hall-a");
        assert!(text.contains("licensed streams  1\n"), "hall-b");
        assert_eq!(
            text.matches("template          base-unit.yaml").count(),
            1,
            "named for the unit that differs"
        );
        assert!(
            !text.contains("template          <built-in>"),
            "and not repeated under every unit that does not"
        );
        // A base unit refuses fourteen fields it never names.
        assert!(text.contains("refused fields    14 with E13 — "));
        assert!(text.contains("+10 more"), "the list is capped");
    }

    #[test]
    fn a_per_device_template_argument_is_split_at_the_first_equals() {
        assert_eq!(
            parse_device_template("hall-b=/tmp/a=b.yaml").unwrap(),
            ("hall-b".to_owned(), PathBuf::from("/tmp/a=b.yaml")),
            "a path may contain `=`; an id may not"
        );
        assert_eq!(
            parse_device_template(" hall-b = x.yaml").unwrap().0,
            "hall-b",
            "whitespace around the id is trimmed"
        );
        assert!(parse_device_template("no-equals-sign").is_err());
        assert!(parse_device_template("=x.yaml").is_err());
        assert!(parse_device_template("hall-b=").is_err());
    }

    #[test]
    fn each_distinct_template_is_checked_once_however_many_units_share_it() {
        let templates = mixed();
        let distinct = templates.distinct();
        assert_eq!(distinct.len(), 2, "two templates, not two units");
        assert!(distinct.contains_key("<built-in>"));
        assert!(distinct.contains_key("base-unit.yaml"));
    }

    #[test]
    fn refusals_are_counted_with_their_code_and_capped() {
        assert_eq!(refusals_text(&BTreeSet::new(), 13), "none");
        let two = BTreeSet::from(["A".to_owned(), "B".to_owned()]);
        assert_eq!(refusals_text(&two, 13), "2 with E13 — A, B");
        let six = (0..6).map(|i| format!("F{i}")).collect();
        assert_eq!(
            refusals_text(&six, 22),
            "6 with E22 — F0, F1, F2, F3, +2 more"
        );
    }

    #[test]
    fn barrier_policies_are_spelled_as_a_devices_file_spells_them() {
        assert_eq!(barrier_name(Barrier::FailBatch), "fail_batch");
        assert_eq!(barrier_name(Barrier::DispatchReady), "dispatch_ready");
    }

    #[test]
    fn stream_lists_are_spelled_for_a_reader() {
        assert_eq!(streams_text(&BTreeSet::new()), "none");
        assert_eq!(streams_text(&BTreeSet::from([1, 2, 3])), "1, 2, 3");
    }

    #[test]
    fn licensed_streams_accepts_the_forms_the_flag_documents() {
        assert_eq!(parse_streams("none").unwrap(), BTreeSet::new());
        assert_eq!(parse_streams("").unwrap(), BTreeSet::new());
        assert_eq!(parse_streams("1").unwrap(), BTreeSet::from([1]));
        assert_eq!(parse_streams(" 1 , 2 ").unwrap(), BTreeSet::from([1, 2]));
        assert_eq!(parse_streams("ALL").unwrap(), catalog_streams());
        assert!(parse_streams("two").is_err());
    }
}
