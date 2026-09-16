//! The inventory routes — what devices and groups this server was configured
//! with.
//!
//! Every path below is relative to the `/v1/inventory` scope
//! [`crate::startup`] mounts them under.
//!
//! ```text
//! GET    /devices           every configured device
//! GET    /devices/{id}      one device, with the latest value of each field
//! POST   /devices           add a device
//! PUT    /devices/{id}      replace one
//! DELETE /devices/{id}      remove one
//! GET    /groups            every configured group
//! GET    /groups/{id}       one group and the devices it addresses
//! POST   /groups            add a group
//! PUT    /groups/{id}       replace one
//! DELETE /groups/{id}       remove one
//!
//! GET    /config/export     the running configuration as a devices file
//! POST   /config/reset      discard runtime changes, adopt the file
//! ```
//!
//! # Why the last two are `/config` and not `/devices`
//!
//! Because what they operate on is the *document*, not the device list. A
//! devices file is `[defaults]`, `[[device]]` and `[[group]]` together, and both
//! routes take all three: an export that left groups out would not load back as
//! the same fleet, and a reset that restored only devices would leave groups
//! describing a membership the file disagrees with. Naming them under
//! `/devices` said otherwise.
//!
//! # The six that change things
//!
//! The fleet moves while the server runs, and these are how. They go through
//! [`LiveInventory`], which is the composition root behind a port — the same
//! arrangement the config scope uses, and for the same reason: applying a change
//! means touching the registry, the poll loops, the relay's per-device tasks and
//! the keepalive, and none of that is storage.
//!
//! There is no `PATCH`. A device is immutable: changing any key mints a
//! different device with a different identity and a new connection, so a body
//! stating only a delta would describe something this system cannot represent.
//! `PUT` therefore replaces wholesale, and an omitted key takes the server's
//! default rather than the previous device's value — which is what makes the
//! same request applied twice leave the same device. The group routes keep the
//! same contract, where it bites hardest on membership: `devices` replaces the
//! member list rather than adding to it, so removing one member means sending
//! the list without it.
//!
//! # What survives a restart
//!
//! Stated once here, because it is the same answer for all six and it is not
//! the obvious one.
//!
//! **The devices file is never written.** No route in this scope touches it; it
//! stays the artifact a deployment reviews and version-controls, which is the
//! contract `PATCH /v1/config` keeps for settings.
//!
//! Whether a change *outlives the process* is a separate question, answered by
//! the deployment's `inventory.runtime_config_path`:
//!
//! * **Unset**, the default: it does not. The next startup reads the devices
//!   file and the fleet is whatever that says.
//! * **Set**: it does. The whole document is written to that path after every
//!   change here, and the next startup loads *that* instead of the devices file
//!   — which also means an edit made to the devices file meanwhile has no effect
//!   until [`reset_config`] adopts it. The server says so at `warn!` on every
//!   startup that loads from state.
//!
//! Either way [`export_config`] is how a running fleet becomes a devices file
//! again: a state file is the server's to write, not a thing anyone reads.
//!
//! # Why these are not answered from the store
//!
//! Every other read route answers from what the sync side *wrote*, which is why
//! an unknown device there yields an empty list rather than a `404` — the store
//! cannot tell "no such device" from "this one has not answered yet". These
//! four answer from the [`DeviceCatalog`] instead, which is the configured set,
//! so here a `404` is a real claim: the id is not in the devices file.
//!
//! That distinction is what makes the index useful. `GET /v1/inventory/devices` returning
//! `[]` from the store would mean "nothing has been polled yet"; returning `[]`
//! from the catalog means "no devices are configured", and those call for very
//! different actions.
//!
//! # What is live here, and only here
//!
//! Two of a device's fields are observations rather than configuration, and the
//! catalog can fill neither: [`status`] is always `unknown` and
//! [`auto_disabled_fields`] always empty, because the catalog is a snapshot
//! taken before the process connected to anything. Both device routes overlay
//! the real values from [`DeviceStatus`], which reads the running registry
//! without dialing. That makes these the only two routes whose answer can change
//! with nothing having been written.
//!
//! # Where a caller learns a field is switched off
//!
//! These two routes, and they are the only place that answers it. A vetoed field
//! is simply absent from `GET /v1/reads/devices/{id}/fields`, and the reads
//! routes cannot say why — the store holds what was written and no catalog of
//! what *could* be (see [`crate::handlers::reads`]). Nor can the global field
//! catalog at `GET /v1/reads`, which is fleet-wide by construction and a veto is
//! per device.
//!
//! So the two halves are reported side by side here: [`disabled_fields`] is what
//! the operator declared, [`auto_disabled_fields`] is what the server inferred,
//! and between them they account for every field a device is not being asked
//! for. A dashboard rendering "—" for a missing value gets to say *why* it is
//! missing from one request it already makes.
//!
//! [`status`]: sismatic_api_types::DeviceSummary::status
//! [`disabled_fields`]: sismatic_api_types::DeviceSummary::disabled_fields
//! [`auto_disabled_fields`]: sismatic_api_types::DeviceSummary::auto_disabled_fields
//!
//! # The one route that reads both
//!
//! [`read_device`] joins the two sides: the catalog says the device exists and
//! how it is addressed, the store says what it last reported. A device that is
//! configured but has never answered is a `200` with an empty `latest`, which
//! is the honest answer and the one a dashboard needs to render a row at all.

use actix_web::{HttpResponse, web};
use sismatic_api_types::{
    ApiError, DeviceDetail, DeviceList, DeviceSummary, DeviceWrite, ExportQuery, GroupList,
    GroupSummary, GroupWrite, Removed,
};
use sismatic_store::ReadStore;
use sismatic_store::catalog::DeviceCatalog;
use sismatic_store::status::DeviceStatus;

use crate::handlers::error::ApiFailure;
use crate::handlers::target::{INVENTORY, reject_group_bare};
use crate::inventory::LiveInventory;

/// `GET /v1/inventory/devices` — every configured device, ordered by id.
#[utoipa::path(
    get,
    path = "/devices",
    context_path = "/v1/inventory",
    tag = "inventory",
    responses(
        (status = 200, description = "Every device in the devices file, ordered by \
             id, each with its live connection state. Empty means none are \
             configured — unlike an empty reads list, which means none have \
             answered.", body = DeviceList),
    ),
)]
pub async fn list_devices(
    catalog: web::Data<dyn DeviceCatalog>,
    status: web::Data<dyn DeviceStatus>,
) -> web::Json<DeviceList> {
    // The catalog is a snapshot of configuration taken before the process
    // connected to anything, so the two live fields it carries are always empty:
    // `status` is `Unknown` and nothing has been inferred yet. Both are overlaid
    // here, which is the only place the configured and the observed halves are
    // in scope. One `all()` for the fleet rather than a lookup per device: the
    // adapter walks its registry once, and a per-device call would walk it once
    // each.
    let live = status.all().await;
    let devices = catalog
        .devices()
        .await
        .into_iter()
        .map(|mut device| {
            // A device in the catalog and absent from the registry keeps the
            // catalog's values — the two are built from one config, so it
            // cannot happen, and inventing `Cold` for it would be a claim
            // rather than an observation.
            if let Some(observed) = live.get(&device.id) {
                device.status = observed.connection;
                device
                    .auto_disabled_fields
                    .clone_from(&observed.auto_disabled);
            }
            device
        })
        .collect();
    web::Json(DeviceList { devices })
}

/// `GET /v1/inventory/devices/{id}` — one device and the latest value of every
/// field it has reported.
#[utoipa::path(
    get,
    path = "/devices/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Device id, as written in the devices file.")),
    responses(
        (status = 200, description = "The device, with one read per field it has \
             answered. `latest` is empty for a device that is configured but has \
             never been reached.", body = DeviceDetail),
        (status = 404, description = "No device has this id, or the id names a device \
             group — in which case the body carries `/v1/inventory/groups/{id}`. Unlike the \
             reads routes' 404, this one is a claim about configuration.",
         body = ApiError),
        (status = 500, description = "The storage backend failed.", body = ApiError),
    ),
)]
pub async fn read_device(
    catalog: web::Data<dyn DeviceCatalog>,
    status: web::Data<dyn DeviceStatus>,
    store: web::Data<dyn ReadStore>,
    path: web::Path<String>,
) -> Result<web::Json<DeviceDetail>, ApiFailure> {
    let id = path.into_inner();
    // A group id was already a 404 here, since the lookup is device-only. What
    // this adds is the other half of the message: which URL answers instead.
    reject_group_bare(&**catalog, &id, INVENTORY).await?;
    let mut device = catalog
        .device(&id)
        .await
        .ok_or_else(|| ApiFailure::NotFound(format!("no device '{id}' is configured")))?;
    let observed = status.observe(&id).await;
    device.status = observed.connection;
    device.auto_disabled_fields = observed.auto_disabled;

    // Only after the device is known to exist: a store read for an unknown id
    // would answer `[]` and turn a 404 into a plausible-looking 200.
    let latest = store.latest_all(id).await?;
    Ok(web::Json(DeviceDetail { device, latest }))
}

/// `POST /v1/inventory/devices` — add a device to the running fleet.
///
/// The change takes effect immediately: poll loops start for the new device's
/// fields, the relay gains a task for its queue, and if it is `eager` the
/// keepalive opens its connection. Nothing else in the fleet is disturbed.
///
/// **The devices file is never written**, whatever else happens — it stays the
/// thing a deployment can put in version control, which is the same contract
/// `PATCH /v1/config` keeps for settings.
///
/// Whether the change *survives a restart* is a second question, and the answer
/// is the deployment's `inventory.runtime_config_path`:
///
/// * **Unset**, the default: it does not. A restart returns to whatever the
///   devices file says. An operator who wants the device back puts it there —
///   `GET /v1/inventory/config/export` renders the running fleet in a form the
///   file accepts.
/// * **Set**: it does. The whole document is written to that path after every
///   change, and that file is what the next startup loads *instead of* the
///   devices file. Which is worth knowing in both directions: a fleet edited
///   here survives, and an edit made to the devices file meanwhile does not take
///   effect until `POST /v1/inventory/config/reset` adopts it.
#[utoipa::path(
    post,
    path = "/devices",
    context_path = "/v1/inventory",
    tag = "inventory",
    request_body = DeviceWrite,
    responses(
        (status = 201, description = "Added. The body is the device as the index \
             now reports it, with the identity the server derived for it. The \
             `Location` header names its detail route.", body = DeviceSummary),
        (status = 400, description = "The body does not describe a device this \
             server can build: a required key supplied neither here nor by the \
             devices file's `[defaults]`, or a `disabled_fields` entry naming no \
             known field.", body = ApiError),
        (status = 409, description = "A device or group already has this id. Use \
             `PUT /v1/inventory/devices/{id}` to replace it.", body = ApiError),
    ),
)]
pub async fn add_device(
    inventory: web::Data<dyn LiveInventory>,
    body: web::Json<DeviceWrite>,
) -> Result<HttpResponse, ApiFailure> {
    let device = inventory.add(body.into_inner()).await?;
    Ok(HttpResponse::Created()
        .insert_header(("Location", format!("/v1/inventory/devices/{}", device.id)))
        .json(device))
}

/// `PUT /v1/inventory/devices/{id}` — replace a device wholesale.
///
/// A device is immutable, so this does not edit one: it builds a *different*
/// device with a different [`uuid`] and swaps it in. The consequence worth
/// knowing is that the device's SSH session is dropped and re-opened, because
/// the connection belonged to the configuration that is being replaced.
///
/// What survives is everything filed under the *id*: its stored reads, its
/// queued writes, and what the server has learned about which fields it
/// refuses — that last being evidence about the recorder at that address, not
/// about the configuration used to reach it.
///
/// Every key the body omits takes the server's default rather than the previous
/// device's value. Read the device first and send back what you want kept.
///
/// [`uuid`]: sismatic_api_types::DeviceSummary::uuid
#[utoipa::path(
    put,
    path = "/devices/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Device id, as the index reports it.")),
    request_body = DeviceWrite,
    responses(
        (status = 200, description = "Replaced. The body is the new device, whose \
             `uuid` differs from the old one whenever any key did.",
         body = DeviceSummary),
        (status = 400, description = "The body does not describe a device this \
             server can build, or states an `id` that disagrees with the path.",
         body = ApiError),
        (status = 404, description = "No device has this id.", body = ApiError),
    ),
)]
pub async fn replace_device(
    inventory: web::Data<dyn LiveInventory>,
    path: web::Path<String>,
    body: web::Json<DeviceWrite>,
) -> Result<web::Json<DeviceSummary>, ApiFailure> {
    let id = path.into_inner();
    Ok(web::Json(inventory.replace(&id, body.into_inner()).await?))
}

/// `DELETE /v1/inventory/devices/{id}` — take a device out of the fleet.
///
/// More than a deletion, because a device that leaves strands things behind.
/// The implementation stops that device's poll loops and relay task, cancels
/// every write still queued for it, and — only if the deployment's
/// `store.cleanup_on_remove` says so — drops its recorded reads. The body
/// reports both counts, so an operator can tell an idle recorder from one that
/// had eleven queued writes.
///
/// Cancelled writes stay readable at `GET /v1/writes/{id}`, reporting
/// `canceled`. Purging them would turn a caller's poll into a `404` and destroy
/// the evidence the cancellation created.
///
/// **A device a group still names is refused.** Cascading would silently change
/// what that group means, and a group quietly losing a member is the same
/// half-a-recording failure the write side's barrier defaults against — so the
/// group is edited first, and the message says so.
#[utoipa::path(
    delete,
    path = "/devices/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Device id, as the index reports it.")),
    responses(
        (status = 200, description = "Removed, with what went: queued writes \
             cancelled, and reads dropped — the latter `null` when \
             `store.cleanup_on_remove` is off and the history was kept.",
         body = Removed),
        (status = 404, description = "No device has this id.", body = ApiError),
        (status = 409, description = "A group still names this device. Remove it \
             from the group first; the message names which.", body = ApiError),
    ),
)]
pub async fn remove_device(
    inventory: web::Data<dyn LiveInventory>,
    path: web::Path<String>,
) -> Result<web::Json<Removed>, ApiFailure> {
    let id = path.into_inner();
    Ok(web::Json(inventory.remove(&id).await?))
}

/// `GET /v1/inventory/config/export` — the running configuration as a devices
/// file.
///
/// The answer to what a runtime-mutable fleet costs: changes made through this
/// scope never reach the devices file, so without this there would be no way to
/// get them back into one. The body is text in the format asked for, and saving
/// it under that extension produces a file the loader reads unchanged.
///
/// That holds whether or not `inventory.runtime_config_path` is set. A
/// deployment with
/// one persists its changes and they survive a restart — but to a *state* file,
/// which is the server's to write and not a thing anyone edits or reviews. This
/// route is how the running fleet becomes a devices file again: one a human
/// reads, a repository holds, and `POST /v1/inventory/config/reset` adopts.
///
/// It is the whole document — `[defaults]`, every `[[device]]` and every
/// `[[group]]` — because that is what "loads back as the same fleet" requires.
/// A group left out would take its members' ability to act together with it.
///
/// **Credentials are omitted unless `include_secrets` is set**, so the default
/// export is not directly loadable and that is deliberate: it lands in shell
/// history, ticket attachments and CI logs, where a plaintext recorder password
/// outlives every process that could have rotated it. The credentials come back
/// from `[defaults]`, an environment variable, or a secret store.
///
/// `promote_auto_disabled_fields_to_disabled_fields` closes the discovery loop.
/// The fleet works out which fields a recorder refuses; this writes those
/// findings into the document as *declared* vetoes; committing the result makes
/// them permanent and free, since a declared veto starts no poll loop at all
/// where an inferred one costs a timer tick per interval.
#[utoipa::path(
    get,
    path = "/config/export",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(ExportQuery),
    responses(
        (status = 200, description = "The fleet as a devices document, in the \
             requested format. `Content-Disposition` names a filename with the \
             extension the loader dispatches on.", body = String),
        (status = 400, description = "The query could not be read — an unknown \
             `format`, or a key this route does not accept.", body = ApiError),
        (status = 500, description = "The document could not be serialized.",
         body = ApiError),
    ),
)]
pub async fn export_config(
    inventory: web::Data<dyn LiveInventory>,
    query: web::Query<ExportQuery>,
) -> Result<HttpResponse, ApiFailure> {
    let query = query.into_inner();
    let body = inventory.export(&query).await?;
    Ok(HttpResponse::Ok()
        .content_type(query.format.content_type())
        // Named so a browser saves it as something the loader will dispatch on,
        // rather than as the route's last path segment with no extension at all.
        .insert_header((
            "Content-Disposition",
            format!(
                "attachment; filename=\"devices.{}\"",
                query.format.extension()
            ),
        ))
        .body(body))
}

/// `POST /v1/inventory/config/reset` — discard runtime changes and adopt the
/// file.
///
/// Every add, replace and remove since startup is undone at once, and what is
/// left is exactly what the devices file describes. The escape hatch for a fleet
/// that has drifted — and the one operation that makes the file authoritative
/// again without a restart.
///
/// It is a whole-fleet `apply`, so the same rules hold as for any other: a
/// device the file and the running fleet agree on keeps its warm SSH session and
/// its learned vetoes, and only what actually differs is disturbed. Devices the
/// file does not describe are *removed*, which cancels their queued writes — so
/// this can strand work in the same way a `DELETE` does, and reports the fleet
/// it ended with so a caller can see what happened.
#[utoipa::path(
    post,
    path = "/config/reset",
    context_path = "/v1/inventory",
    tag = "inventory",
    responses(
        (status = 200, description = "Reset. The body is the fleet as the index \
             now reports it.", body = DeviceList),
        (status = 400, description = "The devices file on disk does not describe \
             a fleet this server can build.", body = ApiError),
        (status = 500, description = "The devices file could not be read.",
         body = ApiError),
    ),
)]
pub async fn reset_config(
    inventory: web::Data<dyn LiveInventory>,
) -> Result<web::Json<DeviceList>, ApiFailure> {
    Ok(web::Json(inventory.reset().await?))
}

/// `GET /v1/inventory/groups` — every configured group, ordered by id.
#[utoipa::path(
    get,
    path = "/groups",
    context_path = "/v1/inventory",
    tag = "inventory",
    responses(
        (status = 200, description = "Every group in the devices file, ordered by id. \
             Each carries its members in the order they were configured.",
         body = GroupList),
    ),
)]
pub async fn list_groups(catalog: web::Data<dyn DeviceCatalog>) -> web::Json<GroupList> {
    web::Json(GroupList {
        groups: catalog.groups().await,
    })
}

/// `POST /v1/inventory/groups` — add a device group to the running fleet.
///
/// A group is a name over devices that already exist plus a policy for what
/// happens when they cannot act together. Every member must resolve, and the id
/// must be free in the namespace devices and groups share — both refused here
/// rather than discovered at the first write addressed to it.
#[utoipa::path(
    post,
    path = "/groups",
    context_path = "/v1/inventory",
    tag = "inventory",
    request_body = GroupWrite,
    responses(
        (status = 201, description = "Added. The body is the group as the index \
             now reports it, with the barrier timeout it resolved to. The \
             `Location` header names its detail route.", body = GroupSummary),
        (status = 400, description = "The body does not describe a group this \
             server can build: no members, a member naming no device, or an \
             unknown barrier policy.", body = ApiError),
        (status = 409, description = "A device or group already has this id.",
         body = ApiError),
    ),
)]
pub async fn add_group(
    inventory: web::Data<dyn LiveInventory>,
    body: web::Json<GroupWrite>,
) -> Result<HttpResponse, ApiFailure> {
    let group = inventory.add_group(body.into_inner()).await?;
    Ok(HttpResponse::Created()
        .insert_header(("Location", format!("/v1/inventory/groups/{}", group.id)))
        .json(group))
}

/// `PUT /v1/inventory/groups/{id}` — replace a device group wholesale.
///
/// `devices` replaces the membership entirely rather than adding to it, so
/// removing a member is sending the list without it. The same replace-not-merge
/// contract the device route has, and the one most likely to surprise here:
/// there is no "add a member" verb because there is no partial update.
#[utoipa::path(
    put,
    path = "/groups/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Group id, as the index reports it.")),
    request_body = GroupWrite,
    responses(
        (status = 200, description = "Replaced. The body is the group as it now \
             stands.", body = GroupSummary),
        (status = 400, description = "The body does not describe a group this \
             server can build, or states an `id` that disagrees with the path.",
         body = ApiError),
        (status = 404, description = "No group has this id.", body = ApiError),
    ),
)]
pub async fn replace_group(
    inventory: web::Data<dyn LiveInventory>,
    path: web::Path<String>,
    body: web::Json<GroupWrite>,
) -> Result<web::Json<GroupSummary>, ApiFailure> {
    let id = path.into_inner();
    Ok(web::Json(
        inventory.replace_group(&id, body.into_inner()).await?,
    ))
}

/// `DELETE /v1/inventory/groups/{id}` — take a device group out of the fleet.
///
/// Strands nothing, which is what makes it different from removing a device. A
/// group owns no queue: a write addressed to one is expanded into per-device
/// rows at submission, so what was accepted is owed to devices that still exist
/// and still dispatches. What goes with the group is the record of what it was
/// last told.
///
/// It is also the route that clears the way for a device removal, which is
/// refused while any group still names the device.
#[utoipa::path(
    delete,
    path = "/groups/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Group id, as the index reports it.")),
    responses(
        (status = 204, description = "Removed. No body: unlike a device removal \
             there is nothing that went with it worth counting."),
        (status = 404, description = "No group has this id.", body = ApiError),
    ),
)]
pub async fn remove_group(
    inventory: web::Data<dyn LiveInventory>,
    path: web::Path<String>,
) -> Result<HttpResponse, ApiFailure> {
    inventory.remove_group(&path.into_inner()).await?;
    Ok(HttpResponse::NoContent().finish())
}

/// `GET /v1/inventory/groups/{id}` — one group and the devices it addresses.
#[utoipa::path(
    get,
    path = "/groups/{id}",
    context_path = "/v1/inventory",
    tag = "inventory",
    params(("id" = String, Path, description = "Group id, as written in the devices file.")),
    responses(
        (status = 200, description = "The group and its member device ids, in \
             configured order.", body = GroupSummary),
        (status = 404, description = "No group has this id.", body = ApiError),
    ),
)]
pub async fn read_group(
    catalog: web::Data<dyn DeviceCatalog>,
    path: web::Path<String>,
) -> Result<web::Json<GroupSummary>, ApiFailure> {
    let id = path.into_inner();
    if let Some(group) = catalog.group(&id).await {
        return Ok(web::Json(group));
    }

    // Devices and groups share one id namespace, so naming a device here is a
    // different mistake from naming nothing: the fix is a different URL, not a
    // different id. Saying which saves the caller a second round trip to find
    // out.
    let message = if catalog.device(&id).await.is_some() {
        format!("'{id}' is a device, not a group; try /v1/inventory/devices/{id}")
    } else {
        format!("no group '{id}' is configured")
    };
    Err(ApiFailure::NotFound(message))
}
